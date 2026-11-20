//! A checkpoint of a session that is being written to: the session's own
//! services are paused while it is copied, and a writer ocean-graft did not start
//! is caught by comparing HEAD and the fingerprint around the copy.
//!
//! Port range 6200-6299. The one service here writes a file and listens on
//! nothing.
mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn ok(r: Result<Value, String>, what: &str) -> Value {
    r.unwrap_or_else(|e| panic!("{what} failed: {e}"))
}

fn session_path(env: &Env, name: &str) -> PathBuf {
    PathBuf::from(env.ocean_graft(&["path", name]).trim())
}

#[test]
fn a_session_that_never_stops_changing_still_gets_a_checkpoint_with_a_warning() {
    let env = Env::new(6200);
    env.init("clone");
    ok(env.api("fork", json!({"name": "busy"})), "fork");
    let path = session_path(&env, "busy");
    // Not a ocean-graft service: nothing to pause, so only the comparison sees it.
    let mut writer = Command::new("sh")
        .args(["-c", "i=0; while :; do i=$((i+1)); echo $i > churn; done"])
        .current_dir(&path)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.join("churn").exists() {
        assert!(Instant::now() < deadline, "the writer never started");
        std::thread::yield_now();
    }
    let r = env.api("checkpoint", json!({"session": "busy", "label": "moving"}));
    writer.kill().unwrap();
    writer.wait().unwrap();
    let r = ok(r, "checkpoint");
    assert!(
        r["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("kept changing"),
        "{r}"
    );
    assert_eq!(r["label"], "moving");
    // Kept without a fingerprint, so the next automatic checkpoint is
    // never skipped as "nothing changed".
    let state = env.state();
    let cp = state["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "moving")
        .expect("the checkpoint was recorded");
    assert!(cp["fingerprint"].is_null(), "{cp}");
    // And it is a usable checkpoint.
    ok(
        env.api("rewind", json!({"session": "busy", "label": "moving"})),
        "rewind",
    );
    assert!(path.join("churn").exists());
}

#[test]
fn a_quiet_session_gets_no_warning_and_one_copy() {
    let env = Env::new(6210);
    env.init("clone");
    ok(env.api("fork", json!({"name": "calm"})), "fork");
    fs::write(session_path(&env, "calm").join("new.txt"), "x\n").unwrap();
    let r = ok(
        env.api("checkpoint", json!({"session": "calm", "label": "still"})),
        "checkpoint",
    );
    assert!(r.get("warning").is_none(), "{r}");
    let state = env.state();
    let cp = state["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "still")
        .unwrap();
    assert!(cp["fingerprint"].is_string(), "{cp}");
    // Only the checkpoint itself is in the session's checkpoint directory:
    // no leftover from a discarded attempt.
    let dir = env.project_dir().join("checkpoints/calm");
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["still".to_string()], "{}", dir.display());
}

/// One letter of `ps -o stat`: T is stopped.
fn process_state(pid: i32) -> String {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_running_service_is_paused_for_the_copy_and_resumed_after() {
    // Pausing is only done where the copy is cheap: a full copy can take
    // minutes, and the user's preview would hang as long.
    let Some(backend) = cheap_backend() else {
        eprintln!("skipping: no backend with cheap checkpoints here");
        return;
    };
    let env = Env::new(6220);
    env.init(backend);
    ok(env.api("fork", json!({"name": "web"})), "fork");
    let path = session_path(&env, "web");
    env.ocean_graft(&[
        "serve",
        "web",
        "--as",
        "writer",
        "--",
        "sh",
        "-c",
        "i=0; while :; do i=$((i+1)); echo $i > counter; done",
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.join("counter").exists() {
        assert!(Instant::now() < deadline, "the service never started");
        std::thread::yield_now();
    }
    let pgid = env.state()["sessions"]["web"]["services"][0]["pid"]
        .as_i64()
        .expect("the service's process group") as i32;
    for round in 0..3 {
        let r = ok(
            env.api(
                "checkpoint",
                json!({"session": "web", "label": format!("cp{round}")}),
            ),
            "checkpoint",
        );
        assert!(r.get("warning").is_none(), "round {round}: {r}");
        // Running again, and still writing.
        assert!(
            !process_state(pgid).starts_with('T'),
            "round {round}: the service was left stopped ({})",
            process_state(pgid)
        );
        let before = read(path.join("counter"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while read(path.join("counter")) == before {
            assert!(
                Instant::now() < deadline,
                "round {round}: no writes after the checkpoint"
            );
            std::thread::yield_now();
        }
    }
    // A fork of the session pauses and resumes the service the same way.
    for round in 0..2 {
        ok(
            env.api(
                "fork",
                json!({"from": "web", "name": format!("child{round}")}),
            ),
            "fork",
        );
        assert!(
            !process_state(pgid).starts_with('T'),
            "fork {round}: the service was left stopped"
        );
        assert!(session_path(&env, &format!("child{round}"))
            .join("counter")
            .exists());
    }
    env.ocean_graft(&["stop", "web"]);
}
