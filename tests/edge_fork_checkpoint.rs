//! Edge-case tests for the `fork`, `checkpoint` and `rewind` tools
//! (src/tools.rs) and the operations behind them (src/ops.rs:
//! create_session, checkpoint, rewind), against plugin/ARCHITECTURE.md.
//!
//! Port range 5400-5499. Only one test starts a service, and it is
//! `sleep`, which listens on nothing; every other port is only recorded in
//! state.json. Each test still takes its own port_base inside the range
//! (step 3) so no two tests could ever collide, and backends are run one
//! after another inside a test.

mod common;
use common::*;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
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
        Ok(v) => panic!("{what} should have failed but returned {v}"),
        Err(e) => e,
    }
}

fn fork(env: &Env, args: Value) -> Value {
    ok(env.api("fork", args.clone()), &format!("fork {args}"))
}

/// Fork exactly one session and return (name, path).
fn fork1(env: &Env, args: Value) -> (String, PathBuf) {
    let v = fork(env, args);
    let s = &v["sessions"][0];
    (
        s["name"].as_str().unwrap().to_string(),
        PathBuf::from(s["path"].as_str().unwrap()),
    )
}

fn checkpoint(env: &Env, session: &str, label: Option<&str>) -> Value {
    let mut args = json!({"session": session});
    if let Some(l) = label {
        args["label"] = json!(l);
    }
    ok(
        env.api("checkpoint", args),
        &format!("checkpoint {session} {label:?}"),
    )
}

fn rewind(env: &Env, session: &str, label: &str) -> Value {
    ok(
        env.api("rewind", json!({"session": session, "label": label})),
        &format!("rewind {session} {label}"),
    )
}

fn wr(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn head(env: &Env, dir: &Path) -> String {
    env.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn branch(env: &Env, dir: &Path) -> String {
    env.git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_string()
}

fn porcelain(env: &Env, dir: &Path) -> String {
    env.git(dir, &["status", "--porcelain", "--untracked-files=all"])
}

fn commit(env: &Env, dir: &Path, msg: &str) {
    env.git(dir, &["add", "-A"]);
    env.git(dir, &["commit", "-q", "-m", msg]);
}

fn names_in(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn state_sessions(env: &Env) -> BTreeSet<String> {
    env.state()["sessions"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn state_checkpoints(env: &Env, session: &str) -> Vec<String> {
    env.state()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["session"] == session)
        .map(|c| c["label"].as_str().unwrap().to_string())
        .collect()
}

fn valid_session_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && !n.starts_with(['.', '-'])
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A file tree as (relative path -> content), skipping .git.
fn tree(dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if rel == ".git" {
                continue;
            }
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                out.insert(rel, fs::read_to_string(&p).unwrap_or_default());
            } else {
                out.insert(rel, "<special>".into());
            }
        }
    }
    out
}

/// Make the standard set of uncommitted changes in a session:
/// a modified tracked file, a staged new file, an untracked file, an
/// ignored file under deps/, and an uncommitted deletion.
fn dirty_up(dir: &Path, tag: &str) {
    wr(
        dir.join("src/app.txt"),
        &format!("line one\nCHANGED {tag}\nline three\n"),
    );
    wr(dir.join("untracked.txt"), &format!("untracked {tag}\n"));
    wr(dir.join("deps/extra.txt"), &format!("ignored {tag}\n"));
    fs::remove_file(dir.join("README.md")).unwrap();
}

fn shoal_cli(env: &Env, args: &[&str]) -> std::process::Output {
    env.shoal_in(&env.repo(), args)
}

fn poll<F: FnMut() -> bool>(within: Duration, mut f: F) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// fork: from main
// ---------------------------------------------------------------------------

#[test]
fn fork_default_starts_from_main_with_warm_template() {
    for backend in backends() {
        let env = setup(5403, backend);
        let repo_head = head(&env, &env.repo());
        let v = fork(&env, json!({}));
        assert!(v["refreshed"].is_null(), "{backend}: {v}");
        let s = &v["sessions"][0];
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(s["name"], "task", "{backend}: {v}");
        assert_eq!(s["branch"], "shoal/task");
        assert_eq!(s["port"], 5403);
        assert_eq!(s["generation"], 1);
        assert!(s["parent"].is_null());
        assert!(s["from_checkpoint"].is_null());
        let path = PathBuf::from(s["path"].as_str().unwrap());
        assert_eq!(path, env.project_dir().join("sessions/task"));
        assert_eq!(
            read(path.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
        assert_eq!(
            read(path.join("deps/installed.txt")),
            "lib 1.0\n",
            "{backend}: warm deps"
        );
        assert_eq!(
            read(path.join(".env")),
            "SECRET=1\n",
            "{backend}: copied .env"
        );
        assert_eq!(head(&env, &path), repo_head);
        assert_eq!(branch(&env, &path), "shoal/task");
        assert_eq!(
            porcelain(&env, &path),
            "",
            "{backend}: fresh session is clean"
        );

        // A second default fork gets a fresh name and the next port block.
        let (n2, p2) = fork1(&env, json!({"from": "main"}));
        assert_eq!(n2, "task-2");
        assert_eq!(branch(&env, &p2), "shoal/task-2");
        let st = env.state();
        assert_eq!(st["sessions"]["task-2"]["port"], 5413);
        // "template" is an alias of main.
        let (n3, _) = fork1(&env, json!({"from": "template"}));
        assert_eq!(n3, "task-3");
        // The source repository is untouched.
        assert_eq!(head(&env, &env.repo()), repo_head);
        assert_eq!(branch(&env, &env.repo()), "main");
    }
}

#[test]
fn fork_count_one_to_eight_numbers_sessions_with_distinct_ports_and_branches() {
    for backend in backends() {
        let env = setup(5400, backend);
        let v = fork(&env, json!({"count": 1, "name": "solo"}));
        assert_eq!(v["sessions"][0]["name"], "solo", "count 1 is not numbered");

        let v = fork(&env, json!({"count": 3, "name": "try"}));
        let names: Vec<&str> = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["try-1", "try-2", "try-3"], "{backend}");

        let v = fork(&env, json!({"count": 8}));
        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 8, "{backend}: {v}");
        assert!(v.get("error").is_none_or(Value::is_null), "{v}");
        for (i, s) in sessions.iter().enumerate() {
            assert_eq!(s["name"], format!("task-{}", i + 1));
            assert_eq!(s["branch"], format!("shoal/task-{}", i + 1));
            let p = PathBuf::from(s["path"].as_str().unwrap());
            assert_eq!(branch(&env, &p), format!("shoal/task-{}", i + 1));
            assert_eq!(read(p.join("deps/installed.txt")), "lib 1.0\n");
        }
        let st = env.state();
        let all = st["sessions"].as_object().unwrap();
        assert_eq!(all.len(), 12);
        let ports: BTreeSet<u64> = all.values().map(|s| s["port"].as_u64().unwrap()).collect();
        assert_eq!(
            ports.len(),
            12,
            "{backend}: ports must be distinct: {ports:?}"
        );
        for p in &ports {
            assert!(
                *p >= 5400 && (*p - 5400) % 10 == 0,
                "port {p} not on a 10-block"
            );
        }
        let branches: BTreeSet<&str> = all
            .values()
            .map(|s| s["branch"].as_str().unwrap())
            .collect();
        assert_eq!(branches.len(), 12);
        let paths: BTreeSet<String> = sessions.iter().map(|s| s["path"].to_string()).collect();
        assert_eq!(paths.len(), 8);
    }
}

#[test]
fn fork_count_out_of_range_or_wrong_type_is_rejected_and_creates_nothing() {
    for backend in backends() {
        let env = setup(5406, backend);
        for count in [json!(0), json!(9), json!(100), json!(u64::MAX)] {
            let e = err(
                env.api("fork", json!({"count": count})),
                &format!("count {count}"),
            );
            assert!(
                e.contains("count must be between 1 and 8"),
                "{backend} {count}: {e}"
            );
        }
        for count in [json!(-1), json!(1.5), json!("2"), json!(true), json!([2])] {
            let e = err(
                env.api("fork", json!({"count": count})),
                &format!("count {count}"),
            );
            assert!(e.contains("count"), "{backend} {count}: {e}");
        }
        // Other malformed arguments.
        let e = err(env.api("fork", json!({"name": 7})), "name 7");
        assert!(e.contains("name must be a string"), "{e}");
        let e = err(env.api("fork", json!({"from": false})), "from false");
        assert!(e.contains("from must be a string"), "{e}");
        let e = err(env.api("fork", json!({"label": "x"})), "unknown arg");
        assert!(e.contains("no argument \"label\""), "{e}");
        let e = err(env.api("fork", json!([1, 2])), "array args");
        assert!(e.contains("JSON object"), "{e}");

        assert!(
            state_sessions(&env).is_empty(),
            "{backend}: nothing may be created"
        );
        assert!(names_in(&env.project_dir().join("sessions")).is_empty());
        assert!(names_in(&env.project_dir().join("volumes")).is_empty());
    }
}

#[test]
fn fork_name_is_sanitised_and_stays_inside_the_sessions_dir() {
    for backend in backends() {
        let env = setup(5409, backend);
        let sessions_dir = env.project_dir().join("sessions");
        let cases = [
            ("My Feature!", "My-Feature"),
            ("../../etc/passwd", "etc-passwd"),
            ("a//b", "a-b"),
            (".hidden", "hidden"),
            ("-x-", "x"),
            ("", "session"),
            ("   ", "session-2"), // "session" is taken by the line above
            ("café au lait", "caf-au-lait"),
            ("fix: login bug #42", "fix-login-bug-42"),
            ("a,b:c\\d", "a-b-c-d"),
        ];
        for (raw, want) in cases {
            let (name, path) = fork1(&env, json!({"name": raw}));
            assert_eq!(name, want, "{backend}: {raw:?}");
            assert!(valid_session_name(&name));
            assert_eq!(path, sessions_dir.join(&name));
            assert_eq!(branch(&env, &path), format!("shoal/{name}"));
        }
        let long = "x".repeat(60);
        let (name, _) = fork1(&env, json!({"name": long}));
        assert_eq!(name, "x".repeat(40), "{backend}: truncated to 40");
        // Nothing escaped the project.
        assert!(!env.project_dir().join("etc").exists());
        assert!(!env.root.path().join("etc").exists());
    }
}

#[test]
fn fork_name_collisions_get_numeric_suffixes() {
    for backend in backends() {
        let env = setup(5412, backend);
        let a = fork1(&env, json!({"name": "dup"})).0;
        let b = fork1(&env, json!({"name": "dup"})).0;
        let c = fork1(&env, json!({"name": "dup"})).0;
        assert_eq!(
            [a.as_str(), b.as_str(), c.as_str()],
            ["dup", "dup-2", "dup-3"],
            "{backend}"
        );
        // An explicit name that is already taken by a suffixed session.
        let d = fork1(&env, json!({"name": "dup-2"})).0;
        assert_eq!(d, "dup-2-2");
        // Two raw names that sanitise to the same thing.
        let e1 = fork1(&env, json!({"name": "same name"})).0;
        let e2 = fork1(&env, json!({"name": "same/name"})).0;
        assert_eq!((e1.as_str(), e2.as_str()), ("same-name", "same-name-2"));
        // A 40-character name keeps its suffix within the limit.
        let long = "y".repeat(40);
        let l1 = fork1(&env, json!({"name": long})).0;
        let l2 = fork1(&env, json!({"name": long})).0;
        assert_eq!(l1, long);
        assert_eq!(l2, format!("{}-2", "y".repeat(38)));
        assert!(valid_session_name(&l2));
        // A leftover directory (not in state) also counts as taken.
        let leftover = env.project_dir().join("sessions/stale");
        fs::create_dir_all(&leftover).unwrap();
        let s = fork1(&env, json!({"name": "stale"})).0;
        assert_eq!(s, "stale-2", "{backend}");
        fs::remove_dir(&leftover).unwrap();
    }
}

// Fixed bug: with count > 1 and a base name of 40 characters, sanitize_name()
// truncates "<base>-1" back to "<base>" before the suffix is applied, so the
// first session has no number and the rest are renumbered by unique_name:
// names come out as "<40 chars>", "<38 chars>-2", "<38 chars>-3" instead of
// "<38 chars>-1", "-2", "-3". The base should be shortened before "-{i}" is
// appended (tools.rs fork()).
#[test]
fn fork_count_with_a_long_name_still_numbers_every_session() {
    let env = setup(5415, "clone");
    let long = "z".repeat(40);
    let v = fork(&env, json!({"name": long, "count": 3}));
    let names: Vec<String> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 3);
    for (i, n) in names.iter().enumerate() {
        assert!(valid_session_name(n), "{n}");
        assert!(
            n.ends_with(&format!("-{}", i + 1)),
            "session {} is named {n:?}: {names:?}",
            i + 1
        );
    }
}

