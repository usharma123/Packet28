//! `Packet28 uninstall`: tear down everything `Packet28 setup` wired into a
//! workspace so no hook, MCP entry, or background process keeps Packet28 alive.
//!
//! Removing the npm package alone is not enough: hooks live in runtime config
//! files (for example `<root>/.claude/settings.json`) and keep firing on every
//! new session or subagent, and each hook invocation auto-starts `packet28d`
//! and the Claude HTTP hook server. This command:
//!
//! 1. flips `hooks_enabled=false` in `.packet28/daemon/hook-runtime-v1.json`
//!    so any hook that still fires becomes a no-op and never starts a process,
//! 2. asks the Claude HTTP hook server and `packet28d` for this workspace to exit,
//! 3. strips Packet28 hook entries from every runtime hook config it knows,
//! 4. removes the `packet28` MCP server entry unless `--keep-mcp` is passed.
//!
//! Workspace data under `.packet28/` is left in place.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use colored::Colorize;
use packet28_daemon_protocol::hooks::{HookRuntimeConfig, RelaunchPreference};
use packet28_daemon_protocol::paths::hook_runtime_config_path;
use serde_json::Value;

use crate::runtime_integrations::{adapters, RuntimeEnvironment};

#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Workspace root for Packet28
    #[arg(long, default_value = ".")]
    pub root: String,
    /// Keep the `packet28` MCP server entries in runtime MCP configs
    #[arg(long)]
    pub keep_mcp: bool,
    /// Report what would change without writing files or stopping processes
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UninstallOptions {
    pub(crate) keep_mcp: bool,
    pub(crate) dry_run: bool,
}

#[derive(Debug, Default)]
pub(crate) struct UninstallReport {
    pub(crate) hook_runtime_disabled: Option<PathBuf>,
    pub(crate) hook_http_server_stopped: bool,
    pub(crate) daemon_stopped: bool,
    pub(crate) hook_files_changed: Vec<PathBuf>,
    pub(crate) mcp_files_changed: Vec<PathBuf>,
    pub(crate) left_in_place: Vec<PathBuf>,
    pub(crate) warnings: Vec<String>,
}

pub fn run(args: UninstallArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    let environment = RuntimeEnvironment::from_process(&root);
    let options = UninstallOptions {
        keep_mcp: args.keep_mcp,
        dry_run: args.dry_run,
    };
    let report = uninstall_workspace(&environment, options)?;
    print_report(&root, &report, options);
    Ok(i32::from(!report.warnings.is_empty()))
}

pub(crate) fn uninstall_workspace(
    environment: &RuntimeEnvironment<'_>,
    options: UninstallOptions,
) -> Result<UninstallReport> {
    let root = environment.root();
    // Disable hooks before tearing down processes.
    let mut report = UninstallReport {
        hook_runtime_disabled: disable_hook_runtime(root, options.dry_run)?,
        ..UninstallReport::default()
    };

    // 2. Stop long-lived processes for this workspace.
    if !options.dry_run {
        match crate::cmd_hook_http::stop_hook_http_server(root) {
            Ok(stopped) => report.hook_http_server_stopped = stopped,
            Err(err) => report
                .warnings
                .push(format!("could not stop Claude HTTP hook server: {err:#}")),
        }
        match crate::cmd_daemon_client::stop_daemon_and_wait(root) {
            Ok(acknowledgement) => report.daemon_stopped = acknowledgement.is_some(),
            Err(err) => report
                .warnings
                .push(format!("could not stop packet28d: {err:#}")),
        }
    }

    // 3. Strip hook entries and MCP servers from every runtime config we manage.
    let runtime_config_path = hook_runtime_config_path(root);
    for adapter in adapters() {
        if let Some(hooks) = adapter.hooks {
            for path in hooks.artifacts(environment) {
                if path == runtime_config_path || !path.exists() {
                    continue;
                }
                // Shared hooks serve other workspaces too. The workspace kill
                // switch disables their activity here without deleting them globally.
                if !path.starts_with(root) {
                    report.left_in_place.push(path);
                    continue;
                }
                match strip_packet28_hooks_from_file(&path, options.dry_run) {
                    Ok(StripOutcome::Changed) => report.hook_files_changed.push(path),
                    Ok(StripOutcome::Unchanged) => {}
                    Ok(StripOutcome::Unsupported) => report.left_in_place.push(path),
                    Err(err) => report.warnings.push(format!("{}: {err:#}", path.display())),
                }
            }
        }
        if options.keep_mcp {
            continue;
        }
        if let Some(mcp) = adapter.mcp {
            for path in mcp.artifacts(environment) {
                if !path.exists() {
                    continue;
                }
                // Configs that live inside the workspace belong to it. Configs
                // outside the workspace (e.g. `~/.codex/config.toml`) are
                // shared across workspaces: only drop the `packet28` entry when
                // it actually points at *this* root, otherwise leave it alone.
                let scope = if path.starts_with(root) {
                    McpRemovalScope::WorkspaceOwned
                } else {
                    McpRemovalScope::SharedForRoot(root)
                };
                match remove_packet28_mcp_server_from_file(&path, scope, options.dry_run) {
                    Ok(StripOutcome::Changed) => report.mcp_files_changed.push(path),
                    Ok(StripOutcome::Unchanged) => {}
                    Ok(StripOutcome::Unsupported) => report.left_in_place.push(path),
                    Err(err) => report.warnings.push(format!("{}: {err:#}", path.display())),
                }
            }
        }
    }
    report.hook_files_changed.sort();
    report.hook_files_changed.dedup();
    report.mcp_files_changed.sort();
    report.mcp_files_changed.dedup();
    report.left_in_place.sort();
    report.left_in_place.dedup();
    Ok(report)
}

