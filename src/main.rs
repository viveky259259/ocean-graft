//! shoal: copy-on-write session volumes for running coding agents in parallel.
//!
//! A template is built once from a git repository and made warm by a setup
//! command (`npm ci`, `cargo fetch`, ...). Every session is a copy-on-write
//! child of that template: a complete checkout with its own `.git`, its own
//! branch and its installed dependencies, created in milliseconds and storing
//! only what it changes.

mod git;
mod project;
mod sys;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use project::{validate_name, Backend, Checkpoint, Generation, Project, Session, State};
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
    },
    /// Fork a session, keeping its uncommitted and ignored files
    Fork { source: String, name: String },
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
    #[arg(long, default_value_t = 4100)]
    port_base: u16,
}

const DEFAULT_SETUP_INPUTS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Cargo.toml",
    "Cargo.lock",
    "go.mod",
    "go.sum",
    "requirements.txt",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
];

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
    let command = match cli.command {
        Cmd::Init(args) => {
            cmd_init(args)?;
            return Ok(0);
        }
        other => other,
    };
    let mut p = Project::open(project::locate(cli.project.as_deref())?)?;
    match command {
        Cmd::Init(_) => unreachable!(),
        Cmd::New { name, from } => cmd_new(&mut p, &name, from.as_deref())?,
        Cmd::Fork { source, name } => cmd_new(&mut p, &name, Some(&source))?,
        Cmd::Ls => cmd_ls(&p)?,
        Cmd::Status { name } => cmd_status(&p, &name)?,
        Cmd::Path { name } => println!("{}", p.ensure_ready(&name)?.display()),
        Cmd::Diff { name, stat } => return cmd_diff(&p, &name, stat),
        Cmd::Exec {
            sandbox,
            name,
            command,
        } => return cmd_exec(p, &name, sandbox, command),
        Cmd::Shell { sandbox, name } => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            return cmd_exec(p, &name, sandbox, vec![shell]);
        }
        Cmd::Checkpoint { name, label } => cmd_checkpoint(&mut p, &name, label)?,
        Cmd::Rewind { name, label } => cmd_rewind(&p, &name, &label)?,
        Cmd::Import { names, all } => cmd_import(&p, names, all)?,
        Cmd::Conflicts => cmd_conflicts(&p)?,
        Cmd::Refresh => cmd_refresh(&mut p)?,
        Cmd::Rm { name, force } => cmd_rm(&mut p, &name, force)?,
        Cmd::Up => cmd_up(&p)?,
        Cmd::Down => cmd_down(&p)?,
        Cmd::Gc => cmd_gc(&mut p)?,
        Cmd::Info => cmd_info(&p)?,
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// init and refresh: building templates
// ---------------------------------------------------------------------------

fn run_setup(dir: &Path, setup: &str) -> Result<()> {
    println!("Running setup in the template: {setup}");
    let start = Instant::now();
    let code = sys::interactive(
        Command::new("sh")
            .arg("-c")
            .arg(setup)
            .current_dir(dir)
            .env("SHOAL_TEMPLATE", "1"),
    )?;
    if code != 0 {
        bail!("setup command exited with status {code}");
    }
    println!("Setup finished in {}", sys::human_duration(start.elapsed()));
    Ok(())
}

