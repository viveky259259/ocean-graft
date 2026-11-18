//! `shoal hook <event>`: Claude Code hook handlers. Input is the hook's JSON
//! on stdin.
//!
//! - worktree-create / worktree-remove replace Claude Code's git worktree
//!   logic. They fail loudly (exit 1, reason on stderr): Claude Code must not
//!   carry on with a half-made directory or delete the wrong one.
//! - stop, session-start and prompt run around every turn. They always exit
//!   0, never wait long for the project lock, and only ever print the
//!   context text SessionStart asks for.

use crate::ops::{self, Origin};
use crate::project::Project;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The most a WorktreeCreate may spend preparing the template (first-time
/// setup, waiting for another refresh, running setup), all together, well
/// inside the hook's 900 s timeout in hooks.json; past it, the session
/// starts from the last good template.
const WORKTREE_REFRESH_BUDGET: Duration = Duration::from_secs(600);

/// How long a WorktreeCreate waits for a refresh another process is running
/// before starting from the last good template.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(60);

/// How long turn hooks wait for a busy project before skipping.
const TURN_HOOK_LOCK_WAIT: Duration = Duration::from_secs(5);
const MAX_INPUT: u64 = 4 * 1024 * 1024;

pub const EVENTS: &[&str] = &[
    "worktree-create",
    "worktree-remove",
    "stop",
    "session-start",
    "prompt",
];

fn read_input() -> Result<Value> {
    let mut raw = String::new();
    std::io::stdin()
        .take(MAX_INPUT)
        .read_to_string(&mut raw)
        .context("could not read the hook input")?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(&raw).context("the hook input is not JSON")
}

fn field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The directory the agent is working in.
fn agent_cwd(input: &Value) -> Option<PathBuf> {
    field(input, "cwd")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
}

/// The project and session `path` is inside, if it is inside a session.
/// A path outside any shoal project is `Ok(None)`; a project that exists but
/// cannot be opened (busy, corrupt state) is an error, so the caller can say
/// why it skipped.
fn session_at(path: &Path, wait: Option<Duration>) -> Result<Option<(Project, String)>> {
    let Ok(dir) = ops::project_for_path(path, false) else {
        return Ok(None);
    };
    let p = match wait {
        Some(w) => Project::open_within(dir, w)?,
        None => Project::open(dir)?,
    };
    Ok(ops::session_for_path(&p, path).map(|name| (p, name)))
}

/// Returns the path to print as the last line of stdout.
pub fn worktree_create(input: &Value) -> Result<PathBuf> {
    let raw = field(input, "name").unwrap_or("worktree");
    let cwd = agent_cwd(input).context("no working directory in the hook input")?;
    // Inside a session (a subagent of an agent that works in one), fork it,
    // so the subagent starts with the parent's uncommitted work.
    let (mut p, origin) = match session_at(&cwd, None)? {
        Some((p, parent)) => (p, Origin::Session(parent)),
        None => {
            let repo = std::env::var_os("CLAUDE_PROJECT_DIR")
                .map(PathBuf::from)
                .filter(|d| d.is_dir())
                .unwrap_or(cwd);
            // One budget for everything below, not one per step.
            let deadline = std::time::Instant::now() + WORKTREE_REFRESH_BUDGET;
            let left = || deadline.saturating_duration_since(std::time::Instant::now());
            let dir = match ops::ensure_project_within(&repo, Some(left())) {
                Ok(dir) => dir,
                Err(err) if err.downcast_ref::<ops::OverBudget>().is_some() => {
                    // First use, and setup is slower than a hook may wait:
                    // finish it in the background; the next worktree works.
                    spawn_detached(&["prepare".as_ref(), repo.as_os_str()]);
                    bail!(
                        "the first-time setup of {} takes longer than a worktree may wait; it goes on in the background, so try again in a few minutes",
                        repo.display()
                    );
                }
                Err(err) => return Err(err),
            };
            let limits = ops::RefreshLimits {
                lock_wait: Some(REFRESH_LOCK_WAIT.min(left())),
                setup: Some(left()),
            };
            match ops::refresh(&dir, false, limits) {
                Ok(r) if !r.up_to_date => ops::progress(format!(
                    "template refreshed to {}",
                    crate::git::short(&r.to)
                )),
                Ok(_) => {}
                // Never block worktree isolation on a refresh.
                Err(err) => {
                    if matches!(
                        err.downcast_ref::<ops::OverBudget>(),
                        Some(ops::OverBudget::Setup)
                    ) {
                        // Too slow to wait for: finish it in the background,
                        // so the following worktrees get the new template.
                        spawn_detached(&[
                            "--project".as_ref(),
                            dir.as_os_str(),
                            "refresh".as_ref(),
                        ]);
                    }
                    ops::progress(format!(
                        "could not refresh the template now ({err:#}); using the last good one"
                    ));
                }
            }
            (Project::open(dir)?, Origin::Template)
        }
    };
    let keep = match &origin {
        Origin::Session(parent) => Some(parent.clone()),
        _ => None,
    };
    let info = ops::create_session_unique(&mut p, &ops::sanitize_name(raw), &origin)?;
    let name = info.name.clone();
    // Swept when idle unless a main session turns out to run in it (see
    // mark_interactive): Claude Code never removes subagent worktrees.
    if let Some(s) = p.state.sessions.get_mut(&name) {
        s.hook_created = true;
        p.save()?;
    }
    let dir = p.dir.clone();
    drop(p);
    if ops::subagent_idle_limit().is_some() {
        start_background_sweep(&dir, keep.as_deref().unwrap_or(&name));
    }
    ops::progress(format!(
        "session {} ready in {} ms on branch {}",
        info.name, info.ready_ms, info.branch
    ));
    Ok(info.path.canonicalize().unwrap_or(info.path))
}

/// Run `shoal <args>` detached (own session, no stdio, cwd /), for work a
/// hook must not wait for. Best effort.
fn spawn_detached(args: &[&std::ffi::OsStr]) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        // Not the hook's cwd: that may be a session, which this process
        // would then keep busy (an overlay could not be unmounted).
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        // Reap it without waiting: a thread is enough, the hook exits soon.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Sweep idle subagent sessions in a detached process, so creating a
/// worktree never waits for it.
fn start_background_sweep(dir: &Path, keep: &str) {
    spawn_detached(&[
        "--project".as_ref(),
        dir.as_os_str(),
        "sweep".as_ref(),
        "--keep".as_ref(),
        keep.as_ref(),
    ]);
}

pub fn worktree_remove(input: &Value) -> Result<()> {
    let path = field(input, "worktree_path").context("no worktree_path in the hook input")?;
    let path = Path::new(path);
    if path.symlink_metadata().is_err() {
        // Already gone. If it was a shoal session, clear its record too, so
        // status does not keep reporting a broken session.
        forget_missing_session(path);
        return Ok(());
    }
    let Some((mut p, name)) = session_at(path, None)? else {
        if path.symlink_metadata().is_err() {
            return Ok(()); // removed by another hook while we waited for the lock
        }
        bail!(
            "{} is not a shoal session, so shoal will not delete it",
            path.display()
        );
    };
    if ops::session_for_path(&p, path).as_deref() != Some(name.as_str())
        || path.canonicalize()? != p.session_path(&name).canonicalize()?
    {
        bail!(
            "{} is inside session {name}, not the session itself; refusing to delete it",
            path.display()
        );
    }
    let info = ops::remove(&mut p, &name, false, true)?;
    if let Some(saved) = &info.saved {
        if let Some(branch) = &saved.import.branch {
            ops::progress(format!(
                "saved {} commit(s) of {name} as {branch} in the source repository",
                saved.import.commits
            ));
        }
    }
    Ok(())
}

fn forget_missing_session(path: &Path) {
    let (Some(name), Some(sessions)) = (path.file_name(), path.parent()) else {
        return;
    };
    let Ok(sessions) = sessions.canonicalize() else {
        return;
    };
    let Some(dir) = sessions.parent() else {
        return;
    };
    let Ok(projects) =
        crate::project::shoal_home().and_then(|h| Ok(h.join("projects").canonicalize()?))
    else {
        return;
    };
    if sessions.file_name() != Some(std::ffi::OsStr::new("sessions"))
        || dir.parent() != Some(projects.as_path())
        || !dir.join("state.json").exists()
    {
        return;
    }
    let name = name.to_string_lossy().into_owned();
    if let Ok(mut p) = Project::open(dir.to_path_buf()) {
        if p.state.sessions.contains_key(&name) && p.session_path(&name).symlink_metadata().is_err()
        {
            // Save, not force: on overlay the work lives in the volume, and a
            // missing mount point does not mean it is gone. A session with
            // nothing left is cleared without saving (already_gone).
            if let Err(err) = ops::remove(&mut p, &name, false, true) {
                ops::progress(format!("could not clear the record of {name}: {err:#}"));
            }
        }
    }
}

/// SessionStart, UserPromptSubmit and Stop (not SubagentStop) only fire for
/// a main Claude Code session, so a session they fire in is one a person
/// works in, never to be swept. Recorded first as a marker file, which
/// needs no project lock (a busy project must not make this miss), then in
/// state when the lock is free.
fn mark_interactive_at(cwd: &Path) {
    let Ok(dir) = ops::project_for_path(cwd, false) else {
        return;
    };
    let Ok(cwd) = cwd.canonicalize() else {
        return;
    };
    let Some(name) = cwd
        .strip_prefix(dir.join("sessions"))
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
    else {
        return;
    };
    let marker = ops::interactive_marker(&dir, &name);
    if marker.exists() {
        return;
    }
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, b"");
}

