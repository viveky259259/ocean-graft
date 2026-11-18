//! Edge-case tests for the smaller tools: `status`, `conflicts`, `preview`
//! and `discard`, plus the argument handling every tool shares (session vs
//! path vs project resolution, unknown keys, wrong types, non-object
//! arguments).
//!
//! Port range 5500-5599. Tests that listen on a port (preview) use the bases
//! 5500, 5520, 5540 and 5580 with one listening session each; a test that
//! loops over backends reuses its slot, one backend after the other. Tests that never listen use distinct odd bases (5501, 5503, ...)
//! so their session ports can never collide with a listening test.
mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

fn setup(port: u16, backend: &str) -> Env {
    let env = Env::new(port);
    env.init(backend);
    env
}

fn ok(r: Result<Value, String>, what: &str) -> Value {
    r.unwrap_or_else(|e| panic!("{what} failed: {e}"))
}

fn err(r: Result<Value, String>, what: &str) -> String {
    match r {
        Ok(v) => panic!("{what} should have failed, got {v}"),
        Err(e) => e,
    }
}

/// Fork one session from main; returns (path, port, branch).
fn fork(env: &Env, name: &str) -> (PathBuf, u16, String) {
    let v = ok(env.api("fork", json!({ "name": name })), "fork");
    let s = &v["sessions"][0];
    assert_eq!(s["name"], name, "{v}");
    (
        PathBuf::from(s["path"].as_str().unwrap()),
        s["port"].as_u64().unwrap() as u16,
        s["branch"].as_str().unwrap().to_string(),
    )
}

fn commit_file(env: &Env, dir: &Path, file: &str, content: &str, msg: &str) -> String {
    let p = dir.join(file);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
    env.git(dir, &["add", "-A", "--", file]);
    env.git(dir, &["commit", "-q", "-m", msg]);
    env.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn branch_exists(env: &Env, branch: &str) -> bool {
    env.command("git", &env.repo())
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .unwrap()
        .status
        .success()
}

fn show(env: &Env, rev_path: &str) -> String {
    env.git(&env.repo(), &["show", rev_path])
}

/// Run `shoal api` with raw (possibly malformed) argument text.
fn api_raw(env: &Env, dir: &Path, tool: &str, raw: &str) -> std::process::Output {
    env.command(BIN, dir)
        .args(["api", tool, raw])
        .output()
        .unwrap()
}

/// `shoal api` with git's upward search stopped at the test root, so a
/// directory outside the test repository really is outside any repository
/// (the test tree itself lives inside a git checkout).
fn api_c(env: &Env, dir: &Path, tool: &str, args: Value) -> Result<Value, String> {
    run_api(
        env.command(BIN, dir)
            .env("GIT_CEILING_DIRECTORIES", env.root.path()),
        tool,
        args,
    )
}

/// Like `api_c`, with CLAUDE_PROJECT_DIR set.
fn api_with_project_dir(
    env: &Env,
    dir: &Path,
    project_dir: &Path,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    run_api(
        env.command(BIN, dir)
            .env("GIT_CEILING_DIRECTORIES", env.root.path())
            .env("CLAUDE_PROJECT_DIR", project_dir),
        tool,
        args,
    )
}

fn run_api(cmd: &mut std::process::Command, tool: &str, args: Value) -> Result<Value, String> {
    let out = cmd.args(["api", tool, &args.to_string()]).output().unwrap();
    let v: Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("non-JSON ({e}): {}\n{}", stdout(&out), stderr(&out)));
    if out.status.success() {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or_default().to_string())
    }
}

fn projects_count(env: &Env) -> usize {
    match fs::read_dir(env.home().join("projects")) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().join("state.json").exists())
            .count(),
        Err(_) => 0,
    }
}

fn session_names(env: &Env) -> Vec<String> {
    env.state()["sessions"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// A directory outside any repository and outside SHOAL_HOME.
fn outside_dir(env: &Env) -> PathBuf {
    let d = env.root.path().join("outside");
    fs::create_dir_all(&d).unwrap();
    d
}

fn poll<F: FnMut() -> bool>(within: Duration, mut f: F) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// HTTP status code of GET / on 127.0.0.1:port, or None if nothing answers.
fn http_status(port: u16) -> Option<u16> {
    let mut s =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500))
            .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn answers(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

const SERVER: &str = "exec python3 -m http.server \"$PORT\" --bind 127.0.0.1";

fn web_service(env: &Env, session: &str) -> Option<Value> {
    env.state()["sessions"][session]["services"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["name"] == "web").cloned())
}

/// Every tool and the argument keys its schema accepts.
const TOOLS: &[(&str, &[&str])] = &[
    ("status", &["path", "project", "session"]),
    (
        "fork",
        &["at", "count", "from", "name", "path", "project", "session"],
    ),
    ("checkpoint", &["label", "path", "project", "session"]),
    ("rewind", &["label", "path", "project", "session"]),
    ("conflicts", &["path", "project", "session"]),
    (
        "land",
        &[
            "commit_message",
            "into",
            "path",
            "project",
            "session",
            "test",
            "timeout_secs",
        ],
    ),
    (
        "preview",
        &["command", "path", "project", "session", "stop"],
    ),
    ("discard", &["keep_work", "path", "project", "session"]),
];

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_without_project_reports_uninitialized_and_creates_nothing() {
    let env = Env::new(5501);
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(v["initialized"], false, "{v}");
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("no shoal project yet"), "{msg}");
    assert!(v["next"].as_str().unwrap().contains("fork"), "{v}");
    assert_eq!(projects_count(&env), 0, "status must not create a project");

    // Same through MCP, from a subdirectory of the repository.
    let mut mcp = env.mcp_in(&env.repo().join("src"));
    mcp.initialize();
    let (is_err, v) = mcp.call(1, "status", json!({}));
    assert!(!is_err, "{v}");
    assert_eq!(v["initialized"], false);
    drop(mcp);
    assert_eq!(projects_count(&env), 0);
}

