use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

fn agent_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("packet28-agent")
}

fn ensure_packet28d_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "packet28d"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to build packet28d");
    });
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
    allowlist: ["analyze", "impact", "slice", "reduce", "assemble", "contextq.assemble", "diffy.analyze", "testy.impact", "stacky.slice", "buildy.reduce", "governed.assemble"]
  paths:
    include: ["**"]
    exclude: []
  token_budget:
    cap: 5000
  runtime_budget:
    cap_ms: 5000
  tool_call_budget:
    cap: 10
  redaction:
    forbidden_patterns: []
  human_review:
    required: false
    on_policy_violation: true
    on_budget_violation: true
    on_redaction_violation: true
    paths: []
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

fn write_wrapped_guard_packet(path: &Path) {
    fs::write(
        path,
        r#"{
  "schema_version": "suite.packet.v1",
  "packet_type": "suite.proxy.run.v1",
  "packet": {
    "tool": "proxy",
    "payload": {
      "highlights": ["my_password_is_secret123"]
    }
  }
}"#,
    )
    .unwrap();
}

fn write_redaction_only_context(path: &Path) {
    fs::write(
        path,
        r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123", "(?i)password"]
"#,
    )
    .unwrap();
}

fn write_permissive_context(path: &Path) {
    fs::write(
        path,
        r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: []
"#,
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

fn write_packet_value(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
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
    let status = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed with {status}", args);
}

fn setup_changed_repo(root: &Path) {
    write_repo_fixture(root);
    git(root, &["init"]);
    git(root, &["add", "src/alpha.rs", "src/beta.rs"]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "init",
        ],
    );
    fs::write(
        root.join("src/alpha.rs"),
        r#"
use crate::beta::Beta;

fn alpha() -> i32 { 2 }
struct Alpha;
"#,
    )
    .unwrap();
    git(root, &["add", "src/alpha.rs"]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "change alpha",
        ],
    );
}

fn init_repo(root: &Path) {
    git(root, &["init"]);
}

fn kernel_cache_file(root: &Path) -> PathBuf {
    root.join(".packet28").join("packet-cache-v2.bin")
}

fn parse_packet_wrapper(output: &[u8], packet_type: &str) -> Value {
    let value: Value = serde_json::from_slice(output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.packet.v1")
    );
    assert_eq!(
        value.get("packet_type").and_then(Value::as_str),
        Some(packet_type)
    );
    assert!(value.get("packet").is_some());
    value
}

fn parse_preflight_response(output: &[u8]) -> Value {
    let value: Value = serde_json::from_slice(output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.preflight.v1")
    );
    value
}

fn packet_payload<'a>(wrapper: &'a Value) -> &'a Value {
    wrapper
        .get("packet")
        .and_then(|packet| packet.get("payload"))
        .expect("packet.payload should exist")
}

