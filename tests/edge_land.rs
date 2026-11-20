//! Edge-case tests for `land`: every status it can return, through the JSON
//! API (`ocean-graft api land`) and the CLI (`ocean-graft land`), plus the invariants
//! that protect the user's work: the target branch is only ever
//! fast-forwarded, the user's uncommitted edits in the source repository
//! survive, and a failed land leaves the session where it was.
//!
//! Port range 5300-5399. Nothing in this file listens on a port (land never
//! starts services), so port bases only need to be distinct per test.
mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

fn setup(port: u16, backend: &str) -> Env {
    let env = Env::new(port);
    env.init(backend);
    env
}

/// Fork a session from main; returns (path, port).
fn fork(env: &Env, name: &str) -> PathBuf {
    let v = env.api("fork", json!({"name": name})).expect("fork");
    PathBuf::from(v["sessions"][0]["path"].as_str().unwrap())
}

fn fork_port(env: &Env, name: &str) -> (PathBuf, u64) {
    let v = env.api("fork", json!({"name": name})).expect("fork");
    (
        PathBuf::from(v["sessions"][0]["path"].as_str().unwrap()),
        v["sessions"][0]["port"].as_u64().unwrap(),
    )
}

/// Write `content` to `file` in `dir` and commit it there.
fn commit_file(env: &Env, dir: &Path, file: &str, content: &str, msg: &str) -> String {
    let p = dir.join(file);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
    env.git(dir, &["add", "-A", "--", file]);
    env.git(dir, &["commit", "-q", "-m", msg]);
    head(env, dir)
}

fn head(env: &Env, dir: &Path) -> String {
    env.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn tip(env: &Env, branch: &str) -> String {
    env.git(&env.repo(), &["rev-parse", &format!("refs/heads/{branch}")])
        .trim()
        .to_string()
}

fn ref_exists(env: &Env, r: &str) -> bool {
    env.command("git", &env.repo())
        .args(["rev-parse", "--verify", "-q", r])
        .output()
        .unwrap()
        .status
        .success()
}

fn is_ancestor(env: &Env, dir: &Path, a: &str, b: &str) -> bool {
    env.command("git", dir)
        .args(["merge-base", "--is-ancestor", a, b])
        .output()
        .unwrap()
        .status
        .success()
}

fn porcelain(env: &Env, dir: &Path) -> String {
    env.git(dir, &["status", "--porcelain"])
}

fn rebase_in_progress(env: &Env, dir: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|d| {
        let p = env
            .git(dir, &["rev-parse", "--git-path", d])
            .trim()
            .to_string();
        let p = PathBuf::from(p);
        let p = if p.is_absolute() { p } else { dir.join(p) };
        p.exists()
    })
}

fn land(env: &Env, args: Value) -> Value {
    env.api("land", args.clone())
        .unwrap_or_else(|e| panic!("land {args} failed: {e}"))
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
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

const APP2: &str = "line one\nline TWO from session\nline three\n";

// ---------------------------------------------------------------------------
// landed
// ---------------------------------------------------------------------------

#[test]
fn landed_with_target_checked_out_updates_tree_and_keeps_unrelated_edits() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5300 + i as u16, backend);
        let repo = env.repo();
        let old_main = tip(&env, "main");
        let s = fork(&env, "a");
        let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat: two");

        // The user's own unrelated work in progress in the source repository.
        fs::write(repo.join("README.md"), "demo\nuser edit in progress\n").unwrap();
        fs::write(repo.join("notes.txt"), "untracked user notes\n").unwrap();
        fs::write(repo.join("deps.lock"), "lib 2.0 (staged)\n").unwrap();
        env.git(&repo, &["add", "deps.lock"]);

        let r = land(&env, json!({"session": "a", "test": ""}));
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
        assert_eq!(r["session"], "a");
        assert_eq!(r["target"], "main");
        assert_eq!(r["commits"], 1, "[{backend}] {r}");
        assert_eq!(r["attempts"], 1, "[{backend}] {r}");
        assert!(r["checkpoint"].is_null(), "no rebase was needed: {r}");
        assert!(r["tests"].is_null(), "test=\"\" must skip tests: {r}");
        assert_eq!(
            r["head"],
            shead.as_str(),
            "[{backend}] fast-forward keeps the session's commit"
        );

        // The target moved forward, never sideways.
        assert_eq!(tip(&env, "main"), shead);
        assert!(is_ancestor(&env, &repo, &old_main, &shead));
        // The working tree follows.
        assert_eq!(read(repo.join("src/app.txt")), APP2, "[{backend}]");
        // The user's edits survive, untouched.
        assert_eq!(
            read(repo.join("README.md")),
            "demo\nuser edit in progress\n"
        );
        assert_eq!(read(repo.join("notes.txt")), "untracked user notes\n");
        assert_eq!(read(repo.join("deps.lock")), "lib 2.0 (staged)\n");
        let cached = env.git(&repo, &["diff", "--cached", "--name-only"]);
        assert_eq!(
            cached.trim(),
            "deps.lock",
            "[{backend}] staged edit must stay staged"
        );
        let st = porcelain(&env, &repo);
        assert!(
            !st.contains("src/app.txt"),
            "[{backend}] tree not in sync with main: {st}"
        );
        // Recorded as ocean-graft/<name> in the source repository.
        assert_eq!(tip(&env, "ocean-graft/a"), shead, "[{backend}]");
        assert_eq!(
            env.git(&repo, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main",
            "the user's checkout must not change branch"
        );
    }
}

#[test]
fn landed_when_target_not_checked_out_moves_only_the_ref() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5303 + i as u16, backend);
        let repo = env.repo();
        env.git(&repo, &["switch", "-q", "-c", "feature"]);
        fs::write(repo.join("README.md"), "user edit on feature\n").unwrap();
        let old_main = tip(&env, "main");
        let s = fork(&env, "a");
        let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
        commit_file(&env, &s, "docs/new.md", "new doc\n", "docs");
        let shead2 = head(&env, &s);
        assert_ne!(shead, shead2);

        let r = land(&env, json!({"session": "a", "test": ""}));
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
        assert_eq!(r["commits"], 2, "[{backend}] {r}");
        assert_eq!(tip(&env, "main"), shead2);
        assert!(is_ancestor(&env, &repo, &old_main, &shead2));
        // Working tree untouched: still on feature, old app.txt, no new doc.
        assert_eq!(
            env.git(&repo, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/feature"
        );
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
        assert!(
            !repo.join("docs/new.md").exists(),
            "[{backend}] working tree was changed"
        );
        assert_eq!(read(repo.join("README.md")), "user edit on feature\n");
        assert_eq!(porcelain(&env, &repo).trim(), "M README.md");
        assert_eq!(tip(&env, "feature"), old_main, "feature must not move");
        assert_eq!(tip(&env, "ocean-graft/a"), shead2);
    }
}

#[test]
fn landed_with_detached_head_in_source_moves_only_the_ref() {
    let env = setup(5306, "clone");
    let repo = env.repo();
    let old_main = tip(&env, "main");
    env.git(&repo, &["checkout", "-q", "--detach"]);
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(tip(&env, "main"), shead);
    assert_eq!(
        head(&env, &repo),
        old_main,
        "detached HEAD must stay where it was"
    );
    assert_eq!(
        read(repo.join("src/app.txt")),
        "line one\nline two\nline three\n"
    );
    assert!(porcelain(&env, &repo).is_empty());
}