#[test]
fn status_reports_every_field_for_every_session() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5503 + i as u16 * 2, backend);
        let (a, a_port, a_branch) = fork(&env, "a");
        let (b, _b_port, b_branch) = fork(&env, "b");
        commit_file(&env, &a, "feature.txt", "f\n", "feature");
        fs::write(a.join("src/app.txt"), "changed\n").unwrap();
        fs::write(a.join("new.txt"), "n\n").unwrap();
        ok(
            env.api("checkpoint", json!({"session": "a", "label": "cp1"})),
            "checkpoint",
        );

        let v = ok(env.api("status", json!({})), "status");
        assert_eq!(v["initialized"], true, "{v}");
        assert_eq!(v["backend"], backend);
        assert_eq!(v["branch"], "main");
        assert_eq!(
            PathBuf::from(v["source"].as_str().unwrap()),
            env.repo().canonicalize().unwrap()
        );
        assert_eq!(
            PathBuf::from(v["project"].as_str().unwrap()),
            env.project_dir()
        );
        assert_eq!(v["template"]["behind_source"], false, "{v}");
        assert!(v["template"]["generation"].is_u64(), "{v}");
        assert_eq!(v["template"]["commit"].as_str().unwrap().len(), 8);
        assert!(
            v["commands"]["setup"]
                .as_str()
                .unwrap()
                .contains("deps.lock"),
            "{v}"
        );
        assert!(v["commands"].get("test").is_some() && v["commands"].get("preview").is_some());
        assert!(v["current_session"].is_null(), "{v}");
        assert!(
            v["overlaps"].is_object(),
            "two sessions -> overlaps reported: {v}"
        );

        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        let sa = sessions.iter().find(|s| s["name"] == "a").unwrap();
        let sb = sessions.iter().find(|s| s["name"] == "b").unwrap();
        for key in [
            "name",
            "path",
            "branch",
            "port",
            "parent",
            "commits",
            "uncommitted_count",
            "uncommitted",
            "idle_secs",
            "checkpoints",
            "services",
        ] {
            assert!(sa.get(key).is_some(), "session field {key} missing: {sa}");
        }
        assert_eq!(PathBuf::from(sa["path"].as_str().unwrap()), a);
        assert_eq!(PathBuf::from(sb["path"].as_str().unwrap()), b);
        assert_eq!(sa["branch"], a_branch.as_str());
        assert_eq!(sb["branch"], b_branch.as_str());
        assert_eq!(sa["port"], a_port);
        assert_ne!(sa["port"], sb["port"]);
        assert_eq!(sa["commits"], 1);
        assert_eq!(sb["commits"], 0);
        assert_eq!(sa["uncommitted_count"], 2, "{sa}");
        let unc: Vec<&str> = sa["uncommitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(
            unc.contains(&"src/app.txt"),
            "leading status column must be stripped exactly: {unc:?}"
        );
        assert!(unc.contains(&"new.txt"), "{unc:?}");
        assert_eq!(sb["uncommitted_count"], 0);
        assert_eq!(sb["uncommitted"], json!([]));
        assert!(sa["idle_secs"].as_u64().unwrap() < 600, "{sa}");
        let cps = sa["checkpoints"].as_array().unwrap();
        assert!(
            cps.iter().any(|c| c["label"] == "cp1"
                && c["auto"] == false
                && c["head"].as_str().unwrap().len() == 8),
            "{cps:?}"
        );
        assert!(cps.iter().all(|c| c["age_secs"].is_u64()));
        assert_eq!(sb["checkpoints"], json!([]));
        assert_eq!(sa["services"], json!([]));

        // One session: overlaps is null.
        ok(
            env.api("discard", json!({"session": "b", "keep_work": false})),
            "discard b",
        );
        let v = ok(env.api("status", json!({})), "status");
        assert!(v["overlaps"].is_null(), "{v}");
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn status_uncommitted_list_is_capped_at_50_and_count_is_exact() {
    let env = setup(5507, "clone");
    let (a, _, _) = fork(&env, "a");
    for i in 0..50 {
        fs::write(a.join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let s = ok(env.api("status", json!({"session": "a"})), "status");
    let sa = &s["sessions"][0];
    assert_eq!(sa["uncommitted_count"], 50);
    assert_eq!(sa["uncommitted"].as_array().unwrap().len(), 50);

    fs::write(a.join("f050.txt"), "x").unwrap();
    let s = ok(env.api("status", json!({})), "status");
    let sa = &s["sessions"][0];
    assert_eq!(sa["uncommitted_count"], 51);
    assert_eq!(sa["uncommitted"].as_array().unwrap().len(), 50);

    for i in 51..130 {
        fs::write(a.join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let s = ok(env.api("status", json!({})), "status");
    let sa = &s["sessions"][0];
    assert_eq!(sa["uncommitted_count"], 130);
    let list = sa["uncommitted"].as_array().unwrap();
    assert_eq!(list.len(), 50);
    assert!(
        list.iter()
            .all(|f| f.as_str().unwrap().starts_with('f') && f.as_str().unwrap().ends_with(".txt")),
        "{list:?}"
    );
}

// Fixed bug: status reports uncommitted paths in `git status --porcelain` v1 form
// without -z, so a file name containing a space comes back C-quoted
// ("\"with space.txt\"", quotes included) and non-ASCII names come back as
// octal escapes. A path Claude copies from status then names no real file.
#[test]
fn status_uncommitted_paths_are_real_file_names() {
    let env = setup(5509, "clone");
    let (a, _, _) = fork(&env, "a");
    fs::write(a.join("with space.txt"), "x").unwrap();
    fs::write(a.join("caf\u{e9}.txt"), "x").unwrap();
    let s = ok(env.api("status", json!({})), "status");
    let list: Vec<String> = s["sessions"][0]["uncommitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    for f in &list {
        assert!(
            a.join(f).exists(),
            "status listed {f:?}, which is not a file in the session (all: {list:?})"
        );
    }
}

#[test]
fn status_path_inside_session_sets_current_session() {
    let env = setup(5511, "clone");
    let (a, _, _) = fork(&env, "a");
    fork(&env, "b");
    // A nested directory inside session a.
    let v = ok(
        env.api("status", json!({"path": a.join("src").to_string_lossy()})),
        "status",
    );
    assert_eq!(v["current_session"], "a", "{v}");
    assert_eq!(
        v["sessions"].as_array().unwrap().len(),
        2,
        "status still lists every session"
    );

    // From inside session a: the working directory resolves the project
    // and path names the session.
    let v = ok(
        env.api_in(&a, "status", json!({"path": a.to_string_lossy()})),
        "status",
    );
    assert_eq!(v["current_session"], "a");

    // A path outside any session (the source repository) resolves the
    // project but no session.
    let v = ok(
        env.api("status", json!({"path": env.repo().to_string_lossy()})),
        "status",
    );
    assert_eq!(v["initialized"], true);
    assert!(v["current_session"].is_null(), "{v}");

    // session names the current session.
    let v = ok(env.api("status", json!({"session": "b"})), "status");
    assert_eq!(v["current_session"], "b");
}

// Fixed bug: status accepts a session name that does not exist and echoes it back
// as current_session, instead of rejecting it with the list of sessions as
// every other tool does. Claude is told it is "in" a session that is not there.
#[test]
fn status_unknown_session_is_not_reported_as_current() {
    let env = setup(5513, "clone");
    fork(&env, "a");
    match env.api("status", json!({"session": "ghost"})) {
        Err(e) => assert!(e.contains("ghost") && e.contains('a'), "{e}"),
        Ok(v) => assert!(
            v["current_session"].is_null(),
            "an unknown session must not be reported as current: {v}"
        ),
    }
}

// Fixed bug: status with a path that does not exist (or any error from resolving
// path/project, including a wrongly typed argument) reports the whole project
// as "initialized": false and suggests fork, even though the project exists
// and has sessions. Claude would conclude its sessions are gone.
#[test]
fn status_bad_path_does_not_claim_the_project_is_uninitialized() {
    let env = setup(5515, "clone");
    fork(&env, "a");
    let missing = env.root.path().join("no/such/dir");
    match env.api("status", json!({"path": missing.to_string_lossy()})) {
        Err(e) => assert!(e.contains("does not exist"), "{e}"),
        Ok(v) => assert_ne!(
            v["initialized"], false,
            "status claims the project is not initialized for a bad path: {v}"
        ),
    }
}

// Fixed bug: wrongly typed path/project are swallowed by status: the type error
// becomes "initialized": false instead of an error.
#[test]
fn status_rejects_wrongly_typed_arguments() {
    let env = setup(5517, "clone");
    fork(&env, "a");
    let e = err(env.api("status", json!({"session": 5})), "status session=5");
    assert!(e.contains("session must be a string"), "{e}");
    let e = err(env.api("status", json!({"bogus": 1})), "status bogus");
    assert!(e.contains("bogus"), "{e}");
    for args in [
        json!({"path": 5}),
        json!({"project": ["x"]}),
        json!({"path": true}),
    ] {
        let r = env.api("status", args.clone());
        assert!(r.is_err(), "status {args} should be rejected, got {r:?}");
    }
}

#[test]
fn status_template_behind_source_tracks_commits_to_main() {
    let env = setup(5519, "clone");
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(v["template"]["behind_source"], false);
    let gen0 = v["template"]["generation"].as_u64().unwrap();

    // An uncommitted edit in the source does not count.
    fs::write(env.repo().join("README.md"), "dirty\n").unwrap();
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(v["template"]["behind_source"], false);
    env.git(&env.repo(), &["checkout", "--", "README.md"]);

    let new_head = commit_file(&env, &env.repo(), "main2.txt", "m\n", "on main");
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(v["template"]["behind_source"], true, "{v}");
    assert_eq!(v["template"]["generation"].as_u64().unwrap(), gen0);

    // A commit on another branch does not make the template behind.
    // Forking refreshes the template.
    fork(&env, "a");
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(v["template"]["behind_source"], false, "{v}");
    assert_eq!(v["template"]["commit"], &new_head[..8]);
    assert!(v["template"]["generation"].as_u64().unwrap() > gen0);

    env.git(&env.repo(), &["checkout", "-q", "-b", "side"]);
    commit_file(&env, &env.repo(), "side.txt", "s\n", "on side");
    env.git(&env.repo(), &["checkout", "-q", "main"]);
    let v = ok(env.api("status", json!({})), "status");
    assert_eq!(
        v["template"]["behind_source"], false,
        "a commit on another branch: {v}"
    );
}

#[test]
fn status_resolves_project_from_project_arg_env_and_session_path() {
    let env = setup(5521, "clone");
    let (a, _, _) = fork(&env, "a");
    let outside = outside_dir(&env);

    // From outside: no project.
    let v = ok(api_c(&env, &outside, "status", json!({})), "status outside");
    assert_eq!(v["initialized"], false);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("not inside a git repository"),
        "{v}"
    );

    // project argument.
    let v = ok(
        api_c(
            &env,
            &outside,
            "status",
            json!({"project": env.repo().to_string_lossy()}),
        ),
        "status project",
    );
    assert_eq!(v["initialized"], true);
    assert_eq!(v["sessions"][0]["name"], "a");

    // project pointing at a subdirectory of the repository.
    let v = ok(
        api_c(
            &env,
            &outside,
            "status",
            json!({"project": env.repo().join("src").to_string_lossy()}),
        ),
        "status project subdir",
    );
    assert_eq!(v["initialized"], true);

    // project pointing at a session (or the project directory itself).
    let v = ok(
        api_c(
            &env,
            &outside,
            "status",
            json!({"project": a.to_string_lossy()}),
        ),
        "status project=session",
    );
    assert_eq!(v["initialized"], true);
    let v = ok(
        api_c(
            &env,
            &outside,
            "status",
            json!({"project": env.project_dir().to_string_lossy()}),
        ),
        "status project=projectdir",
    );
    assert_eq!(v["initialized"], true);

    // CLAUDE_PROJECT_DIR.
    let v = ok(
        api_with_project_dir(&env, &outside, &env.repo(), "status", json!({})),
        "status env",
    );
    assert_eq!(v["initialized"], true);
    // An explicit project beats CLAUDE_PROJECT_DIR.
    let v = ok(
        api_with_project_dir(
            &env,
            &env.repo(),
            &outside,
            "status",
            json!({"project": env.repo().to_string_lossy()}),
        ),
        "status env+project",
    );
    assert_eq!(v["initialized"], true);
    // `shoal --project` on the CLI.
    let out = env
        .command(BIN, &outside)
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .args([
            "--project",
            &env.repo().to_string_lossy(),
            "api",
            "status",
            "{}",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["initialized"], true);

    // project pointing at a directory that is not a git repository.
    let v = ok(
        api_c(
            &env,
            &env.repo(),
            "status",
            json!({"project": outside.to_string_lossy()}),
        ),
        "status non-git",
    );
    assert_eq!(v["initialized"], false);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("not inside a git repository"),
        "{v}"
    );
    assert_eq!(projects_count(&env), 1);
}

// ---------------------------------------------------------------------------
// conflicts
// ---------------------------------------------------------------------------

#[test]
fn conflicts_with_zero_and_one_session_has_the_full_shape() {
    let env = setup(5523, "clone");
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(
        v,
        json!({"changed": [], "pairs": [], "pairs_without_shared_files": 0, "skipped": []})
    );
    let (a, _, _) = fork(&env, "a");
    fs::write(a.join("src/app.txt"), "x\n").unwrap();
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(v["pairs"], json!([]));
    assert_eq!(v["pairs_without_shared_files"], 0);
    assert_eq!(v["changed"], json!([{"session": "a", "files": 1}]), "{v}");
}

#[test]
fn conflicts_detects_a_real_conflict_counting_uncommitted_work() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5525 + i as u16 * 2, backend);
        let (a, _, _) = fork(&env, "a");
        let (b, _, _) = fork(&env, "b");
        // a commits a change to line two; b changes the same line, uncommitted.
        commit_file(
            &env,
            &a,
            "src/app.txt",
            "line one\nline TWO from a\nline three\n",
            "a",
        );
        fs::write(
            b.join("src/app.txt"),
            "line one\nline 2 from b\nline three\n",
        )
        .unwrap();
        let v = ok(env.api("conflicts", json!({})), "conflicts");
        let pairs = v["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 1, "{v}");
        let p = &pairs[0];
        let names = [p["a"].as_str().unwrap(), p["b"].as_str().unwrap()];
        assert!(names.contains(&"a") && names.contains(&"b"), "{p}");
        assert_eq!(p["conflict"], true, "{p}");
        assert_eq!(p["shared_files"], json!(["src/app.txt"]));
        assert_eq!(p["conflicting_files"], json!(["src/app.txt"]));
        assert_eq!(v["pairs_without_shared_files"], 0);

        // The same answer twice (cache), and after b reverts, no pair at all.
        let again = ok(env.api("conflicts", json!({})), "conflicts again");
        assert_eq!(again, v);
        env.git(&b, &["checkout", "--", "src/app.txt"]);
        let v = ok(env.api("conflicts", json!({})), "conflicts after revert");
        assert_eq!(
            v["pairs"],
            json!([]),
            "a stale cached conflict survived: {v}"
        );
        assert_eq!(v["pairs_without_shared_files"], 1);

        // status embeds the same report.
        fs::write(
            b.join("src/app.txt"),
            "line one\nline 2 from b\nline three\n",
        )
        .unwrap();
        let s = ok(env.api("status", json!({})), "status");
        assert_eq!(s["overlaps"]["pairs"][0]["conflict"], true, "{s}");
    }
}

#[test]
fn conflicts_clean_overlap_and_no_overlap() {
    let env = setup(5529, "clone");
    let (a, _, _) = fork(&env, "a");
    let (b, _, _) = fork(&env, "b");
    let (c, _, _) = fork(&env, "c");
    // a and b touch different lines of the same file: shared, no conflict.
    fs::write(a.join("src/app.txt"), "LINE ONE\nline two\nline three\n").unwrap();
    commit_file(
        &env,
        &b,
        "src/app.txt",
        "line one\nline two\nLINE THREE\n",
        "b",
    );
    // c touches unrelated files, including a new untracked one.
    fs::write(c.join("README.md"), "other\n").unwrap();
    fs::write(c.join("c-only.txt"), "c\n").unwrap();
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    let pairs = v["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 1, "{v}");
    assert_eq!(pairs[0]["shared_files"], json!(["src/app.txt"]));
    assert_eq!(pairs[0]["conflict"], false, "{v}");
    assert_eq!(pairs[0]["conflicting_files"], json!([]));
    assert_eq!(v["pairs_without_shared_files"], 2, "{v}");
    let changed = v["changed"].as_array().unwrap();
    let files = |n: &str| {
        changed
            .iter()
            .find(|x| x["session"] == n)
            .map(|x| x["files"].clone())
    };
    assert_eq!(files("a"), Some(json!(1)));
    assert_eq!(files("b"), Some(json!(1)));
    assert_eq!(files("c"), Some(json!(2)));
}

#[test]
fn conflicts_both_adding_the_same_new_file_differently_conflicts() {
    let env = setup(5531, "clone");
    let (a, _, _) = fork(&env, "a");
    let (b, _, _) = fork(&env, "b");
    fs::write(a.join("same.txt"), "from a\n").unwrap();
    fs::write(b.join("same.txt"), "from b\n").unwrap();
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(v["pairs"][0]["shared_files"], json!(["same.txt"]), "{v}");
    assert_eq!(v["pairs"][0]["conflict"], true, "{v}");

    // Identical additions merge cleanly.
    fs::write(b.join("same.txt"), "from a\n").unwrap();
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(v["pairs"][0]["conflict"], false, "{v}");
}

#[test]
fn conflicts_errors_without_a_project_and_creates_nothing() {
    let env = Env::new(5533);
    let e = err(env.api("conflicts", json!({})), "conflicts no project");
    assert!(e.contains("no shoal project yet"), "{e}");
    assert_eq!(projects_count(&env), 0);
    let outside = outside_dir(&env);
    let e = err(
        api_c(
            &env,
            &env.repo(),
            "conflicts",
            json!({"project": outside.to_string_lossy()}),
        ),
        "conflicts non-git",
    );
    assert!(e.contains("not inside a git repository"), "{e}");
    let e = err(
        env.api(
            "conflicts",
            json!({"project": env.root.path().join("nope").to_string_lossy()}),
        ),
        "conflicts missing",
    );
    assert!(e.contains("does not exist"), "{e}");
    assert_eq!(projects_count(&env), 0);
}

// ---------------------------------------------------------------------------
// discard
// ---------------------------------------------------------------------------

#[test]
fn discard_default_saves_wip_and_imports_branch_then_removes() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5535 + i as u16 * 2, backend);
        let (a, _, _) = fork(&env, "a");
        fork(&env, "keep");
        commit_file(&env, &a, "committed.txt", "c\n", "a commit");
        fs::write(a.join("src/app.txt"), "uncommitted edit\n").unwrap();
        fs::write(a.join("untracked.txt"), "u\n").unwrap();
        ok(
            env.api("checkpoint", json!({"session": "a", "label": "cp"})),
            "checkpoint",
        );
        // The user's own uncommitted edit in the source repository.
        fs::write(env.repo().join("README.md"), "user edit\n").unwrap();
        let main_before = env.git(&env.repo(), &["rev-parse", "main"]);

        let v = ok(env.api("discard", json!({"session": "a"})), "discard");
        assert_eq!(v["session"], "a");
        assert_eq!(v["saved"]["committed_wip"], true, "{v}");
        assert_eq!(v["saved"]["import"]["branch"], "shoal/a", "{v}");
        assert_eq!(v["saved"]["import"]["commits"], 2, "{v}");

        assert!(branch_exists(&env, "shoal/a"));
        assert_eq!(show(&env, "shoal/a:committed.txt"), "c\n");
        assert_eq!(show(&env, "shoal/a:src/app.txt"), "uncommitted edit\n");
        assert_eq!(show(&env, "shoal/a:untracked.txt"), "u\n");
        assert!(show(&env, "shoal/a").contains("WIP"), "WIP commit message");

        // Source repository untouched otherwise.
        assert_eq!(env.git(&env.repo(), &["rev-parse", "main"]), main_before);
        assert_eq!(read(env.repo().join("README.md")), "user edit\n");
        assert_eq!(
            env.git(&env.repo(), &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "main"
        );

        // The session, its directory and its checkpoints are gone.
        assert_eq!(session_names(&env), vec!["keep".to_string()]);
        assert!(!a.exists(), "session directory still exists");
        let state = env.state();
        assert!(
            state["checkpoints"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["session"] != "a"),
            "{state}"
        );
        assert!(!env.project_dir().join("checkpoints/a").exists());
        let s = ok(env.api("status", json!({})), "status");
        assert_eq!(s["sessions"].as_array().unwrap().len(), 1);

        // A second discard of the same name fails cleanly.
        let e = err(env.api("discard", json!({"session": "a"})), "discard again");
        assert!(e.contains("no session named \"a\""), "{e}");
    }
}

