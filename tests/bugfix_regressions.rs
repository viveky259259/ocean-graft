//! Regression tests for bugs found during deep code review and live testing.

mod common;

use common::{stderr, stdout, Env, BIN, HTTP_SERVER};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn write(path: impl AsRef<Path>, content: &str) {
    let p = path.as_ref();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

fn session_path(env: &Env, name: &str) -> PathBuf {
    PathBuf::from(env.ocean_graft(&["path", name]).trim())
}

#[test]
fn conflicts_after_rewind_still_detects_real_conflict() {
    let env = Env::new(59000);
    env.init("clone");
    env.ocean_graft(&["new", "a"]);
    env.ocean_graft(&["new", "b"]);
    let pa = session_path(&env, "a");
    let pb = session_path(&env, "b");

    write(pa.join("README.md"), "edit from session a\n");
    env.ocean_graft(&["checkpoint", "a", "cp1"]);
    // First conflicts run caches a's snapshot commit inside a/.git/objects.
    env.ocean_graft(&["conflicts"]);
    // Rewinding replaces a/.git/objects, removing the cached snapshot commit.
    env.ocean_graft(&["rewind", "a", "cp1"]);
    write(pb.join("README.md"), "edit from session b\n");

    let out = env.ocean_graft(&["conflicts"]);
    assert!(
        out.contains("CONFLICT in: README.md"),
        "expected hard conflict after rewind, got:\n{out}"
    );
}

#[test]
fn conflicts_detects_file_versus_directory_conflict() {
    let env = Env::new(59020);
    env.init("clone");
    env.ocean_graft(&["new", "a"]);
    env.ocean_graft(&["new", "b"]);
    let pa = session_path(&env, "a");
    let pb = session_path(&env, "b");

    write(pa.join("collision"), "a file\n");
    write(pb.join("collision/nested.txt"), "nested under directory\n");

    let out = env.ocean_graft(&["conflicts"]);
    assert!(
        out.contains("CONFLICT"),
        "expected file/directory conflict to be reported, got:\n{out}"
    );
}

#[test]
fn rm_refuses_stash_extra_branch_and_dangling_source_commit() {
    let env = Env::new(59040);
    env.init("clone");

    // 1. Stash in session must block `rm` without `--force`.
    env.ocean_graft(&["new", "stashed"]);
    let ps = session_path(&env, "stashed");
    write(ps.join("secret.txt"), "stashed work\n");
    env.git(&ps, &["stash", "push", "-u", "-m", "wip"]);
    let out = env.ocean_graft_in(&env.repo(), &["rm", "stashed"]);
    assert!(
        !out.status.success(),
        "rm without --force should refuse when session has a stash"
    );
    assert!(ps.is_dir(), "session directory was deleted despite stash");

    // 2. Extra local branch must block `rm` without `--force`.
    env.ocean_graft(&["new", "branched"]);
    let pb = session_path(&env, "branched");
    env.git(&pb, &["switch", "-c", "extra-work"]);
    write(pb.join("extra.txt"), "branch commit\n");
    env.git(&pb, &["add", "-A"]);
    env.git(&pb, &["commit", "-m", "extra"]);
    env.git(&pb, &["switch", "ocean-graft/branched"]);
    let out = env.ocean_graft_in(&env.repo(), &["rm", "branched"]);
    assert!(
        !out.status.success(),
        "rm without --force should refuse when session has an extra local branch"
    );
    assert!(pb.is_dir());

    // 3. Commit that is only dangling in source must still block `rm`.
    env.ocean_graft(&["new", "dangling"]);
    let pd = session_path(&env, "dangling");
    write(pd.join("d.txt"), "only here\n");
    env.git(&pd, &["add", "-A"]);
    env.git(&pd, &["commit", "-m", "d"]);
    env.ocean_graft(&["import", "dangling"]);
    env.git(&env.repo(), &["branch", "-D", "ocean-graft/dangling"]);
    let out = env.ocean_graft_in(&env.repo(), &["rm", "dangling"]);
    assert!(
        !out.status.success(),
        "rm without --force should refuse when commit is unreachable (dangling) in source"
    );
    assert!(pd.is_dir());
}

#[test]
fn land_refuses_when_manual_rebase_is_in_progress_without_aborting_it() {
    let env = Env::new(59060);
    env.init("clone");
    env.ocean_graft(&["new", "rb"]);
    let prb = session_path(&env, "rb");

    write(prb.join("f.txt"), "one\n");
    env.git(&prb, &["add", "-A"]);
    env.git(&prb, &["commit", "-m", "one"]);
    write(prb.join("f.txt"), "two\n");
    env.git(&prb, &["add", "-A"]);
    env.git(&prb, &["commit", "-m", "two"]);

    // Pause an interactive rebase at the first commit.
    let status = env
        .command("git", &prb)
        .env(
            "GIT_SEQUENCE_EDITOR",
            "python3 -c 'import sys; p=sys.argv[1]; s=open(p).read().replace(\"pick\", \"edit\", 1); open(p,\"w\").write(s)'",
        )
        .args(["rebase", "-i", "HEAD~2"])
        .status()
        .unwrap();
    assert!(status.success(), "starting interactive rebase failed");
    assert!(prb.join(".git/rebase-merge").is_dir());

    // Advance main so land would otherwise try to rebase.
    write(env.repo().join("main-change.txt"), "advance\n");
    env.git(&env.repo(), &["add", "-A"]);
    env.git(&env.repo(), &["commit", "-m", "advance"]);

    let err = env
        .api("land", json!({"session": "rb", "test": ""}))
        .unwrap_err();
    assert!(
        err.contains("in the middle of a rebase"),
        "unexpected error: {err}"
    );
    assert!(
        prb.join(".git/rebase-merge").is_dir(),
        "land aborted the user's in-progress manual rebase!"
    );
}

#[test]
fn land_retries_when_target_is_reset_backward_protecting_ignored_files() {
    let env = Env::new(59080);
    // Commit C1 adds secret.conf on main.
    write(env.repo().join("secret.conf"), "committed-secret\n");
    env.git(&env.repo(), &["add", "-A"]);
    env.git(&env.repo(), &["commit", "-m", "add secret.conf"]);

    env.init("clone");
    env.ocean_graft(&["new", "s"]);
    let ps = session_path(&env, "s");
    write(ps.join("feat.txt"), "feat\n");
    env.git(&ps, &["add", "-A"]);
    env.git(&ps, &["commit", "-m", "feat"]);

    let marker = env.root.path().join("test-started");
    let test_cmd = format!("touch {} && sleep 2", marker.display());
    let child = env
        .command(BIN, &env.repo())
        .args([
            "api",
            "land",
            &json!({"session": "s", "test": test_cmd}).to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Wait until tests start running (past step 2 fetch).
    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(marker.exists(), "test command never started");

    // Reset main backward to C0 and place a local ignored secret.conf in source.
    env.git(&env.repo(), &["reset", "--hard", "HEAD~1"]);
    let exclude = env.repo().join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("\nsecret.conf\n");
    write(&exclude, &text);
    write(env.repo().join("secret.conf"), "LOCAL_IGNORED_SECRET\n");

    let out = child.wait_with_output().unwrap();
    let val: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        val["status"], "target_blocked",
        "land should detect backward target reset and refuse to clobber ignored file: {val}"
    );
    assert_eq!(
        fs::read_to_string(env.repo().join("secret.conf")).unwrap(),
        "LOCAL_IGNORED_SECRET\n",
        "ignored file in source was overwritten!"
    );
}

#[test]
fn copy_rejects_git_hooks_and_template_side_symlink_escape() {
    // 1. `.git/hooks/...` in ocean-graft.toml copy must be rejected.
    let env = Env::new(59100);
    write(
        env.repo().join("ocean-graft.toml"),
        "copy = [\".git/hooks/post-checkout\"]\n",
    );
    env.git(&env.repo(), &["add", "ocean-graft.toml"]);
    env.git(&env.repo(), &["commit", "-m", "bad copy"]);
    let err = env.api("fork", json!({"name": "a"})).unwrap_err();
    assert!(
        err.contains("not .git") || err.contains("ocean-graft.toml"),
        "expected .git copy rejection, got: {err}"
    );

    // 2. Template-side symlink write escape must be rejected.
    let env2 = Env::new(59110);
    let outside = env2.root.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, env2.repo().join("link")).unwrap();
    write(
        env2.repo().join("ocean-graft.toml"),
        "copy = [\"link/pwned\"]\n",
    );
    env2.git(&env2.repo(), &["add", "link", "ocean-graft.toml"]);
    env2.git(&env2.repo(), &["commit", "-m", "commit symlink"]);
    // Replace symlink with real dir in working tree so `from` canonicalizes inside source.
    fs::remove_file(env2.repo().join("link")).unwrap();
    write(env2.repo().join("link/pwned"), "escaped!\n");

    let err = env2.api("fork", json!({"name": "a"})).unwrap_err();
    assert!(
        err.contains("symlink"),
        "expected template symlink rejection, got: {err}"
    );
    assert!(
        !outside.join("pwned").exists(),
        "file escaped outside template through symlink!"
    );
}

#[test]
fn rewind_rescues_session_whose_git_directory_was_deleted() {
    let env = Env::new(59120);
    env.init("clone");
    env.ocean_graft(&["new", "a"]);
    let pa = session_path(&env, "a");
    env.ocean_graft(&["checkpoint", "a", "good"]);

    fs::remove_dir_all(pa.join(".git")).unwrap();
    assert!(!pa.join(".git").exists());

    env.ocean_graft(&["rewind", "a", "good"]);
    assert!(
        pa.join(".git").is_dir(),
        ".git should be restored by rewind"
    );
    assert!(!env.git(&pa, &["rev-parse", "HEAD"]).trim().is_empty());
}

#[test]
fn in_place_rewind_restores_case_only_rename_on_macos() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let env = Env::new(59140);
    env.init("clone");
    env.ocean_graft(&["new", "a"]);
    let pa = session_path(&env, "a");
    write(pa.join("Foo.txt"), "same content\n");
    env.ocean_graft(&["checkpoint", "a", "cp1"]);

    // Case-only rename on APFS.
    fs::rename(pa.join("Foo.txt"), pa.join("tmp.txt")).unwrap();
    fs::rename(pa.join("tmp.txt"), pa.join("FOO.TXT")).unwrap();

    // Hold cwd inside the session to force restore_in_place.
    let mut holder = Command::new("sleep")
        .arg("10")
        .current_dir(&pa)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));

    let res = env.ocean_graft_in(&env.repo(), &["rewind", "a", "cp1"]);
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(res.status.success(), "rewind failed: {}", stderr(&res));

    let names: Vec<String> = fs::read_dir(&pa)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "Foo.txt"),
        "expected exact case Foo.txt after in-place rewind, found {names:?}"
    );
    assert!(!names.iter().any(|n| n == "FOO.TXT"));
}

