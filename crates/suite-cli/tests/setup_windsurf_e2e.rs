#![cfg(unix)]

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[path = "support/setup_runtime_hooks.rs"]
#[expect(
    dead_code,
    reason = "this binary only exercises the shared setup helper"
)]
mod setup_support;

fn run_setup(root: &Path, home: &Path) {
    setup_support::run_setup(root, home, "windsurf");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease(root) {
            Ok(_lease) => return,
            Err(packet28_daemon_core::DaemonCoreError::DaemonInstanceAlreadyRunning { .. }) => {
                assert!(
                    Instant::now() < deadline,
                    "fixture daemon kept its instance lease after stop"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("failed to verify fixture daemon instance lease release: {error}"),
        }
    }
}

fn setup_windsurf_e2e_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[test]
fn test_setup_windsurf_writes_rules_hooks_and_mcp() {
    let _guard = setup_windsurf_e2e_lock();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    fs::create_dir_all(home.path().join(".codeium").join("windsurf")).unwrap();

    run_setup(root.path(), home.path());

    assert!(root.path().join(".windsurf").join("hooks.json").exists());
    assert!(root
        .path()
        .join(".windsurf")
        .join("rules")
        .join("packet28.md")
        .exists());
    assert!(home
        .path()
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json")
        .exists());
    let rules = fs::read_to_string(
        root.path()
            .join(".windsurf")
            .join("rules")
            .join("packet28.md"),
    )
    .unwrap();
    assert!(rules.contains("Windsurf hooks preserve native commands and permissions"));
    assert!(rules.contains("use explicit Packet28 CLI/MCP tools for reduced output"));
}

#[test]
fn test_setup_windsurf_preserves_existing_mcp_servers_and_hooks() {
    let _guard = setup_windsurf_e2e_lock();
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let windsurf_home = home.path().join(".codeium").join("windsurf");
    fs::create_dir_all(&windsurf_home).unwrap();
    fs::create_dir_all(root.path().join(".windsurf")).unwrap();

    let mcp_config_path = windsurf_home.join("mcp_config.json");
    fs::write(
        &mcp_config_path,
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "existing": {
                    "command": "existing-mcp",
                    "args": ["--flag"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let hooks_path = root.path().join(".windsurf").join("hooks.json");
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "pre_run_command": [
                    {"command": "existing-pre-run"}
                ],
                "custom_event": [
                    {"command": "existing-custom"}
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    run_setup(root.path(), home.path());

    let mcp_config: Value =
        serde_json::from_str(&fs::read_to_string(mcp_config_path).unwrap()).unwrap();
    assert_eq!(
        mcp_config["mcpServers"]["existing"]["command"],
        "existing-mcp"
    );
    assert_eq!(mcp_config["mcpServers"]["existing"]["args"][0], "--flag");
    assert_eq!(mcp_config["mcpServers"]["packet28"]["args"][0], "--root");
    assert_eq!(
        mcp_config["mcpServers"]["packet28"]["args"][1],
        root.path().display().to_string()
    );

    let hooks: Value = serde_json::from_str(&fs::read_to_string(hooks_path).unwrap()).unwrap();
    let pre_run = hooks["hooks"]["pre_run_command"].as_array().unwrap();
    assert!(pre_run
        .iter()
        .any(|entry| entry["command"] == "existing-pre-run"));
    assert!(pre_run.iter().any(|entry| entry["command"]
        .as_str()
        .is_some_and(|command| command.contains("hook windsurf"))));
    assert_eq!(
        hooks["hooks"]["custom_event"][0]["command"],
        "existing-custom"
    );
}
