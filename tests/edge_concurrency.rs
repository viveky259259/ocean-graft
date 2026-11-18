//! Edge-case tests for concurrency, crash leftovers and unusual repository
//! paths, against plugin/ARCHITECTURE.md and src/{ops,project,hook}.rs.
//!
//! Real parallel processes: every test starts several `shoal` processes at
//! once (threads released together by a barrier) and then checks that the
//! project is still consistent: state.json parses, every recorded session and
//! checkpoint directory exists, nothing unrecorded is lying around, ports and
//! names are distinct.
//!
//! Port range 5600-5699. No test here starts anything that listens on a
//! port (ports are only recorded in state.json), so each Env takes its own
//! port_base inside the range rather than a block of 20; there are more tests
//! than 20-port blocks.
//!
//! Commands that may run outside a repository set GIT_CEILING_DIRECTORIES to
//! the test's temporary root: target/tmp may live inside another git repository,
//! and a "non-git directory" would otherwise resolve to it.

mod common;
use common::*;
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

extern "C" {
    #[link_name = "flock"]
    fn c_flock(fd: i32, op: i32) -> i32;
}
const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;

/// Run `f(i)` for i in 0..n on n threads released at the same moment.
fn par<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let barrier = Barrier::new(n);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let f = &f;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    f(i)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// A shoal command with the git ceiling at the test root.
fn cmd(env: &Env, dir: &Path) -> Command {
    let mut c = env.command(BIN, dir);
    c.env("GIT_CEILING_DIRECTORIES", env.root.path());
    c
}

fn run_shoal(env: &Env, dir: &Path, args: &[&str]) -> std::process::Output {
    cmd(env, dir).args(args).output().unwrap()
}

/// `shoal api` in `dir` with the git ceiling set.
fn api_at(env: &Env, dir: &Path, tool: &str, args: Value) -> Result<Value, String> {
    let out = cmd(env, dir)
        .args(["api", tool, &args.to_string()])
        .output()
        .unwrap();
    let text = stdout(&out);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "api {tool} printed non-JSON ({e}):\n{text}\nstderr:\n{}",
            stderr(&out)
        )
    });
    if out.status.success() {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or_default().to_string())
    }
}

fn spawn_hook(env: &Env, event: &str, input: &Value, dir: &Path) -> Child {
    let mut child = cmd(env, dir)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input.to_string().as_bytes());
    drop(stdin);
    child
}

fn finish(child: Child) -> HookResult {
    let out = child.wait_with_output().unwrap();
    HookResult {
        code: out.status.code().unwrap_or(-1),
        stdout: stdout(&out),
        stderr: stderr(&out),
    }
}

fn hook_at(env: &Env, event: &str, input: &Value, dir: &Path) -> HookResult {
    finish(spawn_hook(env, event, input, dir))
}

/// Commit a shoal.toml choosing the backend and port base, so a hook's
/// automatic init uses them.
fn commit_config(env: &Env, backend: &str, port_base: u16) {
    fs::write(
        env.repo().join("shoal.toml"),
        format!("backend = \"{backend}\"\nport_base = {port_base}\n"),
    )
    .unwrap();
    env.git(&env.repo(), &["add", "shoal.toml"]);
    env.git(&env.repo(), &["commit", "-q", "-m", "shoal config"]);
}

/// Directories (not lock files) under $SHOAL_HOME/projects.
fn project_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join("projects")) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

fn dir_names(dir: &Path) -> BTreeSet<String> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// The checks every concurrent test ends with. `stale` names directories
/// under sessions/ (and volumes/) that the test planted on purpose.
fn check_invariants_in(home: &Path, stale: &[&str]) -> Value {
    let projects = project_dirs(home);
    assert_eq!(
        projects.len(),
        1,
        "expected exactly one project dir, found {projects:?}"
    );
    let dir = &projects[0];
    let raw = read(dir.join("state.json"));
    let state: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("state.json is not valid JSON ({e}):\n{raw}"));
    assert!(
        !dir.join("state.json.tmp").exists(),
        "a temporary state file was left behind"
    );
    let sessions = state["sessions"]
        .as_object()
        .expect("sessions is an object");
    let mut ports = BTreeSet::new();
    for (name, s) in sessions {
        assert_eq!(s["name"], name.as_str());
        assert_eq!(s["branch"], format!("shoal/{name}"));
        let path = dir.join("sessions").join(name);
        assert!(path.is_dir(), "recorded session {name} has no directory");
        assert!(
            path.join(".git").exists(),
            "session {name} directory is not a checkout (unmounted or empty)"
        );
        assert!(
            ports.insert(s["port"].as_u64().unwrap()),
            "two sessions share port {}",
            s["port"]
        );
    }
    for entry in dir_names(&dir.join("sessions")) {
        assert!(
            sessions.contains_key(&entry) || stale.contains(&entry.as_str()),
            "sessions/{entry} exists but is not recorded in state.json"
        );
    }
    for entry in dir_names(&dir.join("volumes")) {
        assert!(
            sessions.contains_key(&entry) || stale.contains(&entry.as_str()),
            "volumes/{entry} exists but is not recorded in state.json"
        );
    }
    let mut seen = BTreeSet::new();
    for c in state["checkpoints"]
        .as_array()
        .expect("checkpoints is an array")
    {
        let s = c["session"].as_str().unwrap();
        let l = c["label"].as_str().unwrap();
        assert!(
            sessions.contains_key(s),
            "checkpoint {s}@{l} of an unknown session"
        );
        assert!(
            seen.insert((s.to_string(), l.to_string())),
            "duplicate checkpoint {s}@{l}"
        );
        assert!(
            dir.join("checkpoints").join(s).join(l).is_dir(),
            "checkpoint {s}@{l} has no directory"
        );
    }
    for s in dir_names(&dir.join("checkpoints")) {
        for l in dir_names(&dir.join("checkpoints").join(&s)) {
            assert!(
                seen.contains(&(s.clone(), l.clone())),
                "checkpoints/{s}/{l} exists but is not recorded"
            );
        }
    }
    for g in state["generations"].as_array().unwrap() {
        for layer in g["layers"].as_array().unwrap() {
            assert!(
                dir.join(layer.as_str().unwrap()).is_dir(),
                "generation layer {layer} is missing"
            );
        }
    }
    for entry in dir_names(dir) {
        assert!(!entry.starts_with("trash-"), "left-over {entry}");
        assert!(
            entry != "build" && entry != "build-work",
            "left-over {entry}"
        );
    }
    state
}

fn check_invariants(env: &Env) -> Value {
    check_invariants_in(&env.home(), &[])
}

fn session_path(env: &Env, name: &str) -> PathBuf {
    env.project_dir().join("sessions").join(name)
}

fn first_session(v: &Value) -> (String, PathBuf) {
    let s = &v["sessions"][0];
    (
        s["name"].as_str().unwrap().to_string(),
        PathBuf::from(s["path"].as_str().unwrap()),
    )
}

