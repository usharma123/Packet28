use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use packet28_daemon_core::{
    DaemonIndexRebuildRequest, DaemonIndexStatusRequest, DaemonRequest, DaemonResponse,
};
use serde_json::{json, Value};

use crate::agent_surface;

#[derive(Args)]
pub struct SetupArgs {
    /// Workspace root for Packet28
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Skip interactive prompts and auto-configure all detected runtimes
    #[arg(long)]
    pub yes: bool,

    /// Only generate agent.md fallback files, skip MCP config
    #[arg(long)]
    pub fallback_only: bool,

    /// Specific runtime to configure (claude, cursor, codex, all)
    #[arg(long, default_value = "all")]
    pub runtime: String,
}

struct RuntimeInfo {
    name: &'static str,
    slug: &'static str,
    mcp_config_path: Option<PathBuf>,
    hook_config_path: Option<PathBuf>,
    agent_file_path: PathBuf,
    agent_format: agent_surface::AgentPromptFormat,
    detected: bool,
}

enum McpConfigStatus {
    Written,
    AlreadyConfigured,
    Declined,
}

pub fn run(args: SetupArgs) -> Result<i32> {
    let root = crate::cmd_daemon::resolve_root_arg(&args.root);
    let root_display = root.display().to_string();

    println!();
    println!("{}", "  Packet28 Setup  ".bold().white().on_bright_blue());
    println!();
    println!("  Workspace: {}", root_display.cyan());
    println!();

    // Detect runtimes
    let mut runtimes = detect_runtimes(&root);

    // Filter by --runtime flag
    if args.runtime != "all" {
        runtimes.retain(|r| r.slug == args.runtime);
        if runtimes.is_empty() {
            eprintln!(
                "{} unknown runtime '{}'. Use: claude, cursor, codex, or all",
                "error:".red().bold(),
                args.runtime
            );
            return Ok(1);
        }
    }

    // Print detection results
    println!("  {}", "Detected runtimes:".bold());
    for rt in &runtimes {
        let status = if rt.detected {
            "found".green().bold()
        } else {
            "not found".dimmed()
        };
        println!("    {} {}", rt.name, status);
    }
    println!();

    let selected_runtimes = select_setup_runtimes(&runtimes, args.runtime != "all");
    let mut agent_targets = Vec::new();
    let mut mcp_configured = false;
    let mut hook_configured = false;
    let mut any_hooks_configured = false;
    let mut agent_files_ready = false;

    // Phase 1: MCP config
    if !args.fallback_only {
        let mcp_targets: Vec<&RuntimeInfo> = selected_runtimes
            .iter()
            .copied()
            .filter(|r| r.mcp_config_path.is_some())
            .collect();

        if mcp_targets.is_empty() {
            println!(
                "  {} No MCP-capable runtimes selected. Generating fallback files.",
                "→".yellow()
            );
            println!();
        } else {
            println!("  {}", "Configuring MCP servers:".bold());
            for rt in &mcp_targets {
                let config_path = rt.mcp_config_path.as_ref().unwrap();
                match write_mcp_config(config_path, &root, args.yes)? {
                    McpConfigStatus::Written => {
                        mcp_configured = true;
                        push_unique_runtime(&mut agent_targets, rt);
                        println!(
                            "    {} {} → {}",
                            "✓".green().bold(),
                            rt.name,
                            config_path.display().to_string().dimmed()
                        );
                    }
                    McpConfigStatus::AlreadyConfigured => {
                        mcp_configured = true;
                        push_unique_runtime(&mut agent_targets, rt);
                        println!("    {} {} (already configured)", "·".dimmed(), rt.name,);
                    }
                    McpConfigStatus::Declined => {
                        println!("    {} {} (skipped)", "·".dimmed(), rt.name);
                    }
                }
            }
            println!();
        }
    }

    let hook_targets: Vec<&RuntimeInfo> = selected_runtimes
        .iter()
        .copied()
        .filter(|runtime| runtime.hook_config_path.is_some())
        .collect();
    if !hook_targets.is_empty() {
        println!("  {}", "Installing Claude hooks:".bold());
        for rt in &hook_targets {
            let config_path = rt.hook_config_path.as_ref().unwrap();
            match write_claude_hook_config(config_path, &root, args.yes)? {
                McpConfigStatus::Written => {
                    hook_configured = true;
                    any_hooks_configured = true;
                    println!(
                        "    {} {} hooks → {}",
                        "✓".green().bold(),
                        rt.name,
                        config_path.display().to_string().dimmed()
                    );
                }
                McpConfigStatus::AlreadyConfigured => {
                    hook_configured = true;
                    any_hooks_configured = true;
                    println!(
                        "    {} {} hooks (already configured)",
                        "·".dimmed(),
                        rt.name
                    );
                }
                McpConfigStatus::Declined => {
                    println!("    {} {} hooks (skipped)", "·".dimmed(), rt.name);
                }
            }
        }
        if matches!(
            write_hook_runtime_config(&root, any_hooks_configured)?,
            McpConfigStatus::Written
        ) {
            println!(
                "    {} Packet28 hook runtime → {}",
                "✓".green().bold(),
                packet28_daemon_core::hook_runtime_config_path(&root)
                    .display()
                    .to_string()
                    .dimmed()
            );
        }
        println!();
    }

    for rt in selected_runtimes
        .iter()
        .copied()
        .filter(|r| args.fallback_only || r.mcp_config_path.is_none())
    {
        push_unique_runtime(&mut agent_targets, rt);
    }

    // Phase 2: Agent fallback files
    println!("  {}", "Generating agent instruction files:".bold());
    let root_str = if root_display == "." {
        None
    } else {
        Some(root_display.as_str())
    };

    for rt in &agent_targets {
        let content = agent_surface::render_prompt_fragment(rt.agent_format, root_str);
        let path = &rt.agent_file_path;

        let wrote = write_agent_file(path, &content)?;
        agent_files_ready = true;
        if wrote {
            println!(
                "    {} {} → {}",
                "✓".green().bold(),
                rt.name,
                path.display().to_string().dimmed()
            );
        } else {
            println!("    {} {} (already up to date)", "·".dimmed(), rt.name,);
        }
    }

    // Write a generic fallback only when no runtime-specific target was selected.
    if agent_targets.is_empty() {
        if selected_runtimes.is_empty() {
            let generic_path = root.join("agent.md");
            let content = agent_surface::render_prompt_fragment(
                agent_surface::AgentPromptFormat::Agents,
                root_str,
            );
            let wrote = write_agent_file(&generic_path, &content)?;
            agent_files_ready = true;
            if wrote {
                println!(
                    "    {} {} → {}",
                    "✓".green().bold(),
                    "generic",
                    generic_path.display().to_string().dimmed()
                );
            } else {
                println!("    {} {} (already up to date)", "·".dimmed(), "generic");
            }
        } else {
            println!("    {} no runtime instruction files selected", "·".dimmed());
        }
    }
    println!();

    // Phase 3: Verify daemon
    println!("  {}", "Verifying daemon:".bold());
    match crate::cmd_daemon::ensure_daemon(&root) {
        Ok(_) => {
            println!("    {} daemon running", "✓".green().bold());
            println!("  {}", "Preparing repo index:".bold());
            match crate::cmd_daemon::send_request(
                &root,
                &DaemonRequest::DaemonIndexRebuild {
                    request: DaemonIndexRebuildRequest {
                        root: root.display().to_string(),
                        full: true,
                        paths: Vec::new(),
                    },
                },
            ) {
                Ok(DaemonResponse::DaemonIndexRebuild { .. }) => {
                    match crate::cmd_daemon::send_request(
                        &root,
                        &DaemonRequest::DaemonIndexStatus {
                            request: DaemonIndexStatusRequest {
                                root: root.display().to_string(),
                            },
                        },
                    ) {
                        Ok(DaemonResponse::DaemonIndexStatus { response }) => {
                            println!(
                                "    {} index {} (generation={}, ready={})",
                                "✓".green().bold(),
                                response.manifest.status,
                                response.manifest.generation,
                                response.ready
                            );
                        }
                        Ok(other) => {
                            println!(
                                "    {} unexpected index status response: {other:?}",
                                "·".dimmed()
                            );
                        }
                        Err(err) => {
                            println!("    {} index status unavailable: {}", "·".dimmed(), err);
                        }
                    }
                }
                Ok(other) => {
                    println!(
                        "    {} unexpected index rebuild response: {other:?}",
                        "·".dimmed()
                    );
                }
                Err(err) => {
                    println!("    {} failed to queue index build: {}", "·".dimmed(), err);
                }
            }
        }
        Err(e) => {
            println!("    {} daemon failed to start: {}", "✗".red().bold(), e);
            println!(
                "    {} run `packet28 daemon start --root {}` manually",
                "hint:".cyan().bold(),
                root_display
            );
        }
    }
    println!();

    // Done
    println!("  {}", "Setup complete!".green().bold());
    println!();
    println!("  {}", "Quick start:".bold());

    if mcp_configured && !args.fallback_only {
        println!("    Your agent runtimes are configured to use Packet28 control-plane MCP tools.");
        if hook_configured {
            println!("    Claude hooks will capture tool activity directly into Packet28.");
        }
        println!("    Start a new session and Packet28 intent/handoff tools will be available.");
    } else if agent_files_ready {
        println!("    Agent instruction files have been written.");
        println!("    Include them in your agent's context or system prompt.");
    } else {
        println!("    No runtime artifacts were written.");
        println!("    Re-run setup and select a runtime to configure.");
    }

    println!();
    println!("  {}", "Verify with:".dimmed());
    println!("    packet28 --version");
    println!("    packet28 daemon status --root {root_display}");
    println!("    packet28 doctor --root {root_display}");
    println!();

    Ok(0)
}

