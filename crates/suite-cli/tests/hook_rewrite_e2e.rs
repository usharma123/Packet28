#[path = "support/hook_rewrite.rs"]
mod hook_rewrite;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use std::fs;

use packet28_daemon_protocol::paths::{hook_runtime_config_path, runtime_path, task_registry_path};
use serde_json::{json, Value};
use tempfile::TempDir;

use hook_rewrite::{
    ensure_packet28d_built, init_repo, run_hook_raw, suite_cmd, write_repo_fixture, DaemonStopGuard,
};

fn run_claude_hook(root: &std::path::Path, payload: &Value) -> (i32, String) {
    let (status, stdout, stderr) =
        run_hook_raw("claude", root, &serde_json::to_string(payload).unwrap());
    if !stderr.is_empty() {
        eprintln!("hook diagnostic: {stderr}");
    }
    (status, stdout)
}

fn assert_capture_only(stdout: &str) {
    if stdout.trim().is_empty() {
        return;
    }
    let output: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(output.get("updatedInput").is_none(), "{output}");
    assert!(output.get("permissionDecision").is_none(), "{output}");
    assert!(
        output["hookSpecificOutput"].get("updatedInput").is_none(),
        "{output}"
    );
    assert!(
        output["hookSpecificOutput"]
            .get("permissionDecision")
            .is_none(),
        "{output}"
    );
}

