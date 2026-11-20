mod common;
use common::*;

// Regression tests for the Claude Code integration fixes of commit dda3697
// (rules 2 and 5 of plugin/ARCHITECTURE.md, and the service part of rule 6):
// the libexec/ocean-graft-plugin launcher's exit-code mapping, the plugin
// manifests, MCP protocol negotiation and SIGPIPE survival, honest
// auto_checkpoints, the idle worktree sweep, and the PID-reuse guard on
// service process groups.
//
// Port range 6000-6099. Port bases (step 20): child SIGPIPE disposition
// 6000 (MCP initialize and the closed-pipe exit share it: no sessions),
// SIGPIPE land 6020, auto checkpoints 6040, worktree sweep 6060,
// PID reuse 6080. Within one test, each backend/scenario gets a fresh Env on
// the same base, one after the other (the previous one is dropped first).
//
// Hooks run with GIT_CEILING_DIRECTORIES at the test's temporary root, since
// target/tmp may live inside another git repository.

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin")
}

fn launcher() -> PathBuf {
    plugin_dir().join("libexec/ocean-graft-plugin")
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn write_exe(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }
}

/// Wait for a child with a deadline; kill it if the deadline passes.
fn wait_deadline(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run `program args` with `input` on stdin. `setup` adjusts the command.
fn run_with(program: &Path, args: &[&str], input: &[u8], setup: impl FnOnce(&mut Command)) -> Ran {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    setup(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let r_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let r_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let status = wait_deadline(&mut child, Duration::from_secs(60));
    let _ = writer.join();
    Ran {
        status,
        stdout: String::from_utf8_lossy(&r_out.join().unwrap()).into_owned(),
        stderr: String::from_utf8_lossy(&r_err.join().unwrap()).into_owned(),
    }
}

const TURN_HOOKS: &[&str] = &["stop", "session-start", "prompt"];
const WORKTREE_HOOKS: &[&str] = &["worktree-create", "worktree-remove"];

/// Run `ocean-graft hook <event>` (the real binary) from `dir`.
fn hook_env(
    env: &Env,
    event: &str,
    input: &Value,
    dir: &Path,
    extra: &[(&str, &str)],
    remove: &[&str],
) -> HookResult {
    let r = run_with(
        Path::new(BIN),
        &["hook", event],
        input.to_string().as_bytes(),
        |cmd| {
            cmd.current_dir(dir)
                .env("OCEAN_GRAFT_HOME", env.home())
                .env_remove("OCEAN_GRAFT_PROJECT")
                .env_remove("CLAUDE_PROJECT_DIR")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .env("GIT_CEILING_DIRECTORIES", env.root.path());
            for k in remove {
                cmd.env_remove(k);
            }
            for (k, v) in extra {
                cmd.env(k, v);
            }
        },
    );
    HookResult {
        code: r.code(),
        stdout: r.stdout,
        stderr: r.stderr,
    }
}

fn create_input(name: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "transcript_path": "/dev/null",
        "hook_event_name": "WorktreeCreate",
        "cwd": cwd,
        "name": name,
    })
}

fn turn_input(event: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "test-session",
        "transcript_path": "/dev/null",
        "hook_event_name": event,
        "cwd": cwd,
    })
}

/// WorktreeCreate from `cwd` with the given idle limit (None: the default,
/// with OCEAN_GRAFT_WORKTREE_IDLE_HOURS removed). Returns (path, stderr).
fn worktree_create(
    env: &Env,
    name: &str,
    cwd: &Path,
    idle_hours: Option<&str>,
) -> (PathBuf, String) {
    let r = match idle_hours {
        Some(h) => hook_env(
            env,
            "worktree-create",
            &create_input(name, cwd),
            cwd,
            &[("OCEAN_GRAFT_WORKTREE_IDLE_HOURS", h)],
            &[],
        ),
        None => hook_env(
            env,
            "worktree-create",
            &create_input(name, cwd),
            cwd,
            &[],
            &["OCEAN_GRAFT_WORKTREE_IDLE_HOURS"],
        ),
    };
    assert_eq!(
        r.code, 0,
        "worktree-create {name} failed:\nstdout:\n{}\nstderr:\n{}",
        r.stdout, r.stderr
    );
    let path = PathBuf::from(r.last_line());
    assert!(path.is_dir(), "worktree-create {name} printed {path:?}");
    (path, r.stderr)
}

