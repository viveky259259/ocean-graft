//! Regression tests for the issues the last review round found in the
//! fixes before it: rewind safety-checkpoint pruning and in-place restore,
//! branch names when saving, checkpoints of a session whose files vanished,
//! land running out of time on a lock, and setup dying with shoal.
//! Ports 6100-6199.

mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fork(env: &Env, name: &str) -> PathBuf {
    let r = env.api("fork", json!({ "name": name })).unwrap();
    PathBuf::from(r["sessions"][0]["path"].as_str().unwrap())
}

fn labels(env: &Env, session: &str) -> Vec<String> {
    env.state()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["session"] == session)
        .map(|c| c["label"].as_str().unwrap().to_string())
        .collect()
}

fn rewind(env: &Env, session: &str, label: &str) -> Value {
    env.api("rewind", json!({"session": session, "label": label}))
        .unwrap_or_else(|e| panic!("rewind {session} to {label}: {e}"))
}

/// With ten safety checkpoints already kept, rewinding to the oldest one
/// restores it (it used to be pruned before it was copied). Pruning keeps
/// the newest ten and never a user's own label that merely starts alike.
#[test]
fn rewind_to_the_oldest_safety_checkpoint_restores_it() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(6100 + 20 * i as u16);
        env.init(backend);
        let s = fork(&env, "s");
        fs::write(s.join("v.txt"), "base\n").unwrap();
        env.api("checkpoint", json!({"session": "s", "label": "base"}))
            .unwrap();
        env.api(
            "checkpoint",
            json!({"session": "s", "label": "pre-rewind-good"}),
        )
        .unwrap();
        for n in 1..=10 {
            fs::write(s.join("v.txt"), format!("state {n}\n")).unwrap();
            let r = rewind(&env, "s", "base");
            assert_eq!(r["saved_as"], format!("pre-rewind-{n}"), "[{backend}] {r}");
        }
        assert_eq!(fs::read_to_string(s.join("v.txt")).unwrap(), "base\n");
        // pre-rewind-1 holds "state 1": the oldest kept safety checkpoint.
        let r = rewind(&env, "s", "pre-rewind-1");
        assert_eq!(r["label"], "pre-rewind-1", "[{backend}] {r}");
        assert_eq!(
            fs::read_to_string(s.join("v.txt")).unwrap(),
            "state 1\n",
            "[{backend}] the target was pruned before it was restored"
        );
        let l = labels(&env, "s");
        assert!(l.contains(&"base".to_string()), "[{backend}] {l:?}");
        assert!(
            l.contains(&"pre-rewind-good".to_string()),
            "[{backend}] user label pruned: {l:?}"
        );
        let safety = l
            .iter()
            .filter(|x| {
                x.strip_prefix("pre-rewind-")
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            })
            .count();
        assert!(safety <= 11, "[{backend}] {l:?}");
        assert!(
            l.contains(&"pre-rewind-11".to_string()),
            "[{backend}] {l:?}"
        );
        env.shoal(&["down"]);
    }
}

/// A session a process works in (Claude Code's shell) is rewound in place:
/// the overlay is not unmounted (that fails while busy) and the directory
/// is not swapped (the process would end up in a deleted one). Files,
/// HEAD, untracked and ignored files are restored, the process still sees
/// them through its cwd, and no safety checkpoints pile up on retries.
#[test]
fn rewind_of_a_busy_session_restores_in_place() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(6140 + 20 * i as u16);
        env.init(backend);
        let s = fork(&env, "busy");
        fs::write(s.join("kept.txt"), "checkpointed\n").unwrap();
        fs::create_dir_all(s.join("deps")).unwrap();
        fs::write(s.join("deps/lib.txt"), "dep v1\n").unwrap();
        env.git(&s, &["add", "kept.txt"]);
        env.git(
            &s,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "cp",
            ],
        );
        let head = env.git(&s, &["rev-parse", "HEAD"]).trim().to_string();
        env.api("checkpoint", json!({"session": "busy", "label": "cp"}))
            .unwrap();
        // Change everything after the checkpoint.
        fs::write(s.join("kept.txt"), "changed\n").unwrap();
        fs::write(s.join("new.txt"), "after\n").unwrap();
        fs::write(s.join("deps/lib.txt"), "dep v2\n").unwrap();
        fs::remove_file(s.join("README.md")).unwrap();
        env.git(&s, &["add", "-A"]);
        env.git(
            &s,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "later",
            ],
        );
        // A process sits in the session, as Claude Code's shell does.
        let mut shell = Command::new("sleep")
            .arg("60")
            .current_dir(&s)
            .spawn()
            .unwrap();
        let r = rewind(&env, "busy", "cp");
        assert_eq!(r["head"], head, "[{backend}] {r}");
        assert_eq!(
            fs::read_to_string(s.join("kept.txt")).unwrap(),
            "checkpointed\n"
        );
        assert!(
            !s.join("new.txt").exists(),
            "[{backend}] file made later survived"
        );
        assert_eq!(fs::read_to_string(s.join("README.md")).unwrap(), "demo\n");
        assert_eq!(
            fs::read_to_string(s.join("deps/lib.txt")).unwrap(),
            "dep v1\n"
        );
        assert_eq!(
            env.git(&s, &["status", "--porcelain"]).trim(),
            "",
            "[{backend}]"
        );
        // The process's cwd is still the live session, not a deleted copy.
        let cwd = fs::read_link(format!("/proc/{}/cwd", shell.id())).unwrap();
        assert!(
            !cwd.to_string_lossy().contains("(deleted)"),
            "[{backend}] {cwd:?}"
        );
        assert!(cwd.join("kept.txt").exists(), "[{backend}]");
        // And the rewind can be undone.
        let back = r["saved_as"].as_str().unwrap().to_string();
        rewind(&env, "busy", &back);
        assert_eq!(fs::read_to_string(s.join("new.txt")).unwrap(), "after\n");
        let _ = shell.kill();
        let _ = shell.wait();
        env.shoal(&["down"]);
    }
}

