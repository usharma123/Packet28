#[path = "support/hook_rewrite.rs"]
mod hook_rewrite;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use serde_json::{json, Value};
use tempfile::TempDir;

use hook_rewrite::{
    ensure_packet28d_built, init_repo, run_hook_raw, suite_cmd, write_repo_fixture, DaemonStopGuard,
};

#[test]
#[cfg(unix)]
fn test_hook_rewrite_runtimes_cursor_pretool_preserves_commands_and_returns_empty_json() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let payloads = [
        json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "command":"git status --short src/alpha.rs"
        }),
        json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-tool-input-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "tool_input":{"command":"git status --short src/alpha.rs"}
        }),
        json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-command-line-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "command_line":"git status --short src/alpha.rs"
        }),
        json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-shell-command-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "shell_command":"git status --short src/alpha.rs"
        }),
    ];
    for payload in payloads {
        let (status, stdout, _stderr) = run_hook_raw(
            "cursor",
            dir.path(),
            &serde_json::to_string(&payload).unwrap(),
        );
        assert_eq!(status, 0);
        let rendered: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert!(rendered.get("permission").is_none());
        assert!(rendered.get("updated_input").is_none());
    }

    let (status, stdout, _stderr) = run_hook_raw(
        "cursor",
        dir.path(),
        &serde_json::to_string(&json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-idempotent",
            "cwd":dir.path().to_str().unwrap(),
            "command":"git status --short src/alpha.rs"
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "{}");

    let (status, stdout, _stderr) = run_hook_raw(
        "cursor",
        dir.path(),
        &serde_json::to_string(&json!({
            "hook_event_name":"beforeShellExecution",
            "conversation_id":"cursor-session-noop",
            "cwd":dir.path().to_str().unwrap(),
            "command":"definitely-unsupported-packet28-tool --flag"
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert_eq!(stdout.trim(), "{}");

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_runtimes_gemini_before_tool_preserves_shell_command() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let (status, stdout, _stderr) = run_hook_raw(
        "gemini",
        dir.path(),
        &serde_json::to_string(&json!({
            "tool_name":"run_shell_command",
            "session_id":"gemini-session-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "tool_input":{"command":"git status --short src/alpha.rs"}
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());

    let (status, stdout, _stderr) = run_hook_raw(
        "gemini",
        dir.path(),
        &serde_json::to_string(&json!({
            "tool_name":"read_file",
            "session_id":"gemini-session-noop",
            "cwd":dir.path().to_str().unwrap(),
            "tool_input":{"path":"src/alpha.rs"}
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}
