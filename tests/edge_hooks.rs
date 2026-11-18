//! Edge-case tests for `shoal hook <event>`: WorktreeCreate, WorktreeRemove,
//! Stop/SubagentStop, SessionStart and UserPromptSubmit, against the contract
//! in plugin/ARCHITECTURE.md.
//!
//! Port range 5200-5299. No test here starts anything that listens on a port
//! (ports are only recorded in state.json), so each test simply takes its own
//! port_base inside the range.
//!
//! Every hook is run with GIT_CEILING_DIRECTORIES at the test's temporary
//! root: target/tmp lives inside the ocean git repository, and a "non-git
//! directory" would otherwise resolve to that enclosing repository.

mod common;
use common::*;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Run `shoal hook <event>` with raw bytes on stdin. Write errors are ignored
/// (the hook may stop reading early), and the elapsed time is returned.
fn run_hook_raw(
    env: &Env,
    event: &str,
    input: &[u8],
    dir: &Path,
    project_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
) -> (HookResult, Duration) {
    let mut cmd = env.command(BIN, dir);
    cmd.args(["hook", event])
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = project_dir {
        cmd.env("CLAUDE_PROJECT_DIR", p);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    let _ = writer.join();
    (
        HookResult {
            code: out.status.code().unwrap_or(-1),
            stdout: stdout(&out),
            stderr: stderr(&out),
        },
        elapsed,
    )
}

fn hook(
    env: &Env,
    event: &str,
    input: &Value,
    dir: &Path,
    project_dir: Option<&Path>,
) -> HookResult {
    run_hook_raw(
        env,
        event,
        input.to_string().as_bytes(),
        dir,
        project_dir,
        &[],
    )
    .0
}

fn timed_hook(env: &Env, event: &str, input: &Value, dir: &Path) -> (HookResult, Duration) {
    run_hook_raw(env, event, input.to_string().as_bytes(), dir, None, &[])
}

fn sessions_dir(env: &Env) -> PathBuf {
    env.project_dir().join("sessions")
}

fn session_path(env: &Env, name: &str) -> PathBuf {
    sessions_dir(env).join(name)
}

/// Assert that a WorktreeCreate result created session `name`, that stdout is
/// exactly its path (nothing before, nothing after) and return the path.
fn expect_created(env: &Env, r: &HookResult, name: &str) -> PathBuf {
    assert_eq!(
        r.code, 0,
        "worktree-create for {name} failed\nstdout:\n{}\nstderr:\n{}",
        r.stdout, r.stderr
    );
    let path = session_path(env, name).canonicalize().unwrap_or_else(|e| {
        panic!(
            "session {name} has no directory ({e}); stdout {:?}",
            r.stdout
        )
    });
    assert_eq!(
        r.last_line(),
        path.display().to_string(),
        "last stdout line must be the session path; stderr:\n{}",
        r.stderr
    );
    assert_eq!(
        r.stdout,
        format!("{}\n", path.display()),
        "stdout must carry only the path"
    );
    assert!(path.is_dir());
    assert_eq!(
        env.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim(),
        format!("shoal/{name}")
    );
    assert!(
        env.state()["sessions"][name].is_object(),
        "{name} missing from state.json"
    );
    path
}

fn create_input(name: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "transcript_path": "/dev/null",
        "hook_event_name": "WorktreeCreate",
        "cwd": cwd,
        "name": name,
    })
}

fn remove_input(path: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "hook_event_name": "WorktreeRemove",
        "cwd": "/",
        "worktree_path": path,
    })
}

fn turn_input(event: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "transcript_path": "/dev/null",
        "hook_event_name": event,
        "cwd": cwd,
    })
}

/// (label, auto) of every checkpoint of `session`, in state order.
fn checkpoints(env: &Env, session: &str) -> Vec<(String, bool)> {
    env.state()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["session"] == session)
        .map(|c| {
            (
                c["label"].as_str().unwrap().to_string(),
                c["auto"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn auto_labels(env: &Env, session: &str) -> Vec<String> {
    checkpoints(env, session)
        .into_iter()
        .filter(|(_, auto)| *auto)
        .map(|(l, _)| l)
        .collect()
}

fn fork(env: &Env, name: &str) -> PathBuf {
    let r = env.api("fork", json!({"name": name})).unwrap();
    PathBuf::from(r["sessions"][0]["path"].as_str().unwrap())
}

/// Write shoal.toml at the repository root and commit it.
fn write_config(env: &Env, toml: &str) {
    fs::write(env.repo().join("shoal.toml"), toml).unwrap();
    env.git(&env.repo(), &["add", "shoal.toml"]);
    env.git(&env.repo(), &["commit", "-q", "-m", "shoal config"]);
}

fn config_for(env: &Env, backend: &str, setup: &str) -> String {
    format!(
        "setup = {setup:?}\nsetup_inputs = [\"deps.lock\"]\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
        env.port_base
    )
}

fn has_project_state(env: &Env) -> bool {
    let Ok(entries) = fs::read_dir(env.home().join("projects")) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join("state.json").exists())
}

fn plain_dir(env: &Env, name: &str) -> PathBuf {
    let d = env.root.path().join(name);
    fs::create_dir_all(&d).unwrap();
    d
}

fn source_branches(env: &Env) -> Vec<String> {
    env.git(
        &env.repo(),
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/shoal/",
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

fn set_last_active(env: &Env, session: &str, value: u64) {
    let path = env.project_dir().join("state.json");
    let mut state: Value = serde_json::from_str(&read(&path)).unwrap();
    state["sessions"][session]["last_active"] = json!(value);
    fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Whether the Stop hook takes checkpoints here (see cheap_backend).
fn overlay_available() -> bool {
    cheap_backend().is_some()
}

/// A helper process holding the project's flock until dropped.
struct LockHolder(Child);

fn hold_lock(lock: &Path) -> LockHolder {
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(
            "import fcntl, sys, time\n\
             f = open(sys.argv[1], 'a')\n\
             fcntl.flock(f, fcntl.LOCK_EX)\n\
             print('locked', flush=True)\n\
             time.sleep(60)\n",
        )
        .arg(lock)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("python3 is needed to hold a lock");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "locked", "lock helper did not take the lock");
    LockHolder(child)
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// WorktreeCreate
// ---------------------------------------------------------------------------

/// First use in a repository with no project: the project is initialised from
/// shoal.toml, the setup's stdout (including a line with no trailing newline)
/// goes to stderr, and stdout is exactly the session path.
#[test]
fn create_auto_inits_from_shoal_toml_and_keeps_stdout_clean() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5200 + i as u16 * 2);
        write_config(
            &env,
            &config_for(
                &env,
                backend,
                "echo SETUP-STDOUT-NOISE; printf 'no-newline-noise'; mkdir -p deps && cat deps.lock > deps/installed.txt",
            ),
        );
        assert!(!has_project_state(&env));
        let r = hook(
            &env,
            "worktree-create",
            &create_input("first", &env.repo()),
            &env.repo(),
            None,
        );
        let path = expect_created(&env, &r, "first");
        assert!(
            !r.stdout.contains("NOISE"),
            "[{backend}] setup stdout leaked: {:?}",
            r.stdout
        );
        assert!(
            r.stderr.contains("SETUP-STDOUT-NOISE"),
            "[{backend}] setup output should go to stderr"
        );
        assert_eq!(read(path.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(
            read(path.join(".env")),
            "SECRET=1\n",
            "[{backend}] copy entry missing"
        );
        let state = env.state();
        assert_eq!(state["backend"], backend);
        assert_eq!(state["port_base"], env.port_base);
        assert_eq!(state["generations"].as_array().unwrap().len(), 1);
        assert!(
            env.git(&path, &["status", "--porcelain"]).trim().is_empty(),
            "[{backend}] fresh session should be clean"
        );
    }
}

/// Names Claude Code might pass are turned into valid session names; missing,
/// empty and non-string names get a default.
#[test]
fn create_sanitises_names() {
    let env = Env::new(5204);
    env.init("clone");
    let repo = env.repo();
    let long = "a".repeat(100);
    let cases: Vec<(Value, String)> = vec![
        (json!("feature/auth login"), "feature-auth-login".into()),
        (json!("#1234"), "1234".into()),
        (json!("über café"), "ber-caf".into()),
        (json!("日本語"), "session".into()),
        (json!("-"), "session-2".into()),
        (json!(""), "worktree".into()),
        (Value::Null, "worktree-2".into()),
        (json!(1234), "worktree-3".into()),
        (json!("../../etc/passwd"), "etc-passwd".into()),
        (json!("  spaced   name  "), "spaced-name".into()),
        (json!("tab\tnew\nline"), "tab-new-line".into()),
        (json!("UPPER_case.v2"), "upper_case.v2".into()),
        (json!("name;rm -rf ~"), "name-rm-rf".into()),
        (json!("$(touch pwned)"), "touch-pwned".into()),
        (json!(long.clone()), "a".repeat(40)),
        (json!(long), format!("{}-2", "a".repeat(38))),
    ];
    for (raw, expected) in &cases {
        let mut input = json!({"cwd": repo});
        if !raw.is_null() {
            input["name"] = raw.clone();
        }
        let r = hook(&env, "worktree-create", &input, &repo, None);
        let path = expect_created(&env, &r, expected);
        assert!(
            path.starts_with(sessions_dir(&env).canonicalize().unwrap()),
            "{raw} escaped the sessions directory: {}",
            path.display()
        );
    }
    assert_eq!(
        env.state()["sessions"].as_object().unwrap().len(),
        cases.len()
    );
    assert!(!repo.join("pwned").exists());
    assert!(!env.root.path().join("pwned").exists());
}

// Fixed bug: (medium) sanitize_name keeps "..", "..." and a ".lock" suffix, so the
// branch shoal/<name> is not a valid git ref and the hook fails.
/// Names that pass shoal's own validation but make an invalid git branch.
/// sanitize_name keeps "..", and names ending in ".lock", so the branch
/// shoal/<name> is not a valid ref and `git switch -C` fails: the hook exits 1
/// and `claude --worktree a..b` / `--worktree release.lock` cannot start.
#[test]
fn create_sanitises_names_git_rejects_as_branches() {
    let env = Env::new(5206);
    env.init("clone");
    let repo = env.repo();
    let mut failures = Vec::new();
    for raw in ["a..b", "release.lock", "v1...v2"] {
        let r = hook(
            &env,
            "worktree-create",
            &json!({"name": raw, "cwd": repo}),
            &repo,
            None,
        );
        if r.code != 0 {
            failures.push(format!("{raw:?}: exit {} {}", r.code, r.stderr.trim()));
            continue;
        }
        let path = PathBuf::from(r.last_line());
        assert!(
            path.is_dir(),
            "{raw:?}: printed path {} does not exist",
            path.display()
        );
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let branch = env.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(branch.trim(), format!("shoal/{name}"));
    }
    // Whatever failed must not leave a half-made session behind.
    for entry in fs::read_dir(sessions_dir(&env)).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            env.state()["sessions"][&name].is_object(),
            "half-made session directory left behind: {name}"
        );
    }
    assert!(
        failures.is_empty(),
        "names that should have been sanitised: {failures:#?}"
    );
}

/// Repeated names are numbered; a name that is itself numbered gets another
/// suffix; a leftover directory that is not a session is skipped, not reused.
#[test]
fn create_numbers_name_collisions() {
    let env = Env::new(5208);
    env.init("clone");
    let repo = env.repo();
    let input = |n: &str| json!({"name": n, "cwd": repo});
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("dup"), &repo, None),
        "dup",
    );
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("dup"), &repo, None),
        "dup-2",
    );
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("dup"), &repo, None),
        "dup-3",
    );
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("dup-2"), &repo, None),
        "dup-2-2",
    );

    // A leftover directory (not in state) with someone's file in it.
    let leftover = sessions_dir(&env).join("left");
    fs::create_dir_all(&leftover).unwrap();
    fs::write(leftover.join("keep.txt"), "precious\n").unwrap();
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("left"), &repo, None),
        "left-2",
    );
    assert_eq!(read(leftover.join("keep.txt")), "precious\n");
    assert!(env.state()["sessions"]["left"].is_null());

    // After removing dup, the name is free again.
    let dup = session_path(&env, "dup");
    let r = hook(&env, "worktree-remove", &remove_input(&dup), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    expect_created(
        &env,
        &hook(&env, "worktree-create", &input("dup"), &repo, None),
        "dup",
    );
}