fn packet_debug(wrapper: &Value) -> Option<&Value> {
    packet_payload(wrapper).get("debug")
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

fn write_state_event(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
}

#[test]
fn test_suite_cover_check_smoke() {
    let output = suite_cmd()
        .args([
            "cover",
            "check",
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
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.cover.check.v1");
    assert!(packet_payload(&value).get("passed").is_some());
}

#[test]
fn test_suite_cover_check_rich_json_smoke() {
    let output = suite_cmd()
        .args([
            "cover",
            "check",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--json",
            "--packet-detail",
            "rich",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.cover.check.v1");
    assert!(packet_payload(&value).get("violations").is_some());
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.diff.analyze.v1");
    assert!(packet_debug(&value)
        .and_then(|v| v.get("governed_packet"))
        .and_then(|v| v.get("tool"))
        .and_then(Value::as_str)
        .is_some());
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.test.impact.v1");
    assert!(packet_payload(&value)
        .get("result")
        .and_then(|v| v.get("selected_tests"))
        .is_some());
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.test.impact.v1");
    assert!(packet_debug(&value)
        .and_then(|v| v.get("governed_packet"))
        .and_then(|v| v.get("tool"))
        .and_then(Value::as_str)
        .is_some());
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.diff.analyze.v1");
    assert!(packet_debug(&value)
        .and_then(|v| v.get("kernel_metadata"))
        .and_then(|meta| meta.get("diff"))
        .is_some());
    assert!(packet_debug(&value)
        .and_then(|v| v.get("kernel_metadata"))
        .and_then(|meta| meta.get("governed"))
        .and_then(|governed| governed.get("budget_trim"))
        .is_some());
}

#[test]
fn test_suite_diff_analyze_task_id_propagates_focus_to_map_repo() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--task-id",
            "task-diff",
            "--json",
        ])
        .assert()
        .success();

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "map",
            "repo",
            "--repo-root",
            ".",
            "--task-id",
            "task-diff",
            "--json",
            "--packet-detail",
            "rich",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.map.repo.v1");
    let files = value
        .get("packet")
        .and_then(|packet| packet.get("files"))
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(
        files
            .first()
            .and_then(|file| file.get("path"))
            .and_then(Value::as_str),
        Some("src/alpha.rs")
    );
    assert!(
        files[0].get("relevance").and_then(Value::as_f64).unwrap()
            > files[1].get("relevance").and_then(Value::as_f64).unwrap()
    );
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.test.impact.v1");
    assert!(packet_debug(&value)
        .and_then(|v| v.get("kernel_metadata"))
        .and_then(|meta| meta.get("impact"))
        .is_some());
    assert!(packet_debug(&value)
        .and_then(|v| v.get("kernel_metadata"))
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
fn test_suite_guard_validate_with_context_config_flag() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_guard_context(&context);

    suite_cmd()
        .args([
            "guard",
            "validate",
            "--context-config",
            context.to_str().unwrap(),
        ])
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

    let output = suite_cmd()
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
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.guard.check.v1");
    assert_eq!(
        packet_payload(&value)
            .get("passed")
            .and_then(Value::as_bool),
        Some(true)
    );
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

    let output = suite_cmd()
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
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.guard.check.v1");
    assert_eq!(
        packet_payload(&value)
            .get("passed")
            .and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn test_suite_guard_check_detects_wrapped_packet_redaction() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    let packet = dir.path().join("wrapped-packet.json");
    write_redaction_only_context(&context);
    write_wrapped_guard_packet(&packet);

    let output = suite_cmd()
        .args([
            "guard",
            "check",
            "--packet",
            packet.to_str().unwrap(),
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.guard.check.v1");
    assert!(packet_payload(&value)
        .get("findings")
        .and_then(Value::as_array)
        .and_then(|findings| findings.first())
        .and_then(|finding| finding.get("rule"))
        .and_then(Value::as_str)
        .is_some_and(|rule| rule == "redaction"));
}

#[test]
fn test_suite_cover_check_terminal_default() {
    suite_cmd()
        .args([
            "cover",
            "check",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Quality Gate: PASSED"))
        .stdout(predicate::str::contains("\"schema_version\"").not());
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

    let output = suite_cmd()
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
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.context.assemble.v1");
    assert_eq!(
        value
            .get("packet")
            .and_then(|packet| packet.get("tool"))
            .and_then(Value::as_str),
        Some("contextq")
    );
    assert!(packet_payload(&value).get("sections").is_some());
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

    let output = suite_cmd()
        .args([
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--packet",
            packet_b.to_str().unwrap(),
            "--budget-tokens",
            "1200",
            "--json",
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.context.assemble.v1");
    assert!(packet_debug(&value)
        .and_then(|debug| debug.get("kernel_metadata"))
        .and_then(|meta| meta.get("governed"))
        .is_some());
}

#[test]
fn test_suite_context_correlate_emits_v1_findings() {
    let dir = TempDir::new().unwrap();
    let diff = dir.path().join("diff.json");
    let impact = dir.path().join("impact.json");
    let stack = dir.path().join("stack.json");
    let build = dir.path().join("build.json");
    let map = dir.path().join("map.json");

    write_packet_value(
        &diff,
        &json!({
            "version": "1",
            "tool": "diffy",
            "kind": "diff_analyze",
            "hash": "diff-hash",
            "summary": "changed StopWatch",
            "files": [{"path": "src/StopWatch.java", "relevance": 1.0}],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["diff"], "generated_at_unix": 1},
            "payload": {
                "gate_result": {"passed": true, "violations": []},
                "diffs": [{"path": "src/StopWatch.java", "old_path": null, "status": "Modified", "changed_lines": [10, 11]}]
            }
        }),
    );
    write_packet_value(
        &impact,
        &json!({
            "version": "1",
            "tool": "testy",
            "kind": "test_impact",
            "hash": "impact-hash",
            "summary": "impact",
            "files": [],
            "symbols": [{"name": "StopWatchTest#testSplit", "kind": "test_id", "relevance": 1.0}],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["testmap.bin"], "generated_at_unix": 1},
            "payload": {
                "result": {
                    "selected_tests": ["StopWatchTest#testSplit"],
                    "smoke_tests": [],
                    "missing_mappings": [],
                    "confidence": 0.9,
                    "stale": false,
                    "escalate_full_suite": false
                },
                "known_tests": 1,
                "print_command": null
            }
        }),
    );
    write_packet_value(
        &stack,
        &json!({
            "version": "1",
            "tool": "stacky",
            "kind": "stack_slice",
            "hash": "stack-hash",
            "summary": "stack",
            "files": [{"path": "src/ArrayUtils.java", "relevance": 1.0}],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["stack.log"], "generated_at_unix": 1},
            "payload": {
                "schema_version": "stacky.slice.v1",
                "source": "stack.log",
                "total_failures": 1,
                "unique_failures": 1,
                "duplicates_removed": 0,
                "failures": []
            }
        }),
    );
    write_packet_value(
        &build,
        &json!({
            "version": "1",
            "tool": "buildy",
            "kind": "build_reduce",
            "hash": "build-hash",
            "summary": "build",
            "files": [{"path": "src/CharUtils.java", "relevance": 1.0}],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["build.log"], "generated_at_unix": 1},
            "payload": {
                "schema_version": "buildy.reduce.v1",
                "source": "build.log",
                "total_diagnostics": 1,
                "unique_diagnostics": 1,
                "duplicates_removed": 0,
                "groups": [],
                "ordered_fixes": []
            }
        }),
    );
    write_packet_value(
        &map,
        &json!({
            "version": "1",
            "tool": "mapy",
            "kind": "repo_map",
            "hash": "map-hash",
            "summary": "map",
            "files": [
                {"path": "src/StopWatch.java", "relevance": 1.0},
                {"path": "src/ArrayUtils.java", "relevance": 0.8}
            ],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["repo"], "generated_at_unix": 1},
            "payload": {
                "files_ranked": [{"file_idx": 0, "score": 1.0}, {"file_idx": 1, "score": 0.8}],
                "symbols_ranked": [],
                "edges": [],
                "focus_hits": [],
                "truncation": {"files_dropped": 0, "symbols_dropped": 0, "edges_dropped": 0}
            }
        }),
    );

    let output = suite_cmd()
        .args([
            "context",
            "correlate",
            "--packet",
            diff.to_str().unwrap(),
            "--packet",
            impact.to_str().unwrap(),
            "--packet",
            stack.to_str().unwrap(),
            "--packet",
            build.to_str().unwrap(),
            "--packet",
            map.to_str().unwrap(),
            "--task-id",
            "task-correlation",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.context.correlate.v1");
    let findings = packet_payload(&value)
        .get("findings")
        .and_then(Value::as_array)
        .unwrap();
    assert!(findings.len() >= 3);
    assert!(findings
        .iter()
        .any(|finding| { finding.get("relation").and_then(Value::as_str) == Some("unrelated") }));
    assert!(findings
        .iter()
        .any(|finding| { finding.get("relation").and_then(Value::as_str) == Some("supports") }));
    assert!(findings.iter().any(|finding| {
        finding.get("relation").and_then(Value::as_str) == Some("pre_existing_or_unrelated")
    }));
    assert!(findings
        .iter()
        .any(|finding| { finding.get("rule").and_then(Value::as_str) == Some("shared_file") }));
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"governed_packet\""))
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"governed_packet\""))
        .stdout(predicate::str::contains("\"kernel_audit\""));

    let output = suite_cmd()
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
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.context.assemble.v1");
    assert_eq!(
        value
            .get("packet")
            .and_then(|packet| packet.get("tool"))
            .and_then(Value::as_str),
        Some("contextq")
    );
    assert!(packet_payload(&value).get("assembly").is_some());
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.stack.slice.v1");
    assert!(packet_debug(&value)
        .and_then(|debug| debug.get("kernel_audit"))
        .and_then(|v| v.get("stack"))
        .is_some());
    assert!(packet_debug(&value)
        .and_then(|debug| debug.get("kernel_audit"))
        .and_then(|v| v.get("governed"))
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
            "full",
            "--context-config",
            context.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.build.reduce.v1");
    assert!(packet_debug(&value)
        .and_then(|debug| debug.get("kernel_audit"))
        .and_then(|v| v.get("build"))
        .is_some());
    assert!(packet_debug(&value)
        .and_then(|debug| debug.get("kernel_audit"))
        .and_then(|v| v.get("governed"))
        .is_some());
}

#[test]
fn test_suite_proxy_run_json_smoke() {
    let output = suite_cmd()
        .args(["proxy", "run", "--json", "--", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.proxy.run.v1");
    assert_eq!(
        value
            .get("packet")
            .and_then(|p| p.get("kind"))
            .and_then(Value::as_str),
        Some("command_summary")
    );
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("highlights"))
        .and_then(Value::as_array)
        .map(|v| !v.is_empty())
        .unwrap_or(false));
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("output_lines"))
        .is_none());
}

#[test]
fn test_suite_map_repo_json_smoke() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    let output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_packet_wrapper(&output, "suite.map.repo.v1");
    assert_eq!(
        value
            .get("packet")
            .and_then(|p| p.get("kind"))
            .and_then(Value::as_str),
        Some("repo_map")
    );
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("files_ranked"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("path"))
        .and_then(Value::as_str)
        .is_some());
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("symbols_ranked"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .is_some());
}

