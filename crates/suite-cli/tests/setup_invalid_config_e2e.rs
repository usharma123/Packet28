use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

#[test]
#[cfg(unix)]
fn test_setup_refuses_to_overwrite_invalid_mcp_json() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let claude_config = root.path().join(".mcp.json");
    fs::write(&claude_config, "{ invalid json").unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "setup",
            "--root",
            root.path().to_str().unwrap(),
            "--runtime",
            "claude",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to overwrite invalid JSON",
        ));

    assert_eq!(
        fs::read_to_string(&claude_config).unwrap(),
        "{ invalid json"
    );
}

#[test]
#[cfg(unix)]
fn test_setup_refuses_to_overwrite_invalid_codex_toml() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let codex_config = home.path().join(".codex").join("config.toml");
    fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
    fs::write(&codex_config, "[features").unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .env("PATH", "/usr/bin:/bin")
        .args([
            "setup",
            "--root",
            root.path().to_str().unwrap(),
            "--runtime",
            "codex",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to overwrite invalid TOML",
        ));

    assert_eq!(fs::read_to_string(&codex_config).unwrap(), "[features");
}