/// Where the hook looks for the repository: the input's cwd, then
/// CLAUDE_PROJECT_DIR, then the process directory; CLAUDE_PROJECT_DIR wins
/// over a cwd that is not inside a session.
#[test]
fn create_finds_the_repository_from_cwd_or_project_dir() {
    let env = Env::new(5210);
    env.init("clone");
    let repo = env.repo();
    let outside = plain_dir(&env, "elsewhere");

    // Only CLAUDE_PROJECT_DIR (no cwd in the input, process elsewhere).
    let r = hook(
        &env,
        "worktree-create",
        &json!({"name": "pd-only"}),
        &outside,
        Some(&repo),
    );
    expect_created(&env, &r, "pd-only");

    // Only cwd, a subdirectory of the repository.
    let r = hook(
        &env,
        "worktree-create",
        &json!({"name": "cwd-sub", "cwd": repo.join("src")}),
        &outside,
        None,
    );
    expect_created(&env, &r, "cwd-sub");

    // CLAUDE_PROJECT_DIR points nowhere: fall back to cwd.
    let gone = env.root.path().join("does-not-exist");
    let r = hook(
        &env,
        "worktree-create",
        &json!({"name": "pd-missing", "cwd": repo}),
        &outside,
        Some(&gone),
    );
    expect_created(&env, &r, "pd-missing");

    // cwd is not a repository but CLAUDE_PROJECT_DIR is.
    let r = hook(
        &env,
        "worktree-create",
        &json!({"name": "pd-wins", "cwd": outside}),
        &outside,
        Some(&repo),
    );
    expect_created(&env, &r, "pd-wins");

    // Nothing but the process's own directory.
    let r = hook(
        &env,
        "worktree-create",
        &json!({"name": "proc-cwd"}),
        &repo,
        None,
    );
    expect_created(&env, &r, "proc-cwd");

    // All of them in the one project (project_dir() asserts exactly one).
    assert_eq!(env.state()["sessions"].as_object().unwrap().len(), 5);
    for s in env.state()["sessions"].as_object().unwrap().values() {
        assert!(
            s["parent"].is_null(),
            "template sessions have no parent: {s}"
        );
    }
}

/// A subagent started from inside a session forks that session: it gets the
/// parent's commits, uncommitted edits, untracked and ignored files, and the
/// two are isolated afterwards. CLAUDE_PROJECT_DIR (the repository) does not
/// override a cwd that is inside a session.
#[test]
fn create_inside_a_session_forks_it_with_uncommitted_and_ignored_files() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5212 + i as u16 * 2);
        env.init(backend);
        let parent = fork(&env, "parent");
        fs::write(parent.join("committed.txt"), "c\n").unwrap();
        env.git(&parent, &["add", "committed.txt"]);
        env.git(&parent, &["commit", "-q", "-m", "parent commit"]);
        fs::write(parent.join("src/app.txt"), "edited by parent\n").unwrap();
        fs::write(parent.join("untracked.txt"), "u\n").unwrap();
        fs::write(parent.join("deps/extra.txt"), "ignored\n").unwrap();
        fs::write(parent.join(".env"), "SECRET=parent\n").unwrap();

        let r = hook(
            &env,
            "worktree-create",
            &create_input("child", &parent.join("src")),
            &parent.join("src"),
            Some(&env.repo()),
        );
        let child = expect_created(&env, &r, "child");
        assert_eq!(
            env.state()["sessions"]["child"]["parent"],
            "parent",
            "[{backend}]"
        );
        assert_eq!(read(child.join("committed.txt")), "c\n", "[{backend}]");
        assert_eq!(
            read(child.join("src/app.txt")),
            "edited by parent\n",
            "[{backend}]"
        );
        assert_eq!(read(child.join("untracked.txt")), "u\n", "[{backend}]");
        assert_eq!(
            read(child.join("deps/extra.txt")),
            "ignored\n",
            "[{backend}]"
        );
        assert_eq!(
            read(child.join("deps/installed.txt")),
            "lib 1.0\n",
            "[{backend}]"
        );
        assert_eq!(read(child.join(".env")), "SECRET=parent\n", "[{backend}]");
        assert_eq!(
            env.git(&child, &["rev-parse", "HEAD~0"]),
            env.git(&parent, &["rev-parse", "HEAD"]),
            "[{backend}] child starts at the parent's HEAD"
        );
        assert_eq!(
            env.git(&parent, &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "shoal/parent",
            "[{backend}] parent's branch must not change"
        );

        // Isolation both ways.
        fs::write(child.join("src/app.txt"), "edited by child\n").unwrap();
        fs::write(parent.join("only-parent.txt"), "p\n").unwrap();
        assert_eq!(
            read(parent.join("src/app.txt")),
            "edited by parent\n",
            "[{backend}]"
        );
        assert!(!child.join("only-parent.txt").exists(), "[{backend}]");

        // A fork of the fork (grandchild) works too.
        let r = hook(
            &env,
            "worktree-create",
            &create_input("grandchild", &child),
            &child,
            None,
        );
        let grand = expect_created(&env, &r, "grandchild");
        assert_eq!(env.state()["sessions"]["grandchild"]["parent"], "child");
        assert_eq!(
            read(grand.join("src/app.txt")),
            "edited by child\n",
            "[{backend}]"
        );
    }
}

