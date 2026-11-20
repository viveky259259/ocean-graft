//! A project is one source repository's templates, sessions and checkpoints,
//! stored under `$OCEAN_GRAFT_HOME/projects/<name>-<hash>/`:
//!
//! ```text
//! state.json      what exists (this file is the source of truth)
//! lock            held while a command reads or changes state
//! hub.git         bare repo used to compare sessions with each other
//! layers/         overlay backend: template layers and fork layers
//! templates/      clone backend: one full template directory per generation
//! sessions/<n>/   the session's working directory (an overlay mount, or a clone)
//! volumes/<n>/    overlay backend: the session's upper and work dirs
//! checkpoints/    point-in-time copies of sessions
//! ```

use crate::git;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// overlayfs: the template is a read-only lower layer (Linux, needs root)
    Overlay,
    /// clonefile on macOS, reflink or plain copy on Linux
    Clone,
}

impl Backend {
    pub fn auto() -> Backend {
        if cfg!(target_os = "linux") && unsafe { libc::geteuid() } == 0 {
            Backend::Overlay
        } else {
            Backend::Clone
        }
    }

    pub fn check_supported(self) -> Result<()> {
        if self == Backend::Overlay {
            if !cfg!(target_os = "linux") {
                bail!("the overlay backend is only available on Linux; use --backend clone");
            }
            if unsafe { libc::geteuid() } != 0 {
                bail!("the overlay backend needs root; use --backend clone");
            }
        }
        Ok(())
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Overlay => "overlay",
            Backend::Clone => "clone",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Generation {
    pub id: u32,
    pub commit: String,
    pub created_at: u64,
    /// Paths relative to the project dir. Overlay: layers, top-most first.
    /// Clone: a single template directory.
    pub layers: Vec<String>,
}

/// A long-running process for a session, such as its dev server.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Service {
    pub name: String,
    pub command: Vec<String>,
    pub port: u16,
    /// The service runs as its own process group; this is the group id.
    pub pid: Option<i32>,
    pub started_at: u64,
    /// Stopped by `ocean-graft reap` because the session went idle.
    #[serde(default)]
    pub stopped_for_idle: bool,
    /// On-demand services listen here; a gateway owns `port` and wakes them.
    #[serde(default)]
    pub internal_port: Option<u16>,
    /// The gateway's process group, for on-demand services.
    #[serde(default)]
    pub gateway: Option<i32>,
    /// When the process `pid` (and `gateway`) started, as the OS reports it.
    /// A pid can be reused after a reboot or a long time; a mismatch means
    /// the process is no longer ours and must never be signalled.
    #[serde(default)]
    pub pid_start: Option<String>,
    #[serde(default)]
    pub gateway_start: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub name: String,
    pub generation: u32,
    pub base_commit: String,
    pub branch: String,
    pub port: u16,
    pub created_at: u64,
    pub parent: Option<String>,
    /// Overlay only: the read-only layers under this session, top-most first.
    pub lowers: Vec<String>,
    #[serde(default)]
    pub services: Vec<Service>,
    /// Last time ocean-graft ran something in the session (exec, run, serve, touch...).
    #[serde(default)]
    pub last_active: u64,
    /// Made by the WorktreeCreate hook. Claude Code never sends
    /// WorktreeRemove for the worktrees of subagents and workflow agents, so
    /// ocean-graft removes hook-made sessions itself (saving their work) once idle
    /// for a long time, unless they are `interactive`.
    #[serde(default)]
    pub hook_created: bool,
    /// A main Claude Code session ran in it: SessionStart or UserPromptSubmit
    /// fired with its cwd inside (they never fire for subagents). Such a
    /// session (`claude --worktree`, a background session) is never swept.
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Checkpoint {
    pub session: String,
    pub label: String,
    pub created_at: u64,
    pub head: String,
    /// Taken by a hook at the end of an agent turn (pruned to the newest few).
    #[serde(default)]
    pub auto: bool,
    /// The session's fingerprint when taken; an unchanged session is not
    /// checkpointed again.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct State {
    pub version: u32,
    pub source: PathBuf,
    pub branch: String,
    pub backend: Backend,
    pub setup: Option<String>,
    pub setup_inputs: Vec<String>,
    pub port_base: u16,
    pub generations: Vec<Generation>,
    pub sessions: BTreeMap<String, Session>,
    pub checkpoints: Vec<Checkpoint>,
    /// The last template refresh that failed, so that every fork does not
    /// rebuild (and fail on) the same commit again.
    #[serde(default)]
    pub refresh_failure: Option<RefreshFailure>,
    /// Sessions being copied without the project lock (a full copy, which
    /// can take minutes): their names are taken, and what they copy from
    /// is kept by gc.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Pending>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pending {
    pub name: String,
    pub pid: i32,
    pub pid_start: Option<String>,
    /// A template layer the copy reads from.
    pub source: Option<String>,
}

impl Pending {
    /// For this process, copying from `source`.
    pub fn ours(name: &str, source: Option<String>) -> Pending {
        let pid = std::process::id() as i32;
        Pending {
            name: name.to_string(),
            pid,
            pid_start: sys::process_start(pid),
            source,
        }
    }

    /// Whether the process doing the copy is still running.
    pub fn alive(&self) -> bool {
        let exists = unsafe { libc::kill(self.pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists
            && match (&self.pid_start, sys::process_start(self.pid)) {
                (Some(then), Some(now)) => *then == now,
                _ => true,
            }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RefreshFailure {
    pub commit: String,
    pub error: String,
    pub at: u64,
}

impl State {
    pub fn new(
        source: PathBuf,
        branch: String,
        backend: Backend,
        setup: Option<String>,
        setup_inputs: Vec<String>,
        port_base: u16,
    ) -> State {
        State {
            version: STATE_VERSION,
            source,
            branch,
            backend,
            setup,
            setup_inputs,
            port_base,
            generations: Vec::new(),
            sessions: BTreeMap::new(),
            checkpoints: Vec::new(),
            refresh_failure: None,
            pending: Vec::new(),
        }
    }
}

pub struct Project {
    pub dir: PathBuf,
    pub state: State,
    _lock: File,
}

pub fn ocean_graft_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("OCEAN_GRAFT_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; set OCEAN_GRAFT_HOME")?;
    Ok(PathBuf::from(home).join(".ocean-graft"))
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Where the project for a source repository lives.
pub fn project_dir_for(source: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let name: String = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let hash = fnv1a(source.as_os_str().as_bytes());
    Ok(ocean_graft_home()?
        .join("projects")
        .join(format!("{name}-{hash:08x}")))
}

/// Find the project for the current directory: inside a session, or inside a
/// source repository that has been initialised.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return dir
            .canonicalize()
            .with_context(|| format!("project {} does not exist", dir.display()));
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    if let Ok(projects) = ocean_graft_home()?.join("projects").canonicalize() {
        if let Ok(rest) = cwd.strip_prefix(&projects) {
            if let Some(first) = rest.components().next() {
                return Ok(projects.join(first));
            }
        }
    }
    let source = git::toplevel(&cwd)?;
    let dir = project_dir_for(&source)?;
    if !dir.join("state.json").exists() {
        bail!(
            "{} has no ocean-graft project yet; run `ocean-graft init` there first",
            source.display()
        );
    }
    Ok(dir)
}

fn lock(dir: &Path, wait: Option<std::time::Duration>) -> Result<File> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .with_context(|| format!("could not open the lock in {}", dir.display()))?;
    let Some(wait) = wait else {
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(file);
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err).context("could not lock the project");
            }
        }
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        if std::time::Instant::now() >= deadline {
            bail!("the project is busy (another ocean-graft command holds its lock)");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn read_state(dir: &Path) -> Result<State> {
    let raw = std::fs::read_to_string(dir.join("state.json"))
        .with_context(|| format!("no project state in {}", dir.display()))?;
    let state: State = serde_json::from_str(&raw).context("project state is unreadable")?;
    if state.version != STATE_VERSION {
        bail!("project state version {} is not supported", state.version);
    }
    Ok(state)
}

/// Every initialised project under OCEAN_GRAFT_HOME.
pub fn all_projects() -> Result<Vec<PathBuf>> {
    let projects = ocean_graft_home()?.join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Ok(Vec::new());
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("state.json").exists())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// A port held for a session being created; released when dropped.
pub struct PortReservation {
    pub port: u16,
    file: PathBuf,
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Session names that mean something else to `fork`.
pub const RESERVED_SESSION_NAMES: &[&str] = &["main", "template"];

/// Names become directory names and part of the git branch `ocean-graft/<name>`,
/// so they must be safe as both.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.ends_with(".lock")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("invalid {kind} name {name:?}: use up to 40 letters, digits, '-', '_' or '.', not starting with '.' or '-', without \"..\", and not ending in '.' or \".lock\"");
    }
    Ok(())
}

impl Project {
    pub fn create(dir: PathBuf, state: State) -> Result<Project> {
        let lock = lock(&dir, None)?;
        let project = Project {
            dir,
            state,
            _lock: lock,
        };
        project.save()?;
        Ok(project)
    }

    pub fn open(dir: PathBuf) -> Result<Project> {
        Self::open_with(dir, None)
    }

    /// Like `open`, but give up if the lock is not free within `wait`.
    pub fn open_within(dir: PathBuf, wait: std::time::Duration) -> Result<Project> {
        Self::open_with(dir, Some(wait))
    }

    fn open_with(dir: PathBuf, wait: Option<std::time::Duration>) -> Result<Project> {
        let lock = lock(&dir, wait)?;
        let state = read_state(&dir)?;
        let mut project = Project {
            dir,
            state,
            _lock: lock,
        };
        project.recover_interrupted_rewinds();
        project.recover_abandoned_copies();
        Ok(project)
    }

    /// Run `f` with the project lock released, then take it again and
    /// re-read the state, which other ocean-graft commands may have changed
    /// meanwhile. For copies that can take minutes: what they read must be
    /// protected some other way (a session lock, or a `Pending` entry).
    pub fn without_lock<R>(&mut self, f: impl FnOnce() -> R) -> Result<R> {
        let fd = self._lock.as_raw_fd();
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        let result = f();
        loop {
            if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err).context("could not lock the project again");
            }
        }
        self.state = read_state(&self.dir)?;
        Ok(result)
    }

    /// A copy whose process died (a crash, a kill) left a partial session
    /// directory and its `Pending` entry: remove both.
    fn recover_abandoned_copies(&mut self) {
        let abandoned: Vec<Pending> = self
            .state
            .pending
            .iter()
            .filter(|c| !c.alive())
            .cloned()
            .collect();
        if abandoned.is_empty() {
            return;
        }
        for c in &abandoned {
            if !self.state.sessions.contains_key(&c.name) {
                let _ = sys::remove_tree(self.session_path(&c.name));
            }
        }
        self.state
            .pending
            .retain(|c| !abandoned.iter().any(|a| a.name == c.name && a.pid == c.pid));
        let _ = self.save();
    }

    /// The overlay form: volumes/<name>/upper.restore-<pid> (a staging copy)
    /// and upper.rewind-<pid> (the layer being replaced).
    fn recover_overlay_rewinds(&self) {
        for name in self.state.sessions.keys() {
            let volume = self.volume_dir(name);
            let Ok(entries) = std::fs::read_dir(&volume) else {
                continue;
            };
            let upper = volume.join("upper");
            for entry in entries.flatten() {
                let file = entry.file_name().to_string_lossy().into_owned();
                if let Some(pid) = file.strip_prefix("view-") {
                    // A read-only view an in-place rewind left mounted.
                    if pid.parse::<u32>().is_ok() {
                        let view = entry.path();
                        if sys::is_mounted(&view) {
                            let _ = sys::unmount(&view);
                        }
                        let _ = std::fs::remove_dir(&view);
                    }
                    continue;
                }
                let result = if let Some(pid) = file.strip_prefix("upper.restore-") {
                    if pid.parse::<u32>().is_err() {
                        continue;
                    }
                    sys::remove_tree(entry.path())
                } else if let Some(pid) = file.strip_prefix("upper.rewind-") {
                    if pid.parse::<u32>().is_err() {
                        continue;
                    }
                    if upper.symlink_metadata().is_err() {
                        eprintln!(
                            "ocean-graft: restoring session {name} after an interrupted rewind"
                        );
                        std::fs::rename(entry.path(), &upper)
                    } else {
                        sys::remove_tree(entry.path())
                    }
                } else {
                    continue;
                };
                if let Err(err) = result {
                    eprintln!(
                        "ocean-graft: could not clean up {}: {err}",
                        entry.path().display()
                    );
                }
            }
        }
    }

    /// A clone-backend rewind copies the checkpoint to restore-<name>-<pid>,
    /// then swaps it in, moving the old session to trash-<name>-<pid>, all
    /// under the project lock. Holding the lock now means no rewind is
    /// running, so these are left by one that was killed: a restore copy is
    /// discarded; a trash directory is put back if the session directory is
    /// missing, and otherwise the swap finished and it is garbage.
    fn recover_interrupted_rewinds(&self) {
        self.recover_overlay_rewinds();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = file.strip_prefix("restore-") {
                // A checkpoint copy that never got swapped in.
                if rest
                    .rsplit_once('-')
                    .is_some_and(|(_, pid)| pid.parse::<u32>().is_ok())
                {
                    let _ = sys::remove_tree(entry.path());
                }
                continue;
            }
            let Some(rest) = file.strip_prefix("trash-") else {
                continue;
            };
            let Some((name, pid)) = rest.rsplit_once('-') else {
                continue;
            };
            if pid.parse::<u32>().is_err() || !self.state.sessions.contains_key(name) {
                continue;
            }
            let session = self.session_path(name);
            let result = if session.symlink_metadata().is_err() {
                eprintln!("ocean-graft: restoring session {name} after an interrupted rewind");
                std::fs::rename(entry.path(), &session)
            } else {
                sys::remove_tree(entry.path())
            };
            if let Err(err) = result {
                eprintln!(
                    "ocean-graft: could not clean up {}: {err}",
                    entry.path().display()
                );
            }
        }
    }

