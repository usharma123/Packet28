//! Fresh and concurrent MCP processes for one task draw distinct invocation
//! identities, so they do not reissue a handle, and existing persistent tool
//! evidence keeps its exact bytes.

#[path = "support/mcp_native.rs"]
mod mcp_native;
#[expect(
    dead_code,
    reason = "this integration binary uses one fake upstream from the shared harness"
)]
#[path = "support/mcp_proxy_fake.rs"]
mod mcp_proxy_fake;
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
use mcp_proxy_fake::write_compact_read_server;
use packet28_daemon_protocol::context_store::{ContextStoreGetRequest, ContextStoreListRequest};
use packet28_daemon_protocol::paths::{task_artifact_dir, TaskStorageId};
use process_harness::{HarnessLimits, McpHarness};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const TASK_ID: &str = "same-task-across-mcp-processes";

fn send_tool_call(server: &mut McpHarness, id: u64, name: &str, arguments: Value) {
    write_mcp_message(
        server,
        &json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }),
    );
}

fn successful_payload(response: &Value) -> Value {
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != true,
        "tool call failed: {response:#}"
    );
    response["result"]["structuredContent"].clone()
}

fn search(server: &mut McpHarness, id: u64) -> String {
    send_tool_call(server, id, "packet28.search", search_arguments());
    artifact_id(&successful_payload(&read_mcp_message_for_id(server, id)))
}

fn search_arguments() -> Value {
    json!({"task_id":TASK_ID,"query":"Alpha","fixed_string":true,"response_mode":"full"})
}

fn artifact_id(payload: &Value) -> String {
    payload["artifact_id"]
        .as_str()
        .unwrap_or_else(|| panic!("payload has no artifact_id: {payload:#}"))
        .to_owned()
}

fn fetch(server: &mut McpHarness, id: u64, artifact_id: &str) -> Value {
    send_tool_call(
        server,
        id,
        "packet28.fetch_tool_result",
        json!({"task_id":TASK_ID,"artifact_id":artifact_id}),
    );
    successful_payload(&read_mcp_message_for_id(server, id))
}

fn evidence_dir(root: &Path) -> PathBuf {
    task_artifact_dir(root, &TaskStorageId::try_from(TASK_ID).unwrap()).join("tool-evidence")
}

fn evidence_path(root: &Path, artifact_id: &str) -> PathBuf {
    evidence_dir(root).join(artifact_id)
}

fn evidence_files(root: &Path) -> BTreeSet<String> {
    fs::read_dir(evidence_dir(root))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn load_agent_state_events(root: &Path) -> Vec<Value> {
    let root_string = root.to_string_lossy().into_owned();
    suite_cli::cmd_daemon::execute_context_store_list(
        root,
        ContextStoreListRequest {
            root: root_string.clone(),
            target: Some("agenty.state.write".to_string()),
            limit: 200,
            ..ContextStoreListRequest::default()
        },
    )
    .unwrap()
    .entries
    .into_iter()
    .filter_map(|entry| {
        suite_cli::cmd_daemon::execute_context_store_get(
            root,
            ContextStoreGetRequest {
                root: root_string.clone(),
                key: entry.cache_key,
            },
        )
        .unwrap()
        .entry
    })
    .flat_map(|detail| detail.entry.packets)
    .filter_map(|packet| packet.body["payload"].as_object().cloned())
    .map(Value::Object)
    .filter(|event| event["task_id"] == TASK_ID)
    .collect()
}

/// Every invocation has exactly one start and one terminal event, both with
/// the same sequence, and no identity is shared between invocations.
fn assert_one_lifecycle_per_invocation(events: &[Value], invocation_ids: &BTreeSet<String>) {
    let lifecycle = |kind: &str, invocation_id: &str| {
        events
            .iter()
            .filter(|event| {
                event["data"]["type"] == kind && event["data"]["invocation_id"] == invocation_id
            })
            .collect::<Vec<_>>()
    };
    let observed = events
        .iter()
        .filter(|event| event["data"]["type"] == "tool_invocation_started")
        .map(|event| event["data"]["invocation_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        observed.len(),
        observed.iter().collect::<BTreeSet<_>>().len(),
        "an invocation identity was reissued: {observed:?}"
    );
    for invocation_id in invocation_ids {
        let started = lifecycle("tool_invocation_started", invocation_id);
        let completed = lifecycle("tool_invocation_completed", invocation_id);
        assert_eq!(started.len(), 1, "{invocation_id}: {events:#?}");
        assert_eq!(completed.len(), 1, "{invocation_id}: {events:#?}");
        assert_eq!(
            started[0]["data"]["sequence"],
            completed[0]["data"]["sequence"]
        );
        assert!(started[0]["data"]["sequence"].as_u64().unwrap() >= 1);
    }
}

/// Stops the fixture daemon even when an assertion unwinds the test.
struct DaemonGuard<'a>(&'a Path);

impl DaemonGuard<'_> {
    fn stop(self) {
        suite_cmd()
            .timeout(Duration::from_secs(15))
            .args(["daemon", "stop", "--root", self.0.to_str().unwrap()])
            .assert()
            .success();
        std::mem::forget(self);
    }
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = suite_cmd()
            .timeout(Duration::from_secs(15))
            .args(["daemon", "stop", "--root", self.0.to_str().unwrap()])
            .output();
    }
}

