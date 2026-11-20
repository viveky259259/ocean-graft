//! Full copies (the clone backend on a filesystem that cannot clone) run
//! without the project lock, so other sessions' commands are not held up;
//! a fork of a session that keeps changing is verified like a checkpoint.
//!
//! OCEAN_GRAFT_TEST_COPY_DELAY_MS makes an unlocked copy take that long, so what
//! happens meanwhile can be observed. Where copies are cheap (APFS, a
//! reflink filesystem) nothing is copied unlocked, and those tests skip.
//!
//! Port range 6300-6399. Nothing here listens on a port.
mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn ok(r: Result<Value, String>, what: &str) -> Value {
    r.unwrap_or_else(|e| panic!("{what} failed: {e}"))
}

/// A clone project; None where its copies are cheap (nothing to test).
fn slow_project(port: u16) -> Option<Env> {
    let env = Env::new(port);
    env.init("clone");
    ok(env.api("fork", json!({"name": "a"})), "fork a");
    let marker = env.project_dir().join(".reflink");
    if read(&marker).trim() == "yes" {
        eprintln!("skipping: copies are cheap here, so none runs without the lock");
        return None;
    }
    Some(env)
}

/// `ocean-graft api <tool> <args>` in the background, its unlocked copy slowed.
fn spawn_slow(env: &Env, tool: &str, args: Value, delay_ms: u64) -> Child {
    env.command(BIN, &env.repo())
        .args(["api", tool, &args.to_string()])
        .env("OCEAN_GRAFT_TEST_COPY_DELAY_MS", delay_ms.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finish(child: Child) -> Result<Value, String> {
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let v: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {text} {}", stderr(&out)));
    if out.status.success() {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or_default().to_string())
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pending(env: &Env) -> Vec<String> {
    env.state()["pending"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| c["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed())
}

#[test]
fn other_commands_run_while_a_slow_fork_copies() {
    let Some(env) = slow_project(6300) else {
        return;
    };
    ok(env.api("fork", json!({"name": "b"})), "fork b");
    let slow = spawn_slow(&env, "fork", json!({"name": "slow"}), 4000);
    wait_until("the pending entry", || pending(&env) == ["slow"]);

    // The project is not locked meanwhile.
    let (status, took) = timed(|| ok(env.api("status", json!({})), "status"));
    assert!(took < Duration::from_secs(2), "status took {took:?}");
    let names: Vec<&str> = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"], "a pending session is not listed yet");
    let (_, took) = timed(|| {
        ok(
            env.api("checkpoint", json!({"session": "b"})),
            "checkpoint b",
        )
    });
    assert!(took < Duration::from_secs(3), "checkpoint b took {took:?}");

    // Its name is taken: asked for exactly, an error; as a base, the next.
    let err = env
        .api("fork", json!({"from": "a", "name": "slow"}))
        .unwrap_or_default();
    // The fork tool makes names unique, so this one becomes slow-2.
    assert_eq!(err["sessions"][0]["name"], "slow-2", "{err}");
    let out = env.ocean_graft_in(&env.repo(), &["new", "slow"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("is being created"),
        "{}",
        stderr(&out)
    );

    let r = ok(finish(slow), "the slow fork");
    assert_eq!(r["sessions"][0]["name"], "slow");
    assert!(pending(&env).is_empty(), "{:?}", pending(&env));
    let path = PathBuf::from(r["sessions"][0]["path"].as_str().unwrap());
    assert_eq!(read(path.join("README.md")), "demo\n");
    let state = env.state();
    assert!(state["sessions"]["slow"].is_object());
}

#[test]
fn operations_on_a_session_being_copied_wait_for_the_copy() {
    let Some(env) = slow_project(6310) else {
        return;
    };
    let a = PathBuf::from(env.ocean_graft(&["path", "a"]).trim());
    fs::write(a.join("note.txt"), "before\n").unwrap();
    // A slow checkpoint of a holds a's session lock, not the project lock.
    let slow = spawn_slow(
        &env,
        "checkpoint",
        json!({"session": "a", "label": "slow"}),
        3000,
    );
    let lock = env.project_dir().join("locks/a.lock");
    wait_until("the checkpoint to hold a's lock", || {
        fs::read_to_string(&lock).unwrap_or_default().trim() == "checkpoint"
    });
    let (_, took) = timed(|| ok(env.api("status", json!({})), "status"));
    assert!(took < Duration::from_secs(2), "status took {took:?}");
    // Another checkpoint and a fork of a wait for it, then succeed.
    let (results, took) = timed(|| {
        std::thread::scope(|sc| {
            let cp = sc.spawn(|| env.api("checkpoint", json!({"session": "a", "label": "next"})));
            let fork = sc.spawn(|| env.api("fork", json!({"from": "a", "name": "child"})));
            (cp.join().unwrap(), fork.join().unwrap())
        })
    });
    ok(results.0, "the waiting checkpoint");
    let fork = ok(results.1, "the waiting fork");
    assert!(
        took >= Duration::from_millis(1500),
        "they did not wait: {took:?}"
    );
    ok(finish(slow), "the slow checkpoint");
    let child = PathBuf::from(fork["sessions"][0]["path"].as_str().unwrap());
    assert_eq!(read(child.join("note.txt")), "before\n");
    let labels: Vec<String> = env.state()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["session"] == "a")
        .map(|c| c["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(labels.len(), 2, "{labels:?}");
    assert!(labels.contains(&"slow".into()) && labels.contains(&"next".into()));
    // Released: the lock file says nothing any more.
    assert_eq!(read(&lock), "");
}

#[test]
fn a_land_waits_for_a_copy_of_its_session_instead_of_failing() {
    let Some(env) = slow_project(6320) else {
        return;
    };
    // A slow fork of a holds a's lock: a land of a waits for it (within
    // its deadline) instead of reporting "already being landed".
    let slow = spawn_slow(&env, "fork", json!({"from": "a", "name": "copy"}), 2500);
    let lock = env.project_dir().join("locks/a.lock");
    wait_until("the fork to hold a's lock", || {
        fs::read_to_string(&lock).unwrap_or_default().trim() == "fork"
    });
    let a = PathBuf::from(env.ocean_graft(&["path", "a"]).trim());
    let r = env.api("land", json!({"session": "a", "test": ""}));
    let r = ok(r, "land after the copy");
    assert_eq!(r["status"], "nothing_to_land", "{r}");
    ok(finish(slow), "the slow fork");
    assert!(a.is_dir());
}

#[test]
fn an_abandoned_copy_is_cleaned_up_by_the_next_command() {
    let Some(env) = slow_project(6330) else {
        return;
    };
    let mut slow = spawn_slow(&env, "fork", json!({"name": "lost"}), 30_000);
    wait_until("the pending entry", || pending(&env) == ["lost"]);
    slow.kill().unwrap();
    slow.wait().unwrap();
    let partial = env.project_dir().join("sessions/lost");
    // The next command that opens the project removes the entry and the
    // partial directory, and the name is free again.
    ok(env.api("status", json!({})), "status");
    assert!(pending(&env).is_empty(), "{:?}", pending(&env));
    assert!(!partial.exists(), "the partial copy was left");
    let r = ok(env.api("fork", json!({"name": "lost"})), "fork lost again");
    assert_eq!(r["sessions"][0]["name"], "lost");
}

#[test]
fn gc_keeps_the_template_a_pending_copy_reads() {
    let Some(env) = slow_project(6340) else {
        return;
    };
    let slow = spawn_slow(&env, "fork", json!({"name": "late"}), 3000);
    wait_until("the pending entry", || pending(&env) == ["late"]);
    // A new commit and a refresh make another generation the latest; gc
    // must not remove the one being copied from.
    fs::write(env.repo().join("README.md"), "demo 2\n").unwrap();
    env.git(&env.repo(), &["commit", "-qam", "two"]);
    env.ocean_graft(&["refresh"]);
    env.ocean_graft(&["gc"]);
    let r = ok(finish(slow), "the slow fork");
    let path = PathBuf::from(r["sessions"][0]["path"].as_str().unwrap());
    assert_eq!(read(path.join("README.md")), "demo\n");
}

#[test]
fn a_fork_of_a_session_that_never_stops_changing_comes_with_a_warning() {
    for backend in backends() {
        let env = Env::new(6350 + if backend == "clone" { 0 } else { 10 });
        env.init(backend);
        ok(env.api("fork", json!({"name": "busy"})), "fork busy");
        let path = PathBuf::from(env.ocean_graft(&["path", "busy"]).trim());
        let mut writer = Command::new("sh")
            .args(["-c", "i=0; while :; do i=$((i+1)); echo $i > churn; done"])
            .current_dir(&path)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("the writer", || path.join("churn").exists());
        let r = env.api("fork", json!({"from": "busy", "name": "snap"}));
        writer.kill().unwrap();
        writer.wait().unwrap();
        let r = ok(r, "fork");
        let s = &r["sessions"][0];
        assert!(
            s["warning"]
                .as_str()
                .unwrap_or_default()
                .contains("kept changing"),
            "[{backend}] {r}"
        );
        assert!(PathBuf::from(s["path"].as_str().unwrap())
            .join("churn")
            .exists());

        // A quiet session forks without one.
        let r = ok(
            env.api("fork", json!({"from": "busy", "name": "calm"})),
            "fork",
        );
        assert!(r["sessions"][0].get("warning").is_none(), "[{backend}] {r}");
    }
}