#[test]
fn test_suite_map_repo_cache_flag_writes_kernel_cache_file() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    let cache_file = kernel_cache_file(dir.path());
    assert!(!cache_file.exists());

    suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--cache",
            "--json",
        ])
        .assert()
        .success();

    assert!(cache_file.exists());
    assert!(fs::metadata(cache_file).unwrap().len() > 0);
}

#[test]
fn test_suite_proxy_run_rich_json_smoke() {
    let output = suite_cmd()
        .args([
            "proxy",
            "run",
            "--json",
            "full",
            "--packet-detail",
            "rich",
            "--",
            "ls",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("output_lines"))
        .and_then(Value::as_array)
        .map(|v| !v.is_empty())
        .unwrap_or(false));
}

#[test]
fn test_suite_proxy_run_cache_flag_writes_kernel_cache_file() {
    let dir = TempDir::new().unwrap();
    let cache_file = kernel_cache_file(dir.path());
    assert!(!cache_file.exists());

    suite_cmd()
        .args([
            "proxy",
            "run",
            "--cache",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--json",
            "--",
            "ls",
        ])
        .assert()
        .success();

    assert!(cache_file.exists());
    assert!(fs::metadata(cache_file).unwrap().len() > 0);
}

#[test]
fn test_suite_map_repo_rich_json_smoke() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    let output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json",
            "--packet-detail",
            "rich",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(value
        .get("packet")
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("files_ranked"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("file_idx"))
        .is_some());
}

#[test]
fn test_suite_output_flag_writes_to_file() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    let out = dir.path().join("map-output.json");

    suite_cmd()
        .args([
            "--output",
            out.to_str().unwrap(),
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let written = fs::read_to_string(&out).unwrap();
    let value: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.packet.v1")
    );
}

