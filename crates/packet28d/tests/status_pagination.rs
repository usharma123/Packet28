use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use packet28_daemon_core::storage::save_task_watch_registry_checkpoint;
use packet28_daemon_protocol::frame::{read_frame, write_frame, MAX_SOCKET_MESSAGE_BYTES};
use packet28_daemon_protocol::message::{
    DaemonRequest, DaemonResponse, DaemonRuntimeInfo, DaemonStatus,
};
use packet28_daemon_protocol::paths::{log_path, ready_path, runtime_path};
use packet28_daemon_protocol::registry::{
    DaemonRegistryRequestV1, DaemonRegistryResponseV1, TaskListPageRequestV1,
    MAX_DAEMON_STATUS_V1_RESPONSE_BYTES, MAX_REGISTRY_PAGE_ITEM_BYTES,
    MAX_REGISTRY_PAGE_RESPONSE_BYTES,
};
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistry};
use serde::de::DeserializeOwned;
use serde::Serialize;

const SEEDED_TASKS: usize = 5_000;

struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

trait ReadWrite: Read + Write {}

impl<T: Read + Write> ReadWrite for T {}

fn connect(runtime: &DaemonRuntimeInfo) -> Box<dyn ReadWrite> {
    let mut stream: Box<dyn ReadWrite> =
        if let Some(endpoint) = runtime.socket_path.strip_prefix("tcp://") {
            let stream = TcpStream::connect(endpoint).expect("connect to packet28d TCP endpoint");
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .expect("set daemon response timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(15)))
                .expect("set daemon request timeout");
            Box::new(stream)
        } else {
            #[cfg(unix)]
            {
                let stream = UnixStream::connect(&runtime.socket_path)
                    .expect("connect to packet28d Unix endpoint");
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .expect("set daemon response timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(15)))
                    .expect("set daemon request timeout");
                Box::new(stream)
            }
            #[cfg(not(unix))]
            {
                panic!(
                    "unsupported non-TCP daemon endpoint '{}'",
                    runtime.socket_path
                );
            }
        };
    if let Some(auth) = runtime.transport_auth.as_ref() {
        write_frame(&mut stream, auth).expect("write daemon authentication");
        assert!(matches!(
            read_frame::<_, DaemonResponse>(&mut stream)
                .expect("read authentication response"),
            DaemonResponse::Ack { ref message } if message == "authenticated"
        ));
    }
    stream
}

fn exchange<Request, Response>(stream: &mut Box<dyn ReadWrite>, request: &Request) -> Response
where
    Request: Serialize,
    Response: DeserializeOwned,
{
    write_frame(&mut *stream, request).expect("write daemon request");
    read_frame(&mut *stream).expect("read daemon response")
}

fn wait_for_ready(daemon: &mut Child, root: &std::path::Path) -> DaemonRuntimeInfo {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if ready_path(root).exists() {
            return serde_json::from_slice(
                &std::fs::read(runtime_path(root)).expect("read runtime metadata"),
            )
            .expect("decode runtime metadata");
        }
        if let Some(status) = daemon.try_wait().expect("probe daemon") {
            let log = std::fs::read_to_string(log_path(root))
                .unwrap_or_else(|error| format!("<failed to read daemon log: {error}>"));
            panic!("daemon exited before readiness with {status}; log:\n{log}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not become ready before timeout"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn seeded_five_thousand_task_daemon_keeps_status_live_and_pages_every_task() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tasks = TaskRegistry {
        tasks: (0..SEEDED_TASKS)
            .map(|index| {
                let task_id = format!("seed-task-{index:04}");
                (
                    task_id.clone(),
                    TaskRecord {
                        task_id,
                        ..TaskRecord::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    };
    let unbounded_status = DaemonResponse::Status {
        status: DaemonStatus {
            tasks: tasks.tasks.values().cloned().collect(),
            ..DaemonStatus::default()
        },
    };
    let unbounded_bytes =
        serde_json::to_vec(&unbounded_status).expect("encode unbounded status fixture");
    eprintln!("unbounded_status_bytes={}", unbounded_bytes.len());
    assert!(
        unbounded_bytes.len() > MAX_SOCKET_MESSAGE_BYTES,
        "seed fixture encoded to {} bytes and must reproduce the historical \
         oversized Status response",
        unbounded_bytes.len()
    );
    save_task_watch_registry_checkpoint(workspace.path(), &tasks, &WatchRegistry::default())
        .expect("seed task/watch checkpoint");
    drop(tasks);

    let mut daemon = DaemonChild(
        Command::new(env!("CARGO_BIN_EXE_packet28d"))
            .args(["serve", "--root"])
            .arg(workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn packet28d"),
    );
    let runtime = wait_for_ready(&mut daemon.0, workspace.path());
    let mut stream = connect(&runtime);
    let requests_started = Instant::now();

    let legacy_status: DaemonResponse = exchange(&mut stream, &DaemonRequest::Status);
    assert!(matches!(
        legacy_status,
        DaemonResponse::Error { ref message }
            if message.contains("legacy status requires more than")
                && message.contains("registry_status_v1")
    ));

    let status_response: DaemonRegistryResponseV1 =
        exchange(&mut stream, &DaemonRegistryRequestV1::Status);
    assert!(
        serde_json::to_vec(&status_response)
            .expect("encode bounded V1 status")
            .len()
            <= MAX_DAEMON_STATUS_V1_RESPONSE_BYTES
    );
    let status = match status_response {
        DaemonRegistryResponseV1::Status { status } => *status,
        other => panic!("unexpected registry status response: {other:?}"),
    };
    assert_eq!(status.task_count, SEEDED_TASKS);
    assert_eq!(status.watch_count, 0);
    let expected_revision = status
        .registry_revision
        .expect("new daemon registry status must publish a revision");

    let mut after_task_id = None;
    let mut seen = BTreeSet::new();
    loop {
        let response: DaemonRegistryResponseV1 = exchange(
            &mut stream,
            &DaemonRegistryRequestV1::TaskListPage {
                request: TaskListPageRequestV1 {
                    snapshot_revision: Some(expected_revision.clone()),
                    after_task_id: after_task_id.clone(),
                    limit: 127,
                },
            },
        );
        assert!(
            serde_json::to_vec(&response)
                .expect("encode task page")
                .len()
                <= MAX_REGISTRY_PAGE_RESPONSE_BYTES
        );
        let page = match response {
            DaemonRegistryResponseV1::TaskListPage { page } => page,
            other => panic!("unexpected task page response: {other:?}"),
        };
        assert_eq!(page.snapshot_revision, expected_revision);
        assert_eq!(page.total, SEEDED_TASKS);
        assert!(!page.tasks.is_empty());
        for task in page.tasks {
            if let Some(previous) = seen.last() {
                assert!(previous < &task.task_id);
            }
            assert!(seen.insert(task.task_id));
        }
        let Some(next) = page.next_after_task_id else {
            break;
        };
        assert_eq!(seen.last(), Some(&next));
        assert_ne!(after_task_id.as_ref(), Some(&next));
        after_task_id = Some(next);
    }
    assert_eq!(seen.len(), SEEDED_TASKS);
    assert_eq!(seen.first().map(String::as_str), Some("seed-task-0000"));
    assert_eq!(seen.last().map(String::as_str), Some("seed-task-4999"));
    let request_elapsed = requests_started.elapsed();
    eprintln!("registry_request_elapsed={request_elapsed:?}");
    assert!(
        request_elapsed < Duration::from_secs(5),
        "bounded status plus persistent-session pagination took {request_elapsed:?}"
    );

    assert!(matches!(
        exchange::<_, DaemonResponse>(&mut stream, &DaemonRequest::Stop),
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    assert!(
        daemon.0.wait().expect("join daemon").success(),
        "daemon did not shut down cleanly"
    );
}

#[test]
fn oversized_records_before_between_and_after_healthy_pages_are_all_reported() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let task_ids = [
        "task-00-oversized",
        "task-01-healthy",
        "task-02-oversized",
        "task-03-healthy",
        "task-04-oversized",
    ];
    let tasks = TaskRegistry {
        tasks: task_ids
            .iter()
            .map(|task_id| {
                let oversized = task_id.ends_with("oversized");
                (
                    (*task_id).to_string(),
                    TaskRecord {
                        task_id: (*task_id).to_string(),
                        last_error: oversized.then(|| "x".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES)),
                        ..TaskRecord::default()
                    },
                )
            })
            .collect(),
    };
    let expected_encoded_bytes = tasks
        .tasks
        .iter()
        .filter(|(task_id, _)| task_id.ends_with("oversized"))
        .map(|(task_id, task)| {
            (
                task_id.clone(),
                u64::try_from(serde_json::to_vec(task).unwrap().len()).unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    save_task_watch_registry_checkpoint(workspace.path(), &tasks, &WatchRegistry::default())
        .expect("seed oversized task/watch checkpoint");

    let mut daemon = DaemonChild(
        Command::new(env!("CARGO_BIN_EXE_packet28d"))
            .args(["serve", "--root"])
            .arg(workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn packet28d"),
    );
    let runtime = wait_for_ready(&mut daemon.0, workspace.path());
    let mut stream = connect(&runtime);
    let status: DaemonRegistryResponseV1 = exchange(&mut stream, &DaemonRegistryRequestV1::Status);
    let revision = match status {
        DaemonRegistryResponseV1::Status { status } => status
            .registry_revision
            .expect("registry status must publish a revision"),
        other => panic!("unexpected registry status response: {other:?}"),
    };

    let mut after_task_id = None;
    let mut healthy = Vec::new();
    let mut omitted = BTreeMap::new();
    let mut page_shapes = Vec::new();
    loop {
        let response: DaemonRegistryResponseV1 = exchange(
            &mut stream,
            &DaemonRegistryRequestV1::TaskListPage {
                request: TaskListPageRequestV1 {
                    snapshot_revision: Some(revision.clone()),
                    after_task_id: after_task_id.clone(),
                    limit: 1,
                },
            },
        );
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_REGISTRY_PAGE_RESPONSE_BYTES);
        let page = match response {
            DaemonRegistryResponseV1::TaskListPage { page } => page,
            other => panic!("unexpected task page response: {other:?}"),
        };
        assert_eq!(page.snapshot_revision, revision);
        assert_eq!(page.total, task_ids.len());
        page_shapes.push((
            page.tasks.len(),
            page.omitted_oversized.len(),
            page.next_after_task_id.clone(),
        ));
        healthy.extend(page.tasks.into_iter().map(|task| task.task_id));
        for record in page.omitted_oversized {
            assert_eq!(
                expected_encoded_bytes.get(&record.task_id),
                Some(&record.encoded_bytes)
            );
            assert!(omitted
                .insert(record.task_id, record.encoded_bytes)
                .is_none());
        }
        let Some(next) = page.next_after_task_id else {
            break;
        };
        assert_ne!(after_task_id.as_ref(), Some(&next));
        after_task_id = Some(next);
    }

    assert_eq!(healthy, ["task-01-healthy", "task-03-healthy"]);
    assert_eq!(omitted, expected_encoded_bytes);
    assert_eq!(
        page_shapes,
        [
            (1, 1, Some("task-01-healthy".to_string())),
            (1, 1, Some("task-03-healthy".to_string())),
            (0, 1, None),
        ]
    );

    assert!(matches!(
        exchange::<_, DaemonResponse>(&mut stream, &DaemonRequest::Stop),
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    assert!(daemon.0.wait().expect("join daemon").success());
}
