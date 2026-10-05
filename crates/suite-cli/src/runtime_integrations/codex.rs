use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use packet28_state_fs::StateDir;
use serde_json::{json, Value};
use toml::value::Table as TomlTable;

use super::{
    AgentPromptFormat, IntegrationAction, McpConfigStatus, PromptTarget, RuntimeAdapter,
    RuntimeEnvironment,
};
use crate::cmd_setup::setup_commands::resolve_packet28_mcp_command;
use crate::cmd_setup::{
    read_toml_config, read_toml_config_or_default, toml_table_entry, write_toml_config,
};

pub(crate) const ADAPTER: RuntimeAdapter = RuntimeAdapter {
    name: "Codex",
    slug: "codex",
    prompt_targets,
    detect,
    mcp: Some(IntegrationAction::new(
        configure_mcp,
        mcp_artifacts,
        mcp_status,
    )),
    hooks: Some(IntegrationAction::new(
        configure_hooks,
        hook_artifacts,
        hook_status,
    )),
    writes_hook_runtime_config: true,
};

pub(crate) fn config_path(home: &Path) -> PathBuf {
    // Only apply process configuration to the process's actual home. Callers
    // supplying an explicit synthetic home keep their configuration isolated.
    let override_home = (std::env::var_os("HOME").as_deref() == Some(home.as_os_str()))
        .then(|| std::env::var_os("CODEX_HOME"))
        .flatten()
        .map(PathBuf::from);
    config_path_with_override(home, override_home.as_deref())
}

fn config_path_with_override(home: &Path, codex_home: Option<&Path>) -> PathBuf {
    codex_home
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| home.join(".codex"), Path::to_path_buf)
        .join("config.toml")
}

pub(crate) fn prompt_path(root: &Path) -> PathBuf {
    root.join("AGENTS.md")
}

pub(crate) const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "Stop",
    "SessionEnd",
];
const MAX_HOOK_CONFIG_BYTES: u64 = 1024 * 1024;

pub(crate) fn hooks_path(root: &Path) -> PathBuf {
    root.join(".codex").join("hooks.json")
}

fn hook_artifacts(environment: &RuntimeEnvironment<'_>) -> Vec<PathBuf> {
    vec![hooks_path(environment.root())]
}

fn hook_status(environment: &RuntimeEnvironment<'_>) -> String {
    format!(
        "{}; enable hooks and review/trust them in Codex /hooks",
        hooks_path(environment.root()).display()
    )
}

