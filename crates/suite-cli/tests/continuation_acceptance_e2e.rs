#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

#[expect(
    dead_code,
    reason = "this binary uses packet wrapper parsing from the shared support"
)]
#[path = "support/context_packet.rs"]
mod context_packet;

use process_harness::{HarnessLimits, McpHarness, ProcessHarness};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const IO_TIMEOUT: Duration = Duration::from_secs(15);

fn command(root: &Path, home: &Path) -> std::process::Command {
    let binary_directory = Path::new(env!("CARGO_BIN_EXE_Packet28")).parent().unwrap();
    let mut paths = vec![binary_directory.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command
        .current_dir(root)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"));
    command
}

fn start_mcp(root: &Path, home: &Path, runtime: &str) -> McpHarness {
    let mut command = command(root, home);
    command.args(["mcp", "serve", "--root", root.to_str().unwrap()]);
    let mut server = McpHarness::spawn(&mut command, HarnessLimits::default()).unwrap();
    server
        .request_with_id(
            json!(1),
            "initialize",
            json!({
                "protocolVersion":"2024-11-05", "capabilities":{},
                "clientInfo":{"name":format!("synthetic-{runtime}-contract-client"),"version":"1"}
            }),
            IO_TIMEOUT,
        )
        .unwrap();
    server
}

fn tool(server: &mut McpHarness, id: u64, name: &str, arguments: Value) -> Value {
    let response = server
        .request_with_id(
            json!(id),
            "tools/call",
            json!({
                "name":name, "arguments":arguments
            }),
            IO_TIMEOUT,
        )
        .unwrap();
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["structuredContent"].clone()
}

fn hook(root: &Path, home: &Path, runtime: &str, payload: Value) -> Value {
    let mut command = command(root, home);
    command.args(["hook", runtime, "--root", root.to_str().unwrap()]);
    let output = ProcessHarness::run(
        &mut command,
        &serde_json::to_vec(&payload).unwrap(),
        IO_TIMEOUT,
        HarnessLimits::default(),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.stderr.is_empty() {
        eprintln!(
            "{runtime} hook diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn stop_daemon(root: &Path, home: &Path) {
    let mut command = command(root, home);
    command.args(["daemon", "stop", "--root", root.to_str().unwrap()]);
    let output =
        ProcessHarness::run(&mut command, &[], IO_TIMEOUT, HarnessLimits::default()).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Stop acknowledges the request before the daemon releases its authority.
    // Wait for that release so the following client really starts a new daemon.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(root) {
            Ok(lease) => {
                drop(lease);
                break;
            }
            Err(packet28_daemon_core::DaemonCoreError::DaemonInstanceAlreadyRunning { .. }) => {
                assert!(Instant::now() < deadline, "fixture daemon did not stop");
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("fixture daemon authority check failed: {error}"),
        }
    }
}

struct FixtureDaemonGuard<'a> {
    root: &'a Path,
    home: &'a Path,
}

impl Drop for FixtureDaemonGuard<'_> {
    fn drop(&mut self) {
        let mut command = command(self.root, self.home);
        command.args(["daemon", "stop", "--root", self.root.to_str().unwrap()]);
        let _ = ProcessHarness::run(
            &mut command,
            &[],
            Duration::from_secs(5),
            HarnessLimits::default(),
        );
    }
}

// These fixtures use the documented host envelopes and the real Packet28
// CLI/MCP/daemon. They do not call a model provider or claim live host execution.
fn continuation_survives_restart(runtime: &str) {
    process_harness::ensure_packet28d_built();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let _daemon = FixtureDaemonGuard {
        root: root.path(),
        home: home.path(),
    };
    process_harness::run_git(root.path(), &["init"]);
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/alpha.rs"), "struct Alpha;\n").unwrap();
    let task_id = format!("task-{runtime}-continuation");
    let session_id = format!("session-{runtime}-continuation");
    let mut server = start_mcp(root.path(), home.path(), runtime);
    let written = tool(
        &mut server,
        2,
        "packet28.write_intention",
        json!({
            "task_id":task_id, "text":"Inspect Alpha before editing", "step_id":"inspect", "paths":["src/alpha.rs"]
        }),
    );
    assert_eq!(written["accepted"], true);
    tool(
        &mut server,
        3,
        "packet28.hypothesis_add",
        json!({
            "task_id":task_id, "id":"obsolete-alpha", "text":"Alpha is still the old definition", "paths":["src/alpha.rs"]
        }),
    );
    // Replace the decision before checkpointing so restart must not revive it.
    tool(
        &mut server,
        4,
        "packet28.hypothesis_resolve",
        json!({
            "task_id":task_id, "id":"obsolete-alpha", "status":"rejected"
        }),
    );
    tool(
        &mut server,
        5,
        "packet28.hypothesis_add",
        json!({
            "task_id":task_id, "id":"current-alpha", "text":"Use the revised Alpha implementation", "paths":["src/alpha.rs"]
        }),
    );
    let latest = tool(
        &mut server,
        6,
        "packet28.write_intention",
        json!({
            "task_id":task_id, "text":"Verify revised Alpha", "step_id":"verify", "paths":["src/alpha.rs"]
        }),
    );
    assert_eq!(latest["accepted"], true);
    hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"PostToolUse", "session_id":session_id, "task_id":task_id,
            "turn_id":"turn-1", "tool_use_id":"command-1", "tool_name":"Bash",
            "tool_input":{"command":"grep -n Alpha src/alpha.rs"},
            "tool_response":if runtime == "codex" { json!("1:struct Alpha;\n") } else { json!({"stdout":"1:struct Alpha;\n","stderr":"","exit_code":0}) }
        }),
    );
    hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"Stop", "session_id":session_id, "task_id":task_id,
            "turn_id":"turn-1", "stop_hook_active":false
        }),
    );
    let before = tool(
        &mut server,
        7,
        "packet28.prepare_handoff",
        json!({"task_id":task_id,"response_mode":"full"}),
    );
    assert_eq!(before["handoff_ready"], true);
    assert_eq!(before["latest_intention"]["text"], "Verify revised Alpha");
    let artifact_id = before["context"]["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    stop_daemon(root.path(), home.path());
    server.finish(IO_TIMEOUT).unwrap();

    let mut resumed = start_mcp(root.path(), home.path(), runtime);
    let hypotheses = tool(
        &mut resumed,
        2,
        "packet28.hypothesis_list",
        json!({"task_id":task_id}),
    );
    let hypotheses = hypotheses.as_array().unwrap();
    assert_eq!(hypotheses.len(), 1);
    assert_eq!(hypotheses[0]["id"], "current-alpha");
    let fetched = tool(
        &mut resumed,
        3,
        "packet28.fetch_context",
        json!({"task_id":task_id,"artifact_id":artifact_id}),
    );
    assert_eq!(fetched["latest_intention"]["text"], "Verify revised Alpha");
    let output = hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"SessionStart", "session_id":format!("{session_id}-resume"),
            "task_id":task_id, "source":"resume", "cwd":root.path().to_str().unwrap()
        }),
    );
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("Verify revised Alpha"), "{context}");
    assert!(
        !context.contains("Inspect Alpha before editing"),
        "{context}"
    );
    stop_daemon(root.path(), home.path());
    resumed.finish(IO_TIMEOUT).unwrap();
}

