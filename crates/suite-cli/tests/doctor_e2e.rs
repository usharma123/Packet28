#[expect(
    dead_code,
    reason = "this integration binary exercises a focused subset of the shared harness"
)]
#[path = "support/process_harness.rs"]
mod process_harness;

use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

fn ensure_packet28d_built() {
    process_harness::ensure_packet28d_built();
}

fn write_repo_fixture(root: &Path) {
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
    process_harness::run_git(root, args);
}

fn init_repo(root: &Path) {
    git(root, &["init"]);
}

fn commit_repo_fixture(root: &Path) {
    fs::write(root.join(".gitignore"), ".covy/\n.mcp.json\n.packet28/\n").unwrap();
    git(root, &["add", "--all"]);
    git(
        root,
        &[
            "-c",
            "user.name=Packet28 Test",
            "-c",
            "user.email=packet28-test@example.invalid",
            "commit",
            "-m",
            "initialize doctor fixture",
        ],
    );
}

fn write_cached_coverage_state(root: &Path) {
    let mut coverage = suite_packet_core::CoverageData::new();
    let mut file = suite_packet_core::FileCoverage::new();
    file.lines_instrumented.insert(1);
    file.lines_covered.insert(1);
    coverage.files.insert("src/alpha.rs".to_string(), file);
    let bytes = suite_foundation_core::cache::serialize_coverage(&coverage).unwrap();
    let state_dir = root.join(".covy").join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("latest.bin"), bytes).unwrap();
}

fn write_cached_testmap_state(root: &Path) {
    let mut index = suite_packet_core::TestMapIndex::default();
    index.file_to_tests.insert(
        "src/alpha.rs".to_string(),
        ["tests/alpha_test.rs".to_string()].into_iter().collect(),
    );
    let state_dir = root.join(".covy").join("state");
    fs::create_dir_all(&state_dir).unwrap();
    testy_core::pipeline_testmap::write_testmap(&state_dir.join("testmap.bin"), &index).unwrap();
}

#[test]
#[cfg(unix)]
fn test_doctor_cli_reports_healthy_stack() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    commit_repo_fixture(dir.path());
    write_cached_coverage_state(dir.path());
    write_cached_testmap_state(dir.path());
    fs::write(
        dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "packet28": {
                    "command": "packet28-mcp",
                    "args": ["--root", dir.path().to_str().unwrap()]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    for _ in 0..2 {
        let output = suite_cmd()
            .current_dir(dir.path())
            .args(["doctor", "--root", dir.path().to_str().unwrap(), "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let payload: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(payload["daemon"]["ok"], true);
        assert_eq!(payload["index"]["ok"], true);
        assert!(payload["mcp_config"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["packet28_configured"] == true));
        assert_eq!(payload["handshake"]["ok"], true);
        assert_eq!(payload["reducer_round_trip"]["ok"], true);
        assert!(payload.get("push_notifications").is_some());
        assert_eq!(payload["handoff_round_trip"]["ok"], true);
        assert!(payload["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "experiment_manifest"));
    }

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_doctor_reports_hook_ingest_disabled_explicitly() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    commit_repo_fixture(dir.path());

    // Simulate a stale kill switch (`hooks_enabled: false`) left by a prior
    // `packet28 uninstall`. The daemon rejects every hook ingest in this state.
    let hook_config_path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    fs::create_dir_all(hook_config_path.parent().unwrap()).unwrap();
    let disabled = packet28_daemon_protocol::hooks::HookRuntimeConfig {
        hooks_enabled: false,
        ..Default::default()
    };
    fs::write(
        &hook_config_path,
        format!("{}\n", serde_json::to_string_pretty(&disabled).unwrap()),
    )
    .unwrap();

    let output = suite_cmd()
        .args(["doctor", "--root", dir.path().to_str().unwrap(), "--json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(report["handshake"]["ok"], true);
    assert_eq!(report["reducer_round_trip"]["ok"], false);
    let detail = report["reducer_round_trip"]["detail"].as_str().unwrap();
    assert!(
        detail.contains("hook ingest is disabled") && detail.contains("hooks_enabled=false"),
        "reducer_round_trip detail should name the disabled hook runtime, got: {detail}"
    );
    assert_eq!(report["handoff_round_trip"]["ok"], false);
    assert!(report["push_notifications"]["detail"]
        .as_str()
        .unwrap()
        .contains("disabled"));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_doctor_cli_reports_healthy_runtime() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    commit_repo_fixture(dir.path());

    let output = suite_cmd()
        .args(["doctor", "--root", dir.path().to_str().unwrap(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["daemon"]["ok"], true);
    assert_eq!(report["handshake"]["ok"], true);
    assert_eq!(report["reducer_round_trip"]["ok"], true);
    assert_eq!(report["handoff_round_trip"]["ok"], true);
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "experiment_manifest"));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}
