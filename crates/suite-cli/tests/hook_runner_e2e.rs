#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use assert_cmd::Command;
use packet28_daemon_core::storage::{load_task_events, load_task_registry};
use packet28_daemon_protocol::hooks::HookRuntimeConfig;
use packet28_daemon_protocol::paths::{hook_runtime_config_path, task_artifact_dir, TaskStorageId};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

use process_harness::{HarnessLimits, ProcessHarness, ProcessOutput};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

struct DaemonStopGuard {
    root: PathBuf,
    armed: bool,
}

impl DaemonStopGuard {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            armed: true,
        }
    }

    fn stop(mut self) {
        suite_cmd()
            .args(["daemon", "stop", "--root", self.root.to_str().unwrap()])
            .assert()
            .success();
        self.armed = false;
    }
}

impl Drop for DaemonStopGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
        command.args(["daemon", "stop", "--root", self.root.to_str().unwrap()]);
        let _ = ProcessHarness::run(
            &mut command,
            &[],
            Duration::from_secs(5),
            HarnessLimits::default(),
        );
    }
}

fn assert_process_success(label: &str, output: &ProcessOutput) {
    assert!(
        output.status.success(),
        "{label} failed with status {:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn ensure_packet28d_built() {
    process_harness::ensure_packet28d_built();
}

fn git(root: &Path, args: &[&str]) {
    process_harness::run_git(root, args);
}

fn init_repo(root: &Path) {
    git(root, &["init"]);
}

fn install_counting_cat(dir: &Path) -> (String, std::path::PathBuf) {
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let counter_path = dir.join("cat-count.txt");
    fs::write(&counter_path, "0\n").unwrap();
    let script_path = bin_dir.join("cat");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\ncount=$(/bin/cat \"{count}\" 2>/dev/null || echo 0)\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > \"{count}\"\nexec /bin/cat \"$@\"\n",
            count = counter_path.display()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (path_env, counter_path)
}

#[test]
#[cfg(unix)]
fn test_hook_runner_cli_executes_every_explicit_request() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    fs::write(dir.path().join("sample.txt"), "Alpha\nBeta\n").unwrap();

    let (path_env, counter_path) = install_counting_cat(dir.path());
    let spec = packet28_reducer_core::classify_command("cat sample.txt").unwrap();
    let mut first = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    first.current_dir(dir.path()).env("PATH", &path_env).args([
        "hook",
        "reducer-runner",
        "--root",
        dir.path().to_str().unwrap(),
        "--task-id",
        "task-runner-cache",
        "--family",
        &spec.family,
        "--kind",
        &spec.canonical_kind,
        "--fingerprint",
        &spec.cache_fingerprint,
        "--cwd",
        dir.path().to_str().unwrap(),
        "--",
        "cat",
        "sample.txt",
    ]);
    let first = ProcessHarness::run(&mut first, &[], COMMAND_TIMEOUT, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("first reducer-runner invocation failed: {error}"));
    assert_process_success("first reducer-runner invocation", &first);
    let task_storage_id = TaskStorageId::try_from("task-runner-cache").unwrap();
    assert!(load_task_registry(dir.path())
        .unwrap()
        .tasks
        .contains_key(task_storage_id.as_str()));
    assert!(task_artifact_dir(dir.path(), &task_storage_id)
        .join("hook-spool")
        .is_dir());

    let mut second = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    second.current_dir(dir.path()).env("PATH", &path_env).args([
        "hook",
        "reducer-runner",
        "--root",
        dir.path().to_str().unwrap(),
        "--task-id",
        "task-runner-cache",
        "--family",
        &spec.family,
        "--kind",
        &spec.canonical_kind,
        "--fingerprint",
        &spec.cache_fingerprint,
        "--cwd",
        dir.path().to_str().unwrap(),
        "--",
        "cat",
        "sample.txt",
    ]);
    let second = ProcessHarness::run(&mut second, &[], COMMAND_TIMEOUT, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("second reducer-runner invocation failed: {error}"));
    assert_process_success("second reducer-runner invocation", &second);
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(fs::read_to_string(&counter_path).unwrap().trim(), "2");

    let completions = load_task_events(dir.path(), "task-runner-cache")
        .unwrap()
        .into_iter()
        .filter(|frame| frame.event.data["reason"] == "state_write:tool_result")
        .count();
    assert_eq!(completions, 2, "both executions must write fresh evidence");

    daemon.stop();
}

#[test]
#[cfg(unix)]
fn test_hook_runner_cli_busts_cache_after_out_of_band_file_edit() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    fs::write(dir.path().join("sample.txt"), "Alpha\nBeta\n").unwrap();

    let (path_env, counter_path) = install_counting_cat(dir.path());
    let spec = packet28_reducer_core::classify_command("cat sample.txt").unwrap();
    let runner_args = [
        "hook",
        "reducer-runner",
        "--root",
        dir.path().to_str().unwrap(),
        "--task-id",
        "task-runner-stale-cache",
        "--family",
        &spec.family,
        "--kind",
        &spec.canonical_kind,
        "--fingerprint",
        &spec.cache_fingerprint,
        "--cwd",
        dir.path().to_str().unwrap(),
        "--",
        "cat",
        "sample.txt",
    ];

    let mut first = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    first
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(runner_args);
    let first = ProcessHarness::run(&mut first, &[], COMMAND_TIMEOUT, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("first reducer-runner invocation failed: {error}"));
    assert_process_success("first reducer-runner invocation", &first);

    fs::write(dir.path().join("sample.txt"), "Alpha\nBeta\nGamma\n").unwrap();

    let mut second = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    second
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(runner_args);
    let second = ProcessHarness::run(&mut second, &[], COMMAND_TIMEOUT, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("second reducer-runner invocation failed: {error}"));
    assert_process_success("second reducer-runner invocation", &second);
    assert_ne!(first.stdout, second.stdout);
    assert_eq!(fs::read_to_string(&counter_path).unwrap().trim(), "2");

    daemon.stop();
}

