//! Regression tests for the locking and robustness fixes of commit dda3697
//! (plugin/ARCHITECTURE.md rules 3 and 6, "Land, step by step"):
//!
//! - template refresh runs under its own lock, not the project lock: status
//!   and checkpoint answer while setup runs, concurrent forks build one new
//!   generation, gc leaves a generation being built alone;
//! - a failed refresh falls back to the last good template (fork and
//!   WorktreeCreate) and the next fork refreshes once it is fixed;
//! - the per-session land lock keeps rewind, discard and a second land off a
//!   session being landed, and only that session;
//! - timeout_secs bounds the whole land (queue wait and tests), kills the
//!   test's process group, and is clamped rather than rejected;
//! - ports are unique across projects and skip ports already bound;
//! - status and the Stop hook never fail on, or remove, an agent's
//!   .git/index.lock, and run git with GIT_OPTIONAL_LOCKS=0.
//!
//! Port range 5900-5999. Nothing here starts a service; the only listener is
//! the one `bound_port_is_skipped_by_fork` binds itself, at 5990, which no
//! other test here uses as a port_base. Ports are otherwise only recorded,
//! so a brief bind by shoal's port probe in one test can at most make
//! another test's fork skip a port; no test asserts an exact port except
//! relative to its own listener.

mod common;
use common::*;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
const LOCK_NB: i32 = 4;

/// `shoal init` with a fast setup whose input is deps.lock.
fn init_fast(env: &Env, backend: &str) {
    env.shoal(&[
        "init",
        "--backend",
        backend,
        "--setup",
        "mkdir -p deps && cat deps.lock > deps/installed.txt",
        "--setup-input",
        "deps.lock",
        "--port-base",
        &env.port_base.to_string(),
    ]);
}

fn commit_in(env: &Env, dir: &Path, file: &str, content: &str, msg: &str) {
    if let Some(parent) = dir.join(file).parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(dir.join(file), content).unwrap();
    env.git(dir, &["add", "-A"]);
    env.git(dir, &["commit", "-q", "-m", msg]);
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// Commit to main a shoal.toml whose setup appends a line to a marker file
/// (outside the repository), sleeps `secs` and then installs deps/fresh,
/// together with src/new.txt. Returns the marker path.
fn commit_slow_setup(env: &Env, secs: u32) -> PathBuf {
    let marker = env.root.path().join("setup-runs");
    let setup = format!(
        "echo run >> {}; sleep {secs}; mkdir -p deps && touch deps/fresh",
        sh_quote(&marker)
    );
    fs::write(
        env.repo().join("shoal.toml"),
        format!("setup = {}\n", toml_str(&setup)),
    )
    .unwrap();
    commit_in(env, &env.repo(), "src/new.txt", "new\n", "slow setup");
    marker
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap() // a JSON string is a valid TOML basic string
}

fn first_session(v: &Value) -> (String, PathBuf) {
    let s = &v["sessions"][0];
    (
        s["name"]
            .as_str()
            .unwrap_or_else(|| panic!("no session in {v}"))
            .to_string(),
        PathBuf::from(s["path"].as_str().unwrap()),
    )
}

fn fork(env: &Env, name: &str) -> PathBuf {
    first_session(&env.api("fork", json!({"name": name})).unwrap()).1
}

fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Gone, or a zombie nobody has reaped yet (it no longer runs).
fn pid_gone(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
    }
}

fn tip(env: &Env, branch: &str) -> String {
    env.git(&env.repo(), &["rev-parse", branch])
        .trim()
        .to_string()
}

/// The directory a refresh builds generation `id` in.
fn generation_dir(env: &Env, backend: &str, id: u32) -> PathBuf {
    let parent = if backend == "overlay" {
        "layers"
    } else {
        "templates"
    };
    env.project_dir().join(parent).join(format!("g{id}"))
}

fn generation_ids(state: &Value) -> Vec<u64> {
    state["generations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_u64().unwrap())
        .collect()
}

fn mounted(path: &Path) -> bool {
    let path = path.display().to_string();
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .any(|l| l.split(' ').nth(4) == Some(path.as_str()))
}

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

fn run_hook(
    env: &Env,
    event: &str,
    input: &Value,
    dir: &Path,
    extra: &[(&str, &str)],
) -> HookResult {
    let mut cmd = env.command(BIN, dir);
    cmd.args(["hook", event])
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input.to_string().as_bytes());
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    HookResult {
        code: out.status.code().unwrap_or(-1),
        stdout: stdout(&out),
        stderr: stderr(&out),
    }
}