fn commit_in(env: &Env, dir: &Path, file: &str, content: &str, msg: &str) {
    if let Some(parent) = dir.join(file).parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(dir.join(file), content).unwrap();
    env.git(dir, &["add", "-A"]);
    env.git(dir, &["commit", "-q", "-m", msg]);
}

/// A git repository at `dir` on branch main with one commit.
fn make_repo(env: &Env, dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/app.txt"), "line one\nline two\n").unwrap();
    fs::write(dir.join(".gitignore"), ".env\n").unwrap();
    fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
    env.git(dir, &["init", "-q", "-b", "main"]);
    env.git(dir, &["add", "-A"]);
    env.git(dir, &["commit", "-q", "-m", "initial"]);
}

// ---------------------------------------------------------------------------
// Parallel WorktreeCreate on a repository with no project
// ---------------------------------------------------------------------------

#[test]
fn six_parallel_worktree_creates_on_fresh_repo_build_one_project() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5600 + i as u16;
        let env = Env::new(port);
        commit_config(&env, backend, port);
        let repo = env.repo();
        let results = par(6, |k| {
            hook_at(
                &env,
                "worktree-create",
                &json!({"name": format!("agent-{k}"), "cwd": repo, "hook_event_name": "WorktreeCreate"}),
                &repo,
            )
        });
        let mut paths = BTreeSet::new();
        for (k, r) in results.iter().enumerate() {
            assert_eq!(r.code, 0, "[{backend}] hook {k} failed: {}", r.stderr);
            let line = r.last_line();
            let p = PathBuf::from(&line);
            assert!(
                p.is_absolute(),
                "[{backend}] not an absolute path: {line:?}"
            );
            assert!(
                p.join("src/app.txt").is_file(),
                "[{backend}] {line} is not a checkout"
            );
            assert!(
                paths.insert(line.clone()),
                "[{backend}] path {line} handed out twice"
            );
        }
        let state = check_invariants(&env);
        assert_eq!(state["backend"], backend);
        assert_eq!(
            state["generations"].as_array().unwrap().len(),
            1,
            "[{backend}] one template"
        );
        let tdir = env.project_dir().join(if backend == "overlay" {
            "layers"
        } else {
            "templates"
        });
        assert_eq!(
            dir_names(&tdir).len(),
            1,
            "[{backend}] template dirs: {:?}",
            dir_names(&tdir)
        );
        let names: BTreeSet<String> = state["sessions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let want: BTreeSet<String> = (0..6).map(|k| format!("agent-{k}")).collect();
        assert_eq!(names, want, "[{backend}]");
        for name in &names {
            let p = session_path(&env, name).canonicalize().unwrap();
            assert!(
                paths.contains(&p.display().to_string()),
                "[{backend}] {name} path not printed"
            );
            let branch = env.git(&p, &["rev-parse", "--abbrev-ref", "HEAD"]);
            assert_eq!(branch.trim(), format!("shoal/{name}"));
        }
    }
}

#[test]
fn six_parallel_worktree_creates_with_the_same_name_get_distinct_sessions() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5603 + i as u16;
        let env = Env::new(port);
        commit_config(&env, backend, port);
        let repo = env.repo();
        let results = par(6, |_| {
            hook_at(
                &env,
                "worktree-create",
                &json!({"name": "feature/login", "cwd": repo}),
                &repo,
            )
        });
        let mut paths = BTreeSet::new();
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
            assert!(
                paths.insert(r.last_line()),
                "[{backend}] duplicate path {}",
                r.last_line()
            );
        }
        let state = check_invariants(&env);
        let names: BTreeSet<String> = state["sessions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let want: BTreeSet<String> = std::iter::once("feature-login".to_string())
            .chain((2..=6).map(|k| format!("feature-login-{k}")))
            .collect();
        assert_eq!(names, want, "[{backend}]");
    }
}

#[test]
fn parallel_api_forks_on_fresh_repo_build_one_project() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5606 + i as u16;
        let env = Env::new(port);
        commit_config(&env, backend, port);
        let results = par(5, |k| env.api("fork", json!({"name": format!("w{k}")})));
        for r in &results {
            assert!(r.is_ok(), "[{backend}] {r:?}");
        }
        let state = check_invariants(&env);
        assert_eq!(state["sessions"].as_object().unwrap().len(), 5);
        assert_eq!(state["generations"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn parallel_worktree_creates_after_source_moved_refresh_exactly_once() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5609 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        commit_in(&env, &repo, "src/new.txt", "new\n", "move main");
        let head = env.git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        let results = par(5, |k| {
            hook_at(
                &env,
                "worktree-create",
                &json!({"name": format!("r{k}"), "cwd": repo}),
                &repo,
            )
        });
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
            assert!(
                Path::new(&r.last_line()).join("src/new.txt").is_file(),
                "[{backend}] stale template"
            );
        }
        let state = check_invariants(&env);
        let gens = state["generations"].as_array().unwrap();
        assert_eq!(gens.len(), 2, "[{backend}] exactly one refresh: {gens:?}");
        assert_eq!(gens[1]["commit"], head.as_str());
        for s in state["sessions"].as_object().unwrap().values() {
            assert_eq!(s["base_commit"], head.as_str());
            assert_eq!(s["generation"], 2);
        }
    }
}

