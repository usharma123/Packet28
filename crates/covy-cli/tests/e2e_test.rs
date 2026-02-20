use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn covy_cmd() -> Command {
    Command::cargo_bin("covy").unwrap()
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

#[test]
fn test_help() {
    covy_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Universal code coverage tool"));
}

#[test]
fn test_ingest_lcov() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("coverage.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("lcov/basic.info"),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.exists());
}

#[test]
fn test_ingest_then_report() {
    let dir = TempDir::new().unwrap();
    let state_file = dir.path().join("state.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("lcov/basic.info"),
            "--output",
            state_file.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "report",
            "--input",
            state_file.to_str().unwrap(),
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/main.rs"));

    covy_cmd()
        .args([
            "report",
            "--input",
            state_file.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("total_coverage_pct"));
}

#[test]
fn test_ingest_cobertura() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("coverage.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("cobertura/basic.xml"),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.exists());
}

#[test]
fn test_ingest_jacoco() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("coverage.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("jacoco/basic.xml"),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.exists());
}

#[test]
fn test_ingest_gocov() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("coverage.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("gocov/basic.out"),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.exists());
}

#[test]
fn test_report_no_data() {
    covy_cmd()
        .args(["report", "--input", "/nonexistent/path.bin"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No coverage data found"));
}

#[test]
fn test_report_min_coverage_fail() {
    let dir = TempDir::new().unwrap();
    let state_file = dir.path().join("state.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("lcov/basic.info"),
            "--output",
            state_file.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "report",
            "--input",
            state_file.to_str().unwrap(),
            "--min-coverage",
            "95.0",
            "--color",
            "never",
        ])
        .assert()
        .code(1);
}

#[test]
fn test_ingest_with_strip_prefix() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("coverage.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("lcov/basic.info"),
            "--output",
            output.to_str().unwrap(),
            "--strip-prefix",
            "src/",
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "report",
            "--input",
            output.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("main.rs"))
        .stdout(predicate::str::contains("lib.rs"));
}

#[test]
fn test_ingest_issues_creates_state_file() {
    let dir = TempDir::new().unwrap();

    covy_cmd()
        .current_dir(dir.path())
        .args(["ingest", "--issues", &fixture("sarif/basic.sarif")])
        .assert()
        .success();

    assert!(dir.path().join(".covy/state/issues.bin").exists());
}

#[test]
fn test_check_with_issues_flag() {
    covy_cmd()
        .args([
            "check",
            &fixture("lcov/basic.info"),
            "--issues",
            &fixture("sarif/basic.sarif"),
            "--max-new-errors",
            "0",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--report",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("issue_counts"));
}

#[test]
fn test_check_without_issues_still_works() {
    covy_cmd()
        .args([
            "check",
            &fixture("lcov/basic.info"),
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--report",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("passed"));
}

#[test]
fn test_check_loads_issues_from_state_by_default() {
    covy_cmd()
        .args(["ingest", "--issues", &fixture("sarif/basic.sarif")])
        .assert()
        .success();

    covy_cmd()
        .args([
            "check",
            &fixture("lcov/basic.info"),
            "--max-new-errors",
            "0",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--report",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("issue_counts"));
}

#[test]
fn test_check_can_disable_state_issues_loading() {
    covy_cmd()
        .args(["ingest", "--issues", &fixture("sarif/basic.sarif")])
        .assert()
        .success();

    covy_cmd()
        .args([
            "check",
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
        .stdout(predicate::str::contains("issue_counts").not());
}

#[test]
fn test_check_accepts_packed_issues_input() {
    covy_cmd()
        .args(["ingest", "--issues", &fixture("sarif/basic.sarif")])
        .assert()
        .success();

    covy_cmd()
        .args([
            "check",
            &fixture("lcov/basic.info"),
            "--issues",
            ".covy/state/issues.bin",
            "--max-new-errors",
            "0",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--report",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("issue_counts"));
}

#[test]
fn test_check_loads_coverage_from_state_by_default() {
    covy_cmd()
        .args(["ingest", &fixture("lcov/basic.info")])
        .assert()
        .success();

    covy_cmd()
        .args(["check", "--base", "HEAD", "--head", "HEAD", "--report", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("passed"));
}

#[test]
fn test_check_without_coverage_and_state_fails() {
    let dir = TempDir::new().unwrap();

    covy_cmd()
        .current_dir(dir.path())
        .args(["check", "--base", "HEAD", "--head", "HEAD", "--report", "json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No coverage files specified and no cached coverage state found",
        ));
}
