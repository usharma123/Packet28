use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn suite_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("Packet28")
}

#[test]
fn test_system_query_smart_command_summarizes_source_file() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(
        &source,
        r#"
use anyhow::Result;

#[derive(Debug)]
pub struct Config {
    name: String,
}

pub fn load_config() -> Result<Config> {
    Ok(Config { name: "demo".to_string() })
}
"#,
    )
    .unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args(["smart", source.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Rust module"))
        .stdout(predicate::str::contains("1 fn"))
        .stdout(predicate::str::contains("1 type"))
        .stdout(predicate::str::contains("derive"));

    suite_cmd()
        .current_dir(root.path())
        .args(["smart", source.to_str().unwrap(), "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"command\":\"Packet28 smart\""))
        .stdout(predicate::str::contains("Rust module"));
}

#[test]
fn test_system_query_find_command_supports_native_find_shape() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src").join("a.rs"), "").unwrap();
    fs::write(root.path().join("src").join("b.rs"), "").unwrap();
    fs::write(root.path().join("src").join("note.txt"), "").unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args(["find", ".", "-name", "*.rs", "-type", "f", "-maxdepth", "2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 match(es) under . for *.rs"))
        .stdout(predicate::str::contains("src/a.rs"))
        .stdout(predicate::str::contains("src/b.rs"))
        .stdout(predicate::str::contains("note.txt").not());
}

#[test]
fn test_system_query_grep_command_groups_matches() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src").join("a.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    fs::write(root.path().join("src").join("note.txt"), "fn ignored\n").unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args(["grep", "fn", "src", "--file-type", "rs", "--max", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 matches in 1 files"))
        .stdout(predicate::str::contains("src/a.rs:1:fn alpha() {}"))
        .stdout(predicate::str::contains("[+1 more]"))
        .stdout(predicate::str::contains("ignored").not());
}

#[cfg(unix)]
#[test]
fn test_system_query_grep_fff_engine_delegates_to_p28() {
    let root = TempDir::new().unwrap();
    let fake_p28 = root.path().join("p28");
    fs::write(
        &fake_p28,
        r#"#!/usr/bin/env sh
printf '%s\n' '{"result":{"query":"Alpha","match_count":2,"returned_match_count":2,"paths":["src/a.rs"],"groups":[{"path":"src/a.rs","match_count":2,"displayed_match_count":2,"matches":[{"path":"src/a.rs","line":1,"text":"Alpha one"},{"path":"src/a.rs","line":2,"text":"Alpha two"}]}],"engine":{"engine":"fff_mcp"}}}'
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_p28).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_p28, perms).unwrap();

    suite_cmd()
        .current_dir(root.path())
        .env("P28_SEARCH_BIN", &fake_p28)
        .args(["grep", "Alpha", "--engine", "fff", "--max", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 matches in 1 files"))
        .stdout(predicate::str::contains("src/a.rs:1:Alpha one"))
        .stdout(predicate::str::contains("[+1 more]"));
}

#[test]
fn test_system_query_log_command_deduplicates_noisy_lines() {
    let root = TempDir::new().unwrap();
    let log = root.path().join("app.log");
    fs::write(
        &log,
        "2026-05-12T01:00:00 ERROR failed request id=1001 path=/tmp/a\n\
         2026-05-12T01:00:01 ERROR failed request id=1002 path=/tmp/b\n\
         2026-05-12T01:00:02 WARN retrying request id=2001\n\
         2026-05-12T01:00:03 INFO healthy\n",
    )
    .unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args(["log", log.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Log Summary"))
        .stdout(predicate::str::contains("[error] 2 errors (1 unique)"))
        .stdout(predicate::str::contains("[warn] 1 warnings (1 unique)"))
        .stdout(predicate::str::contains("[info] 1 info messages"))
        .stdout(predicate::str::contains("[x2]"));
}

#[test]
fn test_system_query_pipe_filters_stdin_like_rtk_pipe() {
    let root = TempDir::new().unwrap();

    suite_cmd()
        .current_dir(root.path())
        .args(["pipe", "--filter", "grep"])
        .write_stdin("src/main.rs:10:fn main() {}\nsrc/lib.rs:2:pub fn helper() {}\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("2 matches in 2 files"))
        .stdout(predicate::str::contains("src/main.rs:10:fn main() {}"));

    suite_cmd()
        .current_dir(root.path())
        .args(["pipe"])
        .write_stdin("tests/a.rs\ntests/b.rs\nsrc/main.rs\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("3 paths:"))
        .stdout(predicate::str::contains("src/main.rs"));

    suite_cmd()
        .current_dir(root.path())
        .args(["pipe", "--passthrough"])
        .write_stdin("raw\nunchanged\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("raw\nunchanged\n"));
}

#[test]
#[cfg(unix)]
fn test_system_query_explicit_compact_read_preserves_head_window_and_contents() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("README.md");
    for contents in [
        "alpha\n\nbeta\ngamma\ndelta\nexcluded sixth line\n",
        "updated alpha\n\nupdated beta\nupdated gamma\nupdated delta\nexcluded update\n",
    ] {
        fs::write(&path, contents).unwrap();
        let raw = Command::new("head")
            .args(["-n", "5", path.to_str().unwrap()])
            .timeout(std::time::Duration::from_secs(5))
            .output()
            .unwrap();
        let reduced = suite_cmd()
            .current_dir(root.path())
            .args([
                "--via-daemon",
                "--daemon-root",
                root.path().to_str().unwrap(),
                "compact",
                "read",
                "--root",
                root.path().to_str().unwrap(),
                "--cwd",
                root.path().to_str().unwrap(),
                "--line-start",
                "1",
                "--line-end",
                "5",
                "--json",
                "README.md",
            ])
            .timeout(std::time::Duration::from_secs(5))
            .output()
            .unwrap();
        assert_eq!(reduced.status.code(), raw.status.code());
        assert!(raw.status.success());
        let value: serde_json::Value = serde_json::from_slice(&reduced.stdout).unwrap();
        let expected = String::from_utf8(raw.stdout)
            .unwrap()
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{}|{}", index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(value["line_start"], 1);
        assert_eq!(value["line_end"], 5);
        assert_eq!(value["line_count"], 5);
        assert_eq!(value["compact_preview"], expected);
        let visible = suite_cmd()
            .current_dir(root.path())
            .args([
                "--via-daemon",
                "--daemon-root",
                root.path().to_str().unwrap(),
                "compact",
                "read",
                "--root",
                root.path().to_str().unwrap(),
                "--cwd",
                root.path().to_str().unwrap(),
                "--line-start",
                "1",
                "--line-end",
                "5",
                "README.md",
            ])
            .timeout(std::time::Duration::from_secs(5))
            .output()
            .unwrap();
        assert!(visible.status.success());
        assert_eq!(
            String::from_utf8(visible.stdout).unwrap(),
            format!("{expected}\n")
        );
    }
}
