use assert_cmd::Command;
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse};
use packet28_daemon_protocol::paths::{ready_path, runtime_path};
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

pub fn daemon_bin() -> PathBuf {
    static DAEMON_BIN: OnceLock<PathBuf> = OnceLock::new();
    DAEMON_BIN
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let workspace = manifest_dir
                .parent()
                .and_then(|path| path.parent())
                .expect("workspace root");
            let status = ProcessCommand::new("cargo")
                .args(["build", "-p", "packet28d"])
                .current_dir(workspace)
                .status()
                .expect("build packet28d");
            assert!(status.success(), "packet28d build failed");
            workspace.join("target/debug/packet28d")
        })
        .clone()
}

pub fn cli_with_daemon_env() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("p28"));
    command.env("CARGO_BIN_EXE_packet28d", daemon_bin());
    command
}

pub struct DaemonHandle {
    child: Child,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::zombie_processes)]
pub fn start_daemon(root: &Path) -> DaemonHandle {
    start_daemon_with_transport(root, false, None)
}

pub fn start_daemon_forced_tcp(root: &Path) -> DaemonHandle {
    start_daemon_with_transport(root, true, None)
}

pub fn start_daemon_with_env(root: &Path, envs: &[(&str, &str)]) -> DaemonHandle {
    start_daemon_with_transport_and_env(root, false, None, envs)
}

