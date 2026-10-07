//! Process-level coverage for online, evidence-preserving task-record archival.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use packet28_daemon_core::storage::record_archive::{
    encode_task_record_compact, read_task_record_archive, task_record_archive_digest,
};
use packet28_daemon_core::storage::{
    append_next_task_event, ensure_daemon_dir, save_active_task_record,
    save_task_watch_registry_checkpoint,
};
use packet28_daemon_protocol::broker::{BrokerWriteOp, BrokerWriteStateRequest};
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::hooks::ActiveTaskRecord;
use packet28_daemon_protocol::message::{
    DaemonEvent, DaemonRequest, DaemonResponse, DaemonRuntimeInfo,
};
use packet28_daemon_protocol::paths::{
    log_path, ready_path, runtime_path, task_artifact_dir, task_event_log_path, task_registry_path,
    watch_registry_path, TaskStorageId,
};
use packet28_daemon_protocol::registry::{
    DaemonRegistryRequestV1, DaemonRegistryResponseV1, TaskListPageRequestV1,
    TaskRecordArchiveOutcome, TaskRecordArchiveReportV1, TaskRecordArchiveRequestV1,
    TaskRecordSizeLevel, MAX_REGISTRY_PAGE_ITEM_BYTES,
};
use packet28_daemon_protocol::task::{
    TaskHistoryRecovery, TaskRecord, TaskRegistry, WatchRegistry,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

const BIG: &str = "task-big";
const GOOD: &str = "task-good";

struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(root: &Path, exit_after: Option<&str>) -> (DaemonChild, DaemonRuntimeInfo) {
    spawn_with(root, exit_after, None)
}

fn spawn_with(
    root: &Path,
    exit_after: Option<&str>,
    pause_after_publication: Option<&Path>,
) -> (DaemonChild, DaemonRuntimeInfo) {
    let _ = std::fs::remove_file(ready_path(root));
    let mut command = Command::new(env!("CARGO_BIN_EXE_packet28d"));
    match pause_after_publication {
        Some(release) => command
            .env(
                "PACKET28_TASK_RECORD_ARCHIVE_PAUSE_AFTER",
                "archive_published",
            )
            .env("PACKET28_TASK_RECORD_ARCHIVE_PAUSE_FILE", release),
        None => command
            .env_remove("PACKET28_TASK_RECORD_ARCHIVE_PAUSE_AFTER")
            .env_remove("PACKET28_TASK_RECORD_ARCHIVE_PAUSE_FILE"),
    };
    command
        .args(["serve", "--root"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match exit_after {
        Some(phase) => command.env("PACKET28_TASK_RECORD_ARCHIVE_EXIT_AFTER", phase),
        None => command.env_remove("PACKET28_TASK_RECORD_ARCHIVE_EXIT_AFTER"),
    };
    let mut daemon = DaemonChild(command.spawn().expect("spawn packet28d"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if ready_path(root).exists() {
            let runtime = serde_json::from_slice(&std::fs::read(runtime_path(root)).unwrap())
                .expect("decode runtime metadata");
            return (daemon, runtime);
        }
        if let Some(status) = daemon.0.try_wait().unwrap() {
            let log = std::fs::read_to_string(log_path(root)).unwrap_or_default();
            panic!("daemon exited before readiness with {status}; log:\n{log}");
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn exchange<Request: Serialize, Response: DeserializeOwned>(
    runtime: &DaemonRuntimeInfo,
    request: &Request,
) -> Response {
    let mut stream = UnixStream::connect(&runtime.socket_path).expect("connect to packet28d");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write_frame(&mut stream, request).expect("write request");
    read_frame(&mut stream).expect("read response")
}

fn stop(mut daemon: DaemonChild, runtime: &DaemonRuntimeInfo) {
    let _: DaemonResponse = exchange(runtime, &DaemonRequest::Stop);
    let status = daemon.0.wait().expect("join daemon");
    assert!(status.success(), "daemon stop completed with {status}");
}

fn archive(
    runtime: &DaemonRuntimeInfo,
    request: TaskRecordArchiveRequestV1,
) -> TaskRecordArchiveReportV1 {
    match exchange(
        runtime,
        &DaemonRegistryRequestV1::TaskRecordArchive { request },
    ) {
        DaemonRegistryResponseV1::TaskRecordArchive { report } => report,
        other => panic!("unexpected archive response: {other:?}"),
    }
}

fn exact(task_id: &str, apply: bool) -> TaskRecordArchiveRequestV1 {
    TaskRecordArchiveRequestV1 {
        task_id: Some(task_id.to_string()),
        min_record_bytes: None,
        apply,
    }
}

fn task_status(runtime: &DaemonRuntimeInfo, task_id: &str) -> Option<TaskRecord> {
    match exchange(
        runtime,
        &DaemonRequest::TaskStatus {
            task_id: task_id.to_string(),
        },
    ) {
        DaemonResponse::TaskStatus { task } => task,
        other => panic!("unexpected task status response: {other:?}"),
    }
}

fn record(task_id: &str, timestamp: u64) -> TaskRecord {
    TaskRecord {
        task_id: task_id.to_string(),
        last_completed_at_unix: Some(timestamp),
        ..TaskRecord::default()
    }
}

/// Seeds a dormant oversized record with two events and an artifact beside a
/// smaller, older healthy record, returning the oversized original.
fn seed(root: &Path, extra: impl IntoIterator<Item = TaskRecord>) -> TaskRecord {
    let mut big = TaskRecord {
        last_error: Some("x".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES)),
        question_texts: BTreeMap::from([("q1".to_string(), "kept".to_string())]),
        ..record(BIG, 100)
    };
    let mut registry = TaskRegistry::default();
    for task in [big.clone(), record(GOOD, 50)].into_iter().chain(extra) {
        registry.tasks.insert(task.task_id.clone(), task);
    }
    save_task_watch_registry_checkpoint(root, &registry, &WatchRegistry::default()).unwrap();
    let event = DaemonEvent {
        kind: "seed".to_string(),
        occurred_at_unix: 1,
        data: serde_json::json!({"n": 1}),
    };
    for _ in 0..2 {
        big.last_event_seq = append_next_task_event(root, BIG, &event).unwrap().seq;
    }
    registry.tasks.insert(BIG.to_string(), big.clone());
    save_task_watch_registry_checkpoint(root, &registry, &WatchRegistry::default()).unwrap();
    let artifacts = task_artifact_dir(root, &TaskStorageId::try_from(BIG).unwrap());
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(artifacts.join("brief.md"), b"brief evidence\n").unwrap();
    big
}

fn evidence(root: &Path) -> (Vec<u8>, Vec<u8>) {
    let storage = TaskStorageId::try_from(BIG).unwrap();
    (
        std::fs::read(task_event_log_path(root, &storage)).unwrap(),
        std::fs::read(task_artifact_dir(root, &storage).join("brief.md")).unwrap(),
    )
}

#[test]
fn oversized_dormant_record_is_archived_online_and_survives_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let original = seed(root, []);
    let evidence_before = evidence(root);
    let (daemon, runtime) = spawn(root, None);

    // Early, live diagnostics name the unlistable record before any action.
    let status = match exchange(&runtime, &DaemonRegistryRequestV1::Status) {
        DaemonRegistryResponseV1::Status { status } => *status,
        other => panic!("unexpected status: {other:?}"),
    };
    assert_eq!(status.record_size_warning_count, 1);
    assert_eq!(status.record_size_warnings[0].task_id, BIG);
    assert_eq!(
        status.record_size_warnings[0].level,
        TaskRecordSizeLevel::OverPageLimit
    );

    // A below-floor size selector is rejected rather than reaching healthy records.
    assert!(matches!(
        exchange(
            &runtime,
            &DaemonRegistryRequestV1::TaskRecordArchive {
                request: TaskRecordArchiveRequestV1 {
                    task_id: None,
                    min_record_bytes: Some(0),
                    apply: false,
                },
            },
        ),
        DaemonRegistryResponseV1::Error { ref message } if message.contains("archive floor")
    ));
    // The size selector picks only the oversized record, never the older healthy one.
    let planned = archive(
        &runtime,
        TaskRecordArchiveRequestV1 {
            task_id: None,
            min_record_bytes: Some(64 * 1024),
            apply: false,
        },
    );
    assert_eq!(planned.candidates.len(), 1);
    assert_eq!(planned.candidates[0].task_id, BIG);
    assert_eq!(
        planned.candidates[0].outcome,
        TaskRecordArchiveOutcome::WouldArchive
    );
    let planned_pointer = planned.candidates[0].archive.clone().unwrap();
    assert!(planned_pointer.omitted_fields.contains_key("last_error"));
    // Exact targeting of a healthy small record is refused.
    let healthy_plan = archive(&runtime, exact(GOOD, true));
    assert_eq!(
        healthy_plan.candidates[0].outcome,
        TaskRecordArchiveOutcome::Refused
    );
    // The dry run changed nothing.
    let unchanged = task_status(&runtime, BIG).unwrap();
    assert_eq!(unchanged.last_error, original.last_error);
    assert!(unchanged.archived.is_none());

    let applied = archive(&runtime, exact(BIG, true));
    assert_eq!(applied.daemon_pid, runtime.pid);
    assert_eq!(
        applied.candidates[0].outcome,
        TaskRecordArchiveOutcome::Archived
    );
    let pointer = applied.candidates[0].archive.clone().unwrap();
    assert_eq!(pointer.digest, planned_pointer.digest);

    // Same daemon keeps serving: status, the healthy task, and full pagination.
    assert!(task_status(&runtime, GOOD).is_some());
    let page = match exchange(
        &runtime,
        &DaemonRegistryRequestV1::TaskListPage {
            request: TaskListPageRequestV1::default(),
        },
    ) {
        DaemonRegistryResponseV1::TaskListPage { page } => page,
        other => panic!("unexpected page: {other:?}"),
    };
    assert!(page.omitted_oversized.is_empty());
    let listed = page
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(listed, vec![BIG, GOOD]);
    let tombstone = page.tasks[0].clone();
    assert_eq!(tombstone.archived.as_ref(), Some(&pointer));
    assert_eq!(tombstone.last_event_seq, original.last_event_seq);
    assert_eq!(tombstone.question_texts, original.question_texts);
    assert_eq!(tombstone.last_error, None);
    match exchange(&runtime, &DaemonRegistryRequestV1::Status) {
        DaemonRegistryResponseV1::Status { status } => {
            assert_eq!(status.pid, runtime.pid);
            assert_eq!(status.record_size_warning_count, 0);
        }
        other => panic!("unexpected status: {other:?}"),
    }

    // The archived identity cannot be continued, written, or resurrected.
    let write: DaemonResponse = exchange(
        &runtime,
        &DaemonRequest::BrokerWriteState {
            request: BrokerWriteStateRequest {
                task_id: BIG.to_string(),
                op: Some(BrokerWriteOp::QuestionOpen),
                question_id: Some("q2".to_string()),
                text: Some("resurrect".to_string()),
                ..BrokerWriteStateRequest::default()
            },
        },
    );
    assert!(
        matches!(write, DaemonResponse::Error { ref message } if message.contains("archived")),
        "{write:?}"
    );
    let subscribe: DaemonResponse = exchange(
        &runtime,
        &DaemonRequest::TaskSubscribe {
            task_id: BIG.to_string(),
            replay_last: 0,
            after_seq: None,
        },
    );
    assert!(
        matches!(subscribe, DaemonResponse::Error { ref message } if message.contains("archived"))
    );
    match exchange(
        &runtime,
        &DaemonRequest::TaskCancel {
            task_id: BIG.to_string(),
        },
    ) {
        DaemonResponse::TaskCancel {
            task: Some(task), ..
        } => {
            assert_eq!(task.archived.as_ref(), Some(&pointer));
        }
        other => panic!("unexpected cancel response: {other:?}"),
    }
    let again = archive(&runtime, exact(BIG, true));
    assert_eq!(
        again.candidates[0].outcome,
        TaskRecordArchiveOutcome::AlreadyArchived
    );
    stop(daemon, &runtime);

    // Restart: the tombstone is durable, history is not reinterpreted, and the
    // archive holds the exact original record.
    let (daemon, runtime) = spawn(root, None);
    let restarted = task_status(&runtime, BIG).unwrap();
    assert_eq!(restarted.archived.as_ref(), Some(&pointer));
    assert_eq!(restarted.last_event_seq, original.last_event_seq);
    assert!(restarted.superseded_by.is_none());
    assert_eq!(restarted.question_texts.get("q2"), None);
    stop(daemon, &runtime);
    assert_eq!(evidence(root), evidence_before);
    let archived = read_task_record_archive(root, &restarted).unwrap();
    assert_eq!(archived, encode_task_record_compact(&original).unwrap());
    assert_eq!(task_record_archive_digest(&archived), pointer.digest);
}

#[test]
fn archive_refuses_active_pointer_and_recovery_owners() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let link = TaskHistoryRecovery {
        predecessor_task_id: BIG.to_string(),
        successor_task_id: "task-next".to_string(),
        ..TaskHistoryRecovery::default()
    };
    let owner = TaskRecord {
        last_error: Some("o".repeat(128 * 1024)),
        ..record("task-owner", 10)
    };
    let child = TaskRecord {
        latest_hook_bootstrap_owner_task_id: Some("task-owner".to_string()),
        ..record("task-child", 10)
    };
    let successor = TaskRecord {
        recovered_from: Some(link),
        last_error: Some("s".repeat(128 * 1024)),
        ..record("task-next", 10)
    };
    seed(root, [owner, child, successor]);
    save_active_task_record(
        root,
        &ActiveTaskRecord {
            task_id: BIG.to_string(),
            session_id: None,
            updated_at_unix: 1,
        },
    )
    .unwrap();
    let (daemon, runtime) = spawn(root, None);

    let report = archive(
        &runtime,
        TaskRecordArchiveRequestV1 {
            task_id: None,
            min_record_bytes: Some(64 * 1024),
            apply: true,
        },
    );
    let outcomes = report
        .candidates
        .iter()
        .map(|candidate| (candidate.task_id.as_str(), candidate.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec![
            (BIG, TaskRecordArchiveOutcome::Refused),
            ("task-next", TaskRecordArchiveOutcome::Refused),
            ("task-owner", TaskRecordArchiveOutcome::Refused),
        ]
    );
    let reasons = report
        .candidates
        .iter()
        .map(|candidate| candidate.reason.clone().unwrap())
        .collect::<Vec<_>>();
    assert!(reasons[0].contains("active task pointer"), "{reasons:?}");
    assert!(reasons[1].contains("recovery successor"), "{reasons:?}");
    assert!(reasons[2].contains("bootstrap owner"), "{reasons:?}");
    for task_id in [BIG, "task-next", "task-owner"] {
        assert!(task_status(&runtime, task_id).unwrap().archived.is_none());
    }
    stop(daemon, &runtime);
}

#[test]
fn crash_after_archive_publication_keeps_the_original_authoritative_and_retries_idempotently() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let original = seed(root, []);
    let evidence_before = evidence(root);
    let (mut daemon, runtime) = spawn(root, Some("archive_published"));
    let mut stream = UnixStream::connect(&runtime.socket_path).unwrap();
    write_frame(
        &mut stream,
        &DaemonRegistryRequestV1::TaskRecordArchive {
            request: exact(BIG, true),
        },
    )
    .unwrap();
    let status = daemon.0.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(87),
        "daemon must stop at the injected boundary"
    );

    let (daemon, runtime) = spawn(root, None);
    let survivor = task_status(&runtime, BIG).unwrap();
    assert!(survivor.archived.is_none());
    assert_eq!(survivor.last_error, original.last_error);
    // The unreferenced archive is retained and reused by the retry.
    let digest = task_record_archive_digest(&encode_task_record_compact(&survivor).unwrap());
    let archive_path = task_artifact_dir(root, &TaskStorageId::try_from(BIG).unwrap())
        .join("record-archive")
        .join(format!(
            "{}.task-record.json",
            digest.trim_start_matches("blake3:")
        ));
    assert!(
        archive_path.exists(),
        "pre-commit archive must survive the crash"
    );
    let retried = archive(&runtime, exact(BIG, true));
    assert_eq!(
        retried.candidates[0].outcome,
        TaskRecordArchiveOutcome::Archived
    );
    assert_eq!(
        retried.candidates[0].archive.as_ref().unwrap().digest,
        digest
    );
    stop(daemon, &runtime);
    assert_eq!(evidence(root), evidence_before);
}

#[test]
fn crash_after_durable_tombstone_restarts_with_the_archive_as_authority() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let original = seed(root, []);
    let evidence_before = evidence(root);
    let (mut daemon, runtime) = spawn(root, Some("tombstone_durable"));
    let mut stream = UnixStream::connect(&runtime.socket_path).unwrap();
    write_frame(
        &mut stream,
        &DaemonRegistryRequestV1::TaskRecordArchive {
            request: exact(BIG, true),
        },
    )
    .unwrap();
    let status = daemon.0.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(87),
        "daemon must stop at the injected boundary"
    );

    let (daemon, runtime) = spawn(root, None);
    let tombstone = task_status(&runtime, BIG).unwrap();
    assert!(
        tombstone.archived.is_some(),
        "the durable tombstone must be replayed"
    );
    assert_eq!(tombstone.last_event_seq, original.last_event_seq);
    assert!(tombstone.superseded_by.is_none());
    assert!(task_status(&runtime, GOOD).is_some());
    stop(daemon, &runtime);
    assert_eq!(evidence(root), evidence_before);
    assert_eq!(
        read_task_record_archive(root, &tombstone).unwrap(),
        encode_task_record_compact(&original).unwrap()
    );
}

const FORWARD_ONLY: &str = "task-forward-only";

fn future_evidence() -> serde_json::Value {
    serde_json::json!({"note": "future-evidence-marker", "bytes": "Y".repeat(70_000)})
}

/// Seeds a registry as a newer build would persist it: a known-oversized
/// record that also carries large and small forward fields, a record that is
/// oversized only through a forward field, and a healthy neighbor with its
/// own forward field. Returns the raw task objects.
fn seed_forward_registry(root: &Path) -> serde_json::Value {
    ensure_daemon_dir(root).unwrap();
    let tasks = serde_json::json!({
        (BIG): {
            "task_id": BIG,
            "running": false,
            "cancel_requested": false,
            "pending_replan": false,
            "last_event_seq": 0,
            "last_completed_at_unix": 100,
            "last_error": "x".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES),
            "future_evidence": future_evidence(),
            "future_small": {"v": 7}
        },
        (FORWARD_ONLY): {
            "task_id": FORWARD_ONLY,
            "running": false,
            "last_completed_at_unix": 90,
            "future_blob": "F".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES)
        },
        (GOOD): {
            "task_id": GOOD,
            "running": false,
            "last_completed_at_unix": 50,
            "future_neighbor": {"keep": "neighbor-marker"}
        }
    });
    std::fs::write(
        task_registry_path(root),
        serde_json::to_vec(&serde_json::json!({ "tasks": tasks })).unwrap(),
    )
    .unwrap();
    std::fs::write(watch_registry_path(root), br#"{"watches":[]}"#).unwrap();
    tasks
}