fn mark_interactive(p: &mut Project, name: &str) -> Result<()> {
    if let Some(s) = p.state.sessions.get_mut(name) {
        if !s.interactive {
            s.interactive = true;
            p.save()?;
        }
    }
    Ok(())
}

/// Whether a hook fired for the main session rather than a subagent.
fn main_session_event(input: &Value) -> bool {
    field(input, "agent_id").is_none()
        && !matches!(field(input, "hook_event_name"), Some("SubagentStop"))
}

/// End of a turn (Stop, SubagentStop): checkpoint the session if it changed.
pub fn stop(input: &Value) -> Result<()> {
    let Some(cwd) = agent_cwd(input) else {
        return Ok(());
    };
    // A background session, or one that moved in with EnterWorktree, never
    // fires SessionStart inside its worktree, but its Stop does.
    let main = main_session_event(input);
    if main {
        mark_interactive_at(&cwd);
    }
    let Some((mut p, name)) = session_at(&cwd, Some(TURN_HOOK_LOCK_WAIT))? else {
        return Ok(());
    };
    if main {
        mark_interactive(&mut p, &name)?;
    }
    p.mark_active(&name)?;
    if !ops::checkpoints_are_cheap(&p) {
        return Ok(());
    }
    let info = ops::checkpoint(&mut p, &name, None, true)?;
    if let Some(label) = info.label {
        ops::progress(format!(
            "checkpoint {name}@{label} in {} ms",
            info.elapsed_ms
        ));
    }
    Ok(())
}

