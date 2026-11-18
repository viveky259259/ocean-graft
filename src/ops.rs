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

/// The longest a setup command may run (SHOAL_SETUP_TIMEOUT_SECS), so a
/// hung install cannot hold up every fork behind the refresh lock.
fn setup_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("SHOAL_SETUP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(1200),
    )
}

fn run_setup(dir: &Path, setup: &str, budget: Option<Instant>) -> Result<()> {
    progress(format!("running setup in the template: {setup}"));
    let start = Instant::now();
    let full = setup_timeout();
    let left = budget.map(|d| d.saturating_duration_since(start));
    let (limit, cut_by_budget) = match left {
        Some(left) if left < full => (left, true),
        _ => (full, false),
    };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(setup)
        .current_dir(dir)
        .env("SHOAL_TEMPLATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .with_context(|| format!("could not run setup `{setup}`"))?;
    let pgid = child.id() as i32;
    let _owned = sys::own_group(pgid);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= limit {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = child.wait();
            if cut_by_budget {
                return Err(OverBudget::Setup.into());
            }
            bail!(
                "setup command `{setup}` did not finish within {} (SHOAL_SETUP_TIMEOUT_SECS)",
                sys::human_duration(limit)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
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
    /// Time limit for setup (OverBudget::Setup past it), for hooks.
    pub setup_deadline: Option<Instant>,
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

/// On a case-insensitive disk (macOS by default), a repository with paths
/// that differ only in case cannot be checked out faithfully: one of each
/// pair silently replaces the other, and landing would commit the damage.
fn check_case_collisions(tree: &Path) -> Result<()> {
    if !sys::case_insensitive(tree) {
        return Ok(());
    }
    let files = git::git(tree, &["ls-files", "-z"])?;
    let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for f in files.split('\0').filter(|f| !f.is_empty()) {
        if let Some(other) = seen.insert(f.to_lowercase(), f) {
            bail!(
                "the repository has paths that differ only in case ({other} and {f}), and {} does not tell them apart; put SHOAL_HOME on a case-sensitive volume",
                tree.display()
            );
        }
    }
    Ok(())
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

/// The init lock, waiting at most until `deadline`.
fn init_lock_within(dir: &Path, deadline: Option<Instant>) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let Some(deadline) = deadline else {
        return init_lock(dir);
    };
    let parent = dir.parent().context("project dir has no parent")?;
    std::fs::create_dir_all(parent)?;
    let name = dir.file_name().context("project dir has no name")?;
    let path = parent.join(format!("{}.init-lock", name.to_string_lossy()));
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        if Instant::now() >= deadline {
            return Err(OverBudget::Lock.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn init(o: InitOptions) -> Result<InitReport> {
    let start = Instant::now();
    let source = git::toplevel(&o.repo)?;
    let branch = match o.branch {
        Some(branch) => branch,
        None => {
            let symbolic = git::git_trim(&source, &["symbolic-ref", "-q", "--short", "HEAD"]);
            match symbolic {
                Ok(b) if !b.is_empty() => b,
                _ => "HEAD".to_string(),
            }
        }
    };
    if branch == "HEAD" {
        bail!(
            "{} has a detached HEAD, and shoal sessions track a branch; check out a branch there (git switch <branch>), or run `shoal init --branch <branch>`",
            source.display()
        );
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
    let _guard = init_lock_within(&dir, o.setup_deadline)?;
    if dir.join("state.json").exists() {
        bail!(
            "{} already has a shoal project at {}",
            source.display(),
            dir.display()
        );
    }
    if dir.exists() {
        sys::remove_tree(&dir).context("could not clear a half-built project")?;
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
        check_case_collisions(&template)?;
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
            // Lexical checks cannot see symlinks: resolve the entry and make
            // sure it is still inside the repository.
            let resolved = from
                .canonicalize()
                .with_context(|| format!("copy entry {item} cannot be resolved"))?;
            if !resolved.starts_with(&source) {
                bail!(
                    "copy entry {item} resolves to {}, outside the repository",
                    resolved.display()
                );
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
            run_setup(&template, setup, o.setup_deadline)?;
        }
        git::git(&template, &["status", "--porcelain"])?; // refresh the index's stat data
        git::head(&template)
    })();
    let commit = match built {
        Ok(commit) => commit,
        Err(err) => {
            let _ = sys::remove_tree(&dir);
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
    ensure_project_within(repo, None)
}

/// `ensure_project` for callers that must answer in time: waiting for
/// another first-time setup, and running setup, stop at `budget` with an
/// OverBudget error (the half-built project is cleared, as on any failure).
pub fn ensure_project_within(repo: &Path, budget: Option<Duration>) -> Result<PathBuf> {
    let deadline = budget.map(|b| Instant::now() + b);
    let source = git::toplevel(repo)?;
    let dir = project::project_dir_for(&source)?;
    if dir.join("state.json").exists() {
        return Ok(dir);
    }
    let guard = init_lock_within(&dir, deadline)?;
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
        setup_deadline: deadline,
    }) {
        Ok(_) => Ok(dir),
        // Lost a race with another agent's init: fine, use theirs.
        Err(err)
            if dir.join("state.json").exists() && err.downcast_ref::<OverBudget>().is_none() =>
        {
            Ok(dir)
        }
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
            return Err(NoProject { source }.into());
        }
        Ok(dir)
    }
}

/// A git repository that has no shoal project yet (as opposed to a bad path
/// or a broken project).
#[derive(Debug)]
pub struct NoProject {
    pub source: PathBuf,
}

impl std::fmt::Display for NoProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} has no shoal project yet; fork from it once to create one",
            self.source.display()
        )
    }
}

impl std::error::Error for NoProject {}

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

/// Serialises template refreshes of one project. Held across the slow build
/// (fetch, copy, setup) instead of the project lock, so every other command
/// keeps working while `npm ci` runs.
fn refresh_lock(dir: &Path, deadline: Option<Instant>) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("refresh.lock"))
        .context("could not open the refresh lock")?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = std::io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
        ) {
            return Err(err).context("could not take the refresh lock");
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(OverBudget::Lock.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A refresh stopped because the caller's time budget ran out (not because
/// the commit is broken): not remembered as a failed refresh.
#[derive(Debug, PartialEq)]
pub enum OverBudget {
    /// Another process was building the template, for longer than the wait.
    Lock,
    /// The setup command needed longer than the caller can wait.
    Setup,
}

impl std::fmt::Display for OverBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OverBudget::Lock => "another process is still building the template",
            OverBudget::Setup => "the setup command needs longer than this caller may wait",
        })
    }
}

/// Time limits for a refresh, for callers that must answer in time.
#[derive(Default, Clone, Copy)]
pub struct RefreshLimits {
    /// How long to wait for a refresh another process is running.
    pub lock_wait: Option<Duration>,
    /// How long setup may run here (SHOAL_SETUP_TIMEOUT_SECS still applies).
    pub setup: Option<Duration>,
}

impl std::error::Error for OverBudget {}

/// Build a new template generation if the source branch moved. Takes the
/// project lock only to read the state and to record the new generation;
/// the build itself runs without it. Callers must not hold the project lock.
/// How long a failed refresh of one commit is remembered before the next
/// fork tries it again (a flaky network may have been the cause).
const REFRESH_RETRY_AFTER: u64 = 600;

