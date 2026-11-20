//! Shared harness for ocean-graft's end-to-end tests. Every test drives the real
//! binary in a throwaway repository and OCEAN_GRAFT_HOME, under target/tmp (not
//! /tmp, which the sandbox replaces).
//!
//! Each test file owns a port range (pass it to `Env::new`) so services in
//! parallel tests never collide.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

pub const BIN: &str = env!("CARGO_BIN_EXE_ocean-graft");

/// A static file server on $PORT for preview and gateway tests. Not
/// `python3 -m http.server`: its bind does a reverse DNS lookup, and its
/// stdout is buffered, so a hang there leaves an empty log. This one says
/// "listening" (flushed) once it can take connections.
pub const HTTP_SERVER: &str = r#"exec python3 -u -c "import http.server as h, os, socketserver as s; s.TCPServer.allow_reuse_address = True; srv = s.TCPServer(('127.0.0.1', int(os.environ['PORT'])), h.SimpleHTTPRequestHandler); print('listening on', os.environ['PORT'], flush=True); srv.serve_forever()""#;

pub struct Env {
    pub root: tempfile::TempDir,
    pub port_base: u16,
}

pub fn is_root() -> bool {
    unsafe { libc_geteuid() == 0 }
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc_kill(pid, 0) == 0 }
}

/// Backends to run backend-generic tests against on this machine.
pub fn backends() -> Vec<&'static str> {
    let mut v = vec!["clone"];
    if cfg!(target_os = "linux") && is_root() {
        v.push("overlay");
    }
    v
}

/// The backend on which checkpoints are cheap here, so the Stop hook takes
/// them: overlay on Linux as root, clone on macOS (APFS clonefile).
pub fn cheap_backend() -> Option<&'static str> {
    if cfg!(target_os = "linux") && is_root() {
        Some("overlay")
    } else if cfg!(target_os = "macos") {
        Some("clone")
    } else {
        None
    }
}

/// Gone, or a zombie waiting to be reaped (kill(pid, 0) succeeds on those).
pub fn pid_gone(pid: i32) -> bool {
    if !alive(pid) {
        return true;
    }
    if cfg!(target_os = "linux") {
        return match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                .unwrap_or(false),
            Err(_) => true,
        };
    }
    // macOS and the BSDs: ps reports a zombie's state as Z.
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let st = String::from_utf8_lossy(&out.stdout).trim().to_string();
    st.is_empty() || st.starts_with('Z')
}

/// The working directory of process `pid`.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return fs::read_link(format!("/proc/{pid}/cwd")).ok();
    }
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(PathBuf::from))
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref())
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.as_ref().display()))
}