fn raw_tasks(root: &Path) -> serde_json::Value {
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(task_registry_path(root)).unwrap()).unwrap();
    raw["tasks"].clone()
}

fn assert_archive_holds(root: &Path, tombstone: &TaskRecord, raw: &serde_json::Value) {
    let archived: serde_json::Value =
        serde_json::from_slice(&read_task_record_archive(root, tombstone).unwrap()).unwrap();
    for (field, value) in raw.as_object().unwrap() {
        assert_eq!(
            &archived[field], value,
            "archive of {} must hold raw field {field}",
            tombstone.task_id
        );
    }
}

#[test]
fn forward_fields_are_archived_sized_and_never_lost_through_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let seeded = seed_forward_registry(root);
    let (daemon, runtime) = spawn(root, None);
    assert!(
        raw_tasks(root)[BIG].get("future_evidence").is_some(),
        "startup checkpoint preserves forward fields before archival"
    );

    // A record that is oversized only through forward fields is warned about.
    let status = match exchange(&runtime, &DaemonRegistryRequestV1::Status) {
        DaemonRegistryResponseV1::Status { status } => *status,
        other => panic!("unexpected status: {other:?}"),
    };
    let warned = status
        .record_size_warnings
        .iter()
        .map(|warning| (warning.task_id.as_str(), warning.level))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        warned.get(FORWARD_ONLY),
        Some(&TaskRecordSizeLevel::OverPageLimit)
    );
    assert_eq!(warned.get(BIG), Some(&TaskRecordSizeLevel::OverPageLimit));
    assert!(!warned.contains_key(GOOD));

    // The size selector sees forward bytes and never the healthy neighbor.
    let planned = archive(
        &runtime,
        TaskRecordArchiveRequestV1 {
            task_id: None,
            min_record_bytes: Some(64 * 1024),
            apply: false,
        },
    );
    let planned_ids = planned
        .candidates
        .iter()
        .map(|candidate| candidate.task_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(planned_ids, vec![BIG, FORWARD_ONLY]);
    let big_plan = planned.candidates[0].archive.clone().unwrap();
    assert!(big_plan.omitted_fields.contains_key("future_evidence"));
    assert!(big_plan.omitted_fields.contains_key("last_error"));
    assert!(!big_plan.omitted_fields.contains_key("future_small"));
    assert!(
        planned.candidates[0].encoded_bytes > MAX_REGISTRY_PAGE_ITEM_BYTES as u64 + 70_000,
        "planned size includes forward bytes"
    );

    let mut pointers = BTreeMap::new();
    for task_id in [BIG, FORWARD_ONLY] {
        let applied = archive(&runtime, exact(task_id, true));
        assert_eq!(applied.daemon_pid, runtime.pid);
        assert_eq!(
            applied.candidates[0].outcome,
            TaskRecordArchiveOutcome::Archived,
            "{:?}",
            applied.candidates[0].reason
        );
        let pointer = applied.candidates[0].archive.clone().unwrap();
        if task_id == BIG {
            assert_eq!(pointer.digest, big_plan.digest);
        }
        pointers.insert(task_id, pointer);
    }
    // The archives hold every persisted field, forward ones included, before
    // any restart.
    for task_id in [BIG, FORWARD_ONLY] {
        let tombstone = task_status(&runtime, task_id).unwrap();
        assert_eq!(tombstone.archived.as_ref(), Some(&pointers[task_id]));
        assert_archive_holds(root, &tombstone, &seeded[task_id]);
    }
    assert!(task_status(&runtime, GOOD).is_some());
    stop(daemon, &runtime);

    let check_raw = |raw: &serde_json::Value| {
        assert_eq!(raw[BIG]["future_small"], serde_json::json!({"v": 7}));
        assert!(raw[BIG].get("future_evidence").is_none());
        assert!(raw[BIG]["last_error"].is_null());
        assert!(raw[FORWARD_ONLY].get("future_blob").is_none());
        assert_eq!(
            raw[GOOD]["future_neighbor"],
            seeded[GOOD]["future_neighbor"]
        );
        for task_id in [BIG, FORWARD_ONLY] {
            assert_eq!(
                serde_json::to_vec(&raw[task_id]).unwrap().len() as u64,
                pointers[task_id].tombstone_encoded_bytes,
                "the pointer records the persisted tombstone size of {task_id}"
            );
        }
    };
    check_raw(&raw_tasks(root));

    // Restart: tombstones replay, shed values stay shed, retained and
    // neighboring forward fields stay, and the archives still authenticate.
    let (daemon, runtime) = spawn(root, None);
    for task_id in [BIG, FORWARD_ONLY] {
        let tombstone = task_status(&runtime, task_id).unwrap();
        assert_eq!(tombstone.archived.as_ref(), Some(&pointers[task_id]));
        assert_archive_holds(root, &tombstone, &seeded[task_id]);
    }
    match exchange(&runtime, &DaemonRegistryRequestV1::Status) {
        DaemonRegistryResponseV1::Status { status } => {
            assert_eq!(status.record_size_warning_count, 0);
        }
        other => panic!("unexpected status: {other:?}"),
    }
    stop(daemon, &runtime);
    check_raw(&raw_tasks(root));
}