#[test]
fn discard_keep_work_false_removes_without_saving() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5539 + i as u16 * 2, backend);
        let (a, _, _) = fork(&env, "a");
        commit_file(&env, &a, "committed.txt", "c\n", "a commit");
        fs::write(a.join("untracked.txt"), "u\n").unwrap();
        ok(env.api("checkpoint", json!({"session": "a"})), "checkpoint");
        let v = ok(
            env.api("discard", json!({"session": "a", "keep_work": false})),
            "discard",
        );
        assert_eq!(v["session"], "a");
        assert!(v["saved"].is_null(), "{v}");
        assert!(!branch_exists(&env, "shoal/a"));
        assert!(!a.exists());
        assert!(session_names(&env).is_empty());
        assert!(env.state()["checkpoints"].as_array().unwrap().is_empty());
        assert_eq!(env.git(&env.repo(), &["status", "--porcelain"]), "");
        // The name is free again.
        let (a2, _, _) = fork(&env, "a");
        assert!(
            !a2.join("committed.txt").exists(),
            "a new session must not inherit the old one"
        );
        assert!(!a2.join("untracked.txt").exists());
    }
}

#[test]
fn discard_clean_session_saves_nothing_and_creates_no_branch() {
    let env = setup(5543, "clone");
    fork(&env, "a");
    let v = ok(
        env.api("discard", json!({"session": "a", "keep_work": true})),
        "discard",
    );
    assert_eq!(v["saved"]["committed_wip"], false, "{v}");
    assert!(v["saved"]["import"]["branch"].is_null(), "{v}");
    assert_eq!(v["saved"]["import"]["commits"], 0);
    assert!(!branch_exists(&env, "shoal/a"));
    assert!(session_names(&env).is_empty());
}

