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

fn write_context_packet(path: &Path, packet_id: &str, title: &str, body: &str, path_ref: &str) {
    fs::write(
        path,
        format!(
            r#"{{
  "packet_id": "{packet_id}",
  "tool": "{packet_id}",
  "reducer": "reduce",
  "paths": ["{path_ref}"],
  "sections": [
    {{
      "title": "{title}",
      "body": "{body}",
      "refs": [{{ "kind": "file", "value": "{path_ref}" }}],
      "relevance": 0.9
    }}
  ]
}}"#
        ),
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

#[test]
fn test_suite_context_assemble_smoke() {
    let dir = TempDir::new().unwrap();
    let packet_a = dir.path().join("a.json");
    let packet_b = dir.path().join("b.json");
    write_context_packet(
        &packet_a,
        "diffy",
        "Diff gate",
        "critical regression in coverage",
        "src/lib.rs",
    );
    write_context_packet(
        &packet_b,
        "impact",
        "Impact plan",
        "selected tests for src/lib.rs",
        "src/lib.rs",
    );

    suite_cmd()
        .args([
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--input",
            packet_b.to_str().unwrap(),
            "--budget-tokens",
            "1200",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tool\": \"contextq\""))
        .stdout(predicate::str::contains("\"reducer\": \"assemble\""))
        .stdout(predicate::str::contains("\"sections\""));
}
