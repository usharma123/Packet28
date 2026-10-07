use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use context_kernel_core::{KernelSequenceRequest, KernelStepRequest};
use notify::{RecursiveMode, Watcher as _};
use packet28_daemon_core::storage::{
    append_next_task_event, load_task_events, load_task_registry, load_watch_registry,
    save_task_registry, save_task_watch_registry_checkpoint,
};
use packet28_daemon_protocol::broker::{BrokerWriteOp, BrokerWriteStateRequest};
use packet28_daemon_protocol::commands::WatchSpec;
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::message::{
    DaemonEvent, DaemonEventFrame, DaemonRequest, DaemonResponse, DaemonRuntimeInfo,
};
use packet28_daemon_protocol::paths::{
    ready_path, runtime_path, task_event_log_path, TaskStorageId,
};
use packet28_daemon_protocol::task::{
    TaskLifecycle, TaskRecord, TaskRegistry, WatchRegistration, WatchRegistry,
};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ProcessGroupGuard {
    child: Option<Child>,
    process_group: i32,
}

impl ProcessGroupGuard {
    fn spawn() -> Self {
        let mut child = Command::new("sh")
            .args(["-c", "printf x; exec sleep 30"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated process group");
        let mut ready = [0_u8; 1];
        child
            .stdout
            .take()
            .expect("capture process-group readiness")
            .read_exact(&mut ready)
            .expect("wait for process-group readiness");
        assert_eq!(ready, *b"x");
        let process_group = i32::try_from(child.id()).expect("child pid fits process-group id");
        Self {
            child: Some(child),
            process_group,
        }
    }

    fn pid(&self) -> u32 {
        u32::try_from(self.process_group).expect("positive process-group id")
    }

    fn exists(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| {
            child
                .try_wait()
                .expect("probe process-group leader")
                .is_none()
        })
    }

    fn terminate_and_reap(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        child.wait().expect("reap isolated process group leader");
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

fn spawn_daemon(root: &Path) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_packet28d"))
            .args(["serve", "--root"])
            .arg(root)
            .env("PACKET28D_FORCE_TCP", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn packet28d"),
    )
}

fn wait_for_ready(daemon: &mut ChildGuard, root: &Path) -> DaemonRuntimeInfo {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    })
    .expect("create readiness watcher");
    watcher
        .watch(root, RecursiveMode::Recursive)
        .expect("watch workspace readiness");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ready_path(root).exists() {
            return serde_json::from_slice(
                &fs::read(runtime_path(root)).expect("read daemon runtime"),
            )
            .expect("decode daemon runtime");
        }
        if let Some(status) = daemon.0.try_wait().expect("probe daemon") {
            let mut stderr = String::new();
            daemon
                .0
                .stderr
                .take()
                .expect("captured daemon stderr")
                .read_to_string(&mut stderr)
                .expect("read daemon stderr");
            panic!("daemon exited before readiness with {status}: {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not publish readiness before timeout"
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        event_rx
            .recv_timeout(remaining)
            .expect("readiness watcher timed out")
            .expect("readiness watcher failed");
    }
}

fn wait_for_startup_failure(daemon: &mut ChildGuard, root: &Path) -> String {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    })
    .expect("create startup-failure watcher");
    watcher
        .watch(root, RecursiveMode::Recursive)
        .expect("watch workspace startup");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            !ready_path(root).exists(),
            "unsafe startup unexpectedly published readiness"
        );
        if let Some(status) = daemon.0.try_wait().expect("probe failing daemon") {
            assert!(!status.success(), "unsafe startup unexpectedly succeeded");
            let mut stderr = String::new();
            daemon
                .0
                .stderr
                .take()
                .expect("captured daemon stderr")
                .read_to_string(&mut stderr)
                .expect("read daemon stderr");
            return stderr;
        }
        assert!(
            Instant::now() < deadline,
            "unsafe startup did not exit before timeout"
        );
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        match event_rx.recv_timeout(remaining) {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("startup watcher failed: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("startup watcher disconnected")
            }
        }
    }
}