/// The template is refreshed when the source branch moved since init (setup
/// re-runs when a setup input changed, still without touching stdout), and
/// after the branch is rewritten (amend).
#[test]
fn create_refreshes_the_template_when_the_branch_moved() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5216 + i as u16 * 2);
        write_config(
            &env,
            &config_for(
                &env,
                backend,
                "echo REFRESH-NOISE; mkdir -p deps && cat deps.lock > deps/installed.txt",
            ),
        );
        let repo = env.repo();
        let r = hook(
            &env,
            "worktree-create",
            &create_input("one", &repo),
            &repo,
            None,
        );
        expect_created(&env, &r, "one");

        // Unchanged branch: no new generation.
        let r = hook(
            &env,
            "worktree-create",
            &create_input("two", &repo),
            &repo,
            None,
        );
        expect_created(&env, &r, "two");
        assert_eq!(
            env.state()["generations"].as_array().unwrap().len(),
            1,
            "[{backend}]"
        );

        fs::write(repo.join("src/app.txt"), "new upstream\n").unwrap();
        fs::write(repo.join("deps.lock"), "lib 2.0\n").unwrap();
        env.git(&repo, &["commit", "-q", "-am", "upstream moved"]);
        let head = env.git(&repo, &["rev-parse", "HEAD"]);
        let r = hook(
            &env,
            "worktree-create",
            &create_input("three", &repo),
            &repo,
            None,
        );
        let three = expect_created(&env, &r, "three");
        assert!(!r.stdout.contains("NOISE"), "[{backend}] {:?}", r.stdout);
        assert_eq!(env.git(&three, &["rev-parse", "HEAD"]), head, "[{backend}]");
        assert_eq!(read(three.join("src/app.txt")), "new upstream\n");
        assert_eq!(
            read(three.join("deps/installed.txt")),
            "lib 2.0\n",
            "[{backend}] setup re-ran"
        );
        assert_eq!(env.state()["generations"].as_array().unwrap().len(), 2);
        // Existing sessions are untouched.
        assert_eq!(
            read(session_path(&env, "one").join("src/app.txt")),
            "line one\nline two\nline three\n"
        );

        // History rewritten in the source (amend): next session follows it.
        fs::write(repo.join("src/app.txt"), "amended\n").unwrap();
        env.git(&repo, &["commit", "-q", "-a", "--amend", "-m", "amended"]);
        let amended = env.git(&repo, &["rev-parse", "HEAD"]);
        let r = hook(
            &env,
            "worktree-create",
            &create_input("four", &repo),
            &repo,
            None,
        );
        let four = expect_created(&env, &r, "four");
        assert_eq!(
            env.git(&four, &["rev-parse", "HEAD"]),
            amended,
            "[{backend}]"
        );
        assert_eq!(read(four.join("src/app.txt")), "amended\n");
        assert_eq!(env.state()["generations"].as_array().unwrap().len(), 3);
    }
}

/// Not a repository, an empty repository, a detached HEAD: exit 1 with a
/// reason, nothing on stdout, and no project left behind.
#[test]
fn create_fails_loudly_outside_a_usable_repository() {
    let env = Env::new(5220);
    let plain = plain_dir(&env, "plain");
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &plain),
        &plain,
        None,
    );
    assert_eq!(r.code, 1, "stdout {:?} stderr {:?}", r.stdout, r.stderr);
    assert!(
        r.stdout.trim().is_empty(),
        "no path may be printed: {:?}",
        r.stdout
    );
    assert!(r.stderr.contains("worktree-create"), "{}", r.stderr);
    assert!(
        r.stderr.contains("git"),
        "reason should mention git: {}",
        r.stderr
    );
    assert!(!has_project_state(&env));

    // A repository with no commits.
    let empty = plain_dir(&env, "empty");
    env.git(&empty, &["init", "-q", "-b", "main"]);
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &empty),
        &empty,
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.trim().is_empty());
    assert!(!r.stderr.trim().is_empty());
    assert!(!has_project_state(&env));

    // Detached HEAD in the source repository.
    let repo = env.repo();
    env.git(&repo, &["checkout", "-q", "--detach"]);
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &repo),
        &repo,
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.trim().is_empty());
    assert!(r.stderr.contains("detached"), "{}", r.stderr);
    assert!(!has_project_state(&env));

    // The cwd does not exist at all.
    let gone = env.root.path().join("gone");
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &gone),
        &plain,
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.trim().is_empty());
}

/// Malformed, truncated or oversized JSON fails loudly; empty or whitespace
/// stdin (and JSON that is not an object) means "no fields".
#[test]
fn create_handles_bad_and_empty_stdin() {
    let env = Env::new(5222);
    env.init("clone");
    let repo = env.repo();
    for bad in [
        &b"{not json"[..],
        b"{\"name\": \"x\"",
        b"\xff\xfe\x00garbage",
        b"nul\x00l",
    ] {
        let (r, _) = run_hook_raw(&env, "worktree-create", bad, &repo, None, &[]);
        assert_eq!(r.code, 1, "input {bad:?}: {}", r.stderr);
        assert!(r.stdout.is_empty(), "input {bad:?} printed {:?}", r.stdout);
        assert!(!r.stderr.trim().is_empty());
    }
    assert!(env.state()["sessions"].as_object().unwrap().is_empty());

    // More than the 4 MiB input limit: must fail, not hang, not half-create.
    let mut big = br#"{"name": "big", "pad": ""#.to_vec();
    big.extend(std::iter::repeat_n(b'x', 5 * 1024 * 1024));
    big.extend(br#""}"#);
    let (r, took) = run_hook_raw(&env, "worktree-create", &big, &repo, None, &[]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(took < Duration::from_secs(20));
    assert!(env.state()["sessions"].as_object().unwrap().is_empty());

    // Empty / whitespace stdin: defaults (process cwd, name "worktree").
    let (r, _) = run_hook_raw(&env, "worktree-create", b"", &repo, None, &[]);
    expect_created(&env, &r, "worktree");
    let (r, _) = run_hook_raw(&env, "worktree-create", b"  \n\t ", &repo, None, &[]);
    expect_created(&env, &r, "worktree-2");
    for (raw, expected) in [
        (&b"[]"[..], "worktree-3"),
        (b"null", "worktree-4"),
        (b"\"str\"", "worktree-5"),
    ] {
        let (r, _) = run_hook_raw(&env, "worktree-create", raw, &repo, None, &[]);
        expect_created(&env, &r, expected);
    }
    // A large but legal input works.
    let mut ok = br#"{"name": "padded", "pad": ""#.to_vec();
    ok.extend(std::iter::repeat_n(b'y', 1024 * 1024));
    ok.extend(br#""}"#);
    let (r, _) = run_hook_raw(&env, "worktree-create", &ok, &repo, None, &[]);
    expect_created(&env, &r, "padded");
}

/// A failing setup fails the hook, leaves no half-built project, and the next
/// attempt after fixing shoal.toml succeeds. An invalid shoal.toml is
/// reported.
#[test]
fn create_reports_setup_and_config_failures() {
    let env = Env::new(5224);
    let repo = env.repo();
    write_config(
        &env,
        &config_for(
            &env,
            "clone",
            "echo partial > half.txt; echo SETUP-FAILED-OUT; exit 7",
        ),
    );
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &repo),
        &repo,
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stdout.is_empty(),
        "stdout must stay empty on failure: {:?}",
        r.stdout
    );
    assert!(r.stderr.contains("setup"), "{}", r.stderr);
    assert!(!has_project_state(&env));
    let leftover: Vec<PathBuf> = fs::read_dir(env.home().join("projects"))
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "half-built project left behind: {leftover:?}"
    );
    assert!(
        !repo.join("half.txt").exists(),
        "setup must run in the template, not the source"
    );

    write_config(
        &env,
        &config_for(
            &env,
            "clone",
            "mkdir -p deps && cat deps.lock > deps/installed.txt",
        ),
    );
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &repo),
        &repo,
        None,
    );
    let x = expect_created(&env, &r, "x");
    assert!(x.join("deps/installed.txt").exists());

    // An invalid shoal.toml in a second repository.
    let env2 = Env::new(5226);
    write_config(&env2, "bogus_key = 1\n");
    let r = hook(
        &env2,
        "worktree-create",
        &create_input("y", &env2.repo()),
        &env2.repo(),
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.is_empty());
    assert!(r.stderr.contains("shoal.toml"), "{}", r.stderr);
    assert!(!has_project_state(&env2));
}