/// `force` retries even a commit whose refresh failed recently.
///
/// `budget` bounds the wait for another refresh and the setup command, for
/// callers that must answer in time (the WorktreeCreate hook); running out
/// is an OverBudget error, not remembered as a failure of the commit.
pub fn refresh(dir: &Path, force: bool, limits: RefreshLimits) -> Result<RefreshReport> {
    let start = Instant::now();
    let _refreshing = refresh_lock(dir, limits.lock_wait.map(|w| start + w))?;
    let deadline = limits.setup.map(|b| Instant::now() + b);
    let (latest, branch, backend, old_setup, old_inputs, source_head) = {
        let p = Project::open(dir.to_path_buf())?;
        let source_head = git::git_trim(&p.state.source, &["rev-parse", &p.state.branch])?;
        if let Some(failed) = &p.state.refresh_failure {
            let age = sys::now().saturating_sub(failed.at);
            if !force && failed.commit == source_head && age < REFRESH_RETRY_AFTER {
                bail!(
                    "{} ({} s ago; retried automatically after {} s, or now with `shoal refresh`)",
                    failed.error,
                    age,
                    REFRESH_RETRY_AFTER
                );
            }
        }
        (
            p.latest().clone(),
            p.state.branch.clone(),
            p.state.backend,
            p.state.setup.clone(),
            p.state.setup_inputs.clone(),
            source_head,
        )
    };
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
    let abs = |rel: &str| dir.join(rel);
    let id = latest.id + 1;
    let (rel, build) = match backend {
        Backend::Overlay => {
            let rel = format!("layers/g{id}");
            let build = dir.join("build");
            let work = dir.join("build-work");
            if sys::is_mounted(&build) {
                sys::unmount(&build)?;
            }
            for d in [abs(&rel), work.clone()] {
                if d.exists() {
                    sys::remove_tree(&d)?;
                }
                std::fs::create_dir_all(&d)?;
            }
            std::fs::create_dir_all(&build)?;
            let lowers: Vec<PathBuf> = latest.layers.iter().map(|l| abs(l)).collect();
            let lowers: Vec<&Path> = lowers.iter().map(PathBuf::as_path).collect();
            sys::mount_overlay(&lowers, &abs(&rel), &work, &build)?;
            (rel, build)
        }
        Backend::Clone => {
            let rel = format!("templates/g{id}");
            let d = abs(&rel);
            if d.exists() {
                sys::remove_tree(&d)?;
            }
            sys::clone_tree(&abs(&latest.layers[0]), &d)?;
            (rel, d)
        }
    };
    let result = (|| -> Result<(usize, bool, Option<String>, Vec<String>)> {
        git::git(&build, &["fetch", "-q", "origin", &branch])?;
        git::git(&build, &["reset", "-q", "--hard", "FETCH_HEAD"])?;
        check_case_collisions(&build)?;
        let changed: Vec<String> = git::git(
            &build,
            &["diff", "--name-only", &latest.commit, &source_head],
        )?
        .lines()
        .map(str::to_string)
        .collect();
        // shoal.toml may have changed in these commits: the committed
        // version decides the setup command from now on.
        let (setup, inputs) = if changed.iter().any(|f| f == config::FILE_NAME) {
            let cfg = config::load(&build)?;
            let inputs = if cfg.setup_inputs.is_empty() {
                DEFAULT_SETUP_INPUTS.iter().map(|s| s.to_string()).collect()
            } else {
                cfg.setup_inputs
            };
            (cfg.setup, inputs)
        } else {
            (old_setup.clone(), old_inputs.clone())
        };
        let mut ran = false;
        if let Some(cmd) = &setup {
            if setup != old_setup || touches_setup_inputs(&changed, &inputs) {
                run_setup(&build, cmd, deadline)?;
                ran = true;
            }
        }
        git::git(&build, &["status", "--porcelain"])?;
        Ok((changed.len(), ran, setup, inputs))
    })();
    if backend == Backend::Overlay {
        sys::unmount(&build)?;
        sys::remove_tree(dir.join("build-work"))?;
        std::fs::remove_dir(&build)?;
    }
    let (changed, ran, setup, inputs) = match result {
        Ok(r) => r,
        Err(err) => {
            let _ = sys::remove_tree(abs(&rel));
            if err.downcast_ref::<OverBudget>().is_some() {
                return Err(err);
            }
            if let Ok(mut p) = Project::open(dir.to_path_buf()) {
                p.state.refresh_failure = Some(project::RefreshFailure {
                    commit: source_head.clone(),
                    error: format!(
                        "refreshing the template to {} failed: {err:#}",
                        git::short(&source_head)
                    ),
                    at: sys::now(),
                });
                let _ = p.save();
            }
            return Err(err);
        }
    };
    let mut layers = vec![rel];
    if backend == Backend::Overlay {
        layers.extend(latest.layers.iter().cloned());
    }
    let mut p = Project::open(dir.to_path_buf())?;
    p.state.refresh_failure = None;
    p.state.setup = setup;
    p.state.setup_inputs = inputs;
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
        // Lower case: names become directories and branches, and macOS
        // matches file names (and loose git refs) without regard to case.
        let c = if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        // git branch names may not contain "..", and repeated separators
        // read badly anyway.
        if (c == '-' && out.ends_with('-')) || (c == '.' && out.ends_with('.')) {
            continue;
        }
        out.push(c);
    }
    let mut name: String = out
        .trim_matches(|c| matches!(c, '-' | '.' | '_'))
        .chars()
        .take(40)
        .collect();
    loop {
        let trimmed = name.trim_end_matches(['-', '.', '_']);
        let trimmed = trimmed.strip_suffix(".lock").unwrap_or(trimmed);
        if trimmed.len() == name.len() {
            break;
        }
        name = trimmed.to_string();
    }
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

/// `base` with `suffix` appended, shortening `base` so the result is still a
/// valid name of at most 40 characters.
pub fn with_suffix(base: &str, suffix: &str) -> String {
    let stem: String = base
        .chars()
        .take(40usize.saturating_sub(suffix.len()))
        .collect();
    format!("{}{suffix}", stem.trim_end_matches(['-', '.', '_']))
}

