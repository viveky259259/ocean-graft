//! `shoal mcp`: a Model Context Protocol server over stdio.
//!
//! Newline-delimited JSON-RPC 2.0. stdout carries protocol messages only;
//! everything else goes to stderr. Each tools/call runs on its own thread,
//! so a long `land` does not hold up `status`; the project lock keeps
//! changes consistent. Writes are serialised so messages never interleave.

use crate::tools;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

/// Revisions this server is known to speak. The tools-only subset used here
/// is the same in all of them, so a client asking for another revision gets
/// its own revision echoed back.
pub const KNOWN_PROTOCOLS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    "2026-07-28",
];
pub const FALLBACK_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "shoal gives you instant, isolated copy-on-write sessions of this repository: full checkouts with dependencies installed, each on its own branch and port. Use fork to try approaches in parallel or to keep risky work away from the user's checkout, checkpoint before risky steps and rewind if they go wrong, land to deliver tested work to the user's branch, preview to give the user a URL, and status whenever you need to know where things stand.";

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// The response to a tools/call: JSON text, with isError on failure.
pub fn tool_result(name: &str, args: &Value) -> Value {
    match tools::call(name, args) {
        Ok(value) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{"type": "text", "text": format!("{err:#}")}],
            "isError": true,
        }),
    }
}

/// Handle one message. Returns the response, or None for notifications.
/// tools/call is answered by `on_call` (possibly later, on another thread).
pub fn handle(msg: &Value, on_call: &mut dyn FnMut(Value, String, Value)) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error(
            Value::Null,
            -32600,
            "a request must be a JSON object",
        ));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response from the client, or junk: never answer a response.
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error(id.unwrap_or(Value::Null), -32600, "missing method"));
    };
    let Some(id) = id else {
        return None; // notifications (initialized, cancelled, ...) need no answer
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(FALLBACK_PROTOCOL);
            let version = if asked.is_empty() {
                FALLBACK_PROTOCOL
            } else {
                asked
            };
            if !KNOWN_PROTOCOLS.contains(&version) {
                eprintln!("shoal mcp: client asked for protocol {version}, which shoal has not been tested with; answering in kind");
            }
            Some(result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "shoal", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            ))
        }
        "ping" => Some(result(id, json!({}))),
        "tools/list" => Some(result(id, json!({"tools": tools::definitions()}))),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs params.name"));
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            on_call(id, name.to_string(), args);
            None
        }
        "resources/list" => Some(result(id, json!({"resources": []}))),
        "prompts/list" => Some(result(id, json!({"prompts": []}))),
        other => Some(error(id, -32601, format!("method not found: {other}"))),
    }
}