#[test]
#[cfg(unix)]
fn test_hook_runner_capture_rejection_executes_original_command() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    fs::write(dir.path().join("sample.txt"), "Alpha\nBeta\n").unwrap();
    let config_path = hook_runtime_config_path(dir.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&HookRuntimeConfig {
            hooks_enabled: false,
            ..HookRuntimeConfig::default()
        })
        .unwrap(),
    )
    .unwrap();

    let (path_env, counter_path) = install_counting_cat(dir.path());
    let spec = packet28_reducer_core::classify_command("cat sample.txt").unwrap();
    let task_id = "task-runner-rejected";
    let task_storage_id = TaskStorageId::try_from(task_id).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command.current_dir(dir.path()).env("PATH", path_env).args([
        "hook",
        "reducer-runner",
        "--root",
        dir.path().to_str().unwrap(),
        "--task-id",
        task_id,
        "--family",
        &spec.family,
        "--kind",
        &spec.canonical_kind,
        "--fingerprint",
        &spec.cache_fingerprint,
        "--cwd",
        dir.path().to_str().unwrap(),
        "--",
        "cat",
        "sample.txt",
    ]);
    let output = ProcessHarness::run(&mut command, &[], COMMAND_TIMEOUT, HarnessLimits::default())
        .unwrap_or_else(|error| panic!("rejected reducer-runner invocation failed: {error}"));

    assert_process_success("runner with disabled capture", &output);
    assert_eq!(output.stdout, b"Alpha\nBeta\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_to_string(counter_path).unwrap().trim(), "1");
    assert!(!task_artifact_dir(dir.path(), &task_storage_id).exists());
    assert!(!load_task_registry(dir.path())
        .unwrap()
        .tasks
        .contains_key(task_storage_id.as_str()));

    daemon.stop();
}

#[cfg(unix)]
fn run_cat_runner(root: &Path, path_env: &str, task_id: &str) -> ProcessOutput {
    let spec = packet28_reducer_core::classify_command("cat sample.txt").unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command.current_dir(root).env("PATH", path_env).args([
        "hook",
        "reducer-runner",
        "--root",
        root.to_str().unwrap(),
        "--task-id",
        task_id,
        "--family",
        &spec.family,
        "--kind",
        &spec.canonical_kind,
        "--fingerprint",
        &spec.cache_fingerprint,
        "--cwd",
        root.to_str().unwrap(),
        "--",
        "cat",
        "sample.txt",
    ]);
    ProcessHarness::run(&mut command, &[], COMMAND_TIMEOUT, HarnessLimits::default()).unwrap()
}