fn detect_runtimes(root: &Path) -> Vec<RuntimeInfo> {
    let home = dirs_home();
    vec![
        RuntimeInfo {
            name: "Claude Code",
            slug: "claude",
            mcp_config_path: find_claude_mcp_config(&home, root),
            hook_config_path: Some(root.join(".claude").join("settings.json")),
            agent_file_path: root.join("CLAUDE.md"),
            agent_format: agent_surface::AgentPromptFormat::Claude,
            detected: detect_claude(&home),
        },
        RuntimeInfo {
            name: "Cursor",
            slug: "cursor",
            mcp_config_path: find_cursor_mcp_config(root),
            hook_config_path: None,
            agent_file_path: root.join(".cursorrules"),
            agent_format: agent_surface::AgentPromptFormat::Cursor,
            detected: detect_cursor(&home),
        },
        RuntimeInfo {
            name: "Codex",
            slug: "codex",
            mcp_config_path: None, // Codex doesn't have MCP config yet
            hook_config_path: None,
            agent_file_path: root.join("AGENTS.md"),
            agent_format: agent_surface::AgentPromptFormat::Agents,
            detected: detect_codex(),
        },
    ]
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn detect_claude(home: &Path) -> bool {
    // Claude Code: ~/.claude/ directory or `claude` on PATH
    home.join(".claude").is_dir() || which_exists("claude")
}

fn detect_cursor(home: &Path) -> bool {
    // Cursor: ~/.cursor/ directory or cursor on PATH
    home.join(".cursor").is_dir() || which_exists("cursor")
}

fn detect_codex() -> bool {
    which_exists("codex")
}

fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn find_claude_mcp_config(_home: &Path, root: &Path) -> Option<PathBuf> {
    // Claude Code uses project-level .mcp.json
    Some(root.join(".mcp.json"))
}

fn find_cursor_mcp_config(root: &Path) -> Option<PathBuf> {
    // Cursor uses project-level .cursor/mcp.json
    Some(root.join(".cursor").join("mcp.json"))
}

fn select_setup_runtimes<'a>(
    runtimes: &'a [RuntimeInfo],
    explicit_runtime_selection: bool,
) -> Vec<&'a RuntimeInfo> {
    if explicit_runtime_selection {
        return runtimes.iter().collect();
    }
    runtimes.iter().filter(|runtime| runtime.detected).collect()
}

