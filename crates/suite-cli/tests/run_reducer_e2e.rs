#[path = "support/run_reducer.rs"]
mod run_reducer;

use predicates::prelude::*;
#[cfg(unix)]
use run_reducer::{prepended_path, write_executable_script};
use run_reducer::{suite_cmd, write_cargo_fixture};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_run_reducer_reduces_cargo_check() {
    let root = TempDir::new().unwrap();
    write_cargo_fixture(root.path());

    suite_cmd()
        .current_dir(root.path())
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "cargo",
            "check",
            "--quiet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"rust\""))
        .stdout(predicate::str::contains(
            "\"canonical_kind\":\"rust_check\"",
        ))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));
}

#[test]
#[cfg(unix)]
fn test_run_reducer_reduces_tree_command() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("src/bin")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    fs::write(root.path().join("src/bin/cli.rs"), "fn main() {}\n").unwrap();

    let bin_dir = TempDir::new().unwrap();
    write_executable_script(
        &bin_dir.path().join("tree"),
        "#!/bin/sh\nprintf 'src\\n├── lib.rs\\n└── bin\\n    └── cli.rs\\n\\n1 directory, 2 files\\n'\n",
    );
    let path_env = prepended_path(bin_dir.path());

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "tree",
            "-L",
            "2",
            "src",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"fs\""))
        .stdout(predicate::str::contains("\"canonical_kind\":\"fs_tree\""))
        .stdout(predicate::str::contains(
            "tree listed 1 dir(s), 2 file(s) under src",
        ))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));
}

#[test]
#[cfg(unix)]
fn test_run_reducer_reduces_npm_test_and_pytest() {
    let root = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_executable_script(
        &bin_dir.path().join("npm"),
        "#!/bin/sh\nprintf 'npm test fixture passed\\n'\n",
    );
    write_executable_script(
        &bin_dir.path().join("pytest"),
        "#!/bin/sh\nprintf '2 passed in 0.01s\\n'\n",
    );
    let path_env = prepended_path(bin_dir.path());

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "npm",
            "test",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"javascript\""))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "pytest",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"python\""))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));
}

#[test]
fn test_run_reducer_reduces_file_and_search_commands() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("sample.txt"), "alpha\nbeta\nalpha again\n").unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "cat",
            "sample.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"fs\""))
        .stdout(predicate::str::contains("\"canonical_kind\":\"fs_cat\""))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));

    suite_cmd()
        .current_dir(root.path())
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "grep",
            "alpha",
            "sample.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"fs\""))
        .stdout(predicate::str::contains("\"canonical_kind\":\"fs_grep\""))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));

    suite_cmd()
        .current_dir(root.path())
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "wc",
            "-l",
            "sample.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"family\":\"fs\""))
        .stdout(predicate::str::contains("\"canonical_kind\":\"fs_wc\""))
        .stdout(predicate::str::contains("\"summary\":\"wc 3\""))
        .stdout(predicate::str::contains("\"fallback_reason\":null"));
}

#[test]
#[cfg(unix)]
fn test_run_reducer_renders_successful_rust_summary() {
    let root = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_executable_script(
        &bin_dir.path().join("cargo"),
        "#!/bin/sh\nprintf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'\n",
    );
    suite_cmd()
        .current_dir(root.path())
        .env("PATH", prepended_path(bin_dir.path()))
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "cargo",
            "test",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("cargo test passed (2 tests)\n"));
}

#[test]
fn test_run_reducer_counts_the_complete_rendered_command_body() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("sample.txt"), "alpha\nbeta\n").unwrap();
    let output = suite_cmd()
        .current_dir(root.path())
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "wc",
            "-l",
            "sample.txt",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let summary = value["reduction"]["summary"].as_str().unwrap();
    let preview = value["reduction"]["compact_preview"].as_str().unwrap();
    assert!(!summary.is_empty() && !preview.is_empty());
    let body = format!("{summary}\n{preview}\n");
    let expected_tokens = (body.len() as u64).div_ceil(4);
    assert_eq!(value["reduced_est_tokens"], expected_tokens);
    assert_eq!(value["rendered_command_body"], body);
    assert_eq!(value["estimate_scope"], "rendered_command_body");
    let record: serde_json::Value = serde_json::from_str(
        fs::read_to_string(root.path().join(".packet28/run-savings.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(record["reduced_est_tokens"], expected_tokens);
    assert_eq!(record["estimate_scope"], "rendered_command_body");
}