fn disable_hook_runtime(root: &Path, dry_run: bool) -> Result<Option<PathBuf>> {
    let path = hook_runtime_config_path(root);
    let mut config = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<HookRuntimeConfig>(&raw)
            .with_context(|| format!("refusing to overwrite invalid config {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HookRuntimeConfig::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    config.hooks_enabled = false;
    config.rewrite_enabled = false;
    config.relaunch_preference = RelaunchPreference::HostManaged;
    config.relaunch_command.clear();
    if !dry_run {
        write_json_atomically(&path, &serde_json::to_value(&config)?)?;
    }
    Ok(Some(path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripOutcome {
    Changed,
    Unchanged,
    Unsupported,
}

pub(crate) fn strip_packet28_hooks_from_file(path: &Path, dry_run: bool) -> Result<StripOutcome> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
        return Ok(StripOutcome::Unsupported);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("refusing to edit invalid JSON in '{}'", path.display()))?;
    if !strip_packet28_hooks(&mut value) {
        return Ok(StripOutcome::Unchanged);
    }
    if !dry_run {
        write_json_atomically(path, &value)?;
    }
    Ok(StripOutcome::Changed)
}

/// Remove every hook entry Packet28 installed from a runtime hook config.
/// Returns true when the document changed.
pub(crate) fn strip_packet28_hooks(value: &mut Value) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if let Some(Value::Object(hooks)) = object.get_mut("hooks") {
        let mut empty_events = Vec::new();
        for (event, entries) in hooks.iter_mut() {
            let Some(entries) = entries.as_array_mut() else {
                continue;
            };
            let before = entries.len();
            entries.retain_mut(|entry| {
                if let Some(children) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                    let old_len = children.len();
                    children.retain(|child| !mentions_packet28_hook(child));
                    changed |= children.len() != old_len;
                    !children.is_empty()
                } else {
                    !mentions_packet28_hook(entry)
                }
            });
            if entries.len() != before {
                changed = true;
            }
            if entries.is_empty() {
                empty_events.push(event.clone());
            }
        }
        for event in empty_events {
            hooks.remove(&event);
            changed = true;
        }
        if hooks.is_empty() {
            object.remove("hooks");
            changed = true;
        }
    }
    if let Some(Value::Array(urls)) = object.get_mut("allowedHttpHookUrls") {
        let before = urls.len();
        urls.retain(|url| {
            !url.as_str()
                .map(|url| url.contains("/packet28/"))
                .unwrap_or(false)
        });
        if urls.len() != before {
            changed = true;
        }
        if urls.is_empty() {
            object.remove("allowedHttpHookUrls");
            changed = true;
        }
    }
    changed
}

/// True when any string inside `entry` looks like a Packet28 hook command or
/// Packet28 HTTP hook URL.
fn mentions_packet28_hook(entry: &Value) -> bool {
    match entry {
        Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            lower.contains("/packet28/claude-hook")
                || (lower.contains("packet28") && lower.contains(" hook "))
        }
        Value::Array(items) => items.iter().any(mentions_packet28_hook),
        Value::Object(map) => map.values().any(mentions_packet28_hook),
        _ => false,
    }
}