fn push_unique_runtime<'a>(targets: &mut Vec<&'a RuntimeInfo>, runtime: &'a RuntimeInfo) {
    if targets
        .iter()
        .any(|candidate| candidate.slug == runtime.slug)
    {
        return;
    }
    targets.push(runtime);
}

fn write_mcp_config(path: &Path, root: &Path, auto_yes: bool) -> Result<McpConfigStatus> {
    let root_arg = if root == Path::new(".") {
        ".".to_string()
    } else {
        root.display().to_string()
    };

    let command = resolve_packet28_mcp_command();
    let packet28_entry = json!({
        "command": command,
        "args": ["--root", root_arg]
    });

    // Read existing config or start fresh
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

    // Check if packet28 is already configured
    let servers = config
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));

    if !auto_yes {
        eprint!(
            "    Write MCP config to {}? [Y/n] ",
            path.display().to_string().dimmed()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if !trimmed.is_empty() && trimmed != "y" && trimmed != "yes" {
            return Ok(McpConfigStatus::Declined);
        }
    }

    // Insert packet28 server
    if let Some(obj) = servers.as_object_mut() {
        let needs_write = obj.get("packet28") != Some(&packet28_entry);
        if !needs_write {
            return Ok(McpConfigStatus::AlreadyConfigured);
        }
        obj.insert("packet28".to_string(), packet28_entry);
    }

    // Write back
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&config)?;
    fs::write(path, format!("{content}\n"))?;

    Ok(McpConfigStatus::Written)
}

