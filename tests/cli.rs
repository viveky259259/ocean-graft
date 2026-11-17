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

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

fn alive(pid: i32) -> bool {
    unsafe { libc_kill(pid, 0) == 0 }
}

/// The PID column of `shoal services` for one session's service.
fn service_pid(listing: &str, session: &str) -> Option<i32> {
    listing
        .lines()
        .find(|l| l.split_whitespace().next() == Some(session))
        .and_then(|l| l.split_whitespace().nth(4))
        .and_then(|pid| pid.parse().ok())
}

#[test]
fn queue_limits_concurrent_jobs() {
    use std::time::{Duration, Instant};
    let env = Env::new();
    let _unmount = Unmount(&env);
    env.shoal(&["init", "--backend", "clone"]);
    env.shoal(&["new", "a"]);
    env.shoal(&["queue", "--slots", "2"]);

    // Each job records how many jobs are running when it starts.
    let marks = env.root.path().join("marks");
    fs::create_dir_all(marks.join("running")).unwrap();
    let script = format!(
        "touch {m}/running/$$; ls {m}/running | wc -l >> {m}/counts; sleep 1; rm {m}/running/$$",
        m = marks.display()
    );
    let start = Instant::now();
    let jobs: Vec<_> = (0..5)
        .map(|_| {
            env.command(env!("CARGO_BIN_EXE_shoal"), &env.repo())
                .args(["run", "a", "--", "sh", "-c", &script])
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::sleep(Duration::from_millis(400));
    let queue = env.shoal(&["queue"]);
    assert!(
        queue.contains("2 of 2 slot(s) busy, 3 job(s) waiting"),
        "{queue}"
    );
    assert!(
        queue.contains("RUNNING") && queue.contains("/a "),
        "{queue}"
    );
    for mut job in jobs {
        assert!(job.wait().unwrap().success());
    }
    let most = read(marks.join("counts"))
        .lines()
        .map(|l| l.trim().parse::<u32>().unwrap())
        .max()
        .unwrap();
    assert!(most <= 2, "{most} jobs ran at once with 2 slots");
    // Five one-second jobs, two at a time: at least three rounds.
    assert!(
        start.elapsed() >= Duration::from_millis(2900),
        "{:?}",
        start.elapsed()
    );
    assert!(env
        .shoal(&["queue"])
        .contains("0 of 2 slot(s) busy, 0 job(s) waiting"));

    // Inside a session, `run` finds the session from the working directory.
    let out = env
        .command(env!("CARGO_BIN_EXE_shoal"), &env.path("a"))
        .args(["run", "--", "sh", "-c", "echo $SHOAL_SESSION"])
        .output()
        .unwrap();
    assert_eq!(stdout(&out).trim(), "a", "{}", stderr(&out));
}

#[test]
fn idle_services_are_stopped_and_woken() {
    use std::time::Duration;
    let env = Env::new();
    let _unmount = Unmount(&env);
    env.shoal(&["init", "--backend", "clone"]);
    env.shoal(&["new", "a"]);
    env.shoal(&["new", "b"]);
    let server = ["sh", "-c", "echo listening on $PORT; exec sleep 300"];
    let started = env.shoal(&[&["serve", "a", "--"][..], &server[..]].concat());
    assert!(
        started.contains("Started web in a on port 4100"),
        "{started}"
    );
    env.shoal(&[&["serve", "b", "--"][..], &server[..]].concat());
    let listing = env.shoal(&["services"]);
    let (pid_a, pid_b) = (
        service_pid(&listing, "a").unwrap(),
        service_pid(&listing, "b").unwrap(),
    );
    assert!(alive(pid_a) && alive(pid_b), "{listing}");
    let twice = env.shoal_in(
        &env.repo(),
        &[&["serve", "a", "--"][..], &server[..]].concat(),
    );
    assert!(stderr(&twice).contains("already running"));

    // Both go quiet; then someone works in b.
    std::thread::sleep(Duration::from_secs(2));
    fs::write(env.path("b").join("notes.txt"), "still working\n").unwrap();
    let reaped = env.shoal(&["reap", "--idle", "1s"]);
    assert!(reaped.contains("stopped 1 service(s) in a"), "{reaped}");
    assert!(!reaped.contains("in b"), "{reaped}");
    assert!(!alive(pid_a), "a's server should have stopped");
    assert!(alive(pid_b), "b is active, so its server keeps running");
    let listing = env.shoal(&["services"]);
    assert!(listing.contains("stopped (idle)"), "{listing}");
    assert!(env.shoal(&["ls"]).contains("1 idle-stopped"));

    // Wake brings a's server back on the same port.
    let woke = env.shoal(&["wake", "a"]);
    assert!(woke.contains("Started web in a on port 4100"), "{woke}");
    let pid_a = service_pid(&env.shoal(&["services"]), "a").unwrap();
    assert!(alive(pid_a));

    let stopped = env.shoal(&["stop", "b"]);
    assert!(stopped.contains("Stopped 1 service(s) in b"), "{stopped}");
    assert!(!alive(pid_b));
    env.shoal(&["rm", "a", "--force"]);
    assert!(!alive(pid_a), "rm stops the session's services");
}

#[test]
fn conflict_check_is_incremental() {
    let env = Env::new();
    let _unmount = Unmount(&env);
    env.shoal(&["init", "--backend", "clone"]);
    for name in ["a", "b", "c"] {
        env.shoal(&["new", name]);
    }
    let original = read(env.path("b").join("src/app.txt"));
    fs::write(
        env.path("a").join("src/app.txt"),
        "line one\nfrom a\nline three\n",
    )
    .unwrap();
    fs::write(
        env.path("b").join("src/app.txt"),
        "line one\nfrom b\nline three\n",
    )
    .unwrap();
    fs::write(env.path("c").join("README.md"), "c was here\n").unwrap();

    let first = env.shoal(&["conflicts"]);
    assert!(
        first.contains("a + b") && first.contains("CONFLICT in: src/app.txt"),
        "{first}"
    );
    assert!(
        first.contains("(3 changed since the last check)"),
        "{first}"
    );
    assert!(
        first.contains("1 overlapping pair(s) (1 merged now)"),
        "{first}"
    );

    // Nothing changed: no new snapshots, no new merges, same answer.
    let again = env.shoal(&["conflicts"]);
    assert!(
        again.contains("(0 changed since the last check)"),
        "{again}"
    );
    assert!(again.contains("(0 merged now)"), "{again}");
    assert!(again.contains("CONFLICT in: src/app.txt"), "{again}");

    // b takes its edit back: only b is looked at again, and the conflict is gone.
    fs::write(env.path("b").join("src/app.txt"), &original).unwrap();
    let after = env.shoal(&["conflicts"]);
    assert!(
        after.contains("(1 changed since the last check)"),
        "{after}"
    );
    assert!(after.contains("No conflicts."), "{after}");

    // A committed change counts too, even when the files are the same on disk.
    env.git(&env.path("a"), &["commit", "-q", "-am", "a commits"]);
    let committed = env.shoal(&["conflicts"]);
    assert!(
        committed.contains("(1 changed since the last check)"),
        "{committed}"
    );
}

/// One HTTP/1.0 GET; returns the status line, or None if nothing answered.
fn http_get(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(70)))
        .ok()?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let text = String::from_utf8_lossy(&response);
    text.lines().next().map(str::to_string)
}