#[test]
#[cfg(unix)]
fn test_hook_runner_capture_setup_failure_executes_original_command() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    fs::write(dir.path().join("sample.txt"), b"raw\xff\n").unwrap();
    fs::write(dir.path().join(".packet28"), b"blocked capture directory").unwrap();
    let (path_env, counter_path) = install_counting_cat(dir.path());
    let output = run_cat_runner(dir.path(), &path_env, "task-no-capture");
    assert_process_success("runner with unavailable capture storage", &output);
    assert_eq!(output.stdout, b"raw\xff\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_to_string(counter_path).unwrap().trim(), "1");
}

#[test]
#[cfg(unix)]
fn test_hook_runner_completion_capture_failure_preserves_result_without_rerun() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let counter_path = dir.path().join("run-count.txt");
    let script_path = bin_dir.join("cargo");
    fs::write(&script_path, "#!/bin/sh\nprintf 'run\\n' >> \"$RUN_COUNT\"\nprintf 'raw\\377\\n'\nprintf 'stderr\\n' >&2\nprintf '{invalid' > \"$CAPTURE_CONFIG\"\nrm -f \"$CAPTURE_SPOOL\"/*-stdout.log \"$CAPTURE_SPOOL\"/*-stderr.log\nexit 7\n").unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let spec = packet28_reducer_core::classify_command("cargo test").unwrap();
    let task_id = TaskStorageId::try_from("task-finish-failed").unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args([
            "hook",
            "reducer-runner",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            task_id.as_str(),
            "--family",
            &spec.family,
            "--kind",
            &spec.canonical_kind,
            "--fingerprint",
            &spec.cache_fingerprint,
            "--env",
            &format!("RUN_COUNT={}", counter_path.display()),
            "--env",
            &format!(
                "CAPTURE_CONFIG={}",
                hook_runtime_config_path(dir.path()).display()
            ),
            "--env",
            &format!(
                "CAPTURE_SPOOL={}",
                task_artifact_dir(dir.path(), &task_id)
                    .join("hook-spool")
                    .display()
            ),
            "--",
            "cargo",
            "test",
        ]);
    let output =
        ProcessHarness::run(&mut command, &[], COMMAND_TIMEOUT, HarnessLimits::default()).unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"raw\xff\n");
    assert_eq!(output.stderr, b"stderr\n");
    assert_eq!(fs::read_to_string(counter_path).unwrap(), "run\n");
    daemon.stop();
}

#[test]
#[cfg(unix)]
fn test_hook_runner_preserves_signal_exit_code_with_and_without_capture() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let daemon = DaemonStopGuard::new(dir.path());
    init_repo(dir.path());
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let script_path = bin_dir.join("cargo");
    fs::write(&script_path, "#!/bin/sh\nkill -TERM $$\n").unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let spec = packet28_reducer_core::classify_command("cargo test").unwrap();
    for hooks_enabled in [true, false] {
        let config_path = hook_runtime_config_path(dir.path());
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            serde_json::to_vec(&HookRuntimeConfig {
                hooks_enabled,
                ..HookRuntimeConfig::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
        command
            .current_dir(dir.path())
            .env("PATH", &path_env)
            .args([
                "hook",
                "reducer-runner",
                "--root",
                dir.path().to_str().unwrap(),
                "--task-id",
                "task-signal-exit",
                "--family",
                &spec.family,
                "--kind",
                &spec.canonical_kind,
                "--fingerprint",
                &spec.cache_fingerprint,
                "--",
                "cargo",
                "test",
            ]);
        let output =
            ProcessHarness::run(&mut command, &[], COMMAND_TIMEOUT, HarnessLimits::default())
                .unwrap();
        assert_eq!(
            output.status.code(),
            Some(143),
            "hooks_enabled={hooks_enabled}"
        );
    }
    daemon.stop();
}
