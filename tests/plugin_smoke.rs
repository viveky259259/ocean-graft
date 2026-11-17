//! Smoke test for the harness itself and the three plugin entry points.
mod common;
use common::*;
use serde_json::json;
use std::time::Duration;

#[test]
fn harness_drives_api_mcp_and_hooks() {
    let env = Env::new(5000);
    env.init("clone");
    let forked = env.api("fork", json!({"name": "a"})).unwrap();
    let path = forked["sessions"][0]["path"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&path)
        .join("deps/installed.txt")
        .exists());

    let mut mcp = env.mcp();
    let init = mcp.initialize();
    assert_eq!(init["result"]["protocolVersion"], "2026-07-28");
    let (err, status) = mcp.call(1, "status", json!({}));
    assert!(!err, "{status}");
    assert_eq!(status["sessions"][0]["name"], "a");
    assert!(
        mcp.next_message(Duration::from_millis(200)).is_none(),
        "no stray output"
    );
    assert!(mcp.close().success());

    let r = env.hook(
        "worktree-create",
        &json!({"name": "from-hook"}),
        &env.repo(),
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.last_line().ends_with("/sessions/from-hook"),
        "{}",
        r.stdout
    );
}
