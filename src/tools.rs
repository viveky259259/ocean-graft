//! The tools Claude uses: one dispatcher, JSON in and JSON out. The MCP
//! server and `shoal api` both call `call`; neither implements a tool.

use crate::config;
use crate::conflicts;
use crate::git;
use crate::ops::{self, LandOptions, Origin};
use crate::project::Project;
use crate::services;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_FORKS: u64 = 8;

fn session_props() -> Value {
    json!({
        "session": {"type": "string", "description": "Session name (see status)."},
        "path": {"type": "string", "description": "Any path inside the session, such as your working directory. Use instead of session."},
        "project": {"type": "string", "description": "Path to the repository, when it is not the directory Claude Code was started in."}
    })
}

fn schema(extra: Value, required: &[&str]) -> Value {
    let mut props = session_props();
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

/// Tool definitions in MCP `tools/list` form.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "status",
            "description": "Orientation in one call: the project, every shoal session (path, branch, commits, uncommitted files, checkpoints, preview URLs, idle time) and which sessions change the same files. Call this first, and whenever you are unsure which session you are in.",
            "inputSchema": schema(json!({}), &[]),
        }),
        json!({
            "name": "fork",
            "description": "Create isolated copy-on-write sessions in milliseconds, each a full checkout with dependencies installed, on its own branch and port. from=\"main\" (default) starts from the repository's latest commit; from=<session> copies a session as it is now, uncommitted and ignored files included; add at=<checkpoint> to start from an earlier state. count>1 makes several identical sessions, to try different approaches in parallel and keep the best. Work in the returned path.",
            "inputSchema": schema(json!({
                "from": {"type": "string", "description": "\"main\" (default) or a session name."},
                "at": {"type": "string", "description": "A checkpoint label of the from session."},
                "name": {"type": "string", "description": "Name for the new session; numbered when count > 1."},
                "count": {"type": "integer", "minimum": 1, "maximum": MAX_FORKS, "description": "How many sessions to create (default 1)."}
            }), &[]),
        }),
        json!({
            "name": "checkpoint",
            "description": "Save the whole state of a session (commits, uncommitted and untracked files, installed dependencies, build output) before a risky step such as a migration, a large refactor or a dependency upgrade. Cheap: changes only. Restore with rewind.",
            "inputSchema": schema(json!({
                "label": {"type": "string", "description": "Optional name, e.g. before-migration."}
            }), &[]),
        }),
        json!({
            "name": "rewind",
            "description": "Restore a session to a checkpoint. The state being replaced is always first saved as checkpoint pre-rewind-N (named in the result as saved_as; the newest 10 are kept), so a rewind can itself be undone. label=\"latest\" picks the newest checkpoint other than those pre-rewind ones. Checkpoints named turn-N are taken automatically at the end of each turn when status reports auto_checkpoints: true. Services in the session are restarted.",
            "inputSchema": schema(json!({
                "label": {"type": "string", "description": "Checkpoint label, or \"latest\"."}
            }), &["label"]),
        }),
        json!({
            "name": "conflicts",
            "description": "Which sessions change the same files, and which pairs would actually conflict if merged, counting uncommitted work. Use before landing work from several sessions, or while parallel work is in progress.",
            "inputSchema": schema(json!({}), &[]),
        }),
        json!({
            "name": "land",
            "description": "Deliver a session's work to the user's branch safely: rebase onto the latest target, run the tests through the job queue, then fast-forward the target (never a force). Returns status: landed, nothing_to_land, uncommitted_changes (pass commit_message), conflicts (files listed; the session is left unchanged), tests_failed (output included; rewind to the pre-land checkpoint to undo the rebase), timed_out (timeout_secs ran out: waiting for the project, a test slot or another land, or while tests ran; the tests did not fail, and if checkpoint is set the session was already rebased, which rewinding to that checkpoint undoes), target_blocked (the user's uncommitted edits, or untracked or ignored files listed in files, are in the way) or target_moving. The session cannot be checkpointed, rewound, forked or discarded while it is being landed.",
            "inputSchema": schema(json!({
                "commit_message": {"type": "string", "description": "Commit uncommitted work with this message first."},
                "test": {"type": "string", "description": "Test command; default from shoal.toml or detected. Empty string skips tests."},
                "into": {"type": "string", "description": "Target branch (default: the branch the project tracks)."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 1500, "description": "Time limit for the whole land, including waiting for a test slot and the tests (default 1200, at most 1500 so the call always answers before Claude Code's 30-minute tool limit)."}
            }), &[]),
        }),
        json!({
            "name": "preview",
            "description": "Start the session's app (dev server) and return a URL to give the user. The server sleeps when the session is idle and wakes on the next request. Pass stop=true to stop it.",
            "inputSchema": schema(json!({
                "command": {"type": "string", "description": "Command to run; default from shoal.toml or package.json (dev/start). It must listen on $PORT."},
                "stop": {"type": "boolean"}
            }), &[]),
        }),
        json!({
            "name": "discard",
            "description": "Remove a session you no longer need, such as the losers of a best-of-N. By default its work is kept first: uncommitted changes are committed as WIP and the branch is saved in the source repository as shoal/<name>. keep_work=false deletes it outright.",
            "inputSchema": schema(json!({
                "keep_work": {"type": "boolean", "description": "Default true."}
            }), &[]),
        }),
    ]
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

