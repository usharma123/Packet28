use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use colored::Colorize;
use serde_json::{json, Value};

use super::setup_commands::{apply_generated_relaunch_command, generated_packet28_hook_command};
use super::McpConfigStatus;

const PACKET28_CLAUDE_HTTP_HOOK_PATH: &str = "/packet28/claude-hook";
const PACKET28_CLAUDE_HTTP_TOKEN_HEADER: &str = "X-Packet28-Hook-Token";

pub(crate) fn write_claude_hook_config(
    path: &Path,
    root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let hook_command = generated_packet28_hook_command("claude", root);
    let mut config: BTreeMap<String, Value> = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "refusing to overwrite invalid JSON in '{}'; fix the file and rerun setup",
                path.display()
            )
        })?
    } else {
        BTreeMap::new()
    };
    let mut hooks = json_object_field_or_default(&config, "hooks", path)?;
    if !auto_yes {
        eprint!(
            "    Write Claude hook config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let runtime_config = ensure_hook_http_settings_written(root)?;
    let http_url = claude_http_hook_url(&runtime_config)
        .context("Packet28 Claude HTTP hook settings are incomplete after initialization")?;
    let http_token = runtime_config
        .http_hook_token
        .as_deref()
        .context("Packet28 Claude HTTP hook token is missing after initialization")?;
    let packet28_hooks = build_claude_packet28_hooks(&hook_command, &http_url, http_token);
    // Claude Code expects hook event names (PreToolUse, Stop, etc.) as
    // direct keys under `hooks`. Merge our entries into each event key
    // rather than nesting under a "packet28" grouping key.
    let packet28_events = packet28_hooks.as_object().cloned().unwrap_or_default();
    let mut already_configured = true;
    for (event_name, entries) in &packet28_events {
        let existing = hooks
            .get(event_name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let new_entries = entries.as_array().cloned().unwrap_or_default();
        let mut merged = existing
            .iter()
            .filter(|entry| !is_packet28_claude_hook_entry(entry))
            .cloned()
            .collect::<Vec<_>>();
        merged.extend(new_entries);
        if merged != existing {
            already_configured = false;
            hooks.insert(event_name.clone(), Value::Array(merged));
        }
    }
    // Remove legacy "packet28" grouping key if present.
    if hooks.contains_key("packet28") {
        hooks.remove("packet28");
        already_configured = false;
    }
    if merge_claude_allowed_http_hook_url(&mut config, &http_url) {
        already_configured = false;
    }
    if already_configured {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    config.insert("hooks".to_string(), Value::Object(hooks));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

fn build_claude_packet28_hooks(command: &str, http_url: &str, http_token: &str) -> Value {
    json!({
        "SessionStart": [
            claude_command_hook_entry(Some("startup|resume|clear|compact"), command),
            claude_http_hook_entry(Some("fork"), http_url, http_token)
        ],
        "UserPromptSubmit": [claude_command_hook_entry(None, command)],
        "PreToolUse": [claude_http_hook_entry(Some("*"), http_url, http_token)],
        "PostToolUse": [claude_http_hook_entry(Some("*"), http_url, http_token)],
        "PostToolUseFailure": [claude_http_hook_entry(Some("*"), http_url, http_token)],
        "Stop": [claude_http_hook_entry(None, http_url, http_token)],
        "SubagentStop": [claude_http_hook_entry(Some("*"), http_url, http_token)],
        "PreCompact": [claude_http_hook_entry(Some("manual|auto"), http_url, http_token)],
        "SessionEnd": [claude_http_hook_entry(Some("*"), http_url, http_token)]
    })
}

fn is_packet28_claude_hook_entry(entry: &Value) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.iter().any(|hook| {
        if let Some(url) = hook.get("url").and_then(Value::as_str) {
            return url.contains(PACKET28_CLAUDE_HTTP_HOOK_PATH);
        }
        let command = hook
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        command.contains(" hook claude ")
            && (command.contains("Packet28") || command.contains("packet28"))
    })
}

fn claude_command_hook_entry(matcher: Option<&str>, command: &str) -> Value {
    let mut entry = json!({
        "hooks": [{"type": "command", "command": command}]
    });
    if let Some(matcher) = matcher {
        entry["matcher"] = Value::String(matcher.to_string());
    }
    entry
}

fn claude_http_hook_entry(matcher: Option<&str>, http_url: &str, http_token: &str) -> Value {
    let mut entry = json!({
        "hooks": [{
            "type": "http",
            "url": http_url,
            "headers": {
                (PACKET28_CLAUDE_HTTP_TOKEN_HEADER): http_token
            }
        }]
    });
    if let Some(matcher) = matcher {
        entry["matcher"] = Value::String(matcher.to_string());
    }
    entry
}

fn ensure_hook_http_settings_written(
    root: &Path,
) -> Result<packet28_daemon_protocol::hooks::HookRuntimeConfig> {
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(root);
    let existed = path.exists();
    let mut config = if existed {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str::<packet28_daemon_protocol::hooks::HookRuntimeConfig>(&content)
            .with_context(|| {
                format!("refusing to overwrite invalid JSON in '{}'", path.display())
            })?
    } else {
        packet28_daemon_protocol::hooks::HookRuntimeConfig::default()
    };
    let changed = apply_generated_http_hook_settings(&mut config, root);
    if !existed || changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&config)?),
        )?;
    }
    Ok(config)
}

