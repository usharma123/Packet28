use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

fn agent_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("packet28-agent")
}

fn mcp_cmd() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_Packet28"))
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

fn write_mcp_message(stdin: &mut ChildStdin, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    stdin.write_all(&body).unwrap();
    stdin.flush().unwrap();
}

fn write_mcp_message_newline(stdin: &mut ChildStdin, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    stdin.write_all(&body).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

fn read_mcp_message(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut content_length = None::<usize>;
    let mut line = String::new();
    loop {
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let mut body = vec![0_u8; content_length.unwrap()];
    stdout.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn read_mcp_message_newline(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed).unwrap();
    }
}

fn read_mcp_message_for_id(stdout: &mut BufReader<ChildStdout>, expected_id: u64) -> Value {
    loop {
        let value = read_mcp_message(stdout);
        if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return value;
        }
    }
}

fn start_mcp_server(root: &Path) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = mcp_cmd()
        .current_dir(root)
        .args(["mcp", "serve", "--root", root.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

fn start_mcp_proxy_server(
    root: &Path,
    config_path: &Path,
    task_id: &str,
) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = mcp_cmd()
        .current_dir(root)
        .args([
            "mcp",
            "proxy",
            "--root",
            root.to_str().unwrap(),
            "--upstream-config",
            config_path.to_str().unwrap(),
            "--task-id",
            task_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
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

fn parse_broker_response(output: &[u8]) -> Value {
    let value: Value = serde_json::from_slice(output).unwrap();
    assert!(value.get("context_version").is_some());
    assert!(value.get("brief").is_some());
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
#[cfg(unix)]
fn test_suite_daemon_index_rebuild_and_status() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());

    suite_cmd()
        .args(["daemon", "start", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    let rebuild_output = suite_cmd()
        .args([
            "daemon",
            "index",
            "rebuild",
            "--root",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rebuild: Value = serde_json::from_slice(&rebuild_output).unwrap();
    assert_eq!(rebuild.get("accepted").and_then(Value::as_bool), Some(true));
    assert_eq!(rebuild.get("full").and_then(Value::as_bool), Some(true));

    let start = std::time::Instant::now();
    let mut ready = false;
    while start.elapsed() < Duration::from_secs(5) {
        let status_output = suite_cmd()
            .args([
                "daemon",
                "index",
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
        if status.get("ready").and_then(Value::as_bool) == Some(true) {
            ready = true;
            assert!(
                status
                    .get("manifest")
                    .and_then(|manifest| manifest.get("indexed_files"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "expected daemon index to become ready");

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[cfg(unix)]
fn seed_checkpointed_handoff_task(
    dir: &Path,
    task_id: &str,
    intention_text: &str,
    checkpoint_id: &str,
) {
    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":task_id,
                    "op":"intention",
                    "text":intention_text,
                    "note":"Need the next worker to pick up the handoff packet",
                    "step_id":"investigating",
                    "paths":["src/alpha.rs"],
                    "symbols":["Alpha"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 2);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":task_id,
                    "op":"checkpoint_save",
                    "checkpoint_id":checkpoint_id,
                    "note":"Checkpoint before launching a fresh worker",
                    "paths":["src/alpha.rs"],
                    "symbols":["Alpha"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 3);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_bootstraps_broker_session() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let task_text = "design auth broker";
    let task_id = suite_cli::broker_client::derive_task_id(task_text);
    seed_checkpointed_handoff_task(
        dir.path(),
        &task_id,
        "Continue design auth broker",
        "cp-bootstrap-1",
    );

    let output = agent_cmd()
        .current_dir(dir.path())
        .args([
            "--task",
            task_text,
            "--",
            "sh",
            "-c",
            "printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \"$PACKET28_TASK_ID\" \"$PACKET28_BROKER_BRIEF_PATH\" \"$PACKET28_BROKER_STATE_PATH\" \"$PACKET28_MCP_COMMAND\" \"$PACKET28_BROKER_WINDOW_MODE\" \"$PACKET28_BROKER_SUPERSESSION\" \"$PACKET28_BROKER_PREPARE_HANDOFF_TOOL\"",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 7);
    assert_eq!(lines[0], task_id);
    assert!(Path::new(&lines[1]).exists(), "brief path should exist");
    assert!(Path::new(&lines[2]).exists(), "state path should exist");
    assert!(lines[3].contains("Packet28 mcp serve --root"));
    assert_eq!(lines[4], "replace");
    assert_eq!(lines[5], "1");
    assert_eq!(lines[6], "packet28.prepare_handoff");

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_resumes_from_checkpoint_handoff() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    seed_checkpointed_handoff_task(
        dir.path(),
        "task-handoff-agent",
        "Resume from checkpointed Alpha investigation",
        "cp-agent-1",
    );

    let output = agent_cmd()
        .current_dir(dir.path())
        .args([
            "--wait-for-handoff",
            "--handoff-timeout-secs",
            "5",
            "--task-id",
            "task-handoff-agent",
            "--",
            "sh",
            "-c",
            "printf '%s\n%s\n%s\n%s\n%s\n%s\n' \"$PACKET28_BOOTSTRAP_MODE\" \"$PACKET28_BOOTSTRAP_PATH\" \"$PACKET28_HANDOFF_PATH\" \"$PACKET28_HANDOFF_ARTIFACT_ID\" \"$PACKET28_HANDOFF_CHECKPOINT_ID\" \"$PACKET28_BROKER_PREPARE_HANDOFF_TOOL\"",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[0], "handoff");
    assert!(Path::new(&lines[1]).exists(), "bootstrap path should exist");
    assert!(Path::new(&lines[2]).exists(), "handoff path should exist");
    assert!(
        !lines[3].is_empty(),
        "handoff artifact id should be exported"
    );
    assert_eq!(lines[4], "cp-agent-1");
    assert_eq!(lines[5], "packet28.prepare_handoff");

    let bootstrap: Value = serde_json::from_str(&fs::read_to_string(&lines[1]).unwrap()).unwrap();
    assert_eq!(
        bootstrap["latest_intention"]["text"],
        "Resume from checkpointed Alpha investigation"
    );
    assert_eq!(bootstrap["response_mode"], "full");

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_wait_for_handoff_times_out_when_checkpoint_missing() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    agent_cmd()
        .current_dir(dir.path())
        .args([
            "--wait-for-handoff",
            "--handoff-timeout-secs",
            "1",
            "--handoff-poll-ms",
            "50",
            "--task-id",
            "task-timeout-handoff",
            "--",
            "sh",
            "-c",
            "exit 0",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "timed out waiting for Packet28 handoff",
        ));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_daemon_task_await_handoff_reports_ready_status() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-await",
                    "op":"intention",
                    "text":"Prepare daemon-owned handoff wait",
                    "step_id":"investigating",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 2);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-await",
                    "op":"checkpoint_save",
                    "checkpoint_id":"cp-daemon-1",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 3);
    child.kill().unwrap();
    child.wait().unwrap();

    let output = suite_cmd()
        .args([
            "daemon",
            "task",
            "await-handoff",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-await",
            "--timeout-ms",
            "1000",
            "--poll-ms",
            "50",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["task_status"]["handoff_ready"], true);
    assert!(value["waited_ms"].as_u64().unwrap() <= 1_000);
    assert!(value["polls"].as_u64().unwrap() >= 1);

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_daemon_task_launch_agent_spawns_child_from_handoff() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-launch",
                    "op":"intention",
                    "text":"Launch fresh worker from daemon",
                    "step_id":"handoff",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 2);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-launch",
                    "op":"checkpoint_save",
                    "checkpoint_id":"cp-daemon-launch-1",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 3);
    child.kill().unwrap();
    child.wait().unwrap();

    let output = suite_cmd()
        .args([
            "daemon",
            "task",
            "launch-agent",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-launch",
            "--json",
            "--",
            "sh",
            "-c",
            "printf '%s\\n%s\\n' \"$PACKET28_BOOTSTRAP_MODE\" \"$PACKET28_TASK_ID\"",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let launch_value: Value = serde_json::from_slice(&output).unwrap();
    let log_path = launch_value["log_path"].as_str().unwrap();
    assert_eq!(launch_value["bootstrap_mode"], "handoff");
    assert!(launch_value["pid"].as_u64().unwrap() > 0);

    let mut log_contents = String::new();
    for _ in 0..40 {
        if let Ok(raw) = fs::read_to_string(log_path) {
            log_contents = raw;
            if log_contents.contains("handoff") && log_contents.contains("task-daemon-launch") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(log_contents.contains("handoff"));
    assert!(log_contents.contains("task-daemon-launch"));

    let status_output = suite_cmd()
        .args([
            "daemon",
            "task",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-launch",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_value: Value = serde_json::from_slice(&status_output).unwrap();
    assert_eq!(status_value["latest_agent_bootstrap_mode"], "handoff");
    assert_eq!(
        status_value["latest_agent_pid"].as_u64().unwrap(),
        launch_value["pid"].as_u64().unwrap()
    );
    assert_eq!(status_value["latest_agent_log_path"], log_path);
    assert_eq!(
        status_value["latest_agent_handoff_artifact_id"],
        launch_value["handoff_artifact_id"]
    );
    assert_eq!(
        status_value["latest_agent_handoff_checkpoint_id"],
        launch_value["handoff_checkpoint_id"]
    );
    assert!(status_value["latest_agent_context_version"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_daemon_task_await_handoff_can_require_newer_context_version() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-newer-handoff",
                    "op":"intention",
                    "text":"Prepare initial handoff",
                    "step_id":"handoff",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 2);
    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-newer-handoff",
                    "op":"checkpoint_save",
                    "checkpoint_id":"cp-daemon-newer-1",
                    "paths":["src/alpha.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 3);

    let launch_output = suite_cmd()
        .args([
            "daemon",
            "task",
            "launch-agent",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-newer-handoff",
            "--json",
            "--",
            "sh",
            "-c",
            "printf '%s\\n' \"$PACKET28_BOOTSTRAP_MODE\"",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let launch_value: Value = serde_json::from_slice(&launch_output).unwrap();
    let launched_context_version = suite_cmd()
        .args([
            "daemon",
            "task",
            "status",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-newer-handoff",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let launched_status: Value = serde_json::from_slice(&launched_context_version).unwrap();
    let previous_context_version = launched_status["latest_agent_context_version"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(launch_value["bootstrap_mode"], "handoff");

    suite_cmd()
        .args([
            "daemon",
            "task",
            "await-handoff",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-newer-handoff",
            "--after-context-version",
            &previous_context_version,
            "--timeout-ms",
            "100",
            "--poll-ms",
            "20",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "newer handoff than context version",
        ));

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-daemon-newer-handoff",
                    "op":"intention",
                    "text":"Resume from a newer handoff",
                    "step_id":"editing",
                    "paths":["src/beta.rs"]
                }
            }
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 4);
    child.kill().unwrap();
    child.wait().unwrap();

    let output = suite_cmd()
        .args([
            "daemon",
            "task",
            "await-handoff",
            "--root",
            dir.path().to_str().unwrap(),
            "--task-id",
            "task-daemon-newer-handoff",
            "--after-context-version",
            &previous_context_version,
            "--timeout-ms",
            "1000",
            "--poll-ms",
            "50",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["task_status"]["handoff_ready"], true);
    assert_ne!(
        value["task_status"]["latest_context_version"]
            .as_str()
            .unwrap(),
        previous_context_version
    );

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_doctor_reports_healthy_stack() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    git(dir.path(), &["add", "src/alpha.rs", "src/beta.rs"]);
    git(
        dir.path(),
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

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_mcp_prepare_handoff_requires_checkpoint_and_persists_artifact() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-handoff",
                    "op":"intention",
                    "text":"Inspect Alpha before editing it",
                    "note":"Need a clean resume breadcrumb for the next worker",
                    "step_id":"investigating",
                    "paths":["src/alpha.rs"],
                    "symbols":["Alpha"]
                }
            }
        }),
    );
    let intention = read_mcp_message_for_id(&mut stdout, 2);
    assert_eq!(intention["result"]["structuredContent"]["accepted"], true);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.prepare_handoff",
                "arguments":{
                    "task_id":"task-handoff"
                }
            }
        }),
    );
    let not_ready = read_mcp_message_for_id(&mut stdout, 3);
    let not_ready_payload = &not_ready["result"]["structuredContent"];
    assert_eq!(not_ready_payload["handoff_ready"], false);
    assert!(not_ready_payload["context"].is_null());

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-handoff",
                    "op":"checkpoint_save",
                    "checkpoint_id":"cp-1",
                    "note":"Ready for manual worker relaunch",
                    "paths":["src/alpha.rs"],
                    "symbols":["Alpha"]
                }
            }
        }),
    );
    let checkpoint = read_mcp_message_for_id(&mut stdout, 4);
    assert_eq!(checkpoint["result"]["structuredContent"]["accepted"], true);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"tools/call",
            "params":{
                "name":"packet28.prepare_handoff",
                "arguments":{
                    "task_id":"task-handoff",
                    "response_mode":"slim"
                }
            }
        }),
    );
    let handoff = read_mcp_message_for_id(&mut stdout, 5);
    let handoff_payload = &handoff["result"]["structuredContent"];
    assert_eq!(handoff_payload["handoff_ready"], true);
    assert_eq!(handoff_payload["latest_checkpoint_id"], "cp-1");
    assert_eq!(
        handoff_payload["latest_intention"]["text"],
        "Inspect Alpha before editing it"
    );
    let handoff_context = &handoff_payload["context"];
    assert_eq!(handoff_context["response_mode"], "slim");
    assert_eq!(handoff_context["handoff_ready"], true);
    assert!(handoff_context["brief"]
        .as_str()
        .unwrap()
        .contains("Latest Intention"));
    let handoff_artifact_id = handoff_context["artifact_id"].as_str().unwrap().to_string();

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":6,
            "method":"tools/call",
            "params":{
                "name":"packet28.fetch_context",
                "arguments":{
                    "task_id":"task-handoff",
                    "artifact_id": handoff_artifact_id
                }
            }
        }),
    );
    let fetched = read_mcp_message_for_id(&mut stdout, 6);
    let fetched_payload = &fetched["result"]["structuredContent"];
    assert_eq!(fetched_payload["response_mode"], "full");
    assert_eq!(
        fetched_payload["latest_intention"]["step_id"],
        "investigating"
    );
    assert!(fetched_payload["sections"]
        .as_array()
        .unwrap()
        .iter()
        .any(|section| section["id"] == "agent_intention"));

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":7,
            "method":"tools/call",
            "params":{
                "name":"packet28.task_status",
                "arguments":{
                    "task_id":"task-handoff"
                }
            }
        }),
    );
    let status = read_mcp_message_for_id(&mut stdout, 7);
    let status_payload = &status["result"]["structuredContent"];
    assert_eq!(status_payload["handoff_ready"], false);
    assert_eq!(status_payload["latest_handoff_checkpoint_id"], "cp-1");
    assert_eq!(
        status_payload["latest_handoff_artifact_id"],
        handoff_context["artifact_id"]
    );

    child.kill().unwrap();
    child.wait().unwrap();

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_mcp_native_read_auto_captures_regions() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());
    git(dir.path(), &["add", "src/alpha.rs", "src/beta.rs"]);
    git(
        dir.path(),
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
    write_cached_coverage_state(dir.path());
    write_cached_testmap_state(dir.path());

    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let initialize = read_mcp_message_for_id(&mut stdout, 1);
    assert_eq!(initialize["result"]["serverInfo"]["name"], "Packet28");

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params":{
                "name":"packet28.read_regions",
                "arguments":{
                    "task_id":"task-native-read",
                    "path":"src/alpha.rs",
                    "regions":["src/alpha.rs:4-5"]
                }
            }
        }),
    );
    let read = read_mcp_message_for_id(&mut stdout, 2);
    assert!(
        read.get("error").is_none(),
        "native read returned MCP error: {read}"
    );
    let read_payload = &read["result"]["structuredContent"];
    assert_eq!(read_payload["path"].as_str().unwrap(), "src/alpha.rs");
    assert!(read_payload["regions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "src/alpha.rs:4-5"));
    assert_eq!(read_payload["lines"].as_array().unwrap().len(), 2);
    assert!(read_payload["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "Alpha"));

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_state",
                "arguments":{
                    "task_id":"task-native-read",
                    "op":"checkpoint_save",
                    "checkpoint_id":"cp-native-read",
                    "note":"Checkpoint before preparing handoff for read_regions evidence"
                }
            }
        }),
    );
    let checkpoint = read_mcp_message_for_id(&mut stdout, 3);
    assert_eq!(checkpoint["result"]["structuredContent"]["accepted"], true);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{
                "name":"packet28.prepare_handoff",
                "arguments":{
                    "task_id":"task-native-read",
                    "query":"Where is Alpha defined?",
                    "response_mode":"full"
                }
            }
        }),
    );
    let inspect = read_mcp_message_for_id(&mut stdout, 4);
    let inspect_payload = &inspect["result"]["structuredContent"]["context"];
    assert!(inspect["result"]["structuredContent"]["handoff_ready"]
        .as_bool()
        .unwrap());
    assert!(inspect_payload["recent_tool_invocations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| {
            item["tool_name"] == "packet28.read_regions"
                && item["regions"].as_array().is_some_and(|regions| {
                    regions.iter().any(|region| region == "src/alpha.rs:4-5")
                })
        }));
    assert!(inspect_payload["discovered_paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "src/alpha.rs"));
    child.kill().unwrap();
    child.wait().unwrap();

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_mcp_accepts_newline_json_stdio() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());

    let (mut child, mut stdin, mut stdout) = start_mcp_server(dir.path());

    write_mcp_message_newline(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{}},"clientInfo":{"name":"claude-code","version":"2.1.72"}}
        }),
    );
    let initialize = read_mcp_message_newline(&mut stdout);
    assert_eq!(initialize["result"]["serverInfo"]["name"], "Packet28");
    assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
    assert!(initialize["result"]["capabilities"]["experimental"].is_null());

    write_mcp_message_newline(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/list"
        }),
    );
    let tools = read_mcp_message_newline(&mut stdout);
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28.search"));
    assert!(!tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "packet28.sync"));

    let _ = child.kill();
}

