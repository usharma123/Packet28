use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn covy_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("covy")
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
        .args([
            "check", "--base", "HEAD", "--head", "HEAD", "--report", "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("passed"));
}

#[test]
fn test_check_without_coverage_and_state_fails() {
    let dir = TempDir::new().unwrap();

    covy_cmd()
        .current_dir(dir.path())
        .args([
            "check", "--base", "HEAD", "--head", "HEAD", "--report", "json",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No coverage files specified and no cached coverage state found",
        ));
}

#[test]
fn test_testmap_build_writes_test_to_files_index() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let output = dir.path().join("testmap.bin");

    let line = format!(
        "{{\"test_id\":\"com.foo.BarTest\",\"language\":\"java\",\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(&manifest, line).unwrap();

    covy_cmd()
        .args([
            "testmap",
            "build",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let map = covy_core::cache::deserialize_testmap(&bytes).unwrap();
    assert!(map.test_to_files.contains_key("com.foo.BarTest"));
    assert!(!map.test_to_files["com.foo.BarTest"].is_empty());
    let covered_file = map.test_to_files["com.foo.BarTest"]
        .iter()
        .next()
        .unwrap()
        .clone();
    assert!(map.file_to_tests.contains_key(&covered_file));
    assert!(map.file_to_tests[&covered_file].contains("com.foo.BarTest"));
    assert!(map.metadata.generated_at > 0);
}

#[test]
fn test_impact_json_runs_with_diff_integration() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");

    let line = format!(
        "{{\"test_id\":\"com.foo.BarTest\",\"language\":\"java\",\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(&manifest, line).unwrap();

    covy_cmd()
        .args([
            "testmap",
            "build",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            testmap.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
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
fn test_impact_record_builds_v2_testmap() {
    let dir = TempDir::new().unwrap();
    let per_test_dir = dir.path().join("per-test-lcov");
    std::fs::create_dir_all(&per_test_dir).unwrap();
    std::fs::copy(
        fixture("lcov/basic.info"),
        per_test_dir.join("com.foo.BarTest.info"),
    )
    .unwrap();

    let testmap = dir.path().join("testmap.bin");
    let summary = dir.path().join("testmap.json");

    covy_cmd()
        .args([
            "impact",
            "record",
            "--base-ref",
            "HEAD",
            "--out",
            testmap.to_str().unwrap(),
            "--per-test-lcov-dir",
            per_test_dir.to_str().unwrap(),
            "--summary-json",
            summary.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(testmap.exists());
    assert!(summary.exists());

    let bytes = std::fs::read(&testmap).unwrap();
    let map = covy_core::cache::deserialize_testmap(&bytes).unwrap();
    assert_eq!(
        map.metadata.schema_version,
        covy_core::cache::TESTMAP_SCHEMA_VERSION
    );
    assert!(!map.tests.is_empty());
    assert!(!map.file_index.is_empty());
    assert_eq!(map.tests.len(), map.coverage.len());
}

#[test]
fn test_impact_plan_outputs_stable_json_schema() {
    let dir = TempDir::new().unwrap();
    let per_test_dir = dir.path().join("per-test-lcov");
    std::fs::create_dir_all(&per_test_dir).unwrap();
    std::fs::copy(
        fixture("lcov/basic.info"),
        per_test_dir.join("com.foo.BarTest.info"),
    )
    .unwrap();

    let testmap = dir.path().join("testmap.bin");

    covy_cmd()
        .args([
            "impact",
            "record",
            "--base-ref",
            "HEAD",
            "--out",
            testmap.to_str().unwrap(),
            "--per-test-lcov-dir",
            per_test_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "impact",
            "plan",
            "--base-ref",
            "HEAD",
            "--head-ref",
            "HEAD",
            "--testmap",
            testmap.to_str().unwrap(),
            "--max-tests",
            "5",
            "--target-coverage",
            "0.9",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"changed_lines_total\""))
        .stdout(predicate::str::contains(
            "\"changed_lines_covered_by_plan\"",
        ))
        .stdout(predicate::str::contains("\"plan_coverage_pct\""))
        .stdout(predicate::str::contains("\"tests\""))
        .stdout(predicate::str::contains("\"uncovered_blocks\""))
        .stdout(predicate::str::contains("\"next_command\""));
}

#[test]
fn test_impact_print_command_outputs_helper() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let testmap = dir.path().join("testmap.bin");

    let line = format!(
        "{{\"test_id\":\"com.foo.BarTest\",\"language\":\"java\",\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(&manifest, line).unwrap();

    covy_cmd()
        .args([
            "testmap",
            "build",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            testmap.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "impact",
            "--base",
            "HEAD",
            "--head",
            "HEAD",
            "--testmap",
            testmap.to_str().unwrap(),
            "--print-command",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo \"no impacted tests\""));
}

#[test]
fn test_shard_plan_json_and_file_outputs() {
    let dir = TempDir::new().unwrap();
    let tests_file = dir.path().join("tests.txt");
    let out_dir = dir.path().join("shards");
    std::fs::write(&tests_file, "t1\nt2\nt3\n").unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tests-file",
            tests_file.to_str().unwrap(),
            "--write-files",
            out_dir.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"shards\""))
        .stdout(predicate::str::contains("\"imbalance_ratio\""))
        .stdout(predicate::str::contains("\"parallel_efficiency\""));

    assert!(out_dir.join("shard-1.txt").exists());
    assert!(out_dir.join("shard-2.txt").exists());
}

#[test]
fn test_merge_non_strict_skips_corrupt_artifacts() {
    let dir = TempDir::new().unwrap();
    let bad = dir.path().join("bad.bin");
    std::fs::write(&bad, b"broken").unwrap();

    covy_cmd()
        .args([
            "merge",
            "--coverage",
            bad.to_str().unwrap(),
            "--strict",
            "false",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"skipped_inputs\": 1"))
        .stdout(predicate::str::contains("\"strict_mode\": false"))
        .stdout(predicate::str::contains("\"output_coverage_path\""));
}

#[test]
fn test_merge_strict_fails_on_corrupt_artifacts() {
    let dir = TempDir::new().unwrap();
    let bad = dir.path().join("bad.bin");
    std::fs::write(&bad, b"broken").unwrap();

    covy_cmd()
        .args([
            "merge",
            "--coverage",
            bad.to_str().unwrap(),
            "--strict",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to merge coverage input"));
}

#[test]
fn test_merge_writes_output_coverage_state() {
    let dir = TempDir::new().unwrap();
    let shard = dir.path().join("shard.bin");
    let merged = dir.path().join("merged.bin");

    covy_cmd()
        .args([
            "ingest",
            &fixture("lcov/basic.info"),
            "--output",
            shard.to_str().unwrap(),
        ])
        .assert()
        .success();

    covy_cmd()
        .args([
            "merge",
            "--coverage",
            shard.to_str().unwrap(),
            "--output-coverage",
            merged.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    assert!(merged.exists());
}

#[test]
fn test_merge_writes_output_issues_state() {
    let dir = TempDir::new().unwrap();
    let shard = dir.path().join("issues-shard.bin");
    let merged = dir.path().join("issues-merged.bin");

    covy_cmd()
        .current_dir(dir.path())
        .args(["ingest", "--issues", &fixture("sarif/basic.sarif")])
        .assert()
        .success();

    std::fs::copy(dir.path().join(".covy/state/issues.bin"), &shard).unwrap();

    covy_cmd()
        .args([
            "merge",
            "--issues",
            shard.to_str().unwrap(),
            "--output-issues",
            merged.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    assert!(merged.exists());
}

#[test]
fn test_testmap_build_supports_python_language_metadata() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let output = dir.path().join("testmap.bin");

    let line = format!(
        "{{\"test_id\":\"tests/test_mod.py::test_case\",\"language\":\"python\",\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(&manifest, line).unwrap();

    covy_cmd()
        .args([
            "testmap",
            "build",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&output).unwrap();
    let map = covy_core::cache::deserialize_testmap(&bytes).unwrap();
    assert_eq!(
        map.test_language["tests/test_mod.py::test_case"],
        "python".to_string()
    );
}

#[test]
fn test_testmap_build_writes_timings_output() {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("manifest.jsonl");
    let output = dir.path().join("testmap.bin");
    let timings_output = dir.path().join("testtimings.bin");

    let line = format!(
        "{{\"test_id\":\"com.foo.BarTest\",\"language\":\"java\",\"duration_ms\":1234,\"coverage_report\":\"{}\"}}\n",
        fixture("lcov/basic.info")
    );
    std::fs::write(&manifest, line).unwrap();

    covy_cmd()
        .args([
            "testmap",
            "build",
            "--manifest",
            manifest.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--timings-output",
            timings_output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&timings_output).unwrap();
    let timings = covy_core::cache::deserialize_test_timings(&bytes).unwrap();
    assert_eq!(timings.duration_ms.get("com.foo.BarTest"), Some(&1234));
    assert_eq!(timings.sample_count.get("com.foo.BarTest"), Some(&1));
}

#[test]
fn test_shard_plan_supports_python_nodeids() {
    let dir = TempDir::new().unwrap();
    let tests_file = dir.path().join("py-tests.txt");
    std::fs::write(
        &tests_file,
        "tests/test_mod.py::test_one\ntests/test_mod.py::test_two\n",
    )
    .unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tests-file",
            tests_file.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("tests/test_mod.py::test_one"))
        .stdout(predicate::str::contains("tests/test_mod.py::test_two"));
}

#[test]
fn test_shard_plan_supports_tasks_json() {
    let dir = TempDir::new().unwrap();
    let tasks_file = dir.path().join("tasks.json");
    let payload = serde_json::json!({
        "schema_version": 1,
        "tasks": [
            {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1000},
            {"id": "tests/test_mod.py::test_one", "selector": "tests/test_mod.py::test_one", "est_ms": 800}
        ]
    });
    std::fs::write(&tasks_file, serde_json::to_string(&payload).unwrap()).unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tasks-json",
            tasks_file.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("com.foo.BarTest"))
        .stdout(predicate::str::contains("tests/test_mod.py::test_one"));
}

#[test]
fn test_shard_plan_accepts_whale_lpt_algorithm() {
    let dir = TempDir::new().unwrap();
    let tests_file = dir.path().join("tests.txt");
    std::fs::write(&tests_file, "com.foo.A\ncom.foo.B\ncom.foo.C\n").unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tests-file",
            tests_file.to_str().unwrap(),
            "--algorithm",
            "whale-lpt",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"shards\""))
        .stdout(predicate::str::contains("com.foo.A"));
}

#[test]
fn test_shard_plan_rejects_invalid_algorithm() {
    let dir = TempDir::new().unwrap();
    let tests_file = dir.path().join("tests.txt");
    std::fs::write(&tests_file, "com.foo.A\n").unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "1",
            "--tests-file",
            tests_file.to_str().unwrap(),
            "--algorithm",
            "bad-algo",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value"));
}

#[test]
fn test_shard_plan_pr_tier_excludes_slow_tagged_tasks() {
    let dir = TempDir::new().unwrap();
    let tasks_file = dir.path().join("tasks.json");
    let payload = serde_json::json!({
        "schema_version": 1,
        "tasks": [
            {"id": "fast-test", "selector": "fast-test", "est_ms": 1000, "tags": ["unit"]},
            {"id": "slow-test", "selector": "slow-test", "est_ms": 2000, "tags": ["slow"]}
        ]
    });
    std::fs::write(&tasks_file, serde_json::to_string(&payload).unwrap()).unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tasks-json",
            tasks_file.to_str().unwrap(),
            "--tier",
            "pr",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast-test"))
        .stdout(predicate::str::contains("slow-test").not());
}

#[test]
fn test_shard_plan_nightly_tier_keeps_slow_tagged_tasks() {
    let dir = TempDir::new().unwrap();
    let tasks_file = dir.path().join("tasks.json");
    let payload = serde_json::json!({
        "schema_version": 1,
        "tasks": [
            {"id": "fast-test", "selector": "fast-test", "est_ms": 1000, "tags": ["unit"]},
            {"id": "slow-test", "selector": "slow-test", "est_ms": 2000, "tags": ["slow"]}
        ]
    });
    std::fs::write(&tasks_file, serde_json::to_string(&payload).unwrap()).unwrap();

    covy_cmd()
        .args([
            "shard",
            "plan",
            "--shards",
            "2",
            "--tasks-json",
            tasks_file.to_str().unwrap(),
            "--tier",
            "nightly",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast-test"))
        .stdout(predicate::str::contains("slow-test"));
}

#[test]
fn test_shard_update_ingests_jsonl_timings() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("timings.jsonl");
    let timings_bin = dir.path().join("testtimings.bin");
    std::fs::write(
        &jsonl,
        "{\"test_id\":\"com.foo.BarTest\",\"duration_ms\":1200}\n{\"test_id\":\"tests/test_mod.py::test_one\",\"duration_ms\":900}\n",
    )
    .unwrap();

    covy_cmd()
        .args([
            "shard",
            "update",
            "--timings-jsonl",
            jsonl.to_str().unwrap(),
            "--timings",
            timings_bin.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tests_updated\": 2"));

    let bytes = std::fs::read(&timings_bin).unwrap();
    let timings = covy_core::cache::deserialize_test_timings(&bytes).unwrap();
    assert_eq!(timings.duration_ms.get("com.foo.BarTest"), Some(&1200));
    assert_eq!(
        timings.duration_ms.get("tests/test_mod.py::test_one"),
        Some(&900)
    );
}

#[test]
fn test_shard_update_ingests_junit_xml_timings() {
    let dir = TempDir::new().unwrap();
    let junit = dir.path().join("junit.xml");
    let timings_bin = dir.path().join("testtimings.bin");
    std::fs::write(
        &junit,
        r#"<testsuite><testcase classname="com.foo.BarTest" name="testOne" time="0.250"/></testsuite>"#,
    )
    .unwrap();

    covy_cmd()
        .args([
            "shard",
            "update",
            "--junit-xml",
            junit.to_str().unwrap(),
            "--timings",
            timings_bin.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tests_updated\": 1"));

    let bytes = std::fs::read(&timings_bin).unwrap();
    let timings = covy_core::cache::deserialize_test_timings(&bytes).unwrap();
    assert_eq!(
        timings.duration_ms.get("com.foo.BarTest.testOne"),
        Some(&250)
    );
}

#[test]
fn test_shard_update_ingests_junit_xml_timings_by_class() {
    let dir = TempDir::new().unwrap();
    let junit = dir.path().join("junit.xml");
    let timings_bin = dir.path().join("testtimings.bin");
    std::fs::write(
        &junit,
        r#"<testsuite>
            <testcase classname="com.foo.BarTest" name="testOne" time="0.250"/>
            <testcase classname="com.foo.BarTest" name="testTwo" time="0.150"/>
          </testsuite>"#,
    )
    .unwrap();

    covy_cmd()
        .args([
            "shard",
            "update",
            "--junit-xml",
            junit.to_str().unwrap(),
            "--timings",
            timings_bin.to_str().unwrap(),
            "--junit-id-granularity",
            "class",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"tests_updated\": 1"));

    let bytes = std::fs::read(&timings_bin).unwrap();
    let timings = covy_core::cache::deserialize_test_timings(&bytes).unwrap();
    assert_eq!(timings.duration_ms.get("com.foo.BarTest"), Some(&400));
    assert!(timings.duration_ms.get("com.foo.BarTest.testOne").is_none());
}

#[test]
fn test_shard_update_rejects_invalid_junit_id_granularity() {
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("timings.jsonl");
    let timings_bin = dir.path().join("testtimings.bin");
    std::fs::write(
        &jsonl,
        "{\"test_id\":\"com.foo.BarTest\",\"duration_ms\":1200}\n",
    )
    .unwrap();

    covy_cmd()
        .args([
            "shard",
            "update",
            "--timings-jsonl",
            jsonl.to_str().unwrap(),
            "--timings",
            timings_bin.to_str().unwrap(),
            "--junit-id-granularity",
            "suite",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value"));
}