#[test]
fn landed_by_path_from_inside_the_session() {
    let env = setup(5309, "clone");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    // `path` naming a subdirectory of the session, from the repository.
    let r = land(
        &env,
        json!({"path": s.join("src").to_string_lossy(), "test": ""}),
    );
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(r["session"], "a");
    assert_eq!(tip(&env, "main"), shead);
    // And from the session itself with path ".".
    commit_file(&env, &s, "README.md", "more\n", "more");
    let r = env
        .api_in(
            &s.join("src"),
            "land",
            json!({"path": s.join("src").to_string_lossy(), "test": ""}),
        )
        .unwrap();
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(tip(&env, "main"), head(&env, &s));
}

// ---------------------------------------------------------------------------
// nothing_to_land
// ---------------------------------------------------------------------------

#[test]
fn nothing_to_land_for_a_fresh_session_and_when_landing_twice() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5312 + i as u16, backend);
        let old_main = tip(&env, "main");
        let s = fork(&env, "a");
        let r = land(&env, json!({"session": "a", "test": "false"}));
        assert_eq!(r["status"], "nothing_to_land", "[{backend}] {r}");
        assert_eq!(r["commits"], 0);
        assert!(
            r["tests"].is_null(),
            "tests must not run when there is nothing to land: {r}"
        );
        assert!(r["checkpoint"].is_null());
        assert_eq!(tip(&env, "main"), old_main);

        let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
        assert_eq!(
            land(&env, json!({"session": "a", "test": ""}))["status"],
            "landed"
        );
        let r = land(&env, json!({"session": "a", "test": "false"}));
        assert_eq!(
            r["status"], "nothing_to_land",
            "[{backend}] second land: {r}"
        );
        assert_eq!(tip(&env, "main"), shead);
        assert!(r["message"].as_str().unwrap().contains("main"), "{r}");
    }
}

#[test]
fn nothing_to_land_when_main_moved_but_session_has_no_commits() {
    let env = setup(5315, "clone");
    let s = fork(&env, "a");
    let before = head(&env, &s);
    let new_main = commit_file(&env, &env.repo(), "README.md", "main moved\n", "main");
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "nothing_to_land", "{r}");
    assert_eq!(tip(&env, "main"), new_main);
    assert_eq!(head(&env, &s), before, "the session must not be rewritten");
    assert!(porcelain(&env, &s).is_empty());
}

// Fixed bug: when the session's only commit is already on main as an equivalent
// patch (the user cherry-picked it, or made the same edit), the rebase drops
// it and land reports status "landed" with "landed 0 commit(s)" although
// nothing was delivered; nothing_to_land is the documented status for "the
// target already contains everything".
#[test]
fn patch_already_on_main_is_not_reported_as_landed() {
    let env = setup(5318, "clone");
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat from session");
    // The user makes the same change on main independently.
    let main_tip = commit_file(
        &env,
        &env.repo(),
        "src/app.txt",
        APP2,
        "same change on main",
    );
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(tip(&env, "main"), main_tip, "main must not move: {r}");
    assert!(is_ancestor(
        &env,
        &env.repo(),
        &main_tip,
        &tip(&env, "main")
    ));
    assert_eq!(r["status"], "nothing_to_land", "{r}");
}

// ---------------------------------------------------------------------------
// uncommitted_changes / commit_message
// ---------------------------------------------------------------------------

#[test]
fn uncommitted_changes_lists_files_then_commit_message_lands_them() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5321 + i as u16, backend);
        let repo = env.repo();
        let old_main = tip(&env, "main");
        let s = fork(&env, "a");
        let before = head(&env, &s);
        fs::write(s.join("src/app.txt"), APP2).unwrap();
        fs::write(s.join("new.txt"), "brand new\n").unwrap();
        fs::remove_file(s.join("README.md")).unwrap();
        fs::create_dir_all(s.join("deps")).unwrap();
        fs::write(s.join("deps/ignored.bin"), "ignored\n").unwrap();

        let r = land(&env, json!({"session": "a", "test": "false"}));
        assert_eq!(r["status"], "uncommitted_changes", "[{backend}] {r}");
        let mut files: Vec<String> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_string())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec!["README.md", "new.txt", "src/app.txt"],
            "[{backend}] {r}"
        );
        assert!(
            r["message"].as_str().unwrap().contains("commit_message"),
            "{r}"
        );
        assert!(r["tests"].is_null());
        // Nothing changed anywhere.
        assert_eq!(tip(&env, "main"), old_main);
        assert_eq!(
            head(&env, &s),
            before,
            "[{backend}] land committed without being asked"
        );
        assert!(!ref_exists(&env, "refs/heads/ocean-graft/a"));
        assert_eq!(read(s.join("new.txt")), "brand new\n");

        let r = land(
            &env,
            json!({"session": "a", "test": "", "commit_message": "feat: all of it"}),
        );
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
        assert_eq!(r["commits"], 1);
        let new_main = tip(&env, "main");
        assert!(is_ancestor(&env, &repo, &old_main, &new_main));
        assert_eq!(
            env.git(&repo, &["log", "-1", "--format=%s", "main"]).trim(),
            "feat: all of it"
        );
        let tree = env.git(&repo, &["ls-tree", "-r", "--name-only", "main"]);
        assert!(tree.lines().any(|l| l == "new.txt"), "{tree}");
        assert!(
            !tree.lines().any(|l| l == "README.md"),
            "deletion not landed: {tree}"
        );
        assert!(
            !tree.contains("deps/"),
            "[{backend}] ignored files must never be committed: {tree}"
        );
        assert_eq!(read(repo.join("src/app.txt")), APP2);
        assert!(!repo.join("README.md").exists());
        assert!(
            porcelain(&env, &s).is_empty(),
            "[{backend}] session left dirty"
        );
    }
}

#[test]
fn blank_commit_message_does_not_commit() {
    let env = setup(5324, "clone");
    let s = fork(&env, "a");
    let before = head(&env, &s);
    fs::write(s.join("src/app.txt"), APP2).unwrap();
    for msg in ["", "   ", "\n\t"] {
        let r = land(
            &env,
            json!({"session": "a", "test": "", "commit_message": msg}),
        );
        assert_eq!(r["status"], "uncommitted_changes", "message {msg:?}: {r}");
        assert_eq!(head(&env, &s), before);
    }
    assert_eq!(read(s.join("src/app.txt")), APP2);
}

#[test]
fn staged_and_renamed_changes_count_as_uncommitted() {
    let env = setup(5327, "clone");
    let s = fork(&env, "a");
    env.git(&s, &["mv", "README.md", "README2.md"]);
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "uncommitted_changes", "{r}");
    let files = r["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{r}");
    assert!(files[0].as_str().unwrap().contains("README2.md"), "{r}");
    let r = land(
        &env,
        json!({"session": "a", "test": "", "commit_message": "rename"}),
    );
    assert_eq!(r["status"], "landed", "{r}");
    assert!(env.repo().join("README2.md").exists());
    assert!(!env.repo().join("README.md").exists());
}

