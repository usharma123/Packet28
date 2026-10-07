#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared MCP fixture"
)]
#[path = "support/mcp_native.rs"]
mod mcp_native;
#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use std::collections::BTreeMap;

use mcp_native::{
    ensure_packet28d_built, init_repo, initialize_mcp_session, read_mcp_message_for_id,
    start_mcp_server, stop_mcp_server, suite_cmd, write_mcp_message,
};
use packet28_daemon_core::storage::save_task_watch_registry_checkpoint;
use packet28_daemon_protocol::registry::MAX_REGISTRY_PAGE_ITEM_BYTES;
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistry};
use serde_json::json;

#[test]
#[cfg(unix)]
fn resources_list_accepts_task_pages_with_reported_oversized_omissions() {
    ensure_packet28d_built();
    let workspace = tempfile::tempdir().expect("temporary workspace");
    init_repo(workspace.path());
    let tasks = TaskRegistry {
        tasks: BTreeMap::from([
            (
                "task-healthy".to_string(),
                TaskRecord {
                    task_id: "task-healthy".to_string(),
                    ..TaskRecord::default()
                },
            ),
            (
                "task-oversized".to_string(),
                TaskRecord {
                    task_id: "task-oversized".to_string(),
                    last_error: Some("x".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES)),
                    ..TaskRecord::default()
                },
            ),
        ]),
    };
    save_task_watch_registry_checkpoint(workspace.path(), &tasks, &WatchRegistry::default())
        .expect("seed oversized task/watch checkpoint");

    let mut server = start_mcp_server(workspace.path());
    initialize_mcp_session(&mut server);
    write_mcp_message(
        &mut server,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "resources/list",
            "params": {}
        }),
    );
    let response = read_mcp_message_for_id(&mut server, 2);
    assert!(response.get("error").is_none(), "{response}");
    let resources = response["result"]["resources"]
        .as_array()
        .expect("resources/list must return an array");
    assert!(resources.iter().any(|resource| {
        resource["uri"] == "packet28://current/task"
            && resource["description"] == "Current task metadata for task-healthy"
    }));
    assert!(resources.iter().all(|resource| {
        !resource["uri"]
            .as_str()
            .is_some_and(|uri| uri.contains("task-oversized"))
    }));

    stop_mcp_server(server);
    suite_cmd()
        .args([
            "daemon",
            "stop",
            "--root",
            workspace.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}