/// `base`, or `base-2`, `base-3`... whichever is free. Reserved names
/// ("main", "template") are never handed out.
pub fn unique_name(p: &Project, base: &str) -> String {
    let taken = |n: &str| {
        project::RESERVED_SESSION_NAMES.contains(&n)
            || p.state.sessions.contains_key(n)
            || p.session_path(n).symlink_metadata().is_ok()
            || p.volume_dir(n).exists()
    };
    if !taken(base) {
        return base.to_string();
    }
    for i in 2.. {
        let candidate = with_suffix(base, &format!("-{i}"));
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

/// generation, base commit, lowers, parent, from_checkpoint
type Built = (u32, String, Vec<String>, Option<String>, Option<String>);

pub fn create_session(p: &mut Project, name: &str, origin: &Origin) -> Result<SessionInfo> {
    validate_name("session", name)?;
    if project::RESERVED_SESSION_NAMES.contains(&name) {
        bail!("{name:?} is reserved (fork from \"main\" means the repository); pick another name");
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        bail!(
            "session names are lower case (on macOS, names differing only in case are the same directory): use {:?}",
            name.to_ascii_lowercase()
        );
    }
    if let Some(other) = p
        .state
        .sessions
        .keys()
        .find(|k| k.eq_ignore_ascii_case(name))
    {
        bail!("session {other} already exists");
    }
    let path = p.session_path(name);
    // Check for leftovers before deleting anything (on a case-insensitive
    // disk a differently-cased path is another session's).
    if path.symlink_metadata().is_ok() || p.volume_dir(name).exists() {
        bail!(
            "{} is left over from an earlier session; remove it first",
            path.display()
        );
    }
    // A marker left by an earlier session of the same name is not ours.
    let _ = std::fs::remove_file(interactive_marker(&p.dir, name));
    let orphan_checkpoints = p.dir.join("checkpoints").join(name);
    if orphan_checkpoints.exists()
        && !p
            .state
            .checkpoints
            .iter()
            .any(|c| c.session.eq_ignore_ascii_case(name))
    {
        // Left by a removal that crashed after forgetting the session.
        sys::remove_tree(&orphan_checkpoints)?;
    }
    // Held until this function returns, by when the session is saved.
    let reservation = p.reserve_port()?;
    let port = reservation.port;
    let start = Instant::now();
    let fork_layer = |p: &Project, parent: &str, src: &Path| -> Result<String> {
        // "+" cannot occur in names, so parent/name pairs never collide; the
        // counter covers re-forks of the same pair in the same second. Flat
        // (one directory per layer under layers/), which gc relies on.
        let stem = format!("layers/fork+{parent}+{name}+{}", sys::now());
        let mut layer = stem.clone();
        let mut i = 2;
        while p.abs(&layer).symlink_metadata().is_ok() {
            layer = format!("{stem}+{i}");
            i += 1;
        }
        if let Err(err) = sys::clone_tree(src, &p.abs(&layer)) {
            let _ = sys::remove_tree(p.abs(&layer));
            return Err(err);
        }
        Ok(layer)
    };
    let built = (|| -> Result<Built> {
        Ok(match origin {
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
                // Not while the parent is being landed: its rebase would be
                // copied half done.
                let _busy = try_session_lock(&p.dir, parent, "fork")?;
                let parent_path = p.ensure_ready(parent)?;
                let _paused = PausedServices::for_copy(p, parent);
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
        })
    })();
    let (generation, base_commit, lowers, parent, from_checkpoint) = match built {
        Ok(b) => b,
        Err(err) => {
            // The directory did not exist before (checked above): a failed
            // or partial copy must not be left to block the name.
            if !sys::is_mounted(&path) {
                let _ = sys::remove_tree(&path);
            }
            return Err(err);
        }
    };
    let session = Session {
        name: name.to_string(),
        generation,
        base_commit,
        branch: format!("shoal/{name}"),
        port,
        created_at: sys::now(),
        parent: parent.clone(),
        lowers,
        services: Vec::new(),
        last_active: sys::now(),
        hook_created: false,
        interactive: false,
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

/// A per-session lock held by `land` for its whole run (commit, rebase,
/// tests, fast-forward), so rewind and remove cannot change the session
/// underneath it. They only try it, so nothing ever waits on it while
/// holding the project lock.
fn session_lock_file(project_dir: &Path, name: &str) -> Result<std::fs::File> {
    let dir = project_dir.join("locks");
    std::fs::create_dir_all(&dir)?;
    std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{name}.lock")))
        .context("could not open the session lock")
}

fn try_session_lock(project_dir: &Path, name: &str, what: &str) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = session_lock_file(project_dir, name)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("{name} is being landed right now; {what} it after the land finishes");
    }
    Ok(file)
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
    /// Set when the session kept changing while it was copied: the
    /// checkpoint may mix files from before and after those changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub pruned: Vec<String>,
    pub elapsed_ms: u64,
}

/// Whether a checkpoint costs little: overlay sessions store only their
/// changes, and clones share blocks on APFS and on reflink filesystems.
/// On other filesystems a clone-backend checkpoint is a full copy.
pub fn checkpoints_are_cheap(p: &Project) -> bool {
    match p.state.backend {
        Backend::Overlay => true,
        // APFS clones cost nothing; off APFS (HFS+, exFAT, a network home)
        // clone_tree falls back to a full copy. Probe once and remember.
        #[cfg(target_os = "macos")]
        Backend::Clone => {
            let marker = p.dir.join(".reflink");
            if let Ok(v) = std::fs::read_to_string(&marker) {
                return v.trim() == "yes";
            }
            let ok = sys::clones_supported(&p.dir);
            let _ = std::fs::write(&marker, if ok { "yes\n" } else { "no\n" });
            ok
        }
        #[cfg(not(target_os = "macos"))]
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

/// Checkpoint a session. Refused while the session is being landed (its
/// rebase would be caught half done).
pub fn checkpoint(
    p: &mut Project,
    name: &str,
    label: Option<&str>,
    auto: bool,
) -> Result<CheckpointInfo> {
    p.session(name)?;
    let _busy = try_session_lock(&p.dir, name, "checkpoint")?;
    checkpoint_inner(p, name, label, auto)
}

/// Checkpoint without the session lock, for operations that hold it.
fn checkpoint_inner(
    p: &mut Project,
    name: &str,
    label: Option<&str>,
    auto: bool,
) -> Result<CheckpointInfo> {
    let start = Instant::now();
    let path = p.ensure_ready(name)?;
    let fingerprint = conflicts::fingerprint_with_ignored(&path).ok();
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
                    warning: None,
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
    if label == "latest" {
        bail!("\"latest\" is reserved: rewind with label \"latest\" means the newest checkpoint");
    }
    if p.state
        .checkpoints
        .iter()
        // Ignoring case: on macOS "Before" and "before" are one directory.
        .any(|c| c.session == name && c.label.eq_ignore_ascii_case(&label))
    {
        bail!("{name} already has a checkpoint named {label} (labels are compared without regard to case)");
    }
    let dst = p.checkpoint_dir(name, &label);
    if dst.symlink_metadata().is_ok() {
        // Not ours to delete: a leftover, or another label's files.
        bail!("{} already exists; pick another label", dst.display());
    }
    std::fs::create_dir_all(dst.parent().expect("checkpoint dir has a parent"))?;
    let _paused = PausedServices::for_copy(p, name);
    let src = match p.state.backend {
        Backend::Overlay => p.volume_dir(name).join("upper"),
        Backend::Clone => path.clone(),
    };
    // Writers shoal did not start (a command Claude runs, a queued job) are
    // caught by comparing HEAD and the fingerprint around the copy: a copy
    // taken while they changed things is taken again.
    let mut fingerprint = fingerprint;
    let mut head = git::head(&path)?;
    let mut warning = None;
    for attempt in 1..=CHECKPOINT_ATTEMPTS {
        sys::sync();
        if let Err(err) = sys::clone_tree(&src, &dst) {
            let _ = sys::remove_tree(&dst);
            return Err(err);
        }
        let head_after = git::head(&path)?;
        let after = conflicts::fingerprint_with_ignored(&path).ok();
        // (Where the fingerprint cannot be taken at all, HEAD alone decides.)
        if head_after == head && after == fingerprint {
            break;
        }
        head = head_after;
        if attempt == CHECKPOINT_ATTEMPTS {
            // Keep it: a checkpoint that may mix two moments beats none.
            // Without a fingerprint the next automatic one is not skipped.
            fingerprint = None;
            warning = Some(format!(
                "the session kept changing while it was copied ({CHECKPOINT_ATTEMPTS} tries), so this checkpoint may mix files from before and after those changes; take another once the session is quiet"
            ));
            break;
        }
        sys::remove_tree(&dst)
            .with_context(|| format!("removing the inconsistent copy {}", dst.display()))?;
        fingerprint = after;
    }
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
        warning,
        pruned,
        elapsed_ms: ms(start.elapsed()),
    })
}

const PRE_REWIND_KEPT: usize = 10;

/// Copies of a session that keeps changing, before keeping one with a warning.
const CHECKPOINT_ATTEMPTS: u32 = 3;

/// The session's running services, stopped with SIGSTOP until this is
/// dropped (also on an error or a panic). Only groups that are still
/// ours are signalled.
struct PausedServices(Vec<i32>);

impl PausedServices {
    /// Hold the session's services still while it is copied, so the copy
    /// is one moment of their writes (as after a crash). Not during a full
    /// copy: that can take minutes, and the user's preview would hang as
    /// long.
    fn for_copy(p: &Project, name: &str) -> PausedServices {
        if checkpoints_are_cheap(p) {
            PausedServices::pause(p, name)
        } else {
            PausedServices(Vec::new())
        }
    }

    fn pause(p: &Project, name: &str) -> PausedServices {
        let mut groups = Vec::new();
        if let Ok(s) = p.session(name) {
            for svc in &s.services {
                if let Some(pgid) = svc.pid {
                    if services::owned_group_alive(pgid, svc.pid_start.as_deref())
                        && unsafe { libc::kill(-pgid, libc::SIGSTOP) } == 0
                    {
                        groups.push(pgid);
                    }
                }
            }
        }
        PausedServices(groups)
    }
}

impl Drop for PausedServices {
    fn drop(&mut self) {
        for pgid in &self.0 {
            unsafe {
                libc::kill(-pgid, libc::SIGCONT);
            }
        }
    }
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
    // Best effort: a checkpoint that cannot be deleted keeps its record
    // (and is tried again next turn); the new checkpoint is never lost.
    let mut dropped = Vec::new();
    for label in drop {
        let dir = p.checkpoint_dir(name, &label);
        match sys::remove_tree(&dir) {
            Ok(()) => dropped.push(label),
            Err(err) => progress(format!("could not prune checkpoint {name}@{label}: {err}")),
        }
    }
    p.state
        .checkpoints
        .retain(|c| !(c.session == name && c.auto && dropped.contains(&c.label)));
    Ok(dropped)
}

#[derive(Serialize, Debug)]
pub struct RewindInfo {
    pub session: String,
    pub label: String,
    /// A checkpoint of the state just before the rewind, when it differed
    /// from the target, so the rewind itself can be undone.
    pub saved_as: Option<String>,
    pub head: String,
    /// Services that could not be restarted (the rewind itself succeeded).
    pub restart_errors: Vec<String>,
    pub restarted_services: Vec<String>,
    pub elapsed_ms: u64,
}

/// How a swap rewind ended.
enum Swap {
    /// The checkpoint is live.
    Done,
    /// Something started working in the session (or an overlay could not
    /// be unmounted): nothing was changed; restore in place instead.
    Busy,
}

/// The session is exactly as it was: what the error says did not change it.
#[derive(Debug)]
struct Unchanged;

impl std::fmt::Display for Unchanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the session is unchanged")
    }
}