// Fixed bug: uncommitted file names are cut out of `git status --porcelain` lines
// verbatim, so names with spaces or non-ASCII characters come back
// C-quoted ("\"my notes.txt\"", "\"caf\\303\\251.txt\"") instead of as the
// actual paths Claude would pass to git or open.
#[test]
fn uncommitted_file_names_with_spaces_and_unicode_are_reported_as_paths() {
    let env = setup(5330, "clone");
    let s = fork(&env, "a");
    fs::write(s.join("my notes.txt"), "x\n").unwrap();
    fs::write(s.join("café.txt"), "y\n").unwrap();
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "uncommitted_changes", "{r}");
    let mut files: Vec<&str> = r["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, vec!["café.txt", "my notes.txt"], "{r}");
}

// ---------------------------------------------------------------------------
// conflicts
// ---------------------------------------------------------------------------

#[test]
fn conflicts_leave_the_session_exactly_as_before() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5333 + i as u16, backend);
        let repo = env.repo();
        let s = fork(&env, "a");
        commit_file(&env, &s, "README.md", "session readme\n", "readme");
        let shead = commit_file(
            &env,
            &s,
            "src/app.txt",
            "line one\nSESSION two\nline three\n",
            "two",
        );
        fs::create_dir_all(s.join("deps")).unwrap();
        fs::write(s.join("deps/build.out"), "ignored build output\n").unwrap();
        let main_tip = commit_file(
            &env,
            &repo,
            "src/app.txt",
            "line one\nMAIN two\nline three\n",
            "main two",
        );
        let branch = env.git(&s, &["symbolic-ref", "HEAD"]);

        let r = land(&env, json!({"session": "a", "test": "true"}));
        assert_eq!(r["status"], "conflicts", "[{backend}] {r}");
        assert_eq!(r["files"], json!(["src/app.txt"]), "[{backend}] {r}");
        assert!(
            r["tests"].is_null(),
            "tests must not run on a conflict: {r}"
        );
        assert!(r["head"].is_null() || r["head"] != json!(main_tip));
        // The session is exactly as it was.
        assert_eq!(head(&env, &s), shead, "[{backend}] HEAD moved");
        assert_eq!(
            env.git(&s, &["symbolic-ref", "HEAD"]),
            branch,
            "[{backend}] branch changed"
        );
        assert!(
            !rebase_in_progress(&env, &s),
            "[{backend}] rebase left in progress"
        );
        assert!(
            porcelain(&env, &s).is_empty(),
            "[{backend}] {}",
            porcelain(&env, &s)
        );
        assert_eq!(
            read(s.join("src/app.txt")),
            "line one\nSESSION two\nline three\n"
        );
        assert_eq!(read(s.join("README.md")), "session readme\n");
        assert_eq!(read(s.join("deps/build.out")), "ignored build output\n");
        // Nothing reached the target.
        assert_eq!(tip(&env, "main"), main_tip);
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nMAIN two\nline three\n"
        );
        let msg = r["message"].as_str().unwrap();
        assert!(msg.contains("unchanged") && msg.contains("merge"), "{msg}");
        // A second attempt gives the same answer (no state left behind).
        let r2 = land(&env, json!({"session": "a", "test": "true"}));
        assert_eq!(r2["status"], "conflicts", "[{backend}] {r2}");
        assert_eq!(head(&env, &s), shead);
    }
}

#[test]
fn conflicts_after_commit_message_keep_the_work_committed() {
    let env = setup(5336, "clone");
    let s = fork(&env, "a");
    fs::write(s.join("src/app.txt"), "line one\nSESSION two\nline three\n").unwrap();
    let main_tip = commit_file(
        &env,
        &env.repo(),
        "src/app.txt",
        "line one\nMAIN two\nline three\n",
        "m",
    );
    let r = land(
        &env,
        json!({"session": "a", "test": "", "commit_message": "wip"}),
    );
    assert_eq!(r["status"], "conflicts", "{r}");
    assert_eq!(tip(&env, "main"), main_tip);
    assert!(!rebase_in_progress(&env, &s));
    assert!(porcelain(&env, &s).is_empty());
    assert_eq!(
        read(s.join("src/app.txt")),
        "line one\nSESSION two\nline three\n"
    );
    assert_eq!(env.git(&s, &["log", "-1", "--format=%s"]).trim(), "wip");
}

#[test]
fn conflict_resolved_by_merging_main_then_lands() {
    let env = setup(5339, "clone");
    let repo = env.repo();
    let s = fork(&env, "a");
    commit_file(
        &env,
        &s,
        "src/app.txt",
        "line one\nSESSION two\nline three\n",
        "s",
    );
    let main_tip = commit_file(
        &env,
        &repo,
        "src/app.txt",
        "line one\nMAIN two\nline three\n",
        "m",
    );
    assert_eq!(
        land(&env, json!({"session": "a", "test": ""}))["status"],
        "conflicts"
    );
    // Follow the advice in the message.
    env.git(&s, &["fetch", "-q", &repo.to_string_lossy(), "main"]);
    let merged = env
        .command("git", &s)
        .args(["merge", "-q", "FETCH_HEAD"])
        .output()
        .unwrap();
    assert!(!merged.status.success(), "expected a conflict");
    fs::write(s.join("src/app.txt"), "line one\nBOTH two\nline three\n").unwrap();
    env.git(&s, &["add", "src/app.txt"]);
    env.git(&s, &["commit", "-q", "--no-edit"]);
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "landed", "{r}");
    assert!(is_ancestor(&env, &repo, &main_tip, &tip(&env, "main")));
    assert_eq!(
        read(repo.join("src/app.txt")),
        "line one\nBOTH two\nline three\n"
    );
}

// ---------------------------------------------------------------------------
// tests_failed and checkpoints
// ---------------------------------------------------------------------------

#[test]
fn tests_failed_returns_tail_and_checkpoint_that_rewinds_the_rebase() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5342 + i as u16, backend);
        let repo = env.repo();
        let s = fork(&env, "a");
        let pre = commit_file(&env, &s, "src/app.txt", APP2, "feat");
        fs::create_dir_all(s.join("deps")).unwrap();
        fs::write(s.join("deps/cache"), "c\n").unwrap();
        let main_tip = commit_file(&env, &repo, "README.md", "main moved\n", "m");

        let test = "i=1; while [ $i -le 100 ]; do echo out-line-$i; i=$((i+1)); done; echo to-stderr >&2; exit 3";
        let r = land(&env, json!({"session": "a", "test": test}));
        assert_eq!(r["status"], "tests_failed", "[{backend}] {r}");
        let t = &r["tests"];
        assert_eq!(t["passed"], false);
        assert_eq!(t["exit_code"], 3, "{t}");
        assert_eq!(t["timed_out"], false);
        assert_eq!(t["command"], test);
        let tail = t["output_tail"].as_str().unwrap();
        assert!(tail.contains("out-line-100"), "{tail}");
        assert!(
            tail.contains("to-stderr"),
            "stderr must be captured: {tail}"
        );
        assert!(tail.contains("out-line-42"), "{tail}");
        assert!(
            !tail.contains("out-line-1\n") && !tail.contains("out-line-40\n"),
            "tail too long: {tail}"
        );
        assert!(tail.lines().count() <= 60, "{}", tail.lines().count());
        let label = r["checkpoint"]
            .as_str()
            .expect("checkpoint label")
            .to_string();
        assert_eq!(label, "pre-land-1", "[{backend}] {r}");
        assert!(r["message"].as_str().unwrap().contains(&label), "{r}");

        // Nothing reached the target.
        assert_eq!(tip(&env, "main"), main_tip, "[{backend}]");
        assert!(
            !ref_exists(&env, "refs/heads/ocean-graft/a"),
            "[{backend}] recorded before tests passed"
        );
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
        // The session is rebased onto main now.
        let rebased = head(&env, &s);
        assert_ne!(rebased, pre);
        assert!(is_ancestor(&env, &s, &main_tip, &rebased));

        // Rewinding to the label restores the pre-rebase HEAD and files.
        let rw = env
            .api("rewind", json!({"session": "a", "label": label}))
            .unwrap();
        assert_eq!(
            head(&env, &s),
            pre,
            "[{backend}] rewind did not restore the pre-land HEAD: {rw}"
        );
        assert_eq!(read(s.join("README.md")), "demo\n");
        assert_eq!(read(s.join("deps/cache")), "c\n");
        assert!(porcelain(&env, &s).is_empty());

        // A second failing land takes the next label.
        let r = land(&env, json!({"session": "a", "test": "exit 1"}));
        assert_eq!(r["status"], "tests_failed", "{r}");
        assert_eq!(r["checkpoint"], "pre-land-2", "[{backend}] {r}");
        assert_eq!(tip(&env, "main"), main_tip);
        // And a passing one lands.
        let r = land(&env, json!({"session": "a", "test": "true"}));
        assert_eq!(r["status"], "landed", "{r}");
        assert_eq!(r["tests"]["passed"], true);
        assert!(is_ancestor(&env, &repo, &main_tip, &tip(&env, "main")));
    }
}