fn connect_authenticated(runtime: &DaemonRuntimeInfo) -> TcpStream {
    let endpoint = runtime
        .socket_path
        .strip_prefix("tcp://")
        .expect("forced TCP endpoint");
    let mut stream = TcpStream::connect(endpoint).expect("connect to packet28d");
    let auth = runtime
        .transport_auth
        .as_ref()
        .expect("forced TCP runtime authentication capability");
    write_frame(&mut stream, auth).expect("write daemon authentication prelude");
    let response: DaemonResponse =
        read_frame(&mut stream).expect("read daemon authentication response");
    assert!(matches!(
        response,
        DaemonResponse::Ack { ref message } if message == "authenticated"
    ));
    stream
}

fn stop_and_wait(daemon: &mut ChildGuard, runtime: &DaemonRuntimeInfo) {
    let mut stream = connect_authenticated(runtime);
    write_frame(&mut stream, &DaemonRequest::Stop).expect("write stop request");
    let response: DaemonResponse = read_frame(&mut stream).expect("read stop response");
    assert!(matches!(
        response,
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));

    let status = daemon.0.wait().expect("join daemon after Stop");
    let mut stderr = String::new();
    daemon
        .0
        .stderr
        .take()
        .expect("captured daemon stderr")
        .read_to_string(&mut stderr)
        .expect("read daemon stderr after Stop");
    assert!(
        status.success(),
        "daemon Stop completed with {status}: {stderr}"
    );
}

fn wait_for_task_completion(runtime: &DaemonRuntimeInfo, expected_task_id: &str) {
    let mut stream = connect_authenticated(runtime);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set completion read timeout");
    write_frame(
        &mut stream,
        &DaemonRequest::TaskSubscribe {
            task_id: expected_task_id.to_string(),
            replay_last: usize::MAX,
            after_seq: None,
        },
    )
    .expect("subscribe to recovered task");
    let response: DaemonResponse = read_frame(&mut stream).expect("read subscription ack");
    assert!(matches!(
        response,
        DaemonResponse::TaskSubscribeAck {
            ref task_id,
            ..
        } if task_id == expected_task_id
    ));
    loop {
        let frame: DaemonEventFrame = read_frame(&mut stream).expect("read recovered task event");
        if frame.event.kind == "task_completed" {
            return;
        }
    }
}

#[test]
fn crash_after_durable_replan_checkpoint_child() {
    let Some(root) = std::env::var_os("PACKET28_REPLAN_CHECKPOINT_CRASH_ROOT") else {
        return;
    };
    let mut lifecycle = TaskLifecycle::Idle;
    assert!(
        lifecycle.request_replan().expect("request durable replan"),
        "idle task should schedule one sequence run"
    );
    let registry = TaskRegistry {
        tasks: BTreeMap::from([(
            "durable-replan".to_string(),
            TaskRecord {
                task_id: "durable-replan".to_string(),
                lifecycle,
                sequence_present: true,
                sequence: Some(KernelSequenceRequest {
                    steps: vec![KernelStepRequest {
                        id: "correlate".to_string(),
                        target: "contextq.correlate".to_string(),
                        ..KernelStepRequest::default()
                    }],
                    ..KernelSequenceRequest::default()
                }),
                ..TaskRecord::default()
            },
        )]),
    };
    save_task_watch_registry_checkpoint(Path::new(&root), &registry, &WatchRegistry::default())
        .expect("checkpoint queued replan");

    // This is the crash window in watch processing: request_replan() and its
    // registry checkpoint are durable, but run_sequence_for_task() has not run.
    std::process::exit(86);
}

#[test]
fn daemon_restart_executes_a_durable_queued_replan_exactly_once() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("crash_after_durable_replan_checkpoint_child")
        .env("PACKET28_REPLAN_CHECKPOINT_CRASH_ROOT", workspace.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn crash-window child");
    assert_eq!(
        output.status.code(),
        Some(86),
        "crash-window child failed unexpectedly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        load_task_registry(workspace.path())
            .expect("load queued crash checkpoint")
            .tasks["durable-replan"]
            .lifecycle,
        TaskLifecycle::ReplanPending
    );

    let mut first_daemon = spawn_daemon(workspace.path());
    let first_runtime = wait_for_ready(&mut first_daemon, workspace.path());
    wait_for_task_completion(&first_runtime, "durable-replan");
    stop_and_wait(&mut first_daemon, &first_runtime);
    let first_registry = load_task_registry(workspace.path()).expect("load recovered task");
    assert_eq!(
        first_registry.tasks["durable-replan"].lifecycle,
        TaskLifecycle::Idle
    );
    assert!(first_registry.tasks["durable-replan"]
        .last_completed_at_unix
        .is_some());
    let first_events =
        load_task_events(workspace.path(), "durable-replan").expect("load recovered events");
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame.event.kind == "task_started")
            .count(),
        1
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame.event.kind == "task_completed")
            .count(),
        1
    );

    let mut second_daemon = spawn_daemon(workspace.path());
    let second_runtime = wait_for_ready(&mut second_daemon, workspace.path());
    stop_and_wait(&mut second_daemon, &second_runtime);
    let second_events =
        load_task_events(workspace.path(), "durable-replan").expect("load events after restart");
    assert_eq!(
        second_events
            .iter()
            .filter(|frame| frame.event.kind == "task_started")
            .count(),
        1
    );
    assert_eq!(
        load_task_registry(workspace.path())
            .expect("load task after idempotent restart")
            .tasks["durable-replan"]
            .lifecycle,
        TaskLifecycle::Idle
    );
}

