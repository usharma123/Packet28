#[expect(
    dead_code,
    reason = "shared lifecycle fixtures support native and proxy MCP test binaries"
)]
#[cfg(unix)]
#[path = "support/mcp_lifecycle.rs"]
mod mcp_lifecycle;
#[expect(
    dead_code,
    reason = "this protocol fixture uses the shared build and command harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[cfg(unix)]
use mcp_lifecycle::{corrupt_task_event_log, large_response_batch};
#[cfg(unix)]
use process_harness::BackpressuredMcpHarness;
use process_harness::{HarnessLimits, McpHarness};

const MCP_IO_TIMEOUT: Duration = Duration::from_secs(10);
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn mcp_cmd() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"))
}

fn ensure_packet28d_built() {
    process_harness::ensure_packet28d_built();
}

fn write_repo_fixture(root: &Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("alpha.rs"),
        r#"
use crate::beta::Beta;

fn alpha() {}
struct Alpha;
"#,
    )
    .unwrap();
    fs::write(
        src.join("beta.rs"),
        r#"
fn beta() {}
enum Beta {
  A,
}
"#,
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) {
    process_harness::run_git(root, args);
}

fn init_repo(root: &Path) {
    write_repo_fixture(root);
    git(root, &["init"]);
}

fn write_mcp_message_newline(server: &mut McpHarness, value: &Value) {
    server
        .send_value(value, MCP_IO_TIMEOUT)
        .unwrap_or_else(|error| panic!("failed to write newline MCP message: {error}"));
}

fn read_mcp_message_newline(server: &mut McpHarness) -> Value {
    server
        .receive(MCP_IO_TIMEOUT)
        .unwrap_or_else(|error| panic!("failed to read newline MCP message: {error}"))
}

fn start_mcp_server(root: &Path) -> McpHarness {
    let mut command = mcp_cmd();
    command
        .current_dir(root)
        .args(["mcp", "serve", "--root", root.to_str().unwrap()]);
    McpHarness::spawn_newline_json(&mut command, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("failed to start newline MCP server: {error}"))
}

fn workspace_packet28_version() -> String {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = workspace.parent().unwrap().parent().unwrap();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let value: toml::Value = toml::from_str(&manifest).unwrap();
    value["workspace"]["package"]["version"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
#[cfg(unix)]
fn test_idle_mcp_session_releases_task_store_for_retention() {
    use packet28_daemon_core::task_store_lease::try_acquire_task_store_retention_lease;

    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let mut server = start_mcp_server(dir.path());

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"retention-test","version":"1"}
            }
        }),
    );
    assert_eq!(read_mcp_message_newline(&mut server)["id"], 1);

    let stop = mcp_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "daemon stop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let retention = loop {
        if let Some(retention) = try_acquire_task_store_retention_lease(dir.path()).unwrap() {
            break retention;
        }
        assert!(
            Instant::now() < deadline,
            "idle MCP session retained the task-store writer lease"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    drop(retention);

    write_mcp_message_newline(
        &mut server,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert_eq!(read_mcp_message_newline(&mut server)["id"], 2);

    server
        .finish(MCP_SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("failed to stop newline MCP server: {error}"));
}

#[test]
#[cfg(unix)]
fn test_mcp_native_poller_failure_cancels_a_backpressured_stdout_write() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());

    let task_id = "task-native-poller-failed-with-blocked-stdout";
    let mut command = mcp_cmd();
    command
        .current_dir(dir.path())
        .args(["mcp", "serve", "--root", dir.path().to_str().unwrap()]);
    let mut server =
        BackpressuredMcpHarness::spawn_newline_json(&mut command, HarnessLimits::default())
            .unwrap();

    server
        .send_value(
            &json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"initialize",
                "params":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{},
                    "clientInfo":{"name":"stdout-backpressure-test","version":"1"}
                }
            }),
            MCP_IO_TIMEOUT,
        )
        .unwrap();
    assert_eq!(server.receive(MCP_IO_TIMEOUT).unwrap()["id"], 1);
    server
        .send_value(
            &json!({
                "jsonrpc":"2.0",
                "id":2,
                "method":"tools/call",
                "params":{
                    "name":"packet28.task_status",
                    "arguments":{
                        "task_id":task_id
                    }
                }
            }),
            MCP_IO_TIMEOUT,
        )
        .unwrap();
    let write_response = server.receive(MCP_IO_TIMEOUT).unwrap();
    assert_eq!(write_response["id"], 2);
    assert!(
        write_response.get("result").is_some(),
        "task_status failed: {write_response}"
    );

    let batch = large_response_batch();
    let response_lower_bound = server.send_value(&batch, MCP_IO_TIMEOUT).unwrap();
    server
        .wait_for_stdout_backpressure(response_lower_bound, Duration::from_secs(3))
        .unwrap();
    assert!(
        server.is_running().unwrap(),
        "native MCP server exited before the poller failure was injected"
    );
    corrupt_task_event_log(dir.path(), task_id);

    let output = server
        .wait(Duration::from_secs(4))
        .unwrap_or_else(|error| panic!("failed to wait for native MCP failure: {error}"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MCP notification event-log read failed"),
        "unexpected native MCP failure: {stderr}"
    );

    let stop = mcp_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "daemon stop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
}