/// How aggressively to remove a `packet28` MCP server entry from a config file.
#[derive(Debug, Clone, Copy)]
pub(crate) enum McpRemovalScope<'a> {
    /// The config file lives inside the workspace; any `packet28` entry is ours.
    WorkspaceOwned,
    /// The config file is shared (user-global). Only remove the entry when its
    /// `args` reference this workspace root via `--root <root>`.
    SharedForRoot(&'a Path),
}

fn same_root(candidate: &str, root: &Path) -> bool {
    let candidate = Path::new(candidate);
    if candidate == root {
        return true;
    }
    match (fs::canonicalize(candidate), fs::canonicalize(root)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `args` of the form `[..., "--root", "<root>", ...]` reference `root`.
fn args_reference_root<'a>(mut args: impl Iterator<Item = &'a str>, root: &Path) -> bool {
    while let Some(arg) = args.next() {
        if arg == "--root" {
            if let Some(value) = args.next() {
                if same_root(value, root) {
                    return true;
                }
            }
        } else if let Some(value) = arg.strip_prefix("--root=") {
            if same_root(value, root) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn remove_packet28_mcp_server_from_file(
    path: &Path,
    scope: McpRemovalScope<'_>,
    dry_run: bool,
) -> Result<StripOutcome> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("json") => {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("failed to read '{}'", path.display()))?;
            let mut value: Value = serde_json::from_str(&raw).with_context(|| {
                format!("refusing to edit invalid JSON in '{}'", path.display())
            })?;
            let removed = value
                .get_mut("mcpServers")
                .and_then(Value::as_object_mut)
                .map(|servers| {
                    let ours = match scope {
                        McpRemovalScope::WorkspaceOwned => servers.contains_key("packet28"),
                        McpRemovalScope::SharedForRoot(root) => servers
                            .get("packet28")
                            .and_then(|entry| entry.get("args"))
                            .and_then(Value::as_array)
                            .is_some_and(|args| {
                                args_reference_root(args.iter().filter_map(Value::as_str), root)
                            }),
                    };
                    ours && servers.remove("packet28").is_some()
                })
                .unwrap_or(false);
            if !removed {
                return Ok(StripOutcome::Unchanged);
            }
            if !dry_run {
                write_json_atomically(path, &value)?;
            }
            Ok(StripOutcome::Changed)
        }
        Some("toml") => {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("failed to read '{}'", path.display()))?;
            let mut value: toml::Value = toml::from_str(&raw).with_context(|| {
                format!("refusing to edit invalid TOML in '{}'", path.display())
            })?;
            let removed = value
                .get_mut("mcp_servers")
                .and_then(toml::Value::as_table_mut)
                .map(|servers| {
                    let ours = match scope {
                        McpRemovalScope::WorkspaceOwned => servers.contains_key("packet28"),
                        McpRemovalScope::SharedForRoot(root) => servers
                            .get("packet28")
                            .and_then(|entry| entry.get("args"))
                            .and_then(toml::Value::as_array)
                            .is_some_and(|args| {
                                args_reference_root(
                                    args.iter().filter_map(toml::Value::as_str),
                                    root,
                                )
                            }),
                    };
                    ours && servers.remove("packet28").is_some()
                })
                .unwrap_or(false);
            if !removed {
                return Ok(StripOutcome::Unchanged);
            }
            if !dry_run {
                let rendered = toml::to_string_pretty(&value)
                    .with_context(|| format!("failed to render '{}'", path.display()))?;
                write_atomically(path, rendered.as_bytes())?;
            }
            Ok(StripOutcome::Changed)
        }
        _ => Ok(StripOutcome::Unsupported),
    }
}

fn write_json_atomically(path: &Path, value: &Value) -> Result<()> {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value)?);
    write_atomically(path, rendered.as_bytes())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("'{}' has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create '{}'", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write '{}'", temp_path.display()))?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp_path, metadata.permissions())?;
    }
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace '{}' with '{}'",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