#[test]
fn daemon_restart_replays_a_durably_claimed_replan_after_second_crash() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let registry = TaskRegistry {
        tasks: BTreeMap::from([(
            "claimed-replan".to_string(),
            TaskRecord {
                task_id: "claimed-replan".to_string(),
                lifecycle: TaskLifecycle::RunningRecoveredReplan,
                sequence_present: true,
                sequence: Some(KernelSequenceRequest {
                    steps: vec![KernelStepRequest {
                        id: "correlate".to_string(),
                        target: "contextq.correlate".to_string(),
                        ..KernelStepRequest::default()
                    }],
                    ..KernelSequenceRequest::default()
                }),
                ..TaskRecord::default()
            },
        )]),
    };
    save_task_watch_registry_checkpoint(workspace.path(), &registry, &WatchRegistry::default())
        .expect("checkpoint exact post-claim crash image");

    let mut first_daemon = spawn_daemon(workspace.path());
    let first_runtime = wait_for_ready(&mut first_daemon, workspace.path());
    wait_for_task_completion(&first_runtime, "claimed-replan");
    stop_and_wait(&mut first_daemon, &first_runtime);

    let first_registry = load_task_registry(workspace.path()).expect("load replayed task");
    assert_eq!(
        first_registry.tasks["claimed-replan"].lifecycle,
        TaskLifecycle::Idle
    );
    let first_events =
        load_task_events(workspace.path(), "claimed-replan").expect("load replay events");
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame.event.kind == "task_started")
            .count(),
        1
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame.event.kind == "task_completed")
            .count(),
        1
    );

    let mut second_daemon = spawn_daemon(workspace.path());
    let second_runtime = wait_for_ready(&mut second_daemon, workspace.path());
    assert_eq!(
        load_task_registry(workspace.path())
            .expect("load task at second readiness")
            .tasks["claimed-replan"]
            .lifecycle,
        TaskLifecycle::Idle
    );
    let second_events =
        load_task_events(workspace.path(), "claimed-replan").expect("load second-start events");
    assert_eq!(second_events.len(), first_events.len());
    stop_and_wait(&mut second_daemon, &second_runtime);
    assert_eq!(
        load_task_events(workspace.path(), "claimed-replan")
            .expect("load final replay events")
            .len(),
        first_events.len()
    );
}

#[test]
fn malformed_pending_replan_is_durably_idle_before_readiness() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let registry = TaskRegistry {
        tasks: BTreeMap::from([(
            "malformed-pending".to_string(),
            TaskRecord {
                task_id: "malformed-pending".to_string(),
                lifecycle: TaskLifecycle::ReplanPending,
                sequence_present: true,
                sequence: None,
                ..TaskRecord::default()
            },
        )]),
    };
    save_task_watch_registry_checkpoint(workspace.path(), &registry, &WatchRegistry::default())
        .expect("checkpoint malformed pending task");
    let mut daemon = spawn_daemon(workspace.path());
    let runtime = wait_for_ready(&mut daemon, workspace.path());
    let recovered = load_task_registry(workspace.path()).expect("read ready checkpoint");
    assert_eq!(
        recovered.tasks["malformed-pending"].lifecycle,
        TaskLifecycle::Idle
    );
    assert!(recovered.tasks["malformed-pending"]
        .last_error
        .as_deref()
        .unwrap()
        .contains("downgraded to idle"));
    stop_and_wait(&mut daemon, &runtime);
    let mut restarted = spawn_daemon(workspace.path());
    let runtime = wait_for_ready(&mut restarted, workspace.path());
    stop_and_wait(&mut restarted, &runtime);
    assert_eq!(
        load_task_registry(workspace.path()).unwrap().tasks["malformed-pending"].lifecycle,
        TaskLifecycle::Idle
    );
}