// Fixed bug: discard's import force-updates refs/heads/shoal/<name> ("+HEAD:...").
// Session names are only unique among live sessions, so after one session
// named "a" is discarded (its work saved as shoal/a), a later session that
// reuses the name "a" overwrites shoal/a on its own discard, and the first
// session's saved work is no longer on any branch. "Nothing lost" is broken.
#[test]
fn discard_twice_with_the_same_name_keeps_the_first_saved_work() {
    let env = setup(5545, "clone");
    let (a, _, _) = fork(&env, "a");
    let first = commit_file(&env, &a, "first.txt", "1\n", "first work");
    ok(env.api("discard", json!({"session": "a"})), "discard 1");
    assert_eq!(
        env.git(&env.repo(), &["rev-parse", "shoal/a"]).trim(),
        first
    );

    let (a2, _, _) = fork(&env, "a");
    commit_file(&env, &a2, "second.txt", "2\n", "second work");
    let v = ok(env.api("discard", json!({"session": "a"})), "discard 2");

    let reachable = env.git(
        &env.repo(),
        &["branch", "--contains", &first, "--format=%(refname:short)"],
    );
    assert!(
        !reachable.trim().is_empty(),
        "the first session's saved commit {first} is on no branch any more (second discard: {v})"
    );
}

#[test]
fn discard_unknown_session_lists_existing_sessions() {
    let env = setup(5547, "clone");
    let e = err(
        env.api("discard", json!({"session": "ghost"})),
        "discard none",
    );
    assert!(
        e.contains("ghost") && e.contains("none (create one with fork)"),
        "{e}"
    );
    fork(&env, "alpha");
    fork(&env, "beta");
    let e = err(
        env.api("discard", json!({"session": "ghost"})),
        "discard ghost",
    );
    assert!(e.contains("\"ghost\""), "{e}");
    assert!(e.contains("alpha") && e.contains("beta"), "{e}");
    assert_eq!(session_names(&env).len(), 2, "nothing removed");
}

#[test]
fn discard_resolves_the_session_from_a_path() {
    let env = setup(5549, "clone");
    let (a, _, _) = fork(&env, "a");
    fork(&env, "b");
    fs::write(a.join("work.txt"), "w\n").unwrap();
    // From the session's own working directory, with path, no project.
    let v = ok(
        env.api_in(
            &a.join("src"),
            "discard",
            json!({"path": a.join("src").to_string_lossy()}),
        ),
        "discard path",
    );
    assert_eq!(v["session"], "a");
    assert_eq!(show(&env, "shoal/a:work.txt"), "w\n");
    assert_eq!(session_names(&env), vec!["b".to_string()]);

    // Path outside any session.
    let e = err(
        env.api("discard", json!({"path": env.repo().to_string_lossy()})),
        "discard repo path",
    );
    assert!(e.contains("not inside a shoal session"), "{e}");
    // Path inside the template is not a session either.
    let template = fs::read_dir(env.project_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("template")
                || p.ends_with("templates")
        });
    if let Some(t) = template {
        let e = err(
            env.api("discard", json!({"path": t.to_string_lossy()})),
            "discard template path",
        );
        assert!(e.contains("not inside a shoal session"), "{e}");
    }
    // No session and no path.
    let e = err(env.api("discard", json!({})), "discard nothing");
    assert!(e.contains("pass session"), "{e}");
    // Nonexistent path.
    let e = err(
        env.api("discard", json!({"path": a.to_string_lossy()})),
        "discard removed path",
    );
    assert!(e.contains("does not exist"), "{e}");
    assert_eq!(session_names(&env), vec!["b".to_string()]);
}