/// Serve on stdin/stdout until stdin closes.
pub fn serve() -> anyhow::Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let send = {
        let out = Arc::clone(&out);
        move |v: Value| {
            let mut line = serde_json::to_string(&v).unwrap_or_default();
            line.push('\n');
            let mut out = out.lock().expect("stdout lock");
            let _ = out.write_all(line.as_bytes());
            let _ = out.flush();
        }
    };
    let mut workers = Vec::new();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match input.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("shoal mcp: reading stdin failed: {err}");
                break;
            }
        }
        // One undecodable line is a parse error, not the end of the session.
        let line = match std::str::from_utf8(&raw) {
            Ok(line) => line.trim_end_matches(['\n', '\r']).to_string(),
            Err(err) => {
                send(error(Value::Null, -32700, format!("parse error: {err}")));
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                send(error(Value::Null, -32700, format!("parse error: {err}")));
                continue;
            }
        };
        let messages = match msg {
            Value::Array(batch) if batch.is_empty() => {
                send(error(Value::Null, -32600, "empty batch"));
                continue;
            }
            Value::Array(batch) => batch,
            single => vec![single],
        };
        for m in messages {
            let mut on_call = |id: Value, name: String, args: Value| {
                let send = send.clone();
                workers.push(std::thread::spawn(move || {
                    // A panic in a tool must still answer the call.
                    let outcome = std::panic::catch_unwind(|| tool_result(&name, &args))
                        .unwrap_or_else(|_| {
                            json!({
                                "content": [{"type": "text", "text": format!("shoal: internal error while running {name}; see stderr")}],
                                "isError": true,
                            })
                        });
                    send(result(id, outcome));
                }));
            };
            if let Some(response) = handle(&m, &mut on_call) {
                send(response);
            }
        }
        workers.retain(|w| !w.is_finished());
    }
    // stdin closed: let calls in flight finish and answer before exiting.
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(msg: Value) -> (Option<Value>, Vec<(Value, String, Value)>) {
        let mut calls = Vec::new();
        let r = handle(&msg, &mut |id, name, args| calls.push((id, name, args)));
        (r, calls)
    }

    #[test]
    fn initialize_echoes_the_requested_revision() {
        for v in KNOWN_PROTOCOLS.iter().chain(["2099-01-01"].iter()) {
            let (r, _) = run(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":v,"capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
            );
            let r = r.unwrap();
            assert_eq!(r["result"]["protocolVersion"], *v);
            assert_eq!(r["result"]["serverInfo"]["name"], "shoal");
            assert!(r["result"]["capabilities"]["tools"].is_object());
        }
        let (r, _) = run(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        assert_eq!(r.unwrap()["result"]["protocolVersion"], FALLBACK_PROTOCOL);
    }

    #[test]
    fn notifications_and_responses_get_no_answer() {
        assert!(
            run(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .0
                .is_none()
        );
        assert!(run(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}})
        )
        .0
        .is_none());
        assert!(run(json!({"jsonrpc":"2.0","id":9,"result":{}})).0.is_none());
        assert!(
            run(json!({"jsonrpc":"2.0","id":9,"error":{"code":1,"message":"x"}}))
                .0
                .is_none()
        );
    }

    #[test]
    fn ids_are_echoed_whatever_their_type() {
        for id in [json!(1), json!("abc"), json!(0), json!(-5)] {
            let (r, _) = run(json!({"jsonrpc":"2.0","id":id,"method":"ping"}));
            assert_eq!(r.unwrap()["id"], id);
        }
    }

    #[test]
    fn tools_list_has_every_tool_with_a_valid_schema() {
        let (r, _) = run(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let tools = r.unwrap()["result"]["tools"].as_array().unwrap().clone();
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
        for t in &tools {
            assert!(t["description"].as_str().unwrap().len() > 40);
            assert_eq!(t["inputSchema"]["type"], "object");
            let props = t["inputSchema"]["properties"].as_object().unwrap();
            for req in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(props.contains_key(req.as_str().unwrap()));
            }
            // Names Claude Code builds: mcp__plugin_shoal_shoal__<name>; keep them plain.
            assert!(t["name"]
                .as_str()
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_lowercase()));
        }
    }

    #[test]
    fn tools_call_is_dispatched_and_bad_calls_are_errors() {
        let (r, calls) = run(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{"x":1}}}),
        );
        assert!(r.is_none());
        assert_eq!(
            calls,
            vec![(json!(3), "status".to_string(), json!({"x":1}))]
        );
        let (r, calls) =
            run(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"status"}}));
        assert!(r.is_none());
        assert_eq!(calls[0].2, json!({}));
        let (r, _) = run(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}));
        assert_eq!(r.unwrap()["error"]["code"], -32602);
    }

    #[test]
    fn unknown_methods_and_malformed_requests_are_errors() {
        let (r, _) = run(json!({"jsonrpc":"2.0","id":6,"method":"sampling/createMessage"}));
        assert_eq!(r.unwrap()["error"]["code"], -32601);
        let (r, _) = run(json!([1, 2]));
        assert_eq!(r.unwrap()["error"]["code"], -32600);
        let (r, _) = run(json!({"jsonrpc":"2.0","id":7}));
        assert_eq!(r.unwrap()["error"]["code"], -32600);
        let (r, _) = run(json!({"jsonrpc":"2.0","id":8,"method":"resources/list"}));
        assert_eq!(r.unwrap()["result"]["resources"], json!([]));
    }

    #[test]
    fn tool_errors_are_reported_in_band() {
        let r = tool_result("no-such-tool", &json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
        let r = tool_result("status", &json!("not an object"));
        assert_eq!(r["isError"], true);
    }
}