fn print_report(root: &Path, report: &UninstallReport, options: UninstallOptions) {
    let prefix = if options.dry_run { "would " } else { "" };
    println!(
        "{} Packet28 from {}",
        if options.dry_run {
            "Dry run: uninstalling".bold()
        } else {
            "Uninstalled".bold()
        },
        root.display()
    );
    match &report.hook_runtime_disabled {
        Some(path) => println!("  {prefix}disable hooks in {}", path.display()),
        None => println!("  no hook runtime config found (hooks were never set up here)"),
    }
    if !options.dry_run {
        println!(
            "  Claude HTTP hook server: {}",
            if report.hook_http_server_stopped {
                "stopped"
            } else {
                "not running"
            }
        );
        println!(
            "  packet28d: {}",
            if report.daemon_stopped {
                "stopped"
            } else {
                "not running"
            }
        );
    }
    for path in &report.hook_files_changed {
        println!("  {prefix}remove Packet28 hooks from {}", path.display());
    }
    for path in &report.mcp_files_changed {
        println!(
            "  {prefix}remove packet28 MCP server from {}",
            path.display()
        );
    }
    for path in &report.left_in_place {
        println!(
            "  {} left in place; remove it manually if you no longer want it",
            path.display()
        );
    }
    for warning in &report.warnings {
        println!("  {} {warning}", "warning:".yellow());
    }
    println!(
        "  workspace data under {} was kept; delete it if you want a clean slate",
        root.join(".packet28").display()
    );
    println!(
        "  run this before `npm uninstall -g packet28` so nothing keeps relaunching the daemon"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_claude_command_and_http_hooks_but_keeps_user_hooks() {
        let mut settings = json!({
            "allowedHttpHookUrls": [
                "http://127.0.0.1:52327/packet28/claude-hook",
                "http://127.0.0.1:9000/other"
            ],
            "hooks": {
                "SessionStart": [
                    {"matcher": "startup", "hooks": [{"type": "command", "command": "sh -c 'exec \"$1\" hook claude --root \"$2\"' packet28-hook \"/usr/local/bin/Packet28\" \"${CLAUDE_PROJECT_DIR}\""}]},
                    {"matcher": "fork", "hooks": [{"type": "http", "url": "http://127.0.0.1:52327/packet28/claude-hook"}]},
                    {"hooks": [{"type": "command", "command": "echo hello"}]}
                ],
                "PreToolUse": [
                    {"matcher": "*", "hooks": [{"type": "http", "url": "http://127.0.0.1:52327/packet28/claude-hook"}]}
                ]
            },
            "permissions": {"allow": ["Bash"]}
        });
        assert!(strip_packet28_hooks(&mut settings));
        assert_eq!(
            settings["hooks"]["SessionStart"],
            json!([{"hooks": [{"type": "command", "command": "echo hello"}]}])
        );
        assert!(settings["hooks"].get("PreToolUse").is_none());
        assert_eq!(
            settings["allowedHttpHookUrls"],
            json!(["http://127.0.0.1:9000/other"])
        );
        assert_eq!(settings["permissions"], json!({"allow": ["Bash"]}));
        assert!(!strip_packet28_hooks(&mut settings));
    }

    #[test]
    fn uninstall_preserves_user_hook_in_same_matcher() {
        let user = json!({"type": "command", "command": "/repo/Packet28/scripts/user-check"});
        let mut settings = json!({"hooks": {"Stop": [{"matcher": "*", "hooks": [
            {"type": "command", "command": "Packet28 hook claude --root /repo"}, user.clone()
        ]}]}});
        assert!(strip_packet28_hooks(&mut settings));
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"], json!([user]));
        assert!(!strip_packet28_hooks(&mut settings));
    }

    #[test]
    fn strips_cursor_style_hooks_and_drops_empty_hooks_object() {
        let mut hooks = json!({
            "hooks": {
                "beforeShellExecution": [{"command": "sh -c '...' packet28-hook \"Packet28\" \"/repo\" hook cursor"}],
                "stop": [{"command": "\"/usr/local/bin/Packet28\" hook cursor --root \"/repo\""}]
            }
        });
        assert!(strip_packet28_hooks(&mut hooks));
        assert_eq!(hooks, json!({}));
    }

    #[test]
    fn removes_mcp_server_from_json_and_toml() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join(".mcp.json");
        fs::write(
            &json_path,
            r#"{"mcpServers":{"packet28":{"command":"packet28-mcp"},"other":{"command":"x"}}}"#,
        )
        .unwrap();
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &json_path,
                McpRemovalScope::WorkspaceOwned,
                false
            )
            .unwrap(),
            StripOutcome::Changed
        );
        let value: Value = serde_json::from_str(&fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(value, json!({"mcpServers": {"other": {"command": "x"}}}));
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &json_path,
                McpRemovalScope::WorkspaceOwned,
                false
            )
            .unwrap(),
            StripOutcome::Unchanged
        );

        let toml_path = dir.path().join("config.toml");
        fs::write(
            &toml_path,
            "model = \"gpt\"\n\n[mcp_servers.packet28]\ncommand = \"packet28-mcp\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &toml_path,
                McpRemovalScope::WorkspaceOwned,
                false
            )
            .unwrap(),
            StripOutcome::Changed
        );
        let value: toml::Value = toml::from_str(&fs::read_to_string(&toml_path).unwrap()).unwrap();
        assert_eq!(value["model"].as_str(), Some("gpt"));
        assert!(value["mcp_servers"].get("packet28").is_none());
        assert!(value["mcp_servers"].get("other").is_some());
    }

    #[test]
    fn shared_config_only_loses_entry_that_points_at_this_root() {
        let dir = tempfile::tempdir().unwrap();
        let this_root = dir.path().join("ws-a");
        let other_root = dir.path().join("ws-b");
        fs::create_dir_all(&this_root).unwrap();
        fs::create_dir_all(&other_root).unwrap();

        // Entry for another workspace: must be left untouched.
        let toml_path = dir.path().join("config.toml");
        let other_entry = format!(
            "[mcp_servers.packet28]\ncommand = \"packet28-mcp\"\nargs = [\"--root\", \"{}\", \"--toolset\", \"core\"]\n",
            other_root.display()
        );
        fs::write(&toml_path, &other_entry).unwrap();
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &toml_path,
                McpRemovalScope::SharedForRoot(&this_root),
                false
            )
            .unwrap(),
            StripOutcome::Unchanged
        );
        assert_eq!(fs::read_to_string(&toml_path).unwrap(), other_entry);

        // Entry for this workspace: removed.
        let ours = format!(
            "[mcp_servers.packet28]\ncommand = \"packet28-mcp\"\nargs = [\"--root\", \"{}\", \"--toolset\", \"core\"]\n",
            this_root.display()
        );
        fs::write(&toml_path, ours).unwrap();
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &toml_path,
                McpRemovalScope::SharedForRoot(&this_root),
                false
            )
            .unwrap(),
            StripOutcome::Changed
        );
        let value: toml::Value = toml::from_str(&fs::read_to_string(&toml_path).unwrap()).unwrap();
        assert!(value
            .get("mcp_servers")
            .and_then(|s| s.get("packet28"))
            .is_none());

        // JSON variant, entry without any --root: not ours when shared.
        let json_path = dir.path().join("mcp.json");
        fs::write(
            &json_path,
            r#"{"mcpServers":{"packet28":{"command":"packet28-mcp"}}}"#,
        )
        .unwrap();
        assert_eq!(
            remove_packet28_mcp_server_from_file(
                &json_path,
                McpRemovalScope::SharedForRoot(&this_root),
                false
            )
            .unwrap(),
            StripOutcome::Unchanged
        );
    }

    #[test]
    fn dry_run_does_not_touch_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = r#"{"hooks":{"Stop":[{"hooks":[{"type":"http","url":"http://127.0.0.1:1/packet28/claude-hook"}]}]}}"#;
        fs::write(&path, original).unwrap();
        assert_eq!(
            strip_packet28_hooks_from_file(&path, true).unwrap(),
            StripOutcome::Changed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn disable_hook_runtime_flips_kill_switch_and_clears_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let path = hook_runtime_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let config = HookRuntimeConfig {
            relaunch_preference: RelaunchPreference::DaemonManaged,
            relaunch_command: vec!["claude".to_string(), "--continue".to_string()],
            http_hook_port: Some(52327),
            http_hook_token: Some("token".to_string()),
            ..HookRuntimeConfig::default()
        };
        fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();

        assert_eq!(
            disable_hook_runtime(dir.path(), false).unwrap(),
            Some(path.clone())
        );
        let written: HookRuntimeConfig =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!written.hooks_enabled);
        assert!(!written.rewrite_enabled);
        assert_eq!(written.relaunch_preference, RelaunchPreference::HostManaged);
        assert!(written.relaunch_command.is_empty());
        // Port and token stay so a still-running HTTP server can be reached and stopped.
        assert_eq!(written.http_hook_port, Some(52327));
        assert_eq!(written.http_hook_token.as_deref(), Some("token"));
    }

    #[test]
    fn disable_hook_runtime_creates_kill_switch_without_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = hook_runtime_config_path(dir.path());
        assert_eq!(
            disable_hook_runtime(dir.path(), false).unwrap(),
            Some(path.clone())
        );
        let config: HookRuntimeConfig = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!config.hooks_enabled);
    }
}
