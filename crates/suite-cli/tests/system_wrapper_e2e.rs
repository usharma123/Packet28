use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

#[cfg(unix)]
fn write_executable_script(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
#[cfg(unix)]
fn test_system_wrapper_prettier_and_format_commands_wrap_formatter_output() {
    let root = TempDir::new().unwrap();
    let bin_dir = root.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_executable_script(
        &bin_dir.join("prettier"),
        "#!/bin/sh\nprintf 'Checking formatting...\\nsrc/app.ts\\nCode style issues found in the above file. Forgot to run Prettier?\\n'; exit 1\n",
    );
    write_executable_script(
        &bin_dir.join("ruff"),
        "#!/bin/sh\nprintf 'ruff args:%s\\nWould reformat: src/demo.py\\n1 file would be reformatted\\n' \"$*\"; exit 1\n",
    );
    let path_env = std::env::join_paths(std::iter::once(bin_dir.as_path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["prettier", "--check", "src"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: prettier"))
        .stdout(predicate::str::contains("src/app.ts"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["format", "prettier", "--check", "src"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: prettier"))
        .stdout(predicate::str::contains("src/app.ts"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["format", "ruff", "--check", "src"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: ruff format"))
        .stdout(predicate::str::contains("ruff args:format --check src"))
        .stdout(predicate::str::contains("src/demo.py"));
}

#[test]
#[cfg(unix)]
fn test_system_wrapper_language_tool_commands_wrap_common_rtk_tools() {
    let root = TempDir::new().unwrap();
    let bin_dir = root.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_executable_script(
        &bin_dir.join("npm"),
        "#!/bin/sh\nprintf 'npm test fixture passed\\n'; exit 0\n",
    );
    write_executable_script(
        &bin_dir.join("npx"),
        "#!/bin/sh\nprintf 'src/app.ts(1,7): error TS2322: Type string is not assignable to number.\\n'; exit 2\n",
    );
    write_executable_script(
        &bin_dir.join("tsc"),
        "#!/bin/sh\nprintf 'src/index.ts(2,1): error TS2304: Cannot find name missing.\\n'; exit 2\n",
    );
    write_executable_script(
        &bin_dir.join("vitest"),
        "#!/bin/sh\nprintf 'Tests 1 failed | 7 passed\\nFAIL src/app.test.ts > app\\n'; exit 1\n",
    );
    write_executable_script(
        &bin_dir.join("pytest"),
        "#!/bin/sh\nprintf 'FAILED tests/test_app.py::test_demo - AssertionError\\n1 failed, 4 passed in 0.01s\\n'; exit 1\n",
    );
    write_executable_script(
        &bin_dir.join("ruff"),
        "#!/bin/sh\nprintf 'src/app.py:1:1 F401 unused import\\nFound 1 error.\\n'; exit 1\n",
    );
    let path_env = std::env::join_paths(std::iter::once(bin_dir.as_path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["npm", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok] Command: npm test"))
        .stdout(predicate::str::contains("Test Results:"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["npx", "tsc", "--noEmit"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("[FAIL] Command: npx tsc --noEmit"))
        .stdout(predicate::str::contains("TS2322"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["tsc", "--noEmit"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("[FAIL] Command: tsc --noEmit"))
        .stdout(predicate::str::contains("TS2304"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["vitest", "run"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: vitest run"))
        .stdout(predicate::str::contains("vitest: 1 failed, 7 passed"))
        .stdout(predicate::str::contains("FAIL src/app.test.ts > app"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["pytest", "-q"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: pytest -q"))
        .stdout(predicate::str::contains("tests/test_app.py::test_demo"));

    suite_cmd()
        .current_dir(root.path())
        .env("PATH", &path_env)
        .args(["ruff", "check", "src"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("[FAIL] Command: ruff check src"))
        .stdout(predicate::str::contains("F401"));
}