#[test]
fn parallel_subagent_worktree_creates_inside_a_session_fork_it() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5612 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, parent) = first_session(&env.api("fork", json!({"name": "parent"})).unwrap());
        fs::write(parent.join("src/app.txt"), "parent edit\n").unwrap();
        fs::write(parent.join("wip.txt"), "untracked\n").unwrap();
        let results = par(6, |k| {
            hook_at(
                &env,
                "worktree-create",
                &json!({"name": format!("sub{k}"), "cwd": parent}),
                &parent,
            )
        });
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
            let p = PathBuf::from(r.last_line());
            assert_eq!(
                read(p.join("src/app.txt")),
                "parent edit\n",
                "[{backend}] parent's work missing"
            );
            assert_eq!(read(p.join("wip.txt")), "untracked\n");
            assert_eq!(read(p.join(".env")), "SECRET=1\n");
        }
        let state = check_invariants(&env);
        for k in 0..6 {
            assert_eq!(
                state["sessions"][format!("sub{k}")]["parent"],
                "parent",
                "[{backend}]"
            );
        }
        // The parent is untouched and still usable.
        assert_eq!(read(parent.join("src/app.txt")), "parent edit\n");
        fs::write(parent.join("after.txt"), "x").unwrap();
        for k in 0..6 {
            assert!(
                !session_path(&env, &format!("sub{k}"))
                    .join("after.txt")
                    .exists(),
                "[{backend}] fork shares the parent's upper dir"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel forks through the api
// ---------------------------------------------------------------------------

#[test]
fn eight_parallel_forks_from_the_same_session() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5615 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, base) = first_session(&env.api("fork", json!({"name": "base"})).unwrap());
        commit_in(&env, &base, "committed.txt", "c\n", "base commit");
        fs::write(base.join("src/app.txt"), "uncommitted\n").unwrap();
        fs::create_dir_all(base.join("deps")).unwrap();
        fs::write(base.join("deps/ignored.bin"), "ignored\n").unwrap();
        let results = par(8, |k| {
            env.api("fork", json!({"from": "base", "name": format!("f{k}")}))
        });
        let mut paths = BTreeSet::new();
        for (k, r) in results.iter().enumerate() {
            let v = r
                .as_ref()
                .unwrap_or_else(|e| panic!("[{backend}] fork {k}: {e}"));
            let (name, path) = first_session(v);
            assert_eq!(name, format!("f{k}"));
            assert!(paths.insert(path.clone()));
            assert_eq!(
                read(path.join("src/app.txt")),
                "uncommitted\n",
                "[{backend}]"
            );
            assert_eq!(read(path.join("committed.txt")), "c\n");
            assert_eq!(read(path.join("deps/ignored.bin")), "ignored\n");
            assert_eq!(v["sessions"][0]["parent"], "base");
            assert_eq!(v["sessions"][0]["branch"], format!("shoal/f{k}"));
        }
        let state = check_invariants(&env);
        assert_eq!(state["sessions"].as_object().unwrap().len(), 9);
        // Independent: a change in one fork is not seen by the others or by base.
        fs::write(session_path(&env, "f0").join("only-f0.txt"), "x").unwrap();
        for k in 1..8 {
            assert!(!session_path(&env, &format!("f{k}"))
                .join("only-f0.txt")
                .exists());
        }
        assert!(!base.join("only-f0.txt").exists());
        assert_eq!(read(base.join("src/app.txt")), "uncommitted\n");
    }
}

#[test]
fn parallel_forks_with_the_same_name_are_numbered_not_lost() {
    let env = Env::new(5618);
    env.init("clone");
    let results = par(8, |_| env.api("fork", json!({"name": "dup"})));
    let mut names = BTreeSet::new();
    for r in &results {
        let v = r.as_ref().unwrap_or_else(|e| panic!("{e}"));
        assert!(names.insert(first_session(v).0));
    }
    let want: BTreeSet<String> = std::iter::once("dup".to_string())
        .chain((2..=8).map(|k| format!("dup-{k}")))
        .collect();
    assert_eq!(names, want);
    check_invariants(&env);
}

#[test]
fn parallel_best_of_n_forks_never_share_a_name() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5620 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let results = par(4, |_| env.api("fork", json!({"name": "try", "count": 2})));
        let mut names = BTreeSet::new();
        for r in &results {
            let v = r.as_ref().unwrap_or_else(|e| panic!("[{backend}] {e}"));
            assert!(
                v.get("error").is_none_or(Value::is_null),
                "[{backend}] partial: {v}"
            );
            let s = v["sessions"].as_array().unwrap();
            assert_eq!(s.len(), 2);
            for x in s {
                assert!(
                    names.insert(x["name"].as_str().unwrap().to_string()),
                    "[{backend}] duplicate {x}"
                );
            }
        }
        assert_eq!(names.len(), 8);
        check_invariants(&env);
    }
}

#[test]
fn parallel_forks_from_a_checkpoint() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5623 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, base) = first_session(&env.api("fork", json!({"name": "base"})).unwrap());
        fs::write(base.join("src/app.txt"), "at checkpoint\n").unwrap();
        env.api("checkpoint", json!({"session": "base", "label": "cp-a"}))
            .unwrap();
        fs::write(base.join("src/app.txt"), "after checkpoint\n").unwrap();
        let results = par(6, |k| {
            env.api(
                "fork",
                json!({"from": "base", "at": "cp-a", "name": format!("c{k}")}),
            )
        });
        for r in &results {
            let v = r.as_ref().unwrap_or_else(|e| panic!("[{backend}] {e}"));
            let (_, path) = first_session(v);
            assert_eq!(
                read(path.join("src/app.txt")),
                "at checkpoint\n",
                "[{backend}]"
            );
            assert_eq!(v["sessions"][0]["from_checkpoint"], "cp-a");
        }
        check_invariants(&env);
        assert_eq!(read(base.join("src/app.txt")), "after checkpoint\n");
    }
}

// ---------------------------------------------------------------------------
// Parallel checkpoints
// ---------------------------------------------------------------------------

#[test]
fn parallel_checkpoints_on_the_same_session_get_distinct_labels() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5626 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
        fs::write(a.join("src/app.txt"), "changed\n").unwrap();
        let results = par(10, |_| env.api("checkpoint", json!({"session": "a"})));
        let mut labels = BTreeSet::new();
        for r in &results {
            let v = r.as_ref().unwrap_or_else(|e| panic!("[{backend}] {e}"));
            assert!(
                labels.insert(v["label"].as_str().unwrap().to_string()),
                "[{backend}] label collision: {v}"
            );
        }
        let want: BTreeSet<String> = (1..=10).map(|k| format!("cp{k}")).collect();
        assert_eq!(labels, want, "[{backend}]");
        let state = check_invariants(&env);
        let recorded: BTreeSet<String> = state["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(recorded, want, "[{backend}] every checkpoint recorded");
        // Each one restores the session's state.
        for label in ["cp1", "cp10"] {
            fs::write(a.join("src/app.txt"), "trashed\n").unwrap();
            env.api("rewind", json!({"session": "a", "label": label}))
                .unwrap();
            assert_eq!(
                read(a.join("src/app.txt")),
                "changed\n",
                "[{backend}] {label}"
            );
        }
    }
}

#[test]
fn parallel_checkpoints_with_the_same_label_exactly_one_wins() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5629 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
        fs::write(a.join("src/app.txt"), "v1\n").unwrap();
        let results = par(6, |_| {
            env.api("checkpoint", json!({"session": "a", "label": "same"}))
        });
        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok, 1, "[{backend}] {results:?}");
        for r in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                r.contains("already has a checkpoint named same"),
                "[{backend}] unclear error: {r}"
            );
        }
        let state = check_invariants(&env);
        assert_eq!(state["checkpoints"].as_array().unwrap().len(), 1);
        fs::write(a.join("src/app.txt"), "v2\n").unwrap();
        env.api("rewind", json!({"session": "a", "label": "same"}))
            .unwrap();
        assert_eq!(
            read(a.join("src/app.txt")),
            "v1\n",
            "[{backend}] the winner's copy survived intact"
        );
    }
}

