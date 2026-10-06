use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use colored::Colorize;

use super::McpConfigStatus;
use crate::runtime_integrations::hermes;

fn opencode_plugin_content() -> &'static str {
    r#"import type { Plugin } from "@opencode-ai/plugin"

// Packet28 preserves native command arguments and host permission matching.
// Use explicit Packet28 CLI/MCP reduction when reduced output is needed.
// This replaces the legacy automatic rewrite plugin on setup.
export const Packet28OpenCodePlugin: Plugin = async () => ({
  "tool.execute.before": async () => {},
})
"#
}

pub(crate) fn write_opencode_plugin(path: &Path, auto_yes: bool) -> Result<McpConfigStatus> {
    let content = opencode_plugin_content();
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        if existing == content || existing == format!("{content}\n") {
            return Ok(McpConfigStatus::AlreadyConfigured);
        }
    }
    if !auto_yes {
        eprint!(
            "    Write OpenCode plugin to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(McpConfigStatus::Written)
}

fn hermes_plugin_init_content() -> &'static str {
    r#""""Packet28 preserves native command arguments and host permission matching."""


def register(ctx):
    """Replace the legacy automatic rewrite callback without modifying commands."""
    ctx.register_hook("pre_tool_call", _pre_tool_call)


def _pre_tool_call(tool_name=None, args=None, **_kwargs):
    """Use explicit Packet28 CLI/MCP reduction when reduced output is needed."""
    return
"#
}

fn hermes_plugin_manifest_content() -> &'static str {
    r#"name: packet28-rewrite
version: "0.2.0"
description: Preserve native Hermes command arguments. Use explicit Packet28 CLI/MCP reduction.
author: Packet28
hooks:
  - pre_tool_call
provides_hooks:
  - pre_tool_call
"#
}

pub(crate) fn write_hermes_plugin(home: &Path, auto_yes: bool) -> Result<McpConfigStatus> {
    let plugin_dir = hermes::plugin_dir(home);
    let init_path = plugin_dir.join("__init__.py");
    let manifest_path = plugin_dir.join("plugin.yaml");
    let config_path = hermes::config_path(home);
    if hermes_plugin_is_configured(home)? {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    if !auto_yes {
        eprint!(
            "    Write Hermes plugin to {}? [Y/n] ",
            plugin_dir.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let existing = if config_path.exists() {
        fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read '{}'", config_path.display()))?
    } else {
        String::new()
    };
    let patched = patch_hermes_config(&existing)
        .with_context(|| format!("failed to patch '{}'", config_path.display()))?;
    fs::create_dir_all(&plugin_dir)?;
    fs::write(&init_path, hermes_plugin_init_content())?;
    fs::write(&manifest_path, hermes_plugin_manifest_content())?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config_path, patched)?;
    Ok(McpConfigStatus::Written)
}

fn hermes_plugin_is_configured(home: &Path) -> Result<bool> {
    let plugin_dir = hermes::plugin_dir(home);
    let init_path = plugin_dir.join("__init__.py");
    let manifest_path = plugin_dir.join("plugin.yaml");
    let config_path = hermes::config_path(home);
    if !init_path.exists() || !manifest_path.exists() || !config_path.exists() {
        return Ok(false);
    }
    let init = fs::read_to_string(&init_path)
        .with_context(|| format!("failed to read '{}'", init_path.display()))?;
    let manifest = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read '{}'", manifest_path.display()))?;
    let config = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read '{}'", config_path.display()))?;
    Ok(init == hermes_plugin_init_content()
        && manifest == hermes_plugin_manifest_content()
        && hermes_config_enables_packet28(&config).unwrap_or(false))
}

pub(crate) fn patch_hermes_config(existing: &str) -> Result<String> {
    let mut value = if existing.trim().is_empty() {
        yaml_serde::Value::Mapping(Default::default())
    } else {
        yaml_serde::from_str::<yaml_serde::Value>(existing)?
    };
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("Hermes config root must be a YAML mapping"))?;
    let plugins_key = yaml_serde::Value::String("plugins".to_string());
    let enabled_key = yaml_serde::Value::String("enabled".to_string());
    let plugins = root
        .entry(plugins_key)
        .or_insert_with(|| yaml_serde::Value::Mapping(Default::default()))
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("Hermes config 'plugins' must be a YAML mapping"))?;
    let enabled = plugins
        .entry(enabled_key)
        .or_insert_with(|| yaml_serde::Value::Sequence(Vec::new()))
        .as_sequence_mut()
        .ok_or_else(|| anyhow!("Hermes config 'plugins.enabled' must be a YAML sequence"))?;
    let plugin = yaml_serde::Value::String("packet28-rewrite".to_string());
    if !enabled.iter().any(|entry| entry == &plugin) {
        enabled.push(plugin);
    }
    Ok(yaml_serde::to_string(&value)?)
}

pub(crate) fn hermes_config_enables_packet28(content: &str) -> Result<bool> {
    let value = yaml_serde::from_str::<yaml_serde::Value>(content)?;
    Ok(value
        .get("plugins")
        .and_then(|plugins| plugins.get("enabled"))
        .and_then(yaml_serde::Value::as_sequence)
        .is_some_and(|enabled| {
            enabled
                .iter()
                .any(|entry| entry.as_str() == Some("packet28-rewrite"))
        }))
}