#[test]
#[cfg(unix)]
fn test_same_task_tools_succeed_in_a_fresh_mcp_process_without_replacing_evidence() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let daemon = DaemonGuard(dir.path());

    let mut first = start_mcp_server(dir.path());
    initialize_mcp_session(&mut first);
    let first_artifact = search(&mut first, 2);
    stop_mcp_server(first);
    let first_bytes = fs::read(evidence_path(dir.path(), &first_artifact)).unwrap();

    // A fresh process for the same task is the restart path that used to
    // reissue `tool-invocation-1` and fail on the existing evidence file.
    let mut second = start_mcp_server(dir.path());
    initialize_mcp_session(&mut second);
    let second_artifact = search(&mut second, 2);
    send_tool_call(
        &mut second,
        3,
        "packet28.read_regions",
        json!({
            "task_id":TASK_ID,
            "path":"src/alpha.rs",
            "line_start":1,
            "line_end":2,
            "response_mode":"full"
        }),
    );
    let read_artifact = artifact_id(&successful_payload(&read_mcp_message_for_id(
        &mut second,
        3,
    )));

    let artifacts = BTreeSet::from([
        first_artifact.clone(),
        second_artifact.clone(),
        read_artifact.clone(),
    ]);
    assert_eq!(artifacts.len(), 3, "{artifacts:?}");
    assert_eq!(evidence_files(dir.path()), artifacts);
    assert_eq!(
        fs::read(evidence_path(dir.path(), &first_artifact)).unwrap(),
        first_bytes,
        "the first process's evidence must keep its exact bytes"
    );

    let first_full = fetch(&mut second, 4, &first_artifact);
    let second_full = fetch(&mut second, 5, &second_artifact);
    let read_full = fetch(&mut second, 6, &read_artifact);
    assert_eq!(first_full["query"], "Alpha");
    assert_eq!(second_full["query"], "Alpha");
    assert_eq!(read_full["path"], "src/alpha.rs");
    let invocation_ids = [&first_full, &second_full, &read_full]
        .iter()
        .map(|payload| payload["invocation_id"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(invocation_ids.len(), 3, "{invocation_ids:?}");
    // Sequences remain per-session ordinals and restart in each process.
    assert_eq!(first_full["sequence"], 1);
    assert_eq!(second_full["sequence"], 1);
    assert_eq!(read_full["sequence"], 2);
    for (payload, artifact) in [
        (&first_full, &first_artifact),
        (&second_full, &second_artifact),
        (&read_full, &read_artifact),
    ] {
        assert_eq!(
            format!("{}-result.json", payload["invocation_id"].as_str().unwrap()),
            *artifact
        );
        // The invocation handle form resolves to the same artifact.
        send_tool_call(
            &mut second,
            7,
            "packet28.fetch_tool_result",
            json!({"task_id":TASK_ID,"invocation_id":payload["invocation_id"]}),
        );
        assert_eq!(
            successful_payload(&read_mcp_message_for_id(&mut second, 7)),
            *payload
        );
    }

    assert_one_lifecycle_per_invocation(&load_agent_state_events(dir.path()), &invocation_ids);
    stop_mcp_server(second);
    daemon.stop();
}

#[test]
#[cfg(unix)]
fn test_concurrent_mcp_sessions_for_one_task_issue_distinct_invocations() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let daemon = DaemonGuard(dir.path());

    let mut left = start_mcp_server(dir.path());
    initialize_mcp_session(&mut left);
    let mut right = start_mcp_server(dir.path());
    initialize_mcp_session(&mut right);

    let mut artifacts = Vec::new();
    for round in 0..2_u64 {
        let id = 2 + round;
        // Both sessions have the request in flight before either is read.
        send_tool_call(&mut left, id, "packet28.search", search_arguments());
        send_tool_call(&mut right, id, "packet28.search", search_arguments());
        artifacts.push(artifact_id(&successful_payload(&read_mcp_message_for_id(
            &mut left, id,
        ))));
        artifacts.push(artifact_id(&successful_payload(&read_mcp_message_for_id(
            &mut right, id,
        ))));
    }
    let unique = artifacts.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), 4, "{artifacts:?}");
    assert_eq!(evidence_files(dir.path()), unique);

    let mut invocation_ids = BTreeSet::new();
    for (offset, artifact) in artifacts.iter().enumerate() {
        let payload = fetch(&mut left, 10 + offset as u64, artifact);
        assert_eq!(payload["query"], "Alpha");
        invocation_ids.insert(payload["invocation_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(invocation_ids.len(), 4, "{invocation_ids:?}");
    assert_one_lifecycle_per_invocation(&load_agent_state_events(dir.path()), &invocation_ids);

    stop_mcp_server(right);
    stop_mcp_server(left);
    daemon.stop();
}

fn start_mcp_proxy(root: &Path, config_path: &Path) -> McpHarness {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command.current_dir(root).args([
        "mcp",
        "proxy",
        "--root",
        root.to_str().unwrap(),
        "--upstream-config",
        config_path.to_str().unwrap(),
        "--task-id",
        TASK_ID,
    ]);
    let mut server = McpHarness::spawn(&mut command, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("failed to start MCP proxy: {error}"));
    initialize_mcp_session(&mut server);
    server
}

fn proxy_compact_read(server: &mut McpHarness, id: u64) -> String {
    send_tool_call(server, id, "compact.read", json!({}));
    let payload = successful_payload(&read_mcp_message_for_id(server, id));
    assert_eq!(payload["original_tool"], "compact.read");
    artifact_id(&payload)
}

#[test]
#[cfg(unix)]
fn test_same_task_proxy_evidence_survives_a_fresh_proxy_process() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let daemon = DaemonGuard(dir.path());
    let script_path = dir.path().join("compact_mcp.py");
    write_compact_read_server(&script_path);
    let config_path = dir.path().join(".mcp.proxy.json");
    fs::write(
        &config_path,
        json!({
            "mcpServers": {
                "compact": {
                    "command": "python3",
                    "args": ["-u", script_path.to_str().unwrap()],
                    "framing": "content_length",
                    "compact_tools": ["compact.read"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let mut first = start_mcp_proxy(dir.path(), &config_path);
    let first_artifact = proxy_compact_read(&mut first, 2);
    stop_mcp_server(first);
    let first_bytes = fs::read(evidence_path(dir.path(), &first_artifact)).unwrap();

    let mut second = start_mcp_proxy(dir.path(), &config_path);
    let second_artifact = proxy_compact_read(&mut second, 2);
    assert_ne!(first_artifact, second_artifact);
    assert_eq!(
        evidence_files(dir.path()),
        BTreeSet::from([first_artifact.clone(), second_artifact.clone()])
    );
    assert_eq!(
        fs::read(evidence_path(dir.path(), &first_artifact)).unwrap(),
        first_bytes
    );
    for (id, artifact) in [(3, &first_artifact), (4, &second_artifact)] {
        let payload = fetch(&mut second, id, artifact);
        assert_eq!(payload["structuredContent"]["path"], "src/alpha.rs");
    }

    let events = load_agent_state_events(dir.path());
    let invocation_ids = events
        .iter()
        .filter(|event| event["data"]["type"] == "tool_invocation_completed")
        .map(|event| event["data"]["invocation_id"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        invocation_ids
            .iter()
            .map(|id| format!("{id}-result.json"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first_artifact, second_artifact])
    );
    assert_one_lifecycle_per_invocation(&events, &invocation_ids);

    stop_mcp_server(second);
    daemon.stop();
}
