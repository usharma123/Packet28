#![cfg(unix)]

#[path = "support/setup_runtime_hooks.rs"]
mod setup_runtime_hooks;

use predicates::prelude::*;
use serde_json::Value;
use setup_runtime_hooks::{doctor_command, run_setup};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_setup_runtime_hooks_copilot_writes_instructions_and_pretool_hook() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    run_setup(root.path(), home.path(), "copilot");

    assert!(root
        .path()
        .join(".github")
        .join("copilot-instructions.md")
        .exists());
    let hook_path = root
        .path()
        .join(".github")
        .join("hooks")
        .join("packet28-rewrite.json");
    let settings: Value = serde_json::from_str(&fs::read_to_string(hook_path).unwrap()).unwrap();
    let hooks = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    let command = hooks[0]["command"].as_str().unwrap();
    assert!(command.contains(" hook copilot "));
    assert!(command.contains(root.path().to_str().unwrap()));

    doctor_command(root.path(), home.path(), "copilot")
        .assert()
        .success()
        .stdout(predicate::str::contains("copilot_hook_config"));
}

#[test]
fn test_setup_runtime_hooks_opencode_writes_command_preserving_plugin() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    run_setup(root.path(), home.path(), "opencode");

    assert!(root.path().join("AGENTS.md").exists());
    let plugin_path = home
        .path()
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("packet28.ts");
    let plugin = fs::read_to_string(plugin_path).unwrap();
    assert!(plugin.contains("Packet28 preserves native command arguments"));
    assert!(plugin.contains("tool.execute.before"));

    doctor_command(root.path(), home.path(), "opencode")
        .assert()
        .success()
        .stdout(predicate::str::contains("opencode_plugin"));
}

#[test]
fn test_setup_runtime_hooks_hermes_writes_instructions_plugin_and_config() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    run_setup(root.path(), home.path(), "hermes");

    assert!(root.path().join("AGENTS.md").exists());
    let plugin_dir = home
        .path()
        .join(".hermes")
        .join("plugins")
        .join("packet28-rewrite");
    let init = fs::read_to_string(plugin_dir.join("__init__.py")).unwrap();
    let manifest = fs::read_to_string(plugin_dir.join("plugin.yaml")).unwrap();
    let config = fs::read_to_string(home.path().join(".hermes").join("config.yaml")).unwrap();
    assert!(init.contains("Packet28 preserves native command arguments"));
    assert!(manifest.contains("packet28-rewrite"));
    assert!(config.contains("packet28-rewrite"));

    doctor_command(root.path(), home.path(), "hermes")
        .assert()
        .success()
        .stdout(predicate::str::contains("hermes_plugin"));
}

#[test]
fn test_setup_runtime_hooks_gemini_writes_before_tool_hook_and_prompt() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    run_setup(root.path(), home.path(), "gemini");

    assert!(root.path().join("GEMINI.md").exists());
    let settings_path = home.path().join(".gemini").join("settings.json");
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
    let hooks = settings["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["matcher"].as_str(), Some("run_shell_command"));
    let command = hooks[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains(" hook gemini "));
    assert!(command.contains(root.path().to_str().unwrap()));

    doctor_command(root.path(), home.path(), "gemini")
        .assert()
        .success()
        .stdout(predicate::str::contains("gemini_hook_config"))
        .stdout(predicate::str::contains("runtime_capture_support"));
}
