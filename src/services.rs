//! Long-running session services (dev servers and the like), and the activity
//! signal used to stop them when their session goes idle.
//!
//! A service runs in its own process session (`setsid`), so it survives the
//! terminal that started it, and the whole tree it spawns can be stopped with
//! one signal to its process group. Its output goes to `logs/<session>-<name>.log`
//! in the project directory.

use crate::project::{Backend, Project, Service, Session};
use crate::sys;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(PartialEq, Eq, Debug)]
pub enum State {
    Running,
    StoppedIdle,
    Stopped,
    Exited,
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Running => "running",
            State::StoppedIdle => "stopped (idle)",
            State::Stopped => "stopped",
            State::Exited => "exited",
        }
    }
}

pub fn group_alive(pgid: i32) -> bool {
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn gateway_running(service: &Service) -> bool {
    service.gateway.is_some_and(group_alive)
}

/// A longer label for listings: idle on-demand services wake on request.
pub fn describe(service: &Service) -> &'static str {
    match state(service) {
        State::StoppedIdle if gateway_running(service) => "asleep (on demand)",
        State::Running if service.gateway.is_some() => "running (on demand)",
        other => other.label(),
    }
}

/// Start the gateway that owns an on-demand service's public port.
pub fn start_gateway(p: &Project, session: &Session, service: &mut Service) -> Result<()> {
    let log_path = p
        .dir
        .join("logs")
        .join(format!("{}-{}-gateway.log", session.name, service.name));
    std::fs::create_dir_all(log_path.parent().expect("log path has a parent"))?;
    let log = File::options().create(true).append(true).open(&log_path)?;
    let mut cmd = Command::new(std::env::current_exe().context("could not locate shoal")?);
    cmd.arg("--project")
        .arg(&p.dir)
        .args(["gateway", &session.name, &service.name])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("could not start the gateway")?;
    std::thread::sleep(Duration::from_millis(200));
    if let Some(status) = child.try_wait()? {
        bail!(
            "the gateway for {} exited straight away ({status}); see {}",
            service.name,
            log_path.display()
        );
    }
    service.gateway = Some(child.id() as i32);
    Ok(())
}

pub fn stop_gateway(service: &mut Service) {
    if let Some(g) = service.gateway.take() {
        if group_alive(g) {
            unsafe { libc::kill(-g, libc::SIGTERM) };
            wait_for(&[g], Duration::from_secs(3));
            if group_alive(g) {
                unsafe { libc::kill(-g, libc::SIGKILL) };
                wait_for(&[g], Duration::from_secs(2));
            }
        }
    }
}

pub fn state(service: &Service) -> State {
    match service.pid {
        Some(pgid) if group_alive(pgid) => State::Running,
        Some(_) => State::Exited,
        None if service.stopped_for_idle => State::StoppedIdle,
        None => State::Stopped,
    }
}

/// Resident memory of every process in a process group, in KB.
pub fn group_rss_kb(pgid: i32) -> u64 {
    let Ok(out) = Command::new("ps").args(["axo", "pgid=,rss="]).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let group: i32 = parts.next()?.parse().ok()?;
            let rss: u64 = parts.next()?.parse().ok()?;
            (group == pgid).then_some(rss)
        })
        .sum()
}

