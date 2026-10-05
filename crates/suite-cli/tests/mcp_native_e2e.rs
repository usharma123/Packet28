#[path = "support/mcp_native.rs"]
mod mcp_native;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use mcp_native::{
    ensure_packet28d_built, init_repo, initialize_mcp_session, read_mcp_message_for_id,
    start_mcp_server, stop_mcp_server, suite_cmd, write_mcp_message, write_repo_fixture,
};
use process_harness::McpHarness;
use serde_json::{json, Value};
use tempfile::TempDir;

#[test]
#[cfg(unix)]
fn test_mcp_resources_list_paginates_past_oversized_records() {
    use packet28_daemon_core::storage::save_task_registry;
    use packet28_daemon_protocol::registry::MAX_REGISTRY_PAGE_ITEM_BYTES;
    use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry};
    use process_harness::{HarnessLimits, ProcessHarness};
    use std::process::Command;
    use std::time::{Duration, Instant};

    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    init_repo(dir.path());
    let tasks = [
        ('a', 900_000),
        ('b', 900_000),
        ('c', MAX_REGISTRY_PAGE_ITEM_BYTES + 1),
        ('d', 900_000),
    ]
    .into_iter()
    .map(|(id, bytes)| {
        let task_id = id.to_string();
        (
            task_id.clone(),
            TaskRecord {
                task_id,
                last_error: Some("x".repeat(bytes)),
                ..TaskRecord::default()
            },
        )
    })
    .collect();
    save_task_registry(dir.path(), &TaskRegistry { tasks }).unwrap();
    let daemon_binary = std::path::Path::new(env!("CARGO_BIN_EXE_Packet28"))
        .parent()
        .unwrap()
        .join("packet28d");
    let mut daemon_command = Command::new(daemon_binary);
    daemon_command
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args(["serve", "--root", dir.path().to_str().unwrap()]);
    let mut daemon = ProcessHarness::spawn(&mut daemon_command, HarnessLimits::default()).unwrap();
    let ready = dir.path().join(".packet28/daemon/ready");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon did not become ready: {:?}",
            daemon.diagnostics()
        );
        std::thread::yield_now();
    }

    let mut mcp_command = Command::new(env!("CARGO_BIN_EXE_Packet28"));
    mcp_command
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args(["mcp", "serve", "--root", dir.path().to_str().unwrap()]);
    let mut server = McpHarness::spawn(&mut mcp_command, HarnessLimits::default()).unwrap();
    initialize_mcp_session(&mut server);
    let response = server
        .request_with_id(
            json!(2),
            "resources/list",
            json!({}),
            Duration::from_secs(10),
        )
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let resources = response["result"]["resources"].as_array().unwrap();
    let uris = resources
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        uris,
        vec![
            "packet28://current/task",
            "packet28://current/brief",
            "packet28://task/a/brief",
            "packet28://task/b/brief"
        ]
    );
    assert_eq!(resources[0]["description"], "Current task metadata for d");
    stop_mcp_server(server);
    suite_cmd()
        .env("HOME", home.path())
        .timeout(Duration::from_secs(15))
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(daemon
        .wait(Duration::from_secs(10))
        .unwrap()
        .status
        .success());
}

fn write_intention_via_mcp(
    server: &mut McpHarness,
    id: u64,
    task_id: &str,
    text: &str,
    step_id: &str,
    paths: &[&str],
) -> Value {
    write_mcp_message(
        server,
        &json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_intention",
                "arguments":{
                    "task_id":task_id,
                    "text":text,
                    "step_id":step_id,
                    "paths":paths,
                }
            }
        }),
    );
    read_mcp_message_for_id(server, id)
}

#[test]
#[cfg(unix)]
fn test_mcp_native_write_intention_derives_task_id_from_full_text() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let mut server = start_mcp_server(dir.path());
    initialize_mcp_session(&mut server);

    let intention_text = "Investigate parser regression in the handoff pipeline";
    let derived_task_id = suite_cli::broker_client::derive_task_id(intention_text);
    let response = write_intention_via_mcp(
        &mut server,
        2,
        "",
        intention_text,
        "investigating",
        &["crates/packet28d/src/hooks.rs"],
    );
    assert_eq!(response["result"]["structuredContent"]["accepted"], true);

    write_mcp_message(
        &mut server,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.task_status",
                "arguments":{
                    "task_id": derived_task_id
                }
            }
        }),
    );
    let status = read_mcp_message_for_id(&mut server, 3);
    assert_eq!(
        status["result"]["structuredContent"]["task"]["task_id"],
        derived_task_id
    );

    stop_mcp_server(server);

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}
