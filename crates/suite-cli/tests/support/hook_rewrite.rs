use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process_harness::{HarnessLimits, ProcessHarness};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

pub fn ensure_packet28d_built() {
    crate::process_harness::ensure_packet28d_built();
}

pub fn write_repo_fixture(root: &Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("alpha.rs"),
        r#"
use crate::beta::Beta;

fn alpha() {}
struct Alpha;
"#,
    )
    .unwrap();
    fs::write(
        src.join("beta.rs"),
        r#"
fn beta() {}
enum Beta {
  A,
}
"#,
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) {
    crate::process_harness::run_git(root, args);
}

pub fn init_repo(root: &Path) {
    git(root, &["init"]);
}

pub fn run_hook_raw(runtime: &str, root: &Path, stdin_payload: &str) -> (i32, String, String) {
    let home = root.join("hook-test-home");
    fs::create_dir_all(&home).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    configure_fixture_environment(&mut command, root);
    command
        .current_dir(root)
        .args(["hook", runtime, "--root", root.to_str().unwrap()]);
    let output = ProcessHarness::run(
        &mut command,
        stdin_payload.as_bytes(),
        COMMAND_TIMEOUT,
        HarnessLimits::default(),
    )
    .unwrap_or_else(|error| panic!("{runtime} hook process failed: {error}"));
    (
        output.status.code().unwrap_or(1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn configure_fixture_environment(command: &mut std::process::Command, root: &Path) {
    let home = root.join("hook-test-home");
    command
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"));
}

/// Stops only this fixture's daemon, including during an expected red assertion.
pub struct DaemonStopGuard {
    root: PathBuf,
}

impl DaemonStopGuard {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl Drop for DaemonStopGuard {
    fn drop(&mut self) {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
        configure_fixture_environment(&mut command, &self.root);
        command.args(["daemon", "stop", "--root", self.root.to_str().unwrap()]);
        let _ = ProcessHarness::run(
            &mut command,
            &[],
            Duration::from_secs(5),
            HarnessLimits::default(),
        );
    }
}
