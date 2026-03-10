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
fn write_fake_binary(path: &Path) {
    fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
#[cfg(unix)]
fn test_setup_only_writes_artifacts_for_detected_runtimes() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    write_fake_binary(&bin_dir.path().join("codex"));

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", bin_dir.path().display()),
        )
        .args(["setup", "--root", root.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Agent instruction files have been written.",
        ))
        .stdout(
            predicate::str::contains("Your agent runtimes are configured to use Packet28 via MCP.")
                .not(),
        );

    assert!(root.path().join("AGENTS.md").exists());
    assert!(!root.path().join("CLAUDE.md").exists());
    assert!(!root.path().join(".cursorrules").exists());
}
