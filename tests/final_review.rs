//! Regression tests for the issues the last review round found in the
//! fixes before it: rewind safety-checkpoint pruning and in-place restore,
//! branch names when saving, checkpoints of a session whose files vanished,
//! land running out of time on a lock, and setup dying with ocean-graft.
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
        env.ocean_graft(&["down"]);
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
        let cwd = process_cwd(shell.id()).expect("the shell's cwd");
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
        env.ocean_graft(&["down"]);
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
        env.git(&s, &["switch", "-q", "ocean-graft/s2"]);
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

/// Killing ocean-graft while setup runs kills setup too (it used to run on,
/// orphaned, in its own process group).
#[test]
fn setup_dies_with_ocean_graft() {
    let env = Env::new(6195);
    let marker = env.root.path().join("setup.pid");
    fs::write(
        env.repo().join("ocean-graft.toml"),
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
    assert!(!alive(pid), "setup {pid} outlived ocean-graft");
    let _: &Path = env.repo().as_path();
}

// ---------------------------------------------------------------------------
// Round 5: in-place rewind fidelity, safety checkpoints, sweep marking
// ---------------------------------------------------------------------------

fn set_mtime(path: &Path, secs: i64) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let t = [
        libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), t.as_ptr(), 0) },
        0
    );
}

/// npm stamps every file with one fixed time: a file whose size, mtime and
/// mode match the checkpoint but whose bytes differ must still be restored.
#[test]
fn in_place_rewind_compares_contents_not_just_metadata() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(6150 + 4 * i as u16);
        env.init(backend);
        let s = fork(&env, "pkg");
        let f = s.join("deps/foo/package.json");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, "{\"version\":\"1.0.0\"}\n").unwrap();
        set_mtime(&f, 499162500);
        env.api("checkpoint", json!({"session": "pkg", "label": "v1"}))
            .unwrap();
        fs::write(&f, "{\"version\":\"1.0.1\"}\n").unwrap(); // same size
        set_mtime(&f, 499162500);
        let mut shell = Command::new("sleep")
            .arg("60")
            .current_dir(&s)
            .spawn()
            .unwrap();
        rewind(&env, "pkg", "v1");
        let _ = shell.kill();
        let _ = shell.wait();
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "{\"version\":\"1.0.0\"}\n",
            "[{backend}]"
        );
        env.ocean_graft(&["down"]);
    }
}