fn apply_generated_http_hook_settings(
    config: &mut packet28_daemon_protocol::hooks::HookRuntimeConfig,
    root: &Path,
) -> bool {
    let mut changed = false;
    if config.http_hook_port.is_none() {
        config.http_hook_port = Some(select_loopback_port().unwrap_or(45123));
        changed = true;
    }
    let token_missing = config
        .http_hook_token
        .as_deref()
        .map(str::trim)
        .map(str::is_empty)
        .unwrap_or(true);
    if token_missing {
        config.http_hook_token = Some(generate_http_hook_token(root));
        changed = true;
    }
    changed
}

fn select_loopback_port() -> Option<u16> {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|listener| listener.local_addr().ok().map(|addr| addr.port()))
        .filter(|port| *port != 0)
}

fn generate_http_hook_token(root: &Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let seed = format!("{}:{}:{}", root.display(), std::process::id(), now_nanos);
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}

fn claude_http_hook_url(
    config: &packet28_daemon_protocol::hooks::HookRuntimeConfig,
) -> Option<String> {
    config
        .http_hook_port
        .map(|port| format!("http://127.0.0.1:{port}{PACKET28_CLAUDE_HTTP_HOOK_PATH}"))
}

fn merge_claude_allowed_http_hook_url(
    config: &mut BTreeMap<String, Value>,
    http_url: &str,
) -> bool {
    let mut urls = config
        .get("allowedHttpHookUrls")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if urls.iter().any(|entry| entry == http_url) {
        return false;
    }
    urls.push(http_url.to_string());
    config.insert(
        "allowedHttpHookUrls".to_string(),
        Value::Array(urls.into_iter().map(Value::String).collect()),
    );
    true
}