fn write_claude_hook_config(path: &Path, root: &Path, auto_yes: bool) -> Result<McpConfigStatus> {
    let command = resolve_packet28_cli_command();
    let root_arg = shell_escape(root.display().to_string());
    let hook_command = format!("{command} hook claude --root \"{root_arg}\"");
    let packet28_hooks = json!({
        "SessionStart": [{
            "matcher": "startup|resume|clear|compact",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "UserPromptSubmit": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "PreToolUse": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "PostToolUse": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "Stop": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "SubagentStop": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "PreCompact": [{
            "matcher": "manual|auto",
            "hooks": [{"type": "command", "command": hook_command}]
        }],
        "SessionEnd": [{
            "matcher": ".*",
            "hooks": [{"type": "command", "command": hook_command}]
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
    let mut hooks = config
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if hooks.get("packet28") == Some(&packet28_hooks) {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    hooks.insert("packet28".to_string(), packet28_hooks);
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

fn write_hook_runtime_config(root: &Path, any_hooks_configured: bool) -> Result<McpConfigStatus> {
    if !any_hooks_configured {
        return Ok(McpConfigStatus::Declined);
    }
    let path = packet28_daemon_core::hook_runtime_config_path(root);
    if path.exists() {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let config = packet28_daemon_core::HookRuntimeConfig::default();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(McpConfigStatus::Written)
}

fn shell_escape(value: String) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

fn resolve_packet28_mcp_command() -> String {
    let output = std::process::Command::new("which")
        .arg("packet28-mcp")
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !command.is_empty() {
                return command;
            }
        }
    }
    "packet28-mcp".to_string()
}

fn resolve_packet28_cli_command() -> String {
    for candidate in ["Packet28", "packet28"] {
        let output = std::process::Command::new("which").arg(candidate).output();
        if let Ok(output) = output {
            if output.status.success() {
                let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !command.is_empty() {
                    return command;
                }
            }
        }
    }
    "Packet28".to_string()
}

fn write_agent_file(path: &Path, content: &str) -> Result<bool> {
    // If file exists, check if it already contains Packet28 guidance
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if existing.contains("packet28.write_intention")
            || existing.contains("packet28.prepare_handoff")
            || existing.contains("Packet28 mcp serve")
            || existing.contains("hook claude")
        {
            return Ok(false); // already has Packet28 instructions
        }

        // Append to existing file
        let separator = if existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        fs::write(path, format!("{existing}{separator}{content}\n"))?;
        return Ok(true);
    }

    // Write new file
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{content}\n"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn runtime(
        name: &'static str,
        slug: &'static str,
        detected: bool,
        has_mcp: bool,
    ) -> RuntimeInfo {
        RuntimeInfo {
            name,
            slug,
            mcp_config_path: has_mcp.then(|| PathBuf::from(format!("{slug}.json"))),
            hook_config_path: (slug == "claude").then(|| PathBuf::from(".claude/settings.json")),
            agent_file_path: PathBuf::from(format!("{slug}.md")),
            agent_format: agent_surface::AgentPromptFormat::Agents,
            detected,
        }
    }

    #[test]
    fn select_setup_runtimes_prefers_detected_runtimes_for_all() {
        let runtimes = vec![
            runtime("Claude Code", "claude", false, true),
            runtime("Cursor", "cursor", false, true),
            runtime("Codex", "codex", true, false),
        ];

        let selected = select_setup_runtimes(&runtimes, false);
        let slugs: Vec<&str> = selected.iter().map(|runtime| runtime.slug).collect();

        assert_eq!(slugs, vec!["codex"]);
    }

    #[test]
    fn select_setup_runtimes_keeps_explicit_runtime_requests() {
        let runtimes = vec![runtime("Claude Code", "claude", false, true)];

        let selected = select_setup_runtimes(&runtimes, true);
        let slugs: Vec<&str> = selected.iter().map(|runtime| runtime.slug).collect();

        assert_eq!(slugs, vec!["claude"]);
    }

    #[test]
    fn write_claude_hook_config_installs_packet28_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".claude").join("settings.json");
        let status = write_claude_hook_config(&path, dir.path(), true).unwrap();
        assert!(matches!(status, McpConfigStatus::Written));
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["hooks"]["packet28"]["SessionStart"].is_array());
        assert!(value["hooks"]["packet28"]["PostToolUse"].is_array());
    }
}