impl Env {
    /// A git repository on branch main with one commit:
    /// src/app.txt (3 lines), README.md, deps.lock, .gitignore (deps/, .env)
    /// and an untracked, ignored .env.
    pub fn new(port_base: u16) -> Env {
        let env = Env {
            root: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
            port_base,
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

    pub fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// The single project directory under OCEAN_GRAFT_HOME (panics if not exactly one).
    pub fn project_dir(&self) -> PathBuf {
        let mut dirs: Vec<PathBuf> = fs::read_dir(self.home().join("projects"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("state.json").exists())
            .collect();
        assert_eq!(dirs.len(), 1, "expected one project, found {dirs:?}");
        dirs.pop().unwrap()
    }

    pub fn state(&self) -> Value {
        serde_json::from_str(&read(self.project_dir().join("state.json"))).unwrap()
    }

    pub fn command(&self, program: &str, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(dir)
            .env("OCEAN_GRAFT_HOME", self.home())
            .env_remove("OCEAN_GRAFT_PROJECT")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
        cmd
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.command("git", dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            stderr(&out)
        );
        stdout(&out)
    }

    pub fn ocean_graft_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(BIN, dir).args(args).output().unwrap()
    }

    /// Run ocean-graft in the repository and require success.
    pub fn ocean_graft(&self, args: &[&str]) -> String {
        let out = self.ocean_graft_in(&self.repo(), args);
        assert!(
            out.status.success(),
            "ocean-graft {args:?} failed:\n{}{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    /// `ocean-graft init` with a setup that "installs" deps/installed.txt.
    pub fn init(&self, backend: &str) -> String {
        self.ocean_graft(&[
            "init",
            "--backend",
            backend,
            "--setup",
            "mkdir -p deps && cat deps.lock > deps/installed.txt",
            "--setup-input",
            "deps.lock",
            "--copy",
            ".env",
            "--port-base",
            &self.port_base.to_string(),
        ])
    }

    /// Call a tool through `ocean-graft api` in `dir`. Ok(result) or Err(message).
    pub fn api_in(&self, dir: &Path, tool: &str, args: Value) -> Result<Value, String> {
        let out = self
            .command(BIN, dir)
            .args(["api", tool, &args.to_string()])
            .output()
            .unwrap();
        let text = stdout(&out);
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!(
                "api {tool} printed non-JSON ({e}):\n{text}\nstderr:\n{}",
                stderr(&out)
            )
        });
        if out.status.success() {
            Ok(value)
        } else {
            Err(value["error"].as_str().unwrap_or_default().to_string())
        }
    }

    pub fn api(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.api_in(&self.repo(), tool, args)
    }

    /// Run `ocean-graft hook <event>` with `input` on stdin, from `dir`.
    /// `project_dir` sets CLAUDE_PROJECT_DIR when given.
    pub fn hook(
        &self,
        event: &str,
        input: &Value,
        dir: &Path,
        project_dir: Option<&Path>,
    ) -> HookResult {
        let mut cmd = self.command(BIN, dir);
        cmd.args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = project_dir {
            cmd.env("CLAUDE_PROJECT_DIR", p);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        HookResult {
            code: out.status.code().unwrap_or(-1),
            stdout: stdout(&out),
            stderr: stderr(&out),
        }
    }

    /// Start `ocean-graft mcp` with its working directory in the repository.
    pub fn mcp(&self) -> Mcp {
        self.mcp_in(&self.repo())
    }

    pub fn mcp_in(&self, dir: &Path) -> Mcp {
        let mut child = self
            .command(BIN, dir)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Mcp {
            child,
            stdin: Some(stdin),
            lines: rx,
            pending: Vec::new(),
        }
    }
}

impl Drop for Env {
    /// Stop services and unmount sessions of every project, even after a
    /// failed assertion, so nothing outlives the test.
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(self.home().join("projects")) else {
            return;
        };
        for dir in entries.flatten().map(|e| e.path()) {
            if dir.join("state.json").exists() {
                let _ = self
                    .command(BIN, self.root.path())
                    .arg("--project")
                    .arg(&dir)
                    .arg("down")
                    .output();
            }
        }
    }
}

pub struct HookResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl HookResult {
    /// The last non-empty stdout line (what WorktreeCreate reads as the path).
    pub fn last_line(&self) -> String {
        self.stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_string()
    }
}

/// A live `ocean-graft mcp` process.
pub struct Mcp {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// Messages read while waiting for a different id.
    pending: Vec<Value>,
}

impl Mcp {
    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    pub fn send(&mut self, msg: &Value) {
        self.send_raw(&msg.to_string());
    }

    /// Every stdout line must be a JSON-RPC message; panics otherwise.
    pub fn next_message(&mut self, timeout: Duration) -> Option<Value> {
        let line = self.lines.recv_timeout(timeout).ok()?;
        let v: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carried a non-JSON line ({e}): {line:?}"));
        assert_eq!(v["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
        Some(v)
    }

    /// Wait for the response with this id (others are kept for later).
    pub fn response(&mut self, id: &Value, timeout: Duration) -> Value {
        if let Some(i) = self.pending.iter().position(|m| &m["id"] == id) {
            return self.pending.remove(i);
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let msg = self
                .next_message(left)
                .unwrap_or_else(|| panic!("no response to id {id} within {timeout:?}"));
            if &msg["id"] == id {
                return msg;
            }
            self.pending.push(msg);
        }
    }

    pub fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(&json!(id), Duration::from_secs(120))
    }

    pub fn initialize(&mut self) -> Value {
        let r = self.request(
            0,
            "initialize",
            json!({"protocolVersion": "2026-07-28", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        r
    }

    /// tools/call; returns (is_error, parsed JSON result or the error text).
    pub fn call(&mut self, id: i64, tool: &str, args: Value) -> (bool, Value) {
        let r = self.request(id, "tools/call", json!({"name": tool, "arguments": args}));
        let result = &r["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let value = if is_error {
            Value::String(text.to_string())
        } else {
            serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("tool text is not JSON ({e}): {text}"))
        };
        (is_error, value)
    }

    /// Close stdin and wait for the server to exit.
    pub fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().unwrap()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