/// `shoal api` in `dir` with extra environment variables.
fn api_env(
    env: &Env,
    dir: &Path,
    tool: &str,
    args: Value,
    extra: &[(&str, &str)],
) -> Result<Value, String> {
    let mut cmd = env.command(BIN, dir);
    cmd.args(["api", tool, &args.to_string()]);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
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

/// Hold queue slot `i` of this SHOAL_HOME (an exclusive flock, as a running
/// job does) until the returned file is dropped.
fn hold_slot(env: &Env, i: usize) -> fs::File {
    use std::os::unix::io::AsRawFd;
    let dir = env.home().join("queue");
    fs::create_dir_all(dir.join("waiting")).unwrap();
    let f = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("slot-{i}.lock")))
        .unwrap();
    assert_eq!(unsafe { c_flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) }, 0);
    f
}

// ---------------------------------------------------------------------------
// Refresh runs without the project lock
// ---------------------------------------------------------------------------

#[test]
fn status_and_checkpoint_answer_while_a_refresh_runs_setup() {
    for backend in backends() {
        let env = Env::new(5900);
        init_fast(&env, backend);
        fork(&env, "existing");
        let marker = commit_slow_setup(&env, 4);
        let head = tip(&env, "main");
        std::thread::scope(|sc| {
            let forking = sc.spawn(|| {
                let t = Instant::now();
                (env.api("fork", json!({"name": "fresh"})), t.elapsed())
            });
            assert!(
                wait_for(Duration::from_secs(20), || marker.exists()),
                "[{backend}] setup never started"
            );

            let t = Instant::now();
            let status = env.api("status", json!({}));
            let took = t.elapsed();
            let status =
                status.unwrap_or_else(|e| panic!("[{backend}] status failed during refresh: {e}"));
            assert!(
                took < Duration::from_secs(2),
                "[{backend}] status took {took:?} while a refresh ran setup (project lock held?)"
            );
            assert_eq!(status["template"]["generation"], 1, "[{backend}] {status}");
            assert_eq!(
                status["template"]["behind_source"], true,
                "[{backend}] {status}"
            );

            let t = Instant::now();
            let cp = env.api(
                "checkpoint",
                json!({"session": "existing", "label": "mid-refresh"}),
            );
            let took = t.elapsed();
            let cp =
                cp.unwrap_or_else(|e| panic!("[{backend}] checkpoint failed during refresh: {e}"));
            assert_eq!(cp["label"], "mid-refresh");
            assert!(
                took < Duration::from_secs(2),
                "[{backend}] checkpoint took {took:?}"
            );
            assert!(
                !forking.is_finished(),
                "[{backend}] the refresh finished before status/checkpoint did; the test proved nothing"
            );

            let (fork, took) = forking.join().unwrap();
            let fork = fork.unwrap_or_else(|e| panic!("[{backend}] fork failed: {e}"));
            assert!(
                took >= Duration::from_secs(3),
                "[{backend}] setup did not run: {took:?}"
            );
            assert!(fork["refresh_error"].is_null(), "[{backend}] {fork}");
            assert_eq!(fork["refreshed"]["setup_ran"], true, "[{backend}] {fork}");
            assert_eq!(fork["sessions"][0]["generation"], 2, "[{backend}] {fork}");
            let (_, path) = first_session(&fork);
            assert!(path.join("src/new.txt").is_file(), "[{backend}]");
            assert!(path.join("deps/fresh").is_file(), "[{backend}]");
        });
        let state = env.state();
        assert_eq!(generation_ids(&state), vec![1, 2], "[{backend}]");
        assert_eq!(state["generations"][1]["commit"], head.as_str());
        let cps: Vec<&str> = state["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap())
            .collect();
        assert_eq!(cps, vec!["mid-refresh"], "[{backend}]");
    }
}

#[test]
fn two_concurrent_forks_from_main_build_exactly_one_generation() {
    for backend in backends() {
        let env = Env::new(5920);
        init_fast(&env, backend);
        let marker = commit_slow_setup(&env, 2);
        let head = tip(&env, "main");
        let results = par(2, |k| env.api("fork", json!({"name": format!("f{k}")})));
        let mut refreshed = 0;
        for r in &results {
            let r = r
                .as_ref()
                .unwrap_or_else(|e| panic!("[{backend}] fork failed: {e}"));
            assert!(r["refresh_error"].is_null(), "[{backend}] {r}");
            if !r["refreshed"].is_null() {
                refreshed += 1;
            }
            assert_eq!(r["sessions"][0]["generation"], 2, "[{backend}] {r}");
            let (_, path) = first_session(r);
            assert!(path.join("src/new.txt").is_file() && path.join("deps/fresh").is_file());
        }
        assert_eq!(
            refreshed, 1,
            "[{backend}] exactly one fork reports the refresh"
        );
        assert_eq!(
            read(&marker).lines().count(),
            1,
            "[{backend}] setup ran more than once"
        );
        let state = env.state();
        assert_eq!(generation_ids(&state), vec![1, 2], "[{backend}]");
        assert_eq!(state["generations"][1]["commit"], head.as_str());
        assert!(
            !generation_dir(&env, backend, 3).exists(),
            "[{backend}] stray g3"
        );
    }
}

#[test]
fn gc_during_a_refresh_keeps_the_generation_being_built() {
    for backend in backends() {
        let env = Env::new(5940);
        init_fast(&env, backend);
        let marker = commit_slow_setup(&env, 3);
        std::thread::scope(|sc| {
            let forking = sc.spawn(|| env.api("fork", json!({"name": "fresh"})));
            assert!(
                wait_for(Duration::from_secs(20), || marker.exists()),
                "[{backend}] setup never started"
            );
            assert!(generation_dir(&env, backend, 2).is_dir(), "[{backend}]");
            let t = Instant::now();
            let gc = env.shoal(&["gc"]);
            assert!(
                t.elapsed() < Duration::from_secs(2),
                "[{backend}] gc waited for the refresh"
            );
            assert!(
                !gc.contains("g2"),
                "[{backend}] gc removed the generation being built:\n{gc}"
            );
            assert!(generation_dir(&env, backend, 2).is_dir(), "[{backend}]");
            assert!(
                !forking.is_finished(),
                "[{backend}] refresh finished before gc ran"
            );

            let fork = forking
                .join()
                .unwrap()
                .unwrap_or_else(|e| panic!("[{backend}] fork failed after gc: {e}"));
            assert!(fork["refresh_error"].is_null(), "[{backend}] {fork}");
            assert_eq!(fork["sessions"][0]["generation"], 2);
            let (_, path) = first_session(&fork);
            assert!(path.join("src/new.txt").is_file(), "[{backend}]");
            assert!(path.join("deps/fresh").is_file(), "[{backend}]");
        });
        let state = env.state();
        assert!(generation_ids(&state).contains(&2), "[{backend}] {state}");
        assert!(
            generation_dir(&env, backend, 2).join("deps/fresh").exists(),
            "[{backend}] g2 incomplete"
        );
        // The new generation is complete: another fork from main uses it.
        let again = env.api("fork", json!({"name": "again"})).unwrap();
        assert!(again["refreshed"].is_null(), "[{backend}] {again}");
        assert_eq!(again["sessions"][0]["generation"], 2);
        let (_, path) = first_session(&again);
        assert!(path.join("src/new.txt").is_file() && path.join("deps/fresh").is_file());
        // And a gc now (nothing building) keeps what sessions use.
        env.shoal(&["gc"]);
        let st = env.api("status", json!({})).unwrap();
        for s in st["sessions"].as_array().unwrap() {
            assert!(s["error"].is_null(), "[{backend}] {s}");
        }
    }
}

// ---------------------------------------------------------------------------
// A failed refresh falls back to the last good template
// ---------------------------------------------------------------------------

#[test]
fn failed_refresh_falls_back_and_the_next_fork_refreshes_once_fixed() {
    for backend in backends() {
        let env = Env::new(5960);
        init_fast(&env, backend);
        let repo = env.repo();
        fs::write(
            repo.join("shoal.toml"),
            "setup = \"echo setup-is-broken >&2; exit 3\"\n",
        )
        .unwrap();
        commit_in(&env, &repo, "src/new.txt", "new\n", "broken setup");

        let r = env
            .api("fork", json!({"name": "a"}))
            .unwrap_or_else(|e| panic!("[{backend}] fork failed instead of falling back: {e}"));
        let err = r["refresh_error"]
            .as_str()
            .unwrap_or_else(|| panic!("[{backend}] no refresh_error: {r}"));
        assert!(err.contains("setup"), "[{backend}] {err}");
        assert!(r["refreshed"].is_null(), "[{backend}] {r}");
        assert_eq!(r["sessions"][0]["generation"], 1, "[{backend}] {r}");
        let (_, a) = first_session(&r);
        assert!(a.join("src/app.txt").is_file());
        assert!(
            !a.join("src/new.txt").exists(),
            "[{backend}] fallback is not the last good template"
        );
        assert!(
            a.join("deps/installed.txt").is_file(),
            "[{backend}] last good template lost its deps"
        );

        // The failed build left nothing behind.
        assert!(
            !generation_dir(&env, backend, 2).exists(),
            "[{backend}] half-built g2 left"
        );
        assert!(
            !mounted(&env.project_dir().join("build")),
            "[{backend}] build still mounted"
        );

        let wt = run_hook(
            &env,
            "worktree-create",
            &json!({"name": "wt", "cwd": repo}),
            &repo,
            &[],
        );
        assert_eq!(
            wt.code, 0,
            "[{backend}] worktree-create failed: {}",
            wt.stderr
        );
        let wt_path = PathBuf::from(wt.last_line());
        assert!(
            wt_path.join("src/app.txt").is_file(),
            "[{backend}] {}",
            wt.stdout
        );
        assert!(!wt_path.join("src/new.txt").exists());
        assert!(
            wt.stderr.contains("could not refresh"),
            "[{backend}] {}",
            wt.stderr
        );

        let st = env.api("status", json!({})).unwrap();
        assert_eq!(st["template"]["generation"], 1);
        assert_eq!(st["template"]["behind_source"], true);

        // Fix the setup: the next fork refreshes.
        fs::write(
            repo.join("shoal.toml"),
            "setup = \"mkdir -p deps && touch deps/fixed\"\n",
        )
        .unwrap();
        commit_in(&env, &repo, "src/fixed.txt", "fixed\n", "fix setup");
        let head = tip(&env, "main");
        let r = env.api("fork", json!({"name": "b"})).unwrap();
        assert!(r["refresh_error"].is_null(), "[{backend}] {r}");
        assert_eq!(r["refreshed"]["setup_ran"], true, "[{backend}] {r}");
        assert_eq!(r["refreshed"]["to"], head.as_str(), "[{backend}] {r}");
        assert_eq!(r["sessions"][0]["generation"], 2, "[{backend}] {r}");
        let (_, b) = first_session(&r);
        for f in ["src/new.txt", "src/fixed.txt", "deps/fixed"] {
            assert!(
                b.join(f).exists(),
                "[{backend}] {f} missing after the fixed refresh"
            );
        }
        assert_eq!(
            env.api("status", json!({})).unwrap()["template"]["behind_source"],
            false
        );
        assert_eq!(generation_ids(&env.state()), vec![1, 2]);
    }
}

// ---------------------------------------------------------------------------
// The per-session land lock
// ---------------------------------------------------------------------------

#[test]
fn rewind_discard_and_a_second_land_fail_fast_while_a_session_is_landed() {
    for backend in backends() {
        let env = Env::new(5980);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        let o = fork(&env, "other");
        env.api("checkpoint", json!({"session": "s", "label": "cp"}))
            .unwrap();
        env.api("checkpoint", json!({"session": "other", "label": "cp"}))
            .unwrap();
        commit_in(&env, &s, "src/feature.txt", "feature\n", "feature");
        fs::write(o.join("scratch.txt"), "x\n").unwrap();
        let log = env.project_dir().join("logs/s-land-tests.log");
        std::thread::scope(|sc| {
            let landing =
                sc.spawn(|| env.api("land", json!({"session": "s", "test": "sleep 3; true"})));
            assert!(
                wait_for(Duration::from_secs(20), || log.exists()),
                "[{backend}] the land's tests never started"
            );
            let fast = |what: &str, r: Result<Value, String>, t: Instant, want: &str| {
                let took = t.elapsed();
                let err = r.err().unwrap_or_else(|| {
                    panic!("[{backend}] {what} of the landing session succeeded")
                });
                assert!(err.contains(want), "[{backend}] {what}: {err}");
                assert!(
                    took < Duration::from_millis(1500),
                    "[{backend}] {what} took {took:?}"
                );
            };
            let t = Instant::now();
            fast(
                "rewind",
                env.api("rewind", json!({"session": "s", "label": "cp"})),
                t,
                "being landed",
            );
            let t = Instant::now();
            fast(
                "discard",
                env.api("discard", json!({"session": "s"})),
                t,
                "being landed",
            );
            let t = Instant::now();
            fast(
                "discard keep_work=false",
                env.api("discard", json!({"session": "s", "keep_work": false})),
                t,
                "being landed",
            );
            let t = Instant::now();
            fast(
                "second land",
                env.api("land", json!({"session": "s", "test": ""})),
                t,
                "already being landed",
            );
            let wr = run_hook(
                &env,
                "worktree-remove",
                &json!({"worktree_path": s}),
                &env.repo(),
                &[],
            );
            assert_eq!(
                wr.code, 1,
                "[{backend}] worktree-remove of a landing session: {}",
                wr.stderr
            );
            assert!(
                wr.stderr.contains("being landed"),
                "[{backend}] {}",
                wr.stderr
            );
            assert!(
                s.join("src/feature.txt").is_file(),
                "[{backend}] the landing session was touched"
            );

            // Other sessions are unaffected.
            let rw = env
                .api("rewind", json!({"session": "other", "label": "cp"}))
                .unwrap_or_else(|e| panic!("[{backend}] rewind of another session: {e}"));
            assert_eq!(rw["label"], "cp");
            assert!(!o.join("scratch.txt").exists());
            env.api("checkpoint", json!({"session": "other"})).unwrap();
            env.api("discard", json!({"session": "other", "keep_work": false}))
                .unwrap_or_else(|e| panic!("[{backend}] discard of another session: {e}"));
            assert!(
                !landing.is_finished(),
                "[{backend}] the land ended before the checks; they proved nothing"
            );
            let land = landing.join().unwrap().unwrap();
            assert_eq!(land["status"], "landed", "[{backend}] {land}");
        });
        assert!(env.repo().join("src/feature.txt").is_file());
        // After the land, rewind and discard work again.
        let rw = env
            .api("rewind", json!({"session": "s", "label": "cp"}))
            .unwrap_or_else(|e| panic!("[{backend}] rewind after the land: {e}"));
        assert!(rw["saved_as"].as_str().is_some(), "[{backend}] {rw}");
        assert!(!s.join("src/feature.txt").exists());
        env.api("discard", json!({"session": "s"}))
            .unwrap_or_else(|e| panic!("[{backend}] discard after the land: {e}"));
        assert!(env.state()["sessions"].as_object().unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// The land deadline
// ---------------------------------------------------------------------------

#[test]
fn land_timeout_bounds_the_tests_and_kills_their_process_group() {
    for backend in backends() {
        let env = Env::new(5910);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        commit_in(&env, &s, "src/feature.txt", "feature\n", "feature");
        let before = tip(&env, "main");
        let bg = env.root.path().join("bg.pid");
        let fg = env.root.path().join("fg.pid");
        let test = format!(
            "sleep 30 & echo $! > {}; echo $$ > {}; sleep 30",
            sh_quote(&bg),
            sh_quote(&fg)
        );
        let t = Instant::now();
        let r = env
            .api(
                "land",
                json!({"session": "s", "test": test, "timeout_secs": 2}),
            )
            .unwrap();
        let took = t.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "[{backend}] land took {took:?} with timeout_secs 2"
        );
        assert_eq!(r["status"], "timed_out", "[{backend}] {r}");
        assert_eq!(r["tests"]["timed_out"], true, "[{backend}] {r}");
        assert_eq!(r["tests"]["passed"], false);
        for f in [&bg, &fg] {
            let pid: i32 = read(f).trim().parse().unwrap();
            assert!(
                wait_for(Duration::from_secs(3), || pid_gone(pid)),
                "[{backend}] test process {pid} survived the land deadline"
            );
        }
        assert_eq!(tip(&env, "main"), before, "[{backend}] main moved");
    }
}

#[test]
fn land_timeout_above_the_maximum_is_clamped_and_the_land_runs() {
    for backend in backends() {
        let env = Env::new(5930);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        commit_in(&env, &s, "src/feature.txt", "feature\n", "feature");
        for (i, secs) in [5000u64, 1501, u32::MAX as u64].into_iter().enumerate() {
            if i > 0 {
                commit_in(
                    &env,
                    &s,
                    "src/feature.txt",
                    &format!("feature {i}\n"),
                    "more",
                );
            }
            let r = env
                .api(
                    "land",
                    json!({"session": "s", "test": "true", "timeout_secs": secs}),
                )
                .unwrap_or_else(|e| panic!("[{backend}] timeout_secs {secs} was rejected: {e}"));
            assert_eq!(
                r["status"], "landed",
                "[{backend}] timeout_secs {secs}: {r}"
            );
            assert_eq!(r["tests"]["passed"], true, "[{backend}] {r}");
        }
        assert_eq!(read(env.repo().join("src/feature.txt")), "feature 2\n");
    }
}

#[test]
fn land_deadline_bounds_the_wait_for_a_queue_slot() {
    for backend in backends() {
        let env = Env::new(5950);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        commit_in(&env, &s, "src/feature.txt", "feature\n", "feature");
        let before = tip(&env, "main");
        env.shoal(&["queue", "--slots", "2"]);
        let slots = [hold_slot(&env, 0), hold_slot(&env, 1)];

        // Every slot is held: the land gives up at its deadline.
        let t = Instant::now();
        let r = env
            .api(
                "land",
                json!({"session": "s", "test": "true", "timeout_secs": 2}),
            )
            .unwrap();
        let took = t.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "[{backend}] queue wait not bounded: {took:?}"
        );
        assert!(
            took >= Duration::from_millis(1500),
            "[{backend}] gave up early: {took:?}"
        );
        assert_eq!(r["status"], "timed_out", "[{backend}] {r}");
        assert_eq!(r["tests"]["timed_out"], true, "[{backend}] {r}");
        let tail = r["tests"]["output_tail"].as_str().unwrap_or_default();
        assert!(tail.contains("queue slot"), "[{backend}] {tail}");
        assert_eq!(tip(&env, "main"), before);
        let waiting: Vec<_> = fs::read_dir(env.home().join("queue/waiting"))
            .unwrap()
            .flatten()
            .collect();
        assert!(
            waiting.is_empty(),
            "[{backend}] queue entry left behind: {waiting:?}"
        );

        // One deadline for the whole land: 2 s waiting for a slot plus a test
        // that would run 30 s, with timeout_secs 3, ends after about 3 s.
        let r = std::thread::scope(|sc| {
            let landing = sc.spawn(|| {
                let t = Instant::now();
                let r = env.api(
                    "land",
                    json!({"session": "s", "test": "sleep 30", "timeout_secs": 3}),
                );
                (r, t.elapsed())
            });
            std::thread::sleep(Duration::from_secs(2));
            drop(slots);
            landing.join().unwrap()
        });
        let (r, took) = r;
        let r = r.unwrap();
        assert_eq!(r["status"], "timed_out", "[{backend}] {r}");
        assert_eq!(r["tests"]["timed_out"], true, "[{backend}] {r}");
        assert!(
            took < Duration::from_millis(4300),
            "[{backend}] the queue wait and the tests each got the full timeout: {took:?}"
        );
        let ran = r["tests"]["duration_ms"].as_u64().unwrap();
        assert!(
            ran < 2000,
            "[{backend}] the tests ran {ran} ms after a 2 s wait with a 3 s limit"
        );

        // With the slots free the land runs at once.
        let r = env
            .api(
                "land",
                json!({"session": "s", "test": "true", "timeout_secs": 30}),
            )
            .unwrap();
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
    }
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// A second repository at `<root>/repo2` sharing the Env's SHOAL_HOME.
fn second_repo(env: &Env, backend: &str) -> PathBuf {
    let repo2 = env.root.path().join("repo2");
    fs::create_dir_all(repo2.join("src")).unwrap();
    fs::write(repo2.join("src/app.txt"), "other\n").unwrap();
    env.git(&repo2, &["init", "-q", "-b", "main"]);
    env.git(&repo2, &["add", "-A"]);
    env.git(&repo2, &["commit", "-q", "-m", "initial"]);
    let out = env.shoal_in(
        &repo2,
        &[
            "init",
            "--backend",
            backend,
            "--port-base",
            &env.port_base.to_string(),
        ],
    );
    assert!(out.status.success(), "init repo2: {}", stderr(&out));
    repo2
}

fn all_ports(env: &Env) -> Vec<u64> {
    let mut ports = Vec::new();
    for e in fs::read_dir(env.home().join("projects")).unwrap().flatten() {
        let state = e.path().join("state.json");
        if state.exists() {
            let v: Value = serde_json::from_str(&read(&state)).unwrap();
            for s in v["sessions"].as_object().unwrap().values() {
                ports.push(s["port"].as_u64().unwrap());
            }
        }
    }
    ports
}

#[test]
fn two_repositories_with_the_same_port_base_never_share_a_port() {
    for backend in backends() {
        let env = Env::new(5900);
        init_fast(&env, backend);
        let repo2 = second_repo(&env, backend);
        let mut seen = BTreeSet::new();
        for k in 0..2 {
            for dir in [env.repo(), repo2.clone()] {
                let r = env
                    .api_in(&dir, "fork", json!({"name": format!("s{k}")}))
                    .unwrap();
                let port = r["sessions"][0]["port"].as_u64().unwrap();
                assert!(
                    seen.insert(port),
                    "[{backend}] port {port} handed out twice: {seen:?}"
                );
                assert!(
                    port >= 5900 && (port - 5900).is_multiple_of(10),
                    "[{backend}] {port}"
                );
            }
        }
        assert_eq!(all_ports(&env).len(), 4);
    }
}

// Fixed bug: ports are unique across projects only when forks run one after the
// other. Project::next_port reads the other projects' state.json without
// their locks, and create_session saves the new session only after copying
// the template, so two forks in two repositories with the same port_base
// running at the same time both pick the same free port.
#[test]
fn concurrent_forks_in_two_repositories_never_share_a_port() {
    for backend in backends() {
        let env = Env::new(5920);
        init_fast(&env, backend);
        let repo2 = second_repo(&env, backend);
        let repos = [env.repo(), repo2];
        for round in 0..3 {
            par(2, |k| {
                env.api_in(&repos[k], "fork", json!({"name": format!("r{round}")}))
                    .unwrap_or_else(|e| panic!("[{backend}] fork: {e}"))
            });
        }
        let ports = all_ports(&env);
        let distinct: BTreeSet<u64> = ports.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            ports.len(),
            "[{backend}] two projects were handed the same port: {ports:?}"
        );
    }
}

#[test]
fn bound_port_is_skipped_by_fork() {
    for backend in backends() {
        let env = Env::new(5990);
        init_fast(&env, backend);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 5990))
            .expect("port 5990 is free for this test");
        let r = env.api("fork", json!({"name": "s"})).unwrap();
        let port = r["sessions"][0]["port"].as_u64().unwrap();
        assert_ne!(
            port, 5990,
            "[{backend}] fork handed out a port another process listens on"
        );
        assert!(port > 5990 && (port - 5990).is_multiple_of(10), "[{backend}] {port}");
        drop(listener);
        // Once free, the next session gets it.
        let r = env.api("fork", json!({"name": "t"})).unwrap();
        assert_eq!(r["sessions"][0]["port"], 5990, "[{backend}] {r}");
    }
}