/// SessionStart: tell Claude where it is, as context.
pub fn session_start(input: &Value) -> Result<Option<String>> {
    let Some(cwd) = agent_cwd(input) else {
        return Ok(None);
    };
    mark_interactive_at(&cwd);
    let Some((mut p, name)) = session_at(&cwd, Some(TURN_HOOK_LOCK_WAIT))? else {
        return Ok(None);
    };
    mark_interactive(&mut p, &name)?;
    let s = p.session(&name)?;
    // Only promise automatic checkpoints where the Stop hook takes them.
    let checkpoints = if ops::checkpoints_are_cheap(&p) {
        "A checkpoint is taken automatically at the end of each turn that changed something; use the shoal rewind tool to undo"
    } else {
        "Checkpoints are NOT taken automatically here (this filesystem cannot copy cheaply); call the shoal checkpoint tool yourself before risky steps, and rewind to undo"
    };
    Ok(Some(format!(
        "You are working in shoal session \"{name}\" (branch {}, port {}), an isolated copy-on-write copy of {} with dependencies installed. Your changes stay here until you land them with the shoal land tool. {checkpoints}; fork to try alternatives, and preview to show the user the running app. Run tests and builds from this directory as `{} run -- <command>`: it waits for a free slot in the job queue this machine's sessions share, so parallel sessions do not overload it.",
        s.branch,
        s.port,
        p.state.source.display(),
        sys::shoal_command()
    )))
}

pub fn prompt(input: &Value) -> Result<()> {
    if let Some(cwd) = agent_cwd(input) {
        mark_interactive_at(&cwd);
        if let Some((mut p, name)) = session_at(&cwd, Some(TURN_HOOK_LOCK_WAIT))? {
            mark_interactive(&mut p, &name)?;
            p.mark_active(&name)?;
        }
    }
    Ok(())
}

/// Run a hook; returns the process exit code.
pub fn run(event: &str) -> i32 {
    let input = match read_input() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("shoal hook {event}: {err:#}");
            return if matches!(event, "worktree-create" | "worktree-remove") {
                1
            } else {
                0
            };
        }
    };
    match event {
        "worktree-create" => match worktree_create(&input) {
            Ok(path) => {
                println!("{}", path.display());
                0
            }
            Err(err) => {
                eprintln!("shoal hook worktree-create: {err:#}");
                1
            }
        },
        "worktree-remove" => match worktree_remove(&input) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("shoal hook worktree-remove: {err:#}");
                1
            }
        },
        "stop" => {
            if let Err(err) = stop(&input) {
                eprintln!("shoal hook stop: {err:#}");
            }
            0
        }
        "session-start" => {
            match session_start(&input) {
                Ok(Some(text)) => println!("{text}"),
                Ok(None) => {}
                Err(err) => eprintln!("shoal hook session-start: {err:#}"),
            }
            0
        }
        "prompt" => {
            if let Err(err) = prompt(&input) {
                eprintln!("shoal hook prompt: {err:#}");
            }
            0
        }
        other => {
            eprintln!(
                "shoal hook: unknown event {other:?}; events: {}",
                EVENTS.join(", ")
            );
            // 1, not 2: exit 2 would block a prompt or a stop in Claude Code.
            1
        }
    }
}