#[test]
fn on_demand_service_wakes_on_first_request() {
    use std::time::{Duration, Instant};
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: needs python3 for a test HTTP server");
        return;
    }
    let env = Env::new();
    let _unmount = Unmount(&env);
    env.shoal(&["init", "--backend", "clone", "--port-base", "4700"]);
    env.shoal(&["new", "a"]);
    let started = env.shoal(&[
        "serve",
        "a",
        "--on-demand",
        "--",
        "sh",
        "-c",
        "exec python3 -m http.server \"$PORT\" --bind 127.0.0.1",
    ]);
    assert!(started.contains("on port 4700 (on demand"), "{started}");
    // The server needs a moment to bind its private port; the gateway waits.
    let first = http_get(4700).expect("the gateway answers");
    assert!(first.contains("200"), "{first}");
    assert!(env.shoal(&["services"]).contains("running (on demand)"));

    // Idle: reap stops the server but the gateway stays.
    std::thread::sleep(Duration::from_secs(2));
    let reaped = env.shoal(&["reap", "--idle", "1s"]);
    assert!(reaped.contains("stopped 1 service(s) in a"), "{reaped}");
    assert!(env.shoal(&["services"]).contains("asleep (on demand)"));

    // The next request wakes it and is answered.
    let start = Instant::now();
    let woken = http_get(4700).expect("the gateway answers after reap");
    assert!(woken.contains("200"), "{woken}");
    eprintln!("woke and answered in {:?}", start.elapsed());
    assert!(env.shoal(&["services"]).contains("running (on demand)"));

    // Requests count as activity: a session in use is not reaped.
    std::thread::sleep(Duration::from_secs(2));
    assert!(http_get(4700).unwrap().contains("200"));
    let kept = env.shoal(&["reap", "--idle", "1s"]);
    assert!(!kept.contains("in a"), "a was just used: {kept}");

    // stop removes the gateway too: nothing listens any more.
    env.shoal(&["stop", "a"]);
    assert!(http_get(4700).is_none(), "the gateway should be gone");
}