#[test]
#[cfg(unix)]
fn test_mcp_native_stdio_accepts_newline_json() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());

    let mut server = start_mcp_server(dir.path());

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{}},"clientInfo":{"name":"claude-code","version":"2.1.72"}}
        }),
    );
    let initialize = read_mcp_message_newline(&mut server);
    assert_eq!(initialize["result"]["serverInfo"]["name"], "Packet28");
    assert_eq!(
        initialize["result"]["serverInfo"]["version"],
        workspace_packet28_version()
    );
    assert_eq!(initialize["result"]["protocolVersion"], "2025-03-26");
    assert!(initialize["result"]["capabilities"]["experimental"].is_null());

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/list"
        }),
    );
    let tools = read_mcp_message_newline(&mut server);
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_write_intention"));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_search"));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_read_regions"));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_glob"));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_fetch_tool_result"));
    assert!(!tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28_sync"));

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "params":{}
        }),
    );
    let missing_method = read_mcp_message_newline(&mut server);
    assert_eq!(missing_method["error"]["code"], -32600);

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"definitely/unknown"
        }),
    );
    let unknown_method = read_mcp_message_newline(&mut server);
    assert_eq!(unknown_method["error"]["code"], -32601);

    write_mcp_message_newline(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"tools/list"
        }),
    );
    let after_error = read_mcp_message_newline(&mut server);
    assert!(after_error["result"]["tools"].as_array().is_some());

    write_mcp_message_newline(
        &mut server,
        &json!([
            {
                "jsonrpc":"2.0",
                "id":6,
                "method":"tools/list"
            },
            {
                "jsonrpc":"2.0",
                "method":"notifications/initialized"
            },
            {
                "jsonrpc":"2.0",
                "id":7,
                "method":"definitely/unknown"
            }
        ]),
    );
    let batch = read_mcp_message_newline(&mut server);
    let batch = batch.as_array().expect("batch response");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0]["id"], 6);
    assert!(batch[0]["result"]["tools"].is_array());
    assert_eq!(batch[1]["id"], 7);
    assert_eq!(batch[1]["error"]["code"], -32601);

    write_mcp_message_newline(&mut server, &json!([]));
    let empty_batch = read_mcp_message_newline(&mut server);
    assert_eq!(empty_batch["id"], Value::Null);
    assert_eq!(empty_batch["error"]["code"], -32600);

    server
        .finish(MCP_SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("failed to stop newline MCP server: {error}"));
}