/// Several agents creating worktrees at once in a repository with no project:
/// one project, one template, distinct numbered sessions.
#[test]
fn create_concurrently_on_first_use() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5228 + i as u16 * 2);
        write_config(
            &env,
            &config_for(
                &env,
                backend,
                "sleep 0.5; mkdir -p deps && cat deps.lock > deps/installed.txt",
            ),
        );
        let repo = env.repo();
        let results: Vec<HookResult> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        hook(
                            &env,
                            "worktree-create",
                            &create_input("par", &repo),
                            &repo,
                            None,
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut paths = BTreeSet::new();
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
            assert_eq!(r.stdout.lines().count(), 1, "[{backend}] {:?}", r.stdout);
            let p = PathBuf::from(r.last_line());
            assert!(
                p.join("deps/installed.txt").exists(),
                "[{backend}] {}",
                p.display()
            );
            paths.insert(p);
        }
        assert_eq!(paths.len(), 4, "[{backend}] distinct paths");
        let names: BTreeSet<String> = env.state()["sessions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let expected: BTreeSet<String> = ["par", "par-2", "par-3", "par-4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(names, expected, "[{backend}]");
        assert_eq!(
            env.state()["generations"].as_array().unwrap().len(),
            1,
            "[{backend}]"
        );
    }
}

/// With a corrupt state.json, WorktreeCreate fails loudly and does not
/// silently re-initialise (which would orphan every existing session).
#[test]
fn create_with_corrupt_state_fails_without_reinitialising() {
    let env = Env::new(5232);
    env.init("clone");
    let s = fork(&env, "s");
    let state_path = env.project_dir().join("state.json");
    fs::write(&state_path, "{{{ garbage").unwrap();
    let r = hook(
        &env,
        "worktree-create",
        &create_input("x", &env.repo()),
        &env.repo(),
        None,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.is_empty());
    assert!(r.stderr.contains("state"), "{}", r.stderr);
    assert_eq!(
        read(&state_path),
        "{{{ garbage",
        "state must not be overwritten"
    );
    assert!(s.join("src/app.txt").exists());
}

// ---------------------------------------------------------------------------
// WorktreeRemove
// ---------------------------------------------------------------------------

/// Uncommitted, untracked and committed work is saved as shoal/<name> in the
/// source repository before the session is deleted; the user's checkout is
/// untouched.
#[test]
fn remove_saves_uncommitted_work_then_deletes() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5234 + i as u16 * 2);
        env.init(backend);
        let repo = env.repo();
        let main_before = env.git(&repo, &["rev-parse", "main"]);
        let r = hook(
            &env,
            "worktree-create",
            &create_input("work", &repo),
            &repo,
            None,
        );
        let path = expect_created(&env, &r, "work");
        fs::write(path.join("src/app.txt"), "changed in session\n").unwrap();
        fs::write(path.join("new.txt"), "brand new\n").unwrap();
        fs::remove_file(path.join("README.md")).unwrap();
        env.api("checkpoint", json!({"session": "work", "label": "mine"}))
            .unwrap();

        let r = hook(&env, "worktree-remove", &remove_input(&path), &repo, None);
        assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
        assert!(r.stdout.trim().is_empty(), "[{backend}] {:?}", r.stdout);
        assert!(
            !path.exists(),
            "[{backend}] session directory should be gone"
        );
        assert!(env.state()["sessions"]["work"].is_null());
        assert!(
            checkpoints(&env, "work").is_empty(),
            "[{backend}] checkpoints dropped from state"
        );
        assert!(!env.project_dir().join("checkpoints/work").exists());

        assert_eq!(
            source_branches(&env),
            vec!["shoal/work".to_string()],
            "[{backend}]"
        );
        assert_eq!(
            env.git(&repo, &["show", "shoal/work:src/app.txt"]),
            "changed in session\n"
        );
        assert_eq!(
            env.git(&repo, &["show", "shoal/work:new.txt"]),
            "brand new\n"
        );
        let tree = env.git(&repo, &["ls-tree", "--name-only", "shoal/work"]);
        assert!(
            !tree.lines().any(|l| l == "README.md"),
            "[{backend}] deletion saved too"
        );
        assert!(env
            .git(&repo, &["log", "-1", "--format=%s", "shoal/work"])
            .contains("WIP"));
        assert_eq!(env.git(&repo, &["rev-parse", "shoal/work~1"]), main_before);

        // The user's checkout did not move or change.
        assert_eq!(env.git(&repo, &["rev-parse", "main"]), main_before);
        assert_eq!(
            env.git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "main"
        );
        assert!(env.git(&repo, &["status", "--porcelain"]).trim().is_empty());
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
    }
}

/// Committed (but clean) work is imported as-is, with no WIP commit.
#[test]
fn remove_imports_committed_work_without_wip() {
    let env = Env::new(5238);
    env.init("clone");
    let repo = env.repo();
    let path = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("c", &repo),
            &repo,
            None,
        ),
        "c",
    );
    fs::write(path.join("feature.txt"), "f\n").unwrap();
    env.git(&path, &["add", "feature.txt"]);
    env.git(&path, &["commit", "-q", "-m", "a feature"]);
    let head = env.git(&path, &["rev-parse", "HEAD"]);
    let r = hook(&env, "worktree-remove", &remove_input(&path), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!path.exists());
    assert_eq!(env.git(&repo, &["rev-parse", "shoal/c"]), head);
    assert_eq!(
        env.git(&repo, &["log", "-1", "--format=%s", "shoal/c"])
            .trim(),
        "a feature"
    );
}

/// A clean session with no commits is removed without creating a branch.
#[test]
fn remove_clean_session_creates_no_branch() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5240 + i as u16 * 2);
        env.init(backend);
        let repo = env.repo();
        let path = expect_created(
            &env,
            &hook(
                &env,
                "worktree-create",
                &create_input("clean", &repo),
                &repo,
                None,
            ),
            "clean",
        );
        // Ignored files only: not work that a branch could carry.
        fs::write(path.join("deps/cache.bin"), "cache\n").unwrap();
        let r = hook(&env, "worktree-remove", &remove_input(&path), &repo, None);
        assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
        assert!(!path.exists(), "[{backend}]");
        assert!(
            source_branches(&env).is_empty(),
            "[{backend}] {:?}",
            source_branches(&env)
        );
        assert!(env.state()["sessions"].as_object().unwrap().is_empty());
    }
}