#[test]
fn save_rev_falls_back_past_df_ref_conflict() {
    let env = Env::new(59160);
    env.git(&env.repo(), &["branch", "ocean-graft/api/v1"]);
    env.init("clone");
    env.ocean_graft(&["new", "api"]);
    let pa = session_path(&env, "api");
    write(pa.join("work.txt"), "work\n");
    env.git(&pa, &["add", "-A"]);
    env.git(&pa, &["commit", "-m", "work"]);

    env.ocean_graft(&["import", "api"]);
    let refs = env.git(&env.repo(), &["for-each-ref", "--format=%(refname:short)"]);
    assert!(
        refs.lines().any(|l| l == "ocean-graft/api-2"),
        "expected fallback branch ocean-graft/api-2, got:\n{refs}"
    );
}

#[test]
fn save_work_succeeds_with_commit_gpgsign_enabled() {
    let env = Env::new(59180);
    env.init("clone");
    env.ocean_graft(&["new", "g"]);
    let pg = session_path(&env, "g");
    env.git(&pg, &["config", "commit.gpgsign", "true"]);
    env.git(&pg, &["config", "gpg.program", "/usr/bin/false"]);
    write(pg.join("dirty.txt"), "uncommitted\n");

    let res = env
        .api("discard", json!({"session": "g", "keep_work": true}))
        .expect("discard keep_work=true should bypass commit.gpgsign");
    assert_eq!(res["saved"]["committed_wip"], true);
}