// ---------------------------------------------------------------------------
// fork: from a session
// ---------------------------------------------------------------------------

#[test]
fn fork_from_session_copies_uncommitted_untracked_ignored_and_commits() {
    for backend in backends() {
        let env = setup(5418, backend);
        let (_, parent) = fork1(&env, json!({"name": "parent"}));
        wr(parent.join("committed.txt"), "committed\n");
        commit(&env, &parent, "parent commit");
        dirty_up(&parent, "p");
        wr(parent.join("staged.txt"), "staged\n");
        env.git(&parent, &["add", "staged.txt"]);
        let parent_head = head(&env, &parent);
        let parent_status = porcelain(&env, &parent);
        let parent_tree = tree(&parent);

        let v = fork(&env, json!({"from": "parent"}));
        assert!(
            v["refreshed"].is_null(),
            "fork from a session never refreshes: {v}"
        );
        let s = &v["sessions"][0];
        assert_eq!(s["name"], "parent-fork", "{backend}: default name");
        assert_eq!(s["parent"], "parent");
        assert!(s["from_checkpoint"].is_null());
        assert_eq!(s["generation"], 1);
        let child = PathBuf::from(s["path"].as_str().unwrap());

        assert_eq!(tree(&child), parent_tree, "{backend}: identical file tree");
        assert_eq!(head(&env, &child), parent_head, "{backend}: same HEAD");
        assert_eq!(
            porcelain(&env, &child),
            parent_status,
            "{backend}: same git status"
        );
        assert_eq!(
            env.git(&child, &["diff", "--cached", "--name-only"]).trim(),
            "staged.txt",
            "{backend}: index copied"
        );
        assert!(
            !child.join("README.md").exists(),
            "{backend}: deletion copied"
        );
        assert_eq!(read(child.join("deps/extra.txt")), "ignored p\n");
        assert_eq!(read(child.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(read(child.join(".env")), "SECRET=1\n");
        assert_eq!(branch(&env, &child), "shoal/parent-fork");
        assert_eq!(
            branch(&env, &parent),
            "shoal/parent",
            "{backend}: parent keeps its branch"
        );
        // The parent's status is untouched by the fork.
        assert_eq!(porcelain(&env, &parent), parent_status);
    }
}

#[test]
fn fork_from_session_is_isolated_in_both_directions() {
    for backend in backends() {
        let env = setup(5421, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        dirty_up(&parent, "before");
        let (_, child) = fork1(&env, json!({"from": "p", "name": "c"}));
        let base_head = head(&env, &parent);

        // Parent changes after the fork do not reach the child.
        wr(parent.join("src/app.txt"), "parent after\n");
        wr(parent.join("deps/extra.txt"), "parent deps after\n");
        wr(parent.join("parent-only.txt"), "p\n");
        fs::remove_file(parent.join("untracked.txt")).unwrap();
        commit(&env, &parent, "parent moves on");
        assert_eq!(
            read(child.join("src/app.txt")),
            "line one\nCHANGED before\nline three\n",
            "{backend}"
        );
        assert_eq!(read(child.join("deps/extra.txt")), "ignored before\n");
        assert!(!child.join("parent-only.txt").exists());
        assert!(child.join("untracked.txt").exists());
        assert_eq!(
            head(&env, &child),
            base_head,
            "{backend}: parent's commit not in child"
        );

        // Child changes do not reach the parent.
        wr(child.join("src/app.txt"), "child after\n");
        wr(child.join("child-only.txt"), "c\n");
        fs::remove_dir_all(child.join("deps")).unwrap();
        commit(&env, &child, "child moves on");
        assert_eq!(
            read(parent.join("src/app.txt")),
            "parent after\n",
            "{backend}"
        );
        assert!(!parent.join("child-only.txt").exists());
        assert_eq!(
            read(parent.join("deps/installed.txt")),
            "lib 1.0\n",
            "{backend}: parent deps intact"
        );
        assert_ne!(head(&env, &parent), head(&env, &child));
        assert_eq!(branch(&env, &parent), "shoal/p");
        // Neither commit leaked into the source repository.
        let src_log = env.git(&env.repo(), &["log", "--all", "--format=%s"]);
        assert!(!src_log.contains("moves on"), "{src_log}");
    }
}

#[test]
fn fork_and_rewind_keep_a_replaced_directory_replaced() {
    // rm -rf of a tracked directory followed by mkdir makes an opaque
    // directory in an overlay upper layer; copies of that layer must keep it
    // opaque or the old files reappear.
    for backend in backends() {
        let env = setup(5424, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        fs::remove_dir_all(parent.join("src")).unwrap();
        wr(parent.join("src/new.txt"), "new\n");
        fs::remove_dir_all(parent.join("deps")).unwrap();
        wr(parent.join("deps/other.txt"), "other\n");
        let want_src = names_in(&parent.join("src"));
        assert_eq!(want_src, BTreeSet::from(["new.txt".to_string()]));

        let (_, child) = fork1(&env, json!({"from": "p", "name": "c"}));
        assert_eq!(
            names_in(&child.join("src")),
            want_src,
            "{backend}: fork resurrected old files"
        );
        assert_eq!(
            names_in(&child.join("deps")),
            BTreeSet::from(["other.txt".to_string()]),
            "{backend}"
        );

        checkpoint(&env, "p", Some("replaced"));
        let (_, at) = fork1(&env, json!({"from": "p", "at": "replaced", "name": "at"}));
        assert_eq!(
            names_in(&at.join("src")),
            want_src,
            "{backend}: fork at resurrected old files"
        );

        wr(parent.join("src/later.txt"), "later\n");
        rewind(&env, "p", "replaced");
        assert_eq!(
            names_in(&parent.join("src")),
            want_src,
            "{backend}: rewind resurrected old files"
        );
        assert_eq!(
            names_in(&parent.join("deps")),
            BTreeSet::from(["other.txt".to_string()])
        );
    }
}

#[test]
fn fork_from_a_session_that_is_not_mounted_still_copies_its_work() {
    for backend in backends() {
        let env = setup(5427, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        dirty_up(&parent, "x");
        let want = tree(&parent);
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success(), "{}", stderr(&out));
        let (_, child) = fork1(&env, json!({"from": "p", "name": "c"}));
        assert_eq!(tree(&child), want, "{backend}");
        assert_eq!(tree(&parent), want, "{backend}: parent is mounted again");
        // checkpoint and rewind also work on an unmounted session.
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success(), "{}", stderr(&out));
        checkpoint(&env, "p", Some("after-down"));
        wr(parent.join("src/app.txt"), "changed\n");
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success(), "{}", stderr(&out));
        rewind(&env, "p", "after-down");
        assert_eq!(tree(&parent), want, "{backend}");
    }
}

#[test]
fn fork_from_nonexistent_session_is_rejected_and_leaves_nothing() {
    for backend in backends() {
        let env = setup(5430, backend);
        fork1(&env, json!({"name": "real"}));
        for from in ["ghost", "Real", "real ", "../real"] {
            let e = err(env.api("fork", json!({"from": from})), from);
            assert!(e.contains("no session named"), "{backend} {from}: {e}");
            assert!(e.contains(from), "{e}");
        }
        let e = err(
            env.api("fork", json!({"from": "ghost", "at": "cp1"})),
            "ghost at",
        );
        assert!(e.contains("no session named"), "{e}");
        assert_eq!(state_sessions(&env), BTreeSet::from(["real".to_string()]));
        assert_eq!(
            names_in(&env.project_dir().join("sessions")),
            BTreeSet::from(["real".to_string()])
        );
        let forks: Vec<String> = names_in(&env.project_dir().join("layers"))
            .into_iter()
            .filter(|n| n.starts_with("fork-"))
            .collect();
        assert!(forks.is_empty(), "{forks:?}");
    }
}

// ---------------------------------------------------------------------------
// fork: refresh
// ---------------------------------------------------------------------------

#[test]
fn fork_from_main_after_source_moved_builds_a_new_generation() {
    for backend in backends() {
        let env = setup(5433, backend);
        let (_, old) = fork1(&env, json!({"name": "old"}));
        let old_head = head(&env, &old);

        // The source moves, changing a setup input.
        let repo = env.repo();
        wr(repo.join("src/app.txt"), "v2\n");
        wr(repo.join("deps.lock"), "lib 2.0\n");
        commit(&env, &repo, "v2");
        let new_head = head(&env, &repo);

        let v = fork(&env, json!({"name": "new"}));
        let r = &v["refreshed"];
        assert!(!r.is_null(), "{backend}: {v}");
        assert_eq!(r["up_to_date"], false);
        assert_eq!(r["generation"], 2);
        assert_eq!(r["from"], old_head);
        assert_eq!(r["to"], new_head);
        assert_eq!(r["setup_ran"], true, "{backend}: deps.lock changed");
        assert_eq!(r["changed_files"], 2);
        let s = &v["sessions"][0];
        assert_eq!(s["generation"], 2);
        let new = PathBuf::from(s["path"].as_str().unwrap());
        assert_eq!(head(&env, &new), new_head);
        assert_eq!(read(new.join("src/app.txt")), "v2\n");
        assert_eq!(
            read(new.join("deps/installed.txt")),
            "lib 2.0\n",
            "{backend}: setup re-ran"
        );
        assert_eq!(read(new.join(".env")), "SECRET=1\n");
        assert_eq!(porcelain(&env, &new), "", "{backend}");

        // The old session keeps its commit, files and generation.
        assert_eq!(head(&env, &old), old_head);
        assert_eq!(
            read(old.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
        assert_eq!(read(old.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(env.state()["sessions"]["old"]["generation"], 1);

        // Up to date now: no refresh report.
        let v = fork(&env, json!({}));
        assert!(v["refreshed"].is_null(), "{v}");
        assert_eq!(v["sessions"][0]["generation"], 2);

        // Forking the old session stays on the old commit and does not refresh.
        let v = fork(&env, json!({"from": "old"}));
        assert!(v["refreshed"].is_null());
        assert_eq!(v["sessions"][0]["generation"], 1);
        let p = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
        assert_eq!(head(&env, &p), old_head);

        // A move that does not touch a setup input skips setup but keeps deps.
        wr(repo.join("README.md"), "v3\n");
        commit(&env, &repo, "v3");
        let v = fork(&env, json!({"name": "third"}));
        assert_eq!(v["refreshed"]["generation"], 3, "{v}");
        assert_eq!(v["refreshed"]["setup_ran"], false);
        let p = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
        assert_eq!(read(p.join("README.md")), "v3\n");
        assert_eq!(
            read(p.join("deps/installed.txt")),
            "lib 2.0\n",
            "{backend}: deps carried over"
        );
        // The session on generation 2 is unchanged by generation 3.
        assert_eq!(read(new.join("README.md")), "demo\n");
    }
}

#[test]
fn fork_count_after_source_moved_refreshes_once_and_all_share_it() {
    for backend in backends() {
        let env = setup(5436, backend);
        fork1(&env, json!({"name": "a"}));
        let repo = env.repo();
        wr(repo.join("README.md"), "moved\n");
        commit(&env, &repo, "moved");
        let new_head = head(&env, &repo);
        let v = fork(&env, json!({"count": 3, "name": "n"}));
        assert_eq!(v["refreshed"]["generation"], 2, "{backend}: {v}");
        for s in v["sessions"].as_array().unwrap() {
            assert_eq!(s["generation"], 2);
            assert_eq!(head(&env, Path::new(s["path"].as_str().unwrap())), new_head);
        }
        let gens = env.state()["generations"].as_array().unwrap().len();
        assert_eq!(gens, 2, "{backend}: exactly one new generation");
    }
}

// ---------------------------------------------------------------------------
// fork: at a checkpoint
// ---------------------------------------------------------------------------

#[test]
fn fork_at_checkpoint_starts_from_that_exact_state_after_parent_moved_on() {
    for backend in backends() {
        let env = setup(5439, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        wr(parent.join("c1.txt"), "c1\n");
        commit(&env, &parent, "c1");
        dirty_up(&parent, "at-cp");
        let cp = checkpoint(&env, "p", Some("cp-a"));
        let cp_head = head(&env, &parent);
        assert_eq!(cp["head"], cp_head);
        let cp_tree = tree(&parent);
        let cp_status = porcelain(&env, &parent);

        // The parent moves on: commits, edits, deletes, new ignored files.
        wr(parent.join("src/app.txt"), "much later\n");
        wr(parent.join("later.txt"), "later\n");
        wr(parent.join("deps/later.txt"), "later deps\n");
        fs::remove_file(parent.join("untracked.txt")).unwrap();
        commit(&env, &parent, "later");
        wr(parent.join("uncommitted-later.txt"), "x\n");

        let v = fork(&env, json!({"from": "p", "at": "cp-a", "name": "back"}));
        let s = &v["sessions"][0];
        assert_eq!(s["parent"], "p");
        assert_eq!(s["from_checkpoint"], "cp-a");
        let child = PathBuf::from(s["path"].as_str().unwrap());
        assert_eq!(
            tree(&child),
            cp_tree,
            "{backend}: files as at the checkpoint"
        );
        assert_eq!(
            head(&env, &child),
            cp_head,
            "{backend}: HEAD as at the checkpoint"
        );
        assert_eq!(porcelain(&env, &child), cp_status);
        assert_eq!(branch(&env, &child), "shoal/back");
        // The parent is not rewound by forking from its past.
        assert_eq!(read(parent.join("src/app.txt")), "much later\n");
        assert!(parent.join("uncommitted-later.txt").exists());
        // Default name for fork at.
        let (n, _) = fork1(&env, json!({"from": "p", "at": "cp-a"}));
        assert_eq!(n, "p-fork");
    }
}

#[test]
fn fork_at_checkpoint_survives_parent_discard_gc_and_remount() {
    for backend in backends() {
        let env = setup(5442, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        dirty_up(&parent, "keep");
        checkpoint(&env, "p", Some("cp1"));
        let want = tree(&parent);
        let (_, child) = fork1(&env, json!({"from": "p", "at": "cp1", "name": "c"}));
        // A grandchild from a checkpoint of the child: a chain of layers.
        wr(child.join("grand.txt"), "g\n");
        checkpoint(&env, "c", Some("g1"));
        let (_, grand) = fork1(&env, json!({"from": "c", "at": "g1", "name": "g"}));
        let want_grand = tree(&grand);

        ok(
            env.api("discard", json!({"session": "p", "keep_work": false})),
            "discard p",
        );
        assert!(!state_sessions(&env).contains("p"));
        let e = err(
            env.api("fork", json!({"from": "p", "at": "cp1"})),
            "fork at after discard",
        );
        assert!(e.contains("no session named"), "{e}");
        assert_eq!(
            tree(&child),
            {
                let mut w = want.clone();
                w.insert("grand.txt".into(), "g\n".into());
                w
            },
            "{backend}"
        );

        ok(
            env.api("discard", json!({"session": "c", "keep_work": false})),
            "discard c",
        );
        let out = shoal_cli(&env, &["gc"]);
        assert!(out.status.success(), "gc: {}", stderr(&out));
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success(), "{}", stderr(&out));
        let out = shoal_cli(&env, &["up"]);
        assert!(out.status.success(), "up: {}{}", stdout(&out), stderr(&out));
        assert_eq!(
            tree(&grand),
            want_grand,
            "{backend}: grandchild intact after discard+gc+remount"
        );
        assert_eq!(read(grand.join("deps/extra.txt")), "ignored keep\n");
        // And it is still a working session.
        let st = ok(env.api("status", json!({"session": "g"})), "status");
        assert_eq!(st["sessions"][0]["name"], "g");
        wr(grand.join("src/app.txt"), "edited\n");
        checkpoint(&env, "g", None);
        wr(grand.join("src/app.txt"), "edited again\n");
        rewind(&env, "g", "latest");
        assert_eq!(read(grand.join("src/app.txt")), "edited\n");
    }
}

#[test]
fn fork_at_checkpoint_survives_automatic_pruning() {
    for backend in backends() {
        let env = setup(5445, backend);
        let (_, parent) = fork1(&env, json!({"name": "p"}));
        let stop = |i: usize| {
            wr(parent.join(format!("turn{i}.txt")), &format!("{i}\n"));
            let r = env.hook(
                "stop",
                &json!({"cwd": parent, "hook_event_name": "Stop"}),
                &parent,
                None,
            );
            assert_eq!(r.code, 0, "{}", r.stderr);
        };
        stop(1);
        if !state_checkpoints(&env, "p").contains(&"turn-1".to_string()) {
            eprintln!(
                "{backend}: checkpoints are not cheap here, the Stop hook takes none; skipping"
            );
            continue;
        }
        let (_, child) = fork1(&env, json!({"from": "p", "at": "turn-1", "name": "c"}));
        let want = tree(&child);
        for i in 2..=22 {
            stop(i);
        }
        let labels = state_checkpoints(&env, "p");
        assert!(
            !labels.contains(&"turn-1".to_string()),
            "{backend}: turn-1 pruned: {labels:?}"
        );
        assert_eq!(labels.len(), 20, "{labels:?}");
        assert!(!env.project_dir().join("checkpoints/p/turn-1").exists());
        assert_eq!(tree(&child), want, "{backend}");
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success());
        ok(env.api("status", json!({})), "status remounts");
        assert_eq!(tree(&child), want, "{backend}: after remount");
        assert!(child.join("turn1.txt").exists() && !child.join("turn2.txt").exists());
        let e = err(
            env.api("fork", json!({"from": "p", "at": "turn-1"})),
            "fork at pruned",
        );
        assert!(e.contains("no checkpoint named turn-1"), "{e}");
    }
}

#[test]
fn fork_at_without_a_session_is_rejected() {
    for backend in backends() {
        let env = setup(5448, backend);
        let (_, _) = fork1(&env, json!({"name": "p"}));
        checkpoint(&env, "p", None);
        for args in [
            json!({"at": "cp1"}),
            json!({"from": "main", "at": "cp1"}),
            json!({"from": "template", "at": "cp1"}),
            json!({"at": "cp1", "count": 2}),
        ] {
            let e = err(env.api("fork", args.clone()), &args.to_string());
            assert!(
                e.contains("at needs from to name a session"),
                "{backend} {args}: {e}"
            );
        }
        assert_eq!(state_sessions(&env), BTreeSet::from(["p".to_string()]));
    }
}

#[test]
fn fork_at_nonexistent_checkpoint_is_rejected_and_leaves_nothing() {
    for backend in backends() {
        let env = setup(5451, backend);
        fork1(&env, json!({"name": "p"}));
        fork1(&env, json!({"name": "q"}));
        checkpoint(&env, "p", Some("only-p"));
        let before_layers = names_in(&env.project_dir().join("layers"));
        for (from, at) in [
            ("p", "nope"),
            ("p", "cp1"),
            ("q", "only-p"),
            ("p", ""),
            ("p", "../only-p"),
        ] {
            let e = err(
                env.api("fork", json!({"from": from, "at": at, "name": "x"})),
                at,
            );
            assert!(
                e.contains("no checkpoint named"),
                "{backend} {from}@{at}: {e}"
            );
        }
        assert_eq!(
            state_sessions(&env),
            BTreeSet::from(["p".to_string(), "q".to_string()])
        );
        assert!(!env.project_dir().join("sessions/x").exists());
        assert!(!env.project_dir().join("volumes/x").exists());
        assert_eq!(
            names_in(&env.project_dir().join("layers")),
            before_layers,
            "{backend}"
        );
    }
}

// Fixed bug (overlay): fork layers are named "layers/fork-{parent}-{name}-{unix
// seconds}". Two forks in the same second whose "{parent}-{name}" strings
// are equal collide: parent "a-b" + child "c" and parent "a" + child "b-c"
// both want "layers/fork-a-b-c-<t>", and cp refuses because the directory
// exists. The second fork fails with "layers/fork-a-b-c-<t> already exists".
#[test]
fn fork_layer_names_never_collide_across_parents() {
    for backend in backends() {
        let env = setup(5454, backend);
        fork1(&env, json!({"name": "a-b"}));
        fork1(&env, json!({"name": "a"}));
        // Several rounds so both forks land in the same second at least once.
        for round in 0..3 {
            let c1 = format!("c{round}");
            let c2 = format!("b-c{round}");
            let r1 = env.api("fork", json!({"from": "a-b", "name": c1}));
            let r2 = env.api("fork", json!({"from": "a", "name": c2}));
            assert!(r1.is_ok(), "{backend}: {r1:?}");
            assert!(r2.is_ok(), "{backend}: second fork failed: {r2:?}");
        }
    }
}

// Fixed bug (overlay): discard does not delete a session's fork layer (only gc
// does), and fork layer names carry only one-second resolution. Discarding
// a child and forking it again under the same name from the same parent
// within one second fails with "layers/fork-a-again-<t> already exists".
// This is exactly what a best-of-N retry loop does.
#[test]
fn refork_after_discard_in_the_same_second_works() {
    for backend in backends() {
        let env = setup(5401, backend);
        fork1(&env, json!({"name": "a"}));
        for round in 0..3 {
            let r = env.api("fork", json!({"from": "a", "name": "again"}));
            assert!(r.is_ok(), "{backend} round {round}: {r:?}");
            ok(
                env.api("discard", json!({"session": "again", "keep_work": false})),
                "discard",
            );
            let r = env.api("fork", json!({"from": "a", "name": "again"}));
            assert!(r.is_ok(), "{backend} round {round}: re-fork failed: {r:?}");
            ok(
                env.api("discard", json!({"session": "again", "keep_work": false})),
                "discard",
            );
        }
        // Same through fork-at.
        checkpoint(&env, "a", Some("cp"));
        for round in 0..3 {
            let r = env.api("fork", json!({"from": "a", "at": "cp", "name": "at"}));
            assert!(r.is_ok(), "{backend} round {round}: {r:?}");
            ok(
                env.api("discard", json!({"session": "at", "keep_work": false})),
                "discard",
            );
        }
    }
}

// Fixed bug: a session may be named "main" (or "template"), but fork treats
// from="main"/"template" as the template, so such a session can never be
// forked: fork {"from": "main"} silently starts from the repository instead
// of the session's uncommitted work. Either the names should be refused or
// sanitised away at creation, or the session should win.
#[test]
fn a_session_named_main_can_still_be_forked() {
    let env = setup(5457, "clone");
    let (name, path) = fork1(&env, json!({"name": "main"}));
    wr(path.join("work.txt"), "important\n");
    let v = fork(&env, json!({"from": name, "name": "child"}));
    let child = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
    assert!(
        name != "main" || child.join("work.txt").exists(),
        "fork from={name:?} ignored the session of that name: {v}"
    );
}

// ---------------------------------------------------------------------------
// checkpoint
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_default_labels_count_up_per_session() {
    for backend in backends() {
        let env = setup(5460, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        fork1(&env, json!({"name": "b"}));
        for i in 1..=3 {
            wr(a.join("n.txt"), &i.to_string());
            let c = checkpoint(&env, "a", None);
            assert_eq!(c["label"], format!("cp{i}"), "{backend}: {c}");
            assert_eq!(c["session"], "a");
            assert_eq!(c["auto"], false);
            assert!(c["skipped"].is_null());
            assert_eq!(c["head"], head(&env, &a));
            assert!(env
                .project_dir()
                .join(format!("checkpoints/a/cp{i}"))
                .is_dir());
        }
        // Numbering is per session.
        assert_eq!(checkpoint(&env, "b", None)["label"], "cp1");
        // An explicit cpN moves the counter past it.
        checkpoint(&env, "a", Some("cp7"));
        assert_eq!(checkpoint(&env, "a", None)["label"], "cp8");
        // A checkpoint with no changes at all is still taken.
        assert_eq!(checkpoint(&env, "a", None)["label"], "cp9");
        // Explicit null label behaves like no label.
        let c = ok(
            env.api("checkpoint", json!({"session": "a", "label": null})),
            "null label",
        );
        assert_eq!(c["label"], "cp10");
        assert_eq!(
            state_checkpoints(&env, "a"),
            ["cp1", "cp2", "cp3", "cp7", "cp8", "cp9", "cp10"]
        );
    }
}

#[test]
fn checkpoint_duplicate_label_is_rejected_and_keeps_the_original() {
    for backend in backends() {
        let env = setup(5463, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        fork1(&env, json!({"name": "b"}));
        wr(a.join("v.txt"), "first\n");
        checkpoint(&env, "a", Some("before-migration"));
        wr(a.join("v.txt"), "second\n");
        let e = err(
            env.api(
                "checkpoint",
                json!({"session": "a", "label": "before-migration"}),
            ),
            "duplicate",
        );
        assert!(
            e.contains("already has a checkpoint named before-migration"),
            "{backend}: {e}"
        );
        // Default labels never collide with an explicit "cp1" either.
        checkpoint(&env, "a", Some("cp1"));
        assert_eq!(checkpoint(&env, "a", None)["label"], "cp2");
        // The same label in another session is fine.
        checkpoint(&env, "b", Some("before-migration"));
        // The original was not overwritten.
        rewind(&env, "a", "before-migration");
        assert_eq!(read(a.join("v.txt")), "first\n", "{backend}");
        assert_eq!(
            state_checkpoints(&env, "a")
                .iter()
                .filter(|l| *l == "before-migration")
                .count(),
            1
        );
    }
}

#[test]
fn checkpoint_invalid_labels_are_rejected_without_side_effects() {
    for backend in backends() {
        let env = setup(5466, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("x.txt"), "x\n");
        let bad = [
            String::new(),
            "a/b".into(),
            "../escape".into(),
            "..".into(),
            ".".into(),
            ".hidden".into(),
            "-dash".into(),
            "has space".into(),
            "naïve".into(),
            "a\nb".into(),
            "a:b".into(),
            "a,b".into(),
            "x".repeat(41),
        ];
        for label in &bad {
            let e = err(
                env.api("checkpoint", json!({"session": "a", "label": label})),
                &format!("label {label:?}"),
            );
            assert!(
                e.contains("invalid checkpoint name"),
                "{backend} {label:?}: {e}"
            );
        }
        for label in [json!(5), json!(true), json!(["a"])] {
            let e = err(
                env.api("checkpoint", json!({"session": "a", "label": label})),
                "typed",
            );
            assert!(e.contains("label must be a string"), "{e}");
        }
        assert!(state_checkpoints(&env, "a").is_empty());
        assert!(names_in(&env.project_dir().join("checkpoints/a")).is_empty());
        assert!(!env.project_dir().join("checkpoints/escape").exists());
        assert!(!env.project_dir().join("escape").exists());
        // Boundaries that are valid.
        let forty = "y".repeat(40);
        assert_eq!(checkpoint(&env, "a", Some(&forty))["label"], forty.as_str());
        assert_eq!(
            checkpoint(&env, "a", Some("v1.2_rc-3"))["label"],
            "v1.2_rc-3"
        );
    }
}

// Fixed bug: "latest" is accepted as a checkpoint label, but rewind treats the
// label "latest" as "the newest checkpoint", so once a newer checkpoint
// exists the one named "latest" can never be rewound to (and forking at it
// still works, so fork and rewind disagree on what "latest" means). The label
// should be reserved.
#[test]
fn checkpoint_label_latest_is_reserved() {
    let env = setup(5469, "clone");
    let (_, a) = fork1(&env, json!({"name": "a"}));
    wr(a.join("v.txt"), "named latest\n");
    let r = env.api("checkpoint", json!({"session": "a", "label": "latest"}));
    if r.is_err() {
        return; // reserved: fine
    }
    std::thread::sleep(Duration::from_millis(1100));
    wr(a.join("v.txt"), "newer\n");
    checkpoint(&env, "a", Some("newer"));
    wr(a.join("v.txt"), "now\n");
    let v = rewind(&env, "a", "latest");
    assert_eq!(
        v["label"], "latest",
        "a checkpoint named \"latest\" was accepted but rewind to it went to {}",
        v["label"]
    );
}

#[test]
fn checkpoint_captures_uncommitted_ignored_files_and_head() {
    for backend in backends() {
        let env = setup(5472, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("c.txt"), "c\n");
        commit(&env, &a, "one");
        dirty_up(&a, "cp");
        wr(a.join("staged.txt"), "staged\n");
        env.git(&a, &["add", "staged.txt"]);
        wr(a.join("deps/nested/deep/file.bin"), "binary-ish\0data");
        let c = checkpoint(&env, "a", Some("full"));
        assert_eq!(c["head"], head(&env, &a), "{backend}");
        let want = tree(&a);
        let want_status = porcelain(&env, &a);

        // Fork at the checkpoint shows exactly what it captured.
        let (_, view) = fork1(&env, json!({"from": "a", "at": "full", "name": "view"}));
        assert_eq!(tree(&view), want, "{backend}");
        assert_eq!(porcelain(&env, &view), want_status);
        assert_eq!(head(&env, &view), c["head"].as_str().unwrap());
        assert_eq!(
            fs::read(view.join("deps/nested/deep/file.bin")).unwrap(),
            b"binary-ish\0data"
        );
        // Taking a checkpoint does not change the session itself.
        assert_eq!(tree(&a), want);
        assert_eq!(porcelain(&env, &a), want_status);
        // status reports it.
        let st = ok(env.api("status", json!({"session": "a"})), "status");
        let s = st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "a")
            .unwrap()
            .clone();
        assert_eq!(s["checkpoints"][0]["label"], "full", "{st}");
        assert_eq!(s["checkpoints"][0]["auto"], false);
    }
}

#[test]
fn checkpoint_and_rewind_resolve_the_session_from_a_path() {
    for backend in backends() {
        let env = setup(5475, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        fork1(&env, json!({"name": "b"}));
        wr(a.join("v.txt"), "1\n");
        let deep = a.join("src");
        let c = ok(
            env.api("checkpoint", json!({"path": deep, "label": "by-path"})),
            "checkpoint by path",
        );
        assert_eq!(c["session"], "a", "{backend}");
        wr(a.join("v.txt"), "2\n");
        let r = ok(
            env.api("rewind", json!({"path": a, "label": "by-path"})),
            "rewind by path",
        );
        assert_eq!(r["session"], "a");
        assert_eq!(read(a.join("v.txt")), "1\n");
        // A path outside any session.
        let e = err(
            env.api("checkpoint", json!({"path": env.repo()})),
            "repo path",
        );
        assert!(e.contains("not inside a shoal session"), "{backend}: {e}");
        // A session that does not exist lists the real ones.
        let e = err(env.api("checkpoint", json!({"session": "zzz"})), "zzz");
        assert!(
            e.contains("no session named \"zzz\"") && e.contains("a, b"),
            "{e}"
        );
        // Neither session nor path.
        let e = err(env.api("checkpoint", json!({})), "none");
        assert!(e.contains("pass session"), "{e}");
        assert_eq!(state_checkpoints(&env, "b"), Vec::<String>::new());
    }
}

#[test]
fn concurrent_checkpoints_and_forks_get_distinct_names() {
    for backend in backends() {
        let env = setup(5478, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("v.txt"), "v\n");
        let results: Vec<Result<Value, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| env.api("checkpoint", json!({"session": "a"}))))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut labels = BTreeSet::new();
        for r in results {
            let v = ok(r, "concurrent checkpoint");
            labels.insert(v["label"].as_str().unwrap().to_string());
        }
        let want: BTreeSet<String> = (1..=4).map(|i| format!("cp{i}")).collect();
        assert_eq!(labels, want, "{backend}");

        let results: Vec<Result<Value, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| env.api("fork", json!({"from": "a", "name": "race"}))))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut names = BTreeSet::new();
        for r in results {
            let v = ok(r, "concurrent fork");
            let s = &v["sessions"][0];
            let p = PathBuf::from(s["path"].as_str().unwrap());
            assert_eq!(read(p.join("v.txt")), "v\n");
            assert_eq!(branch(&env, &p), s["branch"].as_str().unwrap());
            names.insert(s["name"].as_str().unwrap().to_string());
        }
        assert_eq!(names.len(), 4, "{backend}: {names:?}");
        assert_eq!(state_sessions(&env).len(), 5);
    }
}

// ---------------------------------------------------------------------------
// rewind
// ---------------------------------------------------------------------------

#[test]
fn rewind_restores_files_head_index_untracked_and_deps() {
    for backend in backends() {
        let env = setup(5481, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("c.txt"), "c\n");
        commit(&env, &a, "kept");
        dirty_up(&a, "cp");
        wr(a.join("staged.txt"), "staged\n");
        env.git(&a, &["add", "staged.txt"]);
        let c = checkpoint(&env, "a", Some("good"));
        let want = tree(&a);
        let want_status = porcelain(&env, &a);
        let want_head = head(&env, &a);

        // Wreck everything.
        wr(a.join("src/app.txt"), "broken\n");
        commit(&env, &a, "bad commit");
        wr(a.join("README.md"), "resurrected\n");
        fs::remove_file(a.join("untracked.txt")).unwrap();
        fs::remove_dir_all(a.join("deps")).unwrap();
        wr(a.join("new-junk.txt"), "junk\n");
        wr(a.join("deps2/x"), "x\n");
        env.git(&a, &["reset", "-q"]);
        let bad_head = head(&env, &a);
        assert_ne!(bad_head, want_head);

        let r = rewind(&env, "a", "good");
        assert_eq!(r["session"], "a");
        assert_eq!(r["label"], "good");
        assert_eq!(r["head"], want_head);
        assert_eq!(r["head"], c["head"]);
        assert_eq!(r["restarted_services"], json!([]));
        assert_eq!(tree(&a), want, "{backend}: files");
        assert_eq!(porcelain(&env, &a), want_status, "{backend}: git status");
        assert_eq!(head(&env, &a), want_head, "{backend}: HEAD");
        assert_eq!(branch(&env, &a), "shoal/a");
        assert_eq!(read(a.join("deps/installed.txt")), "lib 1.0\n");
        assert!(!a.join("new-junk.txt").exists() && !a.join("deps2").exists());
        // The branch ref went back too: the bad commit is no longer on it.
        let log = env.git(&a, &["log", "--format=%s", "shoal/a"]);
        assert!(!log.contains("bad commit") && log.contains("kept"), "{log}");
        // The checkpoint survives being used.
        assert!(state_checkpoints(&env, "a").contains(&"good".to_string()));
        wr(a.join("src/app.txt"), "again\n");
        rewind(&env, "a", "good");
        assert_eq!(tree(&a), want, "{backend}: second rewind");
        // No trash left behind.
        let leftovers: Vec<String> = names_in(&env.project_dir())
            .into_iter()
            .filter(|n| n.starts_with("trash-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn rewind_latest_and_forward_and_back_between_checkpoints() {
    for backend in backends() {
        let env = setup(5484, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("v.txt"), "one\n");
        checkpoint(&env, "a", None);
        std::thread::sleep(Duration::from_millis(1050));
        wr(a.join("v.txt"), "two\n");
        wr(a.join("two-only.txt"), "2\n");
        checkpoint(&env, "a", None);
        wr(a.join("v.txt"), "three\n");

        let r = rewind(&env, "a", "latest");
        assert_eq!(r["label"], "cp2", "{backend}");
        assert_eq!(read(a.join("v.txt")), "two\n");
        let r = rewind(&env, "a", "cp1");
        assert_eq!(r["label"], "cp1");
        assert_eq!(read(a.join("v.txt")), "one\n");
        assert!(!a.join("two-only.txt").exists());
        // Forward again: rewinding does not delete later checkpoints.
        rewind(&env, "a", "cp2");
        assert_eq!(read(a.join("v.txt")), "two\n");
        assert!(a.join("two-only.txt").exists());
        // "latest" means newest created, not most recently restored.
        rewind(&env, "a", "cp1");
        assert_eq!(rewind(&env, "a", "latest")["label"], "cp2");
    }
}

// Fixed bug: rewind "latest" picks the checkpoint with the greatest
// (created_at, label), and created_at has one-second resolution. Two
// checkpoints taken in the same second are ordered by label text instead of
// creation order, so after "zeta" then "alpha" (or cp9 then cp10) "latest"
// restores the OLDER one and silently discards the newer work.
#[test]
fn rewind_latest_picks_the_newest_even_within_one_second() {
    for backend in backends() {
        let env = setup(5487, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        let mut tried = 0;
        loop {
            tried += 1;
            let (older, newer) = (format!("zeta{tried}"), format!("alpha{tried}"));
            wr(a.join("v.txt"), "older\n");
            checkpoint(&env, "a", Some(&older));
            wr(a.join("v.txt"), "newer\n");
            checkpoint(&env, "a", Some(&newer));
            let st = env.state();
            let at = |l: &str| {
                st["checkpoints"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["label"] == l)
                    .unwrap()["created_at"]
                    .as_u64()
                    .unwrap()
            };
            if at(&older) != at(&newer) && tried < 5 {
                continue; // straddled a second boundary; try again
            }
            wr(a.join("v.txt"), "current\n");
            let r = rewind(&env, "a", "latest");
            assert_eq!(
                r["label"],
                newer.as_str(),
                "{backend}: latest restored an older checkpoint"
            );
            assert_eq!(read(a.join("v.txt")), "newer\n");
            break;
        }
    }
}

#[test]
fn rewind_unknown_label_lists_the_known_labels_and_changes_nothing() {
    for backend in backends() {
        let env = setup(5490, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        fork1(&env, json!({"name": "b"}));
        checkpoint(&env, "a", None);
        checkpoint(&env, "a", Some("before-x"));
        checkpoint(&env, "b", Some("b-only"));
        wr(a.join("work.txt"), "unsaved\n");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "nope"})),
            "nope",
        );
        assert!(e.contains("no checkpoint named nope"), "{backend}: {e}");
        assert!(e.contains("cp1") && e.contains("before-x"), "{e}");
        assert!(!e.contains("b-only"), "another session's labels: {e}");
        // A label of another session is not usable here.
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "b-only"})),
            "b-only",
        );
        assert!(e.contains("no checkpoint named b-only"), "{e}");
        // Path-like labels do not escape.
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "../b/b-only"})),
            "path",
        );
        assert!(e.contains("no checkpoint named"), "{e}");
        assert_eq!(
            read(a.join("work.txt")),
            "unsaved\n",
            "{backend}: nothing was rewound"
        );
        // Missing or mistyped label.
        let e = err(env.api("rewind", json!({"session": "a"})), "no label");
        assert!(e.contains("rewind needs label"), "{e}");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": null})),
            "null label",
        );
        assert!(e.contains("rewind needs label"), "{e}");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": 1})),
            "typed",
        );
        assert!(e.contains("label must be a string"), "{e}");
        let e = err(
            env.api("rewind", json!({"session": "ghost", "label": "cp1"})),
            "ghost",
        );
        assert!(e.contains("no session named \"ghost\""), "{e}");
    }
}

