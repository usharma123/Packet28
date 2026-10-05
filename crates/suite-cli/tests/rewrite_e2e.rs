use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

#[test]
fn test_top_level_rewrite_plans_supported_command() {
    let root = TempDir::new().unwrap();
    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "git",
            "status",
            "--short",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"reducer_rewrite\""))
        .stdout(predicate::str::contains("\"reducer_family\":\"git\""));

    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "gradle",
            "test",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"reducer_rewrite\""))
        .stdout(predicate::str::contains("\"reducer_family\":\"jvm\""));
}

#[test]
fn test_top_level_rewrite_respects_repo_exclude_config() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("covy.toml"),
        "[packet28.rewrite]\nexclude_commands = [\"git\"]\n",
    )
    .unwrap();
    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "git",
            "status",
            "--short",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"raw_passthrough\""))
        .stdout(predicate::str::contains("\"reason\":\"config_excluded\""));
}

#[test]
fn test_top_level_rewrite_prints_empty_stdout_for_legacy_plugins() {
    let root = TempDir::new().unwrap();
    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "definitely-unsupported-packet28-tool",
            "--flag",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "git",
            "status",
            "--short",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn test_top_level_rewrite_handles_compound_commands_like_rtk() {
    let root = TempDir::new().unwrap();
    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "cargo",
            "test",
            "&&",
            "htop",
            "||",
            "git",
            "status",
            "--short",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"compound_rewrite\""))
        .stdout(predicate::str::contains("&& htop ||"))
        .stdout(predicate::str::contains("\"applied\":false"))
        .stdout(predicate::str::contains("\"rewritten_command\":null"));

    suite_cmd()
        .current_dir(root.path())
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "cargo",
            "test",
            "|",
            "grep",
            "FAIL",
            "&&",
            "git",
            "status",
            "--short",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"compound_rewrite\""))
        .stdout(predicate::str::contains("| grep FAIL &&"));
}

#[test]
fn test_compact_rewrite_reports_unsupported_without_executable_wrapper() {
    let root = TempDir::new().unwrap();
    suite_cmd()
        .current_dir(root.path())
        .args([
            "compact",
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "cat",
            "protected.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"applied\":false"))
        .stdout(predicate::str::contains("\"rewritten_command\":null"))
        .stdout(predicate::str::contains(
            "automatic_host_rewrite_unsupported",
        ));
}