#[test]
fn live_recovered_cancellation_blocks_readiness_until_process_group_is_quiescent() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let mut process = ProcessGroupGuard::spawn();
    assert!(process.exists(), "isolated process group did not start");
    let watch_id = "cancel-watch".to_string();
    let registry = TaskRegistry {
        tasks: BTreeMap::from([(
            "cancelling-live-agent".to_string(),
            TaskRecord {
                task_id: "cancelling-live-agent".to_string(),
                lifecycle: TaskLifecycle::Cancelling { was_running: true },
                watch_ids: vec![watch_id.clone()],
                latest_agent_pid: Some(process.pid()),
                latest_agent_started_at_unix: Some(7),
                latest_agent_completed_at_unix: None,
                ..TaskRecord::default()
            },
        )]),
    };
    let watches = WatchRegistry {
        watches: vec![WatchRegistration {
            watch_id,
            spec: WatchSpec {
                task_id: "cancelling-live-agent".to_string(),
                ..WatchSpec::default()
            },
            active: true,
            ..WatchRegistration::default()
        }],
    };
    save_task_watch_registry_checkpoint(workspace.path(), &registry, &watches)
        .expect("checkpoint live cancellation residue");
    let tasks_before =
        serde_json::to_value(&registry).expect("encode cancellation task checkpoint");
    let watches_before =
        serde_json::to_value(&watches).expect("encode cancellation watch checkpoint");

    let mut blocked_daemon = spawn_daemon(workspace.path());
    let stderr = wait_for_startup_failure(&mut blocked_daemon, workspace.path());
    assert!(stderr.contains("has a live recovered agent process group"));
    assert!(
        process.exists(),
        "startup must not signal an unauthenticated pid"
    );
    assert_eq!(
        serde_json::to_value(
            load_task_registry(workspace.path()).expect("reload blocked task checkpoint")
        )
        .expect("encode blocked task checkpoint"),
        tasks_before
    );
    assert_eq!(
        serde_json::to_value(
            load_watch_registry(workspace.path()).expect("reload blocked watch checkpoint")
        )
        .expect("encode blocked watch checkpoint"),
        watches_before
    );

    process.terminate_and_reap();
    assert!(
        !process.exists(),
        "isolated process group remained after reap"
    );

    let mut first_daemon = spawn_daemon(workspace.path());
    let first_runtime = wait_for_ready(&mut first_daemon, workspace.path());
    let recovered = load_task_registry(workspace.path()).expect("load completed cancellation");
    let recovered_task = &recovered.tasks["cancelling-live-agent"];
    assert_eq!(recovered_task.lifecycle, TaskLifecycle::Cancelled);
    assert!(recovered_task.watch_ids.is_empty());
    assert!(recovered_task.latest_agent_completed_at_unix.is_some());
    assert!(load_watch_registry(workspace.path())
        .expect("load cleaned watch checkpoint")
        .watches
        .is_empty());
    assert!(load_task_events(workspace.path(), "cancelling-live-agent")
        .expect("load cancellation events")
        .is_empty());
    stop_and_wait(&mut first_daemon, &first_runtime);

    let durable_after_first = serde_json::to_value(
        load_task_registry(workspace.path()).expect("reload completed cancellation"),
    )
    .expect("encode completed cancellation");
    let mut second_daemon = spawn_daemon(workspace.path());
    let second_runtime = wait_for_ready(&mut second_daemon, workspace.path());
    assert_eq!(
        serde_json::to_value(
            load_task_registry(workspace.path()).expect("load idempotent cancellation restart")
        )
        .expect("encode idempotent cancellation restart"),
        durable_after_first
    );
    stop_and_wait(&mut second_daemon, &second_runtime);
}

