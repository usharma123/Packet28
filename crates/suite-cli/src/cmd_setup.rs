use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
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
    agent_file_path: PathBuf,
    agent_format: agent_surface::AgentPromptFormat,
    detected: bool,
}

pub fn run(args: SetupArgs) -> Result<i32> {
    let root = crate::cmd_daemon::resolve_root_arg(&args.root);
    let root_display = root.display().to_string();

    println!();
    println!(
        "{}",
        "  Packet28 Setup  ".bold().white().on_bright_blue()
    );
    println!();
    println!(
        "  Workspace: {}",
        root_display.cyan()
    );
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

    let any_detected = runtimes.iter().any(|r| r.detected);

    // Phase 1: MCP config
    if !args.fallback_only {
        let mcp_targets: Vec<&RuntimeInfo> = runtimes
            .iter()
            .filter(|r| r.detected && r.mcp_config_path.is_some())
            .collect();

        if mcp_targets.is_empty() {
            println!(
                "  {} No MCP-capable runtimes detected. Generating fallback files.",
                "→".yellow()
            );
            println!();
        } else {
            println!("  {}", "Configuring MCP servers:".bold());
            for rt in &mcp_targets {
                let config_path = rt.mcp_config_path.as_ref().unwrap();
                let wrote = write_mcp_config(config_path, &root, args.yes)?;
                if wrote {
                    println!(
                        "    {} {} → {}",
                        "✓".green().bold(),
                        rt.name,
                        config_path.display().to_string().dimmed()
                    );
                } else {
                    println!(
                        "    {} {} (already configured)",
                        "·".dimmed(),
                        rt.name,
                    );
                }
            }
            println!();
        }
    }

    // Phase 2: Agent fallback files
    println!("  {}", "Generating agent instruction files:".bold());
    let root_str = if root_display == "." {
        None
    } else {
        Some(root_display.as_str())
    };

    for rt in &runtimes {
        let content = agent_surface::render_prompt_fragment(rt.agent_format, root_str);
        let path = &rt.agent_file_path;

        let wrote = write_agent_file(path, &content)?;
        if wrote {
            println!(
                "    {} {} → {}",
                "✓".green().bold(),
                rt.name,
                path.display().to_string().dimmed()
            );
        } else {
            println!(
                "    {} {} (already up to date)",
                "·".dimmed(),
                rt.name,
            );
        }
    }

    // Also write a generic agent.md if no specific runtime detected
    if !any_detected {
        let generic_path = root.join("agent.md");
        let content = agent_surface::render_prompt_fragment(
            agent_surface::AgentPromptFormat::Agents,
            root_str,
        );
        let wrote = write_agent_file(&generic_path, &content)?;
        if wrote {
            println!(
                "    {} {} → {}",
                "✓".green().bold(),
                "generic",
                generic_path.display().to_string().dimmed()
            );
        }
    }
    println!();

    // Phase 3: Verify daemon
    println!("  {}", "Verifying daemon:".bold());
    match crate::cmd_daemon::ensure_daemon(&root) {
        Ok(_) => {
            println!(
                "    {} daemon running",
                "✓".green().bold()
            );
        }
        Err(e) => {
            println!(
                "    {} daemon failed to start: {}",
                "✗".red().bold(),
                e
            );
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

    if any_detected && !args.fallback_only {
        println!("    Your agent runtimes are configured to use Packet28 via MCP.");
        println!("    Start a new session and Packet28 context tools will be available.");
    } else {
        println!("    Agent instruction files have been written.");
        println!(
            "    Include them in your agent's context or system prompt."
        );
    }

    println!();
    println!("  {}", "Verify with:".dimmed());
    println!("    packet28 --version");
    println!("    packet28 daemon status --root {root_display}");
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
            agent_file_path: root.join("CLAUDE.md"),
            agent_format: agent_surface::AgentPromptFormat::Claude,
            detected: detect_claude(&home),
        },
        RuntimeInfo {
            name: "Cursor",
            slug: "cursor",
            mcp_config_path: find_cursor_mcp_config(root),
            agent_file_path: root.join(".cursorrules"),
            agent_format: agent_surface::AgentPromptFormat::Cursor,
            detected: detect_cursor(&home),
        },
        RuntimeInfo {
            name: "Codex",
            slug: "codex",
            mcp_config_path: None, // Codex doesn't have MCP config yet
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

fn write_mcp_config(path: &Path, root: &Path, auto_yes: bool) -> Result<bool> {
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
        serde_json::from_str(&content).unwrap_or_default()
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
            return Ok(false);
        }
    }

    // Insert packet28 server
    if let Some(obj) = servers.as_object_mut() {
        let needs_write = obj.get("packet28") != Some(&packet28_entry);
        if !needs_write {
            return Ok(false);
        }
        obj.insert("packet28".to_string(), packet28_entry);
    }

    // Write back
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&config)?;
    fs::write(path, format!("{content}\n"))?;

    Ok(true)
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

fn write_agent_file(path: &Path, content: &str) -> Result<bool> {
    // If file exists, check if it already contains Packet28 guidance
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if existing.contains("packet28.get_context") || existing.contains("Packet28 mcp serve") {
            return Ok(false); // already has Packet28 instructions
        }

        // Append to existing file
        let separator = if existing.ends_with('\n') { "\n" } else { "\n\n" };
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