/// Paths that are not shoal sessions are refused (exit 1) and survive: a
/// directory outside shoal, the source repository, the project directory, a
/// template, the sessions directory, a leftover directory that is not in
/// state, and a dangling symlink.
#[test]
fn remove_refuses_paths_that_are_not_sessions() {
    let env = Env::new(5244);
    env.init("clone");
    let repo = env.repo();
    let real = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("real", &repo),
            &repo,
            None,
        ),
        "real",
    );

    let outside = plain_dir(&env, "precious");
    fs::write(outside.join("keep.txt"), "do not delete\n").unwrap();
    let project = env.project_dir();
    let ghost = sessions_dir(&env).join("ghost");
    fs::create_dir_all(&ghost).unwrap();
    fs::write(ghost.join("keep.txt"), "ghost data\n").unwrap();
    let dangling = env.root.path().join("dangling-link");
    std::os::unix::fs::symlink(env.root.path().join("nowhere"), &dangling).unwrap();

    let candidates = vec![
        outside.clone(),
        repo.clone(),
        repo.join("src"),
        project.clone(),
        project.join("templates/g1"),
        sessions_dir(&env),
        project.join("checkpoints"),
        ghost.clone(),
        env.home(),
        dangling.clone(),
    ];
    for path in &candidates {
        let r = hook(&env, "worktree-remove", &remove_input(path), &repo, None);
        assert_eq!(
            r.code,
            1,
            "{} should be refused; stderr {}",
            path.display(),
            r.stderr
        );
        assert!(!r.stderr.trim().is_empty());
        assert!(
            path.symlink_metadata().is_ok(),
            "{} was deleted",
            path.display()
        );
    }
    assert_eq!(read(outside.join("keep.txt")), "do not delete\n");
    assert_eq!(read(ghost.join("keep.txt")), "ghost data\n");
    assert_eq!(read(repo.join("README.md")), "demo\n");
    assert!(real.join("src/app.txt").exists());
    assert!(env.state()["sessions"]["real"].is_object());
    assert!(project.join("templates/g1/src/app.txt").exists());
}

/// A path inside a session (a subdirectory, its .git) is refused and nothing
/// is deleted.
#[test]
fn remove_refuses_a_path_inside_a_session() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5246 + i as u16 * 2);
        env.init(backend);
        let repo = env.repo();
        let path = expect_created(
            &env,
            &hook(
                &env,
                "worktree-create",
                &create_input("s", &repo),
                &repo,
                None,
            ),
            "s",
        );
        fs::write(path.join("src/work.txt"), "work\n").unwrap();
        for inner in [
            path.join("src"),
            path.join(".git"),
            path.join("src/work.txt"),
        ] {
            let r = hook(&env, "worktree-remove", &remove_input(&inner), &repo, None);
            assert_eq!(
                r.code,
                1,
                "[{backend}] {} should be refused",
                inner.display()
            );
            assert!(
                r.stderr.contains("inside session"),
                "[{backend}] {}",
                r.stderr
            );
            assert!(inner.exists(), "[{backend}] {} deleted", inner.display());
        }
        assert_eq!(read(path.join("src/work.txt")), "work\n");
        assert!(env.state()["sessions"]["s"].is_object());
        assert!(
            source_branches(&env).is_empty(),
            "[{backend}] nothing saved for a refused removal"
        );
    }
}

/// A path that no longer exists is fine (exit 0); removing twice is fine; a
/// trailing slash or "/./" in the session path still names the session.
#[test]
fn remove_tolerates_gone_paths_and_path_spellings() {
    let env = Env::new(5250);
    env.init("clone");
    let repo = env.repo();
    let r = hook(
        &env,
        "worktree-remove",
        &remove_input(&env.root.path().join("never-existed")),
        &repo,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);

    let a = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("a", &repo),
            &repo,
            None,
        ),
        "a",
    );
    let with_slash = PathBuf::from(format!("{}/", a.display()));
    let r = hook(
        &env,
        "worktree-remove",
        &remove_input(&with_slash),
        &repo,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!a.exists());
    let r = hook(&env, "worktree-remove", &remove_input(&a), &repo, None);
    assert_eq!(r.code, 0, "second removal: {}", r.stderr);

    let b = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("b", &repo),
            &repo,
            None,
        ),
        "b",
    );
    let dotted = sessions_dir(&env).join(".").join("b");
    let r = hook(&env, "worktree-remove", &remove_input(&dotted), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!b.exists());
    assert!(env.state()["sessions"].as_object().unwrap().is_empty());
}

/// Missing, empty or non-string worktree_path, malformed JSON and empty
/// stdin all fail loudly.
#[test]
fn remove_requires_a_worktree_path() {
    let env = Env::new(5252);
    env.init("clone");
    let repo = env.repo();
    let s = fork(&env, "s");
    for input in [
        json!({}),
        json!({"worktree_path": ""}),
        json!({"worktree_path": 5}),
        json!({"worktree_path": null}),
    ] {
        let r = hook(&env, "worktree-remove", &input, &s, None);
        assert_eq!(r.code, 1, "{input}: {}", r.stderr);
        assert!(r.stderr.contains("worktree_path"), "{input}: {}", r.stderr);
    }
    for raw in [&b""[..], b"{bad json", b"   "] {
        let (r, _) = run_hook_raw(&env, "worktree-remove", raw, &s, None, &[]);
        assert_eq!(r.code, 1, "{raw:?}: {}", r.stderr);
    }
    assert!(s.exists(), "the session the hook ran in must survive");
    assert!(env.state()["sessions"]["s"].is_object());
    let _ = repo;
}

/// If saving the work fails (the source repository is unavailable), the
/// session is not deleted; once the source is back, removal saves and deletes.
#[test]
fn remove_keeps_the_session_when_saving_fails() {
    let env = Env::new(5254);
    env.init("clone");
    let repo = env.repo();
    let path = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("w", &repo),
            &repo,
            None,
        ),
        "w",
    );
    fs::write(path.join("src/app.txt"), "unsaved work\n").unwrap();
    let moved = env.root.path().join("repo-moved");
    fs::rename(&repo, &moved).unwrap();
    let r = hook(
        &env,
        "worktree-remove",
        &remove_input(&path),
        env.root.path(),
        None,
    );
    fs::rename(&moved, &repo).unwrap();
    assert_eq!(
        r.code, 1,
        "removal must fail when the work cannot be saved: {}",
        r.stderr
    );
    assert!(
        path.exists(),
        "session deleted although its work was not saved"
    );
    assert_eq!(read(path.join("src/app.txt")), "unsaved work\n");
    assert!(env.state()["sessions"]["w"].is_object());

    let r = hook(&env, "worktree-remove", &remove_input(&path), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!path.exists());
    assert_eq!(
        env.git(&repo, &["show", "shoal/w:src/app.txt"]),
        "unsaved work\n"
    );
}

// Fixed bug: (critical) worktree-remove force-fetches into shoal/<name>, so a reused
// worktree name overwrites the work saved from the earlier session.
/// A second session with a name that was used before (Claude Code reuses
/// worktree names) is saved with a forced fetch to the same branch
/// shoal/<name>, overwriting the first session's saved work. The first WIP
/// commit is left on no branch at all (only in the reflog).
#[test]
fn remove_does_not_overwrite_work_saved_by_an_earlier_session_of_the_same_name() {
    let env = Env::new(5256);
    env.init("clone");
    let repo = env.repo();
    let first = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("fix", &repo),
            &repo,
            None,
        ),
        "fix",
    );
    fs::write(first.join("first.txt"), "first attempt\n").unwrap();
    assert_eq!(
        hook(&env, "worktree-remove", &remove_input(&first), &repo, None).code,
        0
    );
    let saved_first = env
        .git(&repo, &["rev-parse", "shoal/fix"])
        .trim()
        .to_string();

    let second = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("fix", &repo),
            &repo,
            None,
        ),
        "fix",
    );
    fs::write(second.join("second.txt"), "second attempt\n").unwrap();
    let r = hook(&env, "worktree-remove", &remove_input(&second), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);

    let holders = env.git(
        &repo,
        &[
            "for-each-ref",
            "--contains",
            &saved_first,
            "--format=%(refname)",
            "refs/heads/",
        ],
    );
    assert!(
        !holders.trim().is_empty(),
        "the first session's saved work ({saved_first}) is on no branch any more; branches: {:?}",
        source_branches(&env)
    );
    // And the second session's work is saved somewhere too.
    let all = source_branches(&env);
    assert!(
        all.iter().any(|b| env
            .git(&repo, &["ls-tree", "--name-only", b])
            .lines()
            .any(|l| l == "second.txt")),
        "second session's work not saved: {all:?}"
    );
}