    /// Replace state.json atomically and durably: write a temporary file,
    /// fsync it, rename it over the old one, fsync the directory. A crash or
    /// power loss leaves either the old state or the new one, never an empty
    /// file.
    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        let tmp = self.dir.join("state.json.tmp");
        {
            let mut f =
                File::create(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
            f.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.dir.join("state.json"))?;
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub fn session(&self, name: &str) -> Result<&Session> {
        self.state
            .sessions
            .get(name)
            .with_context(|| format!("no session named {name:?} (see `ocean-graft ls`)"))
    }

    pub fn session_path(&self, name: &str) -> PathBuf {
        self.dir.join("sessions").join(name)
    }

    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.dir.join("volumes").join(name)
    }

    pub fn checkpoint_dir(&self, session: &str, label: &str) -> PathBuf {
        self.dir.join("checkpoints").join(session).join(label)
    }

    /// The session whose directory contains the current working directory.
    pub fn session_from_cwd(&self) -> Result<String> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let sessions = self.dir.join("sessions").canonicalize()?;
        cwd.strip_prefix(&sessions)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .filter(|name| self.state.sessions.contains_key(name))
            .context("not inside a session directory; name the session")
    }

    /// Record that something just happened in the session.
    pub fn mark_active(&mut self, name: &str) -> Result<()> {
        if let Some(s) = self.state.sessions.get_mut(name) {
            s.last_active = sys::now();
            self.save()?;
        }
        Ok(())
    }

    pub fn log_path(&self, session: &str, service: &str) -> PathBuf {
        self.dir
            .join("logs")
            .join(format!("{session}-{service}.log"))
    }

    pub fn latest(&self) -> &Generation {
        self.state
            .generations
            .last()
            .expect("a project always has a generation")
    }

    /// Reserve the first free block of ten ports at port_base, port_base +
    /// 10, ... (a session's own port and nine for extra services). Free
    /// means: not used by a session of any project on this machine, not
    /// reserved by a session being created right now, and not bound by
    /// anything. The reservation lasts until the returned guard is dropped,
    /// by which time the new session is recorded in state.json.
    pub fn reserve_port(&self) -> Result<PortReservation> {
        let home = ocean_graft_home()?;
        let reservations = home.join("ports");
        std::fs::create_dir_all(&reservations)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(home.join("ports.lock"))
            .context("could not open the port lock")?;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err).context("could not take the port lock");
            }
        }
        let mut used: HashSet<u16> = self.state.sessions.values().map(|s| s.port).collect();
        // Other projects' sessions, read without their locks: their
        // in-flight creations are covered by the reservations below.
        for dir in all_projects().unwrap_or_default() {
            if dir == self.dir {
                continue;
            }
            if let Some(state) = std::fs::read(dir.join("state.json"))
                .ok()
                .and_then(|raw| serde_json::from_slice::<State>(&raw).ok())
            {
                used.extend(state.sessions.values().map(|s| s.port));
            }
        }
        // Reservations of creations in progress; stale ones (their process
        // is gone) are cleared.
        if let Ok(entries) = std::fs::read_dir(&reservations) {
            for entry in entries.flatten() {
                let Ok(port) = entry.file_name().to_string_lossy().parse::<u16>() else {
                    continue;
                };
                let owner = std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
                let alive = owner.is_some_and(|pid| {
                    let rc = unsafe { libc::kill(pid, 0) };
                    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                });
                if alive {
                    used.insert(port);
                } else {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let bound = sys::port_in_use;
        let block_busy = |base: u16| (base..=base + 9).any(bound);
        let base = u32::from(self.state.port_base);
        let port = (0u32..)
            .map(|k| base + k * 10)
            .take_while(|port| *port + 9 <= u32::from(u16::MAX))
            .map(|port| port as u16)
            // Blocks of ten must not overlap any other session's block, even
            // one from a project with another port_base.
            .find(|port| {
                !used.iter().any(|u| u.abs_diff(*port) < 10) && !block_busy(*port)
            })
            .with_context(|| {
                format!(
                    "no free ports left above port_base {} ({} in use on this machine); remove sessions or change port_base",
                    self.state.port_base,
                    used.len()
                )
            })?;
        let file = reservations.join(port.to_string());
        std::fs::write(&file, std::process::id().to_string())?;
        drop(lock);
        Ok(PortReservation { port, file })
    }

    /// Make sure the session's directory is there, mounting it if needed (for
    /// example after a reboot), and return its path.
    pub fn ensure_ready(&self, name: &str) -> Result<PathBuf> {
        let session = self.session(name)?;
        let path = self.session_path(name);
        match self.state.backend {
            Backend::Overlay => {
                if !sys::is_mounted(&path) {
                    self.mount(session)?;
                }
            }
            Backend::Clone => {
                if !path.is_dir() {
                    bail!(
                        "session directory {} is missing; discard the session with keep_work: false to clear its record",
                        path.display()
                    );
                }
            }
        }
        Ok(path)
    }

    /// Overlay backend: mount the session's layers at its session directory.
    pub fn mount(&self, session: &Session) -> Result<()> {
        let volume = self.volume_dir(&session.name);
        let upper = volume.join("upper");
        let work = volume.join("work");
        let target = self.session_path(&session.name);
        for dir in [&upper, &work, &target] {
            std::fs::create_dir_all(dir)?;
        }
        let lowers: Vec<PathBuf> = session.lowers.iter().map(|l| self.abs(l)).collect();
        let lowers: Vec<&Path> = lowers.iter().map(PathBuf::as_path).collect();
        sys::mount_overlay(&lowers, &upper, &work, &target)
    }

    /// Remove a session's directory and private data. Shared layers stay.
    pub fn destroy_volume(&self, name: &str) -> Result<()> {
        let path = self.session_path(name);
        if self.state.backend == Backend::Overlay {
            if sys::is_mounted(&path) {
                sys::unmount(&path)?;
            }
            let volume = self.volume_dir(name);
            if volume.exists() {
                sys::remove_tree(&volume)?;
            }
            if path.exists() {
                std::fs::remove_dir(&path)?;
            }
        } else if path.exists() {
            sys::remove_tree(&path)?;
        }
        Ok(())
    }
}
