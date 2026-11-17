//! Edge cases for `shoal mcp`, driven as a real process: JSON-RPC framing,
//! the initialize handshake, notifications, ids, batches, malformed input,
//! tools/list and tools/call error paths, stdout purity while engine code
//! runs child processes, concurrency of calls, and shutdown with calls in
//! flight.
//!
//! Port range 5100-5199. Tests that create sessions (and so hand out ports)
//! use step-20 bases; the protocol-only tests never create a project, so no
//! port is ever allocated or bound for them, and they take distinct odd
//! values inside the range only so every Env is distinguishable.

mod common;
use common::*;

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Output of a scripted `shoal mcp` run: everything was written to stdin,
/// stdin was closed, and the server ran to exit.
struct Run {
    status: ExitStatus,
    /// Every stdout line, parsed. Panics during collection if a line is not
    /// a JSON-RPC 2.0 message.
    messages: Vec<Value>,
    raw_stdout: String,
    stderr: String,
    elapsed: Duration,
}

impl Run {
    fn by_id(&self, id: &Value) -> Vec<&Value> {
        self.messages.iter().filter(|m| &m["id"] == id).collect()
    }
    fn one(&self, id: &Value) -> &Value {
        let found = self.by_id(id);
        assert_eq!(
            found.len(),
            1,
            "expected exactly one response to id {id}, got {found:?}\nall: {:?}\nstderr: {}",
            self.messages,
            self.stderr
        );
        found[0]
    }
}

/// Parse stdout into JSON-RPC messages, asserting stdout purity.
fn parse_stdout(raw: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in raw.split('\n') {
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout carried a non-JSON line ({e}): {line:?}"));
        assert_eq!(v["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
        assert!(
            v.is_object(),
            "every stdout line must be one message object: {line}"
        );
        out.push(v);
    }
    assert!(
        raw.is_empty() || raw.ends_with('\n'),
        "stdout must end with a newline: {:?}",
        &raw[raw.len().saturating_sub(200)..]
    );
    out
}

/// Run `shoal mcp` in `dir`, feed it `input` (raw bytes), close stdin and
/// wait (at most `limit`) for it to exit.
fn run_bytes_in(env: &Env, dir: &Path, input: Vec<u8>, limit: Duration) -> Run {
    let start = Instant::now();
    let mut child = env
        .command(BIN, dir)
        .arg("mcp")
        // target/tmp lives inside the ocean checkout; stop git discovery at
        // the test root so "not a repository" really is one.
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        let _ = stdin.flush();
        // dropped here: stdin closes
    });
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = so.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = se.read_to_end(&mut buf);
        buf
    });
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            let err = String::from_utf8_lossy(&err_reader.join().unwrap()).into_owned();
            panic!("shoal mcp did not exit within {limit:?} after stdin closed; stderr:\n{err}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = start.elapsed();
    writer.join().unwrap();
    let raw = out_reader.join().unwrap();
    let raw_stdout = String::from_utf8(raw).expect("stdout must be UTF-8");
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap()).into_owned();
    let messages = parse_stdout(&raw_stdout);
    Run {
        status,
        messages,
        raw_stdout,
        stderr,
        elapsed,
    }
}

fn run_lines(env: &Env, lines: &[String]) -> Run {
    let mut input = String::new();
    for l in lines {
        input.push_str(l);
        input.push('\n');
    }
    run_bytes_in(
        env,
        &env.repo(),
        input.into_bytes(),
        Duration::from_secs(60),
    )
}

fn req(id: Value, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn req_np(id: Value, method: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string()
}

fn notif(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

fn call_line(id: Value, tool: &str, args: Value) -> String {
    req(id, "tools/call", json!({"name": tool, "arguments": args}))
}

/// Decode a tools/call response into (is_error, JSON value or error text),
/// checking the result shape on the way.
fn tool_outcome(resp: &Value) -> (bool, Value) {
    assert!(resp.get("error").is_none(), "expected a result, got {resp}");
    let result = &resp["result"];
    let content = result["content"]
        .as_array()
        .unwrap_or_else(|| panic!("no content: {resp}"));
    assert_eq!(content.len(), 1, "{resp}");
    assert_eq!(content[0]["type"], "text", "{resp}");
    let text = content[0]["text"].as_str().expect("text is a string");
    let is_error = result["isError"]
        .as_bool()
        .unwrap_or_else(|| panic!("isError must be a boolean: {resp}"));
    if is_error {
        assert!(
            !text.trim().is_empty(),
            "an error must say something: {resp}"
        );
        (true, Value::String(text.to_string()))
    } else {
        (
            false,
            serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("tool text is not JSON ({e}): {text}")),
        )
    }
}

fn expect_tool_error(resp: &Value, needle: &str) -> String {
    let (is_err, v) = tool_outcome(resp);
    assert!(is_err, "expected isError, got {resp}");
    let text = v.as_str().unwrap().to_string();
    assert!(
        text.to_lowercase().contains(&needle.to_lowercase()),
        "error text {text:?} should mention {needle:?}"
    );
    text
}

fn expect_rpc_error(resp: &Value, code: i64) {
    assert_eq!(resp["error"]["code"], code, "{resp}");
    assert!(
        resp["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "error needs a message: {resp}"
    );
    assert!(resp.get("result").is_none(), "{resp}");
}

/// No project directory may exist under SHOAL_HOME (nothing auto-initialised).
fn assert_no_project(env: &Env) {
    let projects = env.home().join("projects");
    let found: Vec<PathBuf> = fs::read_dir(&projects)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.join("state.json").exists())
                .collect()
        })
        .unwrap_or_default();
    assert!(found.is_empty(), "a project was created: {found:?}");
}

/// Commit a shoal.toml to the env's repository.
fn write_config(env: &Env, toml: &str) {
    fs::write(env.repo().join("shoal.toml"), toml).unwrap();
    env.git(&env.repo(), &["add", "shoal.toml"]);
    env.git(&env.repo(), &["commit", "-q", "-m", "add shoal.toml"]);
}

