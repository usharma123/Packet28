use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
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
  tools:
    allowlist: ["covy"]
  reducers:
    allowlist: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  token_budget:
    cap: 200
  runtime_budget:
    cap_ms: 1000
  redaction:
    forbidden_patterns: ["(?i)password"]
"#,
    )
    .unwrap();
}

fn write_invalid_guard_context(path: &Path) {
    fs::write(
        path,
        r#"
version: 2
policy:
  tools:
    allowlist: [""]
  reducers:
    allowlist: [""]
  paths:
    include: ["["]
    exclude: []
  token_budget:
    cap: 0
  runtime_budget:
    cap_ms: 0
  redaction:
    forbidden_patterns: ["("]
"#,
    )
    .unwrap();
}

fn write_governed_context(path: &Path) {
    fs::write(
        path,
        r#"
version: 1
policy:
  tools:
    allowlist: ["diffy", "testy", "stacky", "buildy", "contextq"]
  reducers:
    allowlist: ["analyze", "impact", "slice", "reduce", "assemble", "contextq.assemble"]
  paths:
    include: ["**"]
    exclude: []
  token_budget:
    cap: 5000
  runtime_budget:
    cap_ms: 5000
  redaction:
    forbidden_patterns: []
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

fn write_denied_guard_packet(path: &Path) {
    fs::write(
        path,
        r#"{
  "tool": "covy",
  "reducer": "merge",
  "paths": ["src/private/secret.rs"],
  "token_usage": 500,
  "runtime_ms": 5000,
  "payload": {"password": "secret"}
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

fn write_stack_log(path: &Path) {
    fs::write(
        path,
        r#"
java.lang.IllegalStateException: boom
  at com.example.Service.run(src/service.rs:42)
  at com.example.Main.main(src/main.rs:10)

java.lang.IllegalStateException: boom
  at com.example.Service.run(src/service.rs:42)
  at com.example.Main.main(src/main.rs:10)
"#,
    )
    .unwrap();
}

fn write_build_log(path: &Path) {
    fs::write(
        path,
        r#"
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
main.c(40,2): warning C4996: use of deprecated function
"#,
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
fn test_suite_diff_analyze_governed_smoke() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_governed_context(&context);

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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"final_packet\""))
        .stdout(predicate::str::contains("\"tool\": \"contextq\""));
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
fn test_suite_test_impact_governed_smoke() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");
    let context = dir.path().join("context.yaml");
    write_manifest(&manifest);
    write_governed_context(&context);

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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"final_packet\""))
        .stdout(predicate::str::contains("\"tool\": \"contextq\""));
}

#[test]
fn test_suite_diff_analyze_governed_json_metadata_shape() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_governed_context(&context);

    let output = suite_cmd()
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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.diff.analyze.v1")
    );
    assert!(value
        .get("kernel_metadata")
        .and_then(|meta| meta.get("diff"))
        .is_some());
    assert!(value
        .get("kernel_metadata")
        .and_then(|meta| meta.get("governed"))
        .and_then(|governed| governed.get("budget_trim"))
        .is_some());
}

#[test]
fn test_suite_test_impact_governed_json_metadata_shape() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");
    let context = dir.path().join("context.yaml");
    write_manifest(&manifest);
    write_governed_context(&context);

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

    let output = suite_cmd()
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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.test.impact.v1")
    );
    assert!(value
        .get("kernel_metadata")
        .and_then(|meta| meta.get("impact"))
        .is_some());
    assert!(value
        .get("kernel_metadata")
        .and_then(|meta| meta.get("governed"))
        .and_then(|governed| governed.get("budget_trim"))
        .is_some());
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
fn test_suite_guard_validate_exit_code_stable_for_invalid_config() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_invalid_guard_context(&context);

    suite_cmd()
        .args(["guard", "validate", "--config", context.to_str().unwrap()])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"valid\": false"));
}

#[test]
fn test_suite_guard_check_exit_code_stable_for_denied_packet() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    let packet = dir.path().join("packet.json");
    write_guard_context(&context);
    write_denied_guard_packet(&packet);

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
        .code(1)
        .stdout(predicate::str::contains("\"passed\": false"));
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
        "testy",
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
        .stdout(predicate::str::contains(
            "\"schema_version\": \"suite.context.assemble.v1\"",
        ))
        .stdout(predicate::str::contains("\"packet\""))
        .stdout(predicate::str::contains("\"tool\": \"contextq\""))
        .stdout(predicate::str::contains("\"reducer\": \"assemble\""))
        .stdout(predicate::str::contains("\"sections\""));
}

#[test]
fn test_suite_context_assemble_governed_smoke() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    let packet_a = dir.path().join("a.json");
    let packet_b = dir.path().join("b.json");
    write_governed_context(&context);
    write_context_packet(
        &packet_a,
        "diffy",
        "Diff gate",
        "critical regression in coverage",
        "src/lib.rs",
    );
    write_context_packet(
        &packet_b,
        "testy",
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
            "--packet",
            packet_b.to_str().unwrap(),
            "--budget-tokens",
            "1200",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"schema_version\": \"suite.context.assemble.v1\"",
        ))
        .stdout(predicate::str::contains("\"final_packet\""))
        .stdout(predicate::str::contains("\"tool\": \"contextq\""));
}

#[test]
fn test_suite_governed_local_workflow_smoke() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");
    let context = dir.path().join("context.yaml");
    let packet_a = dir.path().join("a.json");
    let packet_b = dir.path().join("b.json");

    write_manifest(&manifest);
    write_governed_context(&context);
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
        .args(["guard", "validate", "--config", context.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"valid\": true"));

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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"final_packet\""))
        .stdout(predicate::str::contains("\"kernel_audit\""));

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
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"final_packet\""))
        .stdout(predicate::str::contains("\"kernel_audit\""));

    suite_cmd()
        .args([
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--packet",
            packet_b.to_str().unwrap(),
            "--budget-tokens",
            "1200",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tool\": \"contextq\""))
        .stdout(predicate::str::contains("\"assembly\""));
}

#[test]
fn test_suite_stack_slice_governed_smoke() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("stack.log");
    let context = dir.path().join("context.yaml");
    write_stack_log(&input);
    write_governed_context(&context);

    let output = suite_cmd()
        .args([
            "stack",
            "slice",
            "--input",
            input.to_str().unwrap(),
            "--json",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.stack.slice.v1")
    );
    assert!(value.get("packet").is_some());
    assert!(value.get("final_packet").is_some());
    assert!(value
        .get("kernel_audit")
        .and_then(|v| v.get("stack"))
        .is_some());
}

#[test]
fn test_suite_build_reduce_governed_smoke() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("build.log");
    let context = dir.path().join("context.yaml");
    write_build_log(&input);
    write_governed_context(&context);

    let output = suite_cmd()
        .args([
            "build",
            "reduce",
            "--input",
            input.to_str().unwrap(),
            "--json",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.build.reduce.v1")
    );
    assert!(value.get("packet").is_some());
    assert!(value.get("final_packet").is_some());
    assert!(value
        .get("kernel_audit")
        .and_then(|v| v.get("build"))
        .is_some());
}