fn session_names(env: &Env) -> Vec<String> {
    env.state()["sessions"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// Sleep until the clock has moved past every activity timestamp so far
/// (idle checks compare whole seconds with a strict `>`).
fn let_a_second_pass() {
    std::thread::sleep(Duration::from_millis(1200));
}

// ---------------------------------------------------------------------------
// Launcher: exit-code mapping
// ---------------------------------------------------------------------------

/// A binary that fails (exit 2 or 3) or is killed by a signal: turn hooks
/// still exit 0, worktree hooks exit 1, and nothing ever exits 2.
#[test]
fn launcher_maps_failing_and_killed_binaries() {
    let dir = scratch();
    let fakes: Vec<(&str, String)> = vec![
        ("exit2", "#!/bin/sh\ncat >/dev/null\nexit 2\n".into()),
        ("exit3", "#!/bin/sh\ncat >/dev/null\nexit 3\n".into()),
        ("kill9", "#!/bin/sh\nkill -KILL $$\nexit 0\n".into()),
        ("term", "#!/bin/sh\nkill -TERM $$\nexit 0\n".into()),
        ("segv", "#!/bin/sh\nkill -SEGV $$\nexit 0\n".into()),
    ];
    let mut failures = Vec::new();
    for (label, body) in &fakes {
        let fake = dir.path().join(format!("fake-{label}"));
        write_exe(&fake, body);
        for (events, want) in [(TURN_HOOKS, 0), (WORKTREE_HOOKS, 1)] {
            for event in events {
                let r = run_with(&launcher(), &["hook", event], b"{}", |cmd| {
                    cmd.env("OCEAN_GRAFT_BIN", &fake).current_dir(dir.path());
                });
                if r.code() != want || r.status.signal().is_some() {
                    failures.push(format!(
                        "{label} hook {event}: want exit {want}, got {:?} (stderr {:?})",
                        r.status, r.stderr
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A binary that succeeds: every hook exits 0.
#[test]
fn launcher_passes_success_through() {
    let dir = scratch();
    let fake = dir.path().join("fake-ok");
    write_exe(&fake, "#!/bin/sh\ncat >/dev/null\nexit 0\n");
    for event in TURN_HOOKS.iter().chain(WORKTREE_HOOKS) {
        let r = run_with(&launcher(), &["hook", event], b"{}", |cmd| {
            cmd.env("OCEAN_GRAFT_BIN", &fake).current_dir(dir.path());
        });
        assert_eq!(r.code(), 0, "hook {event}: {:?} {}", r.status, r.stderr);
    }
}

/// No binary at all (OCEAN_GRAFT_BIN unset, nothing bundled, no dev build, none on
/// PATH): turn hooks 0, worktree hooks 1, mcp 1, with a reason on stderr.
#[test]
fn launcher_without_any_binary() {
    let dir = scratch();
    // Nested so that "$here/../../target/release/ocean-graft" does not exist.
    let copy = dir.path().join("a/b/libexec/ocean-graft-plugin");
    write_exe(&copy, &read(launcher()));
    assert!(!copy
        .parent()
        .unwrap()
        .join("../../target/release/ocean-graft")
        .exists());
    let path = "/usr/bin:/bin";
    for d in path.split(':') {
        assert!(
            !Path::new(d).join("ocean-graft").exists(),
            "{d}/ocean-graft exists; this test needs a PATH without ocean-graft"
        );
    }
    for ocean_graft_bin in [None, Some("")] {
        let setup = |cmd: &mut Command| {
            cmd.env("PATH", path).current_dir(dir.path());
            match ocean_graft_bin {
                Some(v) => cmd.env("OCEAN_GRAFT_BIN", v),
                None => cmd.env_remove("OCEAN_GRAFT_BIN"),
            };
        };
        for event in TURN_HOOKS {
            let r = run_with(&copy, &["hook", event], b"{}", setup);
            assert_eq!(
                r.code(),
                0,
                "hook {event} (OCEAN_GRAFT_BIN {ocean_graft_bin:?}): {:?}",
                r.status
            );
            assert!(
                r.stderr.contains("not found"),
                "hook {event}: stderr {:?}",
                r.stderr
            );
        }
        for event in WORKTREE_HOOKS {
            let r = run_with(&copy, &["hook", event], b"{}", setup);
            assert_eq!(
                r.code(),
                1,
                "hook {event} (OCEAN_GRAFT_BIN {ocean_graft_bin:?}): {:?}",
                r.status
            );
            assert!(r.stdout.is_empty(), "hook {event} printed {:?}", r.stdout);
        }
        let r = run_with(&copy, &["mcp"], b"", setup);
        assert_eq!(
            r.code(),
            1,
            "mcp (OCEAN_GRAFT_BIN {ocean_graft_bin:?}): {:?}",
            r.status
        );
        assert!(r.stdout.is_empty(), "mcp printed {:?}", r.stdout);
    }
}

/// stdin reaches the binary, its stdout reaches Claude unchanged (the
/// WorktreeCreate path stays the last line), and arguments pass through.
#[test]
fn launcher_passes_stdin_stdout_and_arguments_through() {
    let dir = scratch();
    let args_file = dir.path().join("args.txt");
    let fake = dir.path().join("fake-echo");
    write_exe(
        &fake,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat\necho /made/up/session/path\n",
            args_file.display()
        ),
    );
    let input = format!(
        "{}\n",
        json!({"cwd": "/x", "name": "a b", "hook_event_name": "WorktreeCreate"})
    );
    for event in WORKTREE_HOOKS.iter().chain(TURN_HOOKS) {
        let r = run_with(&launcher(), &["hook", event], input.as_bytes(), |cmd| {
            cmd.env("OCEAN_GRAFT_BIN", &fake).current_dir(dir.path());
        });
        assert_eq!(r.code(), 0, "hook {event}: {:?} {}", r.status, r.stderr);
        assert_eq!(
            r.stdout,
            format!("{input}/made/up/session/path\n"),
            "hook {event}: stdout not passed through"
        );
        let last = r.stdout.lines().last().unwrap_or_default();
        assert_eq!(last, "/made/up/session/path");
        assert_eq!(read(&args_file), format!("hook\n{event}\n"));
    }
    // mcp: exec'd, so the binary's exit code and both streams are its own.
    let rpc = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    let r = run_with(&launcher(), &["mcp"], rpc.as_bytes(), |cmd| {
        cmd.env("OCEAN_GRAFT_BIN", &fake).current_dir(dir.path());
    });
    assert_eq!(r.code(), 0, "mcp: {:?} {}", r.status, r.stderr);
    assert_eq!(r.stdout, format!("{rpc}/made/up/session/path\n"));
    assert_eq!(read(&args_file), "mcp\n");
}

/// The real binary never exits 2 for an unknown event, directly or through
/// the launcher.
#[test]
fn unknown_hook_event_exits_1_never_2() {
    let dir = scratch();
    for event in ["bogus", "PreToolUse", "worktree_create", ""] {
        let r = run_with(Path::new(BIN), &["hook", event], b"{}", |cmd| {
            cmd.current_dir(dir.path())
                .env("OCEAN_GRAFT_HOME", dir.path().join("home"))
                .env("GIT_CEILING_DIRECTORIES", dir.path());
        });
        assert_eq!(
            r.code(),
            1,
            "ocean-graft hook {event:?}: {:?} stderr {}",
            r.status,
            r.stderr
        );
        assert!(
            r.stdout.is_empty(),
            "ocean-graft hook {event:?} printed {:?}",
            r.stdout
        );
        let r = run_with(&launcher(), &["hook", event], b"{}", |cmd| {
            cmd.current_dir(dir.path())
                .env("OCEAN_GRAFT_BIN", BIN)
                .env("OCEAN_GRAFT_HOME", dir.path().join("home"))
                .env("GIT_CEILING_DIRECTORIES", dir.path());
        });
        assert_ne!(r.code(), 2, "launcher hook {event:?} exited 2");
        assert!(
            r.status.code().is_some(),
            "launcher hook {event:?}: {:?}",
            r.status
        );
    }
}

// ---------------------------------------------------------------------------
// Plugin manifests
// ---------------------------------------------------------------------------

#[test]
fn manifests_point_at_the_libexec_launcher() {
    let hooks: Value = serde_json::from_str(&read(plugin_dir().join("hooks/hooks.json"))).unwrap();
    let want = [
        ("WorktreeCreate", "hook worktree-create"),
        ("WorktreeRemove", "hook worktree-remove"),
        ("Stop", "hook stop"),
        ("SubagentStop", "hook stop"),
        ("SessionStart", "hook session-start"),
        ("UserPromptSubmit", "hook prompt"),
    ];
    let all = hooks["hooks"].as_object().expect("hooks object");
    assert_eq!(
        all.len(),
        want.len(),
        "events: {:?}",
        all.keys().collect::<Vec<_>>()
    );
    for (event, args) in want {
        let groups = hooks["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("no {event}"));
        let commands: Vec<&str> = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert_eq!(commands.len(), 1, "{event}: {commands:?}");
        assert_eq!(
            commands[0],
            format!("\"${{CLAUDE_PLUGIN_ROOT}}/libexec/ocean-graft-plugin\" {args}"),
            "{event}"
        );
    }
    let mcp: Value = serde_json::from_str(&read(plugin_dir().join(".mcp.json"))).unwrap();
    assert_eq!(
        mcp["mcpServers"]["ocean-graft"]["command"],
        "${CLAUDE_PLUGIN_ROOT}/libexec/ocean-graft-plugin"
    );
    assert_eq!(mcp["mcpServers"]["ocean-graft"]["args"], json!(["mcp"]));
    assert!(
        !plugin_dir().join("bin").exists(),
        "plugin/bin must be gone (it is put on PATH)"
    );
    let mode = fs::metadata(launcher()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 == 0o111,
        "launcher is not executable: {mode:o}"
    );
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

const KNOWN: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    "2026-07-28",
];
const NEWEST: &str = "2026-07-28";
const FALLBACK: &str = "2025-06-18";

#[test]
fn mcp_initialize_negotiates_a_known_revision() {
    let env = Env::new(6000);
    let mut m = env.mcp();
    let mut id = 1;
    let mut ask = |m: &mut Mcp, params: Option<Value>| -> Value {
        id += 1;
        let mut msg = json!({"jsonrpc": "2.0", "id": id, "method": "initialize"});
        if let Some(p) = params {
            msg["params"] = p;
        }
        m.send(&msg);
        let r = m.response(&json!(id), Duration::from_secs(30));
        r["result"]["protocolVersion"].clone()
    };
    let client = json!({"name": "t", "version": "0"});
    for v in KNOWN {
        let got = ask(
            &mut m,
            Some(json!({"protocolVersion": v, "capabilities": {}, "clientInfo": client})),
        );
        assert_eq!(got, json!(v), "known revision {v} must be echoed");
    }
    for v in [
        "2099-01-01",
        "1999-12-31",
        "2025-06-19",
        "draft",
        "2026-07-28 ",
    ] {
        let got = ask(
            &mut m,
            Some(json!({"protocolVersion": v, "capabilities": {}, "clientInfo": client})),
        );
        assert_eq!(
            got,
            json!(NEWEST),
            "unknown revision {v:?} must get the newest known"
        );
    }
    let got = ask(
        &mut m,
        Some(json!({"protocolVersion": "", "capabilities": {}, "clientInfo": client})),
    );
    assert_eq!(got, json!(FALLBACK), "empty revision");
    let got = ask(
        &mut m,
        Some(json!({"capabilities": {}, "clientInfo": client})),
    );
    assert_eq!(got, json!(FALLBACK), "missing revision");
    let got = ask(&mut m, Some(json!({"protocolVersion": null})));
    assert_eq!(got, json!(FALLBACK), "null revision");
    let got = ask(&mut m, None);
    assert_eq!(got, json!(FALLBACK), "no params");
    let status = m.close();
    assert!(status.success(), "mcp exited {status:?}");
}

/// The MCP server ignores SIGPIPE itself, but the processes it starts (the
/// tests run by land) must get the default disposition, or a pipeline such
/// as `producer | head -1` never ends.
#[test]
fn mcp_children_get_the_default_sigpipe_disposition() {
    // Passes only if SIGPIPE (signal 13, bit 0x1000) is not ignored.
    // Linux: the SigIgn mask in /proc. Elsewhere (macOS, whose ps has no
    // such column and whose sh does not list it in `trap`): perl reports the
    // inherited disposition as $SIG{PIPE} eq "IGNORE" (checked on a Mac).
    let check = "if [ -r /proc/$$/status ]; then ign=$(sed -n 's/^SigIgn:[[:space:]]*//p' /proc/$$/status); \
                 [ $(( 0x$ign & 0x1000 )) -eq 0 ] || { echo \"SIGPIPE ignored: $ign\"; exit 1; }; \
                 else perl -e 'exit((defined $SIG{PIPE} && $SIG{PIPE} eq q(IGNORE)) ? 1 : 0)' || { echo 'SIGPIPE ignored'; exit 1; }; fi";
    for backend in backends() {
        let env = Env::new(6000);
        env.init(backend);
        let mut m = env.mcp();
        m.initialize();
        let (err, r) = m.call(1, "fork", json!({"name": "kids"}));
        assert!(!err, "[{backend}] fork: {r}");
        let path = PathBuf::from(r["sessions"][0]["path"].as_str().unwrap());
        fs::write(path.join("kid.txt"), "x\n").unwrap();
        let (err, r) = m.call(
            2,
            "land",
            json!({"session": "kids", "commit_message": "kid", "test": check, "timeout_secs": 60}),
        );
        assert!(!err, "[{backend}] land: {r}");
        assert_eq!(
            r["status"], "landed",
            "[{backend}] test processes inherit an ignored SIGPIPE: {r}"
        );
        let status = m.close();
        assert!(status.success(), "[{backend}] {status:?}");
    }
}

// Fixed bug: ocean-graft mcp ignores every stdout write error (mcp.rs serve(): `let _ =
// out.write_all(..)`), so contrary to the comment in main.rs it never ends on
// a broken output pipe; with stdin left open it keeps running indefinitely.
/// main.rs: "a write error then ends the server cleanly after in-flight
/// calls finish". With its stdout gone but stdin still open, the server
/// should notice the failed write and exit instead of serving into a void.
#[test]
fn mcp_exits_when_its_output_pipe_is_gone() {
    let env = Env::new(6000);
    let mut child = env
        .command(BIN, &env.repo())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
    let _ = stdin.flush();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut status = None;
    while Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            status = Some(st);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin);
    let status = status
        .expect("ocean-graft mcp kept running after its stdout pipe broke (stdin still open)");
    assert_ne!(status.signal(), Some(libc::SIGPIPE), "died of SIGPIPE");
}

/// The client closes its end of the pipe while a slow land runs: the server
/// must not die of SIGPIPE, and the land must still complete.
#[test]
fn mcp_survives_sigpipe_and_finishes_a_slow_land() {
    for backend in backends() {
        let env = Env::new(6020);
        env.init(backend);
        let s = env.api("fork", json!({"name": "pipe"})).unwrap();
        let path = PathBuf::from(
            s["sessions"][0]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("fork result {s}")),
        );
        fs::write(path.join("landed.txt"), "from the session\n").unwrap();

        let err_path = env.root.path().join("mcp-stderr.txt");
        let mut child = env
            .command(BIN, &env.repo())
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        // The land is in flight (its tests are running) when the client
        // goes away: closes the read end of stdout, then stdin. A request
        // the server never read is not in flight; one it is running is.
        let mut stdin = child.stdin.take().unwrap();
        let msgs = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": NEWEST, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "land", "arguments": {
                "session": "pipe", "commit_message": "land through a closed pipe", "test": "sleep 2; true"}}}),
        ];
        for m in &msgs {
            stdin.write_all(format!("{m}\n").as_bytes()).unwrap();
        }
        stdin.flush().unwrap();
        let log = env.project_dir().join("logs/pipe-land-tests.log");
        let started = Instant::now();
        while !log.exists() && started.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(log.exists(), "[{backend}] the land never started its tests");
        drop(child.stdout.take());
        drop(stdin);
        let status = wait_deadline(&mut child, Duration::from_secs(90));
        let errtext = read(&err_path);
        assert_ne!(
            status.signal(),
            Some(libc::SIGPIPE),
            "[{backend}] ocean-graft mcp died of SIGPIPE; stderr:\n{errtext}"
        );
        assert!(
            status.success(),
            "[{backend}] ocean-graft mcp exited {status:?}; stderr:\n{errtext}"
        );
        let log = env.git(&env.repo(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            log.trim(),
            "land through a closed pipe",
            "[{backend}] land did not reach main; stderr:\n{errtext}"
        );
        assert_eq!(
            env.git(&env.repo(), &["show", "main:landed.txt"]),
            "from the session\n"
        );
        assert_eq!(read(env.repo().join("landed.txt")), "from the session\n");
    }
}

// ---------------------------------------------------------------------------
// Automatic checkpoints: promised only where the Stop hook takes them
// ---------------------------------------------------------------------------

#[test]
fn auto_checkpoints_report_matches_the_stop_hook() {
    for backend in backends() {
        let env = Env::new(6040);
        env.init(backend);
        let (path, _) = worktree_create(&env, "turns", &env.repo(), None);
        let status = env.api("status", json!({})).unwrap();
        let auto = status["auto_checkpoints"]
            .as_bool()
            .unwrap_or_else(|| panic!("[{backend}] no auto_checkpoints in {status}"));
        if backend == "overlay" {
            assert!(auto, "[overlay] auto_checkpoints must be true");
        }
        // The same report from inside the session.
        let inside = env.api_in(&path, "status", json!({})).unwrap();
        assert_eq!(inside["auto_checkpoints"], json!(auto), "[{backend}]");

        // Two changing turns.
        for turn in 1..=2 {
            fs::write(path.join("src/app.txt"), format!("turn {turn}\n")).unwrap();
            let r = hook_env(&env, "stop", &turn_input("Stop", &path), &path, &[], &[]);
            assert_eq!(r.code, 0, "[{backend}] stop: {}", r.stderr);
            assert!(
                r.stdout.is_empty(),
                "[{backend}] stop printed {:?}",
                r.stdout
            );
        }
        let turns: Vec<String> = env.state()["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["session"] == "turns")
            .filter_map(|c| c["label"].as_str())
            .filter(|l| l.starts_with("turn-"))
            .map(str::to_string)
            .collect();
        if auto {
            assert_eq!(
                turns.len(),
                2,
                "[{backend}] auto_checkpoints is true but the stop hook made {turns:?}"
            );
        } else {
            assert!(
                turns.is_empty(),
                "[{backend}] auto_checkpoints is false but the stop hook made {turns:?}"
            );
        }

        let r = hook_env(
            &env,
            "session-start",
            &turn_input("SessionStart", &path),
            &path,
            &[],
            &[],
        );
        assert_eq!(r.code, 0, "[{backend}] session-start: {}", r.stderr);
        assert!(
            r.stdout.contains("\"turns\""),
            "[{backend}] session-start: {:?}",
            r.stdout
        );
        let says_not = r.stdout.contains("NOT taken automatically");
        let says_auto = r
            .stdout
            .contains("taken automatically at the end of each turn");
        assert_eq!(
            says_not, !auto,
            "[{backend}] auto={auto}, session-start said: {}",
            r.stdout
        );
        assert_eq!(
            says_auto, auto,
            "[{backend}] auto={auto}, session-start said: {}",
            r.stdout
        );
    }
}

// ---------------------------------------------------------------------------
// Idle worktree sweep
// ---------------------------------------------------------------------------

/// A WorktreeCreate as Claude Code sends it for a subagent (no agent fields:
/// it never sends them for WorktreeCreate). Nothing interactive runs in it.
fn subagent_worktree(env: &Env, name: &str, cwd: &Path, idle_hours: &str) -> PathBuf {
    let input = create_input(name, cwd);
    let r = hook_env(
        env,
        "worktree-create",
        &input,
        cwd,
        &[("OCEAN_GRAFT_WORKTREE_IDLE_HOURS", idle_hours)],
        &[],
    );
    assert_eq!(r.code, 0, "worktree-create {name}: {}", r.stderr);
    PathBuf::from(r.last_line())
}

/// Run the sweep the hook starts in the background, synchronously.
fn sweep(env: &Env, idle_hours: &str) -> String {
    let out = env
        .command(BIN, &env.repo())
        .args(["--project", env.project_dir().to_str().unwrap(), "sweep"])
        .env("OCEAN_GRAFT_WORKTREE_IDLE_HOURS", idle_hours)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sweep: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Only idle hook-made sessions that no main session ran in are swept (work
/// saved first). Sessions where SessionStart or UserPromptSubmit fired
/// (`claude --worktree`, background sessions), api forks, sessions with a
/// running service, and sessions some process is working in are kept. 0
/// hours turns it off.
#[test]
fn worktree_sweep_removes_only_idle_subagent_sessions() {
    for backend in backends() {
        let env = Env::new(6060);
        env.init(backend);
        let repo = env.repo();
        // Created with the sweep off, so nothing is removed while setting up.
        let sub1 = subagent_worktree(&env, "sub1", &repo, "0");
        fs::write(sub1.join("sub1-work.txt"), "unsaved work\n").unwrap();
        fs::write(sub1.join("src/app.txt"), "edited in sub1\n").unwrap();
        subagent_worktree(&env, "sub2", &repo, "0");
        subagent_worktree(&env, "svc", &repo, "0");
        env.ocean_graft(&["serve", "svc", "--", "sleep", "300"]);
        let busy = subagent_worktree(&env, "busy", &repo, "0");
        let mut worker = std::process::Command::new("sleep")
            .arg("60")
            .current_dir(&busy)
            .spawn()
            .unwrap();
        let (interactive, _) = worktree_create(&env, "interactive", &repo, Some("0"));
        assert!(interactive.exists());
        // A main session starts in one, and the user types a prompt in another.
        let ss = hook_env(
            &env,
            "session-start",
            &json!({"cwd": interactive, "hook_event_name": "SessionStart"}),
            &interactive,
            &[],
            &[],
        );
        assert_eq!(ss.code, 0, "{}", ss.stderr);
        let prompted = subagent_worktree(&env, "prompted", &repo, "0");
        let up = hook_env(
            &env,
            "prompt",
            &json!({"cwd": prompted, "hook_event_name": "UserPromptSubmit"}),
            &prompted,
            &[],
            &[],
        );
        assert_eq!(up.code, 0, "{}", up.stderr);
        env.api("fork", json!({"name": "manual"})).unwrap();
        let st = env.state();
        assert_eq!(
            st["sessions"]["sub1"]["hook_created"],
            json!(true),
            "[{backend}]"
        );
        assert_eq!(
            st["sessions"]["sub1"]["interactive"],
            json!(false),
            "[{backend}]"
        );
        assert_eq!(
            st["sessions"]["interactive"]["interactive"],
            json!(true),
            "[{backend}]"
        );
        assert_eq!(
            st["sessions"]["prompted"]["interactive"],
            json!(true),
            "[{backend}]"
        );
        assert_eq!(
            st["sessions"]["manual"]["hook_created"],
            json!(false),
            "[{backend}]"
        );
        let svc_pid = st["sessions"]["svc"]["services"][0]["pid"]
            .as_i64()
            .unwrap() as i32;
        let_a_second_pass();

        // 0 turns the sweep off.
        sweep(&env, "0");
        assert_eq!(session_names(&env).len(), 7, "[{backend}] 0 must not sweep");

        // A tiny limit: everything idle is eligible.
        let err = sweep(&env, "0.0001");
        let names = session_names(&env);
        for gone in ["sub1", "sub2"] {
            assert!(
                !names.contains(&gone.to_string()),
                "[{backend}] {gone} kept: {names:?}\n{err}"
            );
        }
        for kept in ["svc", "busy", "interactive", "prompted", "manual"] {
            assert!(
                names.contains(&kept.to_string()),
                "[{backend}] {kept} swept: {names:?}\n{err}"
            );
        }
        assert!(
            alive(svc_pid),
            "[{backend}] the kept session's service was stopped"
        );
        assert_eq!(
            env.git(&repo, &["show", "ocean-graft/sub1:sub1-work.txt"]),
            "unsaved work\n"
        );
        assert_eq!(
            env.git(&repo, &["show", "ocean-graft/sub1:src/app.txt"]),
            "edited in sub1\n"
        );
        assert!(
            !env.project_dir().join("sessions/sub1").exists(),
            "[{backend}] sub1 files left"
        );

        // Once nothing works in it any more, the busy one goes too.
        let _ = worker.kill();
        let _ = worker.wait();
        sweep(&env, "0.0001");
        assert!(
            !session_names(&env).contains(&"busy".to_string()),
            "[{backend}]"
        );

        // The hook starts the sweep itself, in the background, and never
        // sweeps the session it is forking from.
        for i in 1..=4 {
            subagent_worktree(&env, &format!("old{i}"), &repo, "0");
        }
        let_a_second_pass();
        let parent = env.project_dir().join("sessions/old1");
        subagent_worktree(&env, "child", &parent, "0.0001");
        let deadline = Instant::now() + Duration::from_secs(20);
        let left = || {
            let names = session_names(&env);
            (2..=4)
                .filter(|i| names.contains(&format!("old{i}")))
                .count()
        };
        while left() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let names = session_names(&env);
        assert_eq!(
            left(),
            0,
            "[{backend}] background sweep did not run: {names:?}"
        );
        assert!(
            names.contains(&"old1".to_string()),
            "[{backend}] the parent was swept"
        );
        assert!(names.contains(&"child".to_string()), "[{backend}]");
        env.ocean_graft(&["down"]);
    }
}

// ---------------------------------------------------------------------------
// PID reuse guard
// ---------------------------------------------------------------------------

/// An unrelated process in its own session and process group.
struct Unrelated(Child);

impl Unrelated {
    fn start() -> Unrelated {
        let mut cmd = Command::new("sleep");
        cmd.arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Unrelated(cmd.spawn().unwrap())
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    /// Alive and not a zombie (we are its parent, so a killed one lingers
    /// as a zombie until reaped).
    fn running(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for Unrelated {
    fn drop(&mut self) {
        unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Point session `name`'s web service (and gateway) at `pid`, recording a
/// start time that is not that process's.
fn plant_reused_pid(env: &Env, name: &str, pid: i32) {
    let state_path = env.project_dir().join("state.json");
    let mut state: Value = serde_json::from_str(&read(&state_path)).unwrap();
    let port = state["sessions"][name]["port"].as_u64().unwrap();
    state["sessions"][name]["services"] = json!([{
        "name": "web",
        "command": ["sh", "-c", "exec my-dev-server"],
        "port": port,
        "pid": pid,
        "started_at": 1,
        "stopped_for_idle": false,
        "internal_port": port + 50,
        "gateway": pid,
        "pid_start": "1",
        "gateway_start": "1",
    }]);
    fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
}

#[test]
fn reused_pid_is_never_reported_running_or_signalled() {
    for backend in backends() {
        let env = Env::new(6080);
        env.init(backend);
        env.api("fork", json!({"name": "reuse"})).unwrap();
        let mut other = Unrelated::start();
        let pid = other.pid();
        // Its real start time is not "1" (checked where /proc says so).
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            let real_start = stat[stat.rfind(')').unwrap() + 1..]
                .split_whitespace()
                .nth(19)
                .unwrap()
                .to_string();
            assert_ne!(real_start, "1");
        }

        plant_reused_pid(&env, "reuse", pid);
        let status = env.api("status", json!({})).unwrap();
        let session = status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "reuse")
            .unwrap_or_else(|| panic!("[{backend}] no reuse in {status}"))
            .clone();
        let state = session["services"][0]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            !state.contains("running") && !state.contains("asleep"),
            "[{backend}] a reused pid is reported as {state:?}"
        );

        let r = env.api("preview", json!({"session": "reuse", "stop": true}));
        assert!(r.is_ok(), "[{backend}] preview stop: {r:?}");
        assert!(
            other.running(),
            "[{backend}] preview stop killed an unrelated process"
        );
        assert_eq!(r.unwrap()["stopped"], json!(0), "[{backend}]");

        // Plant it again (preview stop forgets the pids) and discard.
        plant_reused_pid(&env, "reuse", pid);
        let r = env.api("discard", json!({"session": "reuse"}));
        assert!(r.is_ok(), "[{backend}] discard: {r:?}");
        assert!(
            other.running(),
            "[{backend}] discard killed an unrelated process"
        );

        // And with the CLI stop, discard keep_work: false, and down.
        env.api("fork", json!({"name": "reuse2"})).unwrap();
        plant_reused_pid(&env, "reuse2", pid);
        env.ocean_graft(&["stop", "reuse2"]);
        assert!(
            other.running(),
            "[{backend}] ocean-graft stop killed an unrelated process"
        );
        plant_reused_pid(&env, "reuse2", pid);
        let r = env.api("discard", json!({"session": "reuse2", "keep_work": false}));
        assert!(r.is_ok(), "[{backend}] discard keep_work false: {r:?}");
        assert!(
            other.running(),
            "[{backend}] discard keep_work=false killed an unrelated process"
        );
        env.api("fork", json!({"name": "reuse3"})).unwrap();
        plant_reused_pid(&env, "reuse3", pid);
        env.ocean_graft(&["down"]);
        assert!(
            other.running(),
            "[{backend}] ocean-graft down killed an unrelated process"
        );
        drop(env);
        assert!(
            other.running(),
            "[{backend}] cleanup killed an unrelated process"
        );
        drop(other);
    }
}