fn configure_hooks(
    environment: &RuntimeEnvironment<'_>,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let directory = StateDir::open(environment.root(), &[".codex"], true)?;
    #[cfg(unix)]
    let _lease = directory.lock_exclusive()?;
    let mut config: Value = match directory.read_bounded("hooks.json", MAX_HOOK_CONFIG_BYTES)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .context("refusing to overwrite invalid Codex hooks JSON")?,
        None => json!({}),
    };
    let original = config.clone();
    let hooks = config
        .as_object_mut()
        .context("Codex hooks config must be an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Codex hooks must be an object")?;
    let command = crate::cmd_setup::setup_commands::generated_packet28_hook_command(
        "codex",
        environment.root(),
    );
    for event in HOOK_EVENTS {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("Codex {event} hooks must be an array"))?;
        for entry in entries.iter_mut() {
            if let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| !is_packet28_hook(handler));
            }
        }
        entries.retain(|entry| {
            !entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        });
        entries.push(json!({"hooks": [{"type": "command", "command": command, "timeout": 10}]}));
    }
    if config == original {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    if !auto_yes {
        eprint!(
            "    Write Codex hooks to {}? [Y/n] ",
            hooks_path(environment.root()).display()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes") {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let bytes = format!("{}\n", serde_json::to_string_pretty(&config)?);
    if bytes.len() as u64 > MAX_HOOK_CONFIG_BYTES {
        anyhow::bail!("merged Codex hooks configuration exceeds {MAX_HOOK_CONFIG_BYTES} bytes");
    }
    directory.write_atomic("hooks.json", bytes.as_bytes())?;
    Ok(McpConfigStatus::Written)
}

fn is_packet28_hook(handler: &Value) -> bool {
    if handler.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let Some(command) = handler.get("command").and_then(Value::as_str) else {
        return false;
    };
    crate::cmd_setup::setup_commands::is_generated_packet28_hook_command(command, "codex")
}

fn prompt_targets(environment: &RuntimeEnvironment<'_>) -> Vec<PromptTarget> {
    vec![PromptTarget {
        path: prompt_path(environment.root()),
        format: AgentPromptFormat::Agents,
    }]
}

fn detect(environment: &RuntimeEnvironment<'_>) -> bool {
    environment.command_exists("codex")
}

fn configure_mcp(environment: &RuntimeEnvironment<'_>, auto_yes: bool) -> Result<McpConfigStatus> {
    let path = config_path(environment.home());
    if mcp_entry_matches(&path, environment.root())? {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    if !auto_yes {
        eprint!(
            "    Register Packet28 MCP in Codex via {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    if environment.command_exists("codex") {
        let args = vec![
            "mcp".to_string(),
            "add".to_string(),
            "packet28".to_string(),
            "--".to_string(),
            resolve_packet28_mcp_command(),
            "--root".to_string(),
            environment.root().display().to_string(),
            "--toolset".to_string(),
            "core".to_string(),
        ];
        if environment.run_command("codex", &args).unwrap_or(false)
            && mcp_entry_matches(&path, environment.root())?
        {
            return Ok(McpConfigStatus::Written);
        }
    }
    write_mcp_config(&path, environment.root())
}

fn mcp_artifacts(environment: &RuntimeEnvironment<'_>) -> Vec<PathBuf> {
    vec![config_path(environment.home())]
}

fn mcp_status(environment: &RuntimeEnvironment<'_>) -> String {
    format!(
        "{} → {}",
        ADAPTER.name,
        config_path(environment.home()).display()
    )
}

fn mcp_entry_matches(path: &Path, root: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let config = read_toml_config(path)?;
    let Some(server) = config
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .and_then(|servers| servers.get("packet28"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(false);
    };
    let command_matches = server
        .get("command")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        == Some(resolve_packet28_mcp_command().as_str());
    let expected_root = root.display().to_string();
    let args_matches = server
        .get("args")
        .and_then(toml::Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()
                == vec!["--root", expected_root.as_str(), "--toolset", "core"]
        })
        .unwrap_or(false);
    Ok(command_matches && args_matches)
}

fn write_mcp_config(path: &Path, root: &Path) -> Result<McpConfigStatus> {
    let mut config = read_toml_config_or_default(path)?;
    let table = config
        .as_table_mut()
        .context("Codex config must be a TOML table")?;
    let servers = toml_table_entry(table, "mcp_servers", path)?;
    let desired_command = resolve_packet28_mcp_command();
    let desired_args = vec![
        toml::Value::String("--root".to_string()),
        toml::Value::String(root.display().to_string()),
        toml::Value::String("--toolset".to_string()),
        toml::Value::String("core".to_string()),
    ];
    let already_configured = servers
        .get("packet28")
        .and_then(toml::Value::as_table)
        .is_some_and(|packet28| {
            packet28
                .get("command")
                .and_then(toml::Value::as_str)
                .map(str::trim)
                == Some(desired_command.as_str())
                && packet28.get("args").and_then(toml::Value::as_array) == Some(&desired_args)
        });
    if already_configured {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    let mut packet28 = TomlTable::new();
    packet28.insert("command".to_string(), toml::Value::String(desired_command));
    packet28.insert("args".to_string(), toml::Value::Array(desired_args));
    servers.insert("packet28".to_string(), toml::Value::Table(packet28));
    write_toml_config(path, &config)?;
    Ok(McpConfigStatus::Written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_config_path_supports_custom_home_without_changing_project_hooks() {
        let home = Path::new("/synthetic/home");
        assert_eq!(
            config_path_with_override(home, None),
            home.join(".codex/config.toml")
        );
        assert_eq!(
            config_path_with_override(home, Some(Path::new("/custom/codex"))),
            Path::new("/custom/codex/config.toml")
        );
        assert_eq!(
            hooks_path(Path::new("/workspace")),
            Path::new("/workspace/.codex/hooks.json")
        );
    }
    use tempfile::tempdir;

    fn no_commands(_: &str) -> bool {
        false
    }
    fn no_runs(_: &str, _: &[String]) -> Result<bool> {
        Ok(false)
    }

    #[test]
    fn codex_hook_ownership_rejects_custom_shell_tails() {
        let generated = crate::cmd_setup::setup_commands::generated_packet28_hook_command(
            "codex",
            Path::new("/old/workspace"),
        );
        assert!(is_packet28_hook(
            &json!({"type":"command","command":generated})
        ));
        for tail in [
            ";user-audit",
            "&&user-audit",
            "|user-audit",
            " ; user-audit",
            " &user-audit",
        ] {
            assert!(
                !is_packet28_hook(
                    &json!({"type":"command","command":format!("{generated}{tail}")})
                ),
                "{tail}"
            );
        }
    }

    #[test]
    fn codex_hooks_preserve_user_handlers_and_migrate_stale_generated_commands() {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let environment = RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_runs);
        let directory = StateDir::open(root.path(), &[".codex"], true).unwrap();
        let stale = crate::cmd_setup::setup_commands::generated_packet28_hook_command(
            "codex",
            Path::new("/old/workspace"),
        );
        let custom_tail = format!("{stale};user-audit");
        directory
            .write_atomic(
                "hooks.json",
                json!({"description":"user hooks","hooks":{
                    "PreToolUse":[{"matcher":"Bash","hooks":[
                        {"type":"command","command":"echo user-hook"},
                    {"type":"command","command":"sh -c 'echo user policy; exec \"$1\" hook codex --root \"$2\"' packet28-hook Packet28 /user-root"},
                        {"type":"command","command":stale},
                        {"type":"command","command":custom_tail}
                    ]}],
                    "Interrupt":[{"hooks":[{"type":"command","command":"echo interrupt"}]}]
                }})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(
            configure_hooks(&environment, true).unwrap(),
            McpConfigStatus::Written
        );
        let bytes = directory
            .read_bounded("hooks.json", MAX_HOOK_CONFIG_BYTES)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["description"], "user hooks");
        let user_handlers = value["hooks"]["PreToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(user_handlers.len(), 3);
        assert_eq!(user_handlers[0]["command"], "echo user-hook");
        assert!(user_handlers[1]["command"]
            .as_str()
            .unwrap()
            .contains("echo user policy"));
        assert_eq!(user_handlers[2]["command"], custom_tail);
        assert_eq!(
            value["hooks"]["Interrupt"][0]["hooks"][0]["command"],
            "echo interrupt"
        );
        let current = &value["hooks"]["PreToolUse"][1]["hooks"][0];
        assert!(is_packet28_hook(current));
        assert!(current["command"]
            .as_str()
            .unwrap()
            .contains(root.path().to_str().unwrap()));
        assert_eq!(
            configure_hooks(&environment, true).unwrap(),
            McpConfigStatus::AlreadyConfigured
        );
        assert_eq!(
            directory
                .read_bounded("hooks.json", MAX_HOOK_CONFIG_BYTES)
                .unwrap()
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn codex_hooks_reject_invalid_existing_json_without_replacing_it() {
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let environment = RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_runs);
        let directory = StateDir::open(root.path(), &[".codex"], true).unwrap();
        for invalid in ["{", "[]", r#"{"hooks":[]}"#, r#"{"hooks":{"Stop":{}}}"#] {
            directory
                .write_atomic("hooks.json", invalid.as_bytes())
                .unwrap();
            assert!(configure_hooks(&environment, true).is_err());
            assert_eq!(
                directory
                    .read_bounded("hooks.json", MAX_HOOK_CONFIG_BYTES)
                    .unwrap()
                    .unwrap(),
                invalid.as_bytes()
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn codex_hooks_refuse_symlinked_config_without_touching_target() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let environment = RuntimeEnvironment::new(root.path(), home.path(), &no_commands, &no_runs);
        let outside = home.path().join("user-hooks.json");
        std::fs::write(&outside, b"{}").unwrap();
        std::fs::create_dir(root.path().join(".codex")).unwrap();
        symlink(&outside, hooks_path(root.path())).unwrap();
        assert!(configure_hooks(&environment, true).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"{}");
    }
}
