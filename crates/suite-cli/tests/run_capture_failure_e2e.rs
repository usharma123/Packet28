#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

#[cfg(unix)]
use process_harness::{HarnessLimits, ProcessHarness};
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tempfile::TempDir;

#[test]
#[cfg(unix)]
fn run_reducer_preserves_result_when_artifact_capture_fails() {
    assert_failed_capture_preserves_result(false, true);
}

#[test]
#[cfg(unix)]
fn run_fallback_preserves_result_when_artifact_capture_fails() {
    assert_failed_capture_preserves_result(false, false);
}

#[test]
#[cfg(unix)]
fn run_reducer_preserves_result_when_analytics_capture_fails() {
    assert_failed_capture_preserves_result(true, true);
}

#[test]
#[cfg(unix)]
fn run_fallback_preserves_result_when_analytics_capture_fails() {
    assert_failed_capture_preserves_result(true, false);
}

#[cfg(unix)]
fn assert_failed_capture_preserves_result(analytics_failure: bool, reducer_supported: bool) {
    let root = TempDir::new().unwrap();
    let state = root.path().join(".packet28");
    if analytics_failure {
        fs::create_dir_all(state.join("run-savings.jsonl")).unwrap();
    } else {
        fs::write(state, b"capture storage is unavailable").unwrap();
    }
    let bin_dir = root.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let program = if reducer_supported {
        "cargo"
    } else {
        "fixture-command"
    };
    let script = bin_dir.join(program);
    fs::write(&script, "#!/bin/sh\nprintf 'run\\n' >> \"$RUN_COUNT\"\nprintf 'raw\\377\\n'\nprintf 'err\\n' >&2\nexit 7\n").unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(script, perms).unwrap();
    let count = root.path().join("run-count.txt");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"));
    command
        .current_dir(root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("RUN_COUNT", &count)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            program,
            "test",
        ]);
    let output = ProcessHarness::run(
        &mut command,
        &[],
        Duration::from_secs(15),
        HarnessLimits::default(),
    )
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "reducer_supported={reducer_supported}"
    );
    assert_eq!(output.stdout, b"raw\xff\n");
    assert_eq!(output.stderr, b"err\n");
    assert_eq!(fs::read_to_string(count).unwrap(), "run\n");
}
