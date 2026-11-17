//! shoal: copy-on-write session volumes for running coding agents in parallel.
//!
//! A template is built once from a git repository and made warm by a setup
//! command (`npm ci`, `cargo fetch`, ...). Every session is a copy-on-write
//! child of that template: a complete checkout with its own `.git`, its own
//! branch and its installed dependencies, created in milliseconds and storing
//! only what it changes.

mod config;
mod conflicts;
mod gateway;
mod git;
mod hook;
mod mcp;
mod ops;
mod project;
mod queue;
mod services;
mod sys;
mod tools;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use project::{Backend, Project, Service, Session};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "shoal",
    version,
    about = "Copy-on-write session volumes for running coding agents in parallel"
)]
struct Cli {
    /// Project directory (default: found from the current directory)
    #[arg(long, global = true, env = "SHOAL_PROJECT")]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a warm template from a git repository
    Init(InitArgs),
    /// Create a session from the latest template
    New {
        name: String,
        /// Fork this session instead, keeping its uncommitted and ignored files
        #[arg(long)]
        from: Option<String>,
        /// With --from: start from one of that session's checkpoints
        #[arg(long, requires = "from")]
        at: Option<String>,
    },
    /// Fork a session, keeping its uncommitted and ignored files
    Fork {
        source: String,
        name: String,
        /// Start from one of the source session's checkpoints instead
        #[arg(long)]
        at: Option<String>,
    },
    /// Land a session's work on the tracked branch: rebase, test, fast-forward
    Land {
        name: String,
        /// Commit uncommitted work with this message first
        #[arg(long, short)]
        message: Option<String>,
        /// Test command (default: shoal.toml or detected)
        #[arg(long, conflicts_with = "no_test")]
        test: Option<String>,
        /// Skip tests
        #[arg(long)]
        no_test: bool,
        /// Target branch (default: the branch the project tracks)
        #[arg(long)]
        into: Option<String>,
        /// Test time limit in seconds
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
    },
    /// Call a tool with JSON arguments and print its JSON result (what the MCP server does)
    Api {
        tool: String,
        /// JSON object of arguments (default: {})
        args: Option<String>,
    },
    /// Serve the tools over MCP on stdin/stdout (for Claude Code)
    Mcp,
    /// Handle a Claude Code hook event, reading its JSON from stdin
    Hook {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(hook::EVENTS))]
        event: String,
    },
    /// List sessions
    #[command(alias = "list")]
    Ls,
    /// Show one session in detail
    Status { name: String },
    /// Print a session's directory
    Path { name: String },
    /// Show everything a session changed, including untracked files
    Diff {
        name: String,
        /// Summarise instead of showing the patch
        #[arg(long)]
        stat: bool,
    },
    /// Run a command in a session (sets SHOAL_SESSION, SHOAL_PORT and PORT)
    Exec {
        /// Linux: make everything outside the session read-only and hide other sessions
        #[arg(long)]
        sandbox: bool,
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Run a heavy command (tests, a build) in a session once a queue slot is
    /// free. With no NAME, uses the session the current directory is in.
    Run {
        name: Option<String>,
        /// Queue slots this job needs (a build that uses every core can take more)
        #[arg(long, default_value_t = 1)]
        slots: usize,
        #[arg(long)]
        sandbox: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show the machine-wide job queue, or change its number of slots
    Queue {
        /// Set the number of jobs that may run at once (default: one per CPU)
        #[arg(long)]
        slots: Option<usize>,
    },
    /// Start a long-running service, such as a dev server, in a session
    Serve {
        name: String,
        /// Service name, so a session can run several
        #[arg(long = "as", default_value = "web")]
        service: String,
        /// Put a gateway on the port that starts the service again on the
        /// first request after `reap` stopped it (the service gets a private
        /// port in PORT)
        #[arg(long)]
        on_demand: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Internal: the wake-on-request gateway for an on-demand service
    #[command(hide = true)]
    Gateway { name: String, service: String },
    /// Stop a session's services (all of them, or the one named)
    Stop {
        name: String,
        service: Option<String>,
    },
    /// List services in this project
    Services,
    /// Restart services that `reap` stopped while the session was idle
    Wake { name: String },
    /// Stop the services of sessions that have been idle, in every project
    Reap {
        /// How long without activity before services stop
        #[arg(long, default_value = "15m")]
        idle: String,
        /// Keep running, checking at this interval (e.g. 1m)
        #[arg(long)]
        watch: Option<String>,
    },
    /// Mark a session as active (for hooks, e.g. when an agent starts a turn)
    Touch { name: String },
    /// Open a shell in a session
    Shell {
        #[arg(long)]
        sandbox: bool,
        name: String,
    },
    /// Save a point-in-time copy of a session, including uncommitted files
    Checkpoint { name: String, label: Option<String> },
    /// Restore a session to a checkpoint
    Rewind { name: String, label: String },
    /// Copy session commits into the source repository as branches shoal/<name>
    Import {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Show files changed by more than one session, and which pairs conflict
    Conflicts,
    /// Build a new template generation from the source repository's latest commit
    Refresh,
    /// Remove a session
    Rm {
        name: String,
        /// Remove even with uncommitted changes or commits that were not imported
        #[arg(long)]
        force: bool,
    },
    /// Mount every session (for example after a reboot)
    Up,
    /// Unmount every session
    Down,
    /// Delete template layers and generations that nothing uses any more
    Gc,
    /// Show the project's template generations and disk use
    Info,
}

#[derive(Args)]
struct InitArgs {
    /// Repository to build the template from (default: the current directory)
    repo: Option<PathBuf>,
    /// Branch to track (default: the repository's current branch)
    #[arg(long)]
    branch: Option<String>,
    /// Command that makes the template warm, run once inside it (e.g. "npm ci")
    #[arg(long)]
    setup: Option<String>,
    /// On refresh, re-run --setup only when one of these files changed
    /// (default: common lockfiles and manifests)
    #[arg(long = "setup-input", value_name = "FILE")]
    setup_inputs: Vec<String>,
    /// An ignored file or directory to copy from the repository into the
    /// template, such as .env (repeatable)
    #[arg(long = "copy", value_name = "PATH")]
    copy: Vec<String>,
    /// Default: overlay when running as root on Linux, clone otherwise
    #[arg(long, value_enum)]
    backend: Option<Backend>,
    /// First port handed to sessions; each session gets a block of ten
    #[arg(long, default_value_t = 4100, value_parser = clap::value_parser!(u16).range(1..=i64::from(shoal_port_max())))]
    port_base: u16,
}

const fn shoal_port_max() -> u16 {
    config::PORT_BASE_MAX
}

fn main() {
    // Exit quietly when the reader goes away (`shoal ls | head`), like other CLIs,
    // instead of panicking on a broken pipe.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("shoal: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    // Commands that are not about one project.
    let command = match cli.command {
        Cmd::Init(args) => {
            cmd_init(args)?;
            return Ok(0);
        }
        Cmd::Queue { slots } => {
            cmd_queue(slots)?;
            return Ok(0);
        }
        Cmd::Reap { idle, watch } => {
            cmd_reap(&idle, watch.as_deref())?;
            return Ok(0);
        }
        Cmd::Api { tool, args } => return cmd_api(&tool, args.as_deref(), cli.project.as_deref()),
        Cmd::Mcp => {
            mcp::serve()?;
            return Ok(0);
        }
        Cmd::Hook { event } => return Ok(hook::run(&event)),
        other => other,
    };
    let mut p = Project::open(project::locate(cli.project.as_deref())?)?;
    match command {
        Cmd::Init(_) | Cmd::Api { .. } | Cmd::Mcp | Cmd::Hook { .. } => unreachable!(),
        Cmd::New { name, from, at } => cmd_new(&mut p, &name, from, at)?,
        Cmd::Fork { source, name, at } => cmd_new(&mut p, &name, Some(source), at)?,
        Cmd::Land {
            name,
            message,
            test,
            no_test,
            into,
            timeout,
        } => {
            let dir = p.dir.clone();
            p.session(&name)?;
            drop(p);
            return cmd_land(&dir, &name, message, test, no_test, into, timeout);
        }
        Cmd::Ls => cmd_ls(&p)?,
        Cmd::Status { name } => cmd_status(&p, &name)?,
        Cmd::Path { name } => println!("{}", p.ensure_ready(&name)?.display()),
        Cmd::Diff { name, stat } => return cmd_diff(&p, &name, stat),
        Cmd::Exec {
            sandbox,
            name,
            command,
        } => return cmd_exec(p, &name, sandbox, command),
        Cmd::Run {
            name,
            slots,
            sandbox,
            command,
        } => return cmd_run(p, name, slots, sandbox, command),
        Cmd::Queue { .. } | Cmd::Reap { .. } => unreachable!(),
        Cmd::Serve {
            name,
            service,
            on_demand,
            command,
        } => cmd_serve(&mut p, &name, &service, on_demand, command)?,
        Cmd::Gateway { name, service } => {
            let svc = p
                .session(&name)?
                .services
                .iter()
                .find(|s| s.name == service)
                .cloned()
                .with_context(|| format!("{name} has no service named {service:?}"))?;
            let internal = svc
                .internal_port
                .with_context(|| format!("{service} is not an on-demand service"))?;
            let dir = p.dir.clone();
            drop(p); // the gateway runs for a long time; never hold the lock
            gateway::run(&dir, &name, svc.port, internal)?;
            return Ok(0);
        }
        Cmd::Stop { name, service } => cmd_stop(&mut p, &name, service.as_deref())?,
        Cmd::Services => cmd_services(&p)?,
        Cmd::Wake { name } => cmd_wake(&mut p, &name)?,
        Cmd::Touch { name } => {
            p.session(&name)?;
            p.mark_active(&name)?;
        }
        Cmd::Shell { sandbox, name } => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            return cmd_exec(p, &name, sandbox, vec![shell]);
        }
        Cmd::Checkpoint { name, label } => cmd_checkpoint(&mut p, &name, label)?,
        Cmd::Rewind { name, label } => cmd_rewind(&mut p, &name, &label)?,
        Cmd::Import { names, all } => cmd_import(&p, names, all)?,
        Cmd::Conflicts => cmd_conflicts(&p)?,
        Cmd::Refresh => cmd_refresh(&mut p)?,
        Cmd::Rm { name, force } => cmd_rm(&mut p, &name, force)?,
        Cmd::Up => cmd_up(&p)?,
        Cmd::Down => cmd_down(&mut p)?,
        Cmd::Gc => cmd_gc(&mut p)?,
        Cmd::Info => cmd_info(&p)?,
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// init and refresh: building templates
// ---------------------------------------------------------------------------

fn cmd_init(args: InitArgs) -> Result<()> {
    let repo = match args.repo {
        Some(repo) => repo,
        None => std::env::current_dir()?,
    };
    let r = ops::init(ops::InitOptions {
        repo,
        branch: args.branch,
        setup: args.setup,
        setup_inputs: args.setup_inputs,
        copy: args.copy,
        backend: args.backend,
        port_base: args.port_base,
    })?;
    println!(
        "Template g1 ready: commit {}, {} on disk, {} backend, {} in total",
        git::short(&r.commit),
        sys::human_bytes(r.template_bytes),
        r.backend,
        sys::human_duration(std::time::Duration::from_millis(r.elapsed_ms))
    );
    println!("Project: {}", r.project.display());
    println!("Next: shoal new <name>");
    Ok(())
}

fn cmd_refresh(p: &mut Project) -> Result<()> {
    let r = ops::refresh(p)?;
    if r.up_to_date {
        println!(
            "Template g{} is already at {} ({})",
            r.generation,
            git::short(&r.to),
            p.state.branch
        );
        return Ok(());
    }
    println!(
        "Template g{}: {}..{}, {} files changed, setup {}, {}",
        r.generation,
        git::short(&r.from),
        git::short(&r.to),
        r.changed_files,
        if r.setup_ran { "re-run" } else { "not needed" },
        sys::human_duration(std::time::Duration::from_millis(r.elapsed_ms))
    );
    println!(
        "New sessions use g{}. Existing sessions stay on their generation.",
        r.generation
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

fn cmd_new(p: &mut Project, name: &str, from: Option<String>, at: Option<String>) -> Result<()> {
    let start = Instant::now();
    let origin = match (from, at) {
        (None, _) => ops::Origin::Template,
        (Some(session), None) => ops::Origin::Session(session),
        (Some(session), Some(label)) => ops::Origin::Checkpoint { session, label },
    };
    let info = ops::create_session(p, name, &origin)?;
    let what = match (&info.parent, &info.from_checkpoint) {
        (Some(parent), Some(cp)) => format!("from {parent}@{cp}"),
        (Some(parent), None) => format!("forked from {parent}"),
        _ => format!("from template g{}", info.generation),
    };
    println!(
        "Created {name} {what} in {} (branch {} {})",
        sys::human_duration(std::time::Duration::from_millis(info.ready_ms)),
        info.branch,
        sys::human_duration(
            start
                .elapsed()
                .saturating_sub(std::time::Duration::from_millis(info.ready_ms))
        )
    );
    println!("  path  {}", info.path.display());
    println!("  port  {}", info.port);
    Ok(())
}

fn cmd_land(
    dir: &Path,
    name: &str,
    message: Option<String>,
    test: Option<String>,
    no_test: bool,
    into: Option<String>,
    timeout: u64,
) -> Result<i32> {
    let r = ops::land(
        dir,
        name,
        &ops::LandOptions {
            test: if no_test { Some(String::new()) } else { test },
            commit_message: message,
            into,
            test_timeout: std::time::Duration::from_secs(timeout.max(1)),
        },
    )?;
    println!("{}: {}", r.status, r.message);
    if !r.files.is_empty() {
        println!("  files: {}", r.files.join(", "));
    }
    if let Some(t) = &r.tests {
        println!(
            "  tests: `{}` {} in {}",
            t.command,
            if t.passed {
                "passed"
            } else if t.timed_out {
                "timed out"
            } else {
                "failed"
            },
            sys::human_duration(std::time::Duration::from_millis(t.duration_ms))
        );
        if !t.passed {
            println!("{}", t.output_tail);
        }
    }
    Ok(if r.status == "landed" || r.status == "nothing_to_land" {
        0
    } else {
        3
    })
}

fn cmd_api(tool: &str, args: Option<&str>, project: Option<&Path>) -> Result<i32> {
    let mut args: serde_json::Value = match args {
        Some(raw) => serde_json::from_str(raw).context("arguments must be a JSON object")?,
        None => serde_json::json!({}),
    };
    if let (Some(dir), Some(obj)) = (project, args.as_object_mut()) {
        obj.entry("project")
            .or_insert_with(|| serde_json::json!(dir.to_string_lossy()));
    }
    match tools::call(tool, &args) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Err(err) => {
            println!("{}", serde_json::json!({"error": format!("{err:#}")}));
            Ok(1)
        }
    }
}

struct SessionSummary {
    commits: u32,
    changed: usize,
    delta: Option<u64>,
}

fn summarize(p: &Project, s: &Session) -> Result<SessionSummary> {
    let path = p.ensure_ready(&s.name)?;
    let commits = git::count_commits(&path, &format!("{}..HEAD", s.base_commit))?;
    let changed = git::status_lines(&path)?.len();
    let delta = match p.state.backend {
        Backend::Overlay => Some(sys::disk_usage(&p.volume_dir(&s.name).join("upper"))),
        Backend::Clone => None,
    };
    Ok(SessionSummary {
        commits,
        changed,
        delta,
    })
}

fn cmd_ls(p: &Project) -> Result<()> {
    if p.state.sessions.is_empty() {
        println!("No sessions. Create one with `shoal new <name>`.");
        return Ok(());
    }
    let width = p
        .state
        .sessions
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<4}  {:<5}  {:>7}  {:>11}  {:>8}  {:>5}  {:<15}  ORIGIN",
        "NAME", "GEN", "PORT", "COMMITS", "UNCOMMITTED", "DELTA", "IDLE", "SERVICES"
    );
    for s in p.state.sessions.values() {
        let sum = summarize(p, s)?;
        let idle = sys::now().saturating_sub(services::last_activity(p, s));
        let running = s
            .services
            .iter()
            .filter(|svc| services::state(svc) == services::State::Running)
            .count();
        let sleeping = s
            .services
            .iter()
            .filter(|svc| services::state(svc) == services::State::StoppedIdle)
            .count();
        let services_label = match (running, sleeping) {
            (0, 0) => "-".to_string(),
            (r, 0) => format!("{r} running"),
            (0, z) => format!("{z} idle-stopped"),
            (r, z) => format!("{r} up, {z} idle"),
        };
        println!(
            "{:<width$}  {:<4}  {:<5}  {:>7}  {:>11}  {:>8}  {:>5}  {:<15}  {}",
            s.name,
            format!("g{}", s.generation),
            s.port,
            sum.commits,
            sum.changed,
            sum.delta
                .map(sys::human_bytes)
                .unwrap_or_else(|| "-".into()),
            services::human_secs(idle),
            services_label,
            match &s.parent {
                Some(parent) => format!("fork of {parent}"),
                None => "template".into(),
            }
        );
    }
    Ok(())
}

#[derive(Default)]
struct DeltaCount {
    modified: usize,
    created: usize,
    deleted: usize,
    bytes: u64,
}

/// What an overlay session changed, grouped by top-level path. Read from the
/// session's upper dir only, so the cost follows the size of the change.
fn upper_delta(upper: &Path, lowers: &[PathBuf]) -> BTreeMap<String, DeltaCount> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut groups: BTreeMap<String, DeltaCount> = BTreeMap::new();
    let mut stack = vec![upper.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(upper).unwrap_or(&path);
            let top = rel
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default();
            let group = groups.entry(top).or_default();
            if meta.file_type().is_char_device() && meta.rdev() == 0 {
                group.deleted += 1;
            } else if lowers
                .iter()
                .any(|l| l.join(rel).symlink_metadata().is_ok())
            {
                group.modified += 1;
                group.bytes += meta.blocks() * 512;
            } else {
                group.created += 1;
                group.bytes += meta.blocks() * 512;
            }
        }
    }
    groups
}

fn cmd_status(p: &Project, name: &str) -> Result<()> {
    let s = p.session(name)?;
    let path = p.ensure_ready(name)?;
    let head = git::head(&path)?;
    let branch = git::git_trim(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sum = summarize(p, s)?;
    println!("{name}");
    println!("  path         {}", path.display());
    println!(
        "  branch       {branch} at {}, {} commit(s) on top of g{} ({})",
        git::short(&head),
        sum.commits,
        s.generation,
        git::short(&s.base_commit)
    );
    println!("  port         {}", s.port);
    println!("  created      {}", sys::human_age(s.created_at));
    if let Some(parent) = &s.parent {
        println!("  forked from  {parent}");
    }
    let checkpoints: Vec<String> = p
        .state
        .checkpoints
        .iter()
        .filter(|c| c.session == name)
        .map(|c| format!("{} ({})", c.label, sys::human_age(c.created_at)))
        .collect();
    if !checkpoints.is_empty() {
        println!("  checkpoints  {}", checkpoints.join(", "));
    }
    let lines = git::status_lines(&path)?;
    println!();
    if lines.is_empty() {
        println!("  No uncommitted changes.");
    } else {
        println!("  Uncommitted changes:");
        for line in &lines {
            println!("    {line}");
        }
    }
    if p.state.backend == Backend::Overlay {
        let lowers: Vec<PathBuf> = s.lowers.iter().map(|l| p.abs(l)).collect();
        let groups = upper_delta(&p.volume_dir(name).join("upper"), &lowers);
        println!();
        println!("  Volume delta, including ignored files (only this is stored per session):");
        let mut total = DeltaCount::default();
        for (top, c) in &groups {
            println!(
                "    {:<24} {:>5} modified  {:>5} new  {:>5} deleted  {:>9}",
                top,
                c.modified,
                c.created,
                c.deleted,
                sys::human_bytes(c.bytes)
            );
            total.modified += c.modified;
            total.created += c.created;
            total.deleted += c.deleted;
            total.bytes += c.bytes;
        }
        println!(
            "    {:<24} {:>5} modified  {:>5} new  {:>5} deleted  {:>9}",
            "total",
            total.modified,
            total.created,
            total.deleted,
            sys::human_bytes(total.bytes)
        );
    }
    Ok(())
}

fn cmd_diff(p: &Project, name: &str, stat: bool) -> Result<i32> {
    let s = p.session(name)?;
    let path = p.ensure_ready(name)?;
    let snapshot = git::snapshot_commit(&path)?;
    let mut cmd = git::command(&path);
    cmd.args(["--no-pager", "diff"]);
    if stat {
        cmd.arg("--stat");
    }
    cmd.args([&s.base_commit, &snapshot]);
    sys::interactive(&mut cmd)
}

fn sandbox_command(project_dir: &Path, session: &Path, command: &[String]) -> Result<Command> {
    if !cfg!(target_os = "linux") {
        bail!("--sandbox is only available on Linux for now");
    }
    if Command::new("bwrap").arg("--version").output().is_err() {
        bail!("--sandbox needs bubblewrap (bwrap) on PATH");
    }
    let mut cmd = Command::new("bwrap");
    cmd.args(["--die-with-parent", "--ro-bind", "/", "/"])
        .args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"])
        // Hide the template, the other sessions and the checkpoints...
        .arg("--tmpfs")
        .arg(project_dir)
        // ...then bring back only this session, writable.
        .arg("--bind")
        .arg(session)
        .arg(session)
        .args(["--setenv", "XDG_CACHE_HOME", "/tmp/.cache"])
        .args(["--setenv", "npm_config_cache", "/tmp/.npm"])
        .arg("--chdir")
        .arg(session)
        .arg("--")
        .args(command);
    Ok(cmd)
}

/// The command to run in a session, with its working directory and environment.
fn session_command(
    project_dir: &Path,
    path: &Path,
    name: &str,
    port: u16,
    sandbox: bool,
    command: &[String],
) -> Result<Command> {
    let mut cmd = if sandbox {
        sandbox_command(project_dir, path, command)?
    } else {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd
    };
    cmd.current_dir(path)
        .env("SHOAL_SESSION", name)
        .env("SHOAL_PORT", port.to_string())
        .env("SHOAL_PROJECT", project_dir);
    if std::env::var_os("PORT").is_none() {
        cmd.env("PORT", port.to_string());
    }
    Ok(cmd)
}

fn cmd_exec(mut p: Project, name: &str, sandbox: bool, command: Vec<String>) -> Result<i32> {
    let path = p.ensure_ready(name)?;
    let port = p.session(name)?.port;
    p.mark_active(name)?;
    let project_dir = p.dir.clone();
    drop(p); // release the project lock while the command runs
    let mut cmd = session_command(&project_dir, &path, name, port, sandbox, &command)?;
    sys::supervised(&mut cmd)
}

fn cmd_run(
    mut p: Project,
    name: Option<String>,
    slots: usize,
    sandbox: bool,
    command: Vec<String>,
) -> Result<i32> {
    let name = match name {
        Some(name) => name,
        None => p.session_from_cwd()?,
    };
    let path = p.ensure_ready(&name)?;
    let port = p.session(&name)?.port;
    p.mark_active(&name)?;
    let project_dir = p.dir.clone();
    drop(p);

    let job = queue::Job {
        pid: std::process::id(),
        project: project_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        session: name.clone(),
        command: command.join(" "),
        slots,
        queued_at: sys::now(),
        started_at: None,
    };
    let queued = Instant::now();
    let held = queue::acquire(&job, |status| {
        eprintln!(
            "shoal: waiting for a queue slot ({} of {} busy, {} waiting)",
            status.busy,
            status.slots,
            status.waiting.len()
        );
    })?;
    let waited = queued.elapsed();
    if waited.as_millis() >= 100 {
        eprintln!("shoal: got a slot after {}", sys::human_duration(waited));
    }
    let mut cmd = session_command(&project_dir, &path, &name, port, sandbox, &command)?;
    let code = sys::supervised(&mut cmd);
    drop(held);
    // The job counts as activity too; best effort, the job's result matters more.
    if let Ok(mut p) = Project::open(project_dir) {
        let _ = p.mark_active(&name);
    }
    code
}

fn cmd_queue(slots: Option<usize>) -> Result<()> {
    if let Some(n) = slots {
        queue::set_slot_count(n)?;
        println!("The queue now runs up to {n} job(s) at once.");
        return Ok(());
    }
    let status = queue::status()?;
    println!(
        "{} of {} slot(s) busy, {} job(s) waiting",
        status.busy,
        status.slots,
        status.waiting.len()
    );
    if !status.running.is_empty() {
        println!();
        println!("RUNNING");
        let mut seen = BTreeSet::new();
        for (_, job) in &status.running {
            if !seen.insert(job.pid) {
                continue; // one line per job, even if it holds several slots
            }
            let started = job.started_at.unwrap_or(job.queued_at);
            println!(
                "  {:<24} {:>6}  slots {}  {}",
                format!("{}/{}", job.project, job.session),
                services::human_secs(sys::now().saturating_sub(started)),
                job.slots,
                job.command
            );
        }
    }
    if !status.waiting.is_empty() {
        println!();
        println!("WAITING");
        for job in &status.waiting {
            println!(
                "  {:<24} {:>6}  slots {}  {}",
                format!("{}/{}", job.project, job.session),
                services::human_secs(sys::now().saturating_sub(job.queued_at)),
                job.slots,
                job.command
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Services and idle shutdown
// ---------------------------------------------------------------------------

fn cmd_serve(
    p: &mut Project,
    name: &str,
    service: &str,
    on_demand: bool,
    command: Vec<String>,
) -> Result<()> {
    let svc = services::serve(p, name, service, on_demand, command)?;
    match svc.internal_port {
        Some(internal) => println!(
            "Started {service} in {name} on port {} (on demand: the service listens on {internal}), log {}",
            svc.port,
            p.log_path(name, service).display()
        ),
        None => println!(
            "Started {service} in {name} on port {} (process group {}), log {}",
            svc.port,
            svc.pid.unwrap_or_default(),
            p.log_path(name, service).display()
        ),
    }
    Ok(())
}

fn cmd_stop(p: &mut Project, name: &str, service: Option<&str>) -> Result<()> {
    if let Some(service) = service {
        if !p.session(name)?.services.iter().any(|s| s.name == service) {
            bail!("{name} has no service named {service:?}");
        }
    }
    let (count, kb) = services::stop_session(p, name, service, false, false)?;
    if count == 0 {
        println!("Nothing was running in {name}.");
    } else {
        println!(
            "Stopped {count} service(s) in {name}, freeing about {}",
            sys::human_bytes(kb * 1024)
        );
    }
    Ok(())
}

fn cmd_services(p: &Project) -> Result<()> {
    let rows: Vec<(&Session, &Service)> = p
        .state
        .sessions
        .values()
        .flat_map(|s| s.services.iter().map(move |svc| (s, svc)))
        .collect();
    if rows.is_empty() {
        println!("No services. Start one with `shoal serve <session> -- <command>`.");
        return Ok(());
    }
    println!(
        "{:<12} {:<8} {:<5}  {:<19} {:>7} {:>9} {:>6}  COMMAND",
        "SESSION", "SERVICE", "PORT", "STATE", "PID", "MEMORY", "IDLE"
    );
    for (session, svc) in rows {
        let state = services::state(svc);
        let running = state == services::State::Running;
        let idle = sys::now().saturating_sub(services::last_activity(p, session));
        println!(
            "{:<12} {:<8} {:<5}  {:<19} {:>7} {:>9} {:>6}  {}",
            session.name,
            svc.name,
            svc.port,
            services::describe(svc),
            if running {
                svc.pid.unwrap_or_default().to_string()
            } else {
                "-".into()
            },
            if running {
                sys::human_bytes(services::group_rss_kb(svc.pid.unwrap_or_default()) * 1024)
            } else {
                "-".into()
            },
            services::human_secs(idle),
            svc.command.join(" ")
        );
    }
    Ok(())
}

fn cmd_wake(p: &mut Project, name: &str) -> Result<()> {
    p.ensure_ready(name)?;
    let session = p.session(name)?.clone();
    let mut woken = 0;
    for (i, svc) in session.services.iter().enumerate() {
        if services::state(svc) != services::State::StoppedIdle {
            continue;
        }
        let mut svc = svc.clone();
        services::start(p, &session, &mut svc)?;
        println!(
            "Started {} in {name} on port {} (process group {})",
            svc.name,
            svc.port,
            svc.pid.unwrap_or_default()
        );
        p.state
            .sessions
            .get_mut(name)
            .expect("session exists")
            .services[i] = svc;
        woken += 1;
    }
    p.mark_active(name)?;
    if woken == 0 {
        println!("{name} has no services stopped for being idle.");
    }
    Ok(())
}

fn reap_once(idle: u64) -> Result<()> {
    for dir in project::all_projects()? {
        let mut p = match Project::open(dir.clone()) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("shoal: skipping {}: {err:#}", dir.display());
                continue;
            }
        };
        let due: Vec<(String, u64)> = p
            .state
            .sessions
            .values()
            .filter(|s| {
                s.services
                    .iter()
                    .any(|svc| services::state(svc) == services::State::Running)
            })
            .map(|s| {
                (
                    s.name.clone(),
                    sys::now().saturating_sub(services::last_activity(&p, s)),
                )
            })
            .filter(|(_, idle_for)| *idle_for >= idle)
            .collect();
        if due.is_empty() {
            continue;
        }
        // Stop every due service in the project together, not one by one.
        let mut owners = Vec::new();
        let mut targets = Vec::new();
        for session in p.state.sessions.values_mut() {
            if !due.iter().any(|(name, _)| *name == session.name) {
                continue;
            }
            for svc in session.services.iter_mut() {
                if services::state(svc) == services::State::Running {
                    svc.stopped_for_idle = true;
                    owners.push(session.name.clone());
                    targets.push(svc);
                }
            }
        }
        let freed = services::stop_all(targets)?;
        p.save()?;
        for (name, idle_for) in due {
            let mine: Vec<u64> = owners
                .iter()
                .zip(&freed)
                .filter(|(owner, _)| **owner == name)
                .map(|(_, kb)| *kb)
                .collect();
            println!(
                "{}: stopped {} service(s) in {name}, idle for {}, freeing about {}",
                dir.file_name().unwrap_or_default().to_string_lossy(),
                mine.len(),
                services::human_secs(idle_for),
                sys::human_bytes(mine.iter().sum::<u64>() * 1024)
            );
        }
    }
    Ok(())
}

fn cmd_reap(idle: &str, watch: Option<&str>) -> Result<()> {
    let idle = services::parse_duration(idle)?.as_secs();
    match watch {
        None => reap_once(idle),
        Some(interval) => {
            let interval = services::parse_duration(interval)?;
            println!(
                "Stopping services idle for {} or more; checking every {}",
                services::human_secs(idle),
                services::human_secs(interval.as_secs())
            );
            loop {
                if let Err(err) = reap_once(idle) {
                    eprintln!("shoal: {err:#}");
                }
                std::thread::sleep(interval);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

fn cmd_checkpoint(p: &mut Project, name: &str, label: Option<String>) -> Result<()> {
    let r = ops::checkpoint(p, name, label.as_deref(), false)?;
    let label = r.label.unwrap_or_default();
    println!(
        "Checkpoint {name}@{label} at {} in {} ({} stored)",
        git::short(r.head.as_deref().unwrap_or_default()),
        sys::human_duration(std::time::Duration::from_millis(r.elapsed_ms)),
        sys::human_bytes(sys::disk_usage(&p.checkpoint_dir(name, &label)))
    );
    Ok(())
}

fn cmd_rewind(p: &mut Project, name: &str, label: &str) -> Result<()> {
    let r = ops::rewind(p, name, label)?;
    println!(
        "Rewound {name} to {} (HEAD {}) in {}",
        r.label,
        git::short(&r.head),
        sys::human_duration(std::time::Duration::from_millis(r.elapsed_ms))
    );
    for svc in r.restarted_services {
        println!("Restarted {svc}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Getting work out: import and conflicts
// ---------------------------------------------------------------------------

fn cmd_import(p: &Project, names: Vec<String>, all: bool) -> Result<()> {
    let names: Vec<String> = if all || names.is_empty() {
        p.state.sessions.keys().cloned().collect()
    } else {
        names
    };
    for name in names {
        let r = ops::import(p, &name)?;
        let note = if r.uncommitted.is_empty() {
            ""
        } else {
            " (it also has uncommitted changes, which stay in the session)"
        };
        match r.branch {
            Some(branch) => println!("{name}: {} commit(s) imported as {branch}{note}", r.commits),
            None => println!("{name}: no commits yet, skipped{note}"),
        }
    }
    Ok(())
}

fn cmd_conflicts(p: &Project) -> Result<()> {
    let start = Instant::now();
    let report = conflicts::check(p)?;
    let width = report
        .changed
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(4);
    println!("Files changed, committed or not:");
    for (name, files) in &report.changed {
        println!("  {name:<width$}  {files}");
    }
    if report.changed.len() >= 2 {
        println!();
        let w = width * 2 + 3;
        let mut conflicts = 0;
        for pair in &report.pairs {
            let label = format!("{} + {}", pair.a, pair.b);
            if pair.conflicts.is_empty() {
                println!(
                    "  {label:<w$}  merge cleanly, both change: {}",
                    pair.shared.join(", ")
                );
            } else {
                conflicts += 1;
                println!("  {label:<w$}  CONFLICT in: {}", pair.conflicts.join(", "));
            }
        }
        if report.quiet_pairs > 0 {
            println!(
                "  {} other pair(s) change no files in common.",
                report.quiet_pairs
            );
        }
        if conflicts == 0 {
            println!("\nNo conflicts.");
        }
    }
    println!(
        "\nChecked {} session(s) ({} changed since the last check) and {} overlapping pair(s) ({} merged now) in {}",
        report.changed.len(),
        report.sessions_resnapshotted,
        report.pairs.len(),
        report.pairs_merged,
        sys::human_duration(start.elapsed())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

fn cmd_rm(p: &mut Project, name: &str, force: bool) -> Result<()> {
    ops::remove(p, name, force, false)?;
    println!("Removed {name}");
    Ok(())
}

fn cmd_up(p: &Project) -> Result<()> {
    for name in p.state.sessions.keys() {
        p.ensure_ready(name)?;
    }
    println!("{} session(s) ready", p.state.sessions.len());
    Ok(())
}

fn cmd_down(p: &mut Project) -> Result<()> {
    // Running services keep a session busy, and `down` usually precedes a shutdown.
    let names: Vec<String> = p.state.sessions.keys().cloned().collect();
    for name in &names {
        let (count, _) = services::stop_session(p, name, None, false, false)?;
        if count > 0 {
            println!("Stopped {count} service(s) in {name}");
        }
    }
    if p.state.backend != Backend::Overlay {
        println!("Nothing to unmount: the clone backend uses plain directories.");
        return Ok(());
    }
    let mut failed = Vec::new();
    for name in p.state.sessions.keys() {
        let path = p.session_path(name);
        if sys::is_mounted(&path) {
            if let Err(err) = sys::unmount(&path) {
                failed.push(format!("{err:#}"));
            }
        }
    }
    if !failed.is_empty() {
        bail!("{}", failed.join("\n"));
    }
    println!("All sessions unmounted");
    Ok(())
}

fn cmd_gc(p: &mut Project) -> Result<()> {
    let latest_id = p.latest().id;
    let mut keep: BTreeSet<String> = p.latest().layers.iter().cloned().collect();
    let mut keep_generations: BTreeSet<u32> = [latest_id].into();
    for s in p.state.sessions.values() {
        keep.extend(s.lowers.iter().cloned());
        if p.state.backend == Backend::Overlay {
            keep_generations.insert(s.generation);
        }
    }
    for g in &p.state.generations {
        if keep_generations.contains(&g.id) {
            keep.extend(g.layers.iter().cloned());
        }
    }
    let mut freed = 0;
    for parent in ["layers", "templates"] {
        let Ok(entries) = std::fs::read_dir(p.dir.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            let rel = format!("{parent}/{}", entry.file_name().to_string_lossy());
            if !keep.contains(&rel) {
                freed += sys::disk_usage(&entry.path());
                std::fs::remove_dir_all(entry.path())?;
                println!("Removed {rel}");
            }
        }
    }
    p.state
        .generations
        .retain(|g| keep_generations.contains(&g.id));
    p.save()?;
    println!("Freed {}", sys::human_bytes(freed));
    Ok(())
}

fn cmd_info(p: &Project) -> Result<()> {
    println!("project   {}", p.dir.display());
    println!(
        "source    {} ({})",
        p.state.source.display(),
        p.state.branch
    );
    println!("backend   {}", p.state.backend.name());
    if let Some(setup) = &p.state.setup {
        println!("setup     {setup}");
    }
    println!();
    for g in &p.state.generations {
        let users = p
            .state
            .sessions
            .values()
            .filter(|s| s.generation == g.id)
            .count();
        println!(
            "g{:<3} {}  {:>9}  {} session(s)  {}",
            g.id,
            git::short(&g.commit),
            sys::human_bytes(sys::disk_usage(&p.abs(&g.layers[0]))),
            users,
            sys::human_age(g.created_at)
        );
    }
    println!();
    println!(
        "disk used by the whole project: {}",
        sys::human_bytes(sys::disk_usage(&p.dir))
    );
    Ok(())
}