#[test]
fn parallel_checkpoints_across_sessions_are_all_recorded() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5632 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        env.api("fork", json!({"name": "s", "count": 4})).unwrap();
        for k in 1..=4 {
            fs::write(
                session_path(&env, &format!("s-{k}")).join("mine.txt"),
                format!("{k}\n"),
            )
            .unwrap();
        }
        // 4 sessions x 3 checkpoints each, all at once.
        let results = par(12, |j| {
            env.api("checkpoint", json!({"session": format!("s-{}", j % 4 + 1)}))
        });
        for r in &results {
            assert!(r.is_ok(), "[{backend}] {r:?}");
        }
        let state = check_invariants(&env);
        let mut per: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in state["checkpoints"].as_array().unwrap() {
            per.entry(c["session"].as_str().unwrap().into())
                .or_default()
                .insert(c["label"].as_str().unwrap().into());
        }
        let want: BTreeSet<String> = ["cp1", "cp2", "cp3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(per.len(), 4, "[{backend}]");
        for (s, labels) in &per {
            assert_eq!(labels, &want, "[{backend}] {s}");
            // Each session's checkpoint holds its own file, not a neighbour's.
            let k = s.trim_start_matches("s-");
            let cp = env.project_dir().join("checkpoints").join(s).join("cp1");
            assert_eq!(read(cp.join("mine.txt")), format!("{k}\n"));
        }
    }
}

#[test]
fn parallel_stop_hooks_on_one_session_take_at_most_one_checkpoint() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5635 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
        fs::write(a.join("src/app.txt"), "turn work\n").unwrap();
        let results = par(6, |_| {
            hook_at(
                &env,
                "stop",
                &json!({"cwd": a, "hook_event_name": "Stop"}),
                &a,
            )
        });
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
            assert!(
                r.stdout.trim().is_empty(),
                "[{backend}] stop printed to stdout: {}",
                r.stdout
            );
        }
        let state = check_invariants(&env);
        let cheap = backend == "overlay"
            || cfg!(target_os = "macos")
            || fs::read_to_string(env.project_dir().join(".reflink"))
                .map(|v| v.trim() == "yes")
                .unwrap_or(false);
        let autos: Vec<&Value> = state["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["auto"] == true)
            .collect();
        assert_eq!(
            autos.len(),
            usize::from(cheap),
            "[{backend}] identical turns checkpointed: {autos:?}"
        );
        if cheap {
            assert_eq!(autos[0]["label"], "turn-1");
        }
    }
}

// ---------------------------------------------------------------------------
// Land alongside other work
// ---------------------------------------------------------------------------

#[test]
fn land_runs_while_other_sessions_fork_and_checkpoint() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5638 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        let (_, lander) = first_session(&env.api("fork", json!({"name": "lander"})).unwrap());
        let (_, other) = first_session(&env.api("fork", json!({"name": "other"})).unwrap());
        commit_in(&env, &lander, "feature.txt", "feature\n", "add feature");
        // main moves too, so land has to rebase (and checkpoint) first.
        commit_in(&env, &repo, "upstream.txt", "upstream\n", "upstream change");
        fs::write(other.join("other.txt"), "o\n").unwrap();
        let land_start = Instant::now();
        let (land_res, others) = std::thread::scope(|s| {
            let land = s.spawn(|| {
                let r = env.api("land", json!({"session": "lander", "test": "sleep 2"}));
                (r, land_start.elapsed())
            });
            // Give land time to reach its test run, then do other work.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !env
                .project_dir()
                .join("logs/lander-land-tests.log")
                .exists()
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            let others = par(6, |k| {
                let r = if k < 3 {
                    env.api("fork", json!({"name": format!("side{k}")}))
                } else if k < 5 {
                    env.api("checkpoint", json!({"session": "other"}))
                } else {
                    env.api("fork", json!({"from": "other", "name": "other-copy"}))
                };
                (r, land_start.elapsed())
            });
            (land.join().unwrap(), others)
        });
        let (land, land_done) = land_res;
        let land = land.unwrap_or_else(|e| panic!("[{backend}] land: {e}"));
        assert_eq!(land["status"], "landed", "[{backend}] {land}");
        assert_eq!(land["checkpoint"], "pre-land-1", "[{backend}]");
        for (r, done) in &others {
            assert!(r.is_ok(), "[{backend}] {r:?}");
            // Rule 3: none of these waited for land's tests.
            assert!(
                *done < land_done,
                "[{backend}] an operation waited for land's tests ({done:?} vs {land_done:?})"
            );
        }
        assert_eq!(read(repo.join("feature.txt")), "feature\n");
        assert_eq!(read(repo.join("upstream.txt")), "upstream\n");
        let log = env.git(&repo, &["log", "--format=%s", "main"]);
        assert!(
            log.starts_with("add feature\nupstream change\n"),
            "[{backend}] {log}"
        );
        assert!(env.git(&repo, &["status", "--porcelain"]).trim().is_empty());
        let state = check_invariants(&env);
        let labels: BTreeSet<String> = state["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                format!(
                    "{}@{}",
                    c["session"].as_str().unwrap(),
                    c["label"].as_str().unwrap()
                )
            })
            .collect();
        for want in ["lander@pre-land-1", "other@cp1", "other@cp2"] {
            assert!(labels.contains(want), "[{backend}] {labels:?}");
        }
        assert_eq!(
            read(session_path(&env, "other-copy").join("other.txt")),
            "o\n"
        );
    }
}

#[test]
fn checkpoint_of_the_landing_session_during_its_tests() {
    let env = Env::new(5641);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let (_, s) = first_session(&env.api("fork", json!({"name": "s"})).unwrap());
    commit_in(&env, &s, "f.txt", "f\n", "work");
    commit_in(&env, &env.repo(), "u.txt", "u\n", "upstream");
    let (land, cp) = std::thread::scope(|sc| {
        let land = sc.spawn(|| env.api("land", json!({"session": "s", "test": "sleep 2"})));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !env.project_dir().join("logs/s-land-tests.log").exists() && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let cp = env.api("checkpoint", json!({"session": "s"}));
        (land.join().unwrap(), cp)
    });
    let land = land.unwrap();
    assert_eq!(land["status"], "landed", "{land}");
    // A checkpoint mid-land could catch the rebase half done, so it is
    // refused with a clear message, and works again once the land is over.
    let e = cp.unwrap_err();
    assert!(e.contains("being landed"), "{e}");
    let cp = env.api("checkpoint", json!({"session": "s"})).unwrap();
    assert_eq!(cp["label"], "cp1");
    let state = check_invariants(&env);
    assert_eq!(state["checkpoints"].as_array().unwrap().len(), 2);
    env.api("rewind", json!({"session": "s", "label": "cp1"}))
        .unwrap();
    assert!(!s.join(".git/rebase-merge").exists() && !s.join(".git/rebase-apply").exists());
    assert_eq!(read(s.join("u.txt")), "u\n");
}