#[test]
fn tests_failed_without_a_rebase_has_no_checkpoint() {
    let env = setup(5345, "clone");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let old = tip(&env, "main");
    let r = land(&env, json!({"session": "a", "test": "false"}));
    assert_eq!(r["status"], "tests_failed", "{r}");
    assert!(r["checkpoint"].is_null(), "{r}");
    assert_eq!(r["tests"]["exit_code"], 1);
    assert_eq!(head(&env, &s), shead);
    assert_eq!(tip(&env, "main"), old);
}

#[test]
fn test_command_runs_in_the_session_with_ci_env_and_null_stdin() {
    let env = setup(5348, "clone");
    let (s, port) = fork_port(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let out = env.root.path().join("test-env.txt");
    // `cat` would hang forever if stdin were inherited from a terminal/pipe.
    let test = format!(
        "echo \"$CI|$OCEAN_GRAFT_SESSION|$OCEAN_GRAFT_PORT|$(pwd -P)\" > {}; cat; echo NOISE-ON-STDOUT; test -f src/app.txt",
        sh_quote(&out)
    );
    let start = Instant::now();
    let r = land(
        &env,
        json!({"session": "a", "test": test, "timeout_secs": 20}),
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "test command waited on stdin"
    );
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(r["tests"]["passed"], true);
    assert!(r["tests"]["output_tail"]
        .as_str()
        .unwrap()
        .contains("NOISE-ON-STDOUT"));
    let got = read(&out);
    let parts: Vec<&str> = got.trim().split('|').collect();
    assert_eq!(parts[0], "1", "{got}");
    assert_eq!(parts[1], "a", "{got}");
    assert_eq!(parts[2], port.to_string(), "{got}");
    assert_eq!(Path::new(parts[3]), s.canonicalize().unwrap(), "{got}");
}

#[test]
fn test_timeout_kills_the_whole_process_group() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5351 + i as u16, backend);
        let s = fork(&env, "a");
        commit_file(&env, &s, "src/app.txt", APP2, "feat");
        let old = tip(&env, "main");
        let bg = env.root.path().join("bg.pid");
        let fg = env.root.path().join("fg.pid");
        let test = format!(
            "sleep 60 & echo $! > {}; echo $$ > {}; sleep 60",
            sh_quote(&bg),
            sh_quote(&fg)
        );
        let start = Instant::now();
        let r = land(
            &env,
            json!({"session": "a", "test": test, "timeout_secs": 1}),
        );
        let took = start.elapsed();
        assert_eq!(r["status"], "timed_out", "[{backend}] {r}");
        assert_eq!(r["tests"]["timed_out"], true, "{r}");
        assert_eq!(r["tests"]["passed"], false);
        assert!(r["tests"]["exit_code"].is_null(), "{r}");
        // timeout_secs bounds the whole land (rebase and queue included), so
        // the tests themselves may get a little under a second of it.
        let ms = r["tests"]["duration_ms"].as_u64().unwrap();
        assert!(ms < 10_000, "duration {ms}ms");
        assert!(
            took >= Duration::from_secs(1),
            "land ended before its time limit: {took:?}"
        );
        assert!(took < Duration::from_secs(20), "land took {took:?}");
        for f in [&bg, &fg] {
            let pid: i32 = read(f).trim().parse().unwrap();
            assert!(
                wait_for(Duration::from_secs(3), || pid_gone(pid)),
                "[{backend}] process {pid} from the test command survived the timeout"
            );
        }
        assert_eq!(tip(&env, "main"), old);
        // The queue slot was released: the next land runs at once.
        let start = Instant::now();
        let r = land(
            &env,
            json!({"session": "a", "test": "true", "timeout_secs": 30}),
        );
        assert_eq!(r["status"], "landed", "{r}");
        assert!(start.elapsed() < Duration::from_secs(15));
    }
}

#[test]
fn zero_timeout_is_clamped_to_one_second_not_unlimited() {
    let env = setup(5354, "clone");
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let start = Instant::now();
    let r = land(
        &env,
        json!({"session": "a", "test": "sleep 30", "timeout_secs": 0}),
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "timeout_secs 0 ran without a limit"
    );
    assert_eq!(r["status"], "timed_out", "{r}");
    assert_eq!(r["tests"]["timed_out"], true);
}

#[test]
fn invalid_land_arguments_are_rejected_before_anything_happens() {
    let env = setup(5357, "clone");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let old = tip(&env, "main");
    for (args, needle) in [
        (json!({"session": "a", "timeout_secs": "5"}), "timeout_secs"),
        (json!({"session": "a", "timeout_secs": -1}), "timeout_secs"),
        (json!({"session": "a", "timeout_secs": 1.5}), "timeout_secs"),
        (json!({"session": "a", "test": 7}), "test"),
        (json!({"session": "a", "into": ["main"]}), "into"),
        (
            json!({"session": "a", "commit_message": true}),
            "commit_message",
        ),
        (json!({"session": "a", "force": true}), "force"),
        (json!({"session": "ghost"}), "ghost"),
        (json!({}), "session"),
    ] {
        let e = env
            .api("land", args.clone())
            .expect_err(&format!("{args} should fail"));
        assert!(e.contains(needle), "{args}: {e}");
    }
    assert_eq!(tip(&env, "main"), old);
    assert_eq!(head(&env, &s), shead);
    assert!(!ref_exists(&env, "refs/heads/ocean-graft/a"));
}

// ---------------------------------------------------------------------------
// target_blocked
// ---------------------------------------------------------------------------

