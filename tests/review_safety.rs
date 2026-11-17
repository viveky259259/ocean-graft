mod common;
use common::*;

// Regression tests for the data-safety fixes of commit dda3697 (rule 4,
// "Nothing lost", and rule 6, "Crash safety", of plugin/ARCHITECTURE.md):
// rewind's pre-rewind-N safety checkpoint, the clone-backend rewind crash
// repair, removal (extra branches and stashes, mid-rebase/merge refusal,
// sessions whose files are already gone, stray directories left by a crash),
// land never overwriting ignored or untracked files, and reused names.
//
// Port range 5800-5899. Five test functions, port bases 20 apart; within a
// function each scenario gets a fresh Env (sequentially) on the same base,
// and never keeps more than two sessions alive at once. Every scenario runs
// even when an earlier one fails; the failures are reported together.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Run each scenario, catching panics, and fail once at the end with every
/// scenario that failed (so one product bug does not hide the others).
fn run_scenarios(scenarios: Vec<(String, Box<dyn FnOnce()>)>) {
    let mut failed = Vec::new();
    for (name, f) in scenarios {
        eprintln!("--- scenario {name}");
        if let Err(e) = catch_unwind(AssertUnwindSafe(f)) {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            failed.push(format!("{name}: {msg}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} scenario(s) failed:\n{}",
        failed.len(),
        failed.join("\n\n")
    );
}

fn setup(port: u16, backend: &str) -> Env {
    let env = Env::new(port);
    env.init(backend);
    env
}

fn fork(env: &Env, name: &str) -> PathBuf {
    let v = env.api("fork", json!({"name": name})).expect("fork");
    assert_eq!(v["sessions"][0]["name"], name, "fork result: {v}");
    PathBuf::from(v["sessions"][0]["path"].as_str().unwrap())
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn head(env: &Env, dir: &Path) -> String {
    env.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn tip(env: &Env, branch: &str) -> String {
    env.git(&env.repo(), &["rev-parse", &format!("refs/heads/{branch}")])
        .trim()
        .to_string()
}

fn ref_exists(env: &Env, dir: &Path, r: &str) -> bool {
    env.command("git", dir)
        .args(["rev-parse", "--verify", "-q", r])
        .output()
        .unwrap()
        .status
        .success()
}

/// Branches of the source repository whose name starts with `prefix`.
fn source_branches(env: &Env, prefix: &str) -> Vec<String> {
    env.git(
        &env.repo(),
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .lines()
    .filter(|b| b.starts_with(prefix))
    .map(str::to_string)
    .collect()
}

/// `git show <rev>:<file>` in the source repository.
fn show(env: &Env, rev: &str, file: &str) -> String {
    env.git(&env.repo(), &["show", &format!("{rev}:{file}")])
}

/// Run git without asserting success; returns (success, stdout+stderr).
fn git_try(env: &Env, dir: &Path, args: &[&str]) -> (bool, String) {
    let out = env.command("git", dir).args(args).output().unwrap();
    (
        out.status.success(),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

/// Every file, symlink and directory under `dir` except `.git`, with file
/// contents, so two states can be compared exactly.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == ".git" {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("symlink -> {:?}", fs::read_link(&path).unwrap()),
                );
            } else if meta.is_dir() {
                out.insert(format!("{rel}/"), String::new());
                walk(root, &path, out);
            } else {
                out.insert(
                    rel,
                    String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// What "the whole session state" means for the rewind tests: files
/// (tracked, untracked, ignored), HEAD, branch, and git's view of them.
#[derive(Debug, PartialEq)]
struct SessionState {
    head: String,
    branch: String,
    status: String,
    files: BTreeMap<String, String>,
}

fn session_state(env: &Env, dir: &Path) -> SessionState {
    SessionState {
        head: head(env, dir),
        branch: env
            .git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string(),
        status: env.git(
            dir,
            &[
                "status",
                "--porcelain=v1",
                "--ignored=matching",
                "--untracked-files=all",
            ],
        ),
        files: tree(dir),
    }
}

fn checkpoint_labels(env: &Env, session: &str) -> Vec<String> {
    env.state()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["session"] == session)
        .map(|c| c["label"].as_str().unwrap().to_string())
        .collect()
}

fn session_names(env: &Env) -> Vec<String> {
    env.state()["sessions"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// Session names as the `status` tool reports them.
fn status_names(env: &Env) -> Vec<String> {
    let v = env.api("status", json!({})).expect("status");
    v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

/// Delete a session's files by hand, as a user (or a crash) would: the clone
/// backend's directory; for overlay, unmount it and delete its volume and
/// mount point.
fn delete_by_hand(env: &Env, backend: &str, name: &str) {
    let dir = env.project_dir();
    let path = dir.join("sessions").join(name);
    if backend == "overlay" {
        unmount(&path);
        fs::remove_dir_all(dir.join("volumes").join(name)).unwrap();
        fs::remove_dir(&path).unwrap();
    } else {
        fs::remove_dir_all(&path).unwrap();
    }
    assert!(path.symlink_metadata().is_err());
}

fn unmount(path: &Path) {
    let out = Command::new("umount").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "umount {}: {}",
        path.display(),
        stderr(&out)
    );
}

fn remove_input(path: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "hook_event_name": "WorktreeRemove",
        "cwd": "/",
        "worktree_path": path,
    })
}

fn cp_a(src: &Path, dst: &Path) {
    let out = Command::new("cp")
        .arg("-a")
        .arg(src)
        .arg(dst)
        .output()
        .unwrap();
    assert!(out.status.success(), "cp -a: {}", stderr(&out));
}

// ---------------------------------------------------------------------------
// rewind: pre-rewind-N, exact undo, "latest" never bounces, restart_errors
// ---------------------------------------------------------------------------

/// State A: a commit, an uncommitted edit, an untracked file, an ignored file.
fn make_state_a(env: &Env, s: &Path) {
    write(&s.join("committed.txt"), "committed in A\n");
    env.git(s, &["add", "committed.txt"]);
    env.git(s, &["commit", "-q", "-m", "A commit"]);
    write(
        &s.join("src/app.txt"),
        "line one\nline two (A)\nline three\n",
    );
    write(&s.join("untracked-a.txt"), "untracked A\n");
    write(&s.join("deps/extra-a.txt"), "ignored A\n");
}

/// State B: another commit, a deleted tracked file, other untracked and
/// ignored files, a changed ignored file.
fn make_state_b(env: &Env, s: &Path) {
    write(&s.join("committed.txt"), "committed in B\n");
    env.git(s, &["add", "committed.txt"]);
    env.git(s, &["commit", "-q", "-m", "B commit"]);
    fs::remove_file(s.join("README.md")).unwrap();
    write(&s.join("src/app.txt"), "line one (B)\n");
    fs::remove_file(s.join("untracked-a.txt")).unwrap();
    write(&s.join("untracked-b.txt"), "untracked B\n");
    write(&s.join("deps/extra-b.txt"), "ignored B\n");
    write(&s.join(".env"), "SECRET=B\n");
}

fn rewind(env: &Env, session: &str, label: &str) -> Value {
    env.api("rewind", json!({"session": session, "label": label}))
        .unwrap_or_else(|e| panic!("rewind {session} to {label}: {e}"))
}

fn rewind_undo_and_latest(backend: &'static str) {
    let env = setup(5800, backend);
    let s = fork(&env, "rw");
    make_state_a(&env, &s);
    let cp = env
        .api("checkpoint", json!({"session": "rw", "label": "a"}))
        .expect("checkpoint a");
    assert_eq!(cp["label"], "a");
    let state_a = session_state(&env, &s);

    // Every rewind saves what it replaces, even an apparently unchanged
    // state: no cheap fingerprint sees stashes, other branches or changes
    // deep inside ignored directories.
    let r = rewind(&env, "rw", "a");
    assert_eq!(r["label"], "a");
    assert_eq!(r["saved_as"], "pre-rewind-1", "{r}");
    assert_eq!(checkpoint_labels(&env, "rw"), vec!["a", "pre-rewind-1"]);
    assert_eq!(session_state(&env, &s), state_a);

    // Changed since the target: the replaced state is saved first.
    make_state_b(&env, &s);
    let state_b = session_state(&env, &s);
    assert_ne!(state_a, state_b);
    let r = rewind(&env, "rw", "a");
    assert_eq!(r["saved_as"], "pre-rewind-2", "{r}");
    assert_eq!(r["head"], state_a.head);
    assert_eq!(session_state(&env, &s), state_a, "rewind to a");

    // Rewinding to the safety checkpoint undoes the rewind exactly.
    let r = rewind(&env, "rw", "pre-rewind-2");
    assert_eq!(r["label"], "pre-rewind-2");
    assert_eq!(r["saved_as"], "pre-rewind-3", "{r}");
    assert_eq!(r["head"], state_b.head);
    assert_eq!(
        session_state(&env, &s),
        state_b,
        "rewinding to pre-rewind-2 must restore files, HEAD, untracked and ignored files"
    );

    // "latest" skips pre-rewind-*: it goes to a, twice, without bouncing.
    let r = rewind(&env, "rw", "latest");
    assert_eq!(r["label"], "a", "latest picked {r}");
    assert_eq!(r["saved_as"], "pre-rewind-4", "{r}");
    assert_eq!(session_state(&env, &s), state_a);
    let r = rewind(&env, "rw", "latest");
    assert_eq!(r["label"], "a", "second latest bounced: {r}");
    assert_eq!(r["saved_as"], "pre-rewind-5", "{r}");
    assert_eq!(session_state(&env, &s), state_a);
    assert_eq!(
        checkpoint_labels(&env, "rw"),
        vec![
            "a",
            "pre-rewind-1",
            "pre-rewind-2",
            "pre-rewind-3",
            "pre-rewind-4",
            "pre-rewind-5"
        ]
    );
    // And pre-rewind-4 still holds state B.
    rewind(&env, "rw", "pre-rewind-4");
    assert_eq!(session_state(&env, &s), state_b);
}

fn rewind_restart_errors(backend: &'static str) {
    let env = setup(5800, backend);
    let s = fork(&env, "svc");
    env.api(
        "checkpoint",
        json!({"session": "svc", "label": "no-marker"}),
    )
    .expect("checkpoint");
    write(&s.join("marker.txt"), "present\n");
    // "picky" refuses to start without marker.txt; "plain" always starts.
    env.shoal(&[
        "serve",
        "svc",
        "--as",
        "picky",
        "--",
        "sh",
        "-c",
        "test -f marker.txt || exit 3; exec sleep 60",
    ]);
    env.shoal(&[
        "serve",
        "svc",
        "--as",
        "plain",
        "--",
        "sh",
        "-c",
        "exec sleep 60",
    ]);
    let r = rewind(&env, "svc", "no-marker");
    let _ = env.shoal_in(&env.repo(), &["stop", "svc"]);
    assert_eq!(r["label"], "no-marker");
    assert_eq!(r["saved_as"], "pre-rewind-1", "{r}");
    assert!(!s.join("marker.txt").exists(), "rewind did not happen");
    let errors: Vec<String> = r["restart_errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no restart_errors array: {r}"))
        .iter()
        .map(|e| e.as_str().unwrap().to_string())
        .collect();
    assert_eq!(errors.len(), 1, "restart_errors: {r}");
    assert!(errors[0].starts_with("picky"), "restart_errors: {r}");
    assert_eq!(r["restarted_services"], json!(["plain"]), "{r}");
}

#[test]
fn rewind_saves_replaced_state_and_reports_restart_errors() {
    let mut scenarios: Vec<(String, Box<dyn FnOnce()>)> = Vec::new();
    for backend in backends() {
        scenarios.push((
            format!("{backend}: rewind saved_as, undo, latest"),
            Box::new(move || rewind_undo_and_latest(backend)),
        ));
        scenarios.push((
            format!("{backend}: rewind restart_errors"),
            Box::new(move || rewind_restart_errors(backend)),
        ));
    }
    run_scenarios(scenarios);
}

// ---------------------------------------------------------------------------
// Crash repair of an interrupted rewind
// ---------------------------------------------------------------------------

fn clone_trash_restored_when_session_missing() {
    let env = setup(5820, "clone");
    let s = fork(&env, "crash-me");
    write(&s.join("work.txt"), "unsaved work\n");
    let before = tree(&s);
    let dir = env.project_dir();
    let trash = dir.join("trash-crash-me-99999");
    fs::rename(&s, &trash).unwrap();
    // Any command repairs it.
    env.shoal(&["ls"]);
    assert!(s.is_dir(), "session directory not restored");
    assert!(trash.symlink_metadata().is_err(), "trash left behind");
    assert_eq!(tree(&s), before);
    assert_eq!(
        env.git(&s, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "shoal/crash-me"
    );
}

fn clone_trash_deleted_when_both_present() {
    let env = setup(5820, "clone");
    let s = fork(&env, "crash-me");
    write(&s.join("work.txt"), "current\n");
    let dir = env.project_dir();
    let trash = dir.join("trash-crash-me-99999");
    cp_a(&s, &trash);
    write(&trash.join("work.txt"), "old, replaced by the rewind\n");
    let before = tree(&s);
    env.api("status", json!({})).expect("status");
    assert!(trash.symlink_metadata().is_err(), "trash not deleted");
    assert_eq!(tree(&s), before, "the live session must be untouched");
}

fn clone_restore_dir_always_deleted() {
    let env = setup(5820, "clone");
    let s = fork(&env, "crash-me");
    let dir = env.project_dir();
    let restore = dir.join("restore-crash-me-99999");
    write(&restore.join("partial.txt"), "half a checkpoint copy\n");
    let before = tree(&s);
    env.api("conflicts", json!({})).expect("conflicts");
    assert!(restore.symlink_metadata().is_err(), "restore copy left");
    assert_eq!(tree(&s), before);
    // Also when the session directory itself is missing: the restore copy is
    // never swapped in, the trash is.
    let trash = dir.join("trash-crash-me-99999");
    write(&restore.join("partial.txt"), "half a checkpoint copy\n");
    fs::rename(&s, &trash).unwrap();
    env.shoal(&["ls"]);
    assert!(restore.symlink_metadata().is_err(), "restore copy left");
    assert!(trash.symlink_metadata().is_err(), "trash left");
    assert_eq!(tree(&s), before, "the session must come back from trash");
}

fn clone_unknown_session_never_touched() {
    let env = setup(5820, "clone");
    let _s = fork(&env, "crash-me");
    let dir = env.project_dir();
    let ghost = dir.join("trash-ghost-99999");
    write(&ghost.join("keep.txt"), "not shoal's to touch\n");
    let not_pid = dir.join("trash-crash-me-notapid");
    write(&not_pid.join("keep.txt"), "no pid suffix\n");
    env.shoal(&["ls"]);
    env.api("status", json!({})).expect("status");
    assert_eq!(read(ghost.join("keep.txt")), "not shoal's to touch\n");
    assert!(
        dir.join("sessions/ghost").symlink_metadata().is_err(),
        "a session that is not in state must not be created"
    );
    assert_eq!(read(not_pid.join("keep.txt")), "no pid suffix\n");
}

// Fixed bug: the overlay rewind moves upper/ aside to upper.rewind-<pid> and then
// copies the checkpoint into a new upper/; Project::open only repairs the
// clone backend's restore-*/trash-* directories, so a rewind killed during
// the copy leaves the session on a partial upper (silently showing neither
// its old state nor the checkpoint) and leaks upper.rewind-<pid>, despite
// rule 6 ("the next command to open the project repairs one that was killed
// halfway").
fn overlay_interrupted_rewind_repaired() {
    // The overlay rewind copies the checkpoint into upper.restore-<pid>,
    // then swaps: upper -> upper.rewind-<pid>, upper.restore-<pid> -> upper.
    let env = setup(5820, "overlay");
    let s = fork(&env, "crash-me");
    write(&s.join("work.txt"), "unsaved work\n");
    let before = tree(&s);
    let dir = env.project_dir();
    let volume = dir.join("volumes/crash-me");

    // Killed while copying: a partial staging copy, upper untouched.
    let staging = volume.join("upper.restore-99999");
    fs::create_dir_all(staging.join("half")).unwrap();
    env.api("status", json!({})).expect("status");
    assert!(
        staging.symlink_metadata().is_err(),
        "partial staging copy left"
    );
    assert_eq!(tree(&s), before, "the session must be untouched");

    // Killed between the two renames: upper is gone, the old layer aside.
    unmount(&s);
    let aside = volume.join("upper.rewind-99999");
    fs::rename(volume.join("upper"), &aside).unwrap();
    fs::create_dir_all(staging.join("complete")).unwrap();
    env.api("status", json!({})).expect("status");
    env.shoal(&["up"]);
    assert_eq!(
        tree(&s),
        before,
        "after an interrupted rewind the session must be back in its last whole state"
    );
    assert!(
        aside.symlink_metadata().is_err(),
        "upper.rewind-99999 left behind"
    );
    assert!(
        staging.symlink_metadata().is_err(),
        "upper.restore-99999 left behind"
    );
}

#[test]
fn interrupted_rewind_is_repaired_on_next_open() {
    let mut scenarios: Vec<(String, Box<dyn FnOnce()>)> = vec![
        (
            "clone: trash restored when the session is missing".into(),
            Box::new(clone_trash_restored_when_session_missing),
        ),
        (
            "clone: trash deleted when the session is present".into(),
            Box::new(clone_trash_deleted_when_both_present),
        ),
        (
            "clone: restore-* always deleted".into(),
            Box::new(clone_restore_dir_always_deleted),
        ),
        (
            "clone: sessions not in state are never touched".into(),
            Box::new(clone_unknown_session_never_touched),
        ),
    ];
    if backends().contains(&"overlay") {
        scenarios.push((
            "overlay: interrupted rewind repaired".into(),
            Box::new(overlay_interrupted_rewind_repaired),
        ));
    }
    run_scenarios(scenarios);
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

// Fixed bug: extra branches are saved as shoal/<name>-<branch>, not
// shoal/<name>--<branch> as rule 4 says: save_work passes "<name>--<branch>"
// through sanitize_name, which collapses "--" to "-" (stashes keep "--"
// because their suffix is added after sanitising). The single dash makes a
// saved branch indistinguishable from (and able to collide with) the import
// branch of a session named "<name>-<branch>".
fn discard_saves_branches_and_stashes(backend: &'static str) {
    let env = setup(5840, backend);
    let main_before = tip(&env, "main");
    let s = fork(&env, "keeper");
    // Committed work on the session's own branch.
    write(&s.join("own.txt"), "own branch work\n");
    env.git(&s, &["add", "own.txt"]);
    env.git(&s, &["commit", "-q", "-m", "own work"]);
    // Another local branch with its own commit.
    env.git(&s, &["switch", "-q", "-c", "side"]);
    write(&s.join("side.txt"), "side branch work\n");
    env.git(&s, &["add", "side.txt"]);
    env.git(&s, &["commit", "-q", "-m", "side work"]);
    let side_head = head(&env, &s);
    env.git(&s, &["switch", "-q", "shoal/keeper"]);
    // A branch whose commit is already in the source: nothing to save.
    env.git(&s, &["branch", "already-there", &main_before]);
    // Two stashes (stash@{0} is the newer one).
    write(&s.join("README.md"), "stash one\n");
    env.git(&s, &["stash", "-q"]);
    write(&s.join("README.md"), "stash two\n");
    env.git(&s, &["stash", "-q"]);
    // And uncommitted work.
    write(&s.join("src/app.txt"), "uncommitted\n");

    let r = env
        .api("discard", json!({"session": "keeper"}))
        .expect("discard");
    assert!(!session_names(&env).contains(&"keeper".to_string()));
    assert_eq!(r["saved"]["committed_wip"], true, "{r}");
    assert_eq!(r["saved"]["import"]["branch"], "shoal/keeper", "{r}");
    let mut extra: Vec<String> = r["saved"]["extra_branches"]
        .as_array()
        .unwrap_or_else(|| panic!("no extra_branches: {r}"))
        .iter()
        .map(|b| b.as_str().unwrap().to_string())
        .collect();
    extra.sort();
    // Contents first, by the names shoal reports, so a naming defect does not
    // hide a content defect.
    assert_eq!(extra.len(), 3, "side, stash 0 and stash 1 expected: {r}");
    let side = extra
        .iter()
        .find(|b| b.ends_with("side"))
        .unwrap_or_else(|| panic!("side branch not saved: {r}"));
    assert_eq!(tip(&env, side), side_head);
    assert_eq!(show(&env, side, "side.txt"), "side branch work\n");
    assert_eq!(
        show(&env, "shoal/keeper--stash-0", "README.md"),
        "stash two\n"
    );
    assert_eq!(
        show(&env, "shoal/keeper--stash-1", "README.md"),
        "stash one\n"
    );
    assert_eq!(show(&env, "shoal/keeper", "own.txt"), "own branch work\n");
    assert_eq!(show(&env, "shoal/keeper", "src/app.txt"), "uncommitted\n");
    // The branch already in the source is skipped: nothing else was saved.
    let mut in_source = source_branches(&env, "shoal/keeper");
    in_source.sort();
    let mut expected_in_source = extra.clone();
    expected_in_source.push("shoal/keeper".to_string());
    expected_in_source.sort();
    assert_eq!(
        in_source, expected_in_source,
        "branches in the source repository"
    );
    assert_eq!(tip(&env, "main"), main_before, "main moved");
    // The names rule 4 promises.
    assert_eq!(
        extra,
        vec![
            "shoal/keeper--side",
            "shoal/keeper--stash-0",
            "shoal/keeper--stash-1"
        ],
        "{r}"
    );
}

/// Make the session's branch and a branch "other" conflict on src/app.txt.
fn conflicting_branches(env: &Env, s: &Path) {
    write(&s.join("src/app.txt"), "ours\n");
    env.git(s, &["commit", "-q", "-am", "ours"]);
    env.git(s, &["switch", "-q", "-c", "other", "HEAD~1"]);
    write(&s.join("src/app.txt"), "theirs\n");
    env.git(s, &["commit", "-q", "-am", "theirs"]);
    env.git(s, &["switch", "-q", "shoal/busy"]);
}

fn discard_refuses_mid_rebase_and_merge(backend: &'static str) {
    let env = setup(5840, backend);
    let s = fork(&env, "busy");
    conflicting_branches(&env, &s);

    let (ok, out) = git_try(&env, &s, &["rebase", "other"]);
    assert!(!ok, "the rebase was supposed to stop on a conflict: {out}");
    let err = env
        .api("discard", json!({"session": "busy"}))
        .expect_err("discard mid-rebase must be refused");
    assert!(err.contains("rebase"), "unclear refusal: {err}");
    assert!(session_names(&env).contains(&"busy".to_string()));
    assert!(status_names(&env).contains(&"busy".to_string()));
    assert!(s.join(".git").exists(), "session files touched");
    assert!(
        source_branches(&env, "shoal/busy").is_empty(),
        "nothing should be saved by a refused removal"
    );
    env.git(&s, &["rebase", "--abort"]);

    let (ok, out) = git_try(&env, &s, &["merge", "other"]);
    assert!(!ok, "the merge was supposed to stop on a conflict: {out}");
    let err = env
        .api("discard", json!({"session": "busy"}))
        .expect_err("discard mid-merge must be refused");
    assert!(err.contains("merge"), "unclear refusal: {err}");
    assert!(status_names(&env).contains(&"busy".to_string()));
    env.git(&s, &["merge", "--abort"]);

    let r = env
        .api("discard", json!({"session": "busy"}))
        .expect("discard after abort");
    assert_eq!(r["saved"]["import"]["branch"], "shoal/busy", "{r}");
    assert_eq!(show(&env, "shoal/busy", "src/app.txt"), "ours\n");
    // The other branch is saved too (its exact name is checked in
    // discard_saves_branches_and_stashes).
    let extra = r["saved"]["extra_branches"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(extra.len(), 1, "{r}");
    assert_eq!(
        show(&env, extra[0].as_str().unwrap(), "src/app.txt"),
        "theirs\n"
    );
    assert!(!session_names(&env).contains(&"busy".to_string()));
}

fn discard_already_gone(backend: &'static str) {
    let env = setup(5840, backend);
    fork(&env, "gone");
    delete_by_hand(&env, backend, "gone");
    let r = env
        .api("discard", json!({"session": "gone"}))
        .expect("discard");
    assert_eq!(r["already_gone"], true, "{r}");
    assert!(!session_names(&env).contains(&"gone".to_string()));
    assert!(!status_names(&env).contains(&"gone".to_string()));
}

fn worktree_remove_missing_session_clears_record(backend: &'static str) {
    let env = setup(5840, backend);
    let s = fork(&env, "wt");
    let path = s.clone();
    delete_by_hand(&env, backend, "wt");
    let r = env.hook("worktree-remove", &remove_input(&path), &env.repo(), None);
    assert_eq!(r.code, 0, "stderr:\n{}", r.stderr);
    assert!(
        !session_names(&env).contains(&"wt".to_string()),
        "record not cleared; stderr:\n{}",
        r.stderr
    );
}

// Fixed bug: on the overlay backend the session directory is only a mount point;
// the work lives in volumes/<name>/upper. When the mount point is gone but
// the upper layer is not (e.g. an empty, unmounted mount point removed after
// a reboot), WorktreeRemove's "already gone" path force-removes the session
// (ops::remove with force and no save), deleting the upper layer and its
// uncommitted work without saving it, against rule 4 ("Any path that
// removes a session first saves its work").
fn overlay_worktree_remove_keeps_work_in_upper() {
    let env = setup(5840, "overlay");
    let s = fork(&env, "wt-upper");
    write(&s.join("precious.txt"), "uncommitted work\n");
    unmount(&s);
    fs::remove_dir(&s).unwrap();
    let upper = env.project_dir().join("volumes/wt-upper/upper");
    assert!(upper.join("precious.txt").is_file());
    let r = env.hook("worktree-remove", &remove_input(&s), &env.repo(), None);
    assert_eq!(r.code, 0, "stderr:\n{}", r.stderr);
    let saved = ref_exists(&env, &env.repo(), "refs/heads/shoal/wt-upper")
        && show(&env, "shoal/wt-upper", "precious.txt") == "uncommitted work\n";
    let kept = upper.join("precious.txt").is_file();
    assert!(
        saved || kept,
        "the session's uncommitted work was deleted without being saved; stderr:\n{}",
        r.stderr
    );
}

fn stray_directory_blocks_exact_name_only(backend: &'static str) {
    let env = setup(5840, backend);
    // A crash between forgetting a session and deleting its files.
    let stray = env.project_dir().join("sessions/stray");
    write(&stray.join("leftover.txt"), "left by a crash\n");
    let out = env.shoal_in(&env.repo(), &["new", "stray"]);
    assert!(!out.status.success(), "new stray must fail");
    let msg = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        msg.contains("left over") && msg.contains("stray"),
        "unclear message: {msg}"
    );
    assert!(!session_names(&env).contains(&"stray".to_string()));
    // Allocated names skip it.
    let v = env.api("fork", json!({"name": "stray"})).expect("fork");
    let name = v["sessions"][0]["name"].as_str().unwrap().to_string();
    assert_ne!(name, "stray");
    assert!(name.starts_with("stray-"), "{v}");
    assert_eq!(read(stray.join("leftover.txt")), "left by a crash\n");
    // A worktree with that name skips it too.
    let input = json!({
        "session_id": "test-session",
        "transcript_path": "/dev/null",
        "hook_event_name": "WorktreeCreate",
        "cwd": env.repo(),
        "name": "stray",
    });
    let r = env.hook("worktree-create", &input, &env.repo(), None);
    assert_eq!(r.code, 0, "stderr:\n{}", r.stderr);
    let path = PathBuf::from(r.last_line());
    assert_ne!(path.file_name().unwrap(), "stray");
    assert!(path.is_dir());
    assert_eq!(read(stray.join("leftover.txt")), "left by a crash\n");
}

// Fixed bug: scenarios "discard saves branches and stashes" (both backends) and
// "overlay: worktree-remove keeps work in upper" fail (see their functions).
#[test]
fn removal_saves_everything_and_survives_odd_states() {
    let mut scenarios: Vec<(String, Box<dyn FnOnce()>)> = Vec::new();
    for backend in backends() {
        scenarios.push((
            format!("{backend}: discard saves branches and stashes"),
            Box::new(move || discard_saves_branches_and_stashes(backend)),
        ));
        scenarios.push((
            format!("{backend}: discard refuses mid-rebase/merge"),
            Box::new(move || discard_refuses_mid_rebase_and_merge(backend)),
        ));
        scenarios.push((
            format!("{backend}: discard already gone"),
            Box::new(move || discard_already_gone(backend)),
        ));
        scenarios.push((
            format!("{backend}: worktree-remove of a missing session"),
            Box::new(move || worktree_remove_missing_session_clears_record(backend)),
        ));
        scenarios.push((
            format!("{backend}: stray directory"),
            Box::new(move || stray_directory_blocks_exact_name_only(backend)),
        ));
    }
    if backends().contains(&"overlay") {
        scenarios.push((
            "overlay: worktree-remove keeps work in upper".into(),
            Box::new(overlay_worktree_remove_keeps_work_in_upper),
        ));
    }
    run_scenarios(scenarios);
}

// ---------------------------------------------------------------------------
// land never overwrites ignored or untracked files in the user's checkout
// ---------------------------------------------------------------------------

fn land(env: &Env, session: &str) -> Value {
    env.api("land", json!({"session": session, "test": ""}))
        .unwrap_or_else(|e| panic!("land {session}: {e}"))
}

fn files_of(r: &Value) -> Vec<String> {
    r["files"]
        .as_array()
        .unwrap_or_else(|| panic!("no files: {r}"))
        .iter()
        .map(|f| f.as_str().unwrap().to_string())
        .collect()
}

fn land_blocked_by_ignored_file(backend: &'static str) {
    let env = setup(5860, backend);
    let main_before = tip(&env, "main");
    let s = fork(&env, "envy");
    write(&s.join(".env"), "SESSION=1\n");
    env.git(&s, &["add", "-f", ".env"]);
    env.git(&s, &["commit", "-q", "-m", "track .env"]);
    let r = land(&env, "envy");
    assert_eq!(r["status"], "target_blocked", "{r}");
    assert_eq!(files_of(&r), vec![".env"]);
    assert_eq!(read(env.repo().join(".env")), "SECRET=1\n", "user's .env");
    assert_eq!(tip(&env, "main"), main_before, "main moved");
    // Moved aside, it lands.
    fs::rename(env.repo().join(".env"), env.repo().join(".env.mine")).unwrap();
    let r = land(&env, "envy");
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(read(env.repo().join(".env")), "SESSION=1\n");
    assert_eq!(read(env.repo().join(".env.mine")), "SECRET=1\n");
    assert_ne!(tip(&env, "main"), main_before);
}

fn land_blocked_by_untracked_file(backend: &'static str) {
    let env = setup(5860, backend);
    let main_before = tip(&env, "main");
    let s = fork(&env, "notes");
    write(&s.join("docs/notes.txt"), "from the session\n");
    env.git(&s, &["add", "docs/notes.txt"]);
    env.git(&s, &["commit", "-q", "-m", "notes"]);
    write(&env.repo().join("docs/notes.txt"), "the user's own notes\n");
    let r = land(&env, "notes");
    assert_eq!(r["status"], "target_blocked", "{r}");
    assert_eq!(files_of(&r), vec!["docs/notes.txt"]);
    assert_eq!(
        read(env.repo().join("docs/notes.txt")),
        "the user's own notes\n"
    );
    assert_eq!(tip(&env, "main"), main_before, "main moved");
    fs::rename(
        env.repo().join("docs/notes.txt"),
        env.repo().join("notes.mine"),
    )
    .unwrap();
    let r = land(&env, "notes");
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(
        read(env.repo().join("docs/notes.txt")),
        "from the session\n"
    );
    assert_eq!(
        read(env.repo().join("notes.mine")),
        "the user's own notes\n"
    );
}

// Fixed bug: land's "in the way" check only looks for the exact added path in the
// checkout. When the session adds `.env/local` and the user's checkout has
// an ignored *file* `.env`, the check sees nothing at `.env/local`, and
// `git merge --ff-only` silently deletes the ignored file to make room for
// the directory, against rule 4 ("land never overwrites ignored files in the
// user's checkout").
fn land_blocked_by_ignored_file_in_parent_path(backend: &'static str) {
    let env = setup(5860, backend);
    let main_before = tip(&env, "main");
    let s = fork(&env, "envdir");
    fs::remove_file(s.join(".env")).unwrap();
    write(&s.join(".env/local"), "SESSION=dir\n");
    env.git(&s, &["add", "-f", ".env/local"]);
    env.git(&s, &["commit", "-q", "-m", "env dir"]);
    let r = land(&env, "envdir");
    let user_env = env.repo().join(".env");
    let intact = user_env.is_file() && read(&user_env) == "SECRET=1\n";
    assert!(
        intact,
        "the user's ignored .env was overwritten or deleted by land: {r}"
    );
    assert_eq!(r["status"], "target_blocked", "{r}");
    assert_eq!(tip(&env, "main"), main_before, "main moved");
}

// Fixed bug: scenario "ignored file where the session adds a directory" fails on
// both backends (see land_blocked_by_ignored_file_in_parent_path).
#[test]
fn land_never_overwrites_untracked_or_ignored_files() {
    let mut scenarios: Vec<(String, Box<dyn FnOnce()>)> = Vec::new();
    for backend in backends() {
        scenarios.push((
            format!("{backend}: ignored file in the way"),
            Box::new(move || land_blocked_by_ignored_file(backend)),
        ));
        scenarios.push((
            format!("{backend}: untracked file in the way"),
            Box::new(move || land_blocked_by_untracked_file(backend)),
        ));
        scenarios.push((
            format!("{backend}: ignored file where the session adds a directory"),
            Box::new(move || land_blocked_by_ignored_file_in_parent_path(backend)),
        ));
    }
    run_scenarios(scenarios);
}

// ---------------------------------------------------------------------------
// Reused names: both sessions' work is kept
// ---------------------------------------------------------------------------

fn reused_names_keep_both(backend: &'static str) {
    let env = setup(5880, backend);
    let main_before = tip(&env, "main");
    let log_before = env.git(&env.repo(), &["log", "--format=%H", "main"]);

    let s = fork(&env, "dup");
    write(&s.join("first.txt"), "first session\n");
    env.git(&s, &["add", "first.txt"]);
    env.git(&s, &["commit", "-q", "-m", "first"]);
    let r = env
        .api("discard", json!({"session": "dup"}))
        .expect("discard 1");
    assert_eq!(r["saved"]["import"]["branch"], "shoal/dup", "{r}");
    let first_tip = tip(&env, "shoal/dup");

    let s = fork(&env, "dup");
    write(&s.join("second.txt"), "second session\n");
    let r = env
        .api("discard", json!({"session": "dup"}))
        .expect("discard 2");
    assert_eq!(r["saved"]["committed_wip"], true, "{r}");
    assert_eq!(r["saved"]["import"]["branch"], "shoal/dup-2", "{r}");

    assert_eq!(tip(&env, "shoal/dup"), first_tip, "shoal/dup moved");
    assert_eq!(show(&env, "shoal/dup", "first.txt"), "first session\n");
    assert_eq!(show(&env, "shoal/dup-2", "second.txt"), "second session\n");
    let (has_first, _) = git_try(
        &env,
        &env.repo(),
        &["cat-file", "-e", "shoal/dup-2:first.txt"],
    );
    assert!(
        !has_first,
        "the second session's branch must not carry the first's work"
    );
    assert_eq!(tip(&env, "main"), main_before, "main moved");
    assert_eq!(
        env.git(&env.repo(), &["log", "--format=%H", "main"]),
        log_before
    );
    for b in ["shoal/dup", "shoal/dup-2"] {
        env.git(
            &env.repo(),
            &["merge-base", "--is-ancestor", &main_before, b],
        );
    }
    let (fsck_ok, fsck) = git_try(&env, &env.repo(), &["fsck", "--no-progress"]);
    assert!(fsck_ok, "fsck: {fsck}");
}

#[test]
fn reused_names_keep_both_sessions_work() {
    let mut scenarios: Vec<(String, Box<dyn FnOnce()>)> = Vec::new();
    for backend in backends() {
        scenarios.push((
            format!("{backend}: reused names"),
            Box::new(move || reused_names_keep_both(backend)),
        ));
    }
    run_scenarios(scenarios);
}
