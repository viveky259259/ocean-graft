//! A project is one source repository's templates, sessions and checkpoints,
//! stored under `$SHOAL_HOME/projects/<name>-<hash>/`:
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
    /// Stopped by `shoal reap` because the session went idle.
    #[serde(default)]
    pub stopped_for_idle: bool,
    /// On-demand services listen here; a gateway owns `port` and wakes them.
    #[serde(default)]
    pub internal_port: Option<u16>,
    /// The gateway's process group, for on-demand services.
    #[serde(default)]
    pub gateway: Option<i32>,
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
    /// Last time shoal ran something in the session (exec, run, serve, touch...).
    #[serde(default)]
    pub last_active: u64,
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
        }
    }
}

pub struct Project {
    pub dir: PathBuf,
    pub state: State,
    _lock: File,
}

pub fn shoal_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("SHOAL_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; set SHOAL_HOME")?;
    Ok(PathBuf::from(home).join(".shoal"))
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
    Ok(shoal_home()?
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
    if let Ok(projects) = shoal_home()?.join("projects").canonicalize() {
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
            "{} has no shoal project yet; run `shoal init` there first",
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
            bail!("the project is busy (another shoal command holds its lock)");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Every initialised project under SHOAL_HOME.
pub fn all_projects() -> Result<Vec<PathBuf>> {
    let projects = shoal_home()?.join("projects");
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

pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("invalid {kind} name {name:?}: use up to 40 letters, digits, '-', '_' or '.'");
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
        let raw = std::fs::read_to_string(dir.join("state.json"))
            .with_context(|| format!("no project state in {}", dir.display()))?;
        let state: State = serde_json::from_str(&raw).context("project state is unreadable")?;
        if state.version != STATE_VERSION {
            bail!("project state version {} is not supported", state.version);
        }
        Ok(Project {
            dir,
            state,
            _lock: lock,
        })
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self.dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.state)?)?;
        std::fs::rename(&tmp, self.dir.join("state.json"))?;
        Ok(())
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub fn session(&self, name: &str) -> Result<&Session> {
        self.state
            .sessions
            .get(name)
            .with_context(|| format!("no session named {name:?} (see `shoal ls`)"))
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

    pub fn next_port(&self) -> u16 {
        let used: HashSet<u16> = self.state.sessions.values().map(|s| s.port).collect();
        (0u16..)
            .filter_map(|k| self.state.port_base.checked_add(k.checked_mul(10)?))
            .find(|port| !used.contains(port))
            .unwrap_or(self.state.port_base)
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
                    bail!("session directory {} is missing", path.display());
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
                std::fs::remove_dir_all(&volume)?;
            }
            if path.exists() {
                std::fs::remove_dir(&path)?;
            }
        } else if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        Ok(())
    }
}
