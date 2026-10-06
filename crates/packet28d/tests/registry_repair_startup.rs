#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use packet28_daemon_core::storage::{
    inspect_task_watch_registry_checkpoint_repair, repair_task_watch_registry_checkpoint,
    save_task_watch_registry_checkpoint, RegistryCheckpointAuthority,
    RegistryCheckpointRepairStatus,
};
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse, DaemonRuntimeInfo};
use packet28_daemon_protocol::paths::{ready_path, runtime_path, task_registry_path};
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistry};

fn request(runtime: &DaemonRuntimeInfo, request: &DaemonRequest) -> DaemonResponse {
    let mut stream = UnixStream::connect(&runtime.socket_path).expect("connect to packet28d");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set daemon response timeout");
    if let Some(auth) = runtime.transport_auth.as_ref() {
        write_frame(&mut stream, auth).expect("write daemon authentication");
        assert!(matches!(
            read_frame::<_, DaemonResponse>(&mut stream).expect("read authentication response"),
            DaemonResponse::Ack { ref message } if message == "authenticated"
        ));
    }
    write_frame(&mut stream, request).expect("write daemon request");
    read_frame(&mut stream).expect("read daemon response")
}

fn spawn(root: &Path, env: &[(&str, &str)]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_packet28d"));
    command
        .args(["serve", "--root"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    command.spawn().expect("spawn packet28d")
}

fn wait_for_exit(child: &mut Child) -> (Option<i32>, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("probe daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not exit before timeout"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("daemon stderr")
        .read_to_string(&mut stderr)
        .expect("read daemon stderr");
    (status.code(), stderr)
}

fn wait_for_ready(child: &mut Child, root: &Path) -> DaemonRuntimeInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ready_path(root).exists() {
            return serde_json::from_slice(&std::fs::read(runtime_path(root)).expect("runtime"))
                .expect("decode runtime metadata");
        }
        assert!(
            child.try_wait().expect("probe daemon").is_none(),
            "daemon exited before readiness"
        );
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn stop(child: &mut Child, runtime: &DaemonRuntimeInfo) {
    assert!(matches!(
        request(runtime, &DaemonRequest::Stop),
        DaemonResponse::Ack { ref message } if message == "stopping"
    ));
    assert!(child.wait().expect("join daemon").success());
}

fn task_status(runtime: &DaemonRuntimeInfo, task_id: &str) -> TaskRecord {
    match request(
        runtime,
        &DaemonRequest::TaskStatus {
            task_id: task_id.to_string(),
        },
    ) {
        DaemonResponse::TaskStatus { task: Some(task) } => task,
        other => panic!("unexpected task status for {task_id}: {other:?}"),
    }
}

fn seed(root: &Path) {
    let tasks = ["task", "healthy-neighbor"]
        .into_iter()
        .map(|task_id| {
            (
                task_id.to_string(),
                TaskRecord {
                    task_id: task_id.to_string(),
                    last_error: Some(format!("{task_id} committed state")),
                    ..TaskRecord::default()
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    save_task_watch_registry_checkpoint(root, &TaskRegistry { tasks }, &WatchRegistry::default())
        .expect("seed paired checkpoint");
}

fn assert_startup_refused(root: &Path, expected: &str) {
    let mut refused = spawn(root, &[]);
    let (code, stderr) = wait_for_exit(&mut refused);
    assert_ne!(code, Some(0), "daemon accepted unjournaled registry bytes");
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!ready_path(root).exists());
}

fn assert_tasks_survive_restart(root: &Path) {
    let mut daemon = spawn(root, &[]);
    let runtime = wait_for_ready(&mut daemon, root);
    for task_id in ["task", "healthy-neighbor"] {
        assert_eq!(
            task_status(&runtime, task_id).last_error.as_deref(),
            Some(format!("{task_id} committed state").as_str())
        );
    }
    stop(&mut daemon, &runtime);
}

#[test]
fn whitespace_edit_of_a_daemon_committed_registry_is_repaired_and_startup_resumes() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path();
    seed(root);
    // Let the daemon itself publish and commit the checkpoint at shutdown.
    let mut daemon = spawn(root, &[]);
    let runtime = wait_for_ready(&mut daemon, root);
    stop(&mut daemon, &runtime);
    let task_path = task_registry_path(root);
    let committed = std::fs::read(&task_path).expect("committed task registry");
    std::fs::write(&task_path, [committed.as_slice(), b" "].concat()).expect("edit registry");

    assert_startup_refused(root, "storage repair");

    let dry = inspect_task_watch_registry_checkpoint_repair(root).expect("dry run");
    assert_eq!(dry.status, RegistryCheckpointRepairStatus::Repairable);
    let applied = repair_task_watch_registry_checkpoint(root).expect("apply repair");
    assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
    assert_eq!(std::fs::read(&task_path).expect("restored"), committed);

    assert_tasks_survive_restart(root);
}

#[test]
fn precommit_watch_crash_with_edited_base_task_is_restored_to_the_committed_base() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path();
    seed(root);
    let mut fault = spawn(
        root,
        &[("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER", "watch")],
    );
    let (code, stderr) = wait_for_exit(&mut fault);
    assert_eq!(
        code,
        Some(86),
        "fault child did not stop after the watch phase: {stderr}"
    );
    let task_path = task_registry_path(root);
    let base = std::fs::read(&task_path).expect("base task registry");
    std::fs::write(&task_path, [base.as_slice(), b"\n"].concat()).expect("edit base registry");

    assert_startup_refused(root, "generations disagree");

    let applied = repair_task_watch_registry_checkpoint(root).expect("apply repair");
    assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
    assert_eq!(
        applied.authority,
        Some(RegistryCheckpointAuthority::PrecommitJournalBase)
    );
    assert_eq!(std::fs::read(&task_path).expect("restored"), base);

    assert_tasks_survive_restart(root);
}

#[test]
fn repair_refuses_while_the_daemon_is_live() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path();
    seed(root);
    let mut daemon = spawn(root, &[]);
    let runtime = wait_for_ready(&mut daemon, root);

    for result in [
        inspect_task_watch_registry_checkpoint_repair(root),
        repair_task_watch_registry_checkpoint(root),
    ] {
        let error = result.expect_err("live daemon must block offline repair");
        assert!(
            error.to_string().contains("exclusive task-store access"),
            "{error}"
        );
    }
    assert_eq!(
        task_status(&runtime, "task").last_error.as_deref(),
        Some("task committed state")
    );
    stop(&mut daemon, &runtime);
}