impl std::error::Error for Unchanged {}

fn unchanged(err: anyhow::Error) -> anyhow::Error {
    err.context(Unchanged)
}

/// Replace the session with checkpoint `saved` by building a copy aside and
/// swapping it in with renames. Errors carry `Unchanged` only when the
/// session is provably as it was; cleanup after a successful swap is best
/// effort (Project::open finishes it).
fn restore_by_swap(p: &Project, session: &Session, saved: &Path) -> Result<Swap> {
    let name = session.name.as_str();
    let path = p.session_path(name);
    let pid = std::process::id();
    match p.state.backend {
        Backend::Overlay => {
            // Copy the checkpoint into a staging layer while the session is
            // untouched, then swap with renames. Project::open repairs a
            // rewind killed at any point: upper.restore-* is discarded;
            // upper.rewind-* is put back when upper is missing and
            // discarded otherwise.
            let volume = p.volume_dir(name);
            let upper = volume.join("upper");
            let work = volume.join("work");
            let staging = volume.join(format!("upper.restore-{pid}"));
            let aside = volume.join(format!("upper.rewind-{pid}"));
            for d in [&staging, &aside] {
                sys::remove_tree(d).map_err(|e| unchanged(e.into()))?;
            }
            if let Err(err) = sys::clone_tree(saved, &staging) {
                let _ = sys::remove_tree(&staging);
                return Err(unchanged(err));
            }
            // Last check before touching anything: the copy may have taken a
            // while, and an overlay cannot be unmounted under a process.
            if sys::is_cwd_of_a_process(&path)
                || (sys::is_mounted(&path) && sys::unmount(&path).is_err())
            {
                let _ = sys::remove_tree(&staging);
                return Ok(Swap::Busy);
            }
            let swapped = (|| -> Result<()> {
                if upper.exists() {
                    std::fs::rename(&upper, &aside)?;
                }
                std::fs::rename(&staging, &upper)?;
                sys::remove_tree(&work)?;
                std::fs::create_dir_all(&work)?;
                p.mount(session)
            })();
            if let Err(err) = swapped {
                // Put the old layer back. Only if that works is the session
                // unchanged.
                let rolled_back = (|| -> Result<()> {
                    if aside.exists() {
                        sys::remove_tree(&upper)?;
                        std::fs::rename(&aside, &upper)?;
                    }
                    let _ = sys::remove_tree(&work);
                    std::fs::create_dir_all(&work)?;
                    if !sys::is_mounted(&path) {
                        p.mount(session)?;
                    }
                    Ok(())
                })();
                let _ = sys::remove_tree(&staging);
                return Err(match rolled_back {
                    Ok(()) => unchanged(err),
                    Err(rb) => err.context(format!(
                        "and putting the old layer back failed ({rb:#}); the next shoal command repairs the session"
                    )),
                });
            }
            if let Err(err) = sys::remove_tree(&aside) {
                progress(format!(
                    "could not clean up {} ({err}); it is removed later",
                    aside.display()
                ));
            }
        }
        Backend::Clone => {
            // Copy into a staging directory first (the session is untouched
            // while the slow copy runs), then swap with two renames. If the
            // process dies at any point, Project::open puts things right:
            // restore-* is always discarded; trash-* is restored when the
            // session directory is missing and discarded otherwise.
            let staging = p.dir.join(format!("restore-{name}-{pid}"));
            let trash = p.dir.join(format!("trash-{name}-{pid}"));
            for d in [&staging, &trash] {
                sys::remove_tree(d).map_err(|e| unchanged(e.into()))?;
            }
            if let Err(err) = sys::clone_tree(saved, &staging) {
                let _ = sys::remove_tree(&staging);
                return Err(unchanged(err));
            }
            // Last check: a process that entered during the copy would be
            // left in a deleted directory by the swap.
            if sys::is_cwd_of_a_process(&path) {
                let _ = sys::remove_tree(&staging);
                return Ok(Swap::Busy);
            }
            std::fs::rename(&path, &trash).map_err(|e| unchanged(e.into()))?;
            if let Err(err) = std::fs::rename(&staging, &path) {
                return Err(match std::fs::rename(&trash, &path) {
                    Ok(()) => {
                        let _ = sys::remove_tree(&staging);
                        unchanged(err.into())
                    }
                    Err(rb) => anyhow::Error::from(err).context(format!(
                        "and moving the session back failed ({rb}); the next shoal command repairs it"
                    )),
                });
            }
            if let Err(err) = sys::remove_tree(&trash) {
                progress(format!(
                    "could not clean up {} ({err}); it is removed later",
                    trash.display()
                ));
            }
        }
    }
    Ok(Swap::Done)
}