#[test]
fn raw_authority_drift_before_commit_refuses_without_mutation() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let seeded = seed_forward_registry(root);
    let release = root.join("archive-release");
    let paused = root.join("archive-release.paused");
    let (daemon, runtime) = spawn_with(root, None, Some(&release));

    let mut stream = UnixStream::connect(&runtime.socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    write_frame(
        &mut stream,
        &DaemonRegistryRequestV1::TaskRecordArchive {
            request: exact(BIG, true),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "archive never reached publication"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // While the archive is published but uncommitted, the raw authority
    // changes the evidence the archive bound.
    let path = task_registry_path(root);
    let committed = std::fs::read(&path).unwrap();
    let mut drifted: serde_json::Value = serde_json::from_slice(&committed).unwrap();
    drifted["tasks"][BIG]["future_evidence"]["note"] = serde_json::json!("drifted");
    std::fs::write(&path, serde_json::to_vec_pretty(&drifted).unwrap()).unwrap();
    std::fs::write(&release, b"go").unwrap();
    let response: DaemonRegistryResponseV1 = read_frame(&mut stream).unwrap();
    let report = match response {
        DaemonRegistryResponseV1::TaskRecordArchive { report } => report,
        other => panic!("unexpected archive response: {other:?}"),
    };
    assert_eq!(
        report.candidates[0].outcome,
        TaskRecordArchiveOutcome::Failed,
        "{:?}",
        report.candidates[0].reason
    );
    assert!(report.candidates[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("original record is unchanged"));
    // Restore the committed authority byte for byte; nothing was mutated.
    std::fs::write(&path, &committed).unwrap();
    let survivor = task_status(&runtime, BIG).unwrap();
    assert!(survivor.archived.is_none());
    assert_eq!(
        survivor.last_error.as_ref().map(String::len),
        Some(MAX_REGISTRY_PAGE_ITEM_BYTES)
    );
    let raw = raw_tasks(root);
    for (field, value) in seeded[BIG].as_object().unwrap() {
        assert_eq!(
            &raw[BIG][field], value,
            "raw field {field} must be unchanged"
        );
    }
    stop(daemon, &runtime);
    assert_eq!(raw_tasks(root)[BIG]["future_evidence"], future_evidence());
}
