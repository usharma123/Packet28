use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{self, Value};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
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
fn test_run_filter_applies_builtin_rtk_compatible_toml_filter() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let bin_dir = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    write_executable_script(
        &bin_dir.join("brew"),
        "#!/bin/sh\nprintf 'run\\n' >> \"$BREW_COUNT\"\nprintf 'Warning: rtk 0.27.1 is already installed and up-to-date.\\nTo reinstall 0.27.1, run:\\n  brew reinstall rtk\\n'; exit 0\n",
    );
    fs::write(root.path().join("old.txt"), "old line\nsame\n").unwrap();
    fs::write(root.path().join("new.txt"), "new line\nsame\n").unwrap();
    let counter = root.path().join("brew-count.txt");
    let path_env = std::env::join_paths(std::iter::once(bin_dir.as_path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", &home)
        .args([
            "verify",
            "filters",
            "--root",
            root.path().to_str().unwrap(),
            "--filter",
            "brew-install",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"filter\":\"brew-install\""));

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", &home)
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "brew",
            "install",
            "rtk",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"route\":\"toml_filter_rewrite\"",
        ))
        .stdout(predicate::str::contains("\"applied\":false"))
        .stdout(predicate::str::contains("\"rewritten_command\":null"))
        .stdout(predicate::str::contains(
            "automatic_host_rewrite_unsupported",
        ));

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", &home)
        .args([
            "rewrite",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "diff",
            "old.txt",
            "new.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"route\":\"reducer_rewrite\""))
        .stdout(predicate::str::contains("\"reducer_family\":\"fs\""))
        .stdout(predicate::str::contains("\"reducer_kind\":\"fs_diff\""));

    let output = suite_cmd()
        .current_dir(root.path())
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env("BREW_COUNT", &counter)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "brew",
            "install",
            "rtk",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["fallback_reason"], Value::Null);
    assert_eq!(value["reduction"]["family"], "custom_filter");
    assert_eq!(value["reduction"]["canonical_kind"], "brew-install");
    assert_eq!(
        value["reduction"]["compact_preview"].as_str().unwrap(),
        "ok (already installed)"
    );
    assert!(value["raw_artifact"]["available"].as_bool().unwrap());
    assert_eq!(fs::read_to_string(&counter).unwrap(), "run\n");

    let diff_output = suite_cmd()
        .current_dir(root.path())
        .env("HOME", &home)
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--json",
            "diff",
            "-u",
            "old.txt",
            "new.txt",
        ])
        .output()
        .unwrap();
    assert_eq!(diff_output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&diff_output.stdout).unwrap();
    assert_eq!(value["reduction"]["family"], "fs");
    assert_eq!(value["reduction"]["canonical_kind"], "fs_diff");
    assert!(value["reduction"]["summary"]
        .as_str()
        .unwrap()
        .contains("diff compared old.txt and new.txt"));
}