#[test]
#[cfg(unix)]
fn claude_contract_continuation_preserves_latest_state_after_daemon_restart() {
    continuation_survives_restart("claude");
}

#[test]
#[cfg(unix)]
fn codex_native_contract_continuation_preserves_latest_state_after_daemon_restart() {
    continuation_survives_restart("codex");
}

#[test]
#[cfg(unix)]
fn codex_doctor_exercises_the_codex_adapter_and_keeps_host_trust_unverified() {
    process_harness::ensure_packet28d_built();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let _daemon = FixtureDaemonGuard {
        root: root.path(),
        home: home.path(),
    };
    process_harness::run_git(root.path(), &["init"]);
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/alpha.rs"), "struct Alpha;\n").unwrap();
    process_harness::run_git(root.path(), &["add", "src/alpha.rs"]);
    process_harness::run_git(
        root.path(),
        &[
            "-c",
            "user.name=Packet28 Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "Initial doctor fixture",
        ],
    );
    let mut setup = command(root.path(), home.path());
    setup.args(["setup", "--runtime", "codex", "--yes"]);
    let output =
        ProcessHarness::run(&mut setup, &[], IO_TIMEOUT, HarnessLimits::default()).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut doctor = command(root.path(), home.path());
    doctor.args(["doctor", "--agent", "codex", "--json"]);
    let output = ProcessHarness::run(
        &mut doctor,
        &[],
        Duration::from_secs(45),
        HarnessLimits::default(),
    )
    .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["reducer_round_trip"]["ok"], true, "{report}");
    assert_eq!(report["handoff_round_trip"]["ok"], true, "{report}");
    let trust = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "codex_hook_trust")
        .unwrap();
    assert_eq!(trust["ok"], false);
    assert_eq!(trust["required"], false);
    let telemetry = rusqlite::Connection::open(home.path().join(".packet28/packet28.db")).unwrap();
    let codex_events: u64 = telemetry
        .query_row(
            "SELECT COUNT(*) FROM hook_events WHERE runtime = 'codex'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let claude_events: u64 = telemetry
        .query_row(
            "SELECT COUNT(*) FROM hook_events WHERE runtime = 'claude'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(codex_events >= 3);
    assert_eq!(claude_events, 0);
    stop_daemon(root.path(), home.path());
}

#[cfg(unix)]
fn repeated_host_commands_execute_each_request(runtime: &str) {
    use std::os::unix::fs::PermissionsExt;
    process_harness::ensure_packet28d_built();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let _daemon = FixtureDaemonGuard {
        root: root.path(),
        home: home.path(),
    };
    process_harness::run_git(root.path(), &["init"]);
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/alpha.rs"), "struct Alpha;\n").unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let cat = bin.join("cat");
    fs::write(
        &cat,
        "#!/bin/sh\nprintf '%s\\n' \"$PACKET28_ACCEPTANCE\" >> invocations\nexec /bin/cat \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&cat, fs::Permissions::from_mode(0o700)).unwrap();
    let task_id = format!("task-{runtime}-repeat");
    let mut server = start_mcp(root.path(), home.path(), runtime);
    tool(
        &mut server,
        2,
        "packet28.write_intention",
        json!({
            "task_id":task_id, "text":"Run both explicit cat requests", "paths":["src/alpha.rs"]
        }),
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for invocation in 1..=2 {
        let output = hook(
            root.path(),
            home.path(),
            runtime,
            json!({
                "hook_event_name":"PreToolUse", "session_id":format!("session-{runtime}-repeat"),
                "task_id":task_id, "turn_id":"turn-repeat", "tool_use_id":format!("request-{invocation}"),
                "cwd":root.path().to_str().unwrap(), "tool_name":"Bash",
                "tool_input":{"command":"PACKET28_ACCEPTANCE=visible cat src/alpha.rs"}
            }),
        );
        assert!(
            output["hookSpecificOutput"]["updatedInput"].is_null(),
            "{output}"
        );
        assert!(
            output["hookSpecificOutput"]["permissionDecision"].is_null(),
            "{output}"
        );
        let executed = "PACKET28_ACCEPTANCE=visible cat src/alpha.rs";
        let mut command = std::process::Command::new("sh");
        command
            .current_dir(root.path())
            .env("HOME", home.path())
            .env("PATH", &path)
            .args(["-c", executed]);
        let execution =
            ProcessHarness::run(&mut command, &[], IO_TIMEOUT, HarnessLimits::default()).unwrap();
        assert!(
            execution.status.success(),
            "{}",
            String::from_utf8_lossy(&execution.stderr)
        );
        let calls = fs::read_to_string(root.path().join("invocations")).unwrap();
        assert_eq!(calls.lines().count(), invocation);
        assert!(calls.lines().all(|value| value == "visible"), "{calls}");
    }
    stop_daemon(root.path(), home.path());
    server.finish(IO_TIMEOUT).unwrap();
}

#[test]
#[cfg(unix)]
fn claude_original_repeated_requests_execute_twice_with_the_requested_environment() {
    repeated_host_commands_execute_each_request("claude");
}

#[test]
#[cfg(unix)]
fn codex_native_capture_hooks_preserve_host_commands_and_requested_environment() {
    repeated_host_commands_execute_each_request("codex");
}

fn append_trusted_state_event(root: &Path, home: &Path, task_id: &str, event: Value) {
    let input = root.join(format!("{}.json", event["event_id"].as_str().unwrap()));
    fs::write(&input, serde_json::to_vec(&event).unwrap()).unwrap();
    let mut child = command(root, home);
    child.args([
        "--via-daemon",
        "--daemon-root",
        root.to_str().unwrap(),
        "context",
        "state",
        "append",
        "--task-id",
        task_id,
        "--input",
        input.to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
        "--json",
    ]);
    let output =
        ProcessHarness::run(&mut child, &[], IO_TIMEOUT, HarnessLimits::default()).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_recovery_preserves_trusted_continuation(runtime: &str) {
    use packet28_daemon_protocol::paths::{
        task_event_log_path, task_events_dir, task_registry_path, TaskStorageId,
    };
    use packet28_daemon_protocol::task::{TaskLifecycle, TaskRegistry};
    process_harness::ensure_packet28d_built();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let _daemon = FixtureDaemonGuard {
        root: root.path(),
        home: home.path(),
    };
    process_harness::run_git(root.path(), &["init"]);
    let predecessor = format!("task-{runtime}-damaged");
    let mut initial = start_mcp(root.path(), home.path(), runtime);
    tool(
        &mut initial,
        2,
        "packet28.write_intention",
        json!({
            "task_id":predecessor, "text":"Keep the latest trusted objective after damaged history"
        }),
    );
    for event in [
        json!({"event_id":"trusted-decision","occurred_at_unix":1,"actor":"agent","kind":"decision_added","data":{"type":"decision_added","decision_id":"d1","text":"Keep the approved storage decision"}}),
        json!({"event_id":"trusted-question","occurred_at_unix":2,"actor":"agent","kind":"question_opened","data":{"type":"question_opened","question_id":"q1","text":"This question was already resolved"}}),
        json!({"event_id":"trusted-resolution","occurred_at_unix":3,"actor":"agent","kind":"question_resolved","data":{"type":"question_resolved","question_id":"q1"}}),
    ] {
        append_trusted_state_event(root.path(), home.path(), &predecessor, event);
    }
    hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"Stop", "task_id":predecessor,
            "session_id":format!("session-{runtime}-damaged"), "stop_hook_active":false,
        }),
    );
    let before = tool(
        &mut initial,
        3,
        "packet28.prepare_handoff",
        json!({"task_id":predecessor,"response_mode":"full"}),
    );
    assert!(before["handoff_ready"].as_bool().unwrap());
    let artifact_id = before["context"]["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    stop_daemon(root.path(), home.path());
    initial.finish(IO_TIMEOUT).unwrap();
    let predecessor_id = TaskStorageId::try_from(predecessor.as_str()).unwrap();
    let original_path = task_event_log_path(root.path(), &predecessor_id);
    let mut damaged_bytes = fs::read(&original_path).unwrap();
    let prior_high_water = damaged_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Value>(line).unwrap()["seq"]
                .as_u64()
                .unwrap()
        })
        .max()
        .unwrap();
    damaged_bytes.extend_from_slice(b"{broken-json-history\n");
    fs::write(&original_path, &damaged_bytes).unwrap();

    let mut resumed = start_mcp(root.path(), home.path(), runtime);
    let registry: TaskRegistry =
        serde_json::from_slice(&fs::read(task_registry_path(root.path())).unwrap()).unwrap();
    let old = &registry.tasks[&predecessor];
    let link = old.superseded_by.as_ref().unwrap();
    let successor = &link.successor_task_id;
    assert_ne!(successor, &predecessor);
    assert_eq!(old.lifecycle, TaskLifecycle::Cancelled);
    assert_eq!(old.last_event_seq, 0);
    assert_eq!(link.prior_last_event_seq, prior_high_water);
    assert_eq!(
        registry.tasks[successor].recovered_from.as_ref(),
        Some(link)
    );
    let quarantine =
        task_events_dir(root.path()).join(link.quarantined_event_log.as_ref().unwrap());
    assert_eq!(fs::read(&quarantine).unwrap(), damaged_bytes);
    assert!(!original_path.exists());

    let output = hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"SessionStart", "session_id":format!("session-{runtime}-damaged-resume"),
            "task_id":predecessor, "source":"resume", "cwd":root.path().to_str().unwrap()
        }),
    );
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("missing linked SessionStart context for {runtime}: {output}"));
    assert!(
        context.contains("Keep the latest trusted objective"),
        "{context}"
    );
    let repeated = hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"SessionStart", "session_id":format!("session-{runtime}-damaged-resume"),
            "task_id":predecessor, "source":"resume", "cwd":root.path().to_str().unwrap()
        }),
    );
    assert!(
        repeated["hookSpecificOutput"]["additionalContext"].is_null(),
        "same session must not receive duplicate context: {repeated}"
    );
    let fresh_session = hook(
        root.path(),
        home.path(),
        runtime,
        json!({
            "hook_event_name":"SessionStart", "session_id":format!("session-{runtime}-new-worker"),
            "task_id":predecessor, "source":"resume", "cwd":root.path().to_str().unwrap()
        }),
    );
    assert!(
        fresh_session["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .is_some_and(|value| value.contains("Keep the latest trusted objective")),
        "new session must receive the existing context: {fresh_session}"
    );

    let fetched = tool(
        &mut resumed,
        2,
        "packet28.fetch_context",
        json!({"task_id":successor,"artifact_id":artifact_id}),
    );
    assert_eq!(
        fetched["latest_intention"]["text"],
        "Keep the latest trusted objective after damaged history"
    );

    let predecessor_fetched = tool(
        &mut resumed,
        20,
        "packet28.fetch_context",
        json!({"task_id":predecessor,"artifact_id":artifact_id}),
    );
    assert_eq!(
        predecessor_fetched["latest_intention"],
        fetched["latest_intention"]
    );

    let mut snapshot_command = command(root.path(), home.path());
    snapshot_command.args([
        "--via-daemon",
        "--daemon-root",
        root.path().to_str().unwrap(),
        "context",
        "state",
        "snapshot",
        "--task-id",
        successor,
        "--root",
        root.path().to_str().unwrap(),
        "--json",
    ]);
    let snapshot_output = ProcessHarness::run(
        &mut snapshot_command,
        &[],
        IO_TIMEOUT,
        HarnessLimits::default(),
    )
    .unwrap();
    assert!(
        snapshot_output.status.success(),
        "{}",
        String::from_utf8_lossy(&snapshot_output.stderr)
    );
    // Use shared support/context_packet.rs packet wrapper helpers here.
    let wrapper =
        context_packet::parse_packet_wrapper(&snapshot_output.stdout, "suite.agent.snapshot.v1");
    let snapshot = context_packet::packet_payload(&wrapper);
    assert_eq!(snapshot["task_id"], *successor);
    assert_eq!(
        snapshot["active_decisions"][0]["text"],
        "Keep the approved storage decision"
    );
    assert!(snapshot["open_questions"].as_array().unwrap().is_empty());

    let latest = tool(
        &mut resumed,
        3,
        "packet28.write_intention",
        json!({"task_id":predecessor,"text":"Verify the linked successor"}),
    );
    assert!(latest["accepted"].as_bool().unwrap());
    let successor_id = TaskStorageId::try_from(successor.as_str()).unwrap();
    let events = fs::read_to_string(task_event_log_path(root.path(), &successor_id)).unwrap();
    let frames: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.first().unwrap()["seq"], 1);
    assert!(frames.iter().all(|frame| frame["task_id"] == *successor));
    assert_eq!(fs::read(quarantine).unwrap(), damaged_bytes);
    assert!(!original_path.exists());
    stop_daemon(root.path(), home.path());
    resumed.finish(IO_TIMEOUT).unwrap();
}

#[test]
#[cfg(unix)]
fn claude_linked_recovery_preserves_trusted_context_and_damaged_bytes() {
    linked_recovery_preserves_trusted_continuation("claude");
}

#[test]
#[cfg(unix)]
fn codex_linked_recovery_preserves_trusted_context_and_damaged_bytes() {
    linked_recovery_preserves_trusted_continuation("codex");
}