#[test]
fn discard_rejects_wrong_types_and_leaves_the_session() {
    let env = setup(5551, "clone");
    let (a, _, _) = fork(&env, "a");
    fs::write(a.join("w.txt"), "w").unwrap();
    for (args, needle) in [
        (
            json!({"session": "a", "keep_work": "false"}),
            "keep_work must be true or false",
        ),
        (
            json!({"session": "a", "keep_work": 0}),
            "keep_work must be true or false",
        ),
        (json!({"session": ["a"]}), "session must be a string"),
        (
            json!({"session": "a", "force": true}),
            "has no argument \"force\"",
        ),
    ] {
        let e = err(env.api("discard", args.clone()), &args.to_string());
        assert!(e.contains(needle), "{args}: {e}");
    }
    assert_eq!(session_names(&env), vec!["a".to_string()]);
    assert!(a.join("w.txt").exists());
    // keep_work: null means the default (keep).
    let v = ok(
        env.api("discard", json!({"session": "a", "keep_work": null})),
        "discard null",
    );
    assert_eq!(v["saved"]["committed_wip"], true, "{v}");
}

#[test]
fn concurrent_discards_of_different_and_same_sessions_keep_state_consistent() {
    let env = setup(5553, "clone");
    let names = ["c1", "c2", "c3", "c4"];
    let mut paths = Vec::new();
    for n in names {
        let (p, _, _) = fork(&env, n);
        fs::write(p.join(format!("{n}.txt")), n).unwrap();
        paths.push(p);
    }
    std::thread::scope(|s| {
        let hs: Vec<_> = names
            .iter()
            .map(|n| {
                let env = &env;
                s.spawn(move || env.api("discard", json!({"session": n})))
            })
            .collect();
        for h in hs {
            ok(h.join().unwrap(), "concurrent discard");
        }
    });
    assert!(session_names(&env).is_empty(), "{}", env.state());
    for (n, p) in names.iter().zip(&paths) {
        assert!(!p.exists());
        assert_eq!(show(&env, &format!("shoal/{n}:{n}.txt")), *n);
    }

    // The same session discarded twice at once: exactly one succeeds.
    let (p, _, _) = fork(&env, "dup");
    fs::write(p.join("dup.txt"), "d").unwrap();
    let results: Vec<Result<Value, String>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..3)
            .map(|_| {
                let env = &env;
                s.spawn(move || env.api("discard", json!({"session": "dup"})))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    for e in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(e.contains("no session named"), "{e}");
    }
    assert_eq!(show(&env, "shoal/dup:dup.txt"), "d");
    assert!(session_names(&env).is_empty());
}

// ---------------------------------------------------------------------------
// preview (listening tests: 5500, 5520, 5540, 5560, 5580)
// ---------------------------------------------------------------------------

#[test]
fn preview_serves_http_is_idempotent_and_stops() {
    for (i, backend) in backends().into_iter().enumerate() {
        // Backends run one after the other, so they can share one port slot.
        let _ = i;
        let env = setup(5500, backend);
        let (_a, port, _) = fork(&env, "a");
        let v = ok(
            env.api("preview", json!({"session": "a", "command": SERVER})),
            "preview",
        );
        let url = v["url"].as_str().unwrap().to_string();
        assert_eq!(url, format!("http://localhost:{port}"), "{v}");
        assert_eq!(v["session"], "a");
        assert_eq!(v["command"], SERVER);
        assert!(v["log"].as_str().unwrap().ends_with("a-web.log"), "{v}");
        assert!(
            poll(Duration::from_secs(20), || http_status(port) == Some(200)),
            "preview URL never answered 200; log: {}",
            fs::read_to_string(v["log"].as_str().unwrap()).unwrap_or_default()
        );

        // Again: already running, same URL, nothing new started.
        let pid_before = web_service(&env, "a").unwrap()["pid"].clone();
        let again = ok(
            env.api("preview", json!({"session": "a", "command": SERVER})),
            "preview again",
        );
        assert_eq!(again["already_running"], true, "{again}");
        assert_eq!(again["url"], url.as_str());
        // Also without a command.
        let again = ok(
            env.api("preview", json!({"session": "a"})),
            "preview again, no command",
        );
        assert_eq!(again["already_running"], true, "{again}");
        assert_eq!(web_service(&env, "a").unwrap()["pid"], pid_before);

        // status shows the service.
        let s = ok(env.api("status", json!({"session": "a"})), "status");
        let svc = &s["sessions"][0]["services"][0];
        assert_eq!(svc["name"], "web");
        assert_eq!(svc["url"], url.as_str());
        assert!(
            svc["state"].as_str().unwrap().starts_with("running"),
            "{svc}"
        );

        // The dev server dies on its own: status must not call it running,
        // and preview restarts it instead of answering already_running.
        let dead = pid_before.as_i64().unwrap() as i32;
        unsafe_kill_group(dead);
        assert!(poll(Duration::from_secs(5), || !alive(dead)));
        let s = ok(env.api("status", json!({})), "status after crash");
        let st = s["sessions"][0]["services"][0]["state"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !st.starts_with("running"),
            "a dead server is reported as {st:?}"
        );
        let v = ok(
            env.api("preview", json!({"session": "a", "command": SERVER})),
            "preview after crash",
        );
        assert!(v.get("already_running").is_none(), "{v}");
        assert_eq!(v["url"], url.as_str());
        assert!(poll(Duration::from_secs(20), || http_status(port) == Some(200)));
        let pid_before = web_service(&env, "a").unwrap()["pid"].clone();
        assert_ne!(pid_before.as_i64().unwrap() as i32, dead);

        // Stop.
        let stopped = ok(
            env.api("preview", json!({"session": "a", "stop": true})),
            "stop",
        );
        assert_eq!(stopped["stopped"], 1, "{stopped}");
        assert!(
            poll(Duration::from_secs(5), || !answers(port)),
            "port still answers after stop"
        );
        let pid = pid_before.as_i64().unwrap() as i32;
        assert!(!alive(pid), "dev server process group still alive");
        let svc = web_service(&env, "a").unwrap();
        assert!(svc["pid"].is_null() && svc["gateway"].is_null(), "{svc}");
        // Stopping again is a no-op.
        let stopped = ok(
            env.api("preview", json!({"session": "a", "stop": true})),
            "stop again",
        );
        assert_eq!(stopped["stopped"], 0);

        // Start again: a fresh start, not already_running, and it answers.
        let v = ok(
            env.api("preview", json!({"session": "a", "command": SERVER})),
            "restart",
        );
        assert!(v.get("already_running").is_none(), "{v}");
        assert_eq!(v["url"], url.as_str());
        assert!(poll(Duration::from_secs(20), || http_status(port) == Some(200)));
        ok(
            env.api("preview", json!({"session": "a", "stop": true})),
            "final stop",
        );
        assert!(poll(Duration::from_secs(5), || !answers(port)));
    }
}

#[test]
fn discard_stops_a_running_preview() {
    for (i, backend) in backends().into_iter().enumerate() {
        let _ = i;
        let env = setup(5520, backend);
        let (a, port, _) = fork(&env, "a");
        let v = ok(
            env.api(
                "preview",
                json!({"path": a.to_string_lossy(), "command": SERVER}),
            ),
            "preview by path",
        );
        assert_eq!(v["session"], "a");
        assert!(poll(Duration::from_secs(20), || http_status(port) == Some(200)));
        let svc = web_service(&env, "a").unwrap();
        let pid = svc["pid"].as_i64().unwrap() as i32;
        let gw = svc["gateway"].as_i64().unwrap() as i32;

        let d = ok(
            env.api("discard", json!({"session": "a", "keep_work": false})),
            "discard",
        );
        assert_eq!(d["session"], "a");
        assert!(
            poll(Duration::from_secs(5), || !answers(port)),
            "port still answers after discard"
        );
        assert!(!alive(pid), "dev server survived discard");
        assert!(
            poll(Duration::from_secs(5), || !alive(gw)),
            "gateway survived discard"
        );
        assert!(!a.exists());
    }
}

