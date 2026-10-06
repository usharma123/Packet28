#![cfg(unix)]

#[path = "support/daemon_lifecycle.rs"]
mod daemon_lifecycle;

use daemon_lifecycle::process_harness::{HarnessLimits, ProcessHarness};
use daemon_lifecycle::{ensure_packet28d_built, init_repo, suite_cmd, write_repo_fixture};
use packet28_daemon_protocol::commands::PacketFetchRequest;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

// The pre-existing size override is part of the compatibility contract.
const RUNTIME_LOG_MAX_BYTES_ENV: &str = "PACKET28_DAEMON_LOG_MAX_BYTES";
const DAEMON_LOG_LIMIT: u64 = 4096;
const HOOK_LOG_LIMIT: u64 = 1024;

/// Stops the fixture daemon even when an assertion fails first.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = suite_cmd()
            .args(["daemon", "stop", "--root", self.0.to_str().unwrap_or(".")])
            .output();
    }
}

/// Stops the fixture daemon and hook server even when an assertion fails.
struct Uninstall {
    root: PathBuf,
    home: PathBuf,
}

impl Drop for Uninstall {
    fn drop(&mut self) {
        let _ = suite_cmd()
            .env("HOME", &self.home)
            .args(["uninstall", "--root", self.root.to_str().unwrap_or(".")])
            .output();
    }
}

fn read_generations(log: &Path) -> String {
    (0..=3)
        .map(|index| {
            let path = if index == 0 {
                log.to_path_buf()
            } else {
                generation(log, index)
            };
            fs::read_to_string(path).unwrap_or_default()
        })
        .collect()
}

fn packet28d_binary() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_Packet28"))
        .parent()
        .unwrap()
        .join("packet28d")
}

fn generation(log: &Path, index: usize) -> PathBuf {
    let mut name = log.file_name().unwrap().to_os_string();
    name.push(format!(".{index}"));
    log.with_file_name(name)
}

/// Asserts the active log and exactly three backups exist within `limit`.
fn assert_bounded_generations(log: &Path, limit: u64) {
    for index in 0..=3 {
        let path = if index == 0 {
            log.to_path_buf()
        } else {
            generation(log, index)
        };
        let size = fs::metadata(&path)
            .unwrap_or_else(|error| panic!("missing generation {}: {error}", path.display()))
            .len();
        assert!(
            size <= limit,
            "{} has {size} bytes; limit {limit}",
            path.display()
        );
    }
    assert!(
        !generation(log, 4).exists(),
        "only three backups may be retained"
    );
}

fn daemon_pid(root: &Path) -> u64 {
    let output = suite_cmd()
        .args([
            "daemon",
            "status",
            "--root",
            root.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&output).unwrap();
    status["pid"].as_u64().unwrap()
}

fn fail_packet_fetch(root: &Path, handle: String) {
    let response = suite_cli::cmd_daemon_client::send_packet_fetch(
        root,
        PacketFetchRequest {
            handle,
            root: root.to_string_lossy().into_owned(),
        },
    );
    assert!(response.is_err(), "missing packet unexpectedly resolved");
}

#[test]
fn background_daemon_rotates_its_own_log_after_the_launcher_exits() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());
    let root = fs::canonicalize(dir.path()).unwrap();
    let log = root.join(".packet28/daemon/packet28d.log");

    // The launcher exits once the daemon is ready; the daemon inherits the
    // size override from the launcher environment.
    suite_cmd()
        .env(RUNTIME_LOG_MAX_BYTES_ENV, DAEMON_LOG_LIMIT.to_string())
        .args(["daemon", "start", "--root", root.to_str().unwrap()])
        .assert()
        .success();
    let _stop = StopDaemon(root.clone());
    let pid = daemon_pid(&root);

    // Every failed request is a daemon diagnostic. One request carries a
    // 512 KiB handle, producing a single huge record without newlines.
    for index in 0..48 {
        fail_packet_fetch(&root, format!("missing-early-{index:03}"));
    }
    fail_packet_fetch(&root, format!("missing-huge-{}", "h".repeat(512 * 1024)));
    for index in 0..48 {
        fail_packet_fetch(&root, format!("missing-late-{index:03}"));
    }

    assert_eq!(
        daemon_pid(&root),
        pid,
        "rotation must happen inside the same running daemon"
    );
    assert_bounded_generations(&log, DAEMON_LOG_LIMIT);
    let active = fs::read_to_string(&log).unwrap();
    assert!(active.contains("missing-late-047"), "{active}");
    let retained = read_generations(&log);
    assert!(
        retained.contains("missing-huge-hhhh") && retained.contains("[truncated]"),
        "the huge diagnostic must be retained only as a truncated record"
    );

    // A terminal startup error from a background daemon reaches its log.
    let mut contender = Command::new(packet28d_binary());
    contender
        .args(["serve", "--managed-log", "--root"])
        .arg(&root);
    let contender = ProcessHarness::run(
        &mut contender,
        &[],
        Duration::from_secs(30),
        HarnessLimits::default(),
    )
    .unwrap();
    assert!(!contender.status.success());
    let active = fs::read_to_string(&log).unwrap();
    assert!(active.contains("packet28d exited with error"), "{active}");

    // A foreground daemon keeps ordinary stderr diagnostics.
    let mut foreground = Command::new(packet28d_binary());
    foreground.args(["serve", "--root"]).arg(&root);
    let foreground = ProcessHarness::run(
        &mut foreground,
        &[],
        Duration::from_secs(30),
        HarnessLimits::default(),
    )
    .unwrap();
    assert!(!foreground.status.success());
    let stderr = String::from_utf8_lossy(&foreground.stderr);
    assert!(stderr.contains("error:"), "{stderr}");
}