#[test]
fn target_blocked_by_user_edit_keeps_the_edit_and_lands_after() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5360 + i as u16, backend);
        let repo = env.repo();
        let old = tip(&env, "main");
        let s = fork(&env, "a");
        let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
        let mine = "line one\nmy own edit, do not lose\nline three\n";
        fs::write(repo.join("src/app.txt"), mine).unwrap();

        let r = land(&env, json!({"session": "a", "test": ""}));
        assert_eq!(r["status"], "target_blocked", "[{backend}] {r}");
        assert_eq!(
            read(repo.join("src/app.txt")),
            mine,
            "[{backend}] the user's edit was touched"
        );
        assert_eq!(tip(&env, "main"), old, "[{backend}]");
        assert_eq!(r["head"], shead.as_str());
        assert_eq!(
            tip(&env, "ocean-graft/a"),
            shead,
            "work must be saved as ocean-graft/a"
        );
        assert!(
            r["message"].as_str().unwrap().contains("ocean-graft/a"),
            "{r}"
        );
        assert!(!rebase_in_progress(&env, &repo));
        assert!(!repo.join(".git/MERGE_HEAD").exists());

        // Once the user's edit is out of the way the same land succeeds.
        env.git(&repo, &["checkout", "--", "src/app.txt"]);
        let r = land(&env, json!({"session": "a", "test": ""}));
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
        assert_eq!(tip(&env, "main"), shead);
        assert_eq!(read(repo.join("src/app.txt")), APP2);
    }
}

#[test]
fn target_blocked_by_staged_edit_keeps_the_index() {
    let env = setup(5363, "clone");
    let repo = env.repo();
    let old = tip(&env, "main");
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    fs::write(repo.join("src/app.txt"), "staged by user\n").unwrap();
    env.git(&repo, &["add", "src/app.txt"]);
    fs::write(repo.join("src/app.txt"), "staged by user\nplus unstaged\n").unwrap();
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "target_blocked", "{r}");
    assert_eq!(tip(&env, "main"), old);
    assert_eq!(
        read(repo.join("src/app.txt")),
        "staged by user\nplus unstaged\n"
    );
    assert_eq!(
        env.git(&repo, &["show", ":src/app.txt"]),
        "staged by user\n"
    );
}

#[test]
fn target_blocked_by_untracked_file_in_the_way() {
    let env = setup(5366, "clone");
    let repo = env.repo();
    let old = tip(&env, "main");
    let s = fork(&env, "a");
    commit_file(&env, &s, "docs/guide.md", "from session\n", "docs");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "the user's untracked draft\n").unwrap();
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "target_blocked", "{r}");
    assert_eq!(tip(&env, "main"), old);
    assert_eq!(
        read(repo.join("docs/guide.md")),
        "the user's untracked draft\n"
    );
}

#[test]
fn target_checked_out_in_a_linked_worktree_is_never_desynchronised() {
    let env = setup(5369, "clone");
    let repo = env.repo();
    env.git(&repo, &["switch", "-q", "-c", "feature"]);
    let wt = env.root.path().join("wt-main");
    env.git(
        &repo,
        &["worktree", "add", "-q", &wt.to_string_lossy(), "main"],
    );
    let old = tip(&env, "main");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let r = land(&env, json!({"session": "a", "test": ""}));
    let st = porcelain(&env, &wt);
    match r["status"].as_str().unwrap() {
        "landed" => {
            assert_eq!(tip(&env, "main"), shead);
            assert!(
                st.is_empty(),
                "main moved under the linked worktree, which now shows a reversal: {st}"
            );
            assert_eq!(read(wt.join("src/app.txt")), APP2);
        }
        "target_blocked" => {
            assert_eq!(tip(&env, "main"), old);
            assert!(st.is_empty(), "{st}");
            assert_eq!(tip(&env, "ocean-graft/a"), shead);
        }
        other => panic!("unexpected status {other}: {r}"),
    }
}

// ---------------------------------------------------------------------------
// into
// ---------------------------------------------------------------------------

#[test]
fn into_another_branch_moves_only_that_branch() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5372 + i as u16, backend);
        let repo = env.repo();
        let initial = tip(&env, "main");
        env.git(&repo, &["branch", "release"]);
        let s = fork(&env, "a");
        let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
        let r = land(&env, json!({"session": "a", "test": "", "into": "release"}));
        assert_eq!(r["status"], "landed", "[{backend}] {r}");
        assert_eq!(r["target"], "release");
        assert_eq!(tip(&env, "release"), shead);
        assert_eq!(tip(&env, "main"), initial, "[{backend}] main must not move");
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
        assert!(porcelain(&env, &repo).is_empty());
        // Landing again into release is nothing_to_land; into main still lands.
        assert_eq!(
            land(&env, json!({"session": "a", "test": "", "into": "release"}))["status"],
            "nothing_to_land"
        );
        let r = land(&env, json!({"session": "a", "test": ""}));
        assert_eq!(r["status"], "landed", "{r}");
        assert_eq!(tip(&env, "main"), shead);
    }
}

#[test]
fn into_a_checked_out_other_branch_updates_the_working_tree() {
    let env = setup(5375, "clone");
    let repo = env.repo();
    let initial = tip(&env, "main");
    env.git(&repo, &["switch", "-q", "-c", "release"]);
    // release has its own extra commit the session never saw.
    let rel = commit_file(&env, &repo, "CHANGELOG.md", "release notes\n", "rel");
    let s = fork(&env, "a");
    let pre = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let r = land(&env, json!({"session": "a", "test": "", "into": "release"}));
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(
        r["checkpoint"], "pre-land-1",
        "rebase onto release needs a checkpoint: {r}"
    );
    let new = tip(&env, "release");
    assert_ne!(new, pre);
    assert!(is_ancestor(&env, &repo, &rel, &new));
    assert_eq!(read(repo.join("src/app.txt")), APP2);
    assert_eq!(read(repo.join("CHANGELOG.md")), "release notes\n");
    assert_eq!(tip(&env, "main"), initial);
    assert!(porcelain(&env, &repo).is_empty());
}