/// Four lands into the checked-out main at once. The contract allows
/// `target_moving` once main moved three times under a land (with four
/// parallel lands the last one can see exactly that), but never
/// `target_blocked` (the user has no edits in the way), never an error, never
/// lost work, never a dirty working tree, and a retry of a `target_moving`
/// land must land.
// Fixed bug: step 5 of land (`git merge --ff-only` in the user's checked-out
// working tree) runs with no lock shared between concurrent lands. Two merges
// interleave: one writes its files into the working tree and index, then
// loses the race to move main. The user's repository is left with stray
// untracked files (`?? f2.txt`) and sometimes a staged deletion of a commit
// that DID land (`D  f4.txt`): a following `git commit` by the user deletes
// landed work from main. The losing land reports `target_blocked`, blaming
// "untracked working tree files would be overwritten" (shoal's own leftovers
// for the user's edits), or `target_moving`, and every later land of that
// session is blocked by the leftovers. Reproduces in most runs.
#[test]
fn parallel_lands_into_the_checked_out_branch_all_land() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5644 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        env.api("fork", json!({"name": "l", "count": 4})).unwrap();
        for k in 1..=4 {
            commit_in(
                &env,
                &session_path(&env, &format!("l-{k}")),
                &format!("f{k}.txt"),
                "x\n",
                &format!("feature {k}"),
            );
        }
        let results = par(4, |k| {
            env.api(
                "land",
                json!({"session": format!("l-{}", k + 1), "test": ""}),
            )
        });
        let mut statuses = Vec::new();
        for r in &results {
            let v = r
                .as_ref()
                .unwrap_or_else(|e| panic!("[{backend}] land errored: {e}"));
            statuses.push(v["status"].as_str().unwrap().to_string());
        }
        // Whatever happened, nothing is lost: every session's work is in the
        // source repository, on main or on shoal/<name>.
        for k in 1..=4 {
            let on_main = repo.join(format!("f{k}.txt")).exists();
            let on_branch = env
                .command("git", &repo)
                .args(["cat-file", "-e", &format!("shoal/l-{k}:f{k}.txt")])
                .status()
                .unwrap()
                .success();
            assert!(
                on_main || on_branch,
                "[{backend}] work of l-{k} is nowhere in the source"
            );
        }
        let dirty = env.git(&repo, &["status", "--porcelain"]);
        assert!(
            dirty.trim().is_empty(),
            "[{backend}] the user's working tree was left dirty by concurrent lands:\n{dirty}\nlands: {statuses:?}\n{results:?}\nlog:\n{}",
            env.git(&repo, &["log", "--oneline", "--stat", "main"])
        );
        let merges = env.git(&repo, &["rev-list", "--merges", "main"]);
        assert!(
            merges.trim().is_empty(),
            "[{backend}] main has merge commits"
        );
        check_invariants(&env);
        assert!(
            statuses
                .iter()
                .all(|s| s == "landed" || s == "target_moving"),
            "[{backend}] concurrent lands: {statuses:?} {results:?}"
        );
        assert!(
            statuses.iter().filter(|s| *s == "landed").count() >= 3,
            "[{backend}] {statuses:?}"
        );
        for (k, st) in statuses.iter().enumerate() {
            if st == "target_moving" {
                let again = env
                    .api(
                        "land",
                        json!({"session": format!("l-{}", k + 1), "test": ""}),
                    )
                    .unwrap();
                assert_eq!(again["status"], "landed", "[{backend}] retry: {again}");
            }
        }
        for k in 1..=4 {
            assert!(
                repo.join(format!("f{k}.txt")).exists(),
                "[{backend}] f{k} not on main"
            );
        }
        let log = env.git(&repo, &["log", "--format=%s", "main"]);
        for k in 1..=4 {
            assert_eq!(
                log.matches(&format!("feature {k}\n")).count(),
                1,
                "[{backend}] {log}"
            );
        }
    }
}

#[test]
fn parallel_lands_into_a_branch_that_is_not_checked_out_all_land() {
    let env = Env::new(5647);
    env.init("clone");
    let repo = env.repo();
    // The user is on another branch, so land updates main by fetch.
    env.git(&repo, &["switch", "-q", "-c", "elsewhere"]);
    env.api("fork", json!({"name": "m", "count": 3})).unwrap();
    for k in 1..=3 {
        commit_in(
            &env,
            &session_path(&env, &format!("m-{k}")),
            &format!("g{k}.txt"),
            "x\n",
            &format!("change {k}"),
        );
    }
    let results = par(3, |k| {
        env.api(
            "land",
            json!({"session": format!("m-{}", k + 1), "test": ""}),
        )
    });
    let statuses: Vec<String> = results
        .iter()
        .map(|r| {
            r.as_ref()
                .map(|v| v["status"].as_str().unwrap().to_string())
                .unwrap_or_else(|e| format!("error: {e}"))
        })
        .collect();
    check_invariants(&env);
    assert_eq!(
        env.git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim(),
        "elsewhere"
    );
    assert!(statuses.iter().all(|s| s == "landed"), "{statuses:?}");
    for k in 1..=3 {
        env.git(&repo, &["cat-file", "-e", &format!("main:g{k}.txt")]);
    }
}

// ---------------------------------------------------------------------------
// Removal racing other work
// ---------------------------------------------------------------------------

#[test]
fn parallel_worktree_removes_save_every_sessions_work() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5650 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        env.api("fork", json!({"name": "rm", "count": 5})).unwrap();
        let mut paths = Vec::new();
        for k in 1..=5 {
            let p = session_path(&env, &format!("rm-{k}"))
                .canonicalize()
                .unwrap();
            fs::write(p.join(format!("work{k}.txt")), format!("work {k}\n")).unwrap();
            paths.push(p);
        }
        // Meanwhile, new worktrees are created.
        let results = par(8, |k| {
            if k < 5 {
                hook_at(
                    &env,
                    "worktree-remove",
                    &json!({"worktree_path": paths[k]}),
                    &repo,
                )
            } else {
                hook_at(
                    &env,
                    "worktree-create",
                    &json!({"name": format!("new{k}"), "cwd": repo}),
                    &repo,
                )
            }
        });
        for r in &results {
            assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
        }
        for (k, p) in paths.iter().enumerate() {
            assert!(!p.exists(), "[{backend}] {} still exists", p.display());
            let k = k + 1;
            let content = env.git(&repo, &["show", &format!("shoal/rm-{k}:work{k}.txt")]);
            assert_eq!(content, format!("work {k}\n"), "[{backend}] work lost");
        }
        let state = check_invariants(&env);
        let names: BTreeSet<String> = state["sessions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            names,
            ["new5", "new6", "new7"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "[{backend}]"
        );
    }
}