/// An in-place overlay rewind does not copy untouched directories of the
/// template up into the session's own layer.
#[test]
fn in_place_overlay_rewind_leaves_untouched_directories_alone() {
    if !backends().contains(&"overlay") {
        return;
    }
    let env = Env::new(6158);
    env.init("overlay");
    let s = fork(&env, "lean");
    let upper = env.project_dir().join("volumes/lean/upper");
    let count = |p: &Path| -> usize {
        fn walk(p: &Path) -> usize {
            fs::read_dir(p)
                .map(|it| {
                    it.flatten()
                        .map(|e| {
                            1 + if e.path().is_dir() {
                                walk(&e.path())
                            } else {
                                0
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(p)
    };
    fs::write(s.join("one.txt"), "1\n").unwrap();
    env.api("checkpoint", json!({"session": "lean", "label": "a"}))
        .unwrap();
    fs::write(s.join("one.txt"), "2\n").unwrap();
    let before = count(&upper);
    let mut shell = Command::new("sleep")
        .arg("60")
        .current_dir(&s)
        .spawn()
        .unwrap();
    rewind(&env, "lean", "a");
    let _ = shell.kill();
    let _ = shell.wait();
    assert_eq!(fs::read_to_string(s.join("one.txt")).unwrap(), "1\n");
    let after = count(&upper);
    assert!(
        after <= before + 1,
        "in-place rewind copied up {before} -> {after} entries"
    );
    assert!(!upper.join("src").exists(), "untouched src/ was copied up");
    env.ocean_graft(&["down"]);
}

/// The session's own running service is stopped before the decision, so it
/// does not force the in-place path: a clone session is swapped (its
/// directory is replaced) when nothing else works in it.
#[test]
fn a_running_service_does_not_force_an_in_place_rewind() {
    use std::os::unix::fs::MetadataExt;
    let env = Env::new(6164);
    env.init("clone");
    let s = fork(&env, "svc");
    env.api("checkpoint", json!({"session": "svc", "label": "a"}))
        .unwrap();
    env.ocean_graft(&["serve", "svc", "--", "sleep", "300"]);
    let ino = fs::metadata(&s).unwrap().ino();
    let r = rewind(&env, "svc", "a");
    assert_eq!(r["restarted_services"], json!(["web"]), "{r}");
    assert_ne!(
        fs::metadata(&s).unwrap().ino(),
        ino,
        "rewound in place although only its own service ran in it"
    );
    env.ocean_graft(&["down"]);
}

/// A swap rewind that fails after the swap (cleanup of the old tree fails)
/// keeps its safety checkpoint: the session has changed, and that
/// checkpoint is the only copy of what was there.
#[test]
fn a_swap_that_fails_after_swapping_keeps_the_safety_checkpoint() {
    let env = Env::new(6168);
    env.init("clone");
    let s = fork(&env, "imm");
    env.api("checkpoint", json!({"session": "imm", "label": "a"}))
        .unwrap();
    fs::write(s.join("work.txt"), "precious\n").unwrap();
    let locked = s.join("locked.txt");
    fs::write(&locked, "x\n").unwrap();
    let chattr = Command::new("chattr").arg("+i").arg(&locked).status();
    if !chattr.is_ok_and(|st| st.success()) {
        eprintln!("skipping: chattr +i not available here");
        return;
    }
    let r = env.api("rewind", json!({"session": "imm", "label": "a"}));
    // Whatever the outcome, the pre-rewind state must survive.
    let l = labels(&env, "imm");
    let safety = l.iter().find(|x| x.starts_with("pre-rewind-")).cloned();
    let _ = Command::new("chattr")
        .arg("-i")
        .args(
            fs::read_dir(env.project_dir())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("trash-"))
                .map(|e| e.path().join("locked.txt")),
        )
        .status();
    let _ = Command::new("chattr").arg("-i").arg(&locked).status();
    let safety = safety.unwrap_or_else(|| panic!("safety checkpoint dropped: {r:?} {l:?}"));
    let cp = env.project_dir().join("checkpoints/imm").join(&safety);
    assert_eq!(
        fs::read_to_string(cp.join("work.txt")).unwrap(),
        "precious\n",
        "{r:?}"
    );
    // Clean up anything immutable left behind so the temp dir can be removed.
    for e in fs::read_dir(env.project_dir()).unwrap().flatten() {
        let _ = Command::new("chattr")
            .args(["-R", "-i"])
            .arg(e.path())
            .status();
    }
}

/// A main session's Stop marks its session interactive (background
/// sessions never fire SessionStart inside their worktree); a subagent's
/// SubagentStop does not.
#[test]
fn main_stop_marks_a_session_interactive_but_subagent_stop_does_not() {
    let env = Env::new(6174);
    env.init("clone");
    let wt = |name: &str| {
        let r = env.hook(
            "worktree-create",
            &json!({"name": name, "cwd": env.repo(), "hook_event_name": "WorktreeCreate"}),
            &env.repo(),
            None,
        );
        assert_eq!(r.code, 0, "{}", r.stderr);
        PathBuf::from(r.last_line())
    };
    let bg = wt("bg");
    let sub = wt("sub");
    let r = env.hook(
        "stop",
        &json!({"cwd": bg, "hook_event_name": "Stop"}),
        &bg,
        None,
    );
    assert_eq!(r.code, 0);
    let r = env.hook(
        "stop",
        &json!({"cwd": sub, "hook_event_name": "SubagentStop", "agent_id": "a1"}),
        &sub,
        None,
    );
    assert_eq!(r.code, 0);
    let st = env.state();
    assert_eq!(st["sessions"]["bg"]["interactive"], json!(true), "{st}");
    assert_eq!(st["sessions"]["sub"]["interactive"], json!(false), "{st}");
    assert!(env.project_dir().join("locks/bg.interactive").exists());
    assert!(!env.project_dir().join("locks/sub.interactive").exists());
}

/// A land that runs out of time on the land lock names its real target.
#[test]
fn timed_out_land_names_its_target() {
    use std::os::unix::io::AsRawFd;
    let env = Env::new(6178);
    env.init("clone");
    let s = fork(&env, "t");
    fs::write(s.join("f.txt"), "x\n").unwrap();
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.project_dir().join("land.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let r = env
        .api(
            "land",
            json!({"session": "t", "commit_message": "c", "test": "", "timeout_secs": 1}),
        )
        .unwrap();
    assert_eq!(r["status"], "timed_out", "{r}");
    assert_eq!(r["target"], "main", "{r}");
    assert!(r["message"].as_str().unwrap().contains("main"), "{r}");
}

// ---------------------------------------------------------------------------
// Round 6: macOS-motivated fixes that can be checked on any platform
// ---------------------------------------------------------------------------

/// Session names are lower case (APFS matches names without regard to
/// case): a mixed-case name is refused before anything is touched, and
/// labels that differ only in case count as the same label.
#[test]
fn names_and_labels_are_case_insensitive() {
    let env = Env::new(6182);
    env.init("clone");
    let s = fork(&env, "api");
    env.api("checkpoint", json!({"session": "api", "label": "Before"}))
        .unwrap();
    let out = env.ocean_graft_in(&env.repo(), &["new", "Api"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("lower case"), "{}", stderr(&out));
    assert!(env.project_dir().join("checkpoints/api/Before").exists());
    let e = env
        .api("checkpoint", json!({"session": "api", "label": "before"}))
        .unwrap_err();
    assert!(e.contains("already has a checkpoint"), "{e}");
    assert!(env.project_dir().join("checkpoints/api/Before").exists());
    let r = env.api("fork", json!({"name": "Mixed-Case"})).unwrap();
    assert_eq!(r["sessions"][0]["name"], "mixed-case");
    let _ = s;
}

/// A read-only directory in a session (a module cache, chmod a-w output)
/// does not stop discard or checkpoint pruning (macOS runs as a normal
/// user, where such directories cannot simply be deleted).
#[test]
fn read_only_directories_do_not_block_removal() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(6186);
    env.init("clone");
    let s = fork(&env, "ro");
    let d = s.join("deps/cache/mod");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("x.go"), "package x\n").unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(s.join("deps/cache"), fs::Permissions::from_mode(0o555)).unwrap();
    env.api("checkpoint", json!({"session": "ro", "label": "a"}))
        .unwrap();
    let r = env.api("discard", json!({"session": "ro", "keep_work": false}));
    assert!(r.is_ok(), "{r:?}");
    assert!(!s.exists(), "session files left");
    assert!(!env.project_dir().join("checkpoints/ro").exists());
}