/// Run the initialize handshake on a live harness Mcp.
fn initialized(mcp: &mut Mcp) {
    let init = mcp.initialize();
    assert!(init["result"]["serverInfo"].is_object(), "{init}");
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[test]
fn initialize_echoes_every_known_revision_and_reports_capabilities() {
    let env = Env::new(5101);
    let known = [
        "2024-11-05",
        "2025-03-26",
        "2025-06-18",
        "2025-11-25",
        "2026-07-28",
    ];
    let lines: Vec<String> = known
        .iter()
        .enumerate()
        .map(|(i, v)| {
            req(
                json!(i),
                "initialize",
                json!({"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
            )
        })
        .collect();
    let run = run_lines(&env, &lines);
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(run.messages.len(), known.len(), "{:?}", run.messages);
    for (i, v) in known.iter().enumerate() {
        let r = run.one(&json!(i));
        let res = &r["result"];
        assert_eq!(res["protocolVersion"], *v, "{r}");
        assert!(res["capabilities"]["tools"].is_object(), "{r}");
        assert_eq!(res["serverInfo"]["name"], "shoal");
        assert!(res["serverInfo"]["version"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        let instructions = res["instructions"].as_str().unwrap_or_default();
        assert!(
            instructions.contains("fork"),
            "instructions should orient Claude: {r}"
        );
        assert!(r.get("error").is_none());
    }
}

#[test]
fn initialize_with_unknown_missing_or_malformed_revision() {
    let env = Env::new(5102);
    let lines = vec![
        // Unknown but well-formed revision: answered with the newest known.
        req(
            json!(1),
            "initialize",
            json!({"protocolVersion": "2099-01-01", "capabilities": {}}),
        ),
        // No params at all.
        req_np(json!(2), "initialize"),
        // params null.
        req(json!(3), "initialize", Value::Null),
        // Empty params.
        req(json!(4), "initialize", json!({})),
        // Empty version string.
        req(json!(5), "initialize", json!({"protocolVersion": ""})),
        // Version of the wrong type.
        req(json!(6), "initialize", json!({"protocolVersion": 20250618})),
        // params as an array.
        req(json!(7), "initialize", json!(["2025-06-18"])),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    assert_eq!(
        run.one(&json!(1))["result"]["protocolVersion"],
        "2026-07-28"
    );
    for id in 2..=7 {
        let r = run.one(&json!(id));
        assert_eq!(
            r["result"]["protocolVersion"], "2025-06-18",
            "id {id} should fall back to the default revision: {r}"
        );
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }
    // The unknown revision is noted on stderr, never on stdout.
    assert!(run.stderr.contains("2099-01-01"), "stderr: {}", run.stderr);
}

#[test]
fn initialize_twice_and_tools_before_initialize_still_answer() {
    let env = Env::new(5103);
    let lines = vec![
        // No handshake first: tools/list and ping still work.
        req_np(json!(1), "tools/list"),
        req_np(json!(2), "ping"),
        req(
            json!(3),
            "initialize",
            json!({"protocolVersion": "2025-03-26"}),
        ),
        req(
            json!(4),
            "initialize",
            json!({"protocolVersion": "2026-07-28"}),
        ),
        notif("notifications/initialized", json!({})),
        req_np(json!(5), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    assert_eq!(run.messages.len(), 5, "{:?}", run.messages);
    assert_eq!(
        run.one(&json!(1))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(run.one(&json!(2))["result"], json!({}));
    assert_eq!(
        run.one(&json!(3))["result"]["protocolVersion"],
        "2025-03-26"
    );
    assert_eq!(
        run.one(&json!(4))["result"]["protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(run.one(&json!(5))["result"], json!({}));
}

// ---------------------------------------------------------------------------
// Notifications, ping, ids
// ---------------------------------------------------------------------------

#[test]
fn notifications_get_no_reply_and_do_not_execute_tools() {
    let env = Env::new(5104);
    let lines = vec![
        notif("notifications/initialized", json!({})),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        notif("notifications/cancelled", json!({"requestId": 3, "reason": "user"})),
        notif("notifications/roots/list_changed", json!({})),
        notif("notifications/progress", json!({"progressToken": "x", "progress": 1})),
        notif("notifications/some-future-thing", json!({"a": [1, 2]})),
        // A ping without an id is a notification too.
        json!({"jsonrpc": "2.0", "method": "ping"}).to_string(),
        // Unknown method as a notification: no -32601, no reply at all.
        json!({"jsonrpc": "2.0", "method": "no/such/method"}).to_string(),
        // A tools/call notification must not run the tool (no reply to carry
        // its result, so it must not create anything either).
        json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "fork", "arguments": {"name": "ghost"}}}).to_string(),
        req_np(json!("last"), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    assert_eq!(
        run.messages.len(),
        1,
        "only the ping is answered: {:?}",
        run.messages
    );
    assert_eq!(run.one(&json!("last"))["result"], json!({}));
    assert_no_project(&env);
}

#[test]
fn ids_of_every_json_type_are_echoed_exactly() {
    let env = Env::new(5105);
    let ids = vec![
        json!(1),
        json!(0),
        json!(-1),
        json!(-9007199254740993i64),
        json!(9007199254740993u64), // beyond f64 precision: must not be rounded
        json!(u64::MAX),
        json!(1.5),
        json!("abc"),
        json!(""),
        json!("0"),
        json!("ünïcødé \u{1F30A} \"quoted\" \\ back"),
        json!("x".repeat(5000)),
    ];
    let mut lines = Vec::new();
    for id in &ids {
        lines.push(req_np(id.clone(), "ping"));
    }
    // Same id types through the tools/call path (answered on a worker thread).
    for id in [json!(-7), json!("call-id"), json!(0)].iter() {
        lines.push(call_line(id.clone(), "status", json!({})));
    }
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in ids.iter().filter(|id| **id != json!(0)) {
        let r = run.one(id);
        assert_eq!(r["result"], json!({}), "{r}");
    }
    for id in [json!(-7), json!("call-id")] {
        let r = run.one(&id);
        let (is_err, v) = tool_outcome(r);
        assert!(!is_err, "{r}");
        assert_eq!(v["initialized"], false);
    }
    // id 0 is shared by a ping and a tools/call: two responses with id 0,
    // one of each kind.
    let zeros = run.by_id(&json!(0));
    assert_eq!(zeros.len(), 2, "{:?}", run.messages);
    assert!(zeros.iter().any(|r| r["result"] == json!({})));
    assert!(zeros.iter().any(|r| r["result"]["content"].is_array()));
    // Raw text check: the big integer id is written verbatim.
    assert!(
        run.raw_stdout.contains("\"id\":9007199254740993"),
        "{}",
        run.raw_stdout
    );
    assert!(run.raw_stdout.contains(&format!("\"id\":{}", u64::MAX)));
}

#[test]
fn ping_returns_an_empty_result_whatever_the_params() {
    let env = Env::new(5106);
    let lines = vec![
        req_np(json!(1), "ping"),
        req(json!(2), "ping", json!({})),
        req(json!(3), "ping", json!({"_meta": {"progressToken": 1}})),
        req(json!(4), "ping", json!([1, 2, 3])),
        req(json!(5), "ping", Value::Null),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in 1..=5 {
        assert_eq!(run.one(&json!(id))["result"], json!({}));
    }
}

// ---------------------------------------------------------------------------
// Batches, framing, malformed input
// ---------------------------------------------------------------------------

#[test]
fn batches_answer_each_request_once_and_skip_notifications() {
    let env = Env::new(5107);
    let batch = json!([
        {"jsonrpc": "2.0", "id": 1, "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "result": {}},
        {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "status", "arguments": {}}},
        {"jsonrpc": "2.0", "id": 5, "method": "bogus/method"},
        42,
        "string",
        null
    ]);
    let run = run_lines(&env, &[batch.to_string(), req_np(json!(99), "ping")]);
    assert!(run.status.success());
    // Responses may come back one per line or as one array line; shoal writes
    // one per line. Either way every request is answered exactly once.
    assert_eq!(run.one(&json!(1))["result"], json!({}));
    assert_eq!(
        run.one(&json!(2))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert!(
        run.by_id(&json!(3)).is_empty(),
        "a client response is never answered"
    );
    let (is_err, v) = tool_outcome(run.one(&json!(4)));
    assert!(!is_err);
    assert_eq!(v["initialized"], false);
    expect_rpc_error(run.one(&json!(5)), -32601);
    // 42, "string" and null are invalid requests: one -32600 each, id null.
    let invalid: Vec<&Value> = run.messages.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(invalid.len(), 3, "{:?}", run.messages);
    for m in invalid {
        expect_rpc_error(m, -32600);
    }
    assert_eq!(run.one(&json!(99))["result"], json!({}));
    assert_eq!(run.messages.len(), 8, "{:?}", run.messages);
}

#[test]
fn empty_batch_and_batch_of_only_notifications() {
    let env = Env::new(5108);
    let lines = vec![
        "[]".to_string(),
        "[ ]".to_string(),
        json!([{"jsonrpc": "2.0", "method": "notifications/initialized"},
               {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}])
        .to_string(),
        json!([{"jsonrpc": "2.0", "id": 1, "result": {}}, {"jsonrpc": "2.0", "id": 2, "error": {"code": 1, "message": "x"}}]).to_string(),
        req_np(json!("end"), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    let nulls: Vec<&Value> = run.messages.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(
        nulls.len(),
        2,
        "one error per empty batch: {:?}",
        run.messages
    );
    for m in nulls {
        expect_rpc_error(m, -32600);
    }
    assert_eq!(run.one(&json!("end"))["result"], json!({}));
    assert_eq!(run.messages.len(), 3, "{:?}", run.messages);
}

#[test]
fn garbage_lines_get_parse_errors_and_the_server_keeps_going() {
    let env = Env::new(5109);
    let garbage = [
        "not json",
        "{",
        "}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"} trailing",
        "{\"a\":1}{\"b\":2}",
        "'single quotes'",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",}",
        "{\"s\":\"\\ud800\"}", // lone surrogate
        "NaN",
        "\u{0}",
    ];
    let mut lines: Vec<String> = garbage.iter().map(|s| s.to_string()).collect();
    lines.push(req_np(json!("alive"), "ping"));
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    let errors: Vec<&Value> = run.messages.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(errors.len(), garbage.len(), "{:?}", run.messages);
    for e in errors {
        expect_rpc_error(e, -32700);
    }
    assert_eq!(run.one(&json!("alive"))["result"], json!({}));
}

#[test]
fn deeply_nested_json_is_a_parse_error_not_a_crash() {
    let env = Env::new(5110);
    let deep = "[".repeat(200_000);
    let deep_obj = format!("{}{}", "{\"a\":".repeat(50_000), "1");
    let run = run_lines(&env, &[deep, deep_obj, req_np(json!(1), "ping")]);
    assert!(
        run.status.success(),
        "server crashed: {:?}\n{}",
        run.status,
        run.stderr
    );
    let errs: Vec<&Value> = run.messages.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(errs.len(), 2, "{:?}", run.messages);
    for e in errs {
        expect_rpc_error(e, -32700);
    }
    assert_eq!(run.one(&json!(1))["result"], json!({}));
}

#[test]
fn blank_lines_whitespace_and_crlf_are_tolerated() {
    let env = Env::new(5111);
    let mut input = Vec::new();
    input.extend_from_slice(b"\n\n   \n\t\n \t \n");
    input.extend_from_slice(req_np(json!(1), "ping").as_bytes());
    input.extend_from_slice(b"\r\n"); // CRLF framing
    input.extend_from_slice(b"\r\n"); // a line holding only \r
    input.extend_from_slice(format!("   {}   \n", req_np(json!(2), "ping")).as_bytes());
    input.extend_from_slice(b"\n\n");
    let run = run_bytes_in(&env, &env.repo(), input, Duration::from_secs(30));
    assert!(run.status.success());
    assert_eq!(
        run.messages.len(),
        2,
        "blank lines must not be answered: {:?}",
        run.messages
    );
    assert_eq!(run.one(&json!(1))["result"], json!({}));
    assert_eq!(run.one(&json!(2))["result"], json!({}));
}

#[test]
fn last_line_without_a_trailing_newline_is_still_answered() {
    let env = Env::new(5112);
    let input = format!(
        "{}\n{}",
        req_np(json!(1), "ping"),
        req_np(json!(2), "tools/list")
    );
    let run = run_bytes_in(
        &env,
        &env.repo(),
        input.into_bytes(),
        Duration::from_secs(30),
    );
    assert!(run.status.success());
    assert_eq!(run.one(&json!(1))["result"], json!({}));
    assert_eq!(
        run.one(&json!(2))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
}

// Fixed bug: a stdin line that is not valid UTF-8 makes `serve` treat the read
// error as end-of-input (`Err(err) => break` in mcp::serve), so the server
// stops reading and exits: every later request goes unanswered and Claude
// Code loses all shoal tools for the rest of the session. A malformed line
// should be a -32700 parse error like any other garbage, and the server
// should keep serving.
#[test]
fn invalid_utf8_line_is_a_parse_error_and_the_server_keeps_going() {
    let env = Env::new(5113);
    let mut input = Vec::new();
    input.extend_from_slice(req_np(json!(1), "ping").as_bytes());
    input.push(b'\n');
    input.extend_from_slice(&[0xff, 0xfe, b'{', 0xc3, b'\n']);
    input.extend_from_slice(req_np(json!(2), "ping").as_bytes());
    input.push(b'\n');
    let run = run_bytes_in(&env, &env.repo(), input, Duration::from_secs(30));
    assert_eq!(run.one(&json!(1))["result"], json!({}));
    assert!(
        run.messages
            .iter()
            .any(|m| m["id"].is_null() && m["error"]["code"] == -32700),
        "expected a parse error for the invalid UTF-8 line: {:?}\nstderr: {}",
        run.messages,
        run.stderr
    );
    assert_eq!(
        run.by_id(&json!(2)).len(),
        1,
        "the request after the bad line must be answered: {:?}\nstderr: {}",
        run.messages,
        run.stderr
    );
}

#[test]
fn very_large_request_line_is_handled() {
    let env = Env::new(5114);
    // A 4 MiB string inside the params of a ping, and a 4 MiB garbage line.
    let big = "y".repeat(4 * 1024 * 1024);
    let lines = vec![
        req(json!(1), "ping", json!({"blob": big})),
        "z".repeat(4 * 1024 * 1024),
        req_np(json!(2), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    assert_eq!(run.one(&json!(1))["result"], json!({}));
    assert_eq!(run.one(&json!(2))["result"], json!({}));
    // The parse error message must not echo 4 MiB back.
    let err = run
        .messages
        .iter()
        .find(|m| m["id"].is_null())
        .expect("parse error");
    expect_rpc_error(err, -32700);
    assert!(
        err.to_string().len() < 10_000,
        "parse error is huge: {} bytes",
        err.to_string().len()
    );
}

// ---------------------------------------------------------------------------
// Requests without method, client responses, unknown methods
// ---------------------------------------------------------------------------

#[test]
fn requests_without_method_and_client_responses() {
    let env = Env::new(5115);
    let lines = vec![
        // id but no method: invalid request, answered with that id.
        json!({"jsonrpc": "2.0", "id": 1}).to_string(),
        json!({"jsonrpc": "2.0", "id": "s", "params": {}}).to_string(),
        // method of the wrong type, with an id.
        json!({"jsonrpc": "2.0", "id": 2, "method": 5}).to_string(),
        json!({"jsonrpc": "2.0", "id": 3, "method": null}).to_string(),
        json!({"jsonrpc": "2.0", "id": 4, "method": ["ping"]}).to_string(),
        // Client responses, success and error: never answered.
        json!({"jsonrpc": "2.0", "id": 10, "result": {}}).to_string(),
        json!({"jsonrpc": "2.0", "id": 11, "result": null}).to_string(),
        json!({"jsonrpc": "2.0", "id": 12, "error": {"code": -1, "message": "no"}}).to_string(),
        json!({"jsonrpc": "2.0", "id": "r", "result": {"roots": []}}).to_string(),
        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "x"}})
            .to_string(),
        req_np(json!("end"), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in [json!(1), json!("s"), json!(2), json!(3), json!(4)] {
        expect_rpc_error(run.one(&id), -32600);
    }
    for id in [json!(10), json!(11), json!(12), json!("r")] {
        assert!(
            run.by_id(&id).is_empty(),
            "response {id} must not be answered: {:?}",
            run.messages
        );
    }
    assert_eq!(run.one(&json!("end"))["result"], json!({}));
    assert_eq!(run.messages.len(), 6, "{:?}", run.messages);
}

// Fixed bug: an invalid request with no id (for example the JSON-RPC 2.0 spec's own
// example `{"jsonrpc":"2.0","method":1,"params":"bar"}`) gets no reply at all;
// mcp::handle returns None for any id-less object without a string method.
// JSON-RPC 2.0 requires an Invalid Request error (-32600) with id null, as
// shoal already does for non-object messages and empty batches.
#[test]
fn invalid_request_without_id_gets_an_error_with_null_id() {
    let env = Env::new(5116);
    let lines = vec![
        json!({"jsonrpc": "2.0", "method": 1, "params": "bar"}).to_string(),
        req_np(json!("end"), "ping"),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    assert_eq!(run.one(&json!("end"))["result"], json!({}));
    let errs: Vec<&Value> = run.messages.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(
        errs.len(),
        1,
        "expected one -32600 with id null: {:?}",
        run.messages
    );
    expect_rpc_error(errs[0], -32600);
}

#[test]
fn unknown_methods_are_method_not_found() {
    let env = Env::new(5117);
    let methods = [
        "foo",
        "",
        "Tools/List",
        "tools/list ",
        "tools/listx",
        "sampling/createMessage",
        "logging/setLevel",
        "completion/complete",
        "resources/read",
        "resources/templates/list",
        "prompts/get",
        "roots/list",
        "notifications/initialized", // sent as a request (with id)
    ];
    let mut lines: Vec<String> = methods
        .iter()
        .enumerate()
        .map(|(i, m)| req(json!(i), m, json!({})))
        .collect();
    lines.push(req_np(json!(100), "resources/list"));
    lines.push(req_np(json!(101), "prompts/list"));
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for (i, m) in methods.iter().enumerate() {
        let r = run.one(&json!(i));
        expect_rpc_error(r, -32601);
        if !m.is_empty() {
            assert!(
                r["error"]["message"].as_str().unwrap().contains(m.trim()),
                "the error should name the method: {r}"
            );
        }
    }
    assert_eq!(run.one(&json!(100))["result"]["resources"], json!([]));
    assert_eq!(run.one(&json!(101))["result"]["prompts"], json!([]));
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

#[test]
fn tools_list_matches_the_documented_tools_and_schemas() {
    let env = Env::new(5118);
    let run = run_lines(
        &env,
        &[
            req_np(json!(1), "tools/list"),
            req(json!(2), "tools/list", json!({"cursor": "anything"})),
        ],
    );
    assert!(run.status.success());
    let r1 = run.one(&json!(1));
    let r2 = run.one(&json!(2));
    assert_eq!(
        r1["result"], r2["result"],
        "a cursor must not change the list"
    );
    assert!(r1["result"].get("nextCursor").is_none_or(Value::is_null));
    let tools = r1["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "status",
            "fork",
            "checkpoint",
            "rewind",
            "conflicts",
            "land",
            "preview",
            "discard"
        ]
    );
    let expect_props: &[(&str, &[&str])] = &[
        ("status", &[]),
        ("fork", &["from", "at", "name", "count"]),
        ("checkpoint", &["label"]),
        ("rewind", &["label"]),
        ("conflicts", &[]),
        ("land", &["commit_message", "test", "into", "timeout_secs"]),
        ("preview", &["command", "stop"]),
        ("discard", &["keep_work"]),
    ];
    for (tool, extra) in expect_props {
        let t = tools.iter().find(|t| t["name"] == *tool).unwrap();
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{t}");
        assert_eq!(schema["additionalProperties"], false, "{t}");
        let props = schema["properties"].as_object().unwrap();
        for common in ["session", "path", "project"] {
            assert_eq!(props[common]["type"], "string", "{tool}.{common}");
        }
        for p in *extra {
            assert!(props.contains_key(*p), "{tool} lacks {p}");
        }
        assert_eq!(
            props.len(),
            3 + extra.len(),
            "{tool} has unexpected properties: {props:?}"
        );
        for (k, v) in props {
            let ty = v["type"]
                .as_str()
                .unwrap_or_else(|| panic!("{tool}.{k} has no type"));
            assert!(
                ["string", "integer", "boolean"].contains(&ty),
                "{tool}.{k}: {ty}"
            );
        }
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        if *tool == "rewind" {
            assert_eq!(required, ["label"]);
        } else {
            assert!(required.is_empty(), "{tool} requires {required:?}");
        }
        let desc = t["description"].as_str().unwrap();
        assert!(desc.len() > 40, "{tool} description too short");
    }
    let fork = tools.iter().find(|t| t["name"] == "fork").unwrap();
    assert_eq!(fork["inputSchema"]["properties"]["count"]["minimum"], 1);
    assert_eq!(fork["inputSchema"]["properties"]["count"]["maximum"], 8);
}

// ---------------------------------------------------------------------------
// tools/call error paths
// ---------------------------------------------------------------------------

#[test]
fn tools_call_accepts_meta_and_extra_params_fields() {
    let env = Env::new(5127);
    let lines = vec![
        req(
            json!(1),
            "tools/call",
            json!({"name": "status", "arguments": {}, "_meta": {"progressToken": "p1"}}),
        ),
        req(
            json!(2),
            "tools/call",
            json!({"name": "status", "arguments": null}),
        ),
        req(
            json!(3),
            "tools/call",
            json!({"name": "status", "somethingNew": [1]}),
        ),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in 1..=3 {
        let (is_err, v) = tool_outcome(run.one(&json!(id)));
        assert!(!is_err, "id {id}: {v}");
        assert_eq!(v["initialized"], false);
    }
}

#[test]
fn an_idle_server_does_not_hold_the_project_lock() {
    let env = Env::new(5128);
    env.init("clone");
    let mut mcp = env.mcp();
    initialized(&mut mcp);
    let (is_err, _) = mcp.call(1, "status", json!({}));
    assert!(!is_err);
    // While the server sits idle (and between its calls), other processes
    // can change the project straight away.
    let t = Instant::now();
    let r = env.api("fork", json!({"name": "from-cli"})).unwrap();
    assert_eq!(r["sessions"][0]["name"], "from-cli");
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "fork waited {:?}",
        t.elapsed()
    );
    // And the server sees the change on its next call.
    let (is_err, st) = mcp.call(2, "status", json!({}));
    assert!(!is_err);
    assert_eq!(st["sessions"][0]["name"], "from-cli", "{st}");
    let (is_err, cp) = mcp.call(
        3,
        "checkpoint",
        json!({"session": "from-cli", "label": "c1"}),
    );
    assert!(!is_err, "{cp}");
    let out = env.shoal(&["ls"]);
    assert!(out.contains("from-cli"), "{out}");
    assert!(mcp.close().success());
}

#[test]
fn tools_call_framing_errors_are_invalid_params() {
    let env = Env::new(5119);
    let lines = vec![
        req_np(json!(1), "tools/call"),
        req(json!(2), "tools/call", Value::Null),
        req(json!(3), "tools/call", json!({})),
        req(json!(4), "tools/call", json!({"name": 5})),
        req(
            json!(5),
            "tools/call",
            json!({"name": null, "arguments": {}}),
        ),
        req(json!(6), "tools/call", json!(["status"])),
        req(json!(7), "tools/call", json!({"arguments": {}})),
        req(json!(8), "tools/call", json!({"name": {"tool": "status"}})),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in 1..=8 {
        expect_rpc_error(run.one(&json!(id)), -32602);
    }
    assert_eq!(run.messages.len(), 8);
    assert_no_project(&env);
}

#[test]
fn tools_call_bad_arguments_are_in_band_errors_and_create_nothing() {
    let env = Env::new(5121);
    let cases: Vec<(&str, Value, &str)> = vec![
        ("nope", json!({}), "unknown tool"),
        ("", json!({}), "unknown tool"),
        ("Status", json!({}), "unknown tool"),
        ("status ", json!({}), "unknown tool"),
        // Arguments of the wrong JSON type.
        ("status", json!("not an object"), "object"),
        ("status", json!([1, 2]), "object"),
        ("status", json!(7), "object"),
        ("fork", json!(true), "object"),
        // Unknown argument keys.
        ("status", json!({"bogus": 1}), "bogus"),
        ("fork", json!({"name": "a", "colour": "red"}), "colour"),
        ("rewind", json!({"label": "x", "Label": "y"}), "Label"),
        ("land", json!({"session": "a", "force": true}), "force"),
        // Wrong argument types.
        ("fork", json!({"count": "2"}), "count"),
        ("fork", json!({"count": -1}), "count"),
        ("fork", json!({"count": 2.5}), "count"),
        ("fork", json!({"count": 0}), "count"),
        ("fork", json!({"count": 9}), "count"),
        ("fork", json!({"count": u64::MAX}), "count"),
        ("fork", json!({"from": ["main"]}), "from"),
        ("fork", json!({"from": "main", "at": "cp1"}), "at"),
        ("fork", json!({"path": 1}), "path"),
        ("fork", json!({"project": false}), "project"),
        ("fork", json!({"at": "cp1"}), "at"),
        // Missing required argument.
        ("rewind", json!({}), "label"),
        ("rewind", json!({"session": "a"}), "label"),
        ("rewind", json!({"session": "a", "label": null}), "label"),
        ("rewind", Value::Null, "label"),
    ];
    let lines: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(i, (tool, args, _))| call_line(json!(i), tool, args.clone()))
        .collect();
    let run = run_lines(&env, &lines);
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(run.messages.len(), cases.len(), "{:?}", run.messages);
    for (i, (tool, args, needle)) in cases.iter().enumerate() {
        let r = run.one(&json!(i));
        let (is_err, v) = tool_outcome(r);
        assert!(is_err, "{tool} {args} should be an error, got {v}");
        let text = v.as_str().unwrap();
        assert!(
            text.to_lowercase().contains(&needle.to_lowercase()),
            "{tool} {args}: error {text:?} should mention {needle:?}"
        );
    }
    // Unknown-tool errors list the real tools so Claude can recover.
    let unknown = run.one(&json!(0))["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(unknown.contains("status") && unknown.contains("fork") && unknown.contains("discard"));
    // None of the rejected calls may have auto-initialised the project.
    assert_no_project(&env);
}

#[test]
fn tools_needing_a_project_explain_how_to_get_one() {
    let env = Env::new(5122);
    let outside = env.root.path().join("not-a-repo");
    fs::create_dir_all(&outside).unwrap();
    let lines = vec![
        call_line(json!(1), "status", json!({})),
        call_line(json!(2), "status", Value::Null),
        req(json!(3), "tools/call", json!({"name": "status"})), // no arguments key
        call_line(json!(4), "checkpoint", json!({"session": "a"})),
        call_line(
            json!(5),
            "rewind",
            json!({"session": "a", "label": "latest"}),
        ),
        call_line(json!(6), "conflicts", json!({})),
        call_line(json!(7), "land", json!({"session": "a"})),
        call_line(json!(8), "preview", json!({"session": "a"})),
        call_line(json!(9), "discard", json!({"session": "a"})),
        call_line(
            json!(10),
            "status",
            json!({"project": "/definitely/not/here"}),
        ),
        call_line(
            json!(11),
            "checkpoint",
            json!({"path": "/definitely/not/here"}),
        ),
        call_line(
            json!(12),
            "fork",
            json!({"project": outside.to_str().unwrap()}),
        ),
        call_line(json!(13), "fork", json!({"from": "someone", "name": "x"})),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    for id in [1, 2, 3] {
        let (is_err, v) = tool_outcome(run.one(&json!(id)));
        assert!(
            !is_err,
            "status without a project is a report, not an error: {v}"
        );
        assert_eq!(v["initialized"], false);
        assert!(v["next"].as_str().unwrap().contains("fork"), "{v}");
    }
    for id in [4, 5, 6, 7, 8, 9] {
        expect_tool_error(run.one(&json!(id)), "fork");
    }
    // A project argument that names nothing is a bad argument, not "no
    // project yet" (which would tell Claude its sessions are gone).
    expect_tool_error(run.one(&json!(10)), "does not exist");
    expect_tool_error(run.one(&json!(11)), "does not exist");
    expect_tool_error(run.one(&json!(12)), "git");
    expect_tool_error(run.one(&json!(13)), "fork");
    assert_no_project(&env);
}

// Fixed bug: fork validates `name` only after it has located (and, from main,
// auto-initialised) the project: `fork {"name": 5}` in a fresh repository
// clones it, runs the setup command and builds the template, then fails with
// "name must be a string". A call rejected for a malformed argument should
// have no side effects; with a slow setup (npm ci) it costs minutes.
#[test]
fn fork_with_a_malformed_name_has_no_side_effects() {
    let env = Env::new(5126);
    write_config(
        &env,
        "setup = \"mkdir -p deps; touch deps/SETUP-RAN\"\nbackend = \"clone\"\nport_base = 5126\n",
    );
    let run = run_lines(&env, &[call_line(json!(1), "fork", json!({"name": 5}))]);
    assert!(run.status.success());
    expect_tool_error(run.one(&json!(1)), "name");
    assert_no_project(&env);
}

// Fixed bug: status turns every error from locating the project into
// {"initialized": false, "next": "call fork to create the first session"},
// including malformed arguments (`path: 1`, `project: false`) and a path that
// does not exist. In a repository that already has a shoal project and
// sessions, Claude is told there is no project and to fork a new one, instead
// of being told its argument was wrong. Argument errors should be isError
// results, and an initialized project must never be reported as missing.
#[test]
fn status_reports_bad_arguments_as_errors_not_as_an_uninitialized_project() {
    let env = Env::new(5125);
    env.init("clone");
    let lines = vec![
        call_line(json!(1), "status", json!({})),
        call_line(json!(2), "status", json!({"path": 1})),
        call_line(json!(3), "status", json!({"project": false})),
        call_line(json!(4), "status", json!({"session": 5})),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success());
    let (is_err, ok) = tool_outcome(run.one(&json!(1)));
    assert!(!is_err);
    assert_eq!(ok["initialized"], true, "{ok}");
    for id in 2..=4 {
        let (is_err, v) = tool_outcome(run.one(&json!(id)));
        assert!(
            is_err,
            "status with a malformed argument (id {id}) must be an error, got {v}"
        );
    }
}

#[test]
fn tool_errors_with_a_project_name_the_fix() {
    let env = Env::new(5140);
    env.init("clone");
    env.api("fork", json!({"name": "alpha"})).unwrap();
    let alpha = env.api("status", json!({})).unwrap()["sessions"][0]["path"]
        .as_str()
        .unwrap()
        .to_string();
    let lines = vec![
        call_line(json!(1), "checkpoint", json!({"session": "missing"})),
        call_line(json!(2), "checkpoint", json!({})),
        call_line(
            json!(3),
            "checkpoint",
            json!({"path": env.repo().to_str().unwrap()}),
        ),
        call_line(
            json!(4),
            "rewind",
            json!({"session": "alpha", "label": "never-made"}),
        ),
        call_line(json!(5), "fork", json!({"from": "missing"})),
        call_line(
            json!(6),
            "fork",
            json!({"from": "alpha", "at": "never-made"}),
        ),
        call_line(json!(7), "land", json!({"session": "alpha", "test": ""})),
        call_line(json!(8), "preview", json!({"session": "alpha"})),
        call_line(json!(9), "status", json!({"path": format!("{alpha}/src")})),
        call_line(
            json!(10),
            "rewind",
            json!({"session": "alpha", "label": ""}),
        ),
        call_line(json!(11), "checkpoint", json!({"session": ""})),
        // Argument types that are only checked once the project is open.
        call_line(
            json!(20),
            "checkpoint",
            json!({"session": "alpha", "label": true}),
        ),
        call_line(json!(21), "checkpoint", json!({"session": 7})),
        call_line(
            json!(22),
            "preview",
            json!({"session": "alpha", "stop": "yes"}),
        ),
        call_line(
            json!(23),
            "discard",
            json!({"session": "alpha", "keep_work": "false"}),
        ),
        call_line(json!(24), "discard", json!({"session": {"name": "alpha"}})),
        call_line(
            json!(25),
            "land",
            json!({"session": "alpha", "timeout_secs": "10"}),
        ),
        call_line(
            json!(26),
            "land",
            json!({"session": "alpha", "timeout_secs": -5}),
        ),
        call_line(
            json!(27),
            "land",
            json!({"session": "alpha", "commit_message": 5}),
        ),
        call_line(
            json!(28),
            "land",
            json!({"session": "alpha", "into": false}),
        ),
        call_line(json!(29), "rewind", json!({"session": "alpha", "label": 3})),
    ];
    let run = run_lines(&env, &lines);
    assert!(run.status.success(), "{}", run.stderr);
    let t = expect_tool_error(run.one(&json!(1)), "missing");
    assert!(t.contains("alpha"), "should list known sessions: {t}");
    expect_tool_error(run.one(&json!(2)), "session");
    expect_tool_error(run.one(&json!(3)), "not inside a shoal session");
    expect_tool_error(run.one(&json!(4)), "never-made");
    expect_tool_error(run.one(&json!(5)), "missing");
    expect_tool_error(run.one(&json!(6)), "never-made");
    let (is_err, land) = tool_outcome(run.one(&json!(7)));
    assert!(!is_err, "{land}");
    assert_eq!(land["status"], "nothing_to_land", "{land}");
    expect_tool_error(run.one(&json!(8)), "preview");
    let (is_err, st) = tool_outcome(run.one(&json!(9)));
    assert!(!is_err, "{st}");
    assert_eq!(st["current_session"], "alpha", "{st}");
    assert!(
        tool_outcome(run.one(&json!(10))).0,
        "an empty label must be an error"
    );
    assert!(
        tool_outcome(run.one(&json!(11))).0,
        "an empty session must be an error"
    );
    for (id, needle) in [
        (20, "label"),
        (21, "session"),
        (22, "stop"),
        (23, "keep_work"),
        (24, "session"),
        (25, "timeout_secs"),
        (26, "timeout_secs"),
        (27, "commit_message"),
        (28, "into"),
        (29, "label"),
    ] {
        expect_tool_error(run.one(&json!(id)), needle);
    }
    // Nothing was discarded by the malformed discard calls.
    assert!(Path::new(&alpha).join("src/app.txt").exists());
    // State is intact after all of that.
    let st = env.api("status", json!({})).unwrap();
    assert_eq!(st["sessions"].as_array().unwrap().len(), 1, "{st}");
}

// ---------------------------------------------------------------------------
// stdout purity
// ---------------------------------------------------------------------------

#[test]
fn stdout_stays_pure_when_fork_auto_initialises_with_a_noisy_setup() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5100);
        let port = 5100 + 10 * i as u16;
        write_config(
            &env,
            &format!(
                "setup = \"echo NOISE-ON-STDOUT; mkdir -p deps\"\nbackend = \"{backend}\"\nport_base = {port}\n"
            ),
        );
        let mut mcp = env.mcp();
        initialized(&mut mcp);
        // The project does not exist yet: this fork creates it, running setup.
        let (is_err, v) = mcp.call(1, "fork", json!({"name": "noisy"}));
        assert!(!is_err, "[{backend}] {v}");
        let path = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
        assert!(path.join("deps").is_dir(), "[{backend}] setup did not run");
        assert_eq!(v["sessions"][0]["name"], "noisy");
        // Land prints to stdout too (its test command).
        fs::write(path.join("feature.txt"), "x\n").unwrap();
        let (is_err, land) = mcp.call(
            2,
            "land",
            json!({"session": "noisy", "commit_message": "feat", "test": "echo LAND-NOISE; echo more >&2; true"}),
        );
        assert!(!is_err, "[{backend}] {land}");
        assert_eq!(land["status"], "landed", "[{backend}] {land}");
        // A dev server that prints to stdout, behind the gateway.
        let (is_err, pv) = mcp.call(
            4,
            "preview",
            json!({"session": "noisy", "command": "echo PREVIEW-NOISE; exec sleep 30"}),
        );
        assert!(!is_err, "[{backend}] {pv}");
        assert_eq!(
            pv["url"],
            format!("http://localhost:{port}"),
            "[{backend}] {pv}"
        );
        let (is_err, st) = mcp.call(3, "status", json!({}));
        assert!(!is_err);
        assert_eq!(st["backend"], backend);
        // Nothing else may be on stdout (next_message panics on non-JSON).
        assert!(
            mcp.next_message(Duration::from_millis(300)).is_none(),
            "[{backend}] stray output"
        );
        assert!(mcp.close().success());
        assert_eq!(read(env.repo().join("feature.txt")), "x\n");
    }
}

#[test]
fn setup_that_reads_stdin_cannot_steal_protocol_bytes() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5120);
        let port = 5120 + 10 * i as u16;
        write_config(
            &env,
            &format!(
                "setup = \"mkdir -p deps; printf PARTIAL-NO-NEWLINE; cat > deps/stdin.txt; echo after-cat\"\nbackend = \"{backend}\"\nport_base = {port}\n"
            ),
        );
        // Write the fork and the following requests in one go, so they are
        // sitting in the pipe while setup runs.
        let lines = vec![
            req(
                json!(0),
                "initialize",
                json!({"protocolVersion": "2025-06-18"}),
            ),
            call_line(json!(1), "fork", json!({"name": "s"})),
            req_np(json!(2), "ping"),
            req_np(json!(3), "tools/list"),
            call_line(json!(4), "status", json!({})),
        ];
        let run = run_lines(&env, &lines);
        assert!(run.status.success(), "[{backend}] {}", run.stderr);
        let (is_err, v) = tool_outcome(run.one(&json!(1)));
        assert!(!is_err, "[{backend}] {v}");
        let path = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
        assert_eq!(
            read(path.join("deps/stdin.txt")),
            "",
            "[{backend}] setup read protocol bytes"
        );
        assert_eq!(run.one(&json!(2))["result"], json!({}));
        assert_eq!(
            run.one(&json!(3))["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        // status may run before or after the fork; either way it is a report.
        assert!(!tool_outcome(run.one(&json!(4))).0);
        assert!(!run.raw_stdout.contains("PARTIAL"), "[{backend}]");
        assert!(!run.raw_stdout.contains("after-cat"), "[{backend}]");
        assert_eq!(run.messages.len(), 5, "[{backend}] {:?}", run.messages);
    }
}

// Fixed bug: services started by a long-lived `shoal mcp` process are its
// children, and nothing ever waits on them. When preview stop (or rewind,
// discard) sends SIGTERM, the dev server dies but stays a zombie of the MCP
// server, `kill(-pgid, 0)` keeps succeeding, and stop_all reports "process
// group N did not stop" (isError) after waiting ~5 s. Through the CLI the
// short-lived shoal process exits, so init reaps the child and this never
// shows. Seen with `ps`: `[sleep] <defunct>` whose parent is `shoal mcp`.
#[test]
fn services_started_through_mcp_can_be_stopped_through_mcp() {
    let env = Env::new(5144);
    env.init("clone");
    env.api("fork", json!({"name": "web"})).unwrap();
    let mut mcp = env.mcp();
    initialized(&mut mcp);
    let (is_err, pv) = mcp.call(
        1,
        "preview",
        json!({"session": "web", "command": "exec sleep 30"}),
    );
    assert!(!is_err, "{pv}");
    // Stopping it in the same server process must work.
    let (is_err, stopped) = mcp.call(2, "preview", json!({"session": "web", "stop": true}));
    assert!(!is_err, "preview stop failed: {stopped}");
    assert_eq!(stopped["stopped"], 1, "{stopped}");
    // Start again, then checkpoint and rewind, which stop and restart it.
    let (is_err, pv) = mcp.call(
        3,
        "preview",
        json!({"session": "web", "command": "exec sleep 30"}),
    );
    assert!(!is_err, "{pv}");
    let (is_err, cp) = mcp.call(4, "checkpoint", json!({"session": "web", "label": "c"}));
    assert!(!is_err, "{cp}");
    let (is_err, rw) = mcp.call(5, "rewind", json!({"session": "web", "label": "c"}));
    assert!(!is_err, "rewind with a running preview failed: {rw}");
    let (is_err, d) = mcp.call(6, "discard", json!({"session": "web"}));
    assert!(!is_err, "discard with a running preview failed: {d}");
    assert!(mcp.close().success());
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn slow_land_does_not_block_status_and_ids_match_out_of_order() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5160 + 10 * i as u16);
        env.init(backend);
        let forked = env.api("fork", json!({"name": "slow"})).unwrap();
        let path = PathBuf::from(forked["sessions"][0]["path"].as_str().unwrap());
        fs::write(path.join("slow.txt"), "slow\n").unwrap();
        let mut mcp = env.mcp();
        initialized(&mut mcp);
        let t0 = Instant::now();
        mcp.send(&json!({"jsonrpc": "2.0", "id": "land", "method": "tools/call", "params": {
            "name": "land",
            "arguments": {"session": "slow", "commit_message": "slow work", "test": "echo LAND-STDOUT; sleep 2; true"}
        }}));
        // Give land a moment to take and release the lock and start tests.
        std::thread::sleep(Duration::from_millis(300));
        mcp.send(&json!({"jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": {"name": "status", "arguments": {}}}));
        mcp.send(&json!({"jsonrpc": "2.0", "id": 12, "method": "ping"}));
        mcp.send(&json!({"jsonrpc": "2.0", "id": 13, "method": "tools/call", "params": {"name": "conflicts", "arguments": {}}}));
        // The first three responses must be the quick ones.
        let mut order = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut got = Vec::new();
        while got.len() < 4 {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = mcp.next_message(left).expect("all four responses");
            order.push((m["id"].clone(), t0.elapsed()));
            got.push(m);
        }
        let ids: Vec<Value> = order.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(
            ids.last().unwrap(),
            "land",
            "[{backend}] land must answer last: {order:?}"
        );
        let status_at = order.iter().find(|(id, _)| id == &json!(11)).unwrap().1;
        let land_at = order.iter().find(|(id, _)| id == "land").unwrap().1;
        assert!(
            status_at < Duration::from_millis(1900),
            "[{backend}] status waited for land's tests: {order:?}"
        );
        assert!(
            land_at >= Duration::from_secs(2),
            "[{backend}] tests did not run? {order:?}"
        );
        let status = got.iter().find(|m| m["id"] == 11).unwrap();
        let (is_err, st) = tool_outcome(status);
        assert!(!is_err, "{st}");
        assert_eq!(st["sessions"][0]["name"], "slow");
        let conflicts = got.iter().find(|m| m["id"] == 13).unwrap();
        assert!(!tool_outcome(conflicts).0, "{conflicts}");
        let land = got.iter().find(|m| m["id"] == "land").unwrap();
        let (is_err, lr) = tool_outcome(land);
        assert!(!is_err, "{lr}");
        assert_eq!(lr["status"], "landed", "[{backend}] {lr}");
        assert!(mcp.close().success());
        assert_eq!(read(env.repo().join("slow.txt")), "slow\n");
    }
}

#[test]
fn many_concurrent_calls_get_exactly_one_well_formed_answer_each() {
    let env = Env::new(5123);
    let mut input = String::new();
    let n = 60;
    for i in 0..n {
        let line = match i % 4 {
            0 => call_line(json!(i), "status", json!({})),
            1 => req_np(json!(i), "ping"),
            2 => req_np(json!(i), "tools/list"),
            _ => call_line(json!(i), "nope", json!({})),
        };
        input.push_str(&line);
        input.push('\n');
    }
    let run = run_bytes_in(
        &env,
        &env.repo(),
        input.into_bytes(),
        Duration::from_secs(60),
    );
    assert!(run.status.success());
    assert_eq!(run.messages.len(), n, "{:?}", run.messages);
    for i in 0..n {
        let r = run.one(&json!(i));
        match i % 4 {
            0 => assert!(!tool_outcome(r).0),
            1 => assert_eq!(r["result"], json!({})),
            2 => assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 8),
            _ => assert!(tool_outcome(r).0),
        }
    }
}

#[test]
fn concurrent_forks_auto_initialise_once_and_get_unique_names() {
    let env = Env::new(5180);
    let backend = *backends().last().unwrap();
    write_config(
        &env,
        &format!("setup = \"sleep 1; mkdir -p deps; echo SETUP-STDOUT\"\nbackend = \"{backend}\"\nport_base = 5180\n"),
    );
    // Six forks with the same name, all in flight at once, before any
    // project exists: one init, six distinct sessions.
    let mut lines = vec![req(
        json!(0),
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )];
    for i in 1..=6 {
        lines.push(call_line(json!(i), "fork", json!({"name": "same"})));
    }
    let run = run_lines(&env, &lines);
    assert!(run.status.success(), "{}", run.stderr);
    let mut names = Vec::new();
    let mut paths = Vec::new();
    for i in 1..=6 {
        let (is_err, v) = tool_outcome(run.one(&json!(i)));
        assert!(!is_err, "fork {i}: {v}\nstderr: {}", run.stderr);
        names.push(v["sessions"][0]["name"].as_str().unwrap().to_string());
        paths.push(PathBuf::from(v["sessions"][0]["path"].as_str().unwrap()));
    }
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 6, "names collided: {names:?}");
    assert!(names.contains(&"same".to_string()));
    for p in &paths {
        assert!(p.join("deps").is_dir(), "{} not set up", p.display());
        assert!(p.join("src/app.txt").exists());
    }
    let state = env.state(); // panics unless exactly one project exists
    assert_eq!(state["sessions"].as_object().unwrap().len(), 6, "{state}");
    let ports: std::collections::HashSet<u64> = state["sessions"]
        .as_object()
        .unwrap()
        .values()
        .map(|s| s["port"].as_u64().unwrap())
        .collect();
    assert_eq!(ports.len(), 6, "ports collided: {state}");
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn closing_stdin_with_a_call_in_flight_answers_it_then_exits_zero() {
    let env = Env::new(5181);
    env.init("clone");
    let forked = env.api("fork", json!({"name": "bye"})).unwrap();
    let path = PathBuf::from(forked["sessions"][0]["path"].as_str().unwrap());
    fs::write(path.join("bye.txt"), "bye\n").unwrap();
    let lines = vec![
        req(
            json!(0),
            "initialize",
            json!({"protocolVersion": "2026-07-28"}),
        ),
        notif("notifications/initialized", json!({})),
        call_line(
            json!(7),
            "land",
            json!({"session": "bye", "commit_message": "bye", "test": "sleep 2; echo DONE; true"}),
        ),
        call_line(json!(8), "status", json!({})),
    ];
    // run_lines closes stdin straight after writing.
    let run = run_lines(&env, &lines);
    assert_eq!(run.status.code(), Some(0), "stderr: {}", run.stderr);
    assert!(
        run.elapsed >= Duration::from_secs(2),
        "exited before the land finished"
    );
    let (is_err, land) = tool_outcome(run.one(&json!(7)));
    assert!(!is_err, "{land}");
    assert_eq!(land["status"], "landed", "{land}");
    assert!(!tool_outcome(run.one(&json!(8))).0);
    assert_eq!(read(env.repo().join("bye.txt")), "bye\n");
    // The landed branch is recorded and no lock is left behind: a new call works.
    assert!(env
        .git(&env.repo(), &["branch", "--list", "shoal/bye"])
        .contains("shoal/bye"));
    let st = env.api("status", json!({})).unwrap();
    assert_eq!(st["initialized"], true);
}

#[test]
fn closing_stdin_with_nothing_in_flight_exits_zero_promptly() {
    let env = Env::new(5124);
    let run = run_bytes_in(&env, &env.repo(), Vec::new(), Duration::from_secs(10));
    assert_eq!(run.status.code(), Some(0));
    assert!(run.messages.is_empty());
    assert!(
        run.elapsed < Duration::from_secs(5),
        "took {:?}",
        run.elapsed
    );
    // Only a notification, then EOF.
    let run = run_lines(&env, &[notif("notifications/initialized", json!({}))]);
    assert_eq!(run.status.code(), Some(0));
    assert!(run.messages.is_empty());
}

#[test]
fn closing_stdin_during_auto_initialising_fork_leaves_a_consistent_project() {
    let env = Env::new(5182);
    write_config(
        &env,
        "setup = \"sleep 2; mkdir -p deps; echo ready > deps/ok\"\nbackend = \"clone\"\nport_base = 5182\n",
    );
    let run = run_lines(
        &env,
        &[call_line(json!(1), "fork", json!({"name": "late"}))],
    );
    assert_eq!(run.status.code(), Some(0), "{}", run.stderr);
    let (is_err, v) = tool_outcome(run.one(&json!(1)));
    assert!(!is_err, "{v}");
    let path = PathBuf::from(v["sessions"][0]["path"].as_str().unwrap());
    assert_eq!(read(path.join("deps/ok")), "ready\n");
    let st = env.api("status", json!({})).unwrap();
    assert_eq!(st["sessions"][0]["name"], "late");
}

// ---------------------------------------------------------------------------
// Large results
// ---------------------------------------------------------------------------

#[test]
fn very_large_status_result_arrives_intact_on_one_line() {
    let env = Env::new(5183);
    env.init("clone");
    for _ in 0..3 {
        let r = env
            .api("fork", json!({"name": "bulk", "count": 8}))
            .unwrap();
        assert_eq!(r["sessions"].as_array().unwrap().len(), 8, "{r}");
        assert!(r.get("error").is_none_or(Value::is_null), "{r}");
    }
    let st = env.api("status", json!({})).unwrap();
    let sessions = st["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 24);
    // Lots of uncommitted files with long names in every session, with
    // characters JSON must escape.
    let long = "n".repeat(120);
    for s in sessions {
        let p = PathBuf::from(s["path"].as_str().unwrap());
        for k in 0..60 {
            fs::write(p.join(format!("{long}-{k:02}-\u{e9}.txt")), "x").unwrap();
        }
        fs::write(p.join("quote\"and\\back.txt"), "y").unwrap();
    }
    let mut mcp = env.mcp();
    initialized(&mut mcp);
    // Use the raw line to check it is one well-formed JSON line.
    let (is_err, v) = mcp.call(1, "status", json!({}));
    assert!(!is_err, "{v}");
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 24);
    for s in sessions {
        assert_eq!(s["uncommitted_count"], 61, "{}", s["name"]);
        let listed = s["uncommitted"].as_array().unwrap();
        assert_eq!(listed.len(), 50, "uncommitted is capped at 50");
    }
    let size = v.to_string().len();
    assert!(size > 200_000, "result was only {size} bytes");
    // conflicts on the same big project is also delivered whole.
    let (is_err, c) = mcp.call(2, "conflicts", json!({}));
    assert!(!is_err, "{c}");
    assert_eq!(
        c["changed"].as_array().unwrap().len(),
        24,
        "{}",
        c.to_string().len()
    );
    assert!(mcp.next_message(Duration::from_millis(200)).is_none());
    assert!(mcp.close().success());
}
