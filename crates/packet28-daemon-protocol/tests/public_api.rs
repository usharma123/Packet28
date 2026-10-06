use std::collections::BTreeSet;
use std::io::Cursor;

use packet28_daemon_protocol::broker::BrokerGetContextRequest;
use packet28_daemon_protocol::commands::WatchSpec;
use packet28_daemon_protocol::context_store::ContextRecallRequest;
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::hooks::HookIngestRequest;
use packet28_daemon_protocol::index::DaemonIndexStatusRequest;
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse, DaemonStatus};
use packet28_daemon_protocol::paths::{daemon_dir, socket_path};
use packet28_daemon_protocol::registry::{
    DaemonRegistryRequestV1, DaemonRegistryResponseV1, DaemonStatusV1, OversizedTaskRecordV1,
    RegistryRevisionV1, TaskListPageV1,
};
use packet28_daemon_protocol::task::TaskAwaitHandoffRequest;

const REVIEWED_MODULES: &[(&str, &str)] = &[
    ("broker", "wire DTO and JSON compatibility tests"),
    ("commands", "command dispatch and JSON compatibility tests"),
    ("context_store", "context-store process and JSON tests"),
    ("frame", "runnable bounded-framing example"),
    ("hooks", "hook-ingest JSON compatibility tests"),
    ("index", "index state and process tests"),
    ("message", "frozen request/response compatibility tests"),
    ("paths", "deterministic endpoint and confinement tests"),
    ("process", "session-detach child-process tests"),
    (
        "registry",
        "runnable additive migration example and JSON tests",
    ),
    ("task", "runnable lifecycle and compile-fail examples"),
];
const ROOT_COMPATIBILITY_EXPORTS: &[&str] = &["message::{DaemonRequest,DaemonResponse}"];

fn frozen_request_match(request: DaemonRequest) -> &'static str {
    match request {
        DaemonRequest::Execute { .. } => "execute",
        DaemonRequest::ExecuteSequence { .. } => "execute_sequence",
        DaemonRequest::Status => "status",
        DaemonRequest::Stop => "stop",
        DaemonRequest::TaskStatus { .. } => "task_status",
        DaemonRequest::TaskAwaitHandoff { .. } => "task_await_handoff",
        DaemonRequest::TaskMarkHandoffConsumed { .. } => "task_mark_handoff_consumed",
        DaemonRequest::TaskLaunchAgent { .. } => "task_launch_agent",
        DaemonRequest::TaskCancel { .. } => "task_cancel",
        DaemonRequest::TaskSubscribe { .. } => "task_subscribe",
        DaemonRequest::WatchList { .. } => "watch_list",
        DaemonRequest::WatchRemove { .. } => "watch_remove",
        DaemonRequest::CoverCheck { .. } => "cover_check",
        DaemonRequest::PacketFetch { .. } => "packet_fetch",
        DaemonRequest::TestShard { .. } => "test_shard",
        DaemonRequest::TestMap { .. } => "test_map",
        DaemonRequest::ContextStoreList { .. } => "context_store_list",
        DaemonRequest::ContextStoreGet { .. } => "context_store_get",
        DaemonRequest::ContextStorePrune { .. } => "context_store_prune",
        DaemonRequest::ContextStoreStats { .. } => "context_store_stats",
        DaemonRequest::ContextRecall { .. } => "context_recall",
        DaemonRequest::BrokerGetContext { .. } => "broker_get_context",
        DaemonRequest::BrokerEstimateContext { .. } => "broker_estimate_context",
        DaemonRequest::BrokerPrepareHandoff { .. } => "broker_prepare_handoff",
        DaemonRequest::BrokerValidatePlan { .. } => "broker_validate_plan",
        DaemonRequest::BrokerDecompose { .. } => "broker_decompose",
        DaemonRequest::BrokerWriteState { .. } => "broker_write_state",
        DaemonRequest::BrokerWriteStateBatch { .. } => "broker_write_state_batch",
        DaemonRequest::BrokerTaskStatus { .. } => "broker_task_status",
        DaemonRequest::ContextResolve { .. } => "context_resolve",
        DaemonRequest::InstructionFileResolve { .. } => "instruction_file_resolve",
        DaemonRequest::HookIngest { .. } => "hook_ingest",
        DaemonRequest::Packet28Search { .. } => "packet28_search",
        DaemonRequest::Packet28SearchGuard { .. } => "packet28_search_guard",
        DaemonRequest::DaemonIndexStatus { .. } => "daemon_index_status",
        DaemonRequest::DaemonIndexRebuild { .. } => "daemon_index_rebuild",
        DaemonRequest::DaemonIndexClear { .. } => "daemon_index_clear",
    }
}

