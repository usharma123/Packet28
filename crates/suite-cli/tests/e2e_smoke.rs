use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("suite")
}

fn fixture(rel: &str) -> String {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    workspace
        .join("tests")
        .join("fixtures")
        .join(rel)
        .to_string_lossy()
        .to_string()
}

fn write_manifest(path: &Path) {
    let line = format!(
        "{{\"test_id\":\"com.foo.BarTest\",\"language\":\"java\",\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(path, line).unwrap();
}

fn write_guard_context(path: &Path) {
    fs::write(
        path,
        r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 200
    runtime_ms_cap: 1000
  redaction:
    forbidden_patterns: ["(?i)password"]
"#,
    )
    .unwrap();
}

fn write_guard_packet(path: &Path) {
    fs::write(
        path,
        r#"{
  "tool": "covy",
  "reducer": "merge",
  "paths": ["src/lib.rs"],
  "token_usage": 50,
  "runtime_ms": 300,
  "payload": {"message": "all clear"}
}"#,
    )
    .unwrap();
}

#[test]
fn test_suite_diff_analyze_smoke() {
    suite_cmd()
        .args([
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"passed\""));
}

#[test]
fn test_suite_test_impact_smoke() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");
    write_manifest(&manifest);

    suite_cmd()
        .args([
            "test",
            "map",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            testmap.to_str().unwrap(),
        ])
        .assert()
        .success();

    suite_cmd()
        .args([
            "test",
            "impact",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--testmap",
            testmap.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"selected_tests\""));
}

#[test]
fn test_suite_guard_validate_smoke() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_guard_context(&context);

    suite_cmd()
        .args(["guard", "validate", "--config", context.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"valid\": true"));
}

#[test]
fn test_suite_guard_check_smoke() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    let packet = dir.path().join("packet.json");
    write_guard_context(&context);
    write_guard_packet(&packet);

    suite_cmd()
        .args([
            "guard",
            "check",
            "--packet",
            packet.to_str().unwrap(),
            "--config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"passed\": true"));
}
