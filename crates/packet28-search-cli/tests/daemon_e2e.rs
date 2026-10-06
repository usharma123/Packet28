mod support;

#[path = "support/daemon.rs"]
mod daemon_support;

use daemon_support::{
    cli_with_daemon_env, daemon_bin, daemon_runtime_pid, process_is_alive, start_daemon,
    start_daemon_forced_tcp, start_daemon_workspace_fallback, stop_detached_daemon,
    DelayedShutdownDaemon,
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