// ---------------------------------------------------------------------------
// Stop / SubagentStop
// ---------------------------------------------------------------------------

/// Stop creates turn-1; an unchanged session gets nothing; an edit, a commit
/// and a SubagentStop from a subdirectory each create the next turn. The
/// checkpoints really hold the state (rewind restores turn-2).
#[test]
fn stop_checkpoints_only_when_something_changed() {
    if !overlay_available() {
        eprintln!("skipping: checkpoints are only cheap here with the overlay backend");
        return;
    }
    let env = Env::new(5260);
    env.init(cheap_backend().unwrap());
    let s = fork(&env, "s");
    let stop = |cwd: &Path, event: &str| {
        let r = hook(&env, "stop", &turn_input(event, cwd), cwd, None);
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(
            r.stdout.is_empty(),
            "stop must print nothing: {:?}",
            r.stdout
        );
    };
    stop(&s, "Stop");
    assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
    stop(&s, "Stop");
    assert_eq!(
        auto_labels(&env, "s"),
        vec!["turn-1"],
        "no change, no checkpoint"
    );

    fs::write(s.join("src/app.txt"), "turn two\n").unwrap();
    stop(&s, "Stop");
    assert_eq!(auto_labels(&env, "s"), vec!["turn-1", "turn-2"]);
    stop(&s, "Stop");
    assert_eq!(auto_labels(&env, "s").len(), 2);

    env.git(&s, &["commit", "-q", "-am", "commit in session"]);
    stop(&s, "Stop");
    assert_eq!(auto_labels(&env, "s").len(), 3, "a commit is a change");

    fs::write(s.join("src/sub.txt"), "subagent\n").unwrap();
    stop(&s.join("src"), "SubagentStop");
    assert_eq!(
        auto_labels(&env, "s"),
        vec!["turn-1", "turn-2", "turn-3", "turn-4"]
    );
    for (_, auto) in checkpoints(&env, "s") {
        assert!(auto);
    }

    env.api("rewind", json!({"session": "s", "label": "turn-2"}))
        .unwrap();
    assert_eq!(read(s.join("src/app.txt")), "turn two\n");
    assert!(!s.join("src/sub.txt").exists());
}

// Fixed bug: (medium) the Stop hook's change detection misses edits to ignored files.
/// The change fingerprint ignores ignored files (it uses `git status`
/// without --ignored), so a turn that only edits an ignored file (.env,
/// generated config, a vendored dependency) is never checkpointed even
/// though checkpoints are documented to include ignored files.
#[test]
fn stop_checkpoints_a_change_to_an_ignored_file() {
    if !overlay_available() {
        eprintln!("skipping: needs the overlay backend");
        return;
    }
    let env = Env::new(5262);
    env.init(cheap_backend().unwrap());
    let s = fork(&env, "s");
    assert_eq!(
        hook(&env, "stop", &turn_input("Stop", &s), &s, None).code,
        0
    );
    assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
    fs::write(s.join(".env"), "SECRET=rotated\n").unwrap();
    assert_eq!(
        hook(&env, "stop", &turn_input("Stop", &s), &s, None).code,
        0
    );
    assert_eq!(
        auto_labels(&env, "s"),
        vec!["turn-1", "turn-2"],
        "an edit to an ignored file is a change"
    );
}

/// More than 20 changing turns keep the newest 20 automatic checkpoints, on
/// disk and in state; manual checkpoints are never pruned.
#[test]
fn stop_keeps_the_newest_twenty_and_never_prunes_manual() {
    if !overlay_available() {
        eprintln!("skipping: needs the overlay backend");
        return;
    }
    let env = Env::new(5264);
    env.init(cheap_backend().unwrap());
    let s = fork(&env, "s");
    env.api("checkpoint", json!({"session": "s", "label": "before"}))
        .unwrap();
    for i in 1..=23 {
        fs::write(s.join("src/app.txt"), format!("turn {i}\n")).unwrap();
        if i == 10 {
            env.api("checkpoint", json!({"session": "s"})).unwrap();
        }
        let r = hook(&env, "stop", &turn_input("Stop", &s), &s, None);
        assert_eq!(r.code, 0, "turn {i}: {}", r.stderr);
    }
    let autos = auto_labels(&env, "s");
    let expected: Vec<String> = (4..=23).map(|i| format!("turn-{i}")).collect();
    assert_eq!(autos, expected);
    let manual: Vec<String> = checkpoints(&env, "s")
        .into_iter()
        .filter(|(_, auto)| !*auto)
        .map(|(l, _)| l)
        .collect();
    assert_eq!(manual.len(), 2, "{manual:?}");
    assert!(manual.contains(&"before".to_string()));
    let dir = env.project_dir().join("checkpoints/s");
    for i in 1..=3 {
        assert!(
            !dir.join(format!("turn-{i}")).exists(),
            "turn-{i} still on disk"
        );
    }
    for label in expected.iter().chain(manual.iter()) {
        assert!(dir.join(label).exists(), "{label} missing on disk");
    }
    // Oldest kept auto checkpoint restores its turn; manual one restores too.
    env.api("rewind", json!({"session": "s", "label": "turn-4"}))
        .unwrap();
    assert_eq!(read(s.join("src/app.txt")), "turn 4\n");
    env.api("rewind", json!({"session": "s", "label": "before"}))
        .unwrap();
    assert_eq!(
        read(s.join("src/app.txt")),
        "line one\nline two\nline three\n"
    );
}

/// Simultaneous Stop and SubagentStop hooks after one change make exactly one
/// checkpoint and all exit 0.
#[test]
fn stop_concurrent_hooks_make_one_checkpoint() {
    if !overlay_available() {
        eprintln!("skipping: needs the overlay backend");
        return;
    }
    let env = Env::new(5266);
    env.init(cheap_backend().unwrap());
    let s = fork(&env, "s");
    fs::write(s.join("src/app.txt"), "changed\n").unwrap();
    let results: Vec<HookResult> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..4)
            .map(|_| sc.spawn(|| hook(&env, "stop", &turn_input("SubagentStop", &s), &s, None)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in &results {
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stdout.is_empty());
    }
    assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
}

/// With the clone backend on a filesystem without reflinks (ext4 here), a
/// checkpoint would be a full copy, so Stop takes none, but still exits 0
/// and marks the session active.
#[test]
fn stop_with_expensive_checkpoints_only_marks_active() {
    let env = Env::new(5268);
    env.init("clone");
    let s = fork(&env, "s");
    set_last_active(&env, "s", 0);
    fs::write(s.join("src/app.txt"), "changed\n").unwrap();
    let r = hook(&env, "stop", &turn_input("Stop", &s), &s, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.is_empty());
    let marker = fs::read_to_string(env.project_dir().join(".reflink")).unwrap_or_default();
    if marker.trim() == "no" {
        assert!(
            checkpoints(&env, "s").is_empty(),
            "no checkpoints without reflinks"
        );
    } else {
        assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
    }
    assert!(
        env.state()["sessions"]["s"]["last_active"]
            .as_u64()
            .unwrap()
            + 60
            >= now_secs()
    );
}

/// Outside a session every turn hook is a silent no-op: in an initialised
/// repository, a plain directory, a template, and a repository with no
/// project (which must not be auto-initialised).
#[test]
fn turn_hooks_outside_a_session_are_silent_noops() {
    let env = Env::new(5270);
    env.init("clone");
    let _s = fork(&env, "s");
    let state_before = read(env.project_dir().join("state.json"));
    let plain = plain_dir(&env, "plain");
    let template = env.project_dir().join("templates/g1");
    let fresh = Env::new(5272);
    let places: Vec<(&Env, PathBuf)> = vec![
        (&env, env.repo()),
        (&env, env.repo().join("src")),
        (&env, plain.clone()),
        (&env, template),
        (&fresh, fresh.repo()),
    ];
    for (e, dir) in &places {
        for event in ["stop", "session-start", "prompt"] {
            let r = hook(e, event, &turn_input("Stop", dir), dir, None);
            assert_eq!(r.code, 0, "{event} in {}: {}", dir.display(), r.stderr);
            assert!(
                r.stdout.is_empty(),
                "{event} in {} printed {:?}",
                dir.display(),
                r.stdout
            );
            // No cwd in the input: CLAUDE_PROJECT_DIR, then the process dir.
            let r = hook(e, event, &json!({}), dir, Some(dir));
            assert_eq!(r.code, 0);
            assert!(r.stdout.is_empty());
        }
    }
    assert_eq!(
        read(env.project_dir().join("state.json")),
        state_before,
        "state must not change"
    );
    assert!(
        !has_project_state(&fresh),
        "a turn hook must never initialise a project"
    );
}