#[test]
fn launcher_keeps_start_time_rotation_for_a_daemon_without_managed_logs() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());
    let root = fs::canonicalize(dir.path()).unwrap();
    let log = root.join(".packet28/daemon/packet28d.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, vec![b'L'; 2 * DAEMON_LOG_LIMIT as usize]).unwrap();

    // Emulates an older packet28d whose `serve --help` lacks --managed-log.
    let legacy = bin.path().join("packet28d");
    fs::write(
        &legacy,
        format!(
            "#!/bin/sh\nif [ \"$2\" = \"--help\" ]; then\n  echo 'Usage: packet28d serve [OPTIONS]'\n  echo '      --root <ROOT>'\n  exit 0\nfi\nexec '{}' \"$@\"\n",
            packet28d_binary().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&legacy, fs::Permissions::from_mode(0o755)).unwrap();

    suite_cmd()
        .env(RUNTIME_LOG_MAX_BYTES_ENV, DAEMON_LOG_LIMIT.to_string())
        .env("CARGO_BIN_EXE_packet28d", &legacy)
        .args(["daemon", "start", "--root", root.to_str().unwrap()])
        .assert()
        .success();
    let _stop = StopDaemon(root.clone());

    assert_eq!(
        fs::metadata(generation(&log, 1)).unwrap().len(),
        2 * DAEMON_LOG_LIMIT
    );
    let active = fs::read_to_string(&log).unwrap();
    assert!(active.contains("starting packet28d"), "{active}");
}

fn http_exchange(port: u16, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request).unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

#[test]
fn background_hook_server_rotates_its_own_log_after_setup_exits() {
    ensure_packet28d_built();
    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    write_repo_fixture(dir.path());
    init_repo(dir.path());
    let root = fs::canonicalize(dir.path()).unwrap();
    let root_arg = root.to_str().unwrap();
    let log = root.join(".packet28/daemon/packet28-hook-http.log");

    suite_cmd()
        .env("HOME", home.path())
        .env(RUNTIME_LOG_MAX_BYTES_ENV, HOOK_LOG_LIMIT.to_string())
        .args(["setup", "--root", root_arg, "--runtime", "claude", "--yes"])
        .assert()
        .success();
    let _uninstall = Uninstall {
        root: root.clone(),
        home: home.path().to_path_buf(),
    };
    let config: Value = serde_json::from_slice(
        &fs::read(root.join(".packet28/daemon/hook-runtime-v1.json")).unwrap(),
    )
    .unwrap();
    let port = config["http_hook_port"].as_u64().unwrap() as u16;
    let token = config["http_hook_token"].as_str().unwrap().to_string();

    // Each malformed request is a server diagnostic.
    for _ in 0..64 {
        http_exchange(port, b"\r\n");
    }
    let health = format!("GET /packet28/health HTTP/1.1\r\nx-packet28-hook-token: {token}\r\n\r\n");
    let response = http_exchange(port, health.as_bytes());
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    assert_bounded_generations(&log, HOOK_LOG_LIMIT);
    let active = fs::read_to_string(&log).unwrap();
    assert!(active.contains("malformed HTTP request line"), "{active}");
}