#[test]
#[cfg(unix)]
fn first_cold_mcp_tool_continues_recovered_task() {
    use packet28_daemon_protocol::paths::{task_event_log_path, TaskStorageId};
    use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry};
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let task_id = "cold-mcp-recovery";
    let mut registry = TaskRegistry::default();
    registry.tasks.insert(
        task_id.to_string(),
        TaskRecord {
            task_id: task_id.to_string(),
            last_event_seq: 40,
            ..TaskRecord::default()
        },
    );
    packet28_daemon_core::storage::save_task_registry(dir.path(), &registry).unwrap();
    let path = task_event_log_path(dir.path(), &TaskStorageId::try_from(task_id).unwrap());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"{damaged history}\n").unwrap();
    let mut server = start_mcp_server(dir.path());
    server.request_with_id(json!(1), "initialize", json!({
        "protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"test","version":"1"}
    }), MCP_IO_TIMEOUT).unwrap();
    let result = server.request_with_id(json!(2), "tools/call", json!({
        "name":"packet28.write_intention", "arguments":{"task_id":task_id,"text":"Continue after cold recovery"}
    }), MCP_IO_TIMEOUT).unwrap();
    assert_ne!(result["result"]["isError"], true, "{result}");
    assert!(result.get("error").is_none(), "{result}");
    let registry = packet28_daemon_core::storage::load_task_registry(dir.path()).unwrap();
    let successor = &registry.tasks[task_id]
        .superseded_by
        .as_ref()
        .unwrap()
        .successor_task_id;
    let events =
        packet28_daemon_core::storage::load_task_events_from_offset(dir.path(), successor, 0)
            .unwrap();
    assert!(!events.events.is_empty());
    assert_eq!(events.events[0].seq, 1);
    server.finish(MCP_SHUTDOWN_TIMEOUT).unwrap();
    let stop = mcp_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(stop.status.success());
}

#[test]
#[cfg(unix)]
fn idle_mcp_recovers_offline_corrupt_task_without_another_tool_call() {
    use packet28_daemon_protocol::paths::{ready_path, task_event_log_path, TaskStorageId};
    use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry};
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let task_id = "idle-recovery";
    let mut registry = TaskRegistry::default();
    registry.tasks.insert(
        task_id.to_string(),
        TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        },
    );
    packet28_daemon_core::storage::save_task_registry(dir.path(), &registry).unwrap();
    let mut server = start_mcp_server(dir.path());
    server.request_with_id(json!(1), "initialize", json!({
        "protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"test","version":"1"}
    }), MCP_IO_TIMEOUT).unwrap();
    let result = server
        .request_with_id(
            json!(2),
            "tools/call",
            json!({
                "name":"packet28.task_status", "arguments":{"task_id":task_id}
            }),
            MCP_IO_TIMEOUT,
        )
        .unwrap();
    assert_ne!(result["result"]["isError"], true, "{result}");
    // Keep the idle notification reader from restarting the daemon while the
    // fixture waits for shutdown and installs offline damage.
    let startup_lease =
        packet28_daemon_core::task_store_lease::acquire_daemon_startup_lease(dir.path()).unwrap();
    let stop = mcp_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(stop.status.success());
    // Stop acknowledges the request before runtime cleanup and lease release.
    let deadline = Instant::now() + MCP_SHUTDOWN_TIMEOUT;
    let stopped_lease = loop {
        match packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(dir.path()) {
            Ok(lease) => break lease,
            Err(packet28_daemon_core::DaemonCoreError::DaemonInstanceAlreadyRunning { .. }) => {
                assert!(Instant::now() < deadline, "fixture daemon did not stop");
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("fixture daemon authority check failed: {error}"),
        }
    };
    assert!(!ready_path(dir.path()).exists());
    let path = task_event_log_path(dir.path(), &TaskStorageId::try_from(task_id).unwrap());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"{damaged history}\n").unwrap();
    drop(stopped_lease);
    drop(startup_lease);
    // No new MCP tool, CLI status, or explicit restart. The notification
    // reader encounters the old checkpoint and waits for recovery readiness.
    let receipt = server.receive(Duration::from_secs(15)).unwrap();
    assert_eq!(
        receipt["method"], "notifications/packet28.task_recovered",
        "{receipt}"
    );
    let successor = receipt["params"]["successor_task_id"].as_str().unwrap();
    assert_eq!(successor, "idle-recovery-recovered-1");
    let write = server
        .request_with_id(
            json!(3),
            "tools/call",
            json!({
                "name":"packet28.write_intention", "arguments":{
                    "task_id":successor, "text":"Continue under the recovered identity"
                }
            }),
            MCP_IO_TIMEOUT,
        )
        .unwrap();
    assert_ne!(write["result"]["isError"], true, "{write}");
    let update = server.receive(MCP_IO_TIMEOUT).unwrap();
    assert_eq!(update["method"], "notifications/packet28.context_updated");
    assert_eq!(update["params"]["task_id"], successor);
    assert_eq!(update["params"]["event_seq"], 1);
    server.finish(MCP_SHUTDOWN_TIMEOUT).unwrap();
    let stop = mcp_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(stop.status.success());
}