#[test]
fn preview_uses_the_preview_command_from_shoal_toml() {
    let env = Env::new(5540);
    fs::write(
        env.repo().join("shoal.toml"),
        format!("preview = '{}'\n", SERVER.replace('\'', "")),
    )
    .unwrap();
    env.git(&env.repo(), &["add", "shoal.toml"]);
    env.git(&env.repo(), &["commit", "-q", "-m", "shoal.toml"]);
    env.init("clone");
    let (_a, port, _) = fork(&env, "a");
    let s = ok(env.api("status", json!({})), "status");
    assert!(
        s["commands"]["preview"]
            .as_str()
            .unwrap()
            .contains("http.server"),
        "{s}"
    );
    let v = ok(env.api("preview", json!({"session": "a"})), "preview");
    assert!(
        v["command"].as_str().unwrap().contains("http.server"),
        "{v}"
    );
    assert!(poll(Duration::from_secs(20), || http_status(port) == Some(200)));
    // An explicit command of only whitespace falls back to shoal.toml too.
    ok(
        env.api("preview", json!({"session": "a", "stop": true})),
        "stop",
    );
    assert!(poll(Duration::from_secs(5), || !answers(port)));
    let v = ok(
        env.api("preview", json!({"session": "a", "command": "   "})),
        "preview blank",
    );
    assert!(
        v["command"].as_str().unwrap().contains("http.server"),
        "{v}"
    );
    assert!(poll(Duration::from_secs(20), || http_status(port) == Some(200)));
    ok(
        env.api("preview", json!({"session": "a", "stop": true})),
        "stop",
    );
    assert!(poll(Duration::from_secs(5), || !answers(port)));
}

// Fixed bug: when another process already listens on the session's port, preview
// reports success with that URL. The gateway is started detached and has to
// open the project (whose lock the preview call still holds) before it binds,
// so start_gateway's 200 ms "exited straight away" check always passes; the
// gateway then fails to bind and exits after preview has returned. The URL
// handed to the user is served by the unrelated process, the dev server is
// left running on its private port with no gateway, and a later preview
// answers already_running for a URL shoal does not serve.
#[test]
fn preview_refuses_when_the_session_port_is_already_taken() {
    let env = setup(5580, "clone");
    let (_a, port, _) = fork(&env, "a");
    assert_eq!(port, 5580);
    let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let r = env.api("preview", json!({"session": "a", "command": SERVER}));
    // Whatever preview said, look at what is left behind.
    std::thread::sleep(Duration::from_millis(1500));
    let svc = web_service(&env, "a");
    let gateway_alive = svc
        .as_ref()
        .and_then(|s| s["gateway"].as_i64())
        .is_some_and(|g| alive(g as i32));
    let server_pid = svc.as_ref().and_then(|s| s["pid"].as_i64());
    // Clean up before asserting so nothing outlives the test.
    let _ = env.api("preview", json!({"session": "a", "stop": true}));
    drop(squatter);
    match r {
        Err(e) => {
            assert!(
                e.contains(&port.to_string()) || e.contains("gateway"),
                "{e}"
            );
            if let Some(pid) = server_pid {
                assert!(
                    !alive(pid as i32),
                    "the dev server was left running after a failed preview"
                );
            }
        }
        Ok(v) => panic!(
            "preview succeeded although port {port} belongs to another process: {v}; \
             gateway alive afterwards: {gateway_alive}"
        ),
    }
}

#[test]
fn preview_without_a_command_and_nothing_detectable_is_a_clear_error() {
    let env = setup(5555, "clone");
    fork(&env, "a");
    for args in [
        json!({"session": "a"}),
        json!({"session": "a", "command": ""}),
        json!({"session": "a", "command": " \t "}),
    ] {
        let e = err(env.api("preview", args.clone()), "preview no command");
        assert!(
            e.contains("no preview command") && e.contains("$PORT"),
            "{args}: {e}"
        );
    }
    assert!(
        web_service(&env, "a").is_none(),
        "a failed preview must not record a service"
    );
}

#[test]
fn preview_command_that_exits_immediately_is_a_clear_error() {
    let env = setup(5557, "clone");
    let (_a, port, _) = fork(&env, "a");
    for cmd in ["exit 3", "definitely-not-a-command-xyz"] {
        let e = err(
            env.api("preview", json!({"session": "a", "command": cmd})),
            cmd,
        );
        assert!(e.contains("exited straight away"), "{cmd}: {e}");
        assert!(
            e.contains("a-web.log"),
            "the error should point at the log: {e}"
        );
    }
    assert!(web_service(&env, "a").is_none(), "{}", env.state());
    assert!(
        !answers(port),
        "no gateway may be left listening after a failed start"
    );
    let s = ok(env.api("status", json!({})), "status");
    assert_eq!(s["sessions"][0]["services"], json!([]));
}

#[test]
fn preview_stop_with_nothing_running_and_argument_errors() {
    let env = setup(5559, "clone");
    let (a, _, _) = fork(&env, "a");
    let v = ok(
        env.api("preview", json!({"session": "a", "stop": true})),
        "stop nothing",
    );
    assert_eq!(v["stopped"], 0);
    assert_eq!(v["session"], "a");
    let v = ok(
        env.api(
            "preview",
            json!({"path": a.to_string_lossy(), "stop": true}),
        ),
        "stop by path",
    );
    assert_eq!(v["session"], "a");
    for (args, needle) in [
        (
            json!({"session": "a", "stop": "yes"}),
            "stop must be true or false",
        ),
        (
            json!({"session": "a", "command": 5}),
            "command must be a string",
        ),
        (
            json!({"session": "ghost", "command": "true"}),
            "no session named \"ghost\"",
        ),
        (json!({"command": "true"}), "pass session"),
        (
            json!({"session": "a", "port": 1}),
            "has no argument \"port\"",
        ),
    ] {
        let e = err(env.api("preview", args.clone()), &args.to_string());
        assert!(e.contains(needle), "{args}: {e}");
    }
    assert!(web_service(&env, "a").is_none());
}

// ---------------------------------------------------------------------------
// Arguments shared by every tool
// ---------------------------------------------------------------------------

#[test]
fn every_tool_rejects_unknown_keys_and_lists_the_valid_ones() {
    let env = Env::new(5561);
    for (tool, keys) in TOOLS {
        let e = err(env.api(tool, json!({"bogus_key": 1})), tool);
        assert!(
            e.contains(&format!("{tool} has no argument \"bogus_key\"")),
            "{tool}: {e}"
        );
        let listed = e
            .split("it takes: ")
            .nth(1)
            .unwrap_or_else(|| panic!("{tool}: {e}"));
        let listed: Vec<&str> = listed.split(", ").map(str::trim).collect();
        assert_eq!(listed, keys.to_vec(), "{tool}: {e}");
    }
    assert_eq!(
        projects_count(&env),
        0,
        "a rejected call must not create a project"
    );
}

#[test]
fn tool_list_matches_the_schemas_and_unknown_tool_lists_tools() {
    let env = Env::new(5563);
    let mut mcp = env.mcp();
    mcp.initialize();
    let list = mcp.request(1, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), TOOLS.len());
    for (tool, keys) in TOOLS {
        let def = tools
            .iter()
            .find(|t| t["name"] == *tool)
            .unwrap_or_else(|| panic!("{tool} missing"));
        let mut props: Vec<String> = def["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        props.sort();
        assert_eq!(
            props,
            keys.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "{tool}"
        );
        assert_eq!(def["inputSchema"]["additionalProperties"], false);
    }
    drop(mcp);
    let e = err(env.api("nonsense", json!({})), "unknown tool");
    assert!(e.contains("unknown tool \"nonsense\""), "{e}");
    for (tool, _) in TOOLS {
        assert!(e.contains(tool), "{e}");
    }
}