fn frozen_response_match(response: DaemonResponse) -> &'static str {
    match response {
        DaemonResponse::Execute { .. } => "execute",
        DaemonResponse::ExecuteSequence { .. } => "execute_sequence",
        DaemonResponse::Status { .. } => "status",
        DaemonResponse::Ack { .. } => "ack",
        DaemonResponse::TaskStatus { .. } => "task_status",
        DaemonResponse::TaskAwaitHandoff { .. } => "task_await_handoff",
        DaemonResponse::TaskMarkHandoffConsumed { .. } => "task_mark_handoff_consumed",
        DaemonResponse::TaskLaunchAgent { .. } => "task_launch_agent",
        DaemonResponse::TaskCancel { .. } => "task_cancel",
        DaemonResponse::TaskSubscribeAck { .. } => "task_subscribe_ack",
        DaemonResponse::WatchList { .. } => "watch_list",
        DaemonResponse::WatchRemove { .. } => "watch_remove",
        DaemonResponse::CoverCheck { .. } => "cover_check",
        DaemonResponse::PacketFetch { .. } => "packet_fetch",
        DaemonResponse::TestShard { .. } => "test_shard",
        DaemonResponse::TestMap { .. } => "test_map",
        DaemonResponse::ContextStoreList { .. } => "context_store_list",
        DaemonResponse::ContextStoreGet { .. } => "context_store_get",
        DaemonResponse::ContextStorePrune { .. } => "context_store_prune",
        DaemonResponse::ContextStoreStats { .. } => "context_store_stats",
        DaemonResponse::ContextRecall { .. } => "context_recall",
        DaemonResponse::BrokerGetContext { .. } => "broker_get_context",
        DaemonResponse::BrokerEstimateContext { .. } => "broker_estimate_context",
        DaemonResponse::BrokerPrepareHandoff { .. } => "broker_prepare_handoff",
        DaemonResponse::BrokerValidatePlan { .. } => "broker_validate_plan",
        DaemonResponse::BrokerDecompose { .. } => "broker_decompose",
        DaemonResponse::BrokerWriteState { .. } => "broker_write_state",
        DaemonResponse::BrokerWriteStateBatch { .. } => "broker_write_state_batch",
        DaemonResponse::BrokerTaskStatus { .. } => "broker_task_status",
        DaemonResponse::ContextResolve { .. } => "context_resolve",
        DaemonResponse::InstructionFileResolve { .. } => "instruction_file_resolve",
        DaemonResponse::HookIngest { .. } => "hook_ingest",
        DaemonResponse::Packet28Search { .. } => "packet28_search",
        DaemonResponse::Packet28SearchGuard { .. } => "packet28_search_guard",
        DaemonResponse::DaemonIndexStatus { .. } => "daemon_index_status",
        DaemonResponse::DaemonIndexRebuild { .. } => "daemon_index_rebuild",
        DaemonResponse::DaemonIndexClear { .. } => "daemon_index_clear",
        DaemonResponse::Error { .. } => "error",
    }
}

#[test]
fn documented_modules_form_a_complete_client_surface() {
    let _watch = WatchSpec::default();
    let _recall = ContextRecallRequest::default();
    let _broker = BrokerGetContextRequest::default();
    let _hook = HookIngestRequest::default();
    let _index = DaemonIndexStatusRequest::default();
    let _await_handoff = TaskAwaitHandoffRequest::default();

    let root = std::path::Path::new("/workspace");
    assert!(daemon_dir(root).ends_with(".packet28/daemon"));
    assert_eq!(
        socket_path(root)
            .extension()
            .and_then(|value| value.to_str()),
        Some("sock")
    );

    let mut bytes = Vec::new();
    write_frame(&mut bytes, &DaemonRequest::Status).unwrap();
    let request: DaemonRequest = read_frame(&mut Cursor::new(bytes)).unwrap();
    assert!(matches!(request, DaemonRequest::Status));

    let response = DaemonResponse::Ack {
        message: "ready".to_string(),
    };
    assert_eq!(
        serde_json::to_value(response).unwrap()["type"],
        serde_json::json!("ack")
    );
}

#[test]
fn legacy_messages_remain_source_and_wire_compatible() {
    assert_eq!(frozen_request_match(DaemonRequest::Status), "status");
    assert_eq!(
        frozen_response_match(DaemonResponse::Ack {
            message: "ready".to_string(),
        }),
        "ack"
    );

    let status = DaemonStatus {
        pid: 7,
        version: "0.2".to_string(),
        socket_path: "/tmp/packet28.sock".to_string(),
        workspace_root: "/workspace".to_string(),
        started_at_unix: 11,
        ready_at_unix: Some(12),
        log_path: "/tmp/packet28.log".to_string(),
        uptime_secs: 13,
        tasks: vec![packet28_daemon_protocol::task::TaskRecord {
            task_id: "legacy-task".to_string(),
            ..packet28_daemon_protocol::task::TaskRecord::default()
        }],
        watches: vec![packet28_daemon_protocol::task::WatchRegistration {
            watch_id: "legacy-watch".to_string(),
            ..packet28_daemon_protocol::task::WatchRegistration::default()
        }],
        index: None,
    };
    let wire = serde_json::to_value(DaemonResponse::Status { status }).unwrap();
    assert_eq!(wire["type"], serde_json::json!("status"));
    assert!(wire["status"].get("task_count").is_none());
    assert_eq!(wire["status"]["tasks"].as_array().unwrap().len(), 1);

    let decoded: DaemonResponse = serde_json::from_value(wire).unwrap();
    let decoded_status = match decoded {
        DaemonResponse::Status { status } => status,
        other => panic!("unexpected legacy status response: {other:?}"),
    };
    let normalized = DaemonStatusV1::from_legacy(decoded_status);
    assert_eq!(normalized.task_count, 1);
    assert_eq!(normalized.watch_count, 1);
    assert_eq!(normalized.registry_revision, None);
}