fn obj(args: &Value) -> Result<&Map<String, Value>> {
    match args {
        Value::Object(m) => Ok(m),
        Value::Null => {
            static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
            Ok(EMPTY.get_or_init(Map::new))
        }
        _ => bail!("arguments must be a JSON object"),
    }
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => bail!("{key} must be a string, got {other}"),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => bail!("{key} must be true or false, got {other}"),
    }
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .with_context(|| format!("{key} must be a positive whole number, got {v}")),
    }
}

/// The project the call is about: from `path`, `project`, CLAUDE_PROJECT_DIR
/// or the working directory, in that order.
fn project_dir(args: &Value, create: bool) -> Result<PathBuf> {
    if let Some(path) = opt_str(args, "path")? {
        return ops::project_for_path(Path::new(&path), create);
    }
    if let Some(project) = opt_str(args, "project")? {
        return ops::project_for_path(Path::new(&project), create);
    }
    let base = match std::env::var_os("CLAUDE_PROJECT_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::current_dir()?,
    };
    ops::project_for_path(&base, create)
}

fn session_name(p: &Project, args: &Value) -> Result<String> {
    if let Some(name) = opt_str(args, "session")? {
        if !p.state.sessions.contains_key(&name) {
            let known: Vec<&String> = p.state.sessions.keys().collect();
            bail!(
                "no session named {name:?}; sessions: {}",
                if known.is_empty() {
                    "none (create one with fork)".to_string()
                } else {
                    known
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        }
        return Ok(name);
    }
    if let Some(path) = opt_str(args, "path")? {
        return ops::session_for_path(p, Path::new(&path)).with_context(|| {
            format!("{path} is not inside a shoal session; pass session, or fork one")
        });
    }
    bail!("pass session (a name from status) or path (a directory inside the session)")
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn session_status(p: &Project, name: &str) -> Result<Value> {
    let s = p.session(name)?;
    let path = p.ensure_ready(name)?;
    let branch = git::git_trim(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let commits = git::count_commits(&path, &format!("{}..HEAD", s.base_commit))?;
    let dirty = git::status_paths(&path)?;
    let idle = sys::now().saturating_sub(services::last_activity(p, s));
    let checkpoints: Vec<Value> = p
        .state
        .checkpoints
        .iter()
        .filter(|c| c.session == name)
        .map(|c| {
            json!({"label": c.label, "auto": c.auto, "head": git::short(&c.head), "age_secs": sys::now().saturating_sub(c.created_at)})
        })
        .collect();
    let svcs: Vec<Value> = s
        .services
        .iter()
        .map(|svc| {
            json!({"name": svc.name, "state": services::describe(svc), "url": format!("http://localhost:{}", svc.port)})
        })
        .collect();
    Ok(json!({
        "name": name,
        "path": path,
        "branch": branch,
        "port": s.port,
        "parent": s.parent,
        "commits": commits,
        // Tests and builds go through the machine-wide job queue, so
        // parallel sessions do not overload the machine.
        "run": format!(
            "{} run --project {} {} -- <command>",
            sys::shoal_command(),
            sys::shell_quote(&p.dir.to_string_lossy()),
            sys::shell_quote(name)
        ),
        "uncommitted_count": dirty.len(),
        "uncommitted": dirty.iter().take(50).collect::<Vec<_>>(),
        "idle_secs": idle,
        "checkpoints": checkpoints,
        "services": svcs,
    }))
}

fn conflicts_json(report: &conflicts::Report) -> Value {
    json!({
        "changed": report.changed.iter().map(|(n, c)| json!({"session": n, "files": c})).collect::<Vec<_>>(),
        "pairs": report.pairs.iter().map(|pair| json!({
            "a": pair.a, "b": pair.b, "shared_files": pair.shared,
            "conflict": !pair.conflicts.is_empty(), "conflicting_files": pair.conflicts,
        })).collect::<Vec<_>>(),
        "pairs_without_shared_files": report.quiet_pairs,
        "skipped": report.skipped.iter().map(|(n, e)| json!({"session": n, "error": e})).collect::<Vec<_>>(),
    })
}

fn status(args: &Value) -> Result<Value> {
    // Validate every argument before looking for the project, so a bad
    // argument is an error and never reads as "no project".
    let path_arg = opt_str(args, "path")?;
    opt_str(args, "project")?;
    let session_arg = opt_str(args, "session")?;
    let dir = match project_dir(args, false) {
        Ok(dir) => dir,
        Err(err)
            if err.downcast_ref::<ops::NoProject>().is_some()
                || err.downcast_ref::<git::NotARepository>().is_some() =>
        {
            return Ok(json!({
                "initialized": false,
                "message": format!("{err:#}"),
                "next": "call fork to create the first session; the project is set up automatically",
            }))
        }
        Err(err) => return Err(err),
    };
    let p = Project::open(dir)?;
    let current = match (path_arg, session_arg) {
        (Some(path), _) => ops::session_for_path(&p, Path::new(&path)),
        (None, Some(_)) => Some(session_name(&p, args)?),
        (None, None) => None,
    };
    let source_head = git::git_trim(&p.state.source, &["rev-parse", &p.state.branch]).ok();
    let latest = p.latest();
    let cfg = config::load(&p.state.source).unwrap_or_default();
    // One broken session must not hide the others.
    let mut sessions = Vec::new();
    let mut broken = 0;
    for name in p.state.sessions.keys() {
        match session_status(&p, name) {
            Ok(v) => sessions.push(v),
            Err(err) => {
                broken += 1;
                sessions.push(json!({
                    "name": name,
                    "path": p.session_path(name),
                    "error": format!("{err:#}"),
                    "next": "discard this session with keep_work: false if its directory is gone",
                }));
            }
        }
    }
    let overlaps = if p.state.sessions.len() - broken >= 2 {
        match conflicts::check(&p) {
            Ok(report) => Some(conflicts_json(&report)),
            Err(err) => Some(json!({"error": format!("{err:#}")})),
        }
    } else {
        None
    };
    Ok(json!({
        "initialized": true,
        "project": p.dir,
        "source": p.state.source,
        "branch": p.state.branch,
        "backend": p.state.backend.name(),
        // Whether the Stop hook checkpoints every changing turn. When false,
        // checkpoint before risky steps yourself.
        "auto_checkpoints": ops::checkpoints_are_cheap(&p),
        "template": {
            "generation": latest.id,
            "commit": git::short(&latest.commit),
            "behind_source": source_head.as_deref().is_some_and(|h| h != latest.commit),
        },
        "commands": {"setup": p.state.setup, "test": cfg.test, "preview": cfg.preview},
        "current_session": current,
        "sessions": sessions,
        "overlaps": overlaps,
    }))
}

fn fork(args: &Value) -> Result<Value> {
    let from = opt_str(args, "from")?.unwrap_or_else(|| "main".into());
    let at = opt_str(args, "at")?;
    let count = opt_u64(args, "count")?.unwrap_or(1);
    if count == 0 || count > MAX_FORKS {
        bail!("count must be between 1 and {MAX_FORKS}");
    }
    let name_arg = opt_str(args, "name")?;
    let from_main = matches!(from.as_str(), "main" | "template");
    if from_main && at.is_some() {
        bail!("at needs from to name a session: checkpoints belong to sessions");
    }
    let dir = project_dir(args, from_main)?;
    // Refresh before taking the project lock: it runs setup without it. A
    // failed refresh (a broken commit, a failing install) must not stop work:
    // fork from the last good template and say why.
    let mut refreshed = None;
    let mut refresh_error = None;
    if from_main {
        match ops::refresh(&dir, false, ops::RefreshLimits::default()) {
            Ok(r) if !r.up_to_date => refreshed = Some(r),
            Ok(_) => {}
            Err(err) => {
                ops::progress(format!(
                    "could not refresh the template ({err:#}); using the last good one"
                ));
                refresh_error = Some(format!(
                    "{err:#}; the session starts from the last good template, which is behind the source branch"
                ));
            }
        }
    }
    let mut p = Project::open(dir)?;
    let origin = if from_main {
        Origin::Template
    } else {
        if !p.state.sessions.contains_key(&from) {
            bail!("no session named {from:?} to fork from");
        }
        match at {
            Some(label) => Origin::Checkpoint {
                session: from.clone(),
                label,
            },
            None => Origin::Session(from.clone()),
        }
    };
    let base = match name_arg {
        Some(n) => ops::sanitize_name(&n),
        None if from_main => "task".to_string(),
        None => ops::sanitize_name(&format!("{from}-fork")),
    };
    let mut created = Vec::new();
    for i in 1..=count {
        let wanted = if count > 1 {
            ops::with_suffix(&base, &format!("-{i}"))
        } else {
            base.clone()
        };
        let name = ops::unique_name(&p, &wanted);
        match ops::create_session(&mut p, &name, &origin) {
            Ok(info) => created.push(info),
            Err(err) if !created.is_empty() => {
                // Report what exists, so nothing is orphaned silently.
                return Ok(
                    json!({"sessions": created, "refreshed": refreshed, "refresh_error": refresh_error,
                    "error": format!("stopped after {} of {count}: {err:#}", created.len())}),
                );
            }
            Err(err) => return Err(err),
        }
    }
    Ok(json!({"sessions": created, "refreshed": refreshed, "refresh_error": refresh_error}))
}

fn checkpoint(args: &Value) -> Result<Value> {
    let mut p = Project::open(project_dir(args, false)?)?;
    let name = session_name(&p, args)?;
    let label = opt_str(args, "label")?;
    Ok(serde_json::to_value(ops::checkpoint(
        &mut p,
        &name,
        label.as_deref(),
        false,
    )?)?)
}

fn rewind(args: &Value) -> Result<Value> {
    let mut p = Project::open(project_dir(args, false)?)?;
    let name = session_name(&p, args)?;
    let label = opt_str(args, "label")?.context("label is required (or \"latest\")")?;
    Ok(serde_json::to_value(ops::rewind(&mut p, &name, &label)?)?)
}

fn conflicts_tool(args: &Value) -> Result<Value> {
    let p = Project::open(project_dir(args, false)?)?;
    Ok(conflicts_json(&conflicts::check(&p)?))
}

fn land(args: &Value) -> Result<Value> {
    let dir = project_dir(args, false)?;
    let name = {
        let p = Project::open(dir.clone())?;
        session_name(&p, args)?
    }; // the lock is released before tests run
    let report = ops::land(
        &dir,
        &name,
        &LandOptions {
            test: opt_str(args, "test")?,
            commit_message: opt_str(args, "commit_message")?,
            into: opt_str(args, "into")?,
            test_timeout: Duration::from_secs(
                opt_u64(args, "timeout_secs")?
                    .unwrap_or(1200)
                    .clamp(1, 1500),
            ),
        },
    )?;
    Ok(serde_json::to_value(report)?)
}

fn preview(args: &Value) -> Result<Value> {
    let mut p = Project::open(project_dir(args, false)?)?;
    let name = session_name(&p, args)?;
    if opt_bool(args, "stop")?.unwrap_or(false) {
        let (count, kb) = services::stop_session(&mut p, &name, Some("web"), false, false)?;
        return Ok(json!({"session": name, "stopped": count, "freed_kb": kb}));
    }
    if let Some(svc) = p.session(&name)?.services.iter().find(|s| s.name == "web") {
        let st = services::state(svc);
        if st == services::State::Running
            || (st == services::State::StoppedIdle && services::gateway_running(svc))
        {
            return Ok(
                json!({"session": name, "url": format!("http://localhost:{}", svc.port),
                "state": services::describe(svc), "already_running": true}),
            );
        }
    }
    let command = match opt_str(args, "command")? {
        Some(c) if !c.trim().is_empty() => c,
        _ => config::load(&p.state.source)?.preview.context(
            "no preview command: pass command (it must listen on $PORT), or set preview in shoal.toml",
        )?,
    };
    let svc = services::serve(
        &mut p,
        &name,
        "web",
        true,
        vec!["sh".into(), "-c".into(), command.clone()],
    )?;
    Ok(json!({
        "session": name,
        "url": format!("http://localhost:{}", svc.port),
        "command": command,
        "log": p.log_path(&name, "web"),
        "note": "the app may take a few seconds to start; the first request waits for it",
    }))
}

fn discard(args: &Value) -> Result<Value> {
    let mut p = Project::open(project_dir(args, false)?)?;
    let name = session_name(&p, args)?;
    let keep = opt_bool(args, "keep_work")?.unwrap_or(true);
    Ok(serde_json::to_value(ops::remove(
        &mut p, &name, !keep, keep,
    )?)?)
}

pub fn call(name: &str, args: &Value) -> Result<Value> {
    let given = obj(args)?;
    if let Some(def) = definitions().into_iter().find(|d| d["name"] == name) {
        let allowed = def["inputSchema"]["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(unknown) = given.keys().find(|k| !allowed.contains_key(*k)) {
            let mut names: Vec<&String> = allowed.keys().collect();
            names.sort();
            bail!(
                "{name} has no argument {unknown:?}; it takes: {}",
                names
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for req in def["inputSchema"]["required"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let key = req.as_str().unwrap_or_default();
            if given.get(key).is_none_or(Value::is_null) {
                bail!("{name} needs {key}");
            }
        }
    }
    match name {
        "status" => status(args),
        "fork" => fork(args),
        "checkpoint" => checkpoint(args),
        "rewind" => rewind(args),
        "conflicts" => conflicts_tool(args),
        "land" => land(args),
        "preview" => preview(args),
        "discard" => discard(args),
        other => bail!(
            "unknown tool {other:?}; tools: {}",
            definitions()
                .iter()
                .filter_map(|d| d["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
