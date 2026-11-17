//! Every shoal operation, implemented once.
//!
//! The CLI, the MCP server (`tools`) and the Claude Code hooks are adapters
//! over these functions. Two rules hold throughout:
//!   - nothing here writes to stdout: in MCP mode stdout is the protocol
//!     channel, and for WorktreeCreate its last line is the path. Progress
//!     goes to stderr through `progress`.
//!   - child processes get stdin from /dev/null (the MCP server's stdin is
//!     the protocol) and their stdout is sent to stderr or captured.

use crate::config;
use crate::conflicts;
use crate::git;
use crate::project::{
    self, validate_name, Backend, Checkpoint, Generation, Project, Session, State,
};
use crate::queue;
use crate::services;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_SETUP_INPUTS: &[&str] = &[
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

/// Automatic (per-turn) checkpoints kept per session.
pub const AUTO_CHECKPOINTS_KEPT: usize = 20;

pub fn progress(message: impl AsRef<str>) {
    eprintln!("shoal: {}", message.as_ref());
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

fn run_setup(dir: &Path, setup: &str) -> Result<()> {
    progress(format!("running setup in the template: {setup}"));
    let start = Instant::now();
    let status = Command::new("sh")
        .arg("-c")
        .arg(setup)
        .current_dir(dir)
        .env("SHOAL_TEMPLATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("could not run setup `{setup}`"))?;
    if !status.success() {
        bail!("setup command `{setup}` failed ({status})");
    }
    progress(format!(
        "setup finished in {}",
        sys::human_duration(start.elapsed())
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

pub struct InitOptions {
    pub repo: PathBuf,
    pub branch: Option<String>,
    pub setup: Option<String>,
    pub setup_inputs: Vec<String>,
    pub copy: Vec<String>,
    pub backend: Option<Backend>,
    pub port_base: u16,
}

#[derive(Serialize, Debug)]
pub struct InitReport {
    pub project: PathBuf,
    pub source: PathBuf,
    pub branch: String,
    pub backend: &'static str,
    pub commit: String,
    pub setup: Option<String>,
    pub template_bytes: u64,
    pub elapsed_ms: u64,
}

/// Serialises everything that creates or deletes a whole project, so two
/// agents starting at once cannot both build (or tear down) the template.
fn init_lock(dir: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let parent = dir.parent().context("project dir has no parent")?;
    std::fs::create_dir_all(parent)?;
    let name = dir.file_name().context("project dir has no name")?;
    let path = parent.join(format!("{}.init-lock", name.to_string_lossy()));
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not take the init lock");
    }
    Ok(file)
}

pub fn init(o: InitOptions) -> Result<InitReport> {
    let start = Instant::now();
    let source = git::toplevel(&o.repo)?;
    let branch = match o.branch {
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
    let backend = o.backend.unwrap_or_else(Backend::auto);
    backend.check_supported()?;
    for item in &o.copy {
        let rel = Path::new(item);
        if item.is_empty() || rel.is_absolute() || rel.components().any(|c| c.as_os_str() == "..") {
            bail!("copy entries must be paths inside the repository: {item:?}");
        }
    }

    let dir = project::project_dir_for(&source)?;
    let _guard = init_lock(&dir)?;
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
    progress(format!(
        "cloning {} ({branch}) into the template",
        source.display()
    ));
    let built = (|| -> Result<String> {
        sys::output(
            Command::new("git")
                .args(["clone", "-q", "--branch", &branch])
                .arg(&source)
                .arg(&template),
        )?;
        // Sessions share the template's packs; they must never repack them.
        git::git(&template, &["config", "gc.auto", "0"])?;
        git::git(&template, &["config", "maintenance.auto", "false"])?;
        for item in &o.copy {
            let from = source.join(item);
            if from.symlink_metadata().is_err() {
                progress(format!(
                    "skipping copy of {item}: not in {}",
                    source.display()
                ));
                continue;
            }
            let to = template.join(item);
            if to.symlink_metadata().is_ok() {
                bail!("{item} is tracked by git, so it is already in the template");
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            sys::clone_tree(&from, &to)?;
        }
        if let Some(setup) = &o.setup {
            run_setup(&template, setup)?;
        }
        git::git(&template, &["status", "--porcelain"])?; // refresh the index's stat data
        git::head(&template)
    })();
    let commit = match built {
        Ok(commit) => commit,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err);
        }
    };

    let setup_inputs = if o.setup_inputs.is_empty() {
        DEFAULT_SETUP_INPUTS.iter().map(|s| s.to_string()).collect()
    } else {
        o.setup_inputs
    };
    let mut state = State::new(
        source.clone(),
        branch.clone(),
        backend,
        o.setup.clone(),
        setup_inputs,
        o.port_base,
    );
    state.generations.push(Generation {
        id: 1,
        commit: commit.clone(),
        created_at: sys::now(),
        layers: vec![rel.to_string()],
    });
    Project::create(dir.clone(), state)?;
    Ok(InitReport {
        project: dir,
        source,
        branch,
        backend: backend.name(),
        commit,
        setup: o.setup,
        template_bytes: sys::disk_usage(&template),
        elapsed_ms: ms(start.elapsed()),
    })
}

/// The project for a repository, creating it on first use from `shoal.toml`
/// and detection. Safe to call from several agents at once.
pub fn ensure_project(repo: &Path) -> Result<PathBuf> {
    let source = git::toplevel(repo)?;
    let dir = project::project_dir_for(&source)?;
    if dir.join("state.json").exists() {
        return Ok(dir);
    }
    let guard = init_lock(&dir)?;
    if dir.join("state.json").exists() {
        return Ok(dir); // another agent finished it while we waited
    }
    drop(guard); // init takes the lock itself
    let cfg = config::load(&source)?;
    progress(format!(
        "first use in {}: building the template{}",
        source.display(),
        cfg.setup
            .as_deref()
            .map(|s| format!(" with `{s}`"))
            .unwrap_or_default()
    ));
    match init(InitOptions {
        repo: source.clone(),
        branch: None,
        setup: cfg.setup,
        setup_inputs: cfg.setup_inputs,
        copy: cfg.copy,
        backend: cfg.backend,
        port_base: cfg.port_base.unwrap_or(4100),
    }) {
        Ok(_) => Ok(dir),
        // Lost a race with another agent's init: fine, use theirs.
        Err(_) if dir.join("state.json").exists() => Ok(dir),
        Err(err) => Err(err),
    }
}

/// The project a path belongs to: a path inside a session or template, or
/// a path inside a source repository (initialising it if `create`).
pub fn project_for_path(path: &Path, create: bool) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    if let Ok(projects) = project::shoal_home()?.join("projects").canonicalize() {
        if let Ok(rest) = path.strip_prefix(&projects) {
            if let Some(first) = rest.components().next() {
                let dir = projects.join(first);
                if dir.join("state.json").exists() {
                    return Ok(dir);
                }
            }
        }
    }
    if create {
        ensure_project(&path)
    } else {
        let source = git::toplevel(&path)?;
        let dir = project::project_dir_for(&source)?;
        if !dir.join("state.json").exists() {
            bail!(
                "{} has no shoal project yet; fork from it once to create one",
                source.display()
            );
        }
        Ok(dir)
    }
}

/// The session whose directory contains `path`, if any.
pub fn session_for_path(p: &Project, path: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    let sessions = p.dir.join("sessions").canonicalize().ok()?;
    let name = path
        .strip_prefix(&sessions)
        .ok()?
        .components()
        .next()?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    p.state.sessions.contains_key(&name).then_some(name)
}

#[derive(Serialize, Debug)]
pub struct RefreshReport {
    pub up_to_date: bool,
    pub generation: u32,
    pub from: String,
    pub to: String,
    pub changed_files: usize,
    pub setup_ran: bool,
    pub elapsed_ms: u64,
}

fn touches_setup_inputs(changed: &[String], inputs: &[String]) -> bool {
    changed.iter().any(|path| {
        inputs
            .iter()
            .any(|input| path == input || path.ends_with(&format!("/{input}")))
    })
}

/// Build a new template generation if the source branch moved.
pub fn refresh(p: &mut Project) -> Result<RefreshReport> {
    let start = Instant::now();
    let latest = p.latest().clone();
    let branch = p.state.branch.clone();
    let source_head = git::git_trim(&p.state.source, &["rev-parse", &branch])?;
    if source_head == latest.commit {
        return Ok(RefreshReport {
            up_to_date: true,
            generation: latest.id,
            from: latest.commit.clone(),
            to: latest.commit,
            changed_files: 0,
            setup_ran: false,
            elapsed_ms: ms(start.elapsed()),
        });
    }
    let id = latest.id + 1;
    let (rel, build) = match p.state.backend {
        Backend::Overlay => {
            let rel = format!("layers/g{id}");
            let build = p.dir.join("build");
            let work = p.dir.join("build-work");
            if sys::is_mounted(&build) {
                sys::unmount(&build)?;
            }
            for dir in [p.abs(&rel), work.clone()] {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)?;
                }
                std::fs::create_dir_all(&dir)?;
            }
            std::fs::create_dir_all(&build)?;
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
    let mut layers = vec![rel];
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
    Ok(RefreshReport {
        up_to_date: false,
        generation: id,
        from: latest.commit,
        to: source_head,
        changed_files: changed,
        setup_ran: ran,
        elapsed_ms: ms(start.elapsed()),
    })
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Turn any label (a Claude Code worktree name, a task title) into a valid
/// session name.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches(|c| matches!(c, '-' | '.' | '_'));
    let mut name: String = trimmed.chars().take(40).collect();
    name = name.trim_end_matches(['-', '.']).to_string();
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

/// `base`, or `base-2`, `base-3`... whichever is free.
pub fn unique_name(p: &Project, base: &str) -> String {
    let taken = |n: &str| {
        p.state.sessions.contains_key(n)
            || p.session_path(n).symlink_metadata().is_ok()
            || p.volume_dir(n).exists()
    };
    if !taken(base) {
        return base.to_string();
    }
    for i in 2.. {
        let suffix = format!("-{i}");
        let stem: String = base.chars().take(40 - suffix.len()).collect();
        let candidate = format!("{}{suffix}", stem.trim_end_matches(['-', '.']));
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone)]
pub enum Origin {
    /// The latest template generation.
    Template,
    /// Another session as it is now, uncommitted and ignored files included.
    Session(String),
    /// Another session as it was at one of its checkpoints.
    Checkpoint { session: String, label: String },
}

#[derive(Serialize, Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub port: u16,
    pub generation: u32,
    pub parent: Option<String>,
    pub from_checkpoint: Option<String>,
    pub ready_ms: u64,
}

pub fn create_session(p: &mut Project, name: &str, origin: &Origin) -> Result<SessionInfo> {
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
    let fork_layer = |p: &Project, parent: &str, src: &Path| -> Result<String> {
        let layer = format!("layers/fork-{parent}-{name}-{}", sys::now());
        sys::clone_tree(src, &p.abs(&layer))?;
        Ok(layer)
    };
    let (generation, base_commit, lowers, parent, from_checkpoint) = match origin {
        Origin::Template => {
            let g = p.latest().clone();
            let lowers = match p.state.backend {
                Backend::Overlay => g.layers.clone(),
                Backend::Clone => {
                    sys::clone_tree(&p.abs(&g.layers[0]), &path)?;
                    Vec::new()
                }
            };
            (g.id, g.commit, lowers, None, None)
        }
        Origin::Session(parent) => {
            let ps = p.session(parent)?.clone();
            let parent_path = p.ensure_ready(parent)?;
            sys::sync();
            let lowers = match p.state.backend {
                Backend::Overlay => {
                    // Freeze the parent's changes into a layer both sessions sit on.
                    let layer = fork_layer(p, parent, &p.volume_dir(parent).join("upper"))?;
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
                Some(parent.clone()),
                None,
            )
        }
        Origin::Checkpoint { session, label } => {
            let ps = p.session(session)?.clone();
            if !p
                .state
                .checkpoints
                .iter()
                .any(|c| &c.session == session && &c.label == label)
            {
                bail!("{session} has no checkpoint named {label}");
            }
            let saved = p.checkpoint_dir(session, label);
            let lowers = match p.state.backend {
                Backend::Overlay => {
                    // A copy, so pruning the checkpoint later cannot break the fork.
                    let layer = fork_layer(p, session, &saved)?;
                    let mut lowers = vec![layer];
                    lowers.extend(ps.lowers.iter().cloned());
                    lowers
                }
                Backend::Clone => {
                    sys::clone_tree(&saved, &path)?;
                    Vec::new()
                }
            };
            (
                ps.generation,
                ps.base_commit,
                lowers,
                Some(session.clone()),
                Some(label.clone()),
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
        parent: parent.clone(),
        lowers,
        services: Vec::new(),
        last_active: sys::now(),
    };
    let created = (|| -> Result<Duration> {
        if p.state.backend == Backend::Overlay {
            p.mount(&session)?;
        }
        let ready = start.elapsed();
        // -C: a fork of a fork of `name` (after rm) may meet a stale branch.
        git::git(&path, &["switch", "-q", "-C", &session.branch])?;
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
    Ok(SessionInfo {
        name: name.to_string(),
        path,
        branch: session.branch,
        port: session.port,
        generation,
        parent,
        from_checkpoint,
        ready_ms: ms(ready),
    })
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct CheckpointInfo {
    pub session: String,
    pub label: Option<String>,
    pub head: Option<String>,
    pub auto: bool,
    /// Why no checkpoint was taken, when none was.
    pub skipped: Option<String>,
    pub pruned: Vec<String>,
    pub elapsed_ms: u64,
}

/// Whether a checkpoint costs little: overlay sessions store only their
/// changes, and clones share blocks on APFS and on reflink filesystems.
/// On other filesystems a clone-backend checkpoint is a full copy.
pub fn checkpoints_are_cheap(p: &Project) -> bool {
    match p.state.backend {
        Backend::Overlay => true,
        Backend::Clone if cfg!(target_os = "macos") => true,
        Backend::Clone => {
            let marker = p.dir.join(".reflink");
            if let Ok(v) = std::fs::read_to_string(&marker) {
                return v.trim() == "yes";
            }
            let a = p.dir.join(".reflink-probe-a");
            let b = p.dir.join(".reflink-probe-b");
            let _ = std::fs::write(&a, b"probe");
            let _ = std::fs::remove_file(&b);
            let ok = Command::new("cp")
                .args(["--reflink=always"])
                .arg(&a)
                .arg(&b)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
            let _ = std::fs::write(&marker, if ok { "yes\n" } else { "no\n" });
            ok
        }
    }
}

fn next_label(p: &Project, session: &str, prefix: &str) -> String {
    let n = p
        .state
        .checkpoints
        .iter()
        .filter(|c| c.session == session)
        .filter_map(|c| c.label.strip_prefix(prefix)?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}{}", n + 1)
}

pub fn checkpoint(
    p: &mut Project,
    name: &str,
    label: Option<&str>,
    auto: bool,
) -> Result<CheckpointInfo> {
    let start = Instant::now();
    let path = p.ensure_ready(name)?;
    let fingerprint = conflicts::fingerprint(&path).ok();
    if auto {
        let last = p
            .state
            .checkpoints
            .iter()
            .filter(|c| c.session == name && c.auto)
            .max_by_key(|c| c.created_at);
        if let (Some(last), Some(fp)) = (last, fingerprint.as_ref()) {
            if last.fingerprint.as_ref() == Some(fp) {
                return Ok(CheckpointInfo {
                    session: name.to_string(),
                    label: None,
                    head: None,
                    auto,
                    skipped: Some("nothing changed since the last automatic checkpoint".into()),
                    pruned: Vec::new(),
                    elapsed_ms: ms(start.elapsed()),
                });
            }
        }
    }
    let label = match label {
        Some(l) => l.to_string(),
        None if auto => next_label(p, name, "turn-"),
        None => next_label(p, name, "cp"),
    };
    validate_name("checkpoint", &label)?;
    if p.state
        .checkpoints
        .iter()
        .any(|c| c.session == name && c.label == label)
    {
        bail!("{name} already has a checkpoint named {label}");
    }
    let dst = p.checkpoint_dir(name, &label);
    std::fs::create_dir_all(dst.parent().expect("checkpoint dir has a parent"))?;
    sys::sync();
    let copied = match p.state.backend {
        Backend::Overlay => sys::clone_tree(&p.volume_dir(name).join("upper"), &dst),
        Backend::Clone => sys::clone_tree(&path, &dst),
    };
    if let Err(err) = copied {
        let _ = std::fs::remove_dir_all(&dst);
        return Err(err);
    }
    let head = git::head(&path)?;
    p.state.checkpoints.push(Checkpoint {
        session: name.to_string(),
        label: label.clone(),
        created_at: sys::now(),
        head: head.clone(),
        auto,
        fingerprint,
    });
    let pruned = if auto {
        prune_auto(p, name)?
    } else {
        Vec::new()
    };
    p.save()?;
    Ok(CheckpointInfo {
        session: name.to_string(),
        label: Some(label),
        head: Some(head),
        auto,
        skipped: None,
        pruned,
        elapsed_ms: ms(start.elapsed()),
    })
}

/// Drop the oldest automatic checkpoints beyond the newest few.
fn prune_auto(p: &mut Project, name: &str) -> Result<Vec<String>> {
    let mut autos: Vec<(u64, u32, String)> = p
        .state
        .checkpoints
        .iter()
        .filter(|c| c.session == name && c.auto)
        .map(|c| {
            let n = c
                .label
                .strip_prefix("turn-")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            (c.created_at, n, c.label.clone())
        })
        .collect();
    if autos.len() <= AUTO_CHECKPOINTS_KEPT {
        return Ok(Vec::new());
    }
    autos.sort();
    let drop: Vec<String> = autos[..autos.len() - AUTO_CHECKPOINTS_KEPT]
        .iter()
        .map(|(_, _, l)| l.clone())
        .collect();
    for label in &drop {
        let dir = p.checkpoint_dir(name, label);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
    }
    p.state
        .checkpoints
        .retain(|c| !(c.session == name && c.auto && drop.contains(&c.label)));
    Ok(drop)
}

#[derive(Serialize, Debug)]
pub struct RewindInfo {
    pub session: String,
    pub label: String,
    pub head: String,
    pub restarted_services: Vec<String>,
    pub elapsed_ms: u64,
}

/// `label` may be "latest" for the newest checkpoint.
pub fn rewind(p: &mut Project, name: &str, label: &str) -> Result<RewindInfo> {
    let session = p.session(name)?.clone();
    let label = if label == "latest" {
        p.state
            .checkpoints
            .iter()
            .filter(|c| c.session == name)
            .max_by_key(|c| (c.created_at, c.label.clone()))
            .map(|c| c.label.clone())
            .with_context(|| format!("{name} has no checkpoints"))?
    } else {
        label.to_string()
    };
    if !p
        .state
        .checkpoints
        .iter()
        .any(|c| c.session == name && c.label == label)
    {
        let known: Vec<String> = p
            .state
            .checkpoints
            .iter()
            .filter(|c| c.session == name)
            .map(|c| c.label.clone())
            .collect();
        bail!(
            "{name} has no checkpoint named {label}; it has: {}",
            if known.is_empty() {
                "none".into()
            } else {
                known.join(", ")
            }
        );
    }
    // Services hold the session open; stop them now and start them again after.
    let restart: Vec<String> = session
        .services
        .iter()
        .filter(|svc| services::state(svc) == services::State::Running)
        .map(|svc| svc.name.clone())
        .collect();
    services::stop_session(p, name, None, false, true)?;
    let start = Instant::now();
    let saved = p.checkpoint_dir(name, &label);
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
            let trash = p.dir.join(format!("trash-{name}-{}", std::process::id()));
            if trash.exists() {
                std::fs::remove_dir_all(&trash)?;
            }
            std::fs::rename(&path, &trash)?;
            if let Err(err) = sys::clone_tree(&saved, &path) {
                let _ = std::fs::remove_dir_all(&path);
                std::fs::rename(&trash, &path)?;
                return Err(err);
            }
            std::fs::remove_dir_all(&trash)?;
        }
    }
    let elapsed = start.elapsed();
    let head = git::head(&path)?;
    let mut restarted = Vec::new();
    let session = p.session(name)?.clone();
    for (i, svc) in session.services.iter().enumerate() {
        if restart.contains(&svc.name) {
            let mut svc = svc.clone();
            services::start(p, &session, &mut svc)?;
            restarted.push(svc.name.clone());
            p.state
                .sessions
                .get_mut(name)
                .expect("session exists")
                .services[i] = svc;
        }
    }
    p.mark_active(name)?;
    Ok(RewindInfo {
        session: name.to_string(),
        label,
        head,
        restarted_services: restarted,
        elapsed_ms: ms(elapsed),
    })
}

// ---------------------------------------------------------------------------
// Getting work out
// ---------------------------------------------------------------------------

/// Commit with the repository's identity, or a shoal identity if none is set.
fn commit_all(dir: &Path, message: &str) -> Result<()> {
    git::git(dir, &["add", "-A"])?;
    let has_identity = git::git_trim(dir, &["config", "user.email"])
        .map(|e| !e.is_empty())
        .unwrap_or(false);
    let mut cmd = git::command(dir);
    if !has_identity {
        cmd.args(["-c", "user.name=shoal", "-c", "user.email=shoal@localhost"]);
    }
    cmd.args(["commit", "-q", "--no-verify", "-m", message]);
    sys::output(&mut cmd)?;
    Ok(())
}

#[derive(Serialize, Debug, Default)]
pub struct ImportInfo {
    pub session: String,
    pub branch: Option<String>,
    pub commits: u32,
    pub uncommitted: Vec<String>,
}

/// Copy the session's commits into the source repository as `shoal/<name>`.
pub fn import(p: &Project, name: &str) -> Result<ImportInfo> {
    let s = p.session(name)?;
    let path = p.ensure_ready(name)?;
    let head = git::head(&path)?;
    let uncommitted = git::status_lines(&path)?;
    if head == s.base_commit {
        return Ok(ImportInfo {
            session: name.to_string(),
            branch: None,
            commits: 0,
            uncommitted,
        });
    }
    let commits = git::count_commits(&path, &format!("{}..HEAD", s.base_commit))?;
    let branch = format!("shoal/{name}");
    git::git(
        &p.state.source,
        &[
            "fetch",
            "-q",
            "--no-tags",
            &path.to_string_lossy(),
            &format!("+HEAD:refs/heads/{branch}"),
        ],
    )?;
    Ok(ImportInfo {
        session: name.to_string(),
        branch: Some(branch),
        commits,
        uncommitted,
    })
}

#[derive(Serialize, Debug, Default)]
pub struct SaveInfo {
    pub committed_wip: bool,
    pub import: ImportInfo,
}

/// Make sure nothing in the session is lost: commit uncommitted work as WIP
/// and import the branch into the source repository.
pub fn save_work(p: &Project, name: &str) -> Result<SaveInfo> {
    let path = p.ensure_ready(name)?;
    let mut committed_wip = false;
    if !git::status_lines(&path)?.is_empty() {
        commit_all(&path, &format!("WIP: work saved from shoal session {name}"))?;
        committed_wip = true;
    }
    Ok(SaveInfo {
        committed_wip,
        import: import(p, name)?,
    })
}

#[derive(Serialize, Debug)]
pub struct RemoveInfo {
    pub session: String,
    pub saved: Option<SaveInfo>,
}

/// Remove a session. Without `force`, refuse if it holds work that is not in
/// the source repository. With `save`, save that work first instead.
pub fn remove(p: &mut Project, name: &str, force: bool, save: bool) -> Result<RemoveInfo> {
    let s = p.session(name)?.clone();
    let mut saved = None;
    if save {
        saved = Some(save_work(p, name)?);
    } else if !force {
        let path = p.ensure_ready(name)?;
        let dirty = git::status_lines(&path)?;
        if !dirty.is_empty() {
            bail!(
                "{name} has uncommitted changes ({} file(s)); commit or land them, or remove with force to discard them",
                dirty.len()
            );
        }
        let head = git::head(&path)?;
        let in_source = git::git(
            &p.state.source,
            &["cat-file", "-e", &format!("{head}^{{commit}}")],
        )
        .is_ok();
        if head != s.base_commit && !in_source {
            bail!("{name} has commits that are not in the source repository; land or import them, or remove with force to discard them");
        }
    }
    services::stop_session(p, name, None, false, false)?;
    p.destroy_volume(name)?;
    let checkpoints = p.dir.join("checkpoints").join(name);
    if checkpoints.exists() {
        std::fs::remove_dir_all(&checkpoints)?;
    }
    p.state.checkpoints.retain(|c| c.session != name);
    p.state.sessions.remove(name);
    p.save()?;
    conflicts::forget(&p.dir, name);
    let _ = git::git(
        &p.dir.join("hub.git"),
        &["update-ref", "-d", &format!("refs/snapshots/{name}")],
    );
    Ok(RemoveInfo {
        session: name.to_string(),
        saved,
    })
}

// ---------------------------------------------------------------------------
// Land
// ---------------------------------------------------------------------------

pub struct LandOptions {
    /// None: use the project's test command; Some("") skips tests.
    pub test: Option<String>,
    /// Commit uncommitted work with this message before landing.
    pub commit_message: Option<String>,
    /// Target branch in the source repository (default: the tracked branch).
    pub into: Option<String>,
    pub test_timeout: Duration,
}

#[derive(Serialize, Debug, Clone)]
pub struct TestRun {
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub output_tail: String,
}

#[derive(Serialize, Debug)]
pub struct LandReport {
    /// landed | nothing_to_land | uncommitted_changes | conflicts |
    /// tests_failed | target_blocked | target_moving
    pub status: &'static str,
    pub session: String,
    pub target: String,
    pub commits: u32,
    pub head: Option<String>,
    pub files: Vec<String>,
    pub tests: Option<TestRun>,
    pub checkpoint: Option<String>,
    pub attempts: u32,
    pub message: String,
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Run the session's test command through the job queue, with no project
/// lock held. Output is captured (never sent to stdout).
fn run_tests(
    project_dir: &Path,
    name: &str,
    path: &Path,
    port: u16,
    command: &str,
    timeout: Duration,
) -> Result<TestRun> {
    let job = queue::Job {
        pid: std::process::id(),
        project: project_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        session: name.to_string(),
        command: format!("land tests: {command}"),
        slots: 1,
        queued_at: sys::now(),
        started_at: None,
    };
    let _slot = queue::acquire(&job, |status| {
        progress(format!(
            "waiting for a queue slot to run tests ({} of {} busy)",
            status.busy, status.slots
        ))
    })?;
    let log = project_dir
        .join("logs")
        .join(format!("{name}-land-tests.log"));
    std::fs::create_dir_all(log.parent().expect("log has a parent"))?;
    let out = std::fs::File::create(&log)?;
    let start = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(path)
        .env("SHOAL_SESSION", name)
        .env("SHOAL_PORT", port.to_string())
        .env("CI", "1")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not run `{command}`"))?;
    let pgid = child.id() as i32;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    Ok(TestRun {
        command: command.to_string(),
        passed: status.map(|s| s.success()).unwrap_or(false),
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms: ms(start.elapsed()),
        output_tail: tail(&output, 60),
    })
}

fn conflicted_files(path: &Path) -> Vec<String> {
    git::git(path, &["diff", "--name-only", "--diff-filter=U"])
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Land a session's work on the target branch of the source repository:
/// commit (optional), rebase onto the target, test, fast-forward. Never
/// rewrites the target and never leaves the session mid-rebase.
pub fn land(project_dir: &Path, name: &str, o: &LandOptions) -> Result<LandReport> {
    let (path, source, target, port, test) = {
        let mut p = Project::open(project_dir.to_path_buf())?;
        let path = p.ensure_ready(name)?;
        let s = p.session(name)?.clone();
        p.mark_active(name)?;
        let target = o.into.clone().unwrap_or_else(|| p.state.branch.clone());
        let test = match &o.test {
            Some(t) if t.trim().is_empty() => None,
            Some(t) => Some(t.clone()),
            None => config::load(&p.state.source)?.test,
        };
        (path, p.state.source.clone(), target, s.port, test)
    };
    let mut report = LandReport {
        status: "landed",
        session: name.to_string(),
        target: target.clone(),
        commits: 0,
        head: None,
        files: Vec::new(),
        tests: None,
        checkpoint: None,
        attempts: 0,
        message: String::new(),
    };

    // 1. Uncommitted work.
    let dirty = git::status_lines(&path)?;
    if !dirty.is_empty() {
        match &o.commit_message {
            Some(message) if !message.trim().is_empty() => commit_all(&path, message)?,
            _ => {
                report.status = "uncommitted_changes";
                report.files = dirty
                    .iter()
                    .map(|l| l[3.min(l.len())..].to_string())
                    .collect();
                report.message = "the session has uncommitted changes; pass commit_message to commit them as part of landing, or commit them yourself".into();
                return Ok(report);
            }
        }
    }

    let target_ref = format!("refs/heads/{target}");
    if git::git(&source, &["rev-parse", "--verify", "-q", &target_ref]).is_err() {
        bail!("the source repository has no branch {target}");
    }
    for attempt in 1..=3 {
        report.attempts = attempt;
        // 2. Bring the target's tip into the session.
        git::git(
            &path,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &source.to_string_lossy(),
                &target_ref,
            ],
        )?;
        let tip = git::git_trim(&path, &["rev-parse", "FETCH_HEAD"])?;
        let ahead = git::count_commits(&path, &format!("{tip}..HEAD"))?;
        if ahead == 0 {
            report.status = "nothing_to_land";
            report.message = format!("{target} already contains everything in {name}");
            return Ok(report);
        }
        // 3. Rebase onto it, with a checkpoint to come back to.
        let based = git::git(&path, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok();
        if !based {
            if report.checkpoint.is_none() {
                let mut p = Project::open(project_dir.to_path_buf())?;
                let label = next_label(&p, name, "pre-land-");
                let cp = checkpoint(&mut p, name, Some(&label), false)?;
                report.checkpoint = cp.label;
            }
            let rebase = git::command(&path)
                .args(["-c", "user.name=shoal", "-c", "user.email=shoal@localhost"])
                .args(["rebase", "-q", &tip])
                .stdin(Stdio::null())
                .output()?;
            if !rebase.status.success() {
                report.files = conflicted_files(&path);
                let _ = git::git(&path, &["rebase", "--abort"]);
                report.status = "conflicts";
                report.message = format!(
                    "rebasing onto {target} conflicts; the session is unchanged. Resolve by merging {target} into the session (git fetch {} {target} && git merge FETCH_HEAD), fix the files, commit, and land again",
                    source.display()
                );
                return Ok(report);
            }
        }
        report.commits = git::count_commits(&path, &format!("{tip}..HEAD"))?;
        // 4. Tests, through the queue and without the project lock.
        if let Some(test) = &test {
            progress(format!("running `{test}` in {name}"));
            let run = run_tests(project_dir, name, &path, port, test, o.test_timeout)?;
            let passed = run.passed;
            report.tests = Some(run);
            if !passed {
                report.status = "tests_failed";
                report.message = match &report.checkpoint {
                    Some(cp) => format!("tests failed after rebasing onto {target}; nothing was landed. Rewind to checkpoint {cp} to undo the rebase"),
                    None => format!("tests failed; nothing was landed on {target}"),
                };
                return Ok(report);
            }
        }
        // 5. Fast-forward the target, and nothing else.
        let head = git::head(&path)?;
        git::git(
            &source,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &path.to_string_lossy(),
                &format!("+HEAD:refs/heads/shoal/{name}"),
            ],
        )?;
        let checked_out = git::git_trim(&source, &["symbolic-ref", "-q", "HEAD"])
            .map(|r| r == target_ref)
            .unwrap_or(false);
        let updated = if checked_out {
            git::git(&source, &["merge", "--ff-only", "-q", &head])
        } else {
            git::git(
                &source,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    &path.to_string_lossy(),
                    &format!("{head}:{target_ref}"),
                ],
            )
        };
        match updated {
            Ok(_) => {
                report.head = Some(head);
                report.message = format!(
                    "landed {} commit(s) on {target}{}",
                    report.commits,
                    if checked_out {
                        " and updated the working tree"
                    } else {
                        ""
                    }
                );
                return Ok(report);
            }
            Err(err) => {
                let now = git::git_trim(&source, &["rev-parse", &target_ref]).unwrap_or_default();
                if now != tip {
                    progress(format!("{target} moved while landing; trying again"));
                    continue;
                }
                report.status = "target_blocked";
                report.head = Some(head);
                report.message = format!(
                    "could not fast-forward {target}: {err:#}. The work is on branch shoal/{name} in the source repository"
                );
                return Ok(report);
            }
        }
    }
    report.status = "target_moving";
    report.message = format!("{target} kept moving during three attempts; land again");
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("bold-oak-a3f2"), "bold-oak-a3f2");
        assert_eq!(sanitize_name("feature/auth login"), "feature-auth-login");
        assert_eq!(sanitize_name("#1234"), "1234");
        assert_eq!(sanitize_name("..hidden"), "hidden");
        assert_eq!(sanitize_name("-x-"), "x");
        assert_eq!(sanitize_name(""), "session");
        assert_eq!(sanitize_name("///"), "session");
        assert_eq!(sanitize_name("über café"), "ber-caf");
        let long = sanitize_name(&"a".repeat(100));
        assert_eq!(long.len(), 40);
        assert!(validate_name("session", &sanitize_name("weird!!name??")).is_ok());
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
        assert_eq!(tail("", 5), "");
    }

    #[test]
    fn setup_inputs_match_paths_and_nested_lockfiles() {
        let inputs: Vec<String> = vec!["package-lock.json".into()];
        assert!(touches_setup_inputs(&["package-lock.json".into()], &inputs));
        assert!(touches_setup_inputs(
            &["web/package-lock.json".into()],
            &inputs
        ));
        assert!(!touches_setup_inputs(
            &["src/package-lock.json.bak".into()],
            &inputs
        ));
        assert!(!touches_setup_inputs(&["README.md".into()], &inputs));
    }
}