#[test]
fn registry_v1_uses_separate_versioned_wire_tags() {
    let request: DaemonRegistryRequestV1 =
        serde_json::from_value(serde_json::json!({ "type": "registry_status_v1" })).unwrap();
    assert!(matches!(request, DaemonRegistryRequestV1::Status));
    assert!(serde_json::from_value::<DaemonRequest>(
        serde_json::json!({ "type": "registry_status_v1" })
    )
    .is_err());
    assert!(matches!(
        serde_json::from_value::<DaemonRegistryResponseV1>(serde_json::json!({
            "type": "error",
            "message": "unknown variant `registry_status_v1`, expected one of `status`, `stop`"
        }))
        .unwrap(),
        DaemonRegistryResponseV1::Error { message }
            if message.contains("unknown variant")
    ));

    let response = DaemonRegistryResponseV1::Status {
        status: Box::new(DaemonStatusV1 {
            task_count: 17,
            watch_count: 3,
            registry_revision: Some(RegistryRevisionV1 {
                instance_id: "daemon-instance".to_string(),
                revision: 42,
            }),
            ..DaemonStatusV1::default()
        }),
    };
    let wire = serde_json::to_value(response).unwrap();
    assert_eq!(wire["type"], serde_json::json!("registry_status_v1"));
    assert_eq!(wire["status"]["task_count"], serde_json::json!(17));
    assert_eq!(
        wire["status"]["registry_revision"],
        serde_json::json!({
            "instance_id": "daemon-instance",
            "revision": 42
        })
    );
}

#[test]
fn task_page_omissions_are_additive_and_round_trip_on_the_wire() {
    let legacy_page = serde_json::json!({
        "snapshot_revision": {
            "instance_id": "daemon-instance",
            "revision": 42
        },
        "tasks": [],
        "total": 1
    });
    let decoded: TaskListPageV1 = serde_json::from_value(legacy_page).unwrap();
    assert!(decoded.omitted_oversized.is_empty());

    let empty_wire = serde_json::to_value(&decoded).unwrap();
    assert!(empty_wire.get("omitted_oversized").is_none());

    let page = TaskListPageV1 {
        omitted_oversized: vec![
            OversizedTaskRecordV1 {
                task_id: "task-a".to_string(),
                encoded_bytes: 1_048_577,
            },
            OversizedTaskRecordV1 {
                task_id: "task-b".to_string(),
                encoded_bytes: 2_097_152,
            },
        ],
        ..decoded
    };
    let wire = serde_json::to_value(&page).unwrap();
    assert_eq!(
        wire["omitted_oversized"],
        serde_json::json!([
            {"task_id": "task-a", "encoded_bytes": 1_048_577},
            {"task_id": "task-b", "encoded_bytes": 2_097_152}
        ])
    );
    let round_trip: TaskListPageV1 = serde_json::from_value(wire).unwrap();
    assert_eq!(round_trip.omitted_oversized, page.omitted_oversized);
}

#[test]
fn every_public_module_has_a_reviewed_classification() {
    let source = include_str!("../src/lib.rs");
    let actual = source
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("pub mod ")
                .and_then(|tail| tail.strip_suffix(';'))
                .map(ToOwned::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let expected = REVIEWED_MODULES
        .iter()
        .map(|(module, reason)| {
            assert!(
                !reason.trim().is_empty(),
                "reviewed module '{module}' needs a coverage reason"
            );
            (*module).to_owned()
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(
        actual, expected,
        "protocol root changed without updating its reviewed documentation inventory"
    );
}

#[test]
fn root_compatibility_exports_remain_an_explicit_allowlist() {
    let source = include_str!("../src/lib.rs");
    let actual = root_reexports(source);
    let expected = ROOT_COMPATIBILITY_EXPORTS
        .iter()
        .map(|export| (*export).to_owned())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        actual, expected,
        "protocol root compatibility exports changed without an inventory update"
    );
}

fn root_reexports(source: &str) -> BTreeSet<String> {
    let mut exports = BTreeSet::new();
    let mut current = None::<String>;

    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(statement) = current.as_mut() {
            statement.push_str(trimmed);
            if trimmed.ends_with(';') {
                let completed = current.take().expect("active public use statement");
                exports.insert(normalize_export(&completed));
            }
        } else if let Some(tail) = trimmed.strip_prefix("pub use ") {
            if tail.ends_with(';') {
                exports.insert(normalize_export(tail));
            } else {
                current = Some(tail.to_owned());
            }
        }
    }

    assert!(current.is_none(), "unterminated root public use statement");
    exports
}

fn normalize_export(statement: &str) -> String {
    let normalized = statement
        .trim_end_matches(';')
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    normalized.replace(",}", "}")
}
