#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The retired hook-suite sample the agent-DX benchmark replays for `vitest run`.
const ONE_FAILURE_STDOUT: &str = include_str!("fixtures/vitest/one_failure.stdout");

struct VitestStub {
    root: TempDir,
    calls: PathBuf,
}

impl VitestStub {
    /// Installs a `vitest` that logs each call's argv, replays the given
    /// streams once and exits with `exit_code`.
    fn new(stdout: &str, stderr: &str, exit_code: i32) -> Self {
        let root = TempDir::new().unwrap();
        let bin_dir = root.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let stdout_path = root.path().join("stdout.txt");
        let stderr_path = root.path().join("stderr.txt");
        let calls = root.path().join("calls.log");
        fs::write(&stdout_path, stdout).unwrap();
        fs::write(&stderr_path, stderr).unwrap();
        let script = bin_dir.join("vitest");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '<%s>' \"$@\" >> '{calls}'\nprintf '\\n' >> '{calls}'\ncat '{stdout}'\ncat '{stderr}' >&2\nexit {exit_code}\n",
                calls = calls.display(),
                stdout = stdout_path.display(),
                stderr = stderr_path.display(),
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
        Self { root, calls }
    }

    fn run(&self, args: &[&str]) -> (i32, String) {
        let path_env = std::env::join_paths(std::iter::once(self.root.path().join("bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let output = assert_cmd::cargo::cargo_bin_cmd!("Packet28")
            .current_dir(self.root.path())
            .env("PATH", path_env)
            .arg("vitest")
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    fn calls(&self) -> Vec<String> {
        read_lines(&self.calls)
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn vitest_one_failure_keeps_test_identity_assertion_and_test_counts() {
    let stub = VitestStub::new(ONE_FAILURE_STDOUT, "", 1);
    let (code, stdout) = stub.run(&["run"]);

    assert_eq!(code, 1, "{stdout}");
    assert_eq!(stub.calls(), vec!["<run>"]);
    assert!(stdout.contains("[FAIL] Command: vitest run"), "{stdout}");
    assert!(
        stdout.contains("vitest: 1 failed, 7 passed; src/api.test.ts"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FAIL  src/api.test.ts > returns error payload"),
        "{stdout}"
    );
    assert!(
        stdout.contains("AssertionError: expected 500 to be 200"),
        "{stdout}"
    );
    assert!(!stdout.contains("2 passed"), "file count leaked: {stdout}");
}

#[test]
fn vitest_forwards_arguments_unchanged_and_runs_once() {
    let stub = VitestStub::new(ONE_FAILURE_STDOUT, "", 1);
    let (code, stdout) = stub.run(&["run", "src/api.test.ts", "-t", "error payload"]);

    assert_eq!(code, 1, "{stdout}");
    assert_eq!(
        stub.calls(),
        vec!["<run><src/api.test.ts><-t><error payload>"]
    );
    assert!(
        stdout.contains("[FAIL] Command: vitest run src/api.test.ts -t error payload"),
        "{stdout}"
    );
}

#[test]
fn vitest_multiple_failures_on_stderr_keep_each_failure() {
    let stdout = " Test Files  1 passed | 2 failed (3)\n      Tests  6 passed | 3 failed (9)\n";
    let stderr = "\u{23af}\u{23af} Failed Tests 3 \u{23af}\u{23af}\n\n FAIL  src/api.test.ts > returns error payload\nAssertionError: expected 500 to be 200\n\n FAIL  src/api.test.ts > rejects bad token\nError: token missing\n\n FAIL  src/db.test.ts > migrates schema\nAssertionError: expected [ 'a' ] to deeply equal [ 'a', 'b' ]\n";
    let stub = VitestStub::new(stdout, stderr, 1);
    let (code, rendered) = stub.run(&["run"]);

    assert_eq!(code, 1, "{rendered}");
    assert_eq!(stub.calls().len(), 1);
    for fact in [
        "vitest: 3 failed, 6 passed",
        "FAIL  src/api.test.ts > returns error payload",
        "AssertionError: expected 500 to be 200",
        "FAIL  src/api.test.ts > rejects bad token",
        "Error: token missing",
        "FAIL  src/db.test.ts > migrates schema",
        "AssertionError: expected [ 'a' ] to deeply equal [ 'a', 'b' ]",
    ] {
        assert!(rendered.contains(fact), "missing {fact:?}: {rendered}");
    }
}

#[test]
fn vitest_success_reports_test_count_and_zero_exit() {
    let stub = VitestStub::new(
        " \u{2713} src/math.test.ts (4)\n \u{2713} src/string.test.ts (3)\n\n Test Files  2 passed (2)\n      Tests  7 passed (7)\n",
        "",
        0,
    );
    let (code, stdout) = stub.run(&["run"]);

    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stub.calls().len(), 1);
    assert!(stdout.contains("[ok] Command: vitest run"), "{stdout}");
    assert!(
        stdout.contains("javascript tests passed (7 tests)"),
        "{stdout}"
    );
    assert!(!stdout.contains("FAIL"), "{stdout}");
}

#[test]
fn vitest_unrecognized_failure_keeps_raw_lines_without_inventing_counts() {
    let stub = VitestStub::new(
        "",
        "failed to load config from /workspace/vitest.config.ts\nSyntaxError: Unexpected token '}'\n",
        1,
    );
    let (code, stdout) = stub.run(&["run"]);

    assert_eq!(code, 1, "{stdout}");
    assert_eq!(stub.calls().len(), 1);
    assert!(stdout.contains("[FAIL] Command: vitest run"), "{stdout}");
    assert!(stdout.contains("output not recognized"), "{stdout}");
    assert!(
        stdout.contains("failed to load config from /workspace/vitest.config.ts"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SyntaxError: Unexpected token '}'"),
        "{stdout}"
    );
    assert!(!stdout.contains("passed"), "{stdout}");
    assert!(!stdout.contains("[ok]"), "{stdout}");
}

#[test]
fn vitest_unrecognized_failure_bounds_long_output_and_keeps_its_end() {
    let mut stderr = (1..=60)
        .map(|index| format!("line {index}\n"))
        .collect::<String>();
    stderr.push_str("Error: worker exited unexpectedly\n");
    let stub = VitestStub::new("", &stderr, 1);
    let (code, stdout) = stub.run(&["run"]);

    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("line 10\n"), "{stdout}");
    assert!(!stdout.contains("line 11\n"), "{stdout}");
    assert!(stdout.contains("... 21 line(s) omitted"), "{stdout}");
    assert!(stdout.contains("line 32\n"), "{stdout}");
    assert!(
        stdout.contains("Error: worker exited unexpectedly"),
        "{stdout}"
    );
}

#[test]
fn vitest_colored_failure_is_not_reduced_to_a_guess() {
    let stub = VitestStub::new(
        " \u{1b}[41m\u{1b}[1m FAIL \u{1b}[22m\u{1b}[49m src/api.test.ts > returns error payload\n\u{1b}[31mAssertionError: expected 500 to be 200\u{1b}[39m\n      Tests  \u{1b}[1m\u{1b}[31m1 failed\u{1b}[39m\u{1b}[22m | \u{1b}[1m\u{1b}[32m7 passed\u{1b}[39m\u{1b}[22m (8)\n",
        "",
        1,
    );
    let (code, stdout) = stub.run(&["run"]);

    assert_eq!(code, 1, "{stdout}");
    assert_eq!(stub.calls().len(), 1);
    assert!(stdout.contains("[FAIL] Command: vitest run"), "{stdout}");
    assert!(
        stdout.contains("src/api.test.ts > returns error payload"),
        "{stdout}"
    );
    assert!(
        stdout.contains("AssertionError: expected 500 to be 200"),
        "{stdout}"
    );
    assert!(!stdout.contains("0 passed"), "{stdout}");
}