pub(crate) fn write_cursor_hook_config(
    path: &Path,
    root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let hook_command = generated_packet28_hook_command("cursor", root);
    let packet28_hooks = json!({
        "beforeSubmitPrompt": [{
            "command": hook_command
        }],
        "beforeShellExecution": [{
            "command": hook_command
        }],
        "afterShellExecution": [{
            "command": hook_command
        }],
        "stop": [{
            "command": hook_command
        }]
    });
    let mut config: BTreeMap<String, Value> = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "refusing to overwrite invalid JSON in '{}'; fix the file and rerun setup",
                path.display()
            )
        })?
    } else {
        BTreeMap::new()
    };
    if !auto_yes {
        eprint!(
            "    Write Cursor hook config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let mut hooks = json_object_field_or_default(&config, "hooks", path)?;
    let packet28_events = packet28_hooks.as_object().cloned().unwrap_or_default();
    let mut already_configured = true;
    for (event_name, entries) in &packet28_events {
        let existing = hooks
            .get(event_name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let new_entries = entries.as_array().cloned().unwrap_or_default();
        let hook_present = new_entries
            .iter()
            .all(|new_entry| existing.iter().any(|entry| entry == new_entry));
        if hook_present {
            continue;
        }
        already_configured = false;
        let mut merged = existing;
        merged.extend(new_entries);
        hooks.insert(event_name.clone(), Value::Array(merged));
    }
    if already_configured {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    config.insert("hooks".to_string(), Value::Object(hooks));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

pub(crate) fn write_gemini_hook_config(
    path: &Path,
    root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let hook_command = generated_packet28_hook_command("gemini", root);
    let packet28_hook = json!({
        "matcher": "run_shell_command",
        "hooks": [{
            "type": "command",
            "command": hook_command
        }]
    });
    let mut config: BTreeMap<String, Value> = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "refusing to overwrite invalid JSON in '{}'; fix the file and rerun setup",
                path.display()
            )
        })?
    } else {
        BTreeMap::new()
    };
    if !auto_yes {
        eprint!(
            "    Write Gemini hook config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let mut hooks = json_object_field_or_default(&config, "hooks", path)?;
    let existing = hooks
        .get("BeforeTool")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if existing.iter().any(|entry| entry == &packet28_hook) {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    let mut merged = existing;
    merged.push(packet28_hook);
    hooks.insert("BeforeTool".to_string(), Value::Array(merged));
    config.insert("hooks".to_string(), Value::Object(hooks));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

pub(crate) fn write_copilot_hook_config(
    path: &Path,
    root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let hook_command = generated_packet28_hook_command("copilot", root);
    let packet28_hook = json!({
        "type": "command",
        "command": hook_command,
        "cwd": ".",
        "timeout": 5
    });
    let mut config: BTreeMap<String, Value> = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "refusing to overwrite invalid JSON in '{}'; fix the file and rerun setup",
                path.display()
            )
        })?
    } else {
        BTreeMap::new()
    };
    if !auto_yes {
        eprint!(
            "    Write Copilot hook config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let mut hooks = json_object_field_or_default(&config, "hooks", path)?;
    let existing = hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if existing.iter().any(|entry| entry == &packet28_hook) {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    let mut merged = existing;
    merged.push(packet28_hook);
    hooks.insert("PreToolUse".to_string(), Value::Array(merged));
    config.insert("hooks".to_string(), Value::Object(hooks));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

pub(crate) fn write_windsurf_hook_config(
    path: &Path,
    root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    let hook_command = generated_packet28_hook_command("windsurf", root);
    let packet28_hooks = json!({
        "pre_user_prompt": [{
            "command": hook_command
        }],
        "pre_run_command": [{
            "command": hook_command
        }],
        "post_run_command": [{
            "command": hook_command
        }],
        "post_cascade_response": [{
            "command": hook_command
        }]
    });
    let mut config: BTreeMap<String, Value> = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str(&content).with_context(|| {
            format!(
                "refusing to overwrite invalid JSON in '{}'; fix the file and rerun setup",
                path.display()
            )
        })?
    } else {
        BTreeMap::new()
    };
    if !auto_yes {
        eprint!(
            "    Write Windsurf hook config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }
    let mut hooks = json_object_field_or_default(&config, "hooks", path)?;
    let packet28_events = packet28_hooks.as_object().cloned().unwrap_or_default();
    let mut already_configured = true;
    for (event_name, entries) in &packet28_events {
        let existing = hooks
            .get(event_name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let new_entries = entries.as_array().cloned().unwrap_or_default();
        let hook_present = new_entries
            .iter()
            .all(|new_entry| existing.iter().any(|entry| entry == new_entry));
        if hook_present {
            continue;
        }
        already_configured = false;
        let mut merged = existing;
        merged.extend(new_entries);
        hooks.insert(event_name.clone(), Value::Array(merged));
    }
    if already_configured {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    config.insert("hooks".to_string(), Value::Object(hooks));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

/// Writes the shared hook runtime configuration when at least one hook has been configured.
///
/// Re-enables hook ingestion when it is disabled and reports whether the configuration
/// was declined, already configured, or written.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let status = write_hook_runtime_config(Path::new("."), false).unwrap();
/// assert!(matches!(status, McpConfigStatus::Declined));
/// ```
pub(crate) fn write_hook_runtime_config(
    root: &Path,
    any_hooks_configured: bool,
) -> Result<McpConfigStatus> {
    if !any_hooks_configured {
        return Ok(McpConfigStatus::Declined);
    }
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(root);
    let existed = path.exists();
    let mut config = if existed {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        serde_json::from_str::<packet28_daemon_protocol::hooks::HookRuntimeConfig>(&content)
            .with_context(|| {
                format!("refusing to overwrite invalid JSON in '{}'", path.display())
            })?
    } else {
        packet28_daemon_protocol::hooks::HookRuntimeConfig::default()
    };
    let mut changed = apply_generated_http_hook_settings(&mut config, root);
    changed |= apply_generated_relaunch_command(&mut config);
    // Configuring a hook runtime is an explicit opt-in to hook ingest. If a
    // prior `packet28 uninstall` (or a manual edit) left the kill switch
    // engaged, re-enable it here. Otherwise setup reports the HTTP hook as
    // healthy while the daemon keeps rejecting every ingest with
    // `accepted: false`, which surfaces downstream as confusing doctor
    // failures instead of an honest "hooks are disabled" signal.
    if !config.hooks_enabled {
        config.hooks_enabled = true;
        changed = true;
    }
    if existed && !changed {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

fn json_object_field_or_default(
    config: &BTreeMap<String, Value>,
    field: &str,
    path: &Path,
) -> Result<serde_json::Map<String, Value>> {
    match config.get(field) {
        None => Ok(serde_json::Map::new()),
        Some(Value::Object(value)) => Ok(value.clone()),
        Some(_) => Err(anyhow!(
            "refusing to overwrite '{field}' in '{}'; expected a JSON object",
            path.display()
        )),
    }
}