/// When another process holds the project lock, every turn hook gives up
/// within ~6 seconds, exits 0 and prints nothing; nothing is checkpointed.
#[test]
fn turn_hooks_give_up_when_the_project_is_locked() {
    let backend = cheap_backend().unwrap_or("clone");
    let env = Env::new(5274);
    env.init(backend);
    let s = fork(&env, "s");
    fs::write(s.join("src/app.txt"), "changed\n").unwrap();
    set_last_active(&env, "s", 0);
    let lock = env.project_dir().join("lock");
    {
        let _held = hold_lock(&lock);
        let results: Vec<(&str, HookResult, Duration)> = std::thread::scope(|sc| {
            let handles: Vec<_> = ["stop", "session-start", "prompt"]
                .into_iter()
                .map(|event| {
                    let s = s.clone();
                    let env = &env;
                    sc.spawn(move || {
                        let (r, took) = timed_hook(env, event, &turn_input("Stop", &s), &s);
                        (event, r, took)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (event, r, took) in results {
            assert_eq!(r.code, 0, "{event}: {}", r.stderr);
            assert!(r.stdout.is_empty(), "{event} printed {:?}", r.stdout);
            assert!(
                took < Duration::from_millis(6500),
                "{event} took {took:?} with the lock held"
            );
        }
        assert!(checkpoints(&env, "s").is_empty());
        assert_eq!(env.state()["sessions"]["s"]["last_active"], 0);
    }
    // Lock released: the hooks work again.
    let (r, took) = timed_hook(&env, "stop", &turn_input("Stop", &s), &s);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(took < Duration::from_secs(5), "{took:?}");
    if backend == "overlay" {
        assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
    }
    assert!(
        env.state()["sessions"]["s"]["last_active"]
            .as_u64()
            .unwrap()
            > 0
    );
}

/// Every turn hook exits 0 with a corrupt, empty, wrong-version or missing
/// state.json, with malformed stdin, and with oversized stdin, and
/// WorktreeRemove never deletes a session whose state it cannot read.
#[test]
fn turn_hooks_exit_zero_on_errors() {
    let env = Env::new(5276);
    env.init("clone");
    let s = fork(&env, "s");
    let state_path = env.project_dir().join("state.json");
    let good = read(&state_path);
    let mut wrong_version: Value = serde_json::from_str(&good).unwrap();
    wrong_version["version"] = json!(99);
    let corruptions: Vec<(&str, Option<String>)> = vec![
        ("garbage", Some("\u{0}\u{1}not json at all {{{".to_string())),
        ("empty", Some(String::new())),
        ("truncated", Some(good[..good.len() / 2].to_string())),
        ("wrong-version", Some(wrong_version.to_string())),
        ("wrong-shape", Some("[1, 2, 3]".to_string())),
        ("missing", None),
    ];
    for (what, content) in &corruptions {
        match content {
            Some(c) => fs::write(&state_path, c).unwrap(),
            None => {
                let _ = fs::remove_file(&state_path);
            }
        }
        for event in ["stop", "session-start", "prompt"] {
            let r = hook(&env, event, &turn_input("Stop", &s), &s, None);
            assert_eq!(r.code, 0, "{event} with {what} state: {}", r.stderr);
            assert!(
                r.stdout.is_empty(),
                "{event} with {what} state printed {:?}",
                r.stdout
            );
        }
        // WorktreeRemove must never delete a session it cannot read state for.
        let r = hook(
            &env,
            "worktree-remove",
            &remove_input(&s),
            &env.repo(),
            None,
        );
        assert_eq!(r.code, 1, "worktree-remove with {what} state: {}", r.stderr);
        assert!(
            s.join("src/app.txt").exists(),
            "session deleted with {what} state"
        );
    }
    fs::write(&state_path, &good).unwrap();

    for event in ["stop", "session-start", "prompt"] {
        for raw in [&b"{not json"[..], b"\xff\xfe", b"[1,"] {
            let (r, _) = run_hook_raw(&env, event, raw, &s, None, &[]);
            assert_eq!(r.code, 0, "{event} with {raw:?}: {}", r.stderr);
            assert!(r.stdout.is_empty());
            assert!(
                !r.stderr.trim().is_empty(),
                "{event}: malformed input should be diagnosed on stderr"
            );
        }
        let mut big = br#"{"cwd": ""#.to_vec();
        big.extend(std::iter::repeat_n(b'z', 5 * 1024 * 1024));
        big.extend(br#""}"#);
        let (r, took) = run_hook_raw(&env, event, &big, &s, None, &[]);
        assert_eq!(r.code, 0, "{event} oversized: {}", r.stderr);
        assert!(r.stdout.is_empty());
        assert!(took < Duration::from_secs(20));
    }
}

// Fixed bug: (low) with a corrupt state.json the turn hooks skip silently (session_at
// turns every error into "not in a session"), so nothing on stderr tells the
// user that per-turn checkpoints have stopped. ARCHITECTURE.md rule 5: turn
// hooks "always exit 0 and write diagnostics to stderr".
/// A Stop hook inside a session whose project state is unreadable exits 0 but
/// says why it did nothing.
#[test]
fn stop_with_corrupt_state_reports_on_stderr() {
    let env = Env::new(5284);
    env.init("clone");
    let s = fork(&env, "s");
    fs::write(env.project_dir().join("state.json"), "{{{ garbage").unwrap();
    let r = hook(&env, "stop", &turn_input("Stop", &s), &s, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.is_empty());
    assert!(
        !r.stderr.trim().is_empty(),
        "corrupt state.json was skipped with no diagnostic on stderr"
    );
}

// ---------------------------------------------------------------------------
// SessionStart / UserPromptSubmit
// ---------------------------------------------------------------------------

/// Inside a session (top level or a subdirectory, or a fork) SessionStart
/// prints one context paragraph naming the session, branch, port and source.
#[test]
fn session_start_names_the_session_branch_and_port() {
    let env = Env::new(5278);
    env.init("clone");
    let repo = env.repo();
    let s = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("ctx", &repo),
            &repo,
            None,
        ),
        "ctx",
    );
    let child = expect_created(
        &env,
        &hook(&env, "worktree-create", &create_input("kid", &s), &s, None),
        "kid",
    );
    for (name, dir) in [
        ("ctx", s.clone()),
        ("ctx", s.join("src")),
        ("kid", child.clone()),
    ] {
        let input = json!({"hook_event_name": "SessionStart", "source": "startup", "cwd": dir});
        let r = hook(&env, "session-start", &input, &dir, None);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let port = env.state()["sessions"][name]["port"].as_u64().unwrap();
        assert!(r.stdout.contains(&format!("\"{name}\"")), "{:?}", r.stdout);
        assert!(
            r.stdout.contains(&format!("shoal/{name}")),
            "{:?}",
            r.stdout
        );
        assert!(r.stdout.contains(&port.to_string()), "{:?}", r.stdout);
        assert!(
            r.stdout
                .contains(&repo.canonicalize().unwrap().display().to_string()),
            "{:?}",
            r.stdout
        );
        assert_eq!(
            r.stdout.trim().lines().count(),
            1,
            "one paragraph: {:?}",
            r.stdout
        );
    }
    // With the cwd only in CLAUDE_PROJECT_DIR.
    let r = hook(&env, "session-start", &json!({}), env.root.path(), Some(&s));
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("\"ctx\""), "{:?}", r.stdout);
    // Outside: nothing.
    let r = hook(&env, "session-start", &json!({"cwd": repo}), &repo, None);
    assert_eq!(r.code, 0);
    assert!(r.stdout.is_empty(), "{:?}", r.stdout);
}

/// UserPromptSubmit marks only the session it runs in as active, and changes
/// nothing outside a session.
#[test]
fn prompt_marks_only_its_session_active() {
    let env = Env::new(5280);
    env.init("clone");
    let a = fork(&env, "a");
    let _b = fork(&env, "b");
    set_last_active(&env, "a", 0);
    set_last_active(&env, "b", 0);
    let r = hook(
        &env,
        "prompt",
        &json!({"hook_event_name": "UserPromptSubmit", "prompt": "hi", "cwd": a.join("src")}),
        &a,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.is_empty(),
        "prompt must not add context: {:?}",
        r.stdout
    );
    let state = env.state();
    let a_active = state["sessions"]["a"]["last_active"].as_u64().unwrap();
    assert!(
        a_active + 60 >= now_secs() && a_active <= now_secs() + 1,
        "{a_active}"
    );
    assert_eq!(
        state["sessions"]["b"]["last_active"], 0,
        "other sessions untouched"
    );

    let before = read(env.project_dir().join("state.json"));
    let r = hook(
        &env,
        "prompt",
        &json!({"prompt": "hi", "cwd": env.repo()}),
        &env.repo(),
        None,
    );
    assert_eq!(r.code, 0);
    assert_eq!(read(env.project_dir().join("state.json")), before);
}

// ---------------------------------------------------------------------------
// CLI and plugin wiring
// ---------------------------------------------------------------------------

/// Unknown or misspelt events are rejected by the CLI (non-zero, nothing on
/// stdout), and the plugin's hooks.json only uses events shoal accepts, wired
/// to the right Claude Code events.
#[test]
fn unknown_events_are_rejected_and_hooks_json_matches() {
    let env = Env::new(5282);
    env.init("clone");
    let repo = env.repo();
    for bad in [
        "bogus",
        "Stop",
        "worktree_create",
        "WorktreeCreate",
        "subagent-stop",
        "",
    ] {
        let (r, _) = run_hook_raw(&env, bad, b"{}", &repo, None, &[]);
        assert_ne!(r.code, 0, "event {bad:?} accepted");
        assert!(r.stdout.is_empty(), "{bad:?}: {:?}", r.stdout);
        assert!(!r.stderr.trim().is_empty());
    }
    let out = env.shoal_in(&repo, &["hook"]);
    assert!(
        !out.status.success(),
        "`shoal hook` with no event must fail"
    );

    let hooks: Value = serde_json::from_str(&read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/hooks/hooks.json"),
    ))
    .unwrap();
    let expected = [
        ("WorktreeCreate", "worktree-create"),
        ("WorktreeRemove", "worktree-remove"),
        ("Stop", "stop"),
        ("SubagentStop", "stop"),
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt"),
    ];
    let map = hooks["hooks"].as_object().unwrap();
    assert_eq!(
        map.len(),
        expected.len(),
        "{:?}",
        map.keys().collect::<Vec<_>>()
    );
    for (claude_event, shoal_event) in expected {
        let cmd = hooks["hooks"][claude_event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("no command for {claude_event}"));
        assert!(
            cmd.trim_end().ends_with(&format!("hook {shoal_event}")),
            "{claude_event} runs {cmd:?}"
        );
        // The wrapper, pointed at this build, runs the event.
        let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/libexec/shoal-plugin");
        let mut c = env.command(wrapper.to_str().unwrap(), &repo);
        c.args(["hook", shoal_event])
            .env("SHOAL_BIN", BIN)
            .env("GIT_CEILING_DIRECTORIES", env.root.path())
            .stdin(Stdio::null());
        let out = c.output().unwrap();
        let code = out.status.code().unwrap_or(-1);
        match shoal_event {
            // Empty stdin from the repository: a default-named session.
            "worktree-create" => assert_eq!(code, 0, "{}", stderr(&out)),
            // Empty stdin: no worktree_path.
            "worktree-remove" => assert_eq!(code, 1, "{}", stderr(&out)),
            _ => {
                assert_eq!(code, 0, "{shoal_event}: {}", stderr(&out));
                assert!(out.stdout.is_empty());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Locking and remounting
// ---------------------------------------------------------------------------

/// Start a hook without waiting for it.
fn spawn_hook(env: &Env, event: &str, input: &Value, dir: &Path) -> Child {
    let mut child = env
        .command(BIN, dir)
        .args(["hook", event])
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    child
}

fn finish(child: Child, within: Duration) -> HookResult {
    let deadline = Instant::now() + within;
    let mut child = child;
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "hook did not finish within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    HookResult {
        code: out.status.code().unwrap_or(-1),
        stdout: stdout(&out),
        stderr: stderr(&out),
    }
}

/// WorktreeCreate and WorktreeRemove wait for a busy project (they must not
/// fail or act without the lock) and complete once it is free.
#[test]
fn worktree_hooks_wait_for_the_project_lock() {
    let env = Env::new(5286);
    env.init("clone");
    let repo = env.repo();
    let victim = expect_created(
        &env,
        &hook(
            &env,
            "worktree-create",
            &create_input("victim", &repo),
            &repo,
            None,
        ),
        "victim",
    );
    fs::write(victim.join("src/app.txt"), "victim work\n").unwrap();
    let lock = env.project_dir().join("lock");

    let held = hold_lock(&lock);
    let create = spawn_hook(
        &env,
        "worktree-create",
        &create_input("waiter", &repo),
        &repo,
    );
    let remove = spawn_hook(&env, "worktree-remove", &remove_input(&victim), &repo);
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut create = create;
    let mut remove = remove;
    while Instant::now() < deadline {
        assert!(
            create.try_wait().unwrap().is_none(),
            "worktree-create finished while the project was locked"
        );
        assert!(
            remove.try_wait().unwrap().is_none(),
            "worktree-remove finished while the project was locked"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        victim.join("src/app.txt").exists(),
        "deleted while another process held the lock"
    );
    assert!(env.state()["sessions"]["waiter"].is_null());
    drop(held);

    let r = finish(create, Duration::from_secs(30));
    expect_created(&env, &r, "waiter");
    let r = finish(remove, Duration::from_secs(30));
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!victim.exists());
    assert_eq!(
        env.git(&repo, &["show", "shoal/victim:src/app.txt"]),
        "victim work\n"
    );
}

/// Overlay sessions unmounted (as after `shoal down` or a reboot) still work
/// with every hook: Stop remounts and checkpoints, WorktreeCreate inside
/// forks with the parent's work, WorktreeRemove saves the work first.
#[test]
fn overlay_hooks_work_after_unmount() {
    if !overlay_available() {
        eprintln!("skipping: needs the overlay backend");
        return;
    }
    let env = Env::new(5288);
    env.init(cheap_backend().unwrap());
    let repo = env.repo();
    let s = fork(&env, "s");
    fs::write(s.join("src/app.txt"), "before unmount\n").unwrap();
    fs::write(s.join("deps/ignored.txt"), "ignored\n").unwrap();
    env.shoal(&["down"]);
    assert!(
        !s.join("src/app.txt").exists(),
        "down should have unmounted the session"
    );

    let r = hook(&env, "stop", &turn_input("Stop", &s), &s, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(auto_labels(&env, "s"), vec!["turn-1"]);
    assert_eq!(read(s.join("src/app.txt")), "before unmount\n");

    env.shoal(&["down"]);
    let r = hook(&env, "worktree-create", &create_input("kid", &s), &s, None);
    let kid = expect_created(&env, &r, "kid");
    assert_eq!(read(kid.join("src/app.txt")), "before unmount\n");
    assert_eq!(read(kid.join("deps/ignored.txt")), "ignored\n");

    env.shoal(&["down"]);
    let r = hook(&env, "session-start", &json!({"cwd": s}), &s, None);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("\"s\""), "{:?}", r.stdout);

    env.shoal(&["down"]);
    let r = hook(&env, "worktree-remove", &remove_input(&s), &repo, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!s.exists());
    assert_eq!(
        env.git(&repo, &["show", "shoal/s:src/app.txt"]),
        "before unmount\n"
    );
    // The fork that sat on s's frozen layer is unaffected (`down` unmounted
    // it too; exec mounts it again).
    assert_eq!(
        env.shoal(&["exec", "kid", "cat", "src/app.txt"]),
        "before unmount\n"
    );
    assert_eq!(read(kid.join("deps/ignored.txt")), "ignored\n");
}