#[test]
fn rewind_a_session_without_checkpoints_fails_cleanly() {
    for backend in backends() {
        let env = setup(5493, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        wr(a.join("work.txt"), "unsaved\n");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "latest"})),
            "latest",
        );
        assert!(e.contains("a has no checkpoints"), "{backend}: {e}");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "cp1"})),
            "cp1",
        );
        assert!(e.contains("it has: none"), "{backend}: {e}");
        assert_eq!(read(a.join("work.txt")), "unsaved\n");
        // Checkpoints of a discarded session with the same name do not linger.
        checkpoint(&env, "a", Some("old"));
        ok(
            env.api("discard", json!({"session": "a", "keep_work": false})),
            "discard",
        );
        let (n, _) = fork1(&env, json!({"name": "a"}));
        assert_eq!(n, "a");
        let e = err(
            env.api("rewind", json!({"session": "a", "label": "old"})),
            "stale",
        );
        assert!(e.contains("it has: none"), "{backend}: {e}");
        assert!(!env.project_dir().join("checkpoints/a/old").exists());
        assert_eq!(checkpoint(&env, "a", None)["label"], "cp1");
    }
}

#[test]
fn rewind_does_not_touch_children_or_siblings() {
    for backend in backends() {
        let env = setup(5496, backend);
        let (_, p) = fork1(&env, json!({"name": "p"}));
        let (_, sib) = fork1(&env, json!({"name": "sib"}));
        wr(p.join("v.txt"), "base\n");
        checkpoint(&env, "p", Some("base"));
        wr(p.join("v.txt"), "evolved\n");
        let (_, child) = fork1(&env, json!({"from": "p", "name": "child"}));
        let (_, at) = fork1(&env, json!({"from": "p", "at": "base", "name": "at"}));
        wr(child.join("mine.txt"), "child\n");
        wr(at.join("mine.txt"), "at\n");
        wr(sib.join("mine.txt"), "sib\n");

        rewind(&env, "p", "base");
        assert_eq!(read(p.join("v.txt")), "base\n");
        assert_eq!(read(child.join("v.txt")), "evolved\n", "{backend}");
        assert_eq!(read(child.join("mine.txt")), "child\n");
        assert_eq!(read(at.join("v.txt")), "base\n");
        assert_eq!(read(at.join("mine.txt")), "at\n");
        assert_eq!(read(sib.join("mine.txt")), "sib\n");
        assert!(!p.join("mine.txt").exists());
        // The children still mount after a full remount.
        let out = shoal_cli(&env, &["down"]);
        assert!(out.status.success());
        let out = shoal_cli(&env, &["up"]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(read(child.join("v.txt")), "evolved\n", "{backend}");
        assert_eq!(read(p.join("v.txt")), "base\n");
    }
}

#[test]
fn rewind_restarts_running_services_and_leaves_stopped_ones() {
    for backend in backends() {
        let env = setup(5499, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        let seen = env.root.path().join(format!("seen-{backend}.txt"));
        wr(a.join("v.txt"), "checkpointed\n");
        checkpoint(&env, "a", Some("cp"));
        wr(a.join("v.txt"), "later\n");

        let script = format!("cat v.txt > '{}'; exec sleep 300", seen.display());
        let out = shoal_cli(&env, &["serve", "a", "--", "sh", "-c", &script]);
        assert!(
            out.status.success(),
            "serve: {}{}",
            stdout(&out),
            stderr(&out)
        );
        let pid_of = |env: &Env| env.state()["sessions"]["a"]["services"][0]["pid"].as_i64();
        let old = pid_of(&env).expect("service pid") as i32;
        assert!(alive(old));
        assert_eq!(read(&seen), "later\n");

        let r = rewind(&env, "a", "cp");
        assert_eq!(r["restarted_services"], json!(["web"]), "{backend}: {r}");
        let new = pid_of(&env).expect("restarted pid") as i32;
        assert_ne!(new, old);
        assert!(alive(new), "{backend}: restarted service is running");
        assert!(
            poll(Duration::from_secs(3), || !alive(old)),
            "{backend}: old service stopped"
        );
        // The new process started after the files were restored, in the session.
        assert_eq!(read(&seen), "checkpointed\n", "{backend}");
        let cwd = process_cwd(new as u32).expect("the service's cwd");
        assert_eq!(cwd, a.canonicalize().unwrap(), "{backend}");
        let st = ok(env.api("status", json!({"session": "a"})), "status");
        let svc = &st["sessions"][0]["services"][0];
        assert_eq!(svc["state"], "running", "{st}");

        // A stopped service is not started by a rewind.
        let out = shoal_cli(&env, &["stop", "a"]);
        assert!(out.status.success(), "stop: {}", stderr(&out));
        assert!(poll(Duration::from_secs(3), || !alive(new)));
        let r = rewind(&env, "a", "cp");
        assert_eq!(r["restarted_services"], json!([]), "{backend}: {r}");
        assert!(pid_of(&env).is_none(), "{backend}: nothing started");
        assert!(!alive(new));
    }
}

// ---------------------------------------------------------------------------
// MCP parity
// ---------------------------------------------------------------------------

#[test]
fn mcp_fork_checkpoint_rewind_match_the_api() {
    let backend = *backends().last().unwrap();
    let env = setup(5402, backend);
    let mut mcp = env.mcp();
    mcp.initialize();
    let (e, v) = mcp.call(1, "fork", json!({"name": "m", "count": 2}));
    assert!(!e, "{v}");
    assert_eq!(v["sessions"][1]["name"], "m-2");
    let path = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
    wr(path.join("v.txt"), "1\n");
    let (e, v) = mcp.call(2, "checkpoint", json!({"path": path}));
    assert!(!e, "{v}");
    assert_eq!(v["label"], "cp1");
    wr(path.join("v.txt"), "2\n");
    let (e, v) = mcp.call(3, "rewind", json!({"session": "m-1", "label": "latest"}));
    assert!(!e, "{v}");
    assert_eq!(read(path.join("v.txt")), "1\n");
    for (id, tool, args, needle) in [
        (
            4,
            "fork",
            json!({"count": 9}),
            "count must be between 1 and 8",
        ),
        (5, "fork", json!({"at": "cp1"}), "at needs from"),
        (
            6,
            "fork",
            json!({"from": "m-1", "at": "nope"}),
            "no checkpoint named nope",
        ),
        (
            7,
            "checkpoint",
            json!({"session": "m-1", "label": "cp1"}),
            "already has a checkpoint",
        ),
        (
            8,
            "rewind",
            json!({"session": "m-1", "label": "nope"}),
            "it has: cp1",
        ),
        (9, "rewind", json!({"session": "m-1"}), "rewind needs label"),
    ] {
        let (e, v) = mcp.call(id, tool, args);
        assert!(e, "{tool} should fail: {v}");
        assert!(v.as_str().unwrap().contains(needle), "{tool}: {v}");
    }
    assert!(mcp.close().success());
}

// Fixed bug (overlay, critical): ops::rewind deletes the session's upper and work
// directories before it copies the checkpoint in. If that copy fails (the
// checkpoint directory is missing or unreadable, or the disk is full), the
// error is returned with the upper layer already gone: every uncommitted
// change and every commit made in the session is lost, and the session is
// left unmounted. The clone backend gets this right (it renames the session
// aside and puts it back on failure). A checkpoint can be missing on disk
// while still listed in state.json, e.g. after a crash between prune_auto's
// remove_dir_all and its state save.
#[test]
fn a_failed_rewind_never_loses_the_sessions_current_work() {
    for backend in backends() {
        let env = setup(5404, backend);
        let (_, a) = fork1(&env, json!({"name": "a"}));
        checkpoint(&env, "a", Some("cp"));
        wr(a.join("precious.txt"), "do not lose me\n");
        wr(a.join("src/app.txt"), "edited\n");
        commit(&env, &a, "precious commit");
        wr(a.join("uncommitted.txt"), "also precious\n");
        let want = tree(&a);
        let want_head = head(&env, &a);
        // The checkpoint's files disappear while state.json still lists it.
        fs::remove_dir_all(env.project_dir().join("checkpoints/a/cp")).unwrap();

        let r = env.api("rewind", json!({"session": "a", "label": "cp"}));
        assert!(
            r.is_err(),
            "{backend}: rewind to a missing checkpoint should fail: {r:?}"
        );
        // Whatever the error, the session must still hold its work.
        ok(
            env.api("status", json!({"session": "a"})),
            "status remounts",
        );
        assert!(
            a.join("precious.txt").exists(),
            "{backend}: rewind failure deleted the session's work"
        );
        assert_eq!(tree(&a), want, "{backend}");
        assert_eq!(head(&env, &a), want_head, "{backend}");
    }
}