#[test]
fn test_suite_map_repo_rich_governed_section_body_uses_rich_payload() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    let context = dir.path().join("context.yaml");
    write_permissive_context(&context);

    let output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json",
            "full",
            "--packet-detail",
            "rich",
            "--context-config",
            context.to_str().unwrap(),
            "--context-budget-tokens",
            "5000",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    let body = value
        .get("packet")
        .and_then(|v| v.get("payload"))
        .and_then(|v| v.get("debug"))
        .and_then(|v| v.get("governed_packet"))
        .and_then(|v| v.get("payload"))
        .and_then(|v| v.get("sections"))
        .and_then(Value::as_array)
        .and_then(|sections| sections.first())
        .and_then(|section| section.get("body"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(body.contains("\"path\""));
    assert!(!body.contains("file_idx"));
}

#[test]
fn test_suite_proxy_run_rich_governed_section_body_uses_rich_payload() {
    let dir = TempDir::new().unwrap();
    let context = dir.path().join("context.yaml");
    write_permissive_context(&context);

    let output = suite_cmd()
        .args([
            "proxy",
            "run",
            "--json",
            "full",
            "--packet-detail",
            "rich",
            "--context-config",
            context.to_str().unwrap(),
            "--context-budget-tokens",
            "5000",
            "--",
            "ls",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    let body = value
        .get("packet")
        .and_then(|v| v.get("payload"))
        .and_then(|v| v.get("debug"))
        .and_then(|v| v.get("governed_packet"))
        .and_then(|v| v.get("payload"))
        .and_then(|v| v.get("sections"))
        .and_then(Value::as_array)
        .and_then(|sections| sections.first())
        .and_then(|section| section.get("body"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(body.contains("\"output_lines\""));
}

#[test]
fn test_compact_packets_respect_byte_slo_and_estimate() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let java_test = workspace.join("JavaTest");
    assert!(java_test.exists(), "JavaTest fixture folder missing");

    let map_output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            java_test.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        map_output.len() <= 2_500,
        "map output exceeded SLO: {}",
        map_output.len()
    );
    let map_value: Value = serde_json::from_slice(&map_output).unwrap();
    let map_packet = map_value.get("packet").unwrap();
    let map_packet_bytes = serde_json::to_vec(map_packet).unwrap().len();
    let map_est_bytes = map_packet
        .get("budget_cost")
        .and_then(|v| v.get("est_bytes"))
        .and_then(Value::as_u64)
        .unwrap() as usize;
    assert_eq!(map_est_bytes, map_packet_bytes);

    let proxy_output = suite_cmd()
        .args(["proxy", "run", "--json", "--", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        proxy_output.len() <= 2_500,
        "proxy output exceeded SLO: {}",
        proxy_output.len()
    );
    let proxy_value: Value = serde_json::from_slice(&proxy_output).unwrap();
    let proxy_packet = proxy_value.get("packet").unwrap();
    let proxy_packet_bytes = serde_json::to_vec(proxy_packet).unwrap().len();
    let proxy_est_bytes = proxy_packet
        .get("budget_cost")
        .and_then(|v| v.get("est_bytes"))
        .and_then(Value::as_u64)
        .unwrap() as usize;
    assert_eq!(proxy_est_bytes, proxy_packet_bytes);
}

#[test]
fn test_suite_context_store_cli_list_get_prune_stats_json() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--cache",
            "--json",
        ])
        .assert()
        .success();

    let stats_output = suite_cmd()
        .args([
            "context",
            "store",
            "stats",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stats: Value = serde_json::from_slice(&stats_output).unwrap();
    assert_eq!(
        stats.get("schema_version").and_then(Value::as_str),
        Some("suite.context.store.stats.v1")
    );
    assert!(
        stats
            .get("stats")
            .and_then(|v| v.get("entries"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1
    );

    let list_output = suite_cmd()
        .args([
            "context",
            "store",
            "ls",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list: Value = serde_json::from_slice(&list_output).unwrap();
    assert_eq!(
        list.get("schema_version").and_then(Value::as_str),
        Some("suite.context.store.list.v1")
    );
    let entries = list.get("entries").and_then(Value::as_array).unwrap();
    assert!(!entries.is_empty());
    let key = entries
        .first()
        .and_then(|v| v.get("cache_key"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let get_output = suite_cmd()
        .args([
            "context",
            "store",
            "get",
            "--root",
            dir.path().to_str().unwrap(),
            "--key",
            key.as_str(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let get_value: Value = serde_json::from_slice(&get_output).unwrap();
    assert_eq!(
        get_value
            .get("entry")
            .and_then(|v| v.get("entry"))
            .and_then(|v| v.get("cache_key"))
            .and_then(Value::as_str),
        Some(key.as_str())
    );

    let prune_output = suite_cmd()
        .args([
            "context",
            "store",
            "gc",
            "--root",
            dir.path().to_str().unwrap(),
            "--all",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let prune_value: Value = serde_json::from_slice(&prune_output).unwrap();
    assert_eq!(
        prune_value.get("schema_version").and_then(Value::as_str),
        Some("suite.context.store.prune.v1")
    );
    assert!(
        prune_value
            .get("report")
            .and_then(|v| v.get("removed"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1
    );
    assert!(
        prune_value
            .get("report")
            .and_then(|v| v.get("reasons"))
            .and_then(|v| v.get("manual_prune"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1
    );
}

#[test]
fn test_suite_context_recall_returns_recent_hits() {
    let dir = TempDir::new().unwrap();
    let packet = dir.path().join("packet.json");
    write_context_packet(
        &packet,
        "diffy",
        "Parser note",
        "missing mappings in parser for src/lib.rs",
        "src/lib.rs",
    );

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "context",
            "assemble",
            "--packet",
            packet.to_str().unwrap(),
            "--cache",
        ])
        .assert()
        .success();

    let output = suite_cmd()
        .args([
            "context",
            "recall",
            "--root",
            dir.path().to_str().unwrap(),
            "--query",
            "mappings parser src/lib.rs",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.context.recall.v1")
    );
    assert!(value
        .get("hits")
        .and_then(Value::as_array)
        .map(|hits| !hits.is_empty())
        .unwrap_or(false));
}

#[test]
fn test_suite_map_repo_terminal_shows_cache_hit_and_miss() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    let first = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first_out = String::from_utf8(first).unwrap();
    assert!(first_out.contains("cache: miss"));

    let second = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second_out = String::from_utf8(second).unwrap();
    assert!(second_out.contains("cache: hit"));
}

#[test]
fn test_suite_diff_analyze_json_includes_cache_block() {
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
            "--cache",
            "--json",
            "full",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert!(value
        .get("packet")
        .and_then(|packet| packet.get("payload"))
        .and_then(|payload| payload.get("debug"))
        .and_then(|debug| debug.get("cache"))
        .and_then(|v| v.get("diff"))
        .and_then(|v| v.get("hit"))
        .and_then(Value::as_bool)
        .is_some());
}

#[test]
fn test_suite_map_repo_profiles_and_handle_fetch_share_hash() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    let compact_output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json=compact",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let compact = parse_packet_wrapper(&compact_output, "suite.map.repo.v1");

    let full_output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json=full",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let full = parse_packet_wrapper(&full_output, "suite.map.repo.v1");

    let handle_output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json=handle",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let handle = parse_packet_wrapper(&handle_output, "suite.map.repo.v1");

    let compact_hash = compact
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(packet_payload(&compact)
        .get("files_ranked")
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("path"))
        .and_then(Value::as_str)
        .is_some());
    assert!(packet_payload(&compact)
        .get("symbols_ranked")
        .and_then(Value::as_array)
        .and_then(|symbols| symbols.first())
        .and_then(|symbol| symbol.get("name"))
        .and_then(Value::as_str)
        .is_some());

    let full_hash = full
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    let handle_hash = handle
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(compact_hash, full_hash);
    assert_eq!(compact_hash, handle_hash);

    let artifact_handle = packet_payload(&handle)
        .get("artifact_handle")
        .cloned()
        .unwrap();
    assert!(packet_payload(&handle)
        .get("files_ranked")
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .and_then(|file| file.get("path"))
        .and_then(Value::as_str)
        .is_some());
    let handle_id = artifact_handle
        .get("handle_id")
        .and_then(Value::as_str)
        .unwrap();
    let artifact_path = artifact_handle.get("path").and_then(Value::as_str).unwrap();
    assert!(Path::new(artifact_path).exists());

    let fetch_output = suite_cmd()
        .args([
            "packet",
            "fetch",
            "--handle",
            handle_id,
            "--root",
            dir.path().to_str().unwrap(),
            "--json=full",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fetched = parse_packet_wrapper(&fetch_output, "suite.map.repo.v1");
    let fetched_hash = fetched
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(compact_hash, fetched_hash);
}

#[test]
fn test_suite_proxy_run_profiles_and_handle_fetch_share_hash() {
    let dir = TempDir::new().unwrap();

    let compact_output = suite_cmd()
        .args([
            "proxy",
            "run",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--json=compact",
            "--",
            "ls",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let compact = parse_packet_wrapper(&compact_output, "suite.proxy.run.v1");

    let full_output = suite_cmd()
        .args([
            "proxy",
            "run",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--json=full",
            "--",
            "ls",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let full = parse_packet_wrapper(&full_output, "suite.proxy.run.v1");

    let handle_output = suite_cmd()
        .args([
            "proxy",
            "run",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--json=handle",
            "--",
            "ls",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let handle = parse_packet_wrapper(&handle_output, "suite.proxy.run.v1");

    let compact_hash = compact
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    let full_hash = full
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    let handle_hash = handle
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(compact_hash, full_hash);
    assert_eq!(compact_hash, handle_hash);

    let artifact_handle = packet_payload(&handle)
        .get("artifact_handle")
        .cloned()
        .unwrap();
    let handle_id = artifact_handle
        .get("handle_id")
        .and_then(Value::as_str)
        .unwrap();

    let fetch_output = suite_cmd()
        .args([
            "packet",
            "fetch",
            "--handle",
            handle_id,
            "--root",
            dir.path().to_str().unwrap(),
            "--json=full",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fetched = parse_packet_wrapper(&fetch_output, "suite.proxy.run.v1");
    let fetched_hash = fetched
        .get("packet")
        .and_then(|packet| packet.get("hash"))
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(compact_hash, fetched_hash);
}

#[test]
fn test_suite_cover_check_report_json_compat_maps_to_packet_wrapper() {
    let output = suite_cmd()
        .args([
            "cover",
            "check",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--report",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.cover.check.v1");
    assert!(packet_payload(&value).get("passed").is_some());
}

#[test]
fn test_suite_map_repo_legacy_json_compat_shape() {
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());

    let output = suite_cmd()
        .args([
            "map",
            "repo",
            "--repo-root",
            dir.path().to_str().unwrap(),
            "--json",
            "--legacy-json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.map.repo.v1")
    );
    assert!(value.get("packet_type").is_none());
    assert!(value.get("packet").is_some());
}

#[test]
fn test_suite_context_state_append_then_snapshot() {
    let dir = TempDir::new().unwrap();
    let event_path = dir.path().join("event.json");
    write_state_event(
        &event_path,
        r#"{
  "event_id": "evt-1",
  "occurred_at_unix": 1700000000,
  "actor": "agent",
  "kind": "question_opened",
  "data": {
    "type": "question_opened",
    "question_id": "q1",
    "text": "Does DateUtils call split()?"
  }
}"#,
    );

    let append_output = suite_cmd()
        .args([
            "context",
            "state",
            "append",
            "--task-id",
            "task-demo",
            "--input",
            event_path.to_str().unwrap(),
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let append = parse_packet_wrapper(&append_output, "suite.agent.state.v1");
    assert_eq!(
        packet_payload(&append)
            .get("task_id")
            .and_then(Value::as_str),
        Some("task-demo")
    );

    let snapshot_output = suite_cmd()
        .args([
            "context",
            "state",
            "snapshot",
            "--task-id",
            "task-demo",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let snapshot = parse_packet_wrapper(&snapshot_output, "suite.agent.snapshot.v1");
    assert_eq!(
        packet_payload(&snapshot)
            .get("event_count")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        packet_payload(&snapshot)
            .get("open_questions")
            .and_then(Value::as_array)
            .map(|questions| questions.len()),
        Some(1)
    );
}

#[test]
fn test_suite_context_assemble_task_id_compresses_read_section() {
    let dir = TempDir::new().unwrap();
    let event_path = dir.path().join("event.json");
    let packet_path = dir.path().join("packet.json");
    write_state_event(
        &event_path,
        r#"{
  "event_id": "evt-1",
  "occurred_at_unix": 1700000000,
  "actor": "agent",
  "kind": "file_read",
  "paths": ["src/time/StopWatch.java"],
  "data": {
    "type": "file_read"
  }
}"#,
    );
    fs::write(
        &packet_path,
        r#"{
  "packet_id": "diffy",
  "sections": [
    {
      "title": "Diff",
      "body": "StopWatch.java changed on lines 10-20",
      "refs": [{"kind": "file", "value": "src/time/StopWatch.java"}],
      "relevance": 0.9
    }
  ]
}"#,
    )
    .unwrap();

    suite_cmd()
        .args([
            "context",
            "state",
            "append",
            "--task-id",
            "task-demo",
            "--input",
            event_path.to_str().unwrap(),
            "--root",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "context",
            "assemble",
            "--packet",
            packet_path.to_str().unwrap(),
            "--task-id",
            "task-demo",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.context.assemble.v1");
    let first_body = packet_payload(&value)
        .get("sections")
        .and_then(Value::as_array)
        .and_then(|sections| sections.first())
        .and_then(|section| section.get("body"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(first_body.starts_with("Reminder: already reviewed"));
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_start_status_stop_cycle() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let status_output = suite_cmd()
        .args([
            "daemon",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).unwrap();
    let expected_root = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        status.get("workspace_root").and_then(Value::as_str),
        expected_root.to_str()
    );
    assert!(status.get("pid").and_then(Value::as_u64).unwrap() > 0);
    assert!(status.get("ready_at_unix").and_then(Value::as_u64).unwrap() > 0);
    assert!(status
        .get("log_path")
        .and_then(Value::as_str)
        .is_some_and(|path| Path::new(path).exists()));
    assert!(dir.path().join(".packet28/daemon/ready").exists());
    assert!(dir.path().join(".packet28/daemon/packet28d.log").exists());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn test_suite_preflight_json_selects_expected_reducers() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "preflight",
            "--task",
            "fix coverage gap in FooService",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--include",
            "impact",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_preflight_response(&output);
    let selected = value
        .get("selection")
        .and_then(|selection| selection.get("selected_reducers"))
        .and_then(Value::as_array)
        .unwrap();
    assert!(selected.iter().any(|item| item.as_str() == Some("cover")));
    assert!(selected.iter().any(|item| item.as_str() == Some("diff")));
    assert!(selected.iter().any(|item| item.as_str() == Some("map")));
    assert!(selected.iter().any(|item| item.as_str() == Some("recall")));
    assert!(value
        .get("selection")
        .and_then(|selection| selection.get("anchors"))
        .and_then(|anchors| anchors.get("symbols"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("FooService")));
    assert!(value
        .get("selection")
        .and_then(|selection| selection.get("skipped"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .any(|item| {
            item.get("reducer").and_then(Value::as_str) == Some("impact")
                && item.get("reason").and_then(Value::as_str) == Some("no_testmap")
        }));
}

#[test]
fn test_suite_preflight_prefers_explicit_coverage_over_cached_state() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    write_cached_coverage_state(dir.path());

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "preflight",
            "--task",
            "fix coverage gap in FooService",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_preflight_response(&output);
    assert!(value
        .get("selection")
        .and_then(|selection| selection.get("selected_reducers"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("cover")));
}

#[test]
fn test_suite_preflight_handle_outputs_fetchable_packet_handles() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "preflight",
            "--task",
            "fix coverage gap in FooService",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--json=handle",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_preflight_response(&output);
    let handle = value
        .get("results")
        .and_then(|results| results.get("packets"))
        .and_then(Value::as_array)
        .and_then(|packets| packets.first())
        .and_then(|packet| packet.get("packet"))
        .and_then(|wrapper| wrapper.get("packet"))
        .and_then(|packet| {
            packet.get("artifact_handle").or_else(|| {
                packet
                    .get("payload")
                    .and_then(|payload| payload.get("artifact_handle"))
            })
        })
        .and_then(|handle| handle.get("handle_id"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let fetched = suite_cmd()
        .current_dir(dir.path())
        .args(["packet", "fetch", "--handle", &handle, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fetched_value: Value = serde_json::from_slice(&fetched).unwrap();
    assert_eq!(
        fetched_value.get("schema_version").and_then(Value::as_str),
        Some("suite.packet.v1")
    );
}

#[test]
#[cfg(unix)]
fn test_suite_preflight_via_daemon_composes_existing_remote_calls() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "--daemon-root",
            dir.path().to_str().unwrap(),
            "preflight",
            "--task",
            "fix coverage gap in FooService",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_preflight_response(&output);
    assert!(value
        .get("selection")
        .and_then(|selection| selection.get("selected_reducers"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("diff")));
    assert!(dir.path().join(".packet28/daemon/runtime.json").exists());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn test_suite_agent_prompt_outputs_all_supported_fragments() {
    for format in ["claude", "agents", "cursor"] {
        let output = suite_cmd()
            .args(["agent-prompt", "--format", format])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let rendered = String::from_utf8(output).unwrap();
        assert!(!rendered.trim().is_empty());
        assert!(rendered.contains("Packet28 preflight"));
        assert!(rendered.contains("--json=compact"));
        assert!(rendered
            .to_ascii_lowercase()
            .contains("fall back to direct file reads"));
    }
}

#[test]
fn test_suite_agent_prompt_root_is_reflected_in_command_example() {
    suite_cmd()
        .args(["agent-prompt", "--format", "claude", "--root", "repo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--root \"repo\""));
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_persists_preflight_and_exports_env() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let env_dump = dir.path().join("env.txt");

    agent_cmd()
        .current_dir(dir.path())
        .args([
            "--task",
            "trace Alpha",
            "--",
            "sh",
            "-c",
            "printf '%s\\n%s\\n' \"$PACKET28_ROOT\" \"$PACKET28_PREFLIGHT_PATH\" > \"$1\"",
            "sh",
            env_dump.to_str().unwrap(),
        ])
        .assert()
        .success();

    let persisted_path = dir
        .path()
        .join(".packet28")
        .join("agent")
        .join("latest-preflight.json");
    assert!(persisted_path.exists());

    let env_lines = fs::read_to_string(&env_dump)
        .unwrap()
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        PathBuf::from(&env_lines[0]).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
    assert_eq!(
        PathBuf::from(&env_lines[1]).canonicalize().unwrap(),
        persisted_path.canonicalize().unwrap()
    );

    let value = parse_preflight_response(&fs::read(&persisted_path).unwrap());
    let selected = value
        .get("selection")
        .and_then(|selection| selection.get("selected_reducers"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(selected.contains(&"map"));
    assert!(selected.contains(&"recall"));
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_returns_child_exit_code() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    agent_cmd()
        .current_dir(dir.path())
        .args(["--task", "trace Alpha", "--", "sh", "-c", "exit 7"])
        .assert()
        .code(7);
}

#[test]
fn test_packet28_agent_requires_child_command() {
    agent_cmd()
        .args(["--task", "review alpha.rs change"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage:"));
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_preflight_failure_skips_child_process() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("child-ran.txt");
    let state_dir = dir.path().join(".covy").join("state");
    fs::create_dir_all(state_dir.join("latest.bin")).unwrap();

    agent_cmd()
        .current_dir(dir.path())
        .args([
            "--task",
            "fix coverage gap",
            "--",
            "sh",
            "-c",
            "touch \"$1\"",
            "sh",
            marker.to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert!(!marker.exists());
}

#[test]
fn test_suite_preflight_machine_failure_emits_suite_error_v1() {
    let dir = TempDir::new().unwrap();

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "preflight",
            "--task",
            "debug stack failure",
            "--include",
            "stack",
            "--stack-input",
            "missing.log",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.error.v1")
    );
    assert_eq!(
        value.get("target").and_then(Value::as_str),
        Some("preflight")
    );
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_suppresses_disconnect_log_noise() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let socket = packet28_daemon_core::socket_path(dir.path());
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    packet28_daemon_core::write_socket_message(
        &mut stream,
        &packet28_daemon_core::DaemonRequest::Status,
    )
    .unwrap();
    drop(stream);

    std::thread::sleep(std::time::Duration::from_millis(300));

    let log = fs::read_to_string(dir.path().join(".packet28/daemon/packet28d.log")).unwrap();
    assert!(!log.contains("request handling failed: Broken pipe"));
    assert!(!log.contains("request handling failed: Connection reset"));
    assert!(!log.contains("request handling failed: unexpected end of file"));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_diff_analyze_via_daemon_matches_packet_shape() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let local_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let via_daemon_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let local = parse_packet_wrapper(&local_output, "suite.diff.analyze.v1");
    let remote = parse_packet_wrapper(&via_daemon_output, "suite.diff.analyze.v1");
    assert_eq!(
        packet_payload(&local)
            .get("gate_result")
            .and_then(|value| value.get("passed"))
            .and_then(Value::as_bool),
        packet_payload(&remote)
            .get("gate_result")
            .and_then(|value| value.get("passed"))
            .and_then(Value::as_bool)
    );
    assert_eq!(
        packet_payload(&local).get("diffs"),
        packet_payload(&remote).get("diffs")
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_task_submit_returns_watch_id_and_watch_list() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let spec_path = dir.path().join("task-spec.json");
    fs::write(
        &spec_path,
        serde_json::to_string_pretty(&json!({
            "task_id": "task-watch",
            "sequence": {
                "steps": [
                    {
                        "id": "map",
                        "target": "mapy.repo",
                        "depends_on": [],
                        "input_packets": [],
                        "policy_context": {
                            "task_id": "task-watch"
                        },
                        "reducer_input": {
                            "repo_root": dir.path(),
                            "focus_paths": [],
                            "focus_symbols": [],
                            "max_files": 10,
                            "max_symbols": 20,
                            "include_tests": false
                        },
                        "budget": {}
                    }
                ],
                "budget": {},
                "reactive": {
                    "enabled": true,
                    "task_id": "task-watch",
                    "append_focused_map": true
                }
            },
            "watches": [
                {
                    "kind": "file",
                    "task_id": "task-watch",
                    "root": dir.path(),
                    "paths": ["src"],
                    "include_globs": ["src/**"],
                    "exclude_globs": []
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let submit_output = suite_cmd()
        .args([
            "daemon",
            "task",
            "submit",
            "--root",
            dir.path().to_str().unwrap(),
            "--spec",
            spec_path.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let submit: Value = serde_json::from_slice(&submit_output).unwrap();
    let watch_id = submit
        .get("watches")
        .and_then(Value::as_array)
        .and_then(|watches| watches.first())
        .and_then(|watch| watch.get("watch_id"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let watches_output = suite_cmd()
        .args([
            "daemon",
            "watch",
            "list",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-watch",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let watches: Value = serde_json::from_slice(&watches_output).unwrap();
    assert_eq!(
        watches
            .as_array()
            .and_then(|watches| watches.first())
            .and_then(|watch| watch.get("watch_id"))
            .and_then(Value::as_str),
        Some(watch_id.as_str())
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_task_submit_autofills_blank_and_missing_step_ids() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let spec_path = dir.path().join("task-spec.json");
    fs::write(
        &spec_path,
        serde_json::to_string_pretty(&json!({
            "task_id": "task-autofill",
            "sequence": {
                "steps": [
                    {
                        "id": "",
                        "target": "mapy.repo",
                        "depends_on": [],
                        "input_packets": [],
                        "policy_context": {
                            "task_id": "task-autofill"
                        },
                        "reducer_input": {
                            "repo_root": dir.path(),
                            "focus_paths": [],
                            "focus_symbols": [],
                            "max_files": 10,
                            "max_symbols": 20,
                            "include_tests": false
                        },
                        "budget": {}
                    },
                    {
                        "target": "mapy.repo",
                        "depends_on": ["mapy-repo-0"],
                        "input_packets": [],
                        "policy_context": {
                            "task_id": "task-autofill"
                        },
                        "reducer_input": {
                            "repo_root": dir.path(),
                            "focus_paths": [],
                            "focus_symbols": [],
                            "max_files": 10,
                            "max_symbols": 20,
                            "include_tests": false
                        },
                        "budget": {}
                    }
                ],
                "budget": {},
                "reactive": {
                    "enabled": true,
                    "task_id": "task-autofill",
                    "append_focused_map": true
                }
            },
            "watches": [
                {
                    "kind": "File",
                    "task_id": "task-autofill",
                    "root": dir.path(),
                    "paths": ["src"],
                    "include_globs": ["src/**"],
                    "exclude_globs": []
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    suite_cmd()
        .args([
            "daemon",
            "task",
            "submit",
            "--root",
            dir.path().to_str().unwrap(),
            "--spec",
            spec_path.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    let status_output = suite_cmd()
        .args([
            "daemon",
            "task",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-autofill",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).unwrap();
    let step_ids = status
        .get("sequence")
        .and_then(|sequence| sequence.get("steps"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|step| step.get("id").and_then(Value::as_str).unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        step_ids,
        vec!["mapy-repo-0".to_string(), "mapy-repo-1".to_string()]
    );
    assert_eq!(
        status
            .get("sequence")
            .and_then(|sequence| sequence.get("steps"))
            .and_then(Value::as_array)
            .and_then(|steps| steps.get(1))
            .and_then(|step| step.get("depends_on"))
            .and_then(Value::as_array)
            .and_then(|depends_on| depends_on.first())
            .and_then(Value::as_str),
        Some("mapy-repo-0")
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_task_submit_accepts_pascal_case_watch_kind() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let spec_path = dir.path().join("task-spec-watch.json");
    fs::write(
        &spec_path,
        serde_json::to_string_pretty(&json!({
            "task_id": "task-watch-kind",
            "sequence": {
                "steps": [
                    {
                        "target": "mapy.repo",
                        "depends_on": [],
                        "input_packets": [],
                        "policy_context": {
                            "task_id": "task-watch-kind"
                        },
                        "reducer_input": {
                            "repo_root": dir.path(),
                            "focus_paths": [],
                            "focus_symbols": [],
                            "max_files": 10,
                            "max_symbols": 20,
                            "include_tests": false
                        },
                        "budget": {}
                    }
                ],
                "budget": {},
                "reactive": {
                    "enabled": true,
                    "task_id": "task-watch-kind",
                    "append_focused_map": true
                }
            },
            "watches": [
                {
                    "kind": "File",
                    "task_id": "task-watch-kind",
                    "root": dir.path(),
                    "paths": ["src"],
                    "include_globs": ["src/**"],
                    "exclude_globs": []
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    suite_cmd()
        .args([
            "daemon",
            "task",
            "submit",
            "--root",
            dir.path().to_str().unwrap(),
            "--spec",
            spec_path.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    let watches_output = suite_cmd()
        .args([
            "daemon",
            "watch",
            "list",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-watch-kind",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let watches: Value = serde_json::from_slice(&watches_output).unwrap();
    assert_eq!(
        watches
            .as_array()
            .and_then(|watches| watches.first())
            .and_then(|watch| watch.get("spec"))
            .and_then(|spec| spec.get("kind"))
            .and_then(Value::as_str),
        Some("file")
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_daemon_failed_submit_cleans_up_task_and_watches() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let spec_path = dir.path().join("bad-task-spec.json");
    fs::write(
        &spec_path,
        serde_json::to_string_pretty(&json!({
            "task_id": "task-invalid",
            "sequence": {
                "steps": [
                    {
                        "id": "",
                        "target": "nope.reducer",
                        "depends_on": [],
                        "input_packets": [],
                        "policy_context": {},
                        "reducer_input": {},
                        "budget": {}
                    }
                ],
                "budget": {},
                "reactive": {
                    "enabled": true,
                    "task_id": "task-invalid",
                    "append_focused_map": true
                }
            },
            "watches": [
                {
                    "kind": "file",
                    "task_id": "task-invalid",
                    "root": dir.path(),
                    "paths": ["src"],
                    "include_globs": ["src/**"],
                    "exclude_globs": []
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    suite_cmd()
        .args([
            "daemon",
            "task",
            "submit",
            "--root",
            dir.path().to_str().unwrap(),
            "--spec",
            spec_path.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .code(2);

    let task_output = suite_cmd()
        .args([
            "daemon",
            "task",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-invalid",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let task_status: Value = serde_json::from_slice(&task_output).unwrap();
    assert!(task_status.is_null());

    let watches_output = suite_cmd()
        .args([
            "daemon",
            "watch",
            "list",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-invalid",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let watches: Value = serde_json::from_slice(&watches_output).unwrap();
    assert!(watches.as_array().unwrap().is_empty());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_via_daemon_uses_explicit_daemon_root_for_map_repo() {
    ensure_packet28d_built();
    let daemon_root = TempDir::new().unwrap();
    let repo_root = TempDir::new().unwrap();
    write_repo_fixture(daemon_root.path());
    init_repo(daemon_root.path());
    write_repo_fixture(repo_root.path());

    let output = suite_cmd()
        .current_dir(repo_root.path())
        .args([
            "--via-daemon",
            "--daemon-root",
            daemon_root.path().to_str().unwrap(),
            "map",
            "repo",
            "--repo-root",
            repo_root.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = parse_packet_wrapper(&output, "suite.map.repo.v1");
    assert!(packet_payload(&value).get("files_ranked").is_some());
    assert!(daemon_root
        .path()
        .join(".packet28/daemon/runtime.json")
        .exists());
    assert!(!repo_root
        .path()
        .join(".packet28/daemon/runtime.json")
        .exists());

    suite_cmd()
        .args([
            "daemon",
            "stop",
            "--root",
            daemon_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_via_daemon_honors_daemon_root_env() {
    ensure_packet28d_built();
    let daemon_root = TempDir::new().unwrap();
    let work_root = TempDir::new().unwrap();
    init_repo(daemon_root.path());
    init_repo(work_root.path());
    let manifest = work_root.path().join("manifest.jsonl");
    let testmap = work_root.path().join("testmap.bin");
    let timings = work_root.path().join("testtimings.bin");
    write_manifest(&manifest);

    suite_cmd()
        .current_dir(work_root.path())
        .env("PACKET28_DAEMON_ROOT", daemon_root.path().to_str().unwrap())
        .args([
            "--via-daemon",
            "test",
            "map",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            testmap.to_str().unwrap(),
            "--timings-output",
            timings.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    assert!(daemon_root
        .path()
        .join(".packet28/daemon/runtime.json")
        .exists());
    assert!(!work_root
        .path()
        .join(".packet28/daemon/runtime.json")
        .exists());

    suite_cmd()
        .args([
            "daemon",
            "stop",
            "--root",
            daemon_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}

#[test]
fn test_suite_context_assemble_machine_failure_emits_suite_error_v1() {
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

    let output = suite_cmd()
        .args([
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--packet",
            packet_b.to_str().unwrap(),
            "--context-config",
            context.to_str().unwrap(),
            "--budget-tokens",
            "1",
            "--budget-bytes",
            "1",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        value.get("schema_version").and_then(Value::as_str),
        Some("suite.error.v1")
    );
}

#[test]
#[cfg(unix)]
fn test_suite_via_daemon_diff_wrapper_surfaces_cache_hit() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());

    let first = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "--daemon-root",
            dir.path().to_str().unwrap(),
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "--daemon-root",
            dir.path().to_str().unwrap(),
            "diff",
            "analyze",
            "--coverage",
            &fixture("lcov/basic.info"),
            "--no-issues-state",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let first_value: Value = serde_json::from_slice(&first).unwrap();
    let second_value: Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(
        first_value.get("cache_hit").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        second_value.get("cache_hit").and_then(Value::as_bool),
        Some(true)
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn test_suite_recall_prefers_summary_snippet_over_target_name() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
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
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--packet",
            packet_b.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "recall",
            "--root",
            ".",
            "--query",
            "critical regression",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    let snippet = value
        .get("hits")
        .and_then(Value::as_array)
        .and_then(|hits| hits.first())
        .and_then(|hit| hit.get("snippet"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(snippet.contains("critical regression"));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn test_suite_recall_json_surfaces_summary_field() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let packet = dir.path().join("stack.json");

    write_packet_value(
        &packet,
        &json!({
            "version": "1",
            "tool": "stacky",
            "kind": "stack_slice",
            "hash": "stack-hash",
            "summary": "stack failures total=2 unique=2",
            "files": [{"path": "src/main.rs", "relevance": 1.0}],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["stack.log"], "generated_at_unix": 1},
            "payload": {
                "total_failures": 2,
                "unique_failures": 2,
                "duplicates_removed": 0
            }
        }),
    );

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "assemble",
            "--packet",
            packet.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    let output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "recall",
            "--root",
            ".",
            "--query",
            "stack failures src/main.rs",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    let first_hit = value
        .get("hits")
        .and_then(Value::as_array)
        .and_then(|hits| hits.first())
        .unwrap();
    assert_eq!(
        first_hit.get("summary").and_then(Value::as_str),
        Some("stack failures total=2 unique=2")
    );
    assert_eq!(
        first_hit.get("snippet").and_then(Value::as_str),
        Some("stack failures total=2 unique=2")
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_test_map_and_shard_via_daemon_auto_start() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");
    let timings = dir.path().join("testtimings.bin");
    let tasks = dir.path().join("tasks.json");
    write_manifest(&manifest);
    fs::write(
        &tasks,
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "tasks": [
                {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1200, "tags": ["unit"]},
                {"id": "com.foo.BazTest", "selector": "com.foo.BazTest", "est_ms": 900, "tags": ["unit"]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let map_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "test",
            "map",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            testmap.to_str().unwrap(),
            "--timings-output",
            timings.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let map_value: Value = serde_json::from_slice(&map_output).unwrap();
    assert_eq!(map_value.get("records").and_then(Value::as_u64), Some(1));
    assert!(dir.path().join(".packet28/daemon/runtime.json").exists());
    assert!(testmap.exists());
    assert!(timings.exists());

    let shard_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "test",
            "shard",
            "--shards",
            "2",
            "--tasks-json",
            tasks.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shard_value: Value = serde_json::from_slice(&shard_output).unwrap();
    assert_eq!(
        shard_value
            .get("shards")
            .and_then(Value::as_array)
            .map(|value| value.len()),
        Some(2)
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_stack_and_build_via_daemon_emit_packet_wrappers() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let stack_input = dir.path().join("stack.log");
    let build_input = dir.path().join("build.log");
    write_stack_log(&stack_input);
    write_build_log(&build_input);

    let stack_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "stack",
            "slice",
            "--input",
            stack_input.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stack_value = parse_packet_wrapper(&stack_output, "suite.stack.slice.v1");
    assert!(packet_payload(&stack_value)
        .get("failures")
        .and_then(Value::as_array)
        .is_some());

    let build_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "build",
            "reduce",
            "--input",
            build_input.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let build_value = parse_packet_wrapper(&build_output, "suite.build.reduce.v1");
    assert!(packet_payload(&build_value)
        .get("groups")
        .and_then(Value::as_array)
        .is_some());

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_suite_context_non_assemble_via_daemon_smoke() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    let diff = dir.path().join("diff.json");
    let impact = dir.path().join("impact.json");
    let event = dir.path().join("event.json");
    let packet_a = dir.path().join("a.json");
    let packet_b = dir.path().join("b.json");

    write_packet_value(
        &diff,
        &json!({
            "version": "1",
            "tool": "diffy",
            "kind": "diff_analyze",
            "hash": "diff-hash",
            "summary": "changed StopWatch",
            "files": [{"path": "src/StopWatch.java", "relevance": 1.0}],
            "symbols": [],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["diff"], "generated_at_unix": 1},
            "payload": {
                "gate_result": {"passed": true, "violations": []},
                "diffs": [{"path": "src/StopWatch.java", "old_path": null, "status": "Modified", "changed_lines": [10, 11]}]
            }
        }),
    );
    write_packet_value(
        &impact,
        &json!({
            "version": "1",
            "tool": "testy",
            "kind": "test_impact",
            "hash": "impact-hash",
            "summary": "impact",
            "files": [],
            "symbols": [{"name": "StopWatchTest#testSplit", "kind": "test_id", "relevance": 1.0}],
            "budget_cost": {"est_tokens": 1, "est_bytes": 1, "runtime_ms": 1, "tool_calls": 1},
            "provenance": {"inputs": ["testmap.bin"], "generated_at_unix": 1},
            "payload": {
                "result": {
                    "selected_tests": ["StopWatchTest#testSplit"],
                    "smoke_tests": [],
                    "missing_mappings": [],
                    "confidence": 0.9,
                    "stale": false,
                    "escalate_full_suite": false
                },
                "known_tests": 1,
                "print_command": null
            }
        }),
    );
    write_state_event(
        &event,
        r#"{
  "event_id": "evt-1",
  "occurred_at_unix": 1,
  "actor": "tester",
  "kind": "focus_set",
  "paths": ["src/lib.rs"],
  "symbols": [],
  "data": {"type": "focus_set"}
}"#,
    );
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

    let correlate_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "correlate",
            "--packet",
            diff.to_str().unwrap(),
            "--packet",
            impact.to_str().unwrap(),
            "--task-id",
            "task-correlation",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let correlate_value = parse_packet_wrapper(&correlate_output, "suite.context.correlate.v1");
    assert!(packet_payload(&correlate_value)
        .get("findings")
        .and_then(Value::as_array)
        .is_some());

    let state_append_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "state",
            "append",
            "--task-id",
            "task-state",
            "--input",
            event.to_str().unwrap(),
            "--root",
            ".",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let state_append_value = parse_packet_wrapper(&state_append_output, "suite.agent.state.v1");
    assert_eq!(
        packet_payload(&state_append_value)
            .get("task_id")
            .and_then(Value::as_str),
        Some("task-state")
    );

    let state_snapshot_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "state",
            "snapshot",
            "--task-id",
            "task-state",
            "--root",
            ".",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let state_snapshot_value =
        parse_packet_wrapper(&state_snapshot_output, "suite.agent.snapshot.v1");
    assert_eq!(
        packet_payload(&state_snapshot_value)
            .get("task_id")
            .and_then(Value::as_str),
        Some("task-state")
    );

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "assemble",
            "--packet",
            packet_a.to_str().unwrap(),
            "--packet",
            packet_b.to_str().unwrap(),
        ])
        .assert()
        .success();

    let store_list_output = suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "store",
            "list",
            "--root",
            ".",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let store_list_value: Value = serde_json::from_slice(&store_list_output).unwrap();
    let entries = store_list_value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap();
    assert!(!entries.is_empty());

    let key = entries[0]
        .get("cache_key")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "store",
            "get",
            "--root",
            ".",
            "--key",
            &key,
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(&key));

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "store",
            "stats",
            "--root",
            ".",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"stats\""));

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "recall",
            "--root",
            ".",
            "--query",
            "critical regression",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"query\":\"critical regression\"",
        ));

    suite_cmd()
        .current_dir(dir.path())
        .args([
            "--via-daemon",
            "context",
            "store",
            "prune",
            "--root",
            ".",
            "--all",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"report\""));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}