#[test]
fn wrong_project_flag_does_not_leak_lock_file() {
    let env = Env::new(59200);
    let out = env.ocean_graft_in(
        &env.repo(),
        &["--project", &env.repo().to_string_lossy(), "ls"],
    );
    assert!(!out.status.success());
    assert!(
        !env.repo().join("lock").exists(),
        "passing a non-project dir to --project leaked a `lock` file!"
    );
}

#[test]
fn worktree_remove_refuses_symlink_pointing_to_real_session() {
    let env = Env::new(59220);
    env.init("clone");
    env.ocean_graft(&["new", "victim"]);
    let pv = session_path(&env, "victim");
    let link = env.root.path().join("fake-worktree-link");
    std::os::unix::fs::symlink(&pv, &link).unwrap();

    let out = env.hook(
        "worktree-remove",
        &json!({"worktree_path": link.to_string_lossy()}),
        &env.repo(),
        None,
    );
    assert_ne!(out.code, 0, "worktree-remove should refuse a symlink path");
    assert!(
        pv.is_dir(),
        "real session was deleted through a symlink worktree_path!"
    );
}

#[test]
fn package_json_fifo_does_not_hang_config_load() {
    let env = Env::new(59240);
    env.init("clone");
    let fifo = env.repo().join("package.json");
    let cpath = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed");

    let mut child = env
        .command(BIN, &env.repo())
        .args(["api", "status", "{}"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            assert!(st.success(), "status failed on FIFO package.json");
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("config::load hung on FIFO package.json!");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sandbox_allows_realpath_tmpdir_and_blocks_flag_injection() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let env = Env::new(59260);
    env.init("clone");
    env.ocean_graft(&["new", "sb"]);

    // 1. realpath and ls -la on $TMPDIR must succeed under Seatbelt.
    let out = env.ocean_graft_in(
        &env.repo(),
        &[
            "exec",
            "--sandbox",
            "sb",
            "--",
            "sh",
            "-c",
            r#"/bin/realpath "$TMPDIR" && ls -la "$TMPDIR" >/dev/null"#,
        ],
    );
    assert!(
        out.status.success(),
        "realpath/ls on $TMPDIR failed under sandbox:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );

    // 2. Leading `-D` must NOT be interpreted as a sandbox-exec flag.
    let inj = env.ocean_graft_in(
        &env.repo(),
        &[
            "exec",
            "--sandbox",
            "sb",
            "--",
            "-D",
            "foo=bar",
            "echo",
            "INJECTED",
        ],
    );
    assert!(
        !inj.status.success(),
        "sandbox-exec accepted -D as its own flag instead of separating with `--`"
    );
    assert!(!stdout(&inj).contains("INJECTED"));
}

#[test]
fn stop_on_asleep_on_demand_clears_idle_flag_and_counts_gateway() {
    let env = Env::new(59280);
    env.init("clone");
    env.ocean_graft(&["new", "web"]);

    env.ocean_graft(&["serve", "web", "--on-demand", "--", "sh", "-c", HTTP_SERVER]);
    env.ocean_graft(&["reap", "--idle", "0s"]);
    let before = env.ocean_graft(&["services"]);
    assert!(
        before.contains("asleep (on demand)"),
        "expected asleep (on demand), got:\n{before}"
    );

    let stop_out = env.ocean_graft(&["stop", "web"]);
    assert!(
        !stop_out.contains("Nothing was running"),
        "stopping an asleep on-demand gateway should report stopping it, got:\n{stop_out}"
    );
    let after = env.ocean_graft(&["services"]);
    assert!(
        !after.contains("stopped (idle)"),
        "stopped_for_idle should be cleared after explicit stop, got:\n{after}"
    );

    // Subsequent wake must NOT resurrect an explicitly stopped service.
    let wake_out = env.ocean_graft(&["wake", "web"]);
    assert!(
        wake_out.contains("no services stopped for being idle"),
        "expected no idle services to wake, got:\n{wake_out}"
    );
}

#[test]
fn launcher_checksum_mismatch_never_falls_back_to_path() {
    let env = Env::new(59300);
    let root = env.root.path();
    let libexec = root.join("plugin/libexec");
    fs::create_dir_all(&libexec).unwrap();
    let launcher = libexec.join("ocean-graft-plugin");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/libexec/ocean-graft-plugin"),
        &launcher,
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();

    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" if os == "darwin" => "arm64",
        other => other,
    };
    let asset = format!("ocean-graft-{os}-{arch}");
    write(
        libexec.join("release.txt"),
        &format!("version 9.9.9\nsha256 {asset} {}\n", "0".repeat(64)),
    );
    write(root.join("server/v9.9.9").join(&asset), "TAMPERED_BYTES");

    let fakebin = root.join("fakebin");
    fs::create_dir_all(&fakebin).unwrap();
    let fake_cli = fakebin.join("ocean-graft");
    write(&fake_cli, "#!/bin/sh\necho FALLBACK_PATH_EXECUTED\n");
    fs::set_permissions(&fake_cli, fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(&launcher)
        .arg("--version")
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("CLAUDE_PLUGIN_DATA", root.join("data"))
        .env(
            "OCEAN_GRAFT_RELEASE_BASE",
            format!("file://{}", root.join("server").display()),
        )
        .env_remove("OCEAN_GRAFT_BIN")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "launcher should fail on SHA-256 mismatch even when ocean-graft is on PATH"
    );
    assert!(
        !stdout(&out).contains("FALLBACK_PATH_EXECUTED"),
        "launcher fell open to PATH after SHA-256 mismatch!"
    );
    assert!(stderr(&out).contains("does not match release.txt"));
}

#[test]
fn queue_set_slot_count_is_atomic_under_concurrent_reads() {
    let env = Env::new(59320);
    env.ocean_graft(&["queue", "--slots", "4"]);

    let home = env.home();
    let writer = std::thread::spawn(move || {
        for _ in 0..80 {
            let st = Command::new(BIN)
                .env("OCEAN_GRAFT_HOME", &home)
                .args(["queue", "--slots", "4"])
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(st.success());
        }
    });
    for _ in 0..80 {
        let out = Command::new(BIN)
            .env("OCEAN_GRAFT_HOME", env.home())
            .args(["queue"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "concurrent `queue` failed during set_slot_count: {}",
            stderr(&out)
        );
    }
    writer.join().unwrap();
}

#[test]
fn parse_duration_rejects_overflow_and_reap_rejects_zero_watch() {
    let env = Env::new(59340);
    let out1 = env.ocean_graft_in(&env.repo(), &["reap", "--idle", "999999999999999999h"]);
    assert!(
        !out1.status.success(),
        "overflowing duration should fail cleanly"
    );
    assert!(stderr(&out1).contains("too large"));

    let out2 = env.ocean_graft_in(&env.repo(), &["reap", "--idle", "1m", "--watch", "0s"]);
    assert!(!out2.status.success(), "--watch 0s should be rejected");
    assert!(stderr(&out2).contains("greater than 0s"));
}