#[test]
#[cfg(unix)]
fn test_packet28_mcp_proxy_namespaces_colliding_tools() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let script_alpha = dir.path().join("alpha_mcp.py");
    fs::write(
        &script_alpha,
        r#"import json, sys

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode("utf-8").split(":", 1)
        headers[name.lower().strip()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    return json.loads(body)

def write_message(value):
    body = json.dumps(value).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    msg_id = message.get("id")
    if msg_id is None:
        continue
    method = message.get("method")
    params = message.get("params", {})
    if method == "initialize":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "alpha", "version": "1"}}})
    elif method == "tools/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"tools": [{"name": "shared.read", "description": "alpha shared tool", "inputSchema": {"type": "object", "properties": {}}}]}})
    elif method == "resources/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"resources": []}})
    elif method == "resources/templates/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"resourceTemplates": []}})
    elif method == "tools/call":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": "alpha ok"}], "structuredContent": {"owner": "alpha"}}})
    else:
        write_message({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "unknown method"}})
"#,
    )
    .unwrap();

    let script_beta = dir.path().join("beta_mcp.py");
    fs::write(
        &script_beta,
        r#"import json, sys

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode("utf-8").split(":", 1)
        headers[name.lower().strip()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    return json.loads(body)

def write_message(value):
    body = json.dumps(value).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    msg_id = message.get("id")
    if msg_id is None:
        continue
    method = message.get("method")
    params = message.get("params", {})
    if method == "initialize":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "beta", "version": "1"}}})
    elif method == "tools/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"tools": [{"name": "shared.read", "description": "beta shared tool", "inputSchema": {"type": "object", "properties": {}}}]}})
    elif method == "resources/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"resources": []}})
    elif method == "resources/templates/list":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"resourceTemplates": []}})
    elif method == "tools/call":
        write_message({"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": "beta ok"}], "structuredContent": {"owner": "beta"}}})
    else:
        write_message({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "unknown method"}})