pub fn start_daemon_workspace_fallback(root: &Path) -> DaemonHandle {
    let temporary_root = root.join("daemon-temp");
    std::fs::create_dir(&temporary_root).unwrap();
    // SAFETY: `geteuid` has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    let unauthentic_socket_parent =
        temporary_root.join(format!("packet28d-sockets-{effective_uid}"));
    std::fs::create_dir(&unauthentic_socket_parent).unwrap();
    std::fs::set_permissions(
        &unauthentic_socket_parent,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    start_daemon_with_transport(root, false, Some(&temporary_root))
}

fn start_daemon_with_transport(
    root: &Path,
    force_tcp: bool,
    temporary_root: Option<&Path>,
) -> DaemonHandle {
    start_daemon_with_transport_and_env(root, force_tcp, temporary_root, &[])
}

fn start_daemon_with_transport_and_env(
    root: &Path,
    force_tcp: bool,
    temporary_root: Option<&Path>,
    envs: &[(&str, &str)],
) -> DaemonHandle {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut command = ProcessCommand::new(daemon_bin());
    command
        .args(["serve", "--root", canonical_root.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if force_tcp {
        command.env("PACKET28D_FORCE_TCP", "1");
    }
    if let Some(temporary_root) = temporary_root {
        command.env("TMPDIR", temporary_root);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if ready_path(&canonical_root).exists() && runtime_path(&canonical_root).exists() {
            return DaemonHandle { child };
        }
        if let Some(status) = child.try_wait().unwrap() {
            let (stdout, stderr) = child_output(&mut child);
            panic!(
                "packet28d exited early for {} with status {status}; stdout={stdout:?} stderr={stderr:?}",
                canonical_root.display()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let (stdout, stderr) = child_output(&mut child);
    panic!(
        "packet28d did not become ready for {}; stdout={stdout:?} stderr={stderr:?}",
        canonical_root.display()
    );
}

/// Sends protocol `Stop` through the daemon's discovered, authenticated
/// endpoint, which may be a per-user temporary socket or TCP.
pub fn stop_daemon(root: &Path) {
    if let Ok(stream) = packet28_daemon_client::transport::connect(root, Duration::from_secs(5)) {
        let reader_stream = stream.try_clone().unwrap();
        let mut writer = std::io::BufWriter::new(stream);
        let mut reader = std::io::BufReader::new(reader_stream);
        let _ = write_frame(&mut writer, &DaemonRequest::Stop);
        let _ = read_frame::<_, DaemonResponse>(&mut reader);
    }
}

fn child_output(child: &mut Child) -> (String, String) {
    let mut stderr = String::new();
    if let Some(mut stream) = child.stderr.take() {
        let _ = stream.read_to_string(&mut stderr);
    }
    let mut stdout = String::new();
    if let Some(mut stream) = child.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    (stdout, stderr)
}

/// Bound for fixture state transitions. It limits a hung test; no assertion
/// depends on how much of it elapses.
const FIXTURE_SIGNAL_TIMEOUT: Duration = Duration::from_secs(20);

pub fn wait_for_fixture_signal(description: &str, mut signaled: impl FnMut() -> bool) {
    let started = Instant::now();
    while !signaled() {
        assert!(
            started.elapsed() < FIXTURE_SIGNAL_TIMEOUT,
            "fixture signal did not arrive: {description}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reports whether `pid` names a live process this user may signal.
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs only existence and permission checks; no
    // pointers are passed and no signal is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

pub fn daemon_runtime_pid(root: &Path) -> Option<u32> {
    let bytes = std::fs::read(runtime_path(root)).ok()?;
    let runtime: packet28_daemon_protocol::message::DaemonRuntimeInfo =
        serde_json::from_slice(&bytes).ok()?;
    Some(runtime.pid)
}

/// Sends protocol `Stop` to a daemon started by p28 and waits until that
/// process has exited, so no detached daemon outlives its fixture.
pub fn stop_detached_daemon(root: &Path) {
    let Some(pid) = daemon_runtime_pid(root) else {
        return;
    };
    stop_daemon(root);
    wait_for_fixture_signal("detached daemon exited", || !process_is_alive(pid));
}

/// A live daemon whose shutdown is held open by a real checkpoint flock.
///
/// The fixture admits a task intention while the watch-registry checkpoint
/// lock is owned by the test, so daemon shutdown must wait for that lock before
/// it can publish, remove runtime files, and release its instance lease.
/// Admission is observed through the daemon's registry revision, not sleeps.
pub struct DelayedShutdownDaemon {
    root: PathBuf,
    daemon: Option<DaemonHandle>,
    checkpoint_lock: Option<std::fs::File>,
    // Held open so the admitted write is never cancelled by a closed client.
    intention_stream: Option<packet28_daemon_client::transport::DaemonStream>,
}

impl DelayedShutdownDaemon {
    pub fn start(root: &Path) -> Self {
        use std::os::fd::AsRawFd as _;

        let root = root.canonicalize().unwrap();
        let daemon_dir = root.join(".packet28/daemon");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("task-registry-v1.json"),
            serde_json::json!({"tasks": {"task": {"task_id": "task", "last_event_seq": 0}}})
                .to_string(),
        )
        .unwrap();
        // The held checkpoint lock, rather than the grace deadline, must
        // decide when the daemon finishes shutting down.
        let daemon = start_daemon_with_env(&root, &[("PACKET28D_SHUTDOWN_GRACE_MS", "120000")]);
        let checkpoint_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(daemon_dir.join(".watch-registry-v1.json.lock"))
            .unwrap();
        // SAFETY: flock only takes an advisory lock on the descriptor owned by
        // `checkpoint_lock`, which stays open for the duration of the call.
        let locked = unsafe { libc::flock(checkpoint_lock.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(locked, 0, "{}", std::io::Error::last_os_error());
        let mut fixture = Self {
            root,
            daemon: Some(daemon),
            checkpoint_lock: Some(checkpoint_lock),
            intention_stream: None,
        };
        fixture.admit_intention();
        fixture
    }

    pub fn original_pid(&self) -> u32 {
        self.daemon.as_ref().expect("original daemon").child.id()
    }

    fn registry_revision(&self) -> u64 {
        use packet28_daemon_protocol::registry::{
            DaemonRegistryRequestV1, DaemonRegistryResponseV1,
        };

        let stream =
            packet28_daemon_client::transport::connect(&self.root, Duration::from_secs(10))
                .unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut writer = std::io::BufWriter::new(stream);
        write_frame(&mut writer, &DaemonRegistryRequestV1::Status).unwrap();
        match read_frame(&mut reader).unwrap() {
            DaemonRegistryResponseV1::Status { status } => {
                status
                    .registry_revision
                    .expect("fixture daemon reports a registry revision")
                    .revision
            }
            other => panic!("unexpected registry status response: {other:?}"),
        }
    }

    fn admit_intention(&mut self) {
        use packet28_daemon_protocol::broker::{BrokerWriteOp, BrokerWriteStateRequest};

        let before = self.registry_revision();
        let mut stream =
            packet28_daemon_client::transport::connect(&self.root, Duration::from_secs(10))
                .unwrap();
        write_frame(
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

    /// Requests shutdown and waits until the daemon has withdrawn readiness.
    pub fn begin_shutdown(&self) {
        stop_daemon(&self.root);
        wait_for_fixture_signal("daemon withdrew readiness", || {
            !ready_path(&self.root).exists()
        });
    }

    pub fn original_is_running(&mut self) -> bool {
        let daemon = self.daemon.as_mut().expect("original daemon");
        daemon.child.try_wait().unwrap().is_none()
    }

    /// Releases the held checkpoint lock and waits for the original daemon to
    /// finish its shutdown.
    pub fn finish_shutdown(&mut self) {
        drop(self.checkpoint_lock.take());
        wait_for_fixture_signal("original daemon exited", || !self.original_is_running());
    }
}

impl Drop for DelayedShutdownDaemon {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock.
        drop(self.checkpoint_lock.take());
        drop(self.intention_stream.take());
        drop(self.daemon.take());
        // A replacement a p28 client started detaches into its own session.
        if let Some(pid) = daemon_runtime_pid(&self.root) {
            if process_is_alive(pid) {
                stop_detached_daemon(&self.root);
            }
        }
    }
}
