#[path = "support/daemon_lifecycle.rs"]
mod daemon_lifecycle;

use daemon_lifecycle::process_harness::{HarnessLimits, ProcessHarness, ProcessOutput};
use daemon_lifecycle::{ensure_packet28d_built, init_repo, suite_cmd, write_repo_fixture};
use serde_json::Value;
use std::fs;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tempfile::TempDir;

#[test]
#[cfg(unix)]
fn test_daemon_lifecycle_cli_stop_does_not_start_missing_daemon() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    assert!(!dir.path().join(".packet28").exists());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout("stopping\n");

    assert!(!dir.path().join(".packet28").exists());
}

#[test]
#[cfg(unix)]
fn test_daemon_lifecycle_cli_start_status_stop_cycle() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let status_output = suite_cmd()
        .args([
            "daemon",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).unwrap();
    let expected_root = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        status.get("workspace_root").and_then(Value::as_str),
        expected_root.to_str()
    );
    assert!(status.get("pid").and_then(Value::as_u64).unwrap() > 0);
    assert!(status.get("ready_at_unix").and_then(Value::as_u64).unwrap() > 0);
    assert!(status
        .get("log_path")
        .and_then(Value::as_str)
        .is_some_and(|path| Path::new(path).exists()));
    assert!(dir.path().join(".packet28/daemon/ready").exists());
    assert!(dir.path().join(".packet28/daemon/packet28d.log").exists());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_concurrent_daemon_clients_share_one_workspace_process() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    let clients = 16;
    let barrier = Arc::new(Barrier::new(clients));
    let root = Arc::new(dir.path().to_path_buf());
    let workers = (0..clients)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let root = Arc::clone(&root);
            std::thread::spawn(move || {
                Barrier::wait(&barrier);
                let mut command =
                    std::process::Command::new(assert_cmd::cargo::cargo_bin!("Packet28"));
                command.args([
                    "daemon",
                    "status",
                    "--root",
                    root.to_str().unwrap(),
                    "--json",
                ]);
                let output = ProcessHarness::run(
                    &mut command,
                    &[],
                    Duration::from_secs(45),
                    HarnessLimits::default(),
                )
                .expect("run lifecycle client within deadline");
                assert!(
                    output.status.success(),
                    "concurrent daemon client failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice::<Value>(&output.stdout)
                    .unwrap()
                    .get("pid")
                    .and_then(Value::as_u64)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();

    let pids = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert!(pids.iter().all(|pid| *pid == pids[0]));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_daemon_lifecycle_forced_tcp_stop_exits_and_releases_endpoint() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .env("PACKET28D_FORCE_TCP", "1")
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let status_output = suite_cmd()
        .args([
            "daemon",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).unwrap();
    let pid = i32::try_from(status.get("pid").and_then(Value::as_u64).unwrap()).unwrap();
    let endpoint = status.get("socket_path").and_then(Value::as_str).unwrap();
    let address = endpoint
        .strip_prefix("tcp://")
        .expect("forced TCP daemon did not publish a TCP endpoint")
        .to_string();
    let runtime_path = dir.path().join(".packet28/daemon/runtime.json");
    let runtime_mode = fs::metadata(&runtime_path).unwrap().permissions().mode() & 0o777;
    let runtime: Value = serde_json::from_slice(&fs::read(&runtime_path).unwrap()).unwrap();
    assert_eq!(runtime_mode, 0o600);
    assert!(runtime
        .get("transport_auth")
        .and_then(|auth| auth.get("secret"))
        .and_then(Value::as_str)
        .is_some_and(|secret| secret.len() == 64));
    assert!(status.get("transport_auth").is_none());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout("stopping\n");

    let started = std::time::Instant::now();
    while process_exists(pid) && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(pid),
        "forced TCP daemon process {pid} did not exit after Stop"
    );
    TcpListener::bind(&address)
        .unwrap_or_else(|error| panic!("TCP endpoint {address} was not released: {error}"));
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 performs a non-mutating process existence check for the
    // positive PID returned by the daemon status response.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[test]
#[cfg(unix)]
fn test_daemon_lifecycle_cli_index_rebuild_and_status() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let rebuild_output = suite_cmd()
        .args([
            "daemon",
            "index",
            "rebuild",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rebuild: Value = serde_json::from_slice(&rebuild_output).unwrap();
    assert_eq!(rebuild.get("accepted").and_then(Value::as_bool), Some(true));
    assert_eq!(rebuild.get("full").and_then(Value::as_bool), Some(true));

    let start = std::time::Instant::now();
    let mut ready = false;
    while start.elapsed() < Duration::from_secs(5) {
        let status_output = suite_cmd()
            .args([
                "daemon",
                "index",
                "status",
                "--root",
                dir.path().to_str().unwrap(),
                "--json",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let status: Value = serde_json::from_slice(&status_output).unwrap();
        if status.get("ready").and_then(Value::as_bool) == Some(true) {
            ready = true;
            assert!(
                status
                    .get("manifest")
                    .and_then(|manifest| manifest.get("indexed_files"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
            );
            assert!(
                status
                    .get("manifest")
                    .and_then(|manifest| manifest.get("regex_weight_table_version"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
            );
            assert_eq!(
                status
                    .get("manifest")
                    .and_then(|manifest| manifest.get("regex_status"))
                    .and_then(Value::as_str),
                Some("ready")
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "expected daemon index to become ready");

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn uninstall_stops_workspace_services_and_stale_hooks_cannot_restart_them() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());
    suite_cmd()
        .env("HOME", home.path())
        .args(["setup", "--root", root, "--runtime", "claude", "--yes"])
        .assert()
        .success();
    let config_path = dir.path().join(".packet28/daemon/hook-runtime-v1.json");
    let config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    let port = config["http_hook_port"].as_u64().unwrap() as u16;
    assert!(TcpListener::bind(("127.0.0.1", port)).is_err());

    let output = suite_cmd()
        .env("HOME", home.path())
        .args(["uninstall", "--root", root])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("Claude HTTP hook server: stopped"),
        "{output}"
    );
    assert!(output.contains("packet28d: stopped"), "{output}");
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    for event in ["SessionStart", "SubagentStart", "SubagentStop", "Stop"] {
        suite_cmd()
            .env("HOME", home.path())
            .args(["hook", "claude", "--root", root])
            .write_stdin(
                serde_json::json!({"hook_event_name": event, "session_id": "stale"}).to_string(),
            )
            .assert()
            .success()
            .stdout("");
    }
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    let stopped = std::time::Instant::now();
    while dir.path().join(".packet28/daemon/runtime.json").exists()
        && stopped.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.path().join(".packet28/daemon/runtime.json").exists());
    let settings: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".claude/settings.json")).unwrap())
            .unwrap();
    assert!(settings.get("hooks").is_none());
    let mcp: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".mcp.json")).unwrap()).unwrap();
    assert!(mcp["mcpServers"].get("packet28").is_none());
    suite_cmd()
        .env("HOME", home.path())
        .args(["uninstall", "--root", root])
        .assert()
        .success();
}

/// Bound for fixture state transitions. It limits a hung test; no assertion
/// depends on how much of it elapses.
#[cfg(unix)]
const FIXTURE_SIGNAL_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(unix)]
fn wait_for_fixture_signal(description: &str, mut signaled: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !signaled() {
        assert!(
            started.elapsed() < FIXTURE_SIGNAL_TIMEOUT,
            "fixture signal did not arrive: {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn daemon_instance_is_held(root: &Path) -> bool {
    match packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(root) {
        Ok(lease) => {
            drop(lease);
            false
        }
        Err(packet28_daemon_core::DaemonCoreError::DaemonInstanceAlreadyRunning { .. }) => true,
        Err(error) => panic!("fixture daemon authority check failed: {error}"),
    }
}

#[cfg(unix)]
fn daemon_runtime_pid(root: &Path) -> Option<u64> {
    let bytes = fs::read(root.join(".packet28/daemon/runtime.json")).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["pid"].as_u64()
}

/// Bound for one lifecycle CLI client. It exceeds the CLI's default stop
/// timeout so a client always reports its own result first.
#[cfg(unix)]
const LIFECYCLE_CLIENT_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(unix)]
fn lifecycle_command(args: &[&str], envs: &[(&str, &str)]) -> std::process::Command {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("Packet28"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}

/// Spawns a lifecycle client that the harness kills if a failing test abandons
/// it. A daemon the client starts detaches into its own session, so the
/// fixture stops daemons through the CLI rather than the harness.
#[cfg(unix)]
fn spawn_lifecycle_client(args: &[&str], envs: &[(&str, &str)]) -> ProcessHarness {
    ProcessHarness::spawn(&mut lifecycle_command(args, envs), HarnessLimits::default())
        .expect("spawn lifecycle client")
}

#[cfg(unix)]
fn run_lifecycle_client(args: &[&str], envs: &[(&str, &str)]) -> ProcessOutput {
    ProcessHarness::run(
        &mut lifecycle_command(args, envs),
        &[],
        LIFECYCLE_CLIENT_TIMEOUT,
        HarnessLimits::default(),
    )
    .expect("run lifecycle client within deadline")
}

#[cfg(unix)]
fn client_is_running(client: &mut ProcessHarness) -> bool {
    client.is_running().expect("poll lifecycle client")
}

#[cfg(unix)]
fn wait_for_client(client: &mut ProcessHarness) -> ProcessOutput {
    client
        .wait(LIFECYCLE_CLIENT_TIMEOUT)
        .expect("lifecycle client finished within deadline")
}

/// A live daemon whose shutdown is held open by a real checkpoint flock.
///
/// The fixture admits a task intention while the watch-registry checkpoint
/// lock is owned by the test, so daemon shutdown must wait for that lock before
/// it can publish, remove runtime files, and release its instance lease.
///
/// Admission is observed through the daemon's in-memory registry revision,
/// not the write response: a debounced checkpoint may already be blocked on
/// the held lock, which delays the response but equally delays shutdown.
#[cfg(unix)]
struct DelayedShutdownFixture {
    dir: TempDir,
    checkpoint_lock: Option<fs::File>,
    // Held open so the admitted write is never cancelled by a closed client.
    intention_stream: Option<packet28_daemon_client::transport::DaemonStream>,
    original_pid: u64,
}

/// Daemon shutdown grace for the fixture. The held checkpoint lock, rather
/// than the grace deadline, must decide when the daemon finishes.
#[cfg(unix)]
const FIXTURE_SHUTDOWN_GRACE_MS: &str = "120000";

#[cfg(unix)]
impl DelayedShutdownFixture {
    fn start() -> Self {
        use std::os::fd::AsRawFd as _;

        ensure_packet28d_built();
        let dir = TempDir::new().unwrap();
        write_repo_fixture(dir.path());
        init_repo(dir.path());
        let daemon_dir = dir.path().join(".packet28/daemon");
        fs::create_dir_all(&daemon_dir).unwrap();
        fs::write(
            daemon_dir.join("task-registry-v1.json"),
            serde_json::json!({"tasks": {"task": {"task_id": "task", "last_event_seq": 0}}})
                .to_string(),
        )
        .unwrap();
        suite_cmd()
            .env("PACKET28D_SHUTDOWN_GRACE_MS", FIXTURE_SHUTDOWN_GRACE_MS)
            .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
            .assert()
            .success();
        let original_pid = daemon_runtime_pid(dir.path()).expect("started daemon runtime pid");

        let checkpoint_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(daemon_dir.join(".watch-registry-v1.json.lock"))
            .unwrap();
        // SAFETY: flock only takes an advisory lock on the descriptor owned by
        // `checkpoint_lock`, which stays open for the duration of the call.
        let locked = unsafe { libc::flock(checkpoint_lock.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(locked, 0, "{}", std::io::Error::last_os_error());
        let mut fixture = Self {
            dir,
            checkpoint_lock: Some(checkpoint_lock),
            intention_stream: None,
            original_pid,
        };
        fixture.admit_intention();
        fixture
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn root_arg(&self) -> &str {
        self.dir.path().to_str().unwrap()
    }

    fn registry_revision(&self) -> u64 {
        let output = suite_cmd()
            .args(["daemon", "status", "--root", self.root_arg(), "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<Value>(&output).unwrap()["registry_revision"]["revision"]
            .as_u64()
            .expect("fixture daemon reports a registry revision")
    }

    fn admit_intention(&mut self) {
        use packet28_daemon_protocol::broker::{BrokerWriteOp, BrokerWriteStateRequest};
        use packet28_daemon_protocol::message::DaemonRequest;

        let before = self.registry_revision();
        let mut stream =
            packet28_daemon_client::transport::connect(self.root(), Duration::from_secs(10))
                .unwrap();
        packet28_daemon_protocol::frame::write_frame(
            &mut stream,
            &DaemonRequest::BrokerWriteState {
                request: BrokerWriteStateRequest {
                    task_id: "task".to_string(),
                    op: Some(BrokerWriteOp::Intention),
                    text: Some("fixture first objective".to_string()),
                    ..BrokerWriteStateRequest::default()
                },
            },
        )
        .unwrap();
        self.intention_stream = Some(stream);
        wait_for_fixture_signal("daemon admitted the fixture intention", || {
            self.registry_revision() > before
        });
    }

    fn wait_for_shutdown_started(&self) {
        let ready = self.root().join(".packet28/daemon/ready");
        wait_for_fixture_signal("daemon withdrew readiness", || !ready.exists());
    }

    fn release_checkpoint_lock(&mut self) {
        // Closing the descriptor releases the flock.
        drop(self.checkpoint_lock.take());
    }

    fn wait_for_instance_release(&self) {
        wait_for_fixture_signal("daemon released its instance", || {
            !daemon_instance_is_held(self.root())
        });
    }
}

#[cfg(unix)]
impl Drop for DelayedShutdownFixture {
    fn drop(&mut self) {
        self.release_checkpoint_lock();
        drop(self.intention_stream.take());
        let _ = suite_cmd()
            .args(["daemon", "stop", "--root", self.root_arg()])
            .timeout(Duration::from_secs(30))
            .output();
    }
}

#[test]
#[cfg(unix)]
fn stop_waits_for_daemon_authority_release_and_concurrent_start_succeeds() {
    let mut fixture = DelayedShutdownFixture::start();
    let root = fixture.root_arg().to_string();

    let mut stop = spawn_lifecycle_client(&["daemon", "stop", "--root", &root], &[]);
    fixture.wait_for_shutdown_started();
    let mut start = spawn_lifecycle_client(&["daemon", "start", "--root", &root], &[]);

    // While the original daemon still owns the workspace, neither lifecycle
    // command may report completion. The window only bounds observation.
    let observed = std::time::Instant::now();
    while observed.elapsed() < Duration::from_millis(750) {
        assert!(daemon_instance_is_held(fixture.root()));
        for (name, client) in [("stop", &mut stop), ("start", &mut start)] {
            if !client_is_running(client) {
                let output = wait_for_client(client);
                panic!(
                    "daemon {name} completed with {} while the stopping daemon still owned \
                     the workspace; stdout={:?} stderr={:?}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(daemon_instance_is_held(fixture.root()));
    assert!(
        fixture
            .root()
            .join(".packet28/daemon/runtime.json")
            .exists(),
        "runtime metadata must stay in place while the stopping daemon owns it"
    );

    fixture.release_checkpoint_lock();
    let stop_output = wait_for_client(&mut stop);
    assert!(
        stop_output.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop_output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&stop_output.stdout), "stopping\n");
    // Completion means the original daemon finished cleanup; any runtime
    // metadata now belongs to the replacement started by the waiting client.
    assert_ne!(
        daemon_runtime_pid(fixture.root()),
        Some(fixture.original_pid)
    );

    let start_output = wait_for_client(&mut start);
    assert!(
        start_output.status.success(),
        "concurrent start failed: stdout={} stderr={}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
    let replacement = daemon_runtime_pid(fixture.root()).expect("replacement runtime pid");
    assert_ne!(replacement, fixture.original_pid);
    assert!(daemon_instance_is_held(fixture.root()));
    let log = fs::read_to_string(fixture.root().join(".packet28/daemon/packet28d.log")).unwrap();
    assert!(
        !log.contains("another Packet28 daemon already owns"),
        "a replacement daemon was spawned while the original owned the workspace:\n{log}"
    );

    // A completed stop of a live daemon removes its runtime files and releases
    // authority, so an immediate start succeeds without retrying.
    suite_cmd()
        .args(["daemon", "stop", "--root", &root])
        .assert()
        .success()
        .stdout("stopping\n");
    assert!(!daemon_instance_is_held(fixture.root()));
    assert!(!fixture
        .root()
        .join(".packet28/daemon/runtime.json")
        .exists());
    suite_cmd()
        .args(["daemon", "start", "--root", &root])
        .assert()
        .success();
    assert_ne!(daemon_runtime_pid(fixture.root()), Some(replacement));
}

#[test]
#[cfg(unix)]
fn stop_timeout_fails_without_touching_runtime_files_of_a_live_owner() {
    let mut fixture = DelayedShutdownFixture::start();
    let root = fixture.root_arg().to_string();
    let socket = PathBuf::from(
        serde_json::from_slice::<Value>(
            &fs::read(fixture.root().join(".packet28/daemon/runtime.json")).unwrap(),
        )
        .unwrap()["socket_path"]
            .as_str()
            .unwrap(),
    );
    assert!(socket.exists());
    let short_timeout = [("PACKET28_DAEMON_STOP_TIMEOUT_MS", "200")];

    let stop = run_lifecycle_client(&["daemon", "stop", "--root", &root], &short_timeout);
    assert!(!stop.status.success(), "stop reported success while held");
    let stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stderr.contains("did not release workspace authority"),
        "missing timeout diagnostic: {stderr}"
    );
    fixture.wait_for_shutdown_started();
    assert!(daemon_instance_is_held(fixture.root()));

    let start = run_lifecycle_client(&["daemon", "start", "--root", &root], &short_timeout);
    assert!(!start.status.success(), "start succeeded while held");
    let stderr = String::from_utf8_lossy(&start.stderr);
    assert!(
        stderr.contains("did not release workspace authority"),
        "missing start timeout diagnostic: {stderr}"
    );
    // Neither timed-out client removed state owned by the live daemon.
    assert!(socket.exists(), "client removed a live daemon socket");
    assert_eq!(
        daemon_runtime_pid(fixture.root()),
        Some(fixture.original_pid)
    );
    let log = fs::read_to_string(fixture.root().join(".packet28/daemon/packet28d.log")).unwrap();
    assert!(
        !log.contains("another Packet28 daemon already owns"),
        "a replacement daemon was spawned while the original owned the workspace:\n{log}"
    );

    fixture.release_checkpoint_lock();
    fixture.wait_for_instance_release();
    suite_cmd()
        .args(["daemon", "start", "--root", &root])
        .assert()
        .success();
    assert_ne!(
        daemon_runtime_pid(fixture.root()),
        Some(fixture.original_pid)
    );
}

#[test]
#[cfg(unix)]
fn corrupt_daemon_instance_lock_fails_closed_for_stop_and_start() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let daemon_dir = dir.path().join(".packet28/daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::write(dir.path().join("lock-target"), b"").unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("lock-target"),
        daemon_dir.join(".daemon-instance.lock"),
    )
    .unwrap();
    fs::write(daemon_dir.join("ready"), b"1\n").unwrap();
    let root = dir.path().to_str().unwrap();

    for command in ["stop", "start"] {
        let output = suite_cmd()
            .args(["daemon", command, "--root", root])
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "daemon {command} accepted a corrupt instance lock"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("instance authority"),
            "daemon {command} did not report the lock failure: {stderr}"
        );
    }
    assert!(daemon_dir.join("ready").exists());
    assert!(!daemon_dir.join("packet28d.log").exists());
}