// Fixed bug: two WorktreeRemove hooks for the same path race. The loser checks
// that the path exists before taking the project lock; once it gets the lock
// the winner has removed the session, session_at() finds nothing, and the
// loser reports "<path> is not a shoal session, so shoal will not delete it"
// and exits 1, so Claude Code sees a failed removal of a worktree that was
// removed (and saved) correctly. Expected: "already gone" -> exit 0.
#[test]
fn parallel_worktree_removes_of_the_same_path_are_idempotent() {
    let env = Env::new(5653);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let repo = env.repo();
    let (_, p) = first_session(&env.api("fork", json!({"name": "twice"})).unwrap());
    let p = p.canonicalize().unwrap();
    fs::write(p.join("keep.txt"), "keep\n").unwrap();
    let results = par(4, |_| {
        hook_at(&env, "worktree-remove", &json!({"worktree_path": p}), &repo)
    });
    assert!(!p.exists());
    assert_eq!(env.git(&repo, &["show", "shoal/twice:keep.txt"]), "keep\n");
    check_invariants(&env);
    for r in &results {
        assert_eq!(
            r.code, 0,
            "a duplicate removal of an already-removed session failed: {}",
            r.stderr
        );
    }
}

#[test]
fn fork_racing_a_discard_of_its_parent() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5656 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
        fs::write(a.join("precious.txt"), "precious\n").unwrap();
        let results = par(5, |k| {
            if k == 0 {
                env.api("discard", json!({"session": "a"}))
            } else {
                env.api("fork", json!({"from": "a", "name": format!("child{k}")}))
            }
        });
        assert!(results[0].is_ok(), "[{backend}] discard: {:?}", results[0]);
        for r in &results[1..] {
            match r {
                Ok(v) => {
                    let (_, path) = first_session(v);
                    assert_eq!(read(path.join("precious.txt")), "precious\n", "[{backend}]");
                }
                Err(e) => assert!(
                    e.contains("no session named \"a\""),
                    "[{backend}] unclear error: {e}"
                ),
            }
        }
        assert_eq!(
            env.git(&repo, &["show", "shoal/a:precious.txt"]),
            "precious\n",
            "[{backend}] discarded work lost"
        );
        let state = check_invariants(&env);
        assert!(state["sessions"].get("a").is_none());
        // A fork made from a's (overlay) layer still works after a is gone.
        for name in state["sessions"].as_object().unwrap().keys() {
            assert_eq!(
                read(session_path(&env, name).join("precious.txt")),
                "precious\n"
            );
        }
    }
}

#[test]
fn parallel_rewinds_and_checkpoints_keep_state_consistent() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5659 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        env.api("fork", json!({"name": "r", "count": 3})).unwrap();
        for k in 1..=3 {
            let p = session_path(&env, &format!("r-{k}"));
            fs::write(p.join("v.txt"), "one\n").unwrap();
            env.api(
                "checkpoint",
                json!({"session": format!("r-{k}"), "label": "one"}),
            )
            .unwrap();
            fs::write(p.join("v.txt"), "two\n").unwrap();
        }
        let results = par(9, |j| {
            let s = format!("r-{}", j % 3 + 1);
            if j < 3 {
                env.api("rewind", json!({"session": s, "label": "one"}))
            } else {
                env.api("checkpoint", json!({"session": s}))
            }
        });
        for r in &results {
            assert!(r.is_ok(), "[{backend}] {r:?}");
        }
        let state = check_invariants(&env);
        // 3 "one" + 6 new, plus one pre-rewind safety checkpoint for each
        // rewind that replaced a changed state (all three did).
        let all = state["checkpoints"].as_array().unwrap();
        let safety = all
            .iter()
            .filter(|c| c["label"].as_str().unwrap().starts_with("pre-rewind-"))
            .count();
        assert_eq!(all.len() - safety, 9, "[{backend}] {all:?}");
        assert_eq!(safety, 3, "[{backend}] {all:?}");
        for k in 1..=3 {
            let v = read(session_path(&env, &format!("r-{k}")).join("v.txt"));
            assert!(v == "one\n", "[{backend}] rewind result: {v:?}");
        }
    }
}

#[test]
fn turn_hooks_and_status_during_parallel_forks_never_fail() {
    let env = Env::new(5662);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
    let results = par(12, |k| match k % 4 {
        0 => {
            let r = env.api("fork", json!({"name": format!("p{k}")}));
            (r.is_ok(), format!("{r:?}"))
        }
        1 => {
            let r = hook_at(&env, "session-start", &json!({"cwd": a}), &a);
            (
                r.code == 0 && r.stdout.contains("shoal session \"a\""),
                format!("{} {}", r.stdout, r.stderr),
            )
        }
        2 => {
            let r = hook_at(&env, "prompt", &json!({"cwd": a}), &a);
            (r.code == 0 && r.stdout.is_empty(), r.stderr)
        }
        _ => {
            let r = env.api("status", json!({}));
            (
                r.as_ref()
                    .map(|v| v["initialized"] == true)
                    .unwrap_or(false),
                format!("{r:?}"),
            )
        }
    });
    for (ok, detail) in &results {
        assert!(ok, "{detail}");
    }
    check_invariants(&env);
}

// ---------------------------------------------------------------------------
// The project lock
// ---------------------------------------------------------------------------

#[test]
fn hooks_with_the_project_lock_held_wait_or_skip() {
    let env = Env::new(5665);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let repo = env.repo();
    let (_, a) = first_session(&env.api("fork", json!({"name": "a"})).unwrap());
    fs::write(a.join("src/app.txt"), "changed\n").unwrap();
    let lock = fs::File::options()
        .write(true)
        .open(env.project_dir().join("lock"))
        .unwrap();
    use std::os::unix::io::AsRawFd;
    assert_eq!(unsafe { c_flock(lock.as_raw_fd(), LOCK_EX) }, 0);

    let start = Instant::now();
    let mut create = spawn_hook(
        &env,
        "worktree-create",
        &json!({"name": "waiter", "cwd": repo}),
        &repo,
    );
    let stop = spawn_hook(&env, "stop", &json!({"cwd": a}), &a);
    let prompt = spawn_hook(&env, "prompt", &json!({"cwd": a}), &a);
    let stop = finish(stop);
    let prompt = finish(prompt);
    let waited = start.elapsed();
    assert_eq!(stop.code, 0, "{}", stop.stderr);
    assert_eq!(prompt.code, 0, "{}", prompt.stderr);
    assert!(
        waited >= Duration::from_secs(4),
        "turn hooks did not wait for the lock: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(20),
        "turn hooks waited too long: {waited:?}"
    );
    // WorktreeCreate must not give up: it is still waiting.
    assert!(
        create.try_wait().unwrap().is_none(),
        "worktree-create gave up while the lock was held"
    );
    assert_eq!(unsafe { c_flock(lock.as_raw_fd(), LOCK_UN) }, 0);
    let create = finish(create);
    assert_eq!(create.code, 0, "{}", create.stderr);
    assert!(
        create.last_line().ends_with("/sessions/waiter"),
        "{}",
        create.stdout
    );
    let state = check_invariants(&env);
    assert!(
        state["checkpoints"].as_array().unwrap().is_empty(),
        "the stop hook checkpointed without the lock"
    );
}

