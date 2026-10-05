#[path = "support/hook_rewrite.rs"]
mod hook_rewrite;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use serde_json::json;
use tempfile::TempDir;

use hook_rewrite::{
    ensure_packet28d_built, init_repo, run_hook_raw, suite_cmd, write_repo_fixture, DaemonStopGuard,
};

#[test]
#[cfg(unix)]
fn test_hook_rewrite_copilot_runtime_preserves_vscode_and_cli_host_commands() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let (status, stdout, _stderr) = run_hook_raw(
        "copilot",
        dir.path(),
        &serde_json::to_string(&json!({
            "tool_name":"Bash",
            "session_id":"copilot-vscode-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "tool_input":{"command":"git status --short src/alpha.rs"}
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());

    let tool_args = serde_json::to_string(&json!({
        "command":"git status --short src/alpha.rs"
    }))
    .unwrap();
    let (status, stdout, _stderr) = run_hook_raw(
        "copilot",
        dir.path(),
        &serde_json::to_string(&json!({
            "toolName":"bash",
            "toolArgs":tool_args
        }))
        .unwrap(),
    );
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());

    let (status, stdout, _stderr) = run_hook_raw(
        "copilot",
        dir.path(),
        &serde_json::to_string(&json!({
            "toolName":"view",
            "toolArgs":"{}"
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
