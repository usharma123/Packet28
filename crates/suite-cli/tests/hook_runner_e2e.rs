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
fn test_hook_runner_rejection_creates_no_managed_task_artifact() {
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

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("reducer-runner task admission was rejected"));
    assert_eq!(fs::read_to_string(counter_path).unwrap().trim(), "0");
    assert!(!task_artifact_dir(dir.path(), &task_storage_id).exists());
    assert!(!load_task_registry(dir.path())
        .unwrap()
        .tasks
        .contains_key(task_storage_id.as_str()));

    daemon.stop();
}