/// Start `service` for `session`, recording its process group in `service`.
pub fn start(p: &Project, session: &Session, service: &mut Service) -> Result<()> {
    let path = p.session_path(&session.name);
    let log_path = p.log_path(&session.name, &service.name);
    std::fs::create_dir_all(log_path.parent().expect("log path has a parent"))?;
    let mut log = File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("could not open {}", log_path.display()))?;
    use std::io::Write;
    writeln!(
        log,
        "--- shoal: starting {} at {}: {}",
        service.name,
        sys::now(),
        service.command.join(" ")
    )?;

    let mut cmd = Command::new(&service.command[0]);
    cmd.args(&service.command[1..])
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env("SHOAL_SESSION", &session.name)
        .env("SHOAL_SERVICE", &service.name)
        .env("SHOAL_PORT", session.port.to_string())
        .env(
            "PORT",
            service.internal_port.unwrap_or(service.port).to_string(),
        )
        .env("SHOAL_PROJECT", &p.dir);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not start `{}`", service.command.join(" ")))?;
    // Catch commands that fail straight away (typos, a port already in use).
    std::thread::sleep(Duration::from_millis(300));
    if let Some(status) = child.try_wait()? {
        bail!(
            "{} exited straight away ({status}); see {}",
            service.name,
            log_path.display()
        );
    }
    service.pid = Some(child.id() as i32);
    service.started_at = sys::now();
    service.stopped_for_idle = false;
    Ok(())
}

fn wait_for(groups: &[i32], within: Duration) {
    let deadline = Instant::now() + within;
    while groups.iter().any(|g| group_alive(*g)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop several services at once: SIGTERM every process group, wait up to
/// 5 s for all of them, then SIGKILL what is left. Stopping in parallel
/// matters because real dev servers take a second or two to shut down.
/// Returns the memory each service was using (KB), in the same order.
pub fn stop_all(services: Vec<&mut Service>) -> Result<Vec<u64>> {
    let groups: Vec<i32> = services.iter().filter_map(|s| s.pid).collect();
    let rss: Vec<u64> = services
        .iter()
        .map(|s| s.pid.map(group_rss_kb).unwrap_or(0))
        .collect();
    for g in &groups {
        if group_alive(*g) {
            unsafe { libc::kill(-g, libc::SIGTERM) };
        }
    }
    wait_for(&groups, Duration::from_secs(5));
    for g in &groups {
        if group_alive(*g) {
            unsafe { libc::kill(-g, libc::SIGKILL) };
        }
    }
    wait_for(&groups, Duration::from_secs(2));
    if let Some(g) = groups.iter().find(|g| group_alive(**g)) {
        bail!("process group {g} did not stop");
    }
    for service in services {
        service.pid = None;
    }
    Ok(rss)
}

/// Directories whose churn is not a sign that someone is working: dependency
/// installs, build output, caches. Git is checked separately.
const QUIET_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".cache",
    ".venv",
    "__pycache__",
];

fn newest_mtime(root: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut newest = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(meta) = std::fs::symlink_metadata(&dir) {
            newest = newest.max(meta.mtime().max(0) as u64);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let name = entry.file_name();
                if !QUIET_DIRS.iter().any(|q| name == *q) {
                    stack.push(entry.path());
                }
            } else {
                newest = newest.max(meta.mtime().max(0) as u64);
            }
        }
    }
    // Commits, checkouts and resets show up here.
    if let Ok(meta) = std::fs::metadata(root.join(".git/logs/HEAD")) {
        newest = newest.max(meta.mtime().max(0) as u64);
    }
    newest
}

/// When the session last showed signs of work: a shoal command run in it, or
/// a change to its files or git history.
pub fn last_activity(p: &Project, session: &Session) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let files = match p.state.backend {
        // Only changed files live in the upper dir, so this is cheap.
        Backend::Overlay => newest_mtime(&p.volume_dir(&session.name).join("upper")),
        Backend::Clone => newest_mtime(&p.session_path(&session.name)),
    };
    // Requests through an on-demand gateway.
    let requests = std::fs::metadata(crate::gateway::activity_file(&p.dir, &session.name))
        .map(|m| m.mtime().max(0) as u64)
        .unwrap_or(0);
    files.max(session.last_active).max(requests)
}

/// "90", "45s", "15m", "2h" -> seconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = number
        .parse()
        .with_context(|| format!("not a duration: {text:?} (try 30s, 15m or 2h)"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => bail!("not a duration: {text:?} (try 30s, 15m or 2h)"),
    };
    Ok(Duration::from_secs(secs))
}

pub fn human_secs(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d", secs / 86400),
    }
}