// ---------------------------------------------------------------------------
// Crash leftovers
// ---------------------------------------------------------------------------

#[test]
fn stale_session_directory_is_refused_by_name_but_worktree_create_moves_on() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5668 + i as u16;
        let env = Env::new(port);
        env.init(backend);
        let repo = env.repo();
        let stale = env.project_dir().join("sessions/ghost");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("orphan.txt"), "do not delete\n").unwrap();

        let out = run_shoal(&env, &repo, &["new", "ghost"]);
        assert!(
            !out.status.success(),
            "[{backend}] new over a stale directory succeeded"
        );
        let err = stderr(&out);
        assert!(
            err.contains("left over") && err.contains("ghost"),
            "[{backend}] unclear message: {err}"
        );

        let r = hook_at(
            &env,
            "worktree-create",
            &json!({"name": "ghost", "cwd": repo}),
            &repo,
        );
        assert_eq!(r.code, 0, "[{backend}] {}", r.stderr);
        assert!(
            r.last_line().ends_with("/sessions/ghost-2"),
            "[{backend}] {}",
            r.stdout
        );

        let v = env.api("fork", json!({"name": "ghost"})).unwrap();
        assert_ne!(first_session(&v).0, "ghost");

        // The stale directory is untouched and still unrecorded.
        assert_eq!(read(stale.join("orphan.txt")), "do not delete\n");
        let state = check_invariants_in(&env.home(), &["ghost"]);
        assert!(state["sessions"].get("ghost").is_none());
    }
}