fn cmd_init(args: InitArgs) -> Result<()> {
    let start = Instant::now();
    let repo = match args.repo {
        Some(repo) => repo,
        None => std::env::current_dir()?,
    };
    let source = git::toplevel(&repo)?;
    let branch = match args.branch {
        Some(branch) => branch,
        None => git::git_trim(&source, &["rev-parse", "--abbrev-ref", "HEAD"])?,
    };
    if branch == "HEAD" {
        bail!("{} has a detached HEAD; pass --branch", source.display());
    }
    git::git_trim(
        &source,
        &["rev-parse", "--verify", &format!("{branch}^{{commit}}")],
    )
    .with_context(|| format!("branch {branch} has no commits"))?;
    let backend = args.backend.unwrap_or_else(Backend::auto);
    backend.check_supported()?;

    let dir = project::project_dir_for(&source)?;
    if dir.join("state.json").exists() {
        bail!(
            "{} already has a shoal project at {}",
            source.display(),
            dir.display()
        );
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("could not clear a half-built project")?;
    }
    for sub in ["layers", "templates", "sessions", "volumes", "checkpoints"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    git::git(&dir, &["init", "--bare", "-q", "hub.git"])?;

    let rel = match backend {
        Backend::Overlay => "layers/g1",
        Backend::Clone => "templates/g1",
    };
    let template = dir.join(rel);
    println!("Cloning {} ({branch}) into the template", source.display());
    sys::output(
        Command::new("git")
            .args(["clone", "-q", "--branch", &branch])
            .arg(&source)
            .arg(&template),
    )?;
    // Sessions share the template's packs; they must never repack them.
    git::git(&template, &["config", "gc.auto", "0"])?;
    git::git(&template, &["config", "maintenance.auto", "false"])?;

    for item in &args.copy {
        let rel_path = Path::new(item);
        if rel_path.is_absolute() || rel_path.components().any(|c| c.as_os_str() == "..") {
            bail!("--copy takes a path inside the repository: {item}");
        }
        let from = source.join(rel_path);
        if from.symlink_metadata().is_err() {
            println!("Skipping --copy {item}: not found in {}", source.display());
            continue;
        }
        let to = template.join(rel_path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if to.symlink_metadata().is_ok() {
            bail!("--copy {item}: the file is tracked by git, so it is already in the template");
        }
        sys::clone_tree(&from, &to)?;
    }

    let setup_inputs = if args.setup_inputs.is_empty() {
        DEFAULT_SETUP_INPUTS.iter().map(|s| s.to_string()).collect()
    } else {
        args.setup_inputs
    };
    if let Some(setup) = &args.setup {
        run_setup(&template, setup)?;
    }
    git::git(&template, &["status", "--porcelain"])?; // refresh the index's stat data
    let commit = git::head(&template)?;

    let mut state = State::new(
        source.clone(),
        branch,
        backend,
        args.setup,
        setup_inputs,
        args.port_base,
    );
    state.generations.push(Generation {
        id: 1,
        commit: commit.clone(),
        created_at: sys::now(),
        layers: vec![rel.to_string()],
    });
    let p = Project::create(dir, state)?;
    println!(
        "Template g1 ready: commit {}, {} on disk, {} backend, {} in total",
        git::short(&commit),
        sys::human_bytes(sys::disk_usage(&template)),
        backend.name(),
        sys::human_duration(start.elapsed())
    );
    println!("Project: {}", p.dir.display());
    println!("Next: shoal new <name>");
    Ok(())
}

fn touches_setup_inputs(changed: &[String], inputs: &[String]) -> bool {
    changed.iter().any(|path| {
        inputs
            .iter()
            .any(|input| path == input || path.ends_with(&format!("/{input}")))
    })
}

fn cmd_refresh(p: &mut Project) -> Result<()> {
    let start = Instant::now();
    let latest = p.latest().clone();
    let branch = p.state.branch.clone();
    let source_head = git::git_trim(&p.state.source, &["rev-parse", &branch])?;
    if source_head == latest.commit {
        println!(
            "Template g{} is already at {} ({branch})",
            latest.id,
            git::short(&source_head)
        );
        return Ok(());
    }
    let id = latest.id + 1;
    let (rel, build) = match p.state.backend {
        Backend::Overlay => {
            let rel = format!("layers/g{id}");
            let build = p.dir.join("build");
            let work = p.dir.join("build-work");
            for dir in [&p.abs(&rel), &build, &work] {
                if dir.exists() && dir != &build {
                    std::fs::remove_dir_all(dir)?;
                }
                std::fs::create_dir_all(dir)?;
            }
            let lowers: Vec<PathBuf> = latest.layers.iter().map(|l| p.abs(l)).collect();
            let lowers: Vec<&Path> = lowers.iter().map(PathBuf::as_path).collect();
            sys::mount_overlay(&lowers, &p.abs(&rel), &work, &build)?;
            (rel, build)
        }
        Backend::Clone => {
            let rel = format!("templates/g{id}");
            let dir = p.abs(&rel);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            sys::clone_tree(&p.abs(&latest.layers[0]), &dir)?;
            (rel.clone(), dir)
        }
    };

    let result = (|| -> Result<(usize, bool)> {
        git::git(&build, &["fetch", "-q", "origin", &branch])?;
        git::git(&build, &["reset", "-q", "--hard", "FETCH_HEAD"])?;
        let changed: Vec<String> = git::git(
            &build,
            &["diff", "--name-only", &latest.commit, &source_head],
        )?
        .lines()
        .map(str::to_string)
        .collect();
        let mut ran = false;
        if let Some(setup) = p.state.setup.clone() {
            if touches_setup_inputs(&changed, &p.state.setup_inputs) {
                run_setup(&build, &setup)?;
                ran = true;
            }
        }
        git::git(&build, &["status", "--porcelain"])?;
        Ok((changed.len(), ran))
    })();

    if p.state.backend == Backend::Overlay {
        sys::unmount(&build)?;
        std::fs::remove_dir_all(p.dir.join("build-work"))?;
        std::fs::remove_dir(&build)?;
    }
    let (changed, ran) = match result {
        Ok(r) => r,
        Err(err) => {
            let _ = std::fs::remove_dir_all(p.abs(&rel));
            return Err(err);
        }
    };

    let mut layers = vec![rel.clone()];
    if p.state.backend == Backend::Overlay {
        layers.extend(latest.layers.iter().cloned());
    }
    p.state.generations.push(Generation {
        id,
        commit: source_head.clone(),
        created_at: sys::now(),
        layers,
    });
    p.save()?;
    println!(
        "Template g{id}: {}..{}, {changed} files changed, setup {}, {} on disk, {}",
        git::short(&latest.commit),
        git::short(&source_head),
        if ran { "re-run" } else { "not needed" },
        sys::human_bytes(sys::disk_usage(&p.abs(&rel))),
        sys::human_duration(start.elapsed())
    );
    println!("New sessions use g{id}. Existing sessions stay on their generation.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

fn cmd_new(p: &mut Project, name: &str, from: Option<&str>) -> Result<()> {
    validate_name("session", name)?;
    if p.state.sessions.contains_key(name) {
        bail!("session {name} already exists");
    }
    let path = p.session_path(name);
    if path.symlink_metadata().is_ok() || p.volume_dir(name).exists() {
        bail!(
            "{} is left over from an earlier session; remove it first",
            path.display()
        );
    }
    let start = Instant::now();
    let (generation, base_commit, lowers, parent) = match from {
        None => {
            let g = p.latest().clone();
            if p.state.backend == Backend::Clone {
                sys::clone_tree(&p.abs(&g.layers[0]), &path)?;
            }
            let lowers = match p.state.backend {
                Backend::Overlay => g.layers.clone(),
                Backend::Clone => Vec::new(),
            };
            (g.id, g.commit, lowers, None)
        }
        Some(parent) => {
            let ps = p.session(parent)?.clone();
            let parent_path = p.ensure_ready(parent)?;
            sys::sync();
            let lowers = match p.state.backend {
                Backend::Overlay => {
                    // Freeze the parent's changes into a layer both sessions sit on.
                    let layer = format!("layers/fork-{parent}-{name}-{}", sys::now());
                    sys::clone_tree(&p.volume_dir(parent).join("upper"), &p.abs(&layer))?;
                    let mut lowers = vec![layer];
                    lowers.extend(ps.lowers.iter().cloned());
                    lowers
                }
                Backend::Clone => {
                    sys::clone_tree(&parent_path, &path)?;
                    Vec::new()
                }
            };
            (
                ps.generation,
                ps.base_commit,
                lowers,
                Some(parent.to_string()),
            )
        }
    };
    let session = Session {
        name: name.to_string(),
        generation,
        base_commit,
        branch: format!("shoal/{name}"),
        port: p.next_port(),
        created_at: sys::now(),
        parent,
        lowers,
    };
    let created = (|| -> Result<std::time::Duration> {
        if p.state.backend == Backend::Overlay {
            p.mount(&session)?;
        }
        let ready = start.elapsed();
        git::git(&path, &["switch", "-q", "-c", &session.branch])?;
        Ok(ready)
    })();
    let ready = match created {
        Ok(ready) => ready,
        Err(err) => {
            let _ = p.destroy_volume(name);
            return Err(err);
        }
    };
    p.state.sessions.insert(name.to_string(), session.clone());
    p.save()?;
    let origin = match &session.parent {
        Some(parent) => format!("forked from {parent}"),
        None => format!("from template g{}", session.generation),
    };
    println!(
        "Created {name} {origin} in {} (branch {} {})",
        sys::human_duration(ready),
        session.branch,
        sys::human_duration(start.elapsed() - ready)
    );
    println!("  path  {}", path.display());
    println!("  port  {}", session.port);
    Ok(())
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
        "{:<width$}  {:<4}  {:<5}  {:>7}  {:>11}  {:>8}  ORIGIN",
        "NAME", "GEN", "PORT", "COMMITS", "UNCOMMITTED", "DELTA"
    );
    for s in p.state.sessions.values() {
        let sum = summarize(p, s)?;
        println!(
            "{:<width$}  {:<4}  {:<5}  {:>7}  {:>11}  {:>8}  {}",
            s.name,
            format!("g{}", s.generation),
            s.port,
            sum.commits,
            sum.changed,
            sum.delta
                .map(sys::human_bytes)
                .unwrap_or_else(|| "-".into()),
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

fn cmd_exec(p: Project, name: &str, sandbox: bool, command: Vec<String>) -> Result<i32> {
    let path = p.ensure_ready(name)?;
    let port = p.session(name)?.port.to_string();
    let project_dir = p.dir.clone();
    drop(p); // release the project lock while the command runs
    let mut cmd = if sandbox {
        sandbox_command(&project_dir, &path, &command)?
    } else {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd
    };
    cmd.current_dir(&path)
        .env("SHOAL_SESSION", name)
        .env("SHOAL_PORT", &port)
        .env("SHOAL_PROJECT", &project_dir);
    if std::env::var_os("PORT").is_none() {
        cmd.env("PORT", &port);
    }
    sys::interactive(&mut cmd)
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

fn cmd_checkpoint(p: &mut Project, name: &str, label: Option<String>) -> Result<()> {
    let path = p.ensure_ready(name)?;
    let label = label.unwrap_or_else(|| {
        let n = p
            .state
            .checkpoints
            .iter()
            .filter(|c| c.session == name)
            .count();
        format!("cp{}", n + 1)
    });
    validate_name("checkpoint", &label)?;
    if p.state
        .checkpoints
        .iter()
        .any(|c| c.session == name && c.label == label)
    {
        bail!("{name} already has a checkpoint named {label}");
    }
    let start = Instant::now();
    let dst = p.checkpoint_dir(name, &label);
    std::fs::create_dir_all(dst.parent().expect("checkpoint dir has a parent"))?;
    sys::sync();
    match p.state.backend {
        Backend::Overlay => sys::clone_tree(&p.volume_dir(name).join("upper"), &dst)?,
        Backend::Clone => sys::clone_tree(&path, &dst)?,
    }
    let head = git::head(&path)?;
    p.state.checkpoints.push(Checkpoint {
        session: name.to_string(),
        label: label.clone(),
        created_at: sys::now(),
        head: head.clone(),
    });
    p.save()?;
    println!(
        "Checkpoint {name}@{label} at {} in {} ({} stored)",
        git::short(&head),
        sys::human_duration(start.elapsed()),
        sys::human_bytes(sys::disk_usage(&dst))
    );
    Ok(())
}

fn cmd_rewind(p: &Project, name: &str, label: &str) -> Result<()> {
    let session = p.session(name)?.clone();
    if !p
        .state
        .checkpoints
        .iter()
        .any(|c| c.session == name && c.label == label)
    {
        bail!("{name} has no checkpoint named {label}");
    }
    let start = Instant::now();
    let saved = p.checkpoint_dir(name, label);
    let path = p.session_path(name);
    match p.state.backend {
        Backend::Overlay => {
            if sys::is_mounted(&path) {
                sys::unmount(&path)?;
            }
            let volume = p.volume_dir(name);
            for dir in ["upper", "work"] {
                let dir = volume.join(dir);
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)?;
                }
            }
            sys::clone_tree(&saved, &volume.join("upper"))?;
            p.mount(&session)?;
        }
        Backend::Clone => {
            let trash = p.dir.join(format!("trash-{name}-{}", sys::now()));
            std::fs::rename(&path, &trash)?;
            sys::clone_tree(&saved, &path)?;
            std::fs::remove_dir_all(&trash)?;
        }
    }
    println!(
        "Rewound {name} to {label} (HEAD {}) in {}",
        git::short(&git::head(&path)?),
        sys::human_duration(start.elapsed())
    );
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
        let s = p.session(&name)?;
        let path = p.ensure_ready(&name)?;
        let head = git::head(&path)?;
        let dirty = !git::status_lines(&path)?.is_empty();
        let note = if dirty {
            " (it also has uncommitted changes, which stay in the session)"
        } else {
            ""
        };
        if head == s.base_commit {
            println!("{name}: no commits yet, skipped{note}");
            continue;
        }
        let commits = git::count_commits(&path, &format!("{}..HEAD", s.base_commit))?;
        let target = format!("refs/heads/shoal/{name}");
        git::git(
            &p.state.source,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &path.to_string_lossy(),
                &format!("+HEAD:{target}"),
            ],
        )?;
        println!("{name}: {commits} commit(s) imported as shoal/{name}{note}");
    }
    Ok(())
}

fn cmd_conflicts(p: &Project) -> Result<()> {
    git::require_version(2, 38)?;
    let hub = p.dir.join("hub.git");
    let mut changed: Vec<(String, BTreeSet<String>)> = Vec::new();
    for (name, s) in &p.state.sessions {
        let path = p.ensure_ready(name)?;
        // Compare what is on disk now, committed or not.
        let snapshot = git::snapshot_commit(&path)?;
        git::git(&path, &["update-ref", "refs/shoal/snapshot", &snapshot])?;
        git::git(
            &hub,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &path.to_string_lossy(),
                &format!("+refs/shoal/snapshot:refs/snapshots/{name}"),
            ],
        )?;
        let files: BTreeSet<String> = git::git(
            &hub,
            &[
                "diff",
                "--name-only",
                &s.base_commit,
                &format!("refs/snapshots/{name}"),
            ],
        )?
        .lines()
        .map(str::to_string)
        .collect();
        changed.push((name.clone(), files));
    }
    let width = changed.iter().map(|(n, _)| n.len()).max().unwrap_or(4);
    println!("Files changed, committed or not:");
    for (name, files) in &changed {
        println!("  {name:<width$}  {}", files.len());
    }
    if changed.len() < 2 {
        return Ok(());
    }
    println!();
    let mut quiet_pairs = 0;
    let mut conflicts = 0;
    for i in 0..changed.len() {
        for j in i + 1..changed.len() {
            let (a, a_files) = &changed[i];
            let (b, b_files) = &changed[j];
            let overlap: Vec<&String> = a_files.intersection(b_files).collect();
            if overlap.is_empty() {
                quiet_pairs += 1;
                continue;
            }
            let out = git::command(&hub)
                .args([
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    "--no-messages",
                    &format!("refs/snapshots/{a}"),
                    &format!("refs/snapshots/{b}"),
                ])
                .output()?;
            let pair = format!("{a} + {b}");
            match out.status.code() {
                Some(0) => println!(
                    "  {pair:<w$}  merge cleanly, both change: {}",
                    join(&overlap),
                    w = width * 2 + 3
                ),
                Some(1) => {
                    conflicts += 1;
                    let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .skip(1)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect();
                    println!(
                        "  {pair:<w$}  CONFLICT in: {}",
                        files.join(", "),
                        w = width * 2 + 3
                    );
                }
                _ => bail!(
                    "git merge-tree failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            }
        }
    }
    if quiet_pairs > 0 {
        println!("  {quiet_pairs} other pair(s) change no files in common.");
    }
    if conflicts == 0 {
        println!("\nNo conflicts.");
    }
    Ok(())
}

fn join(items: &[&String]) -> String {
    items
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

fn cmd_rm(p: &mut Project, name: &str, force: bool) -> Result<()> {
    let s = p.session(name)?.clone();
    if !force {
        let path = p.ensure_ready(name)?;
        if !git::status_lines(&path)?.is_empty() {
            bail!("{name} has uncommitted changes; commit them, or pass --force to discard them");
        }
        let head = git::head(&path)?;
        let imported = git::git(
            &p.state.source,
            &["cat-file", "-e", &format!("{head}^{{commit}}")],
        )
        .is_ok();
        if head != s.base_commit && !imported {
            bail!("{name} has commits that are not in the source repository; run `shoal import {name}`, or pass --force");
        }
    }
    p.destroy_volume(name)?;
    let checkpoints = p.dir.join("checkpoints").join(name);
    if checkpoints.exists() {
        std::fs::remove_dir_all(&checkpoints)?;
    }
    p.state.checkpoints.retain(|c| c.session != name);
    p.state.sessions.remove(name);
    p.save()?;
    let _ = git::git(
        &p.dir.join("hub.git"),
        &["update-ref", "-d", &format!("refs/snapshots/{name}")],
    );
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

fn cmd_down(p: &Project) -> Result<()> {
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
