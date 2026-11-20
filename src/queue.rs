//! A machine-wide job queue for heavy commands (tests, builds, installs).
//!
//! There are N slots, one per CPU by default, shared by every project on the
//! machine. A slot is an exclusive `flock` on `$OCEAN_GRAFT_HOME/queue/slot-<i>.lock`,
//! so a slot is freed by the kernel the moment its job exits, even if the job
//! crashed. No daemon is needed.
//!
//! Waiting jobs first take the "room" lock, so only one waiter at a time
//! competes for slots. That keeps jobs that ask for several slots from being
//! starved, and serves waiters in roughly the order they arrived.
//! Each running or waiting job also writes a small JSON file describing
//! itself, which `ocean-graft queue` reads.

use crate::project::ocean_graft_home;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub pid: u32,
    pub project: String,
    pub session: String,
    pub command: String,
    pub slots: usize,
    pub queued_at: u64,
    #[serde(default)]
    pub started_at: Option<u64>,
}

pub struct Held {
    _locks: Vec<File>,
    metadata: Vec<PathBuf>,
}

impl Drop for Held {
    fn drop(&mut self) {
        // Remove the descriptions before the locks are released.
        for path in &self.metadata {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn dir() -> Result<PathBuf> {
    let dir = ocean_graft_home()?.join("queue");
    std::fs::create_dir_all(dir.join("waiting"))
        .with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

pub fn default_slots() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

pub fn slot_count() -> Result<usize> {
    let file = dir()?.join("slots");
    match std::fs::read_to_string(&file) {
        Ok(text) => text
            .trim()
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .with_context(|| format!("{} does not hold a positive number", file.display())),
        Err(_) => Ok(default_slots()),
    }
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".into());
    let tmp = path.with_file_name(format!(".{name}.tmp.{}.{n}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

pub fn set_slot_count(n: usize) -> Result<()> {
    if n == 0 {
        bail!("the queue needs at least one slot");
    }
    write_atomic(&dir()?.join("slots"), format!("{n}\n").as_bytes())?;
    Ok(())
}

fn open_lock(path: PathBuf) -> Result<File> {
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))
}

fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Wait for `job.slots` slots and hold them until the returned value is dropped.
/// `on_wait` is called once, with the queue's state, if the job has to wait.
pub fn acquire(job: &Job, on_wait: impl FnMut(&Status)) -> Result<Held> {
    acquire_until(job, None, on_wait)
}

/// Like `acquire`, but give up with an error at `deadline`.
pub fn acquire_until(
    job: &Job,
    deadline: Option<Instant>,
    mut on_wait: impl FnMut(&Status),
) -> Result<Held> {
    let too_late = || deadline.is_some_and(|d| Instant::now() >= d);
    if too_late() {
        bail!("gave up waiting for a queue slot (time limit reached)");
    }
    let dir = dir()?;
    let total = slot_count()?;
    let want = job.slots.clamp(1, total);
    // One process (the MCP server) can queue several jobs at once.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let waiting_file = dir.join("waiting").join(format!("{}-{n}.json", job.pid));
    write_atomic(&waiting_file, &serde_json::to_vec(job)?)?;

    let result = (|| -> Result<Held> {
        let room = open_lock(dir.join("room.lock"))?;
        let mut announced = false;
        let start = Instant::now();
        // Waiters line up here. Everyone polls, so a waiter with a deadline
        // is not starved by ones blocked in the kernel.
        loop {
            if unsafe { libc::flock(room.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::EWOULDBLOCK) => {
                    if too_late() {
                        bail!("gave up waiting for a queue slot (time limit reached)");
                    }
                    if !announced && start.elapsed() > Duration::from_millis(50) {
                        announced = true;
                        if let Ok(status) = status() {
                            on_wait(&status);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => return Err(err).context("could not join the queue"),
            }
        }
        let mut locks: Vec<(usize, File)> = Vec::new();
        loop {
            for i in 0..total {
                if locks.len() == want {
                    break;
                }
                if locks.iter().any(|(held, _)| *held == i) {
                    continue;
                }
                let file = open_lock(dir.join(format!("slot-{i}.lock")))?;
                if try_lock(&file) {
                    // Clear any description left by a SIGKILLed holder before
                    // waiting for additional slots, so `queue` never shows the
                    // dead job as running while we hold this lock.
                    let _ = std::fs::remove_file(dir.join(format!("slot-{i}.json")));
                    locks.push((i, file));
                }
            }
            if locks.len() == want {
                break;
            }
            if too_late() {
                bail!("gave up waiting for a queue slot (time limit reached)");
            }
            if !announced && start.elapsed() > Duration::from_millis(50) {
                announced = true;
                if let Ok(status) = status() {
                    on_wait(&status);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(room); // let the next waiter in

        let mut running = job.clone();
        running.started_at = Some(sys::now());
        let mut metadata = Vec::new();
        for (i, _) in &locks {
            let path = dir.join(format!("slot-{i}.json"));
            write_atomic(&path, &serde_json::to_vec(&running)?)?;
            metadata.push(path);
        }
        Ok(Held {
            _locks: locks.into_iter().map(|(_, f)| f).collect(),
            metadata,
        })
    })();
    let _ = std::fs::remove_file(&waiting_file);
    result
}

pub struct Status {
    pub slots: usize,
    /// One entry per busy slot that knows which job holds it.
    pub running: Vec<(usize, Job)>,
    pub busy: usize,
    pub waiting: Vec<Job>,
}

pub fn status() -> Result<Status> {
    let dir = dir()?;
    let slots = slot_count()?;
    let mut running = Vec::new();
    let mut busy = 0;
    for i in 0..slots {
        let file = open_lock(dir.join(format!("slot-{i}.lock")))?;
        if try_lock(&file) {
            continue; // free; any description left behind is stale
        }
        busy += 1;
        if let Ok(raw) = std::fs::read(dir.join(format!("slot-{i}.json"))) {
            if let Ok(job) = serde_json::from_slice::<Job>(&raw) {
                running.push((i, job));
            }
        }
    }
    let mut waiting = Vec::new();
    for entry in std::fs::read_dir(dir.join("waiting"))?.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Ok(job) = serde_json::from_slice::<Job>(&raw) else {
            continue;
        };
        if process_alive(job.pid) {
            waiting.push(job);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    waiting.sort_by_key(|j| (j.queued_at, j.pid));
    Ok(Status {
        slots,
        running,
        busy,
        waiting,
    })
}