// ---------------------------------------------------------------------------
// GIT_OPTIONAL_LOCKS: the agent's index.lock
// ---------------------------------------------------------------------------

#[test]
fn status_and_stop_hook_leave_an_agents_index_lock_alone() {
    for backend in backends() {
        let env = Env::new(5970);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        fork(&env, "t"); // two sessions, so status also runs the overlap check
        fs::write(s.join("src/app.txt"), "changed by the agent\n").unwrap();
        fs::write(s.join("src/untracked.txt"), "new\n").unwrap();
        let lock = s.join(".git/index.lock");
        fs::write(&lock, b"agent-held").unwrap();

        // A git that logs GIT_OPTIONAL_LOCKS for every call shoal makes.
        let fakebin = env.root.path().join("fakebin");
        fs::create_dir_all(&fakebin).unwrap();
        let log = env.root.path().join("git-calls.log");
        let real_git = String::from_utf8(
            std::process::Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let wrapper = fakebin.join("git");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s|%s\\n' \"${{GIT_OPTIONAL_LOCKS-unset}}\" \"$*\" >> {}\nexec {} \"$@\"\n",
                sh_quote(&log),
                sh_quote(Path::new(&real_git))
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = format!(
            "{}:{}",
            fakebin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let extra = [("PATH", path_var.as_str())];

        let st = api_env(&env, &env.repo(), "status", json!({}), &extra)
            .unwrap_or_else(|e| panic!("[{backend}] status failed with index.lock held: {e}"));
        let sessions = st["sessions"].as_array().unwrap();
        let me = sessions.iter().find(|x| x["name"] == "s").unwrap();
        assert!(me["error"].is_null(), "[{backend}] {me}");
        assert_eq!(me["uncommitted_count"], 2, "[{backend}] {me}");
        assert!(st["overlaps"]["error"].is_null(), "[{backend}] {st}");
        assert_eq!(
            read(&lock),
            "agent-held",
            "[{backend}] status touched index.lock"
        );

        let auto = st["auto_checkpoints"].as_bool().unwrap();
        let hook = run_hook(&env, "stop", &json!({"cwd": s}), &s, &extra);
        assert_eq!(hook.code, 0, "[{backend}] {}", hook.stderr);
        assert!(
            !hook.stderr.contains("shoal hook stop:"),
            "[{backend}] stop failed: {}",
            hook.stderr
        );
        assert_eq!(
            read(&lock),
            "agent-held",
            "[{backend}] the stop hook touched index.lock"
        );
        let labels: Vec<String> = env.state()["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["session"] == "s")
            .map(|c| c["label"].as_str().unwrap().to_string())
            .collect();
        if auto {
            assert_eq!(
                labels,
                vec!["turn-1"],
                "[{backend}] stop hook took no checkpoint"
            );
        } else {
            assert!(labels.is_empty(), "[{backend}] {labels:?}");
        }
        let cp = api_env(
            &env,
            &env.repo(),
            "checkpoint",
            json!({"session": "s"}),
            &extra,
        )
        .unwrap_or_else(|e| panic!("[{backend}] checkpoint failed with index.lock held: {e}"));
        assert!(cp["label"].is_string());
        api_env(&env, &env.repo(), "conflicts", json!({}), &extra)
            .unwrap_or_else(|e| panic!("[{backend}] conflicts failed with index.lock held: {e}"));
        assert_eq!(read(&lock), "agent-held");

        let calls = read(&log);
        assert!(
            calls.lines().count() > 3,
            "[{backend}] the git wrapper was not used:\n{calls}"
        );
        let without: Vec<&str> = calls
            .lines()
            .filter(|l| !l.starts_with("0|"))
            .filter(|l| !l.ends_with("|--version"))
            .collect();
        assert!(
            without.is_empty(),
            "[{backend}] git ran without GIT_OPTIONAL_LOCKS=0:\n{}",
            without.join("\n")
        );

        // The agent releases its lock and commits normally.
        fs::remove_file(&lock).unwrap();
        env.git(&s, &["add", "-A"]);
        env.git(&s, &["commit", "-q", "-m", "agent work"]);
        assert!(!lock.exists());
    }
}

/// One MCP server: a land runs on its own thread, so status answers at once
/// and a rewind of the landing session is refused (isError) while it runs.
#[test]
fn mcp_server_answers_status_and_refuses_rewind_during_a_land() {
    for backend in backends() {
        let env = Env::new(5935);
        init_fast(&env, backend);
        let s = fork(&env, "s");
        env.api("checkpoint", json!({"session": "s", "label": "cp"}))
            .unwrap();
        commit_in(&env, &s, "src/feature.txt", "feature\n", "feature");
        let log = env.project_dir().join("logs/s-land-tests.log");
        let mut mcp = env.mcp();
        mcp.initialize();
        mcp.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "land", "arguments": {"session": "s", "test": "sleep 3; true"}}}));
        assert!(
            wait_for(Duration::from_secs(20), || log.exists()),
            "[{backend}] the land's tests never started"
        );
        let t = Instant::now();
        mcp.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "status", "arguments": {}}}));
        let status = mcp.response(&json!(2), Duration::from_secs(2));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "[{backend}] status waited for the land"
        );
        assert_ne!(status["result"]["isError"], true, "[{backend}] {status}");
        mcp.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "rewind", "arguments": {"session": "s", "label": "cp"}}}));
        let rewind = mcp.response(&json!(3), Duration::from_secs(2));
        assert_eq!(rewind["result"]["isError"], true, "[{backend}] {rewind}");
        let text = rewind["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("being landed"), "[{backend}] {text}");
        let land = mcp.response(&json!(1), Duration::from_secs(30));
        let text = land["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        let report: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("[{backend}] land result is not JSON ({e}): {land}"));
        assert_eq!(report["status"], "landed", "[{backend}] {report}");
        assert!(mcp.close().success());
        assert!(s.join("src/feature.txt").is_file());
    }
}