#[test]
fn non_object_and_malformed_arguments_are_rejected() {
    let env = setup(5565, "clone");
    fork(&env, "a");
    for raw in ["[]", "\"session\"", "5", "true", "[{\"session\":\"a\"}]"] {
        for tool in ["status", "conflicts", "discard", "preview"] {
            let out = api_raw(&env, &env.repo(), tool, raw);
            assert!(
                !out.status.success(),
                "{tool} {raw} succeeded: {}",
                stdout(&out)
            );
            let v: Value = serde_json::from_str(&stdout(&out)).unwrap_or_else(|e| {
                panic!(
                    "{tool} {raw}: non-JSON ({e}) {}{}",
                    stdout(&out),
                    stderr(&out)
                )
            });
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("must be a JSON object"),
                "{tool} {raw}: {v}"
            );
        }
    }
    // null is the same as {}.
    let out = api_raw(&env, &env.repo(), "status", "null");
    assert!(out.status.success(), "{}", stdout(&out));
    // Malformed JSON: non-zero exit, no panic, session untouched.
    for raw in ["{", "{session: a}", ""] {
        let out = api_raw(&env, &env.repo(), "discard", raw);
        assert!(!out.status.success(), "discard {raw:?} succeeded");
        assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
    }
    assert_eq!(session_names(&env), vec!["a".to_string()]);
}

#[test]
fn wrongly_typed_arguments_are_rejected_by_every_session_tool() {
    let env = setup(5567, "clone");
    fork(&env, "a");
    let cases: &[(&str, Value, &str)] = &[
        (
            "conflicts",
            json!({"project": 5}),
            "project must be a string",
        ),
        ("conflicts", json!({"path": {}}), "path must be a string"),
        ("discard", json!({"path": 1.5}), "path must be a string"),
        (
            "discard",
            json!({"project": true, "session": "a"}),
            "project must be a string",
        ),
        ("preview", json!({"session": 7}), "session must be a string"),
        (
            "checkpoint",
            json!({"session": "a", "label": 3}),
            "label must be a string",
        ),
        ("rewind", json!({"session": "a"}), "rewind needs label"),
        (
            "rewind",
            json!({"session": "a", "label": null}),
            "rewind needs label",
        ),
        (
            "fork",
            json!({"count": "2"}),
            "count must be a positive whole number",
        ),
        (
            "fork",
            json!({"count": -1}),
            "count must be a positive whole number",
        ),
        (
            "land",
            json!({"session": "a", "timeout_secs": "10"}),
            "timeout_secs must be a positive whole number",
        ),
    ];
    for (tool, args, needle) in cases {
        let e = err(env.api(tool, args.clone()), &format!("{tool} {args}"));
        assert!(e.contains(needle), "{tool} {args}: {e}");
    }
    assert_eq!(session_names(&env), vec!["a".to_string()]);
}

#[test]
fn session_tools_with_a_bad_project_fail_with_the_reason() {
    let env = setup(5569, "clone");
    fork(&env, "a");
    let outside = outside_dir(&env);
    let missing = env.root.path().join("missing");
    // A second, never-initialised repository.
    let other = env.root.path().join("other");
    fs::create_dir_all(&other).unwrap();
    env.git(&other, &["init", "-q", "-b", "main"]);
    for tool in ["conflicts", "discard", "preview", "checkpoint"] {
        let e = err(
            api_c(
                &env,
                &env.repo(),
                tool,
                json!({"session": "a", "project": outside.to_string_lossy()}),
            ),
            tool,
        );
        assert!(e.contains("not inside a git repository"), "{tool}: {e}");
        let e = err(
            env.api(
                tool,
                json!({"session": "a", "project": missing.to_string_lossy()}),
            ),
            tool,
        );
        assert!(e.contains("does not exist"), "{tool}: {e}");
        let e = err(
            env.api(
                tool,
                json!({"session": "a", "project": other.to_string_lossy()}),
            ),
            tool,
        );
        assert!(e.contains("no shoal project yet"), "{tool}: {e}");
    }
    // Run from outside with no project at all.
    let e = err(
        api_c(&env, &outside, "discard", json!({"session": "a"})),
        "discard outside",
    );
    assert!(e.contains("not inside a git repository"), "{e}");
    assert_eq!(
        projects_count(&env),
        1,
        "bad project arguments must not create projects"
    );
    assert_eq!(session_names(&env), vec!["a".to_string()]);
}

#[test]
fn path_takes_precedence_for_the_project_and_works_from_anywhere() {
    let env = setup(5571, "clone");
    let (a, _, _) = fork(&env, "a");
    let outside = outside_dir(&env);
    fs::write(a.join("x.txt"), "x").unwrap();
    // From outside the repository, path alone is enough for every session tool.
    let v = ok(
        api_c(
            &env,
            &outside,
            "checkpoint",
            json!({"path": a.to_string_lossy(), "label": "p"}),
        ),
        "checkpoint path",
    );
    assert_eq!(v["label"], "p");
    let v = ok(
        api_c(
            &env,
            &outside,
            "conflicts",
            json!({"path": a.to_string_lossy()}),
        ),
        "conflicts path",
    );
    assert!(v["pairs"].is_array());
    let v = ok(
        api_c(
            &env,
            &outside,
            "status",
            json!({"path": a.join("src/app.txt").to_string_lossy()}),
        ),
        "status file path",
    );
    assert_eq!(
        v["current_session"], "a",
        "a file inside the session counts: {v}"
    );
    // A bogus project alongside a valid path: path wins for the project.
    let v = ok(
        api_c(
            &env,
            &outside,
            "preview",
            json!({"path": a.to_string_lossy(), "project": outside.to_string_lossy(), "stop": true}),
        ),
        "preview path+bad project",
    );
    assert_eq!(v["session"], "a");
    // session wins over path for the session name when both are given.
    let (b, _, _) = fork(&env, "b");
    let v = ok(
        env.api(
            "checkpoint",
            json!({"session": "b", "path": a.to_string_lossy(), "label": "which"}),
        ),
        "checkpoint both",
    );
    let st = env.state();
    let owner = st["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "which")
        .unwrap()["session"]
        .clone();
    assert_eq!(owner, "b", "{v}");
    let _ = b;
}

#[test]
fn mcp_reports_tool_errors_as_is_error_with_guidance() {
    let env = setup(5573, "clone");
    fork(&env, "a");
    let mut mcp = env.mcp();
    mcp.initialize();
    let (is_err, v) = mcp.call(1, "discard", json!({"session": "ghost"}));
    assert!(is_err, "{v}");
    assert!(v.as_str().unwrap().contains("a"), "{v}");
    let (is_err, v) = mcp.call(2, "status", json!({"nope": 1}));
    assert!(is_err);
    assert!(
        v.as_str()
            .unwrap()
            .contains("it takes: path, project, session"),
        "{v}"
    );
    let (is_err, v) = mcp.call(3, "preview", json!({"session": "a"}));
    assert!(is_err);
    assert!(v.as_str().unwrap().contains("no preview command"), "{v}");
    let (is_err, v) = mcp.call(4, "conflicts", json!({}));
    assert!(!is_err, "{v}");
    let (is_err, v) = mcp.call(5, "discard", json!({"session": "a"}));
    assert!(!is_err, "{v}");
    assert_eq!(v["session"], "a");
    let (is_err, v) = mcp.call(6, "status", json!({}));
    assert!(!is_err);
    assert_eq!(v["sessions"], json!([]));
    assert!(
        mcp.next_message(Duration::from_millis(200)).is_none(),
        "no stray output"
    );
    assert!(mcp.close().success());
}

// ---------------------------------------------------------------------------
// Damaged or unusual state
// ---------------------------------------------------------------------------

// Fixed bug: one session whose directory has gone missing (deleted by hand, a
// failed disk) makes status fail as a whole ("session directory ... is
// missing"), so Claude loses its orientation for every other session too.
// status should report the healthy sessions and flag the broken one.
#[test]
fn status_survives_a_session_whose_directory_is_missing() {
    let env = setup(5575, "clone");
    let (a, _, _) = fork(&env, "a");
    fork(&env, "b");
    fs::remove_dir_all(&a).unwrap();
    let v = ok(env.api("status", json!({})), "status with a broken session");
    let names: Vec<&str> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"b"), "{v}");
}