/// Make the live session match checkpoint `saved` file by file, without
/// unmounting or moving it.
fn restore_in_place(p: &Project, session: &Session, saved: &Path) -> Result<()> {
    let path = p.ensure_ready(&session.name)?;
    match p.state.backend {
        Backend::Overlay => {
            // The checkpoint holds only the session's own layer; mount it
            // read-only over the session's lowers to see the whole tree.
            let view = p
                .volume_dir(&session.name)
                .join(format!("view-{}", std::process::id()));
            if sys::is_mounted(&view) {
                sys::unmount(&view)?;
            }
            std::fs::create_dir_all(&view)?;
            let mut lowers: Vec<PathBuf> = vec![saved.to_path_buf()];
            lowers.extend(session.lowers.iter().map(|l| p.abs(l)));
            let lowers: Vec<&Path> = lowers.iter().map(PathBuf::as_path).collect();
            sys::mount_overlay_readonly(&lowers, &view)?;
            // An entry that neither the session's own layer nor the
            // checkpoint's touches comes from the shared lowers on both
            // sides: identical, skipped without reading (node_modules).
            let upper = p.volume_dir(&session.name).join("upper");
            let identical = |rel: &Path| {
                upper.join(rel).symlink_metadata().is_err()
                    && saved.join(rel).symlink_metadata().is_err()
            };
            let synced = sys::sync_tree(&view, &path, &identical, None);
            let _ = sys::unmount(&view);
            let _ = std::fs::remove_dir(&view);
            synced
        }
        // The checkpoint directory was created when its copy started:
        // live files not written since then match it without reading.
        Backend::Clone => {
            let started = saved.metadata().and_then(|m| m.created()).ok();
            sys::sync_tree(saved, &path, &|_| false, started)
        }
    }
}

/// Start the named services of a session again; returns those started and
/// the errors of those that would not. Records their new state.
fn restart_services(p: &mut Project, name: &str, which: &[String]) -> (Vec<String>, Vec<String>) {
    let mut restarted = Vec::new();
    let mut errors = Vec::new();
    let Ok(session) = p.session(name).cloned() else {
        return (restarted, errors);
    };
    for (i, svc) in session.services.iter().enumerate() {
        if which.contains(&svc.name) && services::state(svc) != services::State::Running {
            let mut svc = svc.clone();
            match services::start(p, &session, &mut svc) {
                Ok(()) => restarted.push(svc.name.clone()),
                Err(err) => errors.push(format!("{}: {err:#}", svc.name)),
            }
            if let Some(s) = p.state.sessions.get_mut(name) {
                s.services[i] = svc;
            }
        }
    }
    let _ = p.save();
    (restarted, errors)
}

/// Forget a checkpoint and delete its files (best effort).
fn drop_checkpoint(p: &mut Project, name: &str, label: &str) {
    let _ = sys::remove_tree(p.checkpoint_dir(name, label));
    p.state
        .checkpoints
        .retain(|c| !(c.session == name && c.label == label));
    let _ = p.save();
}