#[test]
fn stale_volume_directory_is_treated_as_taken() {
    let env = Env::new(5671);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let repo = env.repo();
    let stale = env.project_dir().join("volumes/vol");
    fs::create_dir_all(stale.join("upper")).unwrap();
    fs::write(stale.join("upper/orphan.txt"), "keep\n").unwrap();
    let out = run_shoal(&env, &repo, &["new", "vol"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("left over"), "{}", stderr(&out));
    let r = hook_at(
        &env,
        "worktree-create",
        &json!({"name": "vol", "cwd": repo}),
        &repo,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.last_line().ends_with("/sessions/vol-2"), "{}", r.stdout);
    assert_eq!(read(stale.join("upper/orphan.txt")), "keep\n");
    check_invariants_in(&env.home(), &["vol"]);
}

#[test]
fn parallel_worktree_creates_skip_several_stale_names() {
    let env = Env::new(5674);
    let backend = *backends().last().unwrap();
    env.init(backend);
    let repo = env.repo();
    for name in ["job", "job-2", "job-4"] {
        fs::create_dir_all(env.project_dir().join("sessions").join(name)).unwrap();
    }
    let results = par(4, |_| {
        hook_at(
            &env,
            "worktree-create",
            &json!({"name": "job", "cwd": repo}),
            &repo,
        )
    });
    for r in &results {
        assert_eq!(r.code, 0, "{}", r.stderr);
    }
    let state = check_invariants_in(&env.home(), &["job", "job-2", "job-4"]);
    let names: BTreeSet<String> = state["sessions"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        names,
        ["job-3", "job-5", "job-6", "job-7"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
}

#[test]
fn half_built_project_is_rebuilt_by_the_next_fork_from_main() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5677 + i as u16;
        let env = Env::new(port);
        commit_config(&env, backend, port);
        env.init(backend);
        let dir = env.project_dir();
        // A crash before state.json was written: the files are there, the state is not.
        fs::remove_file(dir.join("state.json")).unwrap();
        fs::write(dir.join("junk-from-crash"), "x").unwrap();
        fs::create_dir_all(dir.join("sessions/half")).unwrap();

        let status = env.api("status", json!({})).unwrap();
        assert_eq!(status["initialized"], false, "[{backend}] {status}");

        let v = env
            .api("fork", json!({"name": "after"}))
            .unwrap_or_else(|e| panic!("[{backend}] {e}"));
        let (_, path) = first_session(&v);
        assert!(path.join("src/app.txt").is_file());
        assert!(
            !dir.join("junk-from-crash").exists(),
            "[{backend}] half-built project not cleared"
        );
        let state = check_invariants(&env);
        assert_eq!(state["generations"].as_array().unwrap().len(), 1);
        assert_eq!(state["backend"], backend);
        assert_eq!(state["sessions"].as_object().unwrap().len(), 1);
    }
}

#[test]
fn half_built_project_is_rebuilt_once_by_parallel_hooks() {
    let env = Env::new(5680);
    let backend = *backends().last().unwrap();
    commit_config(&env, backend, 5680);
    env.init(backend);
    let dir = env.project_dir();
    fs::remove_file(dir.join("state.json")).unwrap();
    let repo = env.repo();
    let results = par(5, |k| {
        hook_at(
            &env,
            "worktree-create",
            &json!({"name": format!("h{k}"), "cwd": repo}),
            &repo,
        )
    });
    for r in &results {
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(Path::new(&r.last_line()).join("src/app.txt").is_file());
    }
    let state = check_invariants(&env);
    assert_eq!(state["sessions"].as_object().unwrap().len(), 5);
    assert_eq!(state["generations"].as_array().unwrap().len(), 1);
}

#[test]
fn leftover_temporary_state_file_does_not_break_the_project() {
    let env = Env::new(5683);
    env.init("clone");
    fs::write(env.project_dir().join("state.json.tmp"), "{ truncated").unwrap();
    let results = par(3, |k| env.api("fork", json!({"name": format!("t{k}")})));
    for r in &results {
        assert!(r.is_ok(), "{r:?}");
    }
    let state = check_invariants(&env);
    assert_eq!(state["sessions"].as_object().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// Unusual repositories and paths
// ---------------------------------------------------------------------------

#[test]
fn repository_path_with_spaces_and_unicode_works_end_to_end() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5686 + i as u16;
        let env = Env::new(port);
        let repo = env
            .root
            .path()
            .join("my projects – ü 日本")
            .join("app (v2) é");
        make_repo(&env, &repo);
        let out = run_shoal(
            &env,
            &repo,
            &[
                "init",
                "--backend",
                backend,
                "--copy",
                ".env",
                "--port-base",
                &port.to_string(),
            ],
        );
        assert!(out.status.success(), "[{backend}] init: {}", stderr(&out));
        let projects = project_dirs(&env.home());
        assert_eq!(projects.len(), 1);
        let pname = projects[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            pname.is_ascii() && !pname.contains(' '),
            "[{backend}] project dir name {pname:?}"
        );

        let results = par(3, |k| {
            api_at(&env, &repo, "fork", json!({"name": format!("u{k}")}))
        });
        for r in &results {
            assert!(r.is_ok(), "[{backend}] {r:?}");
        }
        let hook = hook_at(
            &env,
            "worktree-create",
            &json!({"name": "über task", "cwd": repo}),
            &repo,
        );
        assert_eq!(hook.code, 0, "[{backend}] {}", hook.stderr);
        assert!(
            hook.last_line().ends_with("/sessions/ber-task"),
            "[{backend}] {}",
            hook.stdout
        );

        let u0 = PathBuf::from(
            results[0].as_ref().unwrap()["sessions"][0]["path"]
                .as_str()
                .unwrap(),
        );
        assert_eq!(read(u0.join(".env")), "SECRET=1\n");
        fs::write(u0.join("ünï.txt"), "unicode\n").unwrap();
        api_at(&env, &repo, "checkpoint", json!({"path": u0.join("src")})).unwrap();
        let land = api_at(
            &env,
            &repo,
            "land",
            json!({"session": "u0", "commit_message": "unicode file", "test": ""}),
        )
        .unwrap_or_else(|e| panic!("[{backend}] {e}"));
        assert_eq!(land["status"], "landed", "[{backend}] {land}");
        assert_eq!(read(repo.join("ünï.txt")), "unicode\n");

        let status = api_at(&env, &repo, "status", json!({})).unwrap();
        assert_eq!(status["sessions"].as_array().unwrap().len(), 4);
        let removed = hook_at(
            &env,
            "worktree-remove",
            &json!({"worktree_path": hook.last_line()}),
            &repo,
        );
        assert_eq!(removed.code, 0, "[{backend}] {}", removed.stderr);
        check_invariants(&env);
    }
}

#[test]
fn shoal_home_with_spaces_works() {
    for (i, backend) in backends().into_iter().enumerate() {
        let port = 5689 + i as u16;
        let env = Env::new(port);
        let home = env.root.path().join("shoal home dir");
        let repo = env.repo();
        let run = |args: &[&str]| {
            cmd(&env, &repo)
                .env("SHOAL_HOME", &home)
                .args(args)
                .output()
                .unwrap()
        };
        let out = run(&[
            "init",
            "--backend",
            backend,
            "--port-base",
            &port.to_string(),
        ]);
        assert!(out.status.success(), "[{backend}] {}", stderr(&out));
        let outs = par(3, |k| {
            run(&["api", "fork", &json!({"name": format!("h{k}")}).to_string()])
        });
        for o in &outs {
            assert!(o.status.success(), "[{backend}] {}{}", stdout(o), stderr(o));
        }
        let o = run(&["api", "checkpoint", &json!({"session": "h0"}).to_string()]);
        assert!(o.status.success(), "[{backend}] {}", stdout(&o));
        check_invariants_in(&home, &[]);
        let _ = run(&["down"]);
    }
}

#[test]
fn detached_head_is_refused_with_a_clear_message() {
    let env = Env::new(5692);
    let repo = env.repo();
    commit_in(&env, &repo, "second.txt", "2\n", "second");
    env.git(&repo, &["checkout", "-q", "--detach", "HEAD~1"]);

    let out = run_shoal(&env, &repo, &["init", "--backend", "clone"]);
    assert!(!out.status.success(), "init on a detached HEAD succeeded");
    let err = stderr(&out);
    assert!(
        err.contains("detached HEAD") && err.contains("--branch"),
        "unclear message: {err}"
    );
    assert!(
        project_dirs(&env.home()).is_empty(),
        "a project dir was left behind: {:?}",
        project_dirs(&env.home())
    );

    // Automatic init (hooks, fork) refuses too, without crashing or leaving junk.
    let r = hook_at(
        &env,
        "worktree-create",
        &json!({"name": "d", "cwd": repo}),
        &repo,
    );
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(r.stderr.contains("detached HEAD"), "{}", r.stderr);
    assert!(r.stdout.trim().is_empty(), "printed a path: {}", r.stdout);
    let e = env.api("fork", json!({})).unwrap_err();
    assert!(e.contains("detached HEAD"), "{e}");
    assert!(project_dirs(&env.home()).is_empty());

    // With --branch it works, and tracks that branch.
    let out = run_shoal(
        &env,
        &repo,
        &["init", "--backend", "clone", "--branch", "main"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.state()["branch"], "main");
    let v = env.api("fork", json!({"name": "ok"})).unwrap();
    assert!(
        first_session(&v).1.join("second.txt").is_file(),
        "template is not main's tip"
    );
}

// Fixed bug: on a repository with no commits, `shoal init`, the WorktreeCreate
// hook and `fork` all fail with git's raw "fatal: ambiguous argument 'HEAD':
// unknown revision or path not in the working tree" from
// `git rev-parse --abbrev-ref HEAD`; ops::init's own "branch main has no
// commits" message is never reached. Expected: say the repository has no
// commits yet and to commit first.
#[test]
fn empty_repository_without_commits_is_refused_clearly() {
    let env = Env::new(5695);
    let repo = env.root.path().join("empty");
    fs::create_dir_all(&repo).unwrap();
    env.git(&repo, &["init", "-q", "-b", "main"]);
    let out = run_shoal(&env, &repo, &["init", "--backend", "clone"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        project_dirs(&env.home()).is_empty(),
        "left a project behind"
    );
    let r = hook_at(
        &env,
        "worktree-create",
        &json!({"name": "e", "cwd": repo}),
        &repo,
    );
    assert_eq!(r.code, 1);
    assert!(r.stdout.trim().is_empty());
    let e = api_at(&env, &repo, "fork", json!({})).unwrap_err();
    assert!(project_dirs(&env.home()).is_empty());
    for msg in [&err, &r.stderr, &e] {
        assert!(
            msg.contains("no commits"),
            "message does not say the repository has no commits: {msg}"
        );
    }
}

#[test]
fn non_git_directory_is_refused_clearly() {
    let env = Env::new(5698);
    let plain = env.root.path().join("plain dir");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("file.txt"), "x").unwrap();
    let out = run_shoal(&env, &plain, &["init", "--backend", "clone"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not inside a git repository"),
        "{}",
        stderr(&out)
    );
    let r = hook_at(
        &env,
        "worktree-create",
        &json!({"name": "n", "cwd": plain}),
        &plain,
    );
    assert_eq!(r.code, 1);
    assert!(r.stdout.trim().is_empty());
    assert!(
        r.stderr.contains("not inside a git repository"),
        "{}",
        r.stderr
    );
    let e = api_at(&env, &plain, "fork", json!({})).unwrap_err();
    assert!(e.contains("not inside a git repository"), "{e}");
    let s = api_at(&env, &plain, "status", json!({})).unwrap();
    assert_eq!(s["initialized"], false);
    // Turn hooks outside any repository stay silent and succeed.
    for event in ["stop", "session-start", "prompt"] {
        let r = hook_at(&env, event, &json!({"cwd": plain}), &plain);
        assert_eq!(r.code, 0, "{event}: {}", r.stderr);
        assert!(r.stdout.trim().is_empty(), "{event}: {}", r.stdout);
    }
    assert!(project_dirs(&env.home()).is_empty());
    assert!(!plain.join(".git").exists());
}