#[test]
fn live_recovered_idle_launch_blocks_readiness_and_is_reconciled_after_quiescence() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let mut process = ProcessGroupGuard::spawn();
    assert!(process.exists(), "isolated process group did not start");
    let registry = TaskRegistry {
        tasks: BTreeMap::from([(
            "idle-live-agent".to_string(),
            TaskRecord {
                task_id: "idle-live-agent".to_string(),
                lifecycle: TaskLifecycle::Idle,
                latest_agent_pid: Some(process.pid()),
                latest_agent_started_at_unix: Some(7),
                latest_agent_completed_at_unix: None,
                ..TaskRecord::default()
            },
        )]),
    };
    save_task_watch_registry_checkpoint(workspace.path(), &registry, &WatchRegistry::default())
        .expect("checkpoint live idle launch residue");
    let tasks_before = serde_json::to_value(&registry).expect("encode idle task checkpoint");

    let mut blocked_daemon = spawn_daemon(workspace.path());
    let stderr = wait_for_startup_failure(&mut blocked_daemon, workspace.path());
    assert!(stderr.contains("has a live recovered agent process group"));
    assert!(
        process.exists(),
        "startup must not signal an unauthenticated pid"
    );
    assert_eq!(
        serde_json::to_value(
            load_task_registry(workspace.path()).expect("reload blocked idle checkpoint")
        )
        .expect("encode blocked idle checkpoint"),
        tasks_before
    );

    process.terminate_and_reap();
    let mut first_daemon = spawn_daemon(workspace.path());
    let first_runtime = wait_for_ready(&mut first_daemon, workspace.path());
    let recovered = load_task_registry(workspace.path()).expect("load reconciled idle launch");
    let recovered_task = &recovered.tasks["idle-live-agent"];
    assert_eq!(recovered_task.lifecycle, TaskLifecycle::Idle);
    assert!(recovered_task.latest_agent_completed_at_unix.is_some());
    assert!(recovered_task
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("was interrupted before packet28d restart")));
    stop_and_wait(&mut first_daemon, &first_runtime);

    let durable_after_first =
        serde_json::to_value(recovered).expect("encode reconciled idle launch");
    let mut second_daemon = spawn_daemon(workspace.path());
    let second_runtime = wait_for_ready(&mut second_daemon, workspace.path());
    assert_eq!(
        serde_json::to_value(
            load_task_registry(workspace.path()).expect("load idempotent idle restart")
        )
        .expect("encode idempotent idle restart"),
        durable_after_first
    );
    stop_and_wait(&mut second_daemon, &second_runtime);
}

#[test]
fn daemon_restart_durably_reconciles_crash_residue_once() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let mut registry = TaskRegistry {
        tasks: BTreeMap::new(),
    };
    registry.tasks.insert(
        "crash-residue".to_string(),
        TaskRecord {
            task_id: "crash-residue".to_string(),
            lifecycle: TaskLifecycle::Running,
            last_started_at_unix: Some(7),
            ..TaskRecord::default()
        },
    );
    registry.tasks.insert(
        "cancelling-residue".to_string(),
        TaskRecord {
            task_id: "cancelling-residue".to_string(),
            lifecycle: TaskLifecycle::Cancelling { was_running: true },
            last_error: Some("cancellation was requested before the crash".to_string()),
            ..TaskRecord::default()
        },
    );
    registry.tasks.insert(
        "already-cancelled".to_string(),
        TaskRecord {
            task_id: "already-cancelled".to_string(),
            lifecycle: TaskLifecycle::Cancelled,
            last_completed_at_unix: Some(11),
            last_error: Some("original terminal cancellation history".to_string()),
            ..TaskRecord::default()
        },
    );
    save_task_registry(workspace.path(), &registry).expect("persist crash residue");

    let mut first_daemon = spawn_daemon(workspace.path());
    let first_runtime = wait_for_ready(&mut first_daemon, workspace.path());
    let first_recovery = load_task_registry(workspace.path()).expect("load first recovery");
    let recovered_task = &first_recovery.tasks["crash-residue"];
    assert_eq!(recovered_task.lifecycle, TaskLifecycle::Idle);
    assert!(
        recovered_task
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("Running")),
        "recovered task did not retain its interrupted lifecycle evidence"
    );
    let recovered_cancellation = &first_recovery.tasks["cancelling-residue"];
    assert_eq!(recovered_cancellation.lifecycle, TaskLifecycle::Cancelled);
    assert!(recovered_cancellation.last_completed_at_unix.is_some());
    assert!(
        recovered_cancellation
            .last_error
            .as_deref()
            .is_some_and(|error| {
                error.starts_with("cancellation was requested before the crash; ")
                    && error.contains("task cancellation completed by packet28d restart")
                    && error.contains("Cancelling")
            }),
        "recovered cancellation did not retain durable cancellation evidence"
    );
    assert!(
        load_task_events(workspace.path(), "cancelling-residue")
            .expect("load recovered cancellation events")
            .is_empty(),
        "restart cancellation evidence belongs to the atomic terminal registry record"
    );
    let terminal_task = &first_recovery.tasks["already-cancelled"];
    assert_eq!(terminal_task.lifecycle, TaskLifecycle::Cancelled);
    assert_eq!(terminal_task.last_completed_at_unix, Some(11));
    assert_eq!(
        terminal_task.last_error.as_deref(),
        Some("original terminal cancellation history")
    );
    stop_and_wait(&mut first_daemon, &first_runtime);

    let durable_after_first =
        serde_json::to_value(load_task_registry(workspace.path()).expect("reload first recovery"))
            .expect("encode first recovery");
    let mut second_daemon = spawn_daemon(workspace.path());
    let second_runtime = wait_for_ready(&mut second_daemon, workspace.path());
    let durable_after_second = serde_json::to_value(
        load_task_registry(workspace.path()).expect("load idempotent restart"),
    )
    .expect("encode second recovery");

    assert_eq!(durable_after_second, durable_after_first);
    stop_and_wait(&mut second_daemon, &second_runtime);
}