/// Branch names cut at a dot or ".lock" still make valid save branches, so
/// discard can save (and remove) the session.
#[test]
fn saving_branches_with_awkward_names_works() {
    let env = Env::new(6180);
    env.init("clone");
    let s = fork(&env, "s2");
    for b in [
        "abcdefghijklmnopqrstuvw.xyz",
        "release-candidate-2026.11.x",
        "abcdefghijklmnopqrs.lockxyz",
    ] {
        env.git(&s, &["switch", "-q", "-c", b]);
        fs::write(s.join(format!("{b}.txt")), "work\n").unwrap();
        env.git(&s, &["add", "-A"]);
        env.git(
            &s,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                b,
            ],
        );
        env.git(&s, &["switch", "-q", "shoal/s2"]);
    }
    let r = env.api("discard", json!({"session": "s2"})).unwrap();
    let extra = r["saved"]["extra_branches"].as_array().unwrap();
    assert_eq!(extra.len(), 3, "{r}");
    for b in extra {
        let b = b.as_str().unwrap();
        let ok = Command::new("git")
            .args(["check-ref-format", "--branch", b])
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "invalid branch {b}");
        assert!(!env.git(&env.repo(), &["rev-parse", b]).trim().is_empty());
    }
}

/// When a session's files are gone, removing it keeps its checkpoints (they
/// may be the only copy of its work) instead of deleting them.
#[test]
fn removing_a_vanished_session_keeps_its_checkpoints() {
    let env = Env::new(6185);
    env.init("clone");
    let s = fork(&env, "gone");
    fs::write(s.join("only-copy.txt"), "precious\n").unwrap();
    env.api("checkpoint", json!({"session": "gone", "label": "cp"}))
        .unwrap();
    fs::remove_dir_all(&s).unwrap();
    let r = env.hook(
        "worktree-remove",
        &json!({"worktree_path": s, "hook_event_name": "WorktreeRemove"}),
        &env.repo(),
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(env.state()["sessions"].get("gone").is_none());
    let kept: Vec<PathBuf> = fs::read_dir(env.project_dir().join("orphaned-checkpoints"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        fs::read_to_string(kept[0].join("cp/only-copy.txt")).unwrap(),
        "precious\n"
    );
}

/// Running out of time while waiting for another land's fast-forward is a
/// timed_out status, not a tool error.
#[test]
fn land_out_of_time_on_the_land_lock_is_timed_out() {
    use std::os::unix::io::AsRawFd;
    let env = Env::new(6190);
    env.init("clone");
    let s = fork(&env, "l");
    fs::write(s.join("f.txt"), "x\n").unwrap();
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.project_dir().join("land.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let started = Instant::now();
    let r = env
        .api(
            "land",
            json!({"session": "l", "commit_message": "c", "test": "", "timeout_secs": 2}),
        )
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(r["status"], "timed_out", "{r}");
    drop(lock);
    let r = env
        .api("land", json!({"session": "l", "test": ""}))
        .unwrap();
    assert_eq!(r["status"], "landed", "{r}");
}

/// Killing shoal while setup runs kills setup too (it used to run on,
/// orphaned, in its own process group).
#[test]
fn setup_dies_with_shoal() {
    let env = Env::new(6195);
    let marker = env.root.path().join("setup.pid");
    fs::write(
        env.repo().join("shoal.toml"),
        format!(
            "setup = \"echo $$ > '{}'; sleep 60\"\nbackend = \"clone\"\nport_base = 6195\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut child = env
        .command(BIN, &env.repo())
        .args(["api", "fork", "{}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    assert!(alive(pid));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "setup {pid} outlived shoal");
    let _: &Path = env.repo().as_path();
}