fn assert_claude_capture(command: &str, family: &str, kind: &str) {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let task_id = format!("task-{family}-capture");
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let config = packet28_daemon_protocol::hooks::HookRuntimeConfig {
        rewrite_enabled: true,
        ..Default::default()
    };
    let original = serde_json::to_vec(&config).unwrap();
    fs::write(&config_path, &original).unwrap();
    let mut payload = json!({
        "hook_event_name":"PreToolUse", "task_id":task_id,
        "session_id":format!("session-{family}-capture"),
        "cwd":dir.path().to_str().unwrap(), "tool_name":"Bash",
        "tool_input":{"command":command}
    });
    let (status, stdout) = run_claude_hook(dir.path(), &payload);
    assert_eq!(status, 0);
    assert_capture_only(&stdout);
    payload["hook_event_name"] = json!("PostToolUse");
    payload["tool_response"] = json!({"stdout":"sample completion\n","stderr":"","exit_code":0});
    let (status, _) = run_claude_hook(dir.path(), &payload);
    assert_eq!(status, 0);
    let output = suite_cmd()
        .args([
            "daemon",
            "task",
            "status",
            "--task-id",
            &task_id,
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let task: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(task["task_id"], task_id, "{task}");
    assert_eq!(task["latest_hook_command_kind"], kind, "{task}");
    assert_eq!(fs::read(config_path).unwrap(), original);
    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_cli_degrades_gracefully_on_bad_json_and_no_rewrite() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());

    let (status, stdout, stderr) = run_hook_raw("claude", dir.path(), "{not json");
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());
    assert!(stderr.contains("malformed JSON"));

    let (status, stdout, stderr) = run_hook_raw("cursor", dir.path(), "{not json");
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());
    assert!(stderr.contains("malformed JSON"));

    let (status, stdout, stderr) = run_hook_raw("copilot", dir.path(), "{not json");
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());
    assert!(stderr.contains("malformed JSON"));

    let (status, stdout, stderr) = run_hook_raw("gemini", dir.path(), "{not json");
    assert_eq!(status, 0);
    assert!(stdout.trim().is_empty());
    assert!(stderr.contains("malformed JSON"));

    let (status, stdout) = run_claude_hook(
        dir.path(),
        &json!({
            "hook_event_name":"PreToolUse",
            "task_id":"task-pretool-no-rewrite",
            "session_id":"session-pretool-no-rewrite",
            "cwd":dir.path().to_str().unwrap(),
            "tool_name":"Bash",
            "tool_input":{"command":"definitely-unsupported-packet28-tool --flag"}
        }),
    );
    assert_eq!(status, 0);
    if !matches!(stdout.trim(), "" | "{}") {
        let rendered: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert!(
            rendered["hookSpecificOutput"]["updatedInput"].is_null(),
            "{stdout}"
        );
    }

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_cli_malformed_runtime_config_skips_claude_and_cursor_processing() {
    let dir = TempDir::new().unwrap();
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let original = b"{\"rewrite_enabled\": tru".to_vec();
    fs::write(&config_path, &original).unwrap();
    let root = dir.path().to_str().unwrap();
    let payloads = [
        (
            "claude",
            json!({
                "hook_event_name":"PreToolUse",
                "task_id":"task-invalid-config-claude",
                "session_id":"session-invalid-config-claude",
                "cwd":root,
                "tool_name":"Bash",
                "tool_input":{"command":"git status --short"}
            }),
        ),
        (
            "cursor",
            json!({
                "hook_event_name":"beforeShellExecution",
                "conversation_id":"session-invalid-config-cursor",
                "cwd":root,
                "command":"git status --short"
            }),
        ),
    ];

    for (runtime, payload) in payloads {
        let (status, stdout, stderr) = run_hook_raw(
            runtime,
            dir.path(),
            &serde_json::to_string(&payload).unwrap(),
        );
        assert_eq!(status, 0, "{runtime}: {stderr}");
        assert!(stdout.trim().is_empty(), "{runtime}: {stdout}");
        assert!(
            stderr.contains("failed to parse hook runtime config"),
            "{runtime}: {stderr}"
        );
    }

    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!runtime_path(dir.path()).exists());
    assert!(!task_registry_path(dir.path()).exists());
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_cli_unreadable_runtime_config_skips_processing() {
    let dir = TempDir::new().unwrap();
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(&config_path).unwrap();
    let marker_path = config_path.join("preserve.bin");
    let original = vec![0x00, 0xff, 0x7f];
    fs::write(&marker_path, &original).unwrap();
    let payload = json!({
        "hook_event_name":"PreToolUse",
        "task_id":"task-unreadable-config",
        "session_id":"session-unreadable-config",
        "cwd":dir.path().to_str().unwrap(),
        "tool_name":"Bash",
        "tool_input":{"command":"git status --short"}
    });

    let (status, stdout, stderr) = run_hook_raw(
        "claude",
        dir.path(),
        &serde_json::to_string(&payload).unwrap(),
    );

    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert!(
        stderr.contains("failed to read hook runtime config"),
        "{stderr}"
    );
    assert_eq!(fs::read(marker_path).unwrap(), original);
    assert!(config_path.is_dir());
    assert!(!task_registry_path(dir.path()).exists());
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_status_reports_capture_only_when_runtime_config_is_missing() {
    let dir = TempDir::new().unwrap();

    let output = suite_cmd()
        .args([
            "hook",
            "rewrite",
            "--root",
            dir.path().to_str().unwrap(),
            "status",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        (
            status["rewrite_enabled"].as_bool(),
            status["hooks_enabled"].as_bool(),
            status["fallback_post_tool_capture"].as_bool(),
        ),
        (Some(false), Some(true), Some(true))
    );
    assert!(!hook_runtime_config_path(dir.path()).exists());
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_status_reports_stored_true_flag_as_inactive() {
    let dir = TempDir::new().unwrap();
    let path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = serde_json::to_vec(&packet28_daemon_protocol::hooks::HookRuntimeConfig {
        rewrite_enabled: true,
        ..Default::default()
    })
    .unwrap();
    fs::write(&path, &original).unwrap();
    let output = suite_cmd()
        .args([
            "hook",
            "rewrite",
            "--root",
            dir.path().to_str().unwrap(),
            "status",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["rewrite_enabled"], false);
    assert_eq!(status["legacy_rewrite_enabled"], true);
    assert_eq!(status["mode"], "capture_only");
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_status_rejects_malformed_config_without_replacing_bytes() {
    let dir = TempDir::new().unwrap();
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let original = b"{\"hooks_enabled\": tru".to_vec();
    fs::write(&config_path, &original).unwrap();

    let output = suite_cmd()
        .args([
            "hook",
            "rewrite",
            "--root",
            dir.path().to_str().unwrap(),
            "status",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to parse hook runtime config"),
        "{output:?}"
    );
    assert_eq!(fs::read(config_path).unwrap(), original);
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_toggle_rejects_invalid_utf8_without_replacing_bytes() {
    let dir = TempDir::new().unwrap();
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let original = vec![b'{', b'}', 0xff];
    fs::write(&config_path, &original).unwrap();

    let output = suite_cmd()
        .args([
            "hook",
            "rewrite",
            "--root",
            dir.path().to_str().unwrap(),
            "off",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to read hook runtime config"),
        "{output:?}"
    );
    assert_eq!(fs::read(config_path).unwrap(), original);
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_off_clears_legacy_flag_and_on_is_explicitly_unsupported() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let root = dir.path().to_str().unwrap();

    suite_cmd()
        .args(["hook", "rewrite", "--root", root, "off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("disabled"));

    let (status, stdout) = run_claude_hook(
        dir.path(),
        &json!({
            "hook_event_name":"PreToolUse",
            "task_id":"task-pretool-rewrite-off",
            "session_id":"session-pretool-rewrite-off",
            "cwd":root,
            "tool_name":"Bash",
            "tool_input":{"command":"git status --short src/alpha.rs"}
        }),
    );
    assert_eq!(status, 0);
    if !matches!(stdout.trim(), "" | "{}") {
        let rendered: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert!(
            rendered["hookSpecificOutput"]["updatedInput"].is_null(),
            "{stdout}"
        );
    }

    let status_output = suite_cmd()
        .args(["hook", "rewrite", "--root", root, "status", "--json"])
        .output()
        .unwrap();
    assert!(status_output.status.success());
    let status_json: Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status_json["rewrite_enabled"], false);
    assert_eq!(status_json["fallback_post_tool_capture"], true);

    let config_path = hook_runtime_config_path(dir.path());
    let before = fs::read(&config_path).unwrap();
    suite_cmd()
        .args(["hook", "rewrite", "--root", root, "on"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported"));
    assert_eq!(fs::read(config_path).unwrap(), before);

    suite_cmd()
        .args(["daemon", "stop", "--root", root])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_cli_is_idempotent_and_ignores_non_bash_tools() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let base_payload = json!({
        "hook_event_name":"PreToolUse",
        "task_id":"task-pretool-idempotent",
        "session_id":"session-pretool-idempotent",
        "cwd":dir.path().to_str().unwrap(),
        "tool_name":"Bash",
        "tool_input":{"command":"git status --short src/alpha.rs"}
    });
    let (status, stdout) = run_claude_hook(dir.path(), &base_payload);
    assert_eq!(status, 0);
    assert_capture_only(&stdout);
    let rewritten = "git status --short src/alpha.rs";

    let (status, stdout) = run_claude_hook(
        dir.path(),
        &json!({
            "hook_event_name":"PreToolUse",
            "task_id":"task-pretool-idempotent",
            "session_id":"session-pretool-idempotent",
            "cwd":dir.path().to_str().unwrap(),
            "tool_name":"Bash",
            "tool_input":{"command": rewritten}
        }),
    );
    assert_eq!(status, 0);
    assert_capture_only(&stdout);

    let (status, stdout) = run_claude_hook(
        dir.path(),
        &json!({
            "hook_event_name":"PreToolUse",
            "task_id":"task-pretool-non-bash",
            "session_id":"session-pretool-idempotent",
            "cwd":dir.path().to_str().unwrap(),
            "tool_name":"Read",
            "tool_input":{"file_path":"src/alpha.rs"}
        }),
    );
    assert_eq!(status, 0);
    assert!(matches!(stdout.trim(), "" | "{}"));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_git_command() {
    assert_claude_capture("git status --short src/alpha.rs", "git", "git_status");
}

#[test]
#[cfg(unix)]
fn test_hook_rewrite_cli_does_not_rewrite_grep_extraction_pipeline() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let _daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let (status, stdout) = run_claude_hook(
        dir.path(),
        &json!({
            "hook_event_name":"PreToolUse",
            "task_id":"task-pretool-grep-pipeline",
            "session_id":"session-pretool-grep-pipeline",
            "cwd":dir.path().to_str().unwrap(),
            "tool_name":"Bash",
            "tool_input":{"command":"grep -o 'Alpha' src/alpha.rs | sort -u | wc -l"}
        }),
    );
    assert_eq!(status, 0);
    if !matches!(stdout.trim(), "" | "{}") {
        let rendered: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert!(
            rendered["hookSpecificOutput"]["updatedInput"].is_null(),
            "{stdout}"
        );
    }

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_github_command() {
    assert_claude_capture("gh pr list --limit 5", "github", "gh_pr_list");
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_python_command() {
    assert_claude_capture("python3 -m pytest tests", "python", "python_pytest");
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_javascript_command() {
    assert_claude_capture("npx tsc --noEmit", "javascript", "javascript_tsc");
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_go_command() {
    assert_claude_capture("go test ./...", "go", "go_test");
}

#[test]
#[cfg(unix)]
fn test_hook_capture_preserves_supported_infra_command() {
    assert_claude_capture("kubectl get pods", "infra", "kubectl_get");
}
