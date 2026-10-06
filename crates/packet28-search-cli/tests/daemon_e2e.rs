mod support;

#[path = "support/daemon.rs"]
mod daemon_support;

use daemon_support::{
    cli_with_daemon_env, daemon_bin, daemon_runtime_pid, daemon_status, process_is_alive,
    read_runtime, start_daemon, start_daemon_forced_tcp, start_daemon_workspace_fallback,
    stop_detached_daemon, wait_for_startup_admission, DelayedShutdownDaemon, GatedWorkspace,
};
use packet28_daemon_protocol::message::DaemonRuntimeInfo;
use packet28_daemon_protocol::paths::{log_path, runtime_path, workspace_socket_path};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use predicates::prelude::*;
use support::{cli, output, stderr_text, stdout_text, write_fixture};

#[test]
fn p28_supports_daemon_transport_for_subtree_roots() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["debug", "build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let daemon = start_daemon(workspace);

    cli()
        .current_dir(&subtree)
        .args([
            "Alpha",
            "--fixed-strings",
            "--transport",
            "daemon",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/lib.rs:1:pub struct Alpha;"))
        .stderr(predicate::str::contains("transport=daemon"))
        .stderr(predicate::str::contains("backend=indexed_regex"));

    drop(daemon);
}

#[test]
fn p28_uses_authenticated_forced_tcp_runtime_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["debug", "build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let daemon = start_daemon_forced_tcp(workspace);

    cli()
        .current_dir(&subtree)
        .args([
            "Alpha",
            "--fixed-strings",
            "--transport",
            "daemon",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/lib.rs:1:pub struct Alpha;"))
        .stderr(predicate::str::contains("transport=daemon"))
        .stderr(predicate::str::contains("backend=indexed_regex"));

    drop(daemon);
}

#[test]
fn p28_uses_authoritative_workspace_socket_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["debug", "build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let daemon = start_daemon_workspace_fallback(workspace);
    let runtime: DaemonRuntimeInfo =
        serde_json::from_slice(&fs::read(runtime_path(workspace)).unwrap()).unwrap();
    let canonical_workspace = workspace.canonicalize().unwrap();
    assert_eq!(
        runtime.socket_path,
        workspace_socket_path(&canonical_workspace).to_string_lossy()
    );

    cli()
        .current_dir(&subtree)
        .args([
            "Alpha",
            "--fixed-strings",
            "--transport",
            "daemon",
            "--stats",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/lib.rs:1:pub struct Alpha;"))
        .stderr(predicate::str::contains("transport=daemon"))
        .stderr(predicate::str::contains("backend=indexed_regex"));

    drop(daemon);
}

#[test]
fn indexed_engine_mode_is_enforced_over_daemon_transport() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["debug", "build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let daemon = start_daemon(workspace);

    cli()
        .current_dir(&subtree)
        .args([".+", "--engine", "indexed", "--transport", "daemon"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("planner could not derive"));

    drop(daemon);
}

#[test]
fn debug_guard_reports_daemon_fallback_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    cli()
        .args(["debug", "build", workspace.to_str().unwrap()])
        .assert()
        .success();

    let daemon = start_daemon(workspace);

    cli()
        .args([
            "debug",
            "guard",
            subtree.to_str().unwrap(),
            ".+",
            "--transport",
            "daemon",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("mode=fallback"))
        .stdout(predicate::str::contains("reason="));

    drop(daemon);
}

#[test]
fn p28_auto_starts_daemon_and_waits_for_indexed_backend() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    let subtree = workspace.join("crates/search-sample");
    write_fixture(&subtree);

    let first = output({
        let mut command = cli_with_daemon_env();
        command
            .current_dir(&subtree)
            .args(["Alpha", "--fixed-strings", "--stats"]);
        command
    });

    assert!(first.status.success());
    assert!(stdout_text(&first).contains("src/lib.rs:1:pub struct Alpha;"));
    let first_stderr = stderr_text(&first);
    assert!(first_stderr.contains("transport=daemon"));
    assert!(first_stderr.contains("backend=indexed_regex"));

    stop_detached_daemon(workspace);
}

/// Bound for one p28 client in lifecycle tests. It exceeds every client-side
/// daemon wait, so a client always reports its own result first.
const P28_CLIENT_TIMEOUT: Duration = Duration::from_secs(60);

/// A p28 client process that is killed if a failing test abandons it.
struct P28Client {
    child: Option<Child>,
}

impl P28Client {
    fn spawn(workspace: &Path, args: &[&str]) -> Self {
        let child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("p28"))
            .env("CARGO_BIN_EXE_packet28d", daemon_bin())
            .current_dir(workspace)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn p28 client");
        Self { child: Some(child) }
    }

    fn is_running(&mut self) -> bool {
        let child = self.child.as_mut().expect("p28 client");
        child.try_wait().expect("poll p28 client").is_none()
    }

    fn wait(mut self) -> Output {
        let started = Instant::now();
        while self.is_running() {
            assert!(
                started.elapsed() < P28_CLIENT_TIMEOUT,
                "p28 client did not finish within {P28_CLIENT_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.child
            .take()
            .unwrap()
            .wait_with_output()
            .expect("collect p28 client output")
    }
}

impl P28Client {
    /// Collects the output of a client that has already exited.
    fn take_output(&mut self) -> Output {
        self.child
            .take()
            .unwrap()
            .wait_with_output()
            .expect("collect p28 client output")
    }
}

impl Drop for P28Client {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

const DAEMON_SEARCH_ARGS: [&str; 5] = [
    "Alpha",
    "--fixed-strings",
    "--transport",
    "daemon",
    "--stats",
];

fn runtime_socket(workspace: &Path) -> PathBuf {
    let runtime: DaemonRuntimeInfo =
        serde_json::from_slice(&fs::read(runtime_path(workspace)).unwrap()).unwrap();
    PathBuf::from(runtime.socket_path)
}

fn assert_no_losing_replacement(workspace: &Path) {
    let log = fs::read_to_string(log_path(workspace)).unwrap_or_default();
    assert!(
        !log.contains("another Packet28 daemon already owns"),
        "p28 spawned a replacement daemon while the original owned the workspace:\n{log}"
    );
}

fn lifecycle_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    write_fixture(&workspace);
    (dir, workspace)
}

#[test]
fn p28_waits_for_stopping_daemon_authority_before_bootstrapping_replacement() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = DelayedShutdownDaemon::start(&workspace);
    let original_pid = fixture.original_pid();
    let socket = runtime_socket(&workspace);
    fixture.begin_shutdown();
    let socket_present = socket.exists();

    let mut client = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    // While the original daemon still owns the workspace, p28 may neither
    // remove its runtime files nor spawn a replacement. The window only
    // bounds observation.
    let observed = Instant::now();
    while observed.elapsed() < Duration::from_millis(750) {
        assert!(fixture.original_is_running());
        assert!(
            !socket_present || socket.exists(),
            "p28 removed the socket of a daemon that still owned the workspace"
        );
        assert!(runtime_path(&workspace).exists());
        assert_no_losing_replacement(&workspace);
        if !client.is_running() {
            let output = client.wait();
            panic!(
                "p28 completed with {} while the stopping daemon still owned the workspace; \
                 stdout={:?} stderr={:?}",
                output.status,
                stdout_text(&output),
                stderr_text(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    fixture.finish_shutdown();
    let output = client.wait();
    assert!(
        output.status.success(),
        "p28 failed after the original daemon released authority: stderr={}",
        stderr_text(&output)
    );
    assert!(stdout_text(&output).contains("src/lib.rs:1:pub struct Alpha;"));
    assert!(stderr_text(&output).contains("transport=daemon"));
    let replacement = daemon_runtime_pid(&workspace).expect("replacement runtime pid");
    assert_ne!(replacement, original_pid);
    assert!(process_is_alive(replacement));
    assert_no_losing_replacement(&workspace);

    stop_detached_daemon(&workspace);
}

#[test]
fn p28_bootstrap_gives_up_at_its_bound_without_touching_a_stalled_owner() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = DelayedShutdownDaemon::start(&workspace);
    let original_pid = fixture.original_pid();
    let socket = runtime_socket(&workspace);
    fixture.begin_shutdown();
    let socket_present = socket.exists();

    let started = Instant::now();
    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();
    let elapsed = started.elapsed();
    assert!(
        !output.status.success(),
        "p28 succeeded against a stalled owner"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("did not release workspace authority"),
        "missing authority timeout diagnostic: {stderr}"
    );
    // Bootstrap keeps its pre-existing ~10 s bound; only explicit stop and
    // restart use the longer shutdown grace.
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(25),
        "p28 bootstrap gave up after {elapsed:?}, outside its ~10 s bound"
    );
    assert!(fixture.original_is_running());
    assert!(
        !socket_present || socket.exists(),
        "timed-out p28 removed a live daemon socket"
    );
    assert_eq!(daemon_runtime_pid(&workspace), Some(original_pid));
    assert_no_losing_replacement(&workspace);

    fixture.finish_shutdown();
    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();
    assert!(
        output.status.success(),
        "p28 failed after the stalled owner released authority: stderr={}",
        stderr_text(&output)
    );
    assert_ne!(daemon_runtime_pid(&workspace), Some(original_pid));

    stop_detached_daemon(&workspace);
}

/// Hold a gated startup past the 10 s authority bound. Clients must keep
/// waiting under the separate 30 s startup-readiness phase. Holds are
/// measured from when the test observes the daemon in startup, which is after
/// any bootstrap deadline for that daemon began, so the margin past 10 s does
/// not depend on client process start-up latency.
const HOLD_PAST_AUTHORITY_BOUND: Duration = Duration::from_secs(13);

/// Bootstrap's startup-readiness phase in `packet28d start`.
const STARTUP_READINESS_PHASE: Duration = Duration::from_secs(30);

/// Keeps a gated startup pending until `elapsed` has passed since `from`,
/// asserting throughout that every client is still waiting, the daemon has
/// not published readiness, and no replacement daemon was spawned.
fn hold_pending_startup(
    workspace: &Path,
    clients: &mut [&mut P28Client],
    daemon_pid: u32,
    from: Instant,
    elapsed: Duration,
) {
    while from.elapsed() < elapsed {
        for client in clients.iter_mut() {
            if !client.is_running() {
                let output = client.take_output();
                panic!(
                    "p28 completed with {} after {:?} while startup was pending; stdout={:?} \
                     stderr={:?}",
                    output.status,
                    from.elapsed(),
                    stdout_text(&output),
                    stderr_text(&output)
                );
            }
        }
        let runtime = read_runtime(workspace).expect("pending daemon runtime");
        assert_eq!(runtime.pid, daemon_pid, "pending daemon was replaced");
        assert!(runtime.ready_at_unix.is_none(), "gated daemon became ready");
        assert!(process_is_alive(daemon_pid));
        assert_no_losing_replacement(workspace);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_search_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "p28 failed: stdout={:?} stderr={:?}",
        stdout_text(output),
        stderr_text(output)
    );
    assert!(stdout_text(output).contains("src/lib.rs:1:pub struct Alpha;"));
    assert!(stderr_text(output).contains("transport=daemon"));
}

fn assert_serving_identity(workspace: &Path, pid: u32) {
    let status = daemon_status(workspace).expect("selected daemon answers status");
    assert_eq!(status.pid, pid, "status identity changed");
    let runtime = read_runtime(workspace).expect("selected daemon runtime");
    assert_eq!(runtime.pid, pid, "runtime identity changed");
    assert!(runtime.ready_at_unix.is_some());
    assert_eq!(status.workspace_root, runtime.workspace_root);
}

#[test]
fn p28_waits_for_spawned_daemon_startup_held_past_authority_bound() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let started = Instant::now();
    let mut client = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    let daemon_pid = wait_for_startup_admission(&workspace).pid;
    let admitted = Instant::now();

    hold_pending_startup(
        &workspace,
        &mut [&mut client],
        daemon_pid,
        admitted,
        HOLD_PAST_AUTHORITY_BOUND,
    );
    fixture.release();
    let output = client.wait();
    let elapsed = started.elapsed();

    assert_search_succeeded(&output);
    assert!(
        elapsed < STARTUP_READINESS_PHASE,
        "p28 finished after {elapsed:?}, beyond the startup-readiness phase"
    );
    assert_serving_identity(&workspace, daemon_pid);
    assert_no_losing_replacement(&workspace);
    fixture.close();
}

fn reuses_connected_starting_daemon(force_tcp: bool) {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let daemon_pid = fixture.start_direct(force_tcp);
    let endpoint = read_runtime(&workspace).unwrap().socket_path;
    assert_eq!(endpoint.starts_with("tcp://"), force_tcp);

    let started = Instant::now();
    let mut client = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    hold_pending_startup(
        &workspace,
        &mut [&mut client],
        daemon_pid,
        started,
        HOLD_PAST_AUTHORITY_BOUND,
    );
    assert!(fixture.direct_is_running());
    fixture.release();
    let output = client.wait();

    assert_search_succeeded(&output);
    assert!(started.elapsed() < STARTUP_READINESS_PHASE);
    assert_serving_identity(&workspace, daemon_pid);
    assert_eq!(read_runtime(&workspace).unwrap().socket_path, endpoint);
    assert!(fixture.direct_is_running());
    assert_no_losing_replacement(&workspace);
    fixture.close();
}

#[test]
fn p28_reuses_connected_starting_daemon_over_unix_socket() {
    reuses_connected_starting_daemon(false);
}

#[test]
fn p28_reuses_connected_starting_daemon_over_authenticated_tcp() {
    reuses_connected_starting_daemon(true);
}

#[test]
fn concurrent_p28_starters_share_one_slow_starting_daemon() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let started = Instant::now();
    let mut first = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    let mut second = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    let daemon_pid = wait_for_startup_admission(&workspace).pid;
    let admitted = Instant::now();

    hold_pending_startup(
        &workspace,
        &mut [&mut first, &mut second],
        daemon_pid,
        admitted,
        HOLD_PAST_AUTHORITY_BOUND,
    );
    fixture.release();
    let first = first.wait();
    let second = second.wait();

    assert_search_succeeded(&first);
    assert_search_succeeded(&second);
    assert!(started.elapsed() < STARTUP_READINESS_PHASE);
    assert_serving_identity(&workspace, daemon_pid);
    assert_no_losing_replacement(&workspace);
    fixture.close();
}

#[test]
fn p28_startup_timeout_is_bounded_and_leaves_the_starting_daemon_running() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let started = Instant::now();
    let client = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS);
    let daemon_pid = wait_for_startup_admission(&workspace).pid;

    // The spawned daemon accepts connections but never answers while gated,
    // so each status request must end at the phase deadline.
    let output = client.wait();
    let elapsed = started.elapsed();
    assert!(
        !output.status.success(),
        "p28 succeeded against a gated daemon"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("did not become ready within 30000 ms")
            && stderr.contains("startup readiness phase")
            && stderr.contains(&format!("pid {daemon_pid}"))
            && stderr.contains("left running")
            && stderr.contains("packet28d.log"),
        "missing startup timeout diagnostic: {stderr}"
    );
    assert!(
        elapsed >= Duration::from_secs(29) && elapsed < Duration::from_secs(36),
        "startup readiness timeout after {elapsed:?}, outside its ~30 s phase"
    );

    // The caller's timeout did not stop the healthy slow starter.
    assert!(process_is_alive(daemon_pid));
    let runtime = read_runtime(&workspace).expect("pending daemon runtime");
    assert_eq!(runtime.pid, daemon_pid);
    assert!(runtime.ready_at_unix.is_none());
    fixture.release();
    wait_for_fixture_signal_or_panic(&workspace, daemon_pid);
    assert_serving_identity(&workspace, daemon_pid);
    assert_no_losing_replacement(&workspace);
    fixture.close();
}

fn wait_for_fixture_signal_or_panic(workspace: &Path, pid: u32) {
    let started = Instant::now();
    while daemon_status(workspace).map(|status| status.pid) != Some(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "released daemon pid {pid} did not become ready"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn p28_bootstrap_bounds_an_offline_owner_with_never_ready_runtime() {
    use std::os::unix::fs::OpenOptionsExt as _;

    let (_dir, workspace) = lifecycle_workspace();
    let daemon_dir = workspace.join(".packet28/daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    // A non-serving owner holds the instance lease. Its never-ready runtime
    // metadata names an endpoint with no live listener, so it is not a
    // startup candidate and gets only the authority bound.
    let instance = daemon_support::hold_instance_lock(&workspace);
    let dead_socket = daemon_dir.join("offline.sock");
    let runtime = DaemonRuntimeInfo {
        pid: std::process::id(),
        socket_path: dead_socket.to_string_lossy().to_string(),
        workspace_root: workspace.to_string_lossy().to_string(),
        ..DaemonRuntimeInfo::default()
    };
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(runtime_path(&workspace))
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, &serde_json::to_vec(&runtime).unwrap())
        })
        .unwrap();
    let runtime_before = fs::read(runtime_path(&workspace)).unwrap();

    let started = Instant::now();
    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();
    let elapsed = started.elapsed();

    assert!(
        !output.status.success(),
        "p28 succeeded against an offline owner"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("did not release workspace authority")
            && stderr.contains("authority phase"),
        "missing authority timeout diagnostic: {stderr}"
    );
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(25),
        "offline owner bound was {elapsed:?}, not the ~10 s authority phase"
    );
    assert_eq!(fs::read(runtime_path(&workspace)).unwrap(), runtime_before);
    assert!(
        !log_path(&workspace).exists(),
        "p28 spawned a replacement daemon"
    );
    drop(instance);
}

#[test]
fn p28_fails_closed_on_corrupt_runtime_of_a_starting_owner() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let daemon_pid = fixture.start_direct(false);
    let corrupt = b"{not runtime metadata";
    let replacement = workspace.join(".packet28/daemon/runtime.json.corrupt");
    fs::write(&replacement, corrupt).unwrap();
    fs::set_permissions(
        &replacement,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    fs::rename(&replacement, runtime_path(&workspace)).unwrap();

    let started = Instant::now();
    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();

    assert!(
        !output.status.success(),
        "p28 accepted corrupt runtime metadata"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("runtime metadata"),
        "missing runtime integrity diagnostic: {stderr}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "corrupt metadata was retried instead of failing closed"
    );
    assert_eq!(fs::read(runtime_path(&workspace)).unwrap(), corrupt);
    assert!(fixture.direct_is_running());
    assert!(process_is_alive(daemon_pid));
    assert!(
        !log_path(&workspace).exists(),
        "p28 spawned a replacement daemon"
    );
    // Readiness republishes authentic metadata, after which the fixture can
    // stop the owner through its endpoint.
    fixture.release();
    wait_for_fixture_signal_or_panic(&workspace, daemon_pid);
    fixture.close();
}

#[test]
fn p28_reports_early_daemon_exit_without_waiting_for_readiness() {
    let (_dir, workspace) = lifecycle_workspace();
    let daemon_dir = workspace.join(".packet28/daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::write(daemon_dir.join("task-registry-v1.json"), b"{not a registry").unwrap();

    let started = Instant::now();
    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();

    assert!(
        !output.status.success(),
        "p28 succeeded with corrupt durable state"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("exited with") && stderr.contains("before becoming ready"),
        "missing early-exit diagnostic: {stderr}"
    );
    assert!(started.elapsed() < Duration::from_secs(9));
    assert!(daemon_status(&workspace).is_none());
    assert!(daemon_support::instance_lease_released(&workspace));
}

#[test]
fn packet28d_start_reuses_connected_starting_daemon() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut fixture = GatedWorkspace::hold(&workspace);
    let daemon_pid = fixture.start_direct(false);

    let started = Instant::now();
    let mut bootstrap = P28Client {
        child: Some(
            std::process::Command::new(daemon_bin())
                .args(["start", "--root", workspace.to_str().unwrap()])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn packet28d start"),
        ),
    };
    hold_pending_startup(
        &workspace,
        &mut [&mut bootstrap],
        daemon_pid,
        started,
        HOLD_PAST_AUTHORITY_BOUND,
    );
    fixture.release();
    let output = bootstrap.wait();

    assert!(
        output.status.success(),
        "packet28d start failed: {}",
        stderr_text(&output)
    );
    assert!(started.elapsed() < STARTUP_READINESS_PHASE);
    assert!(fixture.direct_is_running());
    assert_serving_identity(&workspace, daemon_pid);
    assert_no_losing_replacement(&workspace);
    fixture.close();
}

/// Another workspace's daemon endpoint that records whether anything
/// connected, and a disposable process standing in for its pid.
struct ForeignDaemon {
    root: tempfile::TempDir,
    listener: std::os::unix::net::UnixListener,
    owner: Child,
}

impl ForeignDaemon {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(root.path().join("foreign.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let owner = std::process::Command::new("sleep")
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn foreign owner stand-in");
        Self {
            root,
            listener,
            owner,
        }
    }

    fn root(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap()
    }

    /// Publishes owner-private runtime metadata in `workspace` copied from
    /// this daemon, and returns its bytes.
    fn publish_copied_runtime(&self, workspace: &Path, ready: bool) -> Vec<u8> {
        use std::os::unix::fs::OpenOptionsExt as _;

        let runtime = DaemonRuntimeInfo {
            pid: self.owner.id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_unix: 1,
            ready_at_unix: ready.then_some(2),
            socket_path: self
                .root()
                .join("foreign.sock")
                .to_string_lossy()
                .to_string(),
            workspace_root: self.root().to_string_lossy().to_string(),
            log_path: self
                .root()
                .join("packet28d.log")
                .to_string_lossy()
                .to_string(),
            transport_auth: None,
        };
        let bytes = serde_json::to_vec(&runtime).unwrap();
        fs::create_dir_all(workspace.join(".packet28/daemon")).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(runtime_path(workspace))
            .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
            .unwrap();
        bytes
    }

    fn assert_never_contacted(&mut self) {
        match self.listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(_) => panic!("a client connected to another workspace's daemon endpoint"),
            Err(error) => panic!("foreign listener failed: {error}"),
        }
        assert!(
            self.owner.try_wait().unwrap().is_none(),
            "the foreign owner process was signalled"
        );
    }
}

impl Drop for ForeignDaemon {
    fn drop(&mut self) {
        let _ = self.owner.kill();
        let _ = self.owner.wait();
    }
}

const FOREIGN_WORKSPACE_DIAGNOSTIC: &str = "refusing to use another workspace's daemon";

fn packet28d_start(workspace: &Path) -> Output {
    P28Client {
        child: Some(
            std::process::Command::new(daemon_bin())
                .args(["start", "--root", workspace.to_str().unwrap()])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn packet28d start"),
        ),
    }
    .wait()
}

#[test]
fn foreign_workspace_runtime_fails_closed_while_its_authority_is_held() {
    // Build packet28d before timing; the first lookup runs Cargo.
    daemon_bin();
    for ready in [true, false] {
        let (_dir, workspace) = lifecycle_workspace();
        let mut foreign = ForeignDaemon::start();
        fs::create_dir_all(workspace.join(".packet28/daemon")).unwrap();
        let instance = daemon_support::hold_instance_lock(&workspace);
        let runtime_before = foreign.publish_copied_runtime(&workspace, ready);

        for (entry, output, elapsed) in [
            {
                let started = Instant::now();
                let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();
                ("p28", output, started.elapsed())
            },
            {
                let started = Instant::now();
                let output = packet28d_start(&workspace);
                ("packet28d start", output, started.elapsed())
            },
        ] {
            assert!(
                !output.status.success(),
                "{entry} accepted another workspace's daemon (ready={ready})"
            );
            let stderr = stderr_text(&output);
            assert!(
                stderr.contains(FOREIGN_WORKSPACE_DIAGNOSTIC)
                    && stderr.contains(&foreign.root().to_string_lossy().to_string()),
                "{entry} missing foreign-workspace diagnostic (ready={ready}): {stderr}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "{entry} waited {elapsed:?} instead of failing closed (ready={ready})"
            );
        }
        assert_eq!(fs::read(runtime_path(&workspace)).unwrap(), runtime_before);
        assert!(
            !log_path(&workspace).exists(),
            "a daemon was spawned for the workspace"
        );
        assert!(!workspace_socket_path(&workspace).exists());
        foreign.assert_never_contacted();
        drop(instance);
    }
}

#[test]
fn p28_replaces_copied_foreign_runtime_after_authority_release() {
    let (_dir, workspace) = lifecycle_workspace();
    let mut foreign = ForeignDaemon::start();
    foreign.publish_copied_runtime(&workspace, true);
    // The fixture owns and stops the daemon p28 spawns; startup is not held.
    let mut fixture = GatedWorkspace::hold(&workspace);
    fixture.release();

    let output = P28Client::spawn(&workspace, &DAEMON_SEARCH_ARGS).wait();

    assert_search_succeeded(&output);
    foreign.assert_never_contacted();
    let runtime = read_runtime(&workspace).expect("replacement runtime");
    assert_eq!(runtime.workspace_root, workspace.to_string_lossy());
    assert_ne!(runtime.pid, foreign.owner.id());
    assert_serving_identity(&workspace, runtime.pid);
    fixture.close();
}