#[test]
fn discard_a_session_whose_directory_is_missing() {
    let env = setup(5577, "clone");
    let (a, _, _) = fork(&env, "a");
    fork(&env, "b");
    fs::remove_dir_all(&a).unwrap();
    // status points at the fix for the broken session and still reports b.
    let st = ok(env.api("status", json!({})), "status with a broken session");
    let broken = st["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "a")
        .unwrap()
        .clone();
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("keep_work: false"),
        "{broken}"
    );
    // Nothing is left to save: discard (even keeping work) clears the record
    // and says so.
    let v = ok(env.api("discard", json!({"session": "a"})), "discard keep");
    assert_eq!(v["already_gone"], true, "{v}");
    assert!(v["saved"].is_null(), "{v}");
    assert_eq!(session_names(&env), vec!["b".to_string()]);
    let v = ok(env.api("status", json!({})), "status after cleanup");
    assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn discard_a_parent_leaves_its_forked_child_intact() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5579 + i as u16 * 2, backend);
        let (a, _, _) = fork(&env, "a");
        commit_file(&env, &a, "parent-commit.txt", "pc\n", "parent");
        fs::write(a.join("parent-wip.txt"), "pw\n").unwrap();
        ok(
            env.api("checkpoint", json!({"session": "a", "label": "cp"})),
            "checkpoint",
        );
        let v = ok(
            env.api("fork", json!({"from": "a", "name": "child"})),
            "fork child",
        );
        let child = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
        let v = ok(
            env.api("fork", json!({"from": "a", "at": "cp", "name": "child2"})),
            "fork at cp",
        );
        let child2 = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());

        ok(
            env.api("discard", json!({"session": "a", "keep_work": false})),
            "discard parent",
        );
        for c in [&child, &child2] {
            assert_eq!(read(c.join("parent-commit.txt")), "pc\n", "{backend}");
            assert_eq!(read(c.join("parent-wip.txt")), "pw\n", "{backend}");
            assert_eq!(read(c.join("deps/installed.txt")), "lib 1.0\n");
            fs::write(c.join("child-edit.txt"), "ce\n").unwrap();
        }
        let s = ok(env.api("status", json!({})), "status");
        assert_eq!(s["sessions"].as_array().unwrap().len(), 2, "{s}");
        // The child's own discard still saves everything, parent's work included.
        let d = ok(
            env.api("discard", json!({"session": "child"})),
            "discard child",
        );
        assert_eq!(d["saved"]["committed_wip"], true, "{d}");
        assert_eq!(show(&env, "shoal/child:parent-commit.txt"), "pc\n");
        assert_eq!(show(&env, "shoal/child:child-edit.txt"), "ce\n");
    }
}

#[test]
fn discard_fails_safely_when_its_branch_is_checked_out_in_the_source() {
    let env = setup(5583, "clone");
    let (a, _, _) = fork(&env, "a");
    commit_file(&env, &a, "one.txt", "1\n", "one");
    ok(env.api("discard", json!({"session": "a"})), "first discard");
    // The user checks out the saved branch to look at it, and edits a file.
    env.git(&env.repo(), &["checkout", "-q", "shoal/a"]);
    fs::write(env.repo().join("one.txt"), "user edit\n").unwrap();

    let (a2, _, _) = fork(&env, "a");
    commit_file(&env, &a2, "two.txt", "2\n", "two");
    let r = env.api("discard", json!({"session": "a"}));
    // Whatever happens, the user's checkout and edit survive...
    assert_eq!(read(env.repo().join("one.txt")), "user edit\n");
    assert_eq!(
        env.git(&env.repo(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim(),
        "shoal/a"
    );
    match r {
        // ...and if the save failed, the session (and its commit) is still there.
        Err(e) => {
            assert!(session_names(&env).contains(&"a".to_string()), "{e}");
            assert!(a2.join("two.txt").exists());
        }
        Ok(v) => {
            let branch = v["saved"]["import"]["branch"].as_str().unwrap().to_string();
            assert_eq!(show(&env, &format!("{branch}:two.txt")), "2\n", "{v}");
        }
    }
}

#[test]
fn conflicts_modify_versus_delete_is_a_conflict() {
    let env = setup(5585, "clone");
    let (a, _, _) = fork(&env, "a");
    let (b, _, _) = fork(&env, "b");
    fs::remove_file(a.join("README.md")).unwrap();
    commit_file(&env, &b, "README.md", "changed\n", "b");
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(v["pairs"][0]["shared_files"], json!(["README.md"]), "{v}");
    assert_eq!(v["pairs"][0]["conflict"], true, "{v}");
    assert_eq!(
        v["pairs"][0]["conflicting_files"],
        json!(["README.md"]),
        "{v}"
    );
    // Both delete it: shared, but no conflict.
    env.git(&b, &["rm", "-q", "README.md"]);
    let v = ok(env.api("conflicts", json!({})), "conflicts");
    assert_eq!(v["pairs"][0]["conflict"], false, "{v}");
}

fn unsafe_kill_group(pgid: i32) {
    let out = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .output()
        .unwrap();
    assert!(out.status.success(), "kill: {}", stderr(&out));
}

// ---------------------------------------------------------------------------
// The queued run command Claude is told to use
// ---------------------------------------------------------------------------

/// `status` (per session) and the SessionStart context give Claude a
/// command line for `shoal run`. Claude's Bash tool has no `shoal` on PATH
/// when the plugin downloaded the binary, so the line names the binary by
/// its full path, quoted: here a copy in a directory whose name has a space
/// and a quote. Pasted into a shell, both lines run the command in the
/// session through the queue and keep its exit code.
#[test]
fn the_run_command_in_status_and_session_start_works_from_a_shell() {
    let env = setup(5587, "clone");
    let odd = env.root.path().join("it's a dir");
    fs::create_dir_all(&odd).unwrap();
    let bin = odd.join("shoal");
    fs::copy(BIN, &bin).unwrap();
    let api = |tool: &str, args: Value| -> Value {
        let out = env
            .command(bin.to_str().unwrap(), &env.repo())
            .args(["api", tool, &args.to_string()])
            .output()
            .unwrap();
        assert!(out.status.success(), "{tool}: {}", stderr(&out));
        serde_json::from_str(&stdout(&out)).unwrap()
    };
    api("fork", json!({"name": "q"}));
    let status = api("status", json!({}));
    let run = status["sessions"][0]["run"].as_str().unwrap().to_string();
    assert!(run.contains("<command>"), "{run}");
    let session_path = PathBuf::from(status["sessions"][0]["path"].as_str().unwrap());

    // From anywhere (here the user's checkout), the status line targets
    // the session and keeps the exit code.
    let line = run.replace("<command>", "sh -c 'pwd > from-status; exit 7'");
    let out = env
        .command("sh", &env.repo())
        .args(["-c", &line])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7), "{line}: {}", stderr(&out));
    assert_eq!(
        fs::canonicalize(read(session_path.join("from-status")).trim()).unwrap(),
        fs::canonicalize(&session_path).unwrap()
    );
    assert!(!env.repo().join("from-status").exists());

    // The SessionStart context's command runs from the session directory.
    let mut cmd = env.command(bin.to_str().unwrap(), &session_path);
    cmd.args(["hook", "session-start"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"cwd": session_path}).to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let context = stdout(&out);
    let start = context.find("as `").unwrap() + 4;
    let end = start + context[start..].find('`').unwrap();
    let line = context[start..end].replace("<command>", "touch from-context");
    let out = env
        .command("sh", &session_path)
        .args(["-c", &line])
        .output()
        .unwrap();
    assert!(out.status.success(), "{line}: {}", stderr(&out));
    assert!(session_path.join("from-context").exists(), "{line}");
}