/// Keep the newest PRE_REWIND_KEPT rewind safety checkpoints (labels
/// exactly pre-rewind-<number>), never pruning `keep`.
fn prune_pre_rewind(p: &mut Project, name: &str, keep: &str) -> Result<()> {
    let is_safety = |l: &str| {
        l.strip_prefix("pre-rewind-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let matching: Vec<String> = p
        .state
        .checkpoints
        .iter()
        .filter(|c| c.session == name && is_safety(&c.label) && c.label != keep)
        .map(|c| c.label.clone())
        .collect();
    let keep_n = PRE_REWIND_KEPT.saturating_sub(usize::from(is_safety(keep)));
    if matching.len() <= keep_n {
        return Ok(());
    }
    for label in &matching[..matching.len() - keep_n] {
        drop_checkpoint(p, name, label);
    }
    Ok(())
}

/// `label` may be "latest" for the newest checkpoint.
pub fn rewind(p: &mut Project, name: &str, label: &str) -> Result<RewindInfo> {
    let session = p.session(name)?.clone();
    let label = if label == "latest" {
        p.state
            .checkpoints
            .iter()
            .rev() // checkpoints are appended in the order they are taken
            // A rewind's own safety checkpoint is how to undo it, not where
            // "latest" should go: rewinding to latest twice must not bounce.
            .find(|c| c.session == name && !c.label.starts_with("pre-rewind-"))
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
    let _busy = try_session_lock(&p.dir, name, "rewind")?;
    let saved = p.checkpoint_dir(name, &label);
    if !saved.is_dir() {
        bail!(
            "checkpoint {name}@{label} is recorded but its files are missing ({})",
            saved.display()
        );
    }
    // Nothing lost: keep the current state as a checkpoint first, so the
    // rewind can be undone. Always: no cheap fingerprint sees everything a
    // rewind replaces (stashes, other branches, deep inside ignored trees).
    let safety = next_label(p, name, "pre-rewind-");
    let saved_as = checkpoint_inner(p, name, Some(&safety), false)?.label;
    let start = Instant::now();
    let path = p.session_path(name);
    // Stop the session's own services first: they run in it, and must not
    // count as "a process working in the session".
    if let Err(err) = services::stop_session(p, name, None, false, true) {
        let _ = restart_services(p, name, &restart);
        if let Some(l) = &saved_as {
            drop_checkpoint(p, name, l);
        }
        return Err(err.context("could not stop the session's services; nothing was changed"));
    }
    // A session some process works in (Claude Code's shell sits in it) is
    // restored in place: an overlay cannot be unmounted under it, and a
    // swapped directory would leave the process in a deleted one. The swap
    // checks again just before it renames anything.
    let swap = if sys::is_cwd_of_a_process(&path) {
        Ok(Swap::Busy)
    } else {
        restore_by_swap(p, &session, &saved)
    };
    let restored = match swap {
        Ok(Swap::Done) => Ok(()),
        Ok(Swap::Busy) => restore_in_place(p, &session, &saved).map_err(|err| {
            // Possibly half restored: the safety checkpoint is the way back.
            err.context(format!(
                "rewinding {name} in place failed partway; rewind to {} to get back the state before this rewind",
                saved_as.as_deref().unwrap_or("the pre-rewind checkpoint")
            ))
        }),
        Err(err) => Err(err),
    };
    if let Err(err) = restored {
        // Whatever happened, start again what the rewind stopped.
        let _ = restart_services(p, name, &restart);
        // Drop this attempt's safety checkpoint only when the session is
        // provably unchanged (so retries do not push older states out);
        // otherwise it may be the only copy of what was there.
        if err.downcast_ref::<Unchanged>().is_some() {
            if let Some(l) = &saved_as {
                drop_checkpoint(p, name, l);
            }
            return Err(err.context(format!("could not rewind {name} to {label}")));
        }
        return Err(err.context(format!(
            "rewinding {name} to {label} failed; the state before it is checkpoint {}",
            saved_as.as_deref().unwrap_or("pre-rewind")
        )));
    }
    // Only now, and never the checkpoint just restored.
    prune_pre_rewind(p, name, &label)?;
    let elapsed = start.elapsed();
    let head = git::head(&path)?;
    // The rewind is done; a service that will not start again is reported,
    // not turned into a failure of the whole call.
    let (restarted, restart_errors) = restart_services(p, name, &restart);
    p.mark_active(name)?;
    Ok(RewindInfo {
        session: name.to_string(),
        label,
        saved_as,
        restart_errors,
        head,
        restarted_services: restarted,
        elapsed_ms: ms(elapsed),
    })
}

// ---------------------------------------------------------------------------
// Getting work out
// ---------------------------------------------------------------------------

/// A git command in `dir` with the repository's identity, or shoal's when
/// none is configured (so commits and rebases never fail for want of one).
fn git_with_identity(dir: &Path) -> Command {
    let has_identity = git::git_trim(dir, &["config", "user.email"])
        .map(|e| !e.is_empty())
        .unwrap_or(false);
    let mut cmd = git::command(dir);
    if !has_identity {
        cmd.args(["-c", "user.name=shoal", "-c", "user.email=shoal@localhost"]);
    }
    cmd
}

fn commit_all(dir: &Path, message: &str) -> Result<()> {
    git::git(dir, &["add", "-A"])?;
    let mut cmd = git_with_identity(dir);
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

/// Copy the session's HEAD into the source repository as `shoal/<name>`, or
/// `shoal/<name>-2`, `-3`... when `shoal/<name>` already holds commits this
/// HEAD does not contain (an earlier session of the same name, or the user's
/// own commits). The fetch is never forced, so git itself refuses to move a
/// branch anywhere but forward.
pub(crate) fn save_branch(source: &Path, session: &Path, name: &str) -> Result<String> {
    save_rev(source, session, "HEAD", name)
}

/// `rev` of the session saved as `shoal/<name>` (or `-2`, `-3`...).
fn save_rev(source: &Path, session: &Path, rev: &str, name: &str) -> Result<String> {
    // Existing shoal/ branches, to skip names that differ only in case: on
    // macOS a loose ref shoal/api would shadow a packed shoal/Api.
    let existing: Vec<String> = git::git(
        source,
        &["for-each-ref", "--format=%(refname)", "refs/heads/shoal/"],
    )
    .map(|o| o.lines().map(str::to_string).collect())
    .unwrap_or_default();
    let mut last_err = None;
    for i in 1..=50 {
        let branch = if i == 1 {
            format!("shoal/{name}")
        } else {
            format!("shoal/{name}-{i}")
        };
        let full = format!("refs/heads/{branch}");
        if existing
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&full) && *r != full)
        {
            continue;
        }
        let fetched = git::git(
            source,
            &[
                "fetch",
                "-q",
                "--no-tags",
                &session.to_string_lossy(),
                &format!("{rev}:refs/heads/{branch}"),
            ],
        );
        match fetched {
            Ok(_) => return Ok(branch),
            Err(err) => {
                let exists = git::git(
                    source,
                    &[
                        "rev-parse",
                        "--verify",
                        "-q",
                        &format!("refs/heads/{branch}"),
                    ],
                )
                .is_ok();
                if !exists {
                    // Not a name clash: a real failure.
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.expect("at least one attempt"))
        .with_context(|| format!("could not save the work of {name} to any shoal/{name}-N branch"))
}

/// Copy the session's commits into the source repository as `shoal/<name>`.
pub fn import(p: &Project, name: &str) -> Result<ImportInfo> {
    let s = p.session(name)?;
    let path = p.ensure_ready(name)?;
    let head = git::head(&path)?;
    let uncommitted = git::status_paths(&path)?;
    if head == s.base_commit {
        return Ok(ImportInfo {
            session: name.to_string(),
            branch: None,
            commits: 0,
            uncommitted,
        });
    }
    let commits = git::count_commits(&path, &format!("{}..HEAD", s.base_commit))?;
    let branch = save_branch(&p.state.source, &path, name)?;
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
    /// Other branches and stashes in the session, saved as their own
    /// branches in the source repository.
    pub extra_branches: Vec<String>,
}

/// A rebase, merge, cherry-pick or revert the session is in the middle of.
fn operation_in_progress(path: &Path) -> Option<&'static str> {
    let git_path = |p: &str| -> Option<PathBuf> {
        let rel = git::git_trim(path, &["rev-parse", "--git-path", p]).ok()?;
        let full = if Path::new(&rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            path.join(rel)
        };
        full.symlink_metadata().is_ok().then_some(full)
    };
    [
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
    ]
    .into_iter()
    .find(|(file, _)| git_path(file).is_some())
    .map(|(_, what)| what)
}

/// Make sure nothing in the session is lost: commit uncommitted work as WIP,
/// import the branch into the source repository, and save any other local
/// branches and stashes too. Ignored files (dependencies, build output) are
/// not saved: they are reproducible by design.
pub fn save_work(p: &Project, name: &str) -> Result<SaveInfo> {
    let path = p.ensure_ready(name)?;
    if let Some(what) = operation_in_progress(&path) {
        bail!(
            "{name} is in the middle of {what}; finish it or abort it (for example `git rebase --abort` in {}) before removing the session",
            path.display()
        );
    }
    let mut committed_wip = false;
    if !git::status_lines(&path)?.is_empty() {
        commit_all(&path, &format!("WIP: work saved from shoal session {name}"))?;
        committed_wip = true;
    }
    let import = import(p, name)?;
    let mut extra_branches = Vec::new();
    let own = format!("shoal/{name}");
    let branches = git::git(
        &path,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads/",
        ],
    )?;
    // Every local branch, the session's own included: import saved HEAD,
    // which need not be on it (a detached HEAD, another branch checked
    // out). Branches whose tip the source already has are skipped below.
    let extra_name = |tag: &str| {
        // Cut first, then sanitise, so the result never ends in '.' or
        // ".lock" (invalid in a git branch name).
        let short: String = tag.chars().take(24).collect();
        let full = with_suffix(
            &sanitize_name(name),
            &format!("--{}", sanitize_name(&short)),
        );
        if validate_name("branch", &full).is_ok() {
            full
        } else {
            with_suffix(&sanitize_name(name), "--branch")
        }
    };
    let mut extras: Vec<(String, String)> = branches
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(b, h)| {
            let tag = if b == own { "branch" } else { b };
            (extra_name(tag), h.to_string())
        })
        .collect();
    let stashes = git::git(&path, &["stash", "list", "--format=%H"]).unwrap_or_default();
    for (i, h) in stashes.lines().enumerate() {
        extras.push((extra_name(&format!("stash-{i}")), h.to_string()));
    }
    for (branch_name, commit) in extras {
        // Already in the source repository (the template's own branches):
        // nothing to save.
        if git::git(
            &p.state.source,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .is_ok()
        {
            continue;
        }
        extra_branches.push(save_rev(&p.state.source, &path, &commit, &branch_name)?);
    }
    Ok(SaveInfo {
        committed_wip,
        import,
        extra_branches,
    })
}

#[derive(Serialize, Debug)]
pub struct RemoveInfo {
    pub session: String,
    pub saved: Option<SaveInfo>,
    /// The session's files were already gone; only its record was cleared.
    pub already_gone: bool,
    /// Where its checkpoints were kept, when its files were already gone
    /// (they may hold the only copy of its work).
    pub kept_checkpoints: Option<PathBuf>,
}

/// Remove a session. Without `force`, refuse if it holds work that is not in
/// the source repository. With `save`, save that work first instead.
pub fn remove(p: &mut Project, name: &str, force: bool, save: bool) -> Result<RemoveInfo> {
    let s = p.session(name)?.clone();
    let _busy = try_session_lock(&p.dir, name, "remove")?;
    let mut saved = None;
    // A session whose files are already gone (deleted by hand, a crash) has
    // nothing left to save; removing it just clears the record.
    let already_gone = match p.state.backend {
        Backend::Clone => !p.session_path(name).is_dir(),
        Backend::Overlay => !p.volume_dir(name).join("upper").is_dir(),
    };
    if already_gone {
        progress(format!(
            "{name}'s directory is already gone; clearing its record"
        ));
    } else if save {
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
    // The step that commonly fails (unmounting a busy overlay) goes first,
    // while the record still exists and the call can simply be retried.
    let session_dir = p.session_path(name);
    if p.state.backend == Backend::Overlay && sys::is_mounted(&session_dir) {
        sys::unmount(&session_dir).with_context(|| {
            format!(
                "could not unmount {name} (is a shell or editor using it?); nothing was removed"
            )
        })?;
    }
    // Then forget the session, then delete its files: a crash in between
    // leaves a stray directory (skipped by name allocation, reported on
    // reuse), never a record pointing at a half-deleted session.
    p.state.checkpoints.retain(|c| c.session != name);
    p.state.sessions.remove(name);
    p.save()?;
    conflicts::forget(&p.dir, name);
    let _ = std::fs::remove_file(interactive_marker(&p.dir, name));
    p.destroy_volume(name)?;
    let checkpoints = p.dir.join("checkpoints").join(name);
    let mut kept_checkpoints = None;
    if checkpoints.exists() {
        if already_gone && !force {
            // The checkpoints may be the only copy of this session's work
            // left: keep them where a person can find them.
            let keep = p
                .dir
                .join("orphaned-checkpoints")
                .join(format!("{name}-{}", sys::now()));
            std::fs::create_dir_all(keep.parent().expect("has a parent"))?;
            std::fs::rename(&checkpoints, &keep)?;
            progress(format!("kept {name}'s checkpoints in {}", keep.display()));
            kept_checkpoints = Some(keep);
        } else {
            sys::remove_tree(&checkpoints)?;
        }
    }
    Ok(RemoveInfo {
        session: name.to_string(),
        saved,
        already_gone,
        kept_checkpoints,
    })
}

/// Written (without the project lock) when a main Claude Code session runs
/// in session `name`; such a session is never swept.
pub fn interactive_marker(project_dir: &Path, name: &str) -> PathBuf {
    project_dir
        .join("locks")
        .join(format!("{name}.interactive"))
}

/// How long a subagent session may sit idle before `sweep_subagents`
/// removes it (its work saved first): SHOAL_WORKTREE_IDLE_HOURS, default
/// 72. Zero turns the sweep off.
pub fn subagent_idle_limit() -> Option<u64> {
    let hours = std::env::var("SHOAL_WORKTREE_IDLE_HOURS")
        .ok()
        .and_then(|h| h.trim().parse::<f64>().ok())
        .filter(|h| h.is_finite() && *h >= 0.0)
        .unwrap_or(72.0);
    (hours > 0.0).then_some((hours * 3600.0) as u64)
}

/// Remove hook-made, non-interactive sessions (subagent and workflow
/// worktrees, which Claude Code never removes) idle longer than
/// `max_idle_secs`: no running services, no process working in them, and
/// not `keep`. Work is saved to the source repository first; a session
/// that cannot be saved is left alone. At most `limit` per run.
///
/// The process scan (slow on macOS) runs once, without the project lock.
/// Each session is then checked again and removed under its own short hold
/// of the lock, so a session that came back into use is left alone.
pub fn sweep_subagents(
    dir: &Path,
    max_idle_secs: u64,
    limit: usize,
    keep: Option<&str>,
) -> Result<Vec<String>> {
    let candidates: Vec<(String, PathBuf)> = {
        let p = Project::open(dir.to_path_buf())?;
        p.state
            .sessions
            .values()
            .filter(|s| s.hook_created && !s.interactive && Some(s.name.as_str()) != keep)
            .filter(|s| !interactive_marker(&p.dir, &s.name).exists())
            .map(|s| (s.name.clone(), p.session_path(&s.name)))
            .collect()
    };
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // Cannot tell which sessions are in use: remove nothing.
    let Some(cwds) = sys::process_cwds() else {
        progress("could not list the working directories of processes; not sweeping");
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for (name, path) in candidates {
        if removed.len() >= limit {
            break;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        if cwds.iter().any(|c| c.starts_with(&canonical)) {
            continue;
        }
        let mut p = Project::open(dir.to_path_buf())?;
        let Some(s) = p.state.sessions.get(&name).cloned() else {
            continue;
        };
        let idle = sys::now().saturating_sub(services::last_activity(&p, &s));
        let running = s
            .services
            .iter()
            .any(|svc| services::state(svc) == services::State::Running);
        if s.interactive
            || interactive_marker(&p.dir, &name).exists()
            || running
            || idle <= max_idle_secs
        {
            continue;
        }
        match remove(&mut p, &name, false, true) {
            Ok(_) => removed.push(name),
            Err(err) => progress(format!("left idle session {name} in place: {err:#}")),
        }
    }
    Ok(removed)
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

#[derive(Serialize, Debug, Clone)]
pub struct LandReport {
    /// landed | nothing_to_land | uncommitted_changes | conflicts |
    /// tests_failed | timed_out | target_blocked | target_moving
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
    deadline: Instant,
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
    let _slot = queue::acquire_until(&job, Some(deadline), |status| {
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
    let _owned = sys::own_group(pgid);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
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

/// Whether checking out new file `rel` (absent at `tip`) in `source` would
/// replace something git does not track there: the path itself, or a parent
/// that is an untracked file or symlink where a directory must go. Tracked
/// parents are git's business (it refuses if the user changed them).
fn path_in_the_way(source: &Path, session: &Path, tip: &str, rel: &str) -> bool {
    if source.join(rel).symlink_metadata().is_ok() {
        // On a case-insensitive disk this may be a tracked file under
        // another case (a case-only rename): git's business, not in the way.
        let on_disk = on_disk_name(source, rel);
        let tracked = on_disk.as_deref().is_some_and(|actual| {
            actual != rel
                && git::git(session, &["cat-file", "-e", &format!("{tip}:{actual}")]).is_ok()
        });
        return !tracked;
    }
    let tracked_at_tip =
        |p: &str| git::git(session, &["cat-file", "-e", &format!("{tip}:{p}")]).is_ok();
    let parts: Vec<&str> = rel.split('/').collect();
    for i in 1..parts.len() {
        let prefix = parts[..i].join("/");
        match source.join(&prefix).symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => continue,
            Ok(_) => return !tracked_at_tip(&prefix),
            Err(_) => return false,
        }
    }
    false
}

/// The path `rel` as it is spelled on disk under `root` (the case may differ
/// from `rel` on a case-insensitive filesystem).
fn on_disk_name(root: &Path, rel: &str) -> Option<String> {
    let mut actual = Vec::new();
    let mut dir = root.to_path_buf();
    for part in rel.split('/') {
        let entry = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        dir = dir.join(&name);
        actual.push(name);
    }
    Some(actual.join("/"))
}

/// Serialises the final fast-forward of every land in one project.
/// A land ran out of its time limit somewhere other than in its tests,
/// with the report as far as it had got (rebased? checkpoint? saved?).
#[derive(Debug)]
struct OutOfTime(&'static str, Option<Box<LandReport>>);

impl std::fmt::Display for OutOfTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the land ran out of time (timeout_secs) {}", self.0)
    }
}

/// Attach the land's progress to an OutOfTime error.
fn with_progress(err: anyhow::Error, report: &LandReport) -> anyhow::Error {
    match err.downcast::<OutOfTime>() {
        Ok(OutOfTime(what, _)) => OutOfTime(what, Some(Box::new(report.clone()))).into(),
        Err(err) => err,
    }
}

impl std::error::Error for OutOfTime {}

/// Open the project within what is left of a land's time.
fn open_by(project_dir: &Path, deadline: Instant) -> Result<Project> {
    let left = deadline.saturating_duration_since(Instant::now());
    Project::open_within(
        project_dir.to_path_buf(),
        left.max(Duration::from_millis(1)),
    )
    .map_err(|err| {
        if format!("{err:#}").contains("busy") {
            OutOfTime("waiting for the project lock", None).into()
        } else {
            err
        }
    })
}

fn land_lock(project_dir: &Path, deadline: Instant) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(project_dir.join("land.lock"))
        .context("could not open the land lock")?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = std::io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
        ) {
            return Err(err).context("could not take the land lock");
        }
        if Instant::now() >= deadline {
            return Err(OutOfTime("waiting for another land to finish", None).into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Land a session's work on the target branch of the source repository:
/// commit (optional), rebase onto the target, test, fast-forward. Never
/// rewrites the target and never leaves the session mid-rebase.
pub fn land(project_dir: &Path, name: &str, o: &LandOptions) -> Result<LandReport> {
    match land_inner(project_dir, name, o) {
        // Out of time anywhere is a status Claude can act on, not an error.
        Err(err) if err.downcast_ref::<OutOfTime>().is_some() => {
            let out = err.downcast::<OutOfTime>().expect("checked above");
            let what = out.to_string();
            let mut report = match out.1 {
                Some(report) => *report,
                None => LandReport {
                    status: "timed_out",
                    session: name.to_string(),
                    // state.json is replaced atomically, so it can be read
                    // without the lock the land just failed to get.
                    target: o.into.clone().unwrap_or_else(|| {
                        std::fs::read(project_dir.join("state.json"))
                            .ok()
                            .and_then(|raw| serde_json::from_slice::<project::State>(&raw).ok())
                            .map(|s| s.branch)
                            .unwrap_or_default()
                    }),
                    commits: 0,
                    head: None,
                    files: Vec::new(),
                    tests: None,
                    checkpoint: None,
                    attempts: 0,
                    message: String::new(),
                },
            };
            report.status = "timed_out";
            report.message = match &report.checkpoint {
                Some(cp) => format!(
                    "{what}; nothing was landed on {}. The session was already rebased onto it (rewind to checkpoint {cp} to undo that). Land again, with a larger timeout_secs if needed",
                    report.target
                ),
                None => format!(
                    "{what}; nothing was landed on {}. Land again, with a larger timeout_secs if needed",
                    report.target
                ),
            };
            Ok(report)
        }
        other => other,
    }
}

fn land_inner(project_dir: &Path, name: &str, o: &LandOptions) -> Result<LandReport> {
    // test_timeout bounds the whole land: queue waits, retries and tests.
    let deadline = Instant::now() + o.test_timeout;
    let _busy = {
        let p = open_by(project_dir, deadline)?;
        p.session(name)?;
        try_session_lock(project_dir, name, "land")
            .map_err(|_| anyhow::anyhow!("{name} is already being landed"))?
    };
    let (path, source, target, port, test) = {
        let mut p = open_by(project_dir, deadline)?;
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

    let target_ref = format!("refs/heads/{target}");
    if git::git(&source, &["rev-parse", "--verify", "-q", &target_ref]).is_err() {
        bail!("the source repository has no branch {target}");
    }

    // 1. Uncommitted work.
    let dirty = git::status_paths(&path)?;
    if !dirty.is_empty() {
        match &o.commit_message {
            Some(message) if !message.trim().is_empty() => commit_all(&path, message)?,
            _ => {
                report.status = "uncommitted_changes";
                report.files = dirty;
                report.message = "the session has uncommitted changes; pass commit_message to commit them as part of landing, or commit them yourself".into();
                return Ok(report);
            }
        }
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
                let mut p =
                    open_by(project_dir, deadline).map_err(|e| with_progress(e, &report))?;
                let label = next_label(&p, name, "pre-land-");
                let cp = checkpoint_inner(&mut p, name, Some(&label), false)?;
                report.checkpoint = cp.label;
            }
            let rebase = git_with_identity(&path)
                .args(["rebase", "-q", &tip])
                .stdin(Stdio::null())
                .output()?;
            if !rebase.status.success() {
                report.files = conflicted_files(&path);
                let _ = git::git(&path, &["rebase", "--abort"]);
                if report.files.is_empty() {
                    // Not a conflict: say what git said.
                    bail!(
                        "rebasing {name} onto {target} failed; the session is unchanged: {}",
                        String::from_utf8_lossy(&rebase.stderr).trim()
                    );
                }
                report.status = "conflicts";
                report.message = format!(
                    "rebasing onto {target} conflicts; the session is unchanged. Resolve by merging {target} into the session (git fetch {} {target} && git merge FETCH_HEAD), fix the files, commit, and land again",
                    source.display()
                );
                return Ok(report);
            }
        }
        report.commits = git::count_commits(&path, &format!("{tip}..HEAD"))?;
        if report.commits == 0 {
            // The rebase dropped every commit: the target already has them.
            report.status = "nothing_to_land";
            report.message =
                format!("{target} already contains the changes in {name} (as equivalent commits)");
            return Ok(report);
        }
        // 4. Tests, through the queue and without the project lock.
        if let Some(test) = &test {
            progress(format!("running `{test}` in {name}"));
            let run = match run_tests(project_dir, name, &path, port, test, deadline) {
                Ok(run) => run,
                Err(err) if Instant::now() >= deadline => TestRun {
                    command: test.clone(),
                    passed: false,
                    exit_code: None,
                    timed_out: true,
                    duration_ms: 0,
                    output_tail: format!("{err:#}"),
                },
                Err(err) => return Err(err),
            };
            let passed = run.passed;
            let timed_out = run.timed_out;
            report.tests = Some(run);
            if timed_out {
                // Out of time (waiting for a test slot, or the tests ran
                // past the deadline): not a test failure.
                report.status = "timed_out";
                report.message = format!(
                    "the land ran out of time (timeout_secs) before the tests finished; they did not fail, and nothing was landed on {target}. Land again with a larger timeout_secs, or when the machine is less busy"
                );
                return Ok(report);
            }
            if !passed {
                report.status = "tests_failed";
                report.message = match &report.checkpoint {
                    Some(cp) => format!("tests failed after rebasing onto {target}; nothing was landed. Rewind to checkpoint {cp} to undo the rebase"),
                    None => format!("tests failed; nothing was landed on {target}"),
                };
                return Ok(report);
            }
        }
        // 5. Fast-forward the target, and nothing else. One land at a time
        // per source repository: `git merge --ff-only` writes the user's
        // working tree before it moves the branch, so two at once could
        // leave one's files checked out under the other's commit.
        let head = git::head(&path)?;
        let saved = save_branch(&source, &path, name)?;
        let _land_guard =
            land_lock(project_dir, deadline).map_err(|e| with_progress(e, &report))?;
        let checked_out = git::git_trim(&source, &["symbolic-ref", "-q", "HEAD"])
            .map(|r| r == target_ref)
            .unwrap_or(false);
        if checked_out {
            // git refuses to overwrite untracked files, but silently replaces
            // ignored ones (.env, local config). Refuse for both.
            // --no-renames: a file that arrives by rename is still a new path.
            let added = git::git(
                &path,
                &[
                    "diff",
                    "--no-renames",
                    "--name-only",
                    "-z",
                    "--diff-filter=A",
                    &tip,
                    &head,
                ],
            )?;
            let in_the_way: Vec<String> = added
                .split('\0')
                .filter(|f| !f.is_empty())
                .filter(|f| path_in_the_way(&source, &path, &tip, f))
                .map(str::to_string)
                .collect();
            if !in_the_way.is_empty() {
                report.status = "target_blocked";
                report.head = Some(head);
                report.files = in_the_way;
                report.message = format!(
                    "landing would overwrite files in the source checkout that git does not track (ignored or untracked); move them aside and land again. The work is on branch {saved} in the source repository"
                );
                return Ok(report);
            }
        }
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
                    "could not fast-forward {target}: {err:#}. The work is on branch {saved} in the source repository"
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
