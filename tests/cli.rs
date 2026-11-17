//! End-to-end tests: drive the real binary against a throwaway repository.
//! The overlay test needs root on Linux and is skipped otherwise.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    root: tempfile::TempDir,
}

/// Unmounts everything under the temp dir before it is deleted.
struct Unmount<'a>(&'a Env);

impl Drop for Unmount<'_> {
    fn drop(&mut self) {
        let _ = self.0.shoal_in(&self.0.repo(), &["down"]);
    }
}

impl Env {
    fn new() -> Env {
        // Under target/tmp rather than /tmp: the sandbox gives commands a
        // private /tmp, which would hide whether a write escaped.
        let env = Env {
            root: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
        };
        let repo = env.repo();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/app.txt"), "line one\nline two\nline three\n").unwrap();
        fs::write(repo.join("README.md"), "demo\n").unwrap();
        fs::write(repo.join("deps.lock"), "lib 1.0\n").unwrap();
        fs::write(repo.join(".gitignore"), "deps/\n.env\n").unwrap();
        fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        env.git(&repo, &["init", "-q", "-b", "main"]);
        env.git(&repo, &["add", "-A"]);
        env.git(&repo, &["commit", "-q", "-m", "initial"]);
        env
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn command(&self, program: &str, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(dir)
            .env("SHOAL_HOME", self.root.path().join("home"))
            .env_remove("SHOAL_PROJECT")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
        cmd
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.command("git", dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn shoal_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_shoal"), dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn shoal(&self, args: &[&str]) -> String {
        let out = self.shoal_in(&self.repo(), args);
        assert!(
            out.status.success(),
            "shoal {args:?} failed:\n{}{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn path(&self, session: &str) -> PathBuf {
        PathBuf::from(self.shoal(&["path", session]).trim())
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(path).unwrap()
}

fn lifecycle(backend: &str) {
    let env = Env::new();
    let _unmount = Unmount(&env);
    let repo = env.repo();

    // A warm template: the setup command's output is in every session.
    env.shoal(&[
        "init",
        "--backend",
        backend,
        "--setup",
        "mkdir -p deps && cat deps.lock > deps/installed.txt",
        "--setup-input",
        "deps.lock",
        "--copy",
        ".env",
    ]);
    env.shoal(&["new", "a"]);
    env.shoal(&["new", "b"]);
    let (a, b) = (env.path("a"), env.path("b"));
    assert_eq!(read(a.join("deps/installed.txt")), "lib 1.0\n");
    assert_eq!(read(a.join(".env")), "SECRET=1\n");
    assert_eq!(env.git(&a, &["branch", "--show-current"]).trim(), "shoal/a");

    // Isolation: edits, new files and deletes stay in their session.
    fs::write(
        a.join("src/app.txt"),
        "line one\nchanged by a\nline three\n",
    )
    .unwrap();
    fs::write(a.join("deps/extra.txt"), "only in a\n").unwrap();
    fs::remove_file(a.join("README.md")).unwrap();
    assert_eq!(
        read(b.join("src/app.txt")),
        "line one\nline two\nline three\n"
    );
    assert!(!b.join("deps/extra.txt").exists());
    assert!(b.join("README.md").exists());
    assert_eq!(
        read(repo.join("src/app.txt")),
        "line one\nline two\nline three\n"
    );

    let status = env.shoal(&["status", "a"]);
    assert!(status.contains("M src/app.txt"), "{status}");
    assert!(status.contains("D README.md"), "{status}");
    if backend == "overlay" {
        // The volume delta also sees the ignored dependency directory.
        assert!(status.contains("deps"), "{status}");
    }
    let diff = env.shoal(&["diff", "a", "--stat"]);
    assert!(diff.contains("src/app.txt"), "{diff}");

    // Each session gets its own port in the environment.
    let ports = env.shoal(&[
        "exec",
        "a",
        "--",
        "sh",
        "-c",
        "echo $SHOAL_SESSION:$SHOAL_PORT",
    ]);
    assert_eq!(ports.trim(), "a:4100");
    let ports = env.shoal(&["exec", "b", "sh", "-c", "echo $SHOAL_SESSION:$PORT"]);
    assert_eq!(ports.trim(), "b:4110");

    // Conflicts are found from what is on disk, before anything is committed.
    fs::write(
        b.join("src/app.txt"),
        "line one\nchanged by b\nline three\n",
    )
    .unwrap();
    let report = env.shoal(&["conflicts"]);
    assert!(report.contains("CONFLICT in: src/app.txt"), "{report}");

    // Checkpoint, wreck, rewind: uncommitted and ignored files come back.
    env.shoal(&["checkpoint", "a", "before"]);
    fs::remove_dir_all(a.join("src")).unwrap();
    fs::remove_dir_all(a.join("deps")).unwrap();
    env.shoal(&["rewind", "a", "before"]);
    let a = env.path("a");
    assert_eq!(
        read(a.join("src/app.txt")),
        "line one\nchanged by a\nline three\n"
    );
    assert_eq!(read(a.join("deps/extra.txt")), "only in a\n");

    // Fork keeps the parent's uncommitted and ignored state; then they diverge.
    env.shoal(&["fork", "a", "c"]);
    let c = env.path("c");
    assert_eq!(
        read(c.join("src/app.txt")),
        "line one\nchanged by a\nline three\n"
    );
    assert_eq!(read(c.join("deps/extra.txt")), "only in a\n");
    assert_eq!(env.git(&c, &["branch", "--show-current"]).trim(), "shoal/c");
    fs::write(c.join("deps/extra.txt"), "changed in c\n").unwrap();
    assert_eq!(read(a.join("deps/extra.txt")), "only in a\n");

    // Commits come back to the source repository as branches.
    env.git(&a, &["add", "-A"]);
    env.git(&a, &["commit", "-q", "-m", "change from a"]);
    let imported = env.shoal(&["import", "a"]);
    assert!(
        imported.contains("1 commit(s) imported as shoal/a"),
        "{imported}"
    );
    let log = env.git(&repo, &["log", "-1", "--format=%s", "shoal/a"]);
    assert_eq!(log.trim(), "change from a");

    // Refresh: the source moves on and the lockfile changed, so setup re-runs.
    fs::write(repo.join("deps.lock"), "lib 2.0\n").unwrap();
    env.git(&repo, &["commit", "-q", "-am", "bump lib"]);
    let refreshed = env.shoal(&["refresh"]);
    assert!(refreshed.contains("Template g2"), "{refreshed}");
    assert!(refreshed.contains("setup re-run"), "{refreshed}");
    env.shoal(&["new", "d"]);
    let d = env.path("d");
    assert_eq!(read(d.join("deps/installed.txt")), "lib 2.0\n");
    assert_eq!(read(b.join("deps/installed.txt")), "lib 1.0\n");
    let listing = env.shoal(&["ls"]);
    assert!(listing.contains("g2"), "{listing}");

    // rm refuses to lose work unless forced.
    let refused = env.shoal_in(&repo, &["rm", "b"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("uncommitted"));
    env.shoal(&["rm", "b", "--force"]);
    env.shoal(&["rm", "d"]);
    assert!(!env.shoal(&["ls"]).contains("\nb "));

    // Sessions come back after being unmounted (as after a reboot).
    env.shoal(&["down"]);
    assert_eq!(read(env.path("c").join("deps/extra.txt")), "changed in c\n");

    env.shoal(&["gc"]);
    let info = env.shoal(&["info"]);
    assert!(info.contains("g2"), "{info}");
}

#[test]
fn clone_backend_lifecycle() {
    lifecycle("clone");
}

#[test]
fn overlay_backend_lifecycle() {
    if !cfg!(target_os = "linux") || unsafe { libc_geteuid() } != 0 {
        eprintln!("skipped: the overlay backend needs root on Linux");
        return;
    }
    lifecycle("overlay");
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn sandbox_confines_writes() {
    if !cfg!(target_os = "linux") || Command::new("bwrap").arg("--version").output().is_err() {
        eprintln!("skipped: needs bubblewrap on Linux");
        return;
    }
    let env = Env::new();
    let _unmount = Unmount(&env);
    env.shoal(&["init"]);
    env.shoal(&["new", "a"]);
    env.shoal(&["new", "b"]);
    let b = env.path("b");
    let outside = env.root.path().join("outside.txt");
    let script = format!(
        "echo ok > inside.txt; \
         (echo x > {outside}) 2>/dev/null && echo OUTSIDE=written || echo OUTSIDE=denied; \
         [ -e {b} ] && echo SIBLING=visible || echo SIBLING=hidden",
        outside = outside.display(),
        b = b.display()
    );
    let out = env.shoal(&["exec", "--sandbox", "a", "--", "sh", "-c", &script]);
    assert!(out.contains("OUTSIDE=denied"), "{out}");
    assert!(out.contains("SIBLING=hidden"), "{out}");
    assert!(env.path("a").join("inside.txt").exists());
    assert!(!outside.exists());
}