"#,
    )
    .unwrap();

    let config_path = dir.path().join(".mcp.proxy.json");
    fs::write(
        &config_path,
        json!({
            "mcpServers": {
                "alpha": {
                    "command": "python3",
                    "args": ["-u", script_alpha.to_str().unwrap()]
                },
                "beta": {
                    "command": "python3",
                    "args": ["-u", script_beta.to_str().unwrap()]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let (mut child, mut stdin, mut stdout) =
        start_mcp_proxy_server(dir.path(), &config_path, "task-proxy-collision");

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/list"
        }),
    );
    let tools = read_mcp_message_for_id(&mut stdout, 2);
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "alpha.shared.read"));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "beta.shared.read"));

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"beta.shared.read",
                "arguments":{}
            }
        }),
    );
    let response = read_mcp_message_for_id(&mut stdout, 3);
    assert_eq!(
        response["result"]["structuredContent"]["owner"]
            .as_str()
            .unwrap(),
        "beta"
    );

    child.kill().unwrap();
    child.wait().unwrap();

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
fn test_packet28_mcp_proxy_caches_tool_catalog_and_respects_timeout_ms() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    init_repo(dir.path());
    write_repo_fixture(dir.path());

    let counter_path = dir.path().join("tools-list-count.txt");
    let script_path = dir.path().join("slow_mcp.py");
    fs::write(
        &script_path,
        format!(
            r#"import json, pathlib, sys, time

COUNTER = pathlib.Path({counter:?})

def read_message():
    headers = {{}}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode("utf-8").split(":", 1)
        headers[name.lower().strip()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    return json.loads(body)

def write_message(value):
    body = json.dumps(value).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {{len(body)}}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    msg_id = message.get("id")
    if msg_id is None:
        continue
    method = message.get("method")
    if method == "initialize":
        write_message({{"jsonrpc": "2.0", "id": msg_id, "result": {{"protocolVersion": "2024-11-05", "capabilities": {{"tools": {{}}, "resources": {{}}}}, "serverInfo": {{"name": "slow", "version": "1"}}}}}})
    elif method == "tools/list":
        count = 0
        if COUNTER.exists():
            count = int(COUNTER.read_text() or "0")
        COUNTER.write_text(str(count + 1))
        write_message({{"jsonrpc": "2.0", "id": msg_id, "result": {{"tools": [{{"name": "slow.read", "description": "slow tool", "inputSchema": {{"type": "object", "properties": {{}}}}}}]}}}})
    elif method == "resources/list":
        write_message({{"jsonrpc": "2.0", "id": msg_id, "result": {{"resources": []}}}})
    elif method == "resources/templates/list":
        write_message({{"jsonrpc": "2.0", "id": msg_id, "result": {{"resourceTemplates": []}}}})
    elif method == "tools/call":
        time.sleep(0.2)
        write_message({{"jsonrpc": "2.0", "id": msg_id, "result": {{"content": [{{"type": "text", "text": "slow ok"}}]}}}})
    else:
        write_message({{"jsonrpc": "2.0", "id": msg_id, "error": {{"code": -32601, "message": "unknown method"}}}})
"#,
            counter = counter_path,
        ),
    )
    .unwrap();

    let config_path = dir.path().join(".mcp.proxy.json");
    fs::write(
        &config_path,
        json!({
            "mcpServers": {
                "slow": {
                    "command": "python3",
                    "args": ["-u", script_path.to_str().unwrap()],
                    "timeout_ms": 50
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let (mut child, mut stdin, mut stdout) =
        start_mcp_proxy_server(dir.path(), &config_path, "task-proxy-timeout");

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }),
    );
    let _ = read_mcp_message_for_id(&mut stdout, 1);

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/list"
        }),
    );
    let tools = read_mcp_message_for_id(&mut stdout, 2);
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "slow.read"));
    assert_eq!(fs::read_to_string(&counter_path).unwrap().trim(), "1");

    write_mcp_message(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"slow.read",
                "arguments":{}
            }
        }),
    );
    let timeout = read_mcp_message_for_id(&mut stdout, 3);
    assert!(timeout["error"]["message"]
        .as_str()
        .unwrap()
        .contains("50ms"));
    assert!(timeout["error"]["message"]
        .as_str()
        .unwrap()
        .contains("python3 -u"));
    assert_eq!(fs::read_to_string(&counter_path).unwrap().trim(), "1");

    child.kill().unwrap();
    child.wait().unwrap();

    suite_cmd()
        .args(["daemon", "stop", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
#[cfg(unix)]
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
        assert!(rendered.contains("Packet28 mcp serve"));
        assert!(rendered.contains("packet28.search"));
        assert!(rendered.contains("packet28.read_regions"));
        assert!(rendered.contains("packet28.prepare_handoff"));
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
fn test_packet28_agent_persists_bootstrap_and_exports_env() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let task_text = "trace Alpha";
    let task_id = suite_cli::broker_client::derive_task_id(task_text);
    seed_checkpointed_handoff_task(
        dir.path(),
        &task_id,
        "Trace Alpha from checkpointed handoff",
        "cp-env-1",
    );
    let env_dump = dir.path().join("env.txt");

    agent_cmd()
        .current_dir(dir.path())
        .args([
            "--task",
            task_text,
            "--",
            "sh",
            "-c",
            "printf '%s\\n%s\\n' \"$PACKET28_ROOT\" \"$PACKET28_BOOTSTRAP_PATH\" > \"$1\"",
            "sh",
            env_dump.to_str().unwrap(),
        ])
        .assert()
        .success();

    let persisted_path = dir
        .path()
        .join(".packet28")
        .join("agent")
        .join("latest-bootstrap.json");
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

    let value = parse_broker_response(&fs::read(&persisted_path).unwrap());
    assert!(value["brief"].as_str().unwrap().contains("Task Objective"));
    assert!(value["sections"].as_array().unwrap().len() >= 2);
}

#[test]
#[cfg(unix)]
fn test_packet28_agent_returns_child_exit_code() {
    let dir = TempDir::new().unwrap();
    setup_changed_repo(dir.path());
    let task_text = "trace Alpha";
    let task_id = suite_cli::broker_client::derive_task_id(task_text);
    seed_checkpointed_handoff_task(
        dir.path(),
        &task_id,
        "Trace Alpha and preserve child exit code",
        "cp-exit-1",
    );

    agent_cmd()
        .current_dir(dir.path())
        .args(["--task", task_text, "--", "sh", "-c", "exit 7"])
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
fn test_suite_daemon_suppresses_disconnect_log_noise() {
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
    let socket = PathBuf::from(status.get("socket_path").and_then(Value::as_str).unwrap());
    let start = std::time::Instant::now();
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(stream) => break stream,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    && start.elapsed() < std::time::Duration::from_secs(15) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => panic!(
                "failed to connect to daemon socket {}: {err}",
                socket.display()
            ),
        }
    };
    packet28_daemon_core::write_socket_message(
        &mut stream,
        &packet28_daemon_core::DaemonRequest::Status,
    )
    .unwrap();
    drop(stream);

    std::thread::sleep(std::time::Duration::from_millis(300));

    let log_path = dir.path().join(".packet28/daemon/packet28d.log");
    let start = std::time::Instant::now();
    while !log_path.exists() && start.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let log = fs::read_to_string(&log_path).unwrap();
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