#[test]
fn daemon_startup_fences_missing_and_short_history_before_ready_and_accepts_successor_event() {
    for missing in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let task_id = "lost-history";
        let mut record = TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        };
        save_task_watch_registry_checkpoint(
            root,
            &TaskRegistry {
                tasks: BTreeMap::from([(task_id.to_string(), record.clone())]),
            },
            &WatchRegistry::default(),
        )
        .unwrap();
        append_next_task_event(
            root,
            task_id,
            &DaemonEvent {
                kind: "before-loss".to_string(),
                occurred_at_unix: 1,
                data: serde_json::Value::Null,
            },
        )
        .unwrap();
        record.last_event_seq = 9;
        save_task_watch_registry_checkpoint(
            root,
            &TaskRegistry {
                tasks: BTreeMap::from([(task_id.to_string(), record)]),
            },
            &WatchRegistry::default(),
        )
        .unwrap();
        let canonical = task_event_log_path(root, &TaskStorageId::try_from(task_id).unwrap());
        let bytes = fs::read(&canonical).unwrap();
        if missing {
            fs::remove_file(&canonical).unwrap();
        }
        let mut daemon = spawn_daemon(root);
        let runtime = wait_for_ready(&mut daemon, root);
        let ready_registry = load_task_registry(root).unwrap();
        let predecessor = &ready_registry.tasks[task_id];
        let link = predecessor.superseded_by.as_ref().unwrap();
        assert_eq!(link.prior_last_event_seq, 9);
        assert_eq!(predecessor.lifecycle, TaskLifecycle::Cancelled);
        assert_eq!(
            ready_registry.tasks[&link.successor_task_id]
                .recovered_from
                .as_ref(),
            Some(link)
        );
        assert!(!canonical.exists());
        if !missing {
            let quarantine = canonical
                .parent()
                .unwrap()
                .join(link.quarantined_event_log.as_ref().unwrap());
            assert_eq!(fs::read(quarantine).unwrap(), bytes);
        }
        let mut stream = connect_authenticated(&runtime);
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write_frame(
            &mut stream,
            &DaemonRequest::BrokerWriteState {
                request: BrokerWriteStateRequest {
                    task_id: link.successor_task_id.clone(),
                    op: Some(BrokerWriteOp::Intention),
                    text: Some("continue after lost history".to_string()),
                    ..BrokerWriteStateRequest::default()
                },
            },
        )
        .unwrap();
        let response: DaemonResponse = read_frame(&mut stream).unwrap();
        assert!(
            matches!(response, DaemonResponse::BrokerWriteState { .. }),
            "{response:?}"
        );
        stop_and_wait(&mut daemon, &runtime);
        let frames = load_task_events(root, &link.successor_task_id).unwrap();
        assert_eq!(frames.first().unwrap().seq, 1);
        assert!(append_next_task_event(
            root,
            task_id,
            &DaemonEvent {
                kind: "late".to_string(),
                occurred_at_unix: 2,
                data: serde_json::Value::Null
            }
        )
        .is_err());
        assert!(!canonical.exists());
    }
}