#[test]
fn into_a_missing_branch_is_a_clear_error() {
    let env = setup(5378, "clone");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let old = tip(&env, "main");
    let e = env
        .api(
            "land",
            json!({"session": "a", "test": "", "into": "no-such-branch"}),
        )
        .expect_err("landing into a missing branch must fail");
    assert!(e.contains("no-such-branch"), "{e}");
    assert!(
        !ref_exists(&env, "refs/heads/no-such-branch"),
        "branch was created"
    );
    assert_eq!(tip(&env, "main"), old);
    assert_eq!(head(&env, &s), shead);
    assert!(!ref_exists(&env, "refs/heads/ocean-graft/a"));
    // CLI: non-zero and the same message on stderr.
    let out = env.ocean_graft_in(
        &env.repo(),
        &["land", "a", "--no-test", "--into", "no-such-branch"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no-such-branch"), "{}", stderr(&out));
}

// Fixed bug: with commit_message, land commits the session's uncommitted work
// before it checks that the target branch exists, so a call that fails
// with "no branch X" has still rewritten the session (its work is now a
// commit). A rejected call should not change anything.
#[test]
fn into_a_missing_branch_does_not_commit_the_session_first() {
    let env = setup(5381, "clone");
    let s = fork(&env, "a");
    let before = head(&env, &s);
    fs::write(s.join("src/app.txt"), APP2).unwrap();
    let e = env
        .api(
            "land",
            json!({"session": "a", "test": "", "into": "nope", "commit_message": "x"}),
        )
        .expect_err("must fail");
    assert!(e.contains("nope"), "{e}");
    assert_eq!(read(s.join("src/app.txt")), APP2, "work lost");
    assert_eq!(
        head(&env, &s),
        before,
        "a failed land committed the session's work"
    );
    assert!(!porcelain(&env, &s).is_empty());
}

#[test]
fn into_revision_expressions_and_malformed_names_never_touch_main() {
    let env = setup(5384, "clone");
    let repo = env.repo();
    commit_file(&env, &repo, "README.md", "second\n", "second");
    let old = tip(&env, "main");
    let s = fork(&env, "a");
    let shead = commit_file(&env, &s, "src/app.txt", APP2, "feat");
    for into in [
        "",
        "main~1",
        "main^",
        "../main",
        "refs/heads/main",
        "main..HEAD",
        "-main",
        "HEAD",
    ] {
        let res = env.api("land", json!({"session": "a", "test": "", "into": into}));
        assert!(
            res.is_err(),
            "into {into:?} should be rejected, got {res:?}"
        );
        assert_eq!(tip(&env, "main"), old, "into {into:?} moved main");
        assert_eq!(head(&env, &s), shead, "into {into:?} changed the session");
        assert!(!rebase_in_progress(&env, &s));
    }
    let refs = env.git(&repo, &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(
        refs.lines()
            .filter(|l| l.starts_with("refs/heads/"))
            .count(),
        1,
        "stray branches created: {refs}"
    );
}

// ---------------------------------------------------------------------------
// ocean-graft.toml test command
// ---------------------------------------------------------------------------

#[test]
fn tests_come_from_ocean_graft_toml_and_empty_test_skips_them() {
    let env = setup(5387, "clone");
    let repo = env.repo();
    let marker = env.root.path().join("toml-test-ran");
    let cmd = format!("touch {}; echo toml-test-output; exit 4", sh_quote(&marker));
    fs::write(
        repo.join("ocean-graft.toml"),
        format!("test = {}\n", toml_str(&cmd)),
    )
    .unwrap();
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let old = tip(&env, "main");

    let r = land(&env, json!({"session": "a"}));
    assert_eq!(r["status"], "tests_failed", "{r}");
    assert_eq!(r["tests"]["command"], cmd.as_str());
    assert_eq!(r["tests"]["exit_code"], 4);
    assert!(r["tests"]["output_tail"]
        .as_str()
        .unwrap()
        .contains("toml-test-output"));
    assert!(marker.exists());
    assert_eq!(tip(&env, "main"), old);

    // An explicit test overrides ocean-graft.toml.
    fs::remove_file(&marker).unwrap();
    let r = land(&env, json!({"session": "a", "test": "true"}));
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(r["tests"]["command"], "true");
    assert!(
        !marker.exists(),
        "ocean-graft.toml test ran despite an explicit test"
    );

    // test="" skips even though ocean-graft.toml has a failing test.
    let s2 = fork(&env, "b");
    commit_file(&env, &s2, "README.md", "b\n", "b");
    let r = land(&env, json!({"session": "b", "test": ""}));
    assert_eq!(r["status"], "landed", "{r}");
    assert!(r["tests"].is_null(), "{r}");
    assert!(!marker.exists(), "ocean-graft.toml test ran with test=\"\"");
    // Whitespace-only test also skips.
    commit_file(&env, &s2, "README.md", "b2\n", "b2");
    let r = land(&env, json!({"session": "b", "test": "   "}));
    assert_eq!(r["status"], "landed", "{r}");
    assert!(!marker.exists());

    // CLI: --no-test skips it, plain `ocean-graft land` runs it.
    commit_file(&env, &s2, "README.md", "b3\n", "b3");
    let out = env.ocean_graft_in(&repo, &["land", "b"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).starts_with("tests_failed:"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("toml-test-output"),
        "{}",
        stdout(&out)
    );
    assert!(marker.exists());
    let out = env.ocean_graft_in(&repo, &["land", "b", "--no-test"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(stdout(&out).starts_with("landed:"), "{}", stdout(&out));
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[test]
fn cli_land_statuses_and_exit_codes() {
    let env = setup(5390, "clone");
    let repo = env.repo();
    let s = fork(&env, "a");

    let out = env.ocean_graft_in(&repo, &["land", "a", "--no-test"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("nothing_to_land:"),
        "{}",
        stdout(&out)
    );

    fs::write(s.join("src/app.txt"), APP2).unwrap();
    let out = env.ocean_graft_in(&repo, &["land", "a", "--no-test"]);
    assert_eq!(out.status.code(), Some(3));
    let o = stdout(&out);
    assert!(
        o.starts_with("uncommitted_changes:") && o.contains("files: src/app.txt"),
        "{o}"
    );

    let out = env.ocean_graft_in(&repo, &["land", "a", "--test", "true", "--no-test"]);
    assert!(!out.status.success(), "--test and --no-test must conflict");
    assert_eq!(read(s.join("src/app.txt")), APP2);

    let out = env.ocean_graft_in(&repo, &["land", "a", "-m", "feat: cli", "--test", "exit 9"]);
    assert_eq!(out.status.code(), Some(3));
    let o = stdout(&out);
    assert!(
        o.starts_with("tests_failed:") && o.contains("failed"),
        "{o}"
    );

    let out = env.ocean_graft_in(
        &repo,
        &["land", "a", "--test", "sleep 20", "--timeout", "1"],
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(stdout(&out).contains("timed out"), "{}", stdout(&out));

    let out = env.ocean_graft_in(&repo, &["land", "a", "--test", "true"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(stdout(&out).starts_with("landed:"), "{}", stdout(&out));
    assert_eq!(
        env.git(&repo, &["log", "-1", "--format=%s", "main"]).trim(),
        "feat: cli"
    );
    assert_eq!(read(repo.join("src/app.txt")), APP2);

    let out = env.ocean_graft_in(&repo, &["land", "ghost", "--no-test"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("ghost"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Sequences and concurrency
// ---------------------------------------------------------------------------

#[test]
fn two_sessions_land_in_sequence_and_the_second_rebases_over_the_first() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = setup(5393 + i as u16, backend);
        let repo = env.repo();
        let initial = tip(&env, "main");
        let a = fork(&env, "a");
        let b = fork(&env, "b");
        let ah = commit_file(&env, &a, "README.md", "from a\n", "a");
        let bh = commit_file(&env, &b, "src/app.txt", APP2, "b");

        let ra = land(&env, json!({"session": "a", "test": "test -f README.md"}));
        assert_eq!(ra["status"], "landed", "[{backend}] {ra}");
        assert_eq!(tip(&env, "main"), ah);
        let rb = land(
            &env,
            json!({"session": "b", "test": "grep -q 'from a' README.md"}),
        );
        assert_eq!(
            rb["status"], "landed",
            "[{backend}] tests must see a's work: {rb}"
        );
        assert_eq!(rb["checkpoint"], "pre-land-1", "{rb}");
        assert_eq!(rb["commits"], 1);
        let new = tip(&env, "main");
        assert_ne!(new, bh, "b had to be rebased");
        assert_eq!(rb["head"], new.as_str());
        assert_eq!(env.git(&repo, &["rev-parse", "main^"]).trim(), ah);
        for old in [&initial, &ah] {
            assert!(is_ancestor(&env, &repo, old, &new));
        }
        assert_eq!(read(repo.join("README.md")), "from a\n");
        assert_eq!(read(repo.join("src/app.txt")), APP2);
        assert_eq!(tip(&env, "ocean-graft/b"), new);
        assert_eq!(tip(&env, "ocean-graft/a"), ah);
        assert!(porcelain(&env, &repo).is_empty());
    }
}

/// The target moves exactly once, while land's tests run: the first
/// fast-forward fails, land retries, rebases over the new commit and lands.
fn moves_once_during_tests(env: &Env, checked_out: bool) {
    let repo = env.repo();
    if !checked_out {
        env.git(&repo, &["switch", "-q", "-c", "elsewhere"]);
    }
    let initial = tip(env, "main");
    let s = fork(env, "a");
    commit_file(env, &s, "src/app.txt", APP2, "feat");
    let started = env.root.path().join("started");
    let go = env.root.path().join("go");
    let test = format!(
        "touch {}; n=0; while [ ! -f {} ] && [ $n -lt 400 ]; do sleep 0.05; n=$((n+1)); done; test -f {}",
        sh_quote(&started),
        sh_quote(&go),
        sh_quote(&go)
    );
    let mover = {
        let started = started.clone();
        let go = go.clone();
        let repo = repo.clone();
        let home = env.home();
        std::thread::spawn(move || {
            assert!(
                wait_for(Duration::from_secs(20), || started.exists()),
                "tests never started"
            );
            let cmd = |args: &[&str]| {
                let out = std::process::Command::new("git")
                    .current_dir(&repo)
                    .env("OCEAN_GRAFT_HOME", &home)
                    .env("GIT_AUTHOR_NAME", "Bg")
                    .env("GIT_AUTHOR_EMAIL", "bg@example.invalid")
                    .env("GIT_COMMITTER_NAME", "Bg")
                    .env("GIT_COMMITTER_EMAIL", "bg@example.invalid")
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let c = if checked_out {
                fs::write(repo.join("moved.txt"), "moved\n").unwrap();
                cmd(&["add", "moved.txt"]);
                cmd(&["commit", "-q", "-m", "moved"]);
                cmd(&["rev-parse", "HEAD"])
            } else {
                let old = cmd(&["rev-parse", "refs/heads/main"]);
                let tree = cmd(&["rev-parse", &format!("{old}^{{tree}}")]);
                let c = cmd(&["commit-tree", &tree, "-p", &old, "-m", "moved"]);
                cmd(&["update-ref", "refs/heads/main", &c, &old]);
                c
            };
            fs::write(&go, "").unwrap();
            c
        })
    };
    let r = land(
        env,
        json!({"session": "a", "test": test, "timeout_secs": 60}),
    );
    let moved = mover.join().unwrap();
    assert_eq!(r["status"], "landed", "checked_out={checked_out}: {r}");
    assert_eq!(r["attempts"], 2, "{r}");
    let new = tip(env, "main");
    assert_eq!(r["head"], new.as_str());
    assert!(is_ancestor(env, &repo, &initial, &new));
    assert!(
        is_ancestor(env, &repo, &moved, &new),
        "the concurrent commit was dropped"
    );
    assert_eq!(env.git(&repo, &["rev-parse", "main^"]).trim(), moved);
    if checked_out {
        assert_eq!(read(repo.join("src/app.txt")), APP2);
        assert_eq!(read(repo.join("moved.txt")), "moved\n");
        assert!(
            porcelain(env, &repo).is_empty(),
            "{}",
            porcelain(env, &repo)
        );
    } else {
        assert_eq!(
            read(repo.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );
    }
}

#[test]
fn target_moving_once_during_tests_retries_and_lands_checked_out() {
    let env = setup(5396, "clone");
    moves_once_during_tests(&env, true);
}

#[test]
fn target_moving_once_during_tests_retries_and_lands_not_checked_out() {
    let env = setup(5397, "clone");
    moves_once_during_tests(&env, false);
}

#[test]
fn target_that_keeps_moving_ends_in_target_moving_and_main_is_never_rewritten() {
    let env = setup(5398, "clone");
    let repo = env.repo();
    env.git(&repo, &["switch", "-q", "-c", "elsewhere"]);
    let initial = tip(&env, "main");
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let stop = Arc::new(AtomicBool::new(false));
    let made: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Each test run takes 0.4s; main moves every ~50ms, so every
    // fast-forward attempt finds it moved.
    let mover = {
        let stop = stop.clone();
        let made = made.clone();
        let env_repo = repo.clone();
        let home = env.home();
        std::thread::spawn(move || {
            let shim = Env2 {
                repo: env_repo,
                home,
            };
            while !stop.load(Ordering::SeqCst) {
                if let Some(c) = shim.plumbing_commit("main") {
                    made.lock().unwrap().push(c);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let r = land(
        &env,
        json!({"session": "a", "test": "sleep 0.4", "timeout_secs": 30}),
    );
    stop.store(true, Ordering::SeqCst);
    mover.join().unwrap();
    let status = r["status"].as_str().unwrap();
    assert!(status == "landed" || status == "target_moving", "{r}");
    let final_tip = tip(&env, "main");
    assert!(is_ancestor(&env, &repo, &initial, &final_tip));
    let made = made.lock().unwrap();
    assert!(!made.is_empty());
    for c in made.iter() {
        assert!(
            is_ancestor(&env, &repo, c, &final_tip),
            "concurrent commit {c} lost from main"
        );
    }
    if status == "landed" {
        let h = r["head"].as_str().unwrap();
        assert!(is_ancestor(&env, &repo, h, &final_tip));
    } else {
        assert_eq!(r["attempts"], 3, "{r}");
        assert!(r["message"].as_str().unwrap().contains("land again"), "{r}");
        let tree = env.git(&repo, &["show", "main:src/app.txt"]);
        assert_eq!(
            tree, "line one\nline two\nline three\n",
            "target_moving but the work reached main"
        );
    }
    // The session is not left mid-rebase and still holds its work.
    assert!(!rebase_in_progress(&env, &s));
    assert_eq!(read(s.join("src/app.txt")), APP2);
}

/// A thread-safe stand-in for the parts of Env the mover thread needs.
struct Env2 {
    repo: PathBuf,
    home: PathBuf,
}

impl Env2 {
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("git")
            .current_dir(&self.repo)
            .env("OCEAN_GRAFT_HOME", &self.home)
            .env("GIT_AUTHOR_NAME", "Bg")
            .env("GIT_AUTHOR_EMAIL", "bg@example.invalid")
            .env("GIT_COMMITTER_NAME", "Bg")
            .env("GIT_COMMITTER_EMAIL", "bg@example.invalid")
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn plumbing_commit(&self, branch: &str) -> Option<String> {
        let r = format!("refs/heads/{branch}");
        let old = self.git(&["rev-parse", &r])?;
        let tree = self.git(&["rev-parse", &format!("{old}^{{tree}}")])?;
        let c = self.git(&["commit-tree", &tree, "-p", &old, "-m", "bg"])?;
        self.git(&["update-ref", &r, &c, &old])?;
        Some(c)
    }

    /// A real commit in the checked-out working tree (index and all).
    fn worktree_commit(&self, n: usize) -> Option<String> {
        fs::write(self.repo.join(format!("bg-{n}.txt")), format!("{n}\n")).ok()?;
        self.git(&["add", &format!("bg-{n}.txt")])?;
        self.git(&["commit", "-q", "-m", &format!("bg {n}")])?;
        self.git(&["rev-parse", "HEAD"])
    }
}

#[test]
fn target_checked_out_and_moving_by_real_commits_never_reports_blocked() {
    let env = setup(5399, "clone");
    let repo = env.repo();
    let initial = tip(&env, "main");
    let s = fork(&env, "a");
    commit_file(&env, &s, "src/app.txt", APP2, "feat");
    let stop = Arc::new(AtomicBool::new(false));
    let made: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mover = {
        let stop = stop.clone();
        let made = made.clone();
        let shim = Env2 {
            repo: repo.clone(),
            home: env.home(),
        };
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                if let Some(c) = shim.worktree_commit(n) {
                    made.lock().unwrap().push(c);
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        })
    };
    let r = land(
        &env,
        json!({"session": "a", "test": "sleep 0.3", "timeout_secs": 30}),
    );
    stop.store(true, Ordering::SeqCst);
    mover.join().unwrap();
    let status = r["status"].as_str().unwrap();
    let final_tip = tip(&env, "main");
    assert!(is_ancestor(&env, &repo, &initial, &final_tip));
    for c in made.lock().unwrap().iter() {
        assert!(
            is_ancestor(&env, &repo, c, &final_tip),
            "commit {c} lost from main"
        );
    }
    assert!(!repo.join(".git/MERGE_HEAD").exists());
    // The user (the mover) has no edits in the way, so blocked would be a lie.
    assert!(status == "landed" || status == "target_moving", "{r}");
    if status == "landed" {
        assert_eq!(read(repo.join("src/app.txt")), APP2);
    }
}

// Two lands finishing at the same moment race on the source repository's
// index.lock in `git merge --ff-only`. Neither may report target_blocked
// (the user has no edits) and main must contain whatever was reported landed.
#[test]
fn concurrent_lands_of_two_sessions_both_end_well() {
    let env = setup(5357 + 1, "clone");
    let repo = env.repo();
    let initial = tip(&env, "main");
    let a = fork(&env, "a");
    let b = fork(&env, "b");
    commit_file(&env, &a, "README.md", "from a\n", "a");
    commit_file(&env, &b, "src/app.txt", APP2, "b");
    let run = |name: &'static str| {
        let mut cmd = env.command(BIN, &repo);
        cmd.args([
            "api",
            "land",
            &json!({"session": name, "test": "sleep 0.2"}).to_string(),
        ]);
        cmd
    };
    let mut ca = run("a")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut cb = run("b")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let parse = |child: &mut std::process::Child| -> Value {
        let mut s = String::new();
        use std::io::Read;
        child.stdout.take().unwrap().read_to_string(&mut s).unwrap();
        child.wait().unwrap();
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("{e}: {s}"))
    };
    let ra = parse(&mut ca);
    let rb = parse(&mut cb);
    for r in [&ra, &rb] {
        let st = r["status"].as_str().unwrap_or_default();
        assert!(st == "landed" || st == "target_moving", "a={ra}\nb={rb}");
    }
    let final_tip = tip(&env, "main");
    assert!(is_ancestor(&env, &repo, &initial, &final_tip));
    for r in [&ra, &rb] {
        if r["status"] == "landed" {
            assert!(
                is_ancestor(&env, &repo, r["head"].as_str().unwrap(), &final_tip),
                "{r}"
            );
        }
    }
    assert!(
        porcelain(&env, &repo).is_empty(),
        "{}",
        porcelain(&env, &repo)
    );
}

#[test]
fn landing_a_fork_of_a_session_carries_the_parent_commits() {
    let env = setup(5325, "clone");
    let repo = env.repo();
    let a = fork(&env, "a");
    let ah = commit_file(&env, &a, "README.md", "from a\n", "a");
    fs::write(a.join("src/app.txt"), APP2).unwrap(); // uncommitted, carried by the fork
    let v = env.api("fork", json!({"from": "a", "name": "b"})).unwrap();
    let b = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
    assert_eq!(read(b.join("src/app.txt")), APP2);
    let r = land(
        &env,
        json!({"session": "b", "test": "", "commit_message": "b"}),
    );
    assert_eq!(r["status"], "landed", "{r}");
    assert_eq!(r["commits"], 2, "{r}");
    assert!(is_ancestor(&env, &repo, &ah, &tip(&env, "main")));
    // a's commit is already on main; a still holds uncommitted work.
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "uncommitted_changes", "{r}");
    env.git(&a, &["checkout", "--", "src/app.txt"]);
    let r = land(&env, json!({"session": "a", "test": ""}));
    assert_eq!(r["status"], "nothing_to_land", "{r}");
}

#[test]
fn land_over_mcp_reports_statuses_as_results_not_errors() {
    let env = setup(5328, "clone");
    let s = fork(&env, "a");
    let mut mcp = env.mcp();
    mcp.initialize();
    fs::write(s.join("src/app.txt"), APP2).unwrap();
    let (err, v) = mcp.call(1, "land", json!({"session": "a", "test": ""}));
    assert!(!err, "{v}");
    assert_eq!(v["status"], "uncommitted_changes");
    let (err, v) = mcp.call(
        2,
        "land",
        json!({"session": "a", "test": "echo boom; exit 2", "commit_message": "m"}),
    );
    assert!(!err, "tests_failed is a result, not a tool error: {v}");
    assert_eq!(v["status"], "tests_failed");
    assert!(v["tests"]["output_tail"].as_str().unwrap().contains("boom"));
    let (err, v) = mcp.call(3, "land", json!({"session": "a", "into": "nope"}));
    assert!(err, "a missing branch is a tool error: {v}");
    assert!(v.as_str().unwrap().contains("nope"), "{v}");
    let (err, v) = mcp.call(
        4,
        "land",
        json!({"path": s.to_string_lossy(), "test": "true"}),
    );
    assert!(!err, "{v}");
    assert_eq!(v["status"], "landed");
    assert!(
        mcp.next_message(Duration::from_millis(200)).is_none(),
        "stray output"
    );
    assert!(mcp.close().success());
    assert_eq!(read(env.repo().join("src/app.txt")), APP2);
}

#[test]
fn many_sessions_landed_in_turn_keep_linear_history() {
    let env = setup(5320 - 1, "clone");
    let repo = env.repo();
    let mut tips = vec![tip(&env, "main")];
    let names = ["s1", "s2", "s3", "s4"];
    let paths: Vec<PathBuf> = names.iter().map(|n| fork(&env, n)).collect();
    for (n, p) in names.iter().zip(&paths) {
        commit_file(&env, p, &format!("files/{n}.txt"), &format!("{n}\n"), n);
    }
    for n in names {
        let r = land(&env, json!({"session": n, "test": ""}));
        assert_eq!(r["status"], "landed", "{n}: {r}");
        let t = tip(&env, "main");
        assert!(
            is_ancestor(&env, &repo, tips.last().unwrap(), &t),
            "{n} rewrote main"
        );
        tips.push(t);
    }
    let merges = env.git(&repo, &["rev-list", "--merges", "main"]);
    assert!(
        merges.trim().is_empty(),
        "land created merge commits: {merges}"
    );
    assert_eq!(env.git(&repo, &["rev-list", "--count", "main"]).trim(), "5");
    for n in names {
        assert_eq!(read(repo.join(format!("files/{n}.txt"))), format!("{n}\n"));
    }
}
