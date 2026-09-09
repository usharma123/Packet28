use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
#[cfg(test)]
use packet28_daemon_protocol::hooks::RelaunchPreference;
use packet28_daemon_protocol::index::DaemonIndexRebuildRequest;
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse};
use packet28_state_fs::StateDir;
use serde_json::{json, Value};
use toml::value::Table as TomlTable;

use crate::agent_surface;
#[cfg(test)]
use crate::cmd_setup_index::classify_setup_index_status;
use crate::cmd_setup_index::{
    render_setup_index_progress, verify_setup_index, SetupIndexVerification,
};
use crate::cmd_setup_render::{
    format_runtime_detection_badge, format_setup_badge, render_setup_banner,
    render_setup_detection_overview, render_setup_intro, render_setup_menu_hint,
    render_setup_menu_option, render_setup_section, render_setup_step, runtime_capability_summary,
    setup_menu_index_badge, SetupBadgeStyle,
};
#[cfg(test)]
use crate::cmd_setup_runtime::PromptTarget;
use crate::cmd_setup_runtime::{
    adapters, detect_runtimes, prompt_target_label, push_prompt_targets, select_setup_runtimes,
    RuntimeEnvironment, RuntimeInfo,
};

#[path = "cmd_setup_commands.rs"]
pub(crate) mod setup_commands;
use setup_commands::resolve_packet28_mcp_command;
#[cfg(test)]
use setup_commands::{
    apply_generated_relaunch_command, guarded_packet28_hook_command, resolve_packet28_cli_command,
    shell_escape,
};
#[path = "cmd_setup_hooks.rs"]
pub(crate) mod setup_hooks;
use setup_hooks::write_hook_runtime_config;
#[path = "cmd_setup_plugins.rs"]
pub(crate) mod setup_plugins;

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

    /// Specific supported runtime slug to configure, or `all` for detected runtimes
    #[arg(long, default_value = "all")]
    pub runtime: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpConfigStatus {
    Written,
    AlreadyConfigured,
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupMode {
    Recommended,
    Custom,
    GuidanceOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupRuntimeScope {
    Detected,
    All,
    Single(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetupPlanChoice {
    pub(crate) mode: SetupMode,
    pub(crate) runtime_scope: SetupRuntimeScope,
    pub(crate) fallback_only: bool,
}

/// `.gitignore` entry for Packet28's always-ephemeral daemon/index state.
const PACKET28_GITIGNORE_ENTRY: &str = ".packet28/";

/// Returns true when `content` already ignores Packet28's `.packet28/` runtime
/// directory in any of the common equivalent spellings.
fn gitignore_covers_packet28_dir(content: &str) -> bool {
    content.lines().map(str::trim_end).any(|line| {
        matches!(
            line,
            ".packet28"
                | ".packet28/"
                | "/.packet28"
                | "/.packet28/"
                | ".packet28/*"
                | ".packet28/**"
        )
    })
}

/// Ensures the repository `.gitignore` ignores Packet28's `.packet28/` runtime
/// directory.
///
/// The daemon writes runtime and index state under `.packet28/` continuously.
/// If that directory is tracked, the working tree is perpetually dirty, which
/// blocks the full regex index from ever publishing (it requires a clean Git
/// working tree). Ignoring it once breaks that cycle so the index can build.
///
/// This only manages Packet28's own ephemeral directory. User instruction files
/// and MCP config are never auto-ignored. It runs only inside a Git repository,
/// is idempotent, and preserves any existing `.gitignore` content. Returns the
/// path when it added the entry, or `None` when no change was needed.
fn ensure_packet28_gitignore(root: &Path) -> Result<Option<PathBuf>> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let path = root.join(".gitignore");
    let directory = StateDir::open(root, &[], false)
        .with_context(|| format!("failed to open repository root '{}'", root.display()))?;
    let existing = directory
        .read_bounded(".gitignore", u64::MAX)
        .with_context(|| format!("failed to read '{}'", path.display()))?
        .map(String::from_utf8)
        .transpose()
        .with_context(|| format!("failed to read '{}' as UTF-8", path.display()))?
        .unwrap_or_default();
    if gitignore_covers_packet28_dir(&existing) {
        return Ok(None);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str("# Packet28 daemon runtime and index state\n");
    updated.push_str(PACKET28_GITIGNORE_ENTRY);
    updated.push('\n');
    directory
        .write_atomic(".gitignore", updated.as_bytes())
        .with_context(|| format!("failed to update '{}'", path.display()))?;
    Ok(Some(path))
}

pub fn run(args: SetupArgs) -> Result<i32> {
    let root = crate::cmd_daemon::resolve_root_arg(&args.root);
    let root_display = root.display().to_string();

    // Detect runtimes
    let runtime_environment = RuntimeEnvironment::from_process(&root);
    let runtimes = detect_runtimes(&runtime_environment);
    let setup_choice = match resolve_setup_choice(&args, &runtimes)? {
        Some(choice) => choice,
        None => {
            println!("  {}", "Setup canceled.".yellow().bold());
            println!();
            return Ok(0);
        }
    };
    let selected_runtimes = select_setup_runtimes(&runtimes, &setup_choice);
    let auto_apply = render_setup_intro(
        &root_display,
        &runtimes,
        &selected_runtimes,
        &setup_choice,
        args.yes,
    )?;
    if !auto_apply {
        println!("  {}", "Setup canceled.".yellow().bold());
        println!();
        return Ok(0);
    }

    let mut prompt_targets = Vec::new();
    let mut mcp_configured = false;
    let mut hook_configured = false;
    let mut hook_service_configured = false;
    let mut any_hook_runtime_configs_written = false;
    let mut agent_files_ready = false;
    let mut exit_code = 0;

    // Ignore Packet28's own runtime directory before the daemon starts writing
    // into it. Otherwise a tracked `.packet28/` keeps the working tree dirty and
    // the full regex index can never publish (it requires a clean tree).
    if let Some(path) = ensure_packet28_gitignore(&root)? {
        println!(
            "  {} ignored {} in {}",
            "✓".green().bold(),
            PACKET28_GITIGNORE_ENTRY,
            path.display().to_string().dimmed()
        );
        println!(
            "  {} commit the updated .gitignore so daemon writes stop dirtying the tree",
            "hint:".cyan().bold()
        );
        println!();
    }

    render_setup_step(
        1,
        4,
        "Configure Runtime Integrations",
        "Wire up MCP and hooks for the runtimes you selected.",
    );
    if !setup_choice.fallback_only {
        if !selected_runtimes
            .iter()
            .copied()
            .any(|runtime| runtime.adapter.mcp.is_some())
        {
            println!(
                "  {} No MCP-capable runtimes selected. Falling back to instruction files.",
                format_setup_badge("note", SetupBadgeStyle::Warning)
            );
            println!();
        } else {
            render_setup_section("MCP servers");
            for rt in &selected_runtimes {
                let Some(mcp) = rt.adapter.mcp else {
                    continue;
                };
                match mcp.configure(&runtime_environment, auto_apply)? {
                    McpConfigStatus::Written => {
                        mcp_configured = true;
                        push_prompt_targets(&mut prompt_targets, &rt.prompt_targets);
                        println!(
                            "    {} {}",
                            "✓".green().bold(),
                            mcp.status(&runtime_environment).dimmed()
                        );
                    }
                    McpConfigStatus::AlreadyConfigured => {
                        mcp_configured = true;
                        push_prompt_targets(&mut prompt_targets, &rt.prompt_targets);
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

    if selected_runtimes
        .iter()
        .copied()
        .any(|runtime| runtime.adapter.hooks.is_some())
    {
        render_setup_section("Runtime hooks");
        for rt in &selected_runtimes {
            let Some(hooks) = rt.adapter.hooks else {
                continue;
            };
            match hooks.configure(&runtime_environment, auto_apply)? {
                McpConfigStatus::Written => {
                    hook_configured = true;
                    hook_service_configured |= rt.adapter.writes_hook_runtime_config;
                    if rt.adapter.writes_hook_runtime_config {
                        any_hook_runtime_configs_written = true;
                    }
                    println!(
                        "    {} {} hooks → {}",
                        "✓".green().bold(),
                        rt.name,
                        hooks.status(&runtime_environment).dimmed()
                    );
                }
                McpConfigStatus::AlreadyConfigured => {
                    hook_configured = true;
                    hook_service_configured |= rt.adapter.writes_hook_runtime_config;
                    if rt.adapter.writes_hook_runtime_config {
                        any_hook_runtime_configs_written = true;
                    }
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
            write_hook_runtime_config(&root, any_hook_runtime_configs_written)?,
            McpConfigStatus::Written
        ) {
            println!(
                "    {} Packet28 hook runtime → {}",
                "✓".green().bold(),
                packet28_daemon_protocol::paths::hook_runtime_config_path(&root)
                    .display()
                    .to_string()
                    .dimmed()
            );
        }
        if any_hook_runtime_configs_written {
            match crate::cmd_daemon::restart_daemon(&root) {
                Ok(_) => {
                    println!(
                        "    {} restarted packet28d for hook compatibility",
                        "✓".green().bold()
                    );
                }
                Err(err) => {
                    println!(
                        "    {} could not restart packet28d automatically: {}",
                        "·".dimmed(),
                        err
                    );
                }
            }
        }
        if hook_service_configured {
            crate::cmd_hook_http::ensure_hook_http_server_for_root(&root)
                .context("failed to start the runtime HTTP hook service during setup")?;
            println!(
                "    {} Runtime HTTP hook service is healthy",
                "✓".green().bold()
            );
        }
        println!();
    }

    for rt in selected_runtimes
        .iter()
        .copied()
        .filter(|runtime| setup_choice.fallback_only || runtime.adapter.mcp.is_none())
    {
        push_prompt_targets(&mut prompt_targets, &rt.prompt_targets);
    }

    render_setup_step(
        2,
        4,
        "Write Instruction Files",
        "Refresh the prompt fragments each runtime reads from this repo.",
    );
    // These files live inside the configured workspace and are consumed with
    // that workspace as the runtime cwd. Keep their examples relative so a
    // clone, rename, or move does not preserve the installer's absolute path.
    let root_str = None;

    for target in &prompt_targets {
        let content = agent_surface::render_prompt_fragment(target.format, root_str);
        let path = &target.path;

        let wrote = write_agent_file(path, &content)?;
        agent_files_ready = true;
        if wrote {
            println!(
                "    {} {} → {}",
                "✓".green().bold(),
                prompt_target_label(target),
                path.display().to_string().dimmed()
            );
        } else {
            println!(
                "    {} {} (already up to date)",
                "·".dimmed(),
                prompt_target_label(target),
            );
        }
    }

    // Write a generic fallback only when no runtime-specific target was selected.
    if prompt_targets.is_empty() {
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
                    "    {} generic → {}",
                    "✓".green().bold(),
                    generic_path.display().to_string().dimmed()
                );
            } else {
                println!("    {} generic (already up to date)", "·".dimmed());
            }
        } else {
            println!("    {} no runtime instruction files selected", "·".dimmed());
        }
    }
    println!();

    render_setup_step(
        3,
        4,
        "Start Daemon And Build Indexes",
        "Bring packet28d online, then verify both indexes are healthy.",
    );
    render_setup_section("Daemon");
    match crate::cmd_daemon::ensure_daemon(&root) {
        Ok(_) => {
            println!("    {} daemon running", "✓".green().bold());
            render_setup_section("Repo index");
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
                Ok(DaemonResponse::DaemonIndexRebuild { response }) => {
                    if response.accepted {
                        println!("    {} rebuild queued", "✓".green().bold());
                    } else {
                        println!("    {} rebuild request was not accepted", "✗".red().bold());
                        exit_code = 1;
                    }
                    match verify_setup_index(&root)? {
                        SetupIndexVerification::Ready(response) => {
                            println!(
                                "    {} index ready (generation={}, regex_generation={}, files={}, regex_files={})",
                                "✓".green().bold(),
                                response.manifest.generation,
                                response.manifest.regex_generation.unwrap_or_default(),
                                response.manifest.indexed_files,
                                response.manifest.regex_indexed_files
                            );
                        }
                        SetupIndexVerification::Building(response) => {
                            println!(
                                "    {} index still building ({})",
                                "·".yellow().bold(),
                                render_setup_index_progress(&response)
                            );
                        }
                        SetupIndexVerification::Deferred => {
                            println!(
                                "    {} index deferred: commit or stash workspace changes, including the setup files, then run `Packet28 daemon index rebuild --root {}`",
                                "·".yellow().bold(),
                                root_display
                            );
                        }
                        SetupIndexVerification::Failed { response, reason } => {
                            if let Some(response) = response {
                                println!(
                                    "    {} index failed: {} (status={}, regex_status={}, generation={}, regex_generation={})",
                                    "✗".red().bold(),
                                    reason,
                                    response.manifest.status,
                                    response.manifest.regex_status.as_deref().unwrap_or("missing"),
                                    response.manifest.generation,
                                    response.manifest.regex_generation.unwrap_or_default()
                                );
                            } else {
                                println!("    {} index failed: {}", "✗".red().bold(), reason);
                            }
                            println!(
                                "    {} run `Packet28 daemon index rebuild --root {}` and inspect `Packet28 daemon index status --root {} --json`",
                                "hint:".cyan().bold(),
                                root_display,
                                root_display
                            );
                            exit_code = 1;
                        }
                    }
                }
                Ok(other) => {
                    println!(
                        "    {} unexpected index rebuild response: {other:?}",
                        "·".dimmed()
                    );
                    exit_code = 1;
                }
                Err(err) => {
                    println!(
                        "    {} failed to queue index build: {}",
                        "✗".red().bold(),
                        err
                    );
                    exit_code = 1;
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
            exit_code = 1;
        }
    }
    println!();

    render_setup_step(
        4,
        4,
        "Summary",
        "A quick recap of what changed and how to verify the install.",
    );
    if exit_code == 0 {
        println!("    {}", "Setup complete.".green().bold());
    } else {
        println!("    {}", "Setup finished with errors.".red().bold());
    }
    println!();
    render_setup_section("What changed");

    if mcp_configured && !setup_choice.fallback_only {
        println!("    Your agent runtimes are configured to use Packet28 control-plane MCP tools.");
        if hook_configured {
            println!(
                "    Selected runtime hooks will capture tool activity directly into Packet28."
            );
        }
        println!("    Reducer-runner cache safety is enabled with workspace fingerprints.");
        println!("    Start a new session and Packet28 intent/handoff tools will be available.");
    } else if agent_files_ready {
        println!("    Agent instruction files have been written.");
        println!("    Include them in your agent's context or system prompt.");
    } else {
        println!("    No runtime artifacts were written.");
        println!("    Re-run setup and select a runtime to configure.");
    }

    println!();
    render_setup_section("Verify with");
    println!("    packet28 --version");
    println!("    packet28 daemon status --root {root_display}");
    println!("    packet28 doctor --root {root_display}");
    println!("    packet28-mcp --root {root_display}  # then call packet28.agent_status");
    println!();

    Ok(exit_code)
}

fn resolve_setup_choice(
    args: &SetupArgs,
    runtimes: &[RuntimeInfo],
) -> Result<Option<SetupPlanChoice>> {
    if args.yes || has_explicit_setup_overrides(args) {
        return explicit_setup_choice(args, runtimes).map(Some);
    }
    prompt_setup_choice(runtimes)
}

fn has_explicit_setup_overrides(args: &SetupArgs) -> bool {
    args.fallback_only || args.runtime != "all"
}

fn explicit_setup_choice(args: &SetupArgs, runtimes: &[RuntimeInfo]) -> Result<SetupPlanChoice> {
    let runtime_scope = if args.runtime == "all" {
        SetupRuntimeScope::Detected
    } else if runtimes.iter().any(|runtime| runtime.slug == args.runtime) {
        SetupRuntimeScope::Single(args.runtime.clone())
    } else {
        anyhow::bail!(
            "unknown runtime '{}'. Use: {}, or all",
            args.runtime,
            supported_runtime_slugs().join(", ")
        );
    };
    let mode = if args.fallback_only {
        SetupMode::GuidanceOnly
    } else if args.runtime == "all" {
        SetupMode::Recommended
    } else {
        SetupMode::Custom
    };
    Ok(SetupPlanChoice {
        mode,
        runtime_scope,
        fallback_only: args.fallback_only,
    })
}

fn supported_runtime_slugs() -> Vec<&'static str> {
    adapters().iter().map(|adapter| adapter.slug).collect()
}

fn prompt_setup_choice(runtimes: &[RuntimeInfo]) -> Result<Option<SetupPlanChoice>> {
    render_setup_banner(
        "Packet28 Setup Wizard",
        "Choose how much Packet28 should configure for this workspace.",
    );
    render_setup_detection_overview(runtimes);
    render_setup_menu_option(
        1,
        "Recommended",
        "Detected runtimes",
        "Configure the runtimes Packet28 found and keep the rest untouched.",
        true,
    );
    render_setup_menu_option(
        2,
        "Advanced",
        "Choose scope",
        "Pick detected, all supported, or a single runtime before anything is written.",
        false,
    );
    render_setup_menu_option(
        3,
        "Guidance only",
        "Instruction files only",
        "Skip MCP integration and only write the prompt files needed for manual setup.",
        false,
    );
    render_setup_menu_hint();
    println!();

    let selection = prompt_menu_selection("  Select a setup path [1]: ", 3, 1)?;
    let Some(selection) = selection else {
        return Ok(None);
    };

    let choice = match selection {
        1 => SetupPlanChoice {
            mode: SetupMode::Recommended,
            runtime_scope: SetupRuntimeScope::Detected,
            fallback_only: false,
        },
        2 => match prompt_advanced_setup_choice(runtimes)? {
            Some(choice) => choice,
            None => return Ok(None),
        },
        3 => SetupPlanChoice {
            mode: SetupMode::GuidanceOnly,
            runtime_scope: SetupRuntimeScope::Detected,
            fallback_only: true,
        },
        _ => return Ok(None),
    };
    Ok(Some(choice))
}

fn prompt_advanced_setup_choice(runtimes: &[RuntimeInfo]) -> Result<Option<SetupPlanChoice>> {
    render_setup_banner(
        "Advanced Runtime Scope",
        "Choose exactly which runtimes Packet28 should target.",
    );
    render_setup_detection_overview(runtimes);
    render_setup_menu_option(
        1,
        "Detected runtimes",
        "Low risk",
        "Only touch runtimes that already appear to be installed on this machine.",
        true,
    );
    render_setup_menu_option(
        2,
        "All supported runtimes",
        "Broader coverage",
        "Write setup for every supported runtime so this repo is ready across editors.",
        false,
    );
    render_setup_menu_option(
        3,
        "Single runtime",
        "Most precise",
        "Target one runtime explicitly and leave every other integration alone.",
        false,
    );
    render_setup_menu_hint();
    println!();

    let selection = prompt_menu_selection("  Select a runtime scope [1]: ", 3, 1)?;
    let Some(selection) = selection else {
        return Ok(None);
    };

    let runtime_scope = match selection {
        1 => SetupRuntimeScope::Detected,
        2 => SetupRuntimeScope::All,
        3 => prompt_single_runtime_scope(runtimes)?,
        _ => return Ok(None),
    };

    Ok(Some(SetupPlanChoice {
        mode: SetupMode::Custom,
        runtime_scope,
        fallback_only: false,
    }))
}

fn prompt_single_runtime_scope(runtimes: &[RuntimeInfo]) -> Result<SetupRuntimeScope> {
    render_setup_banner(
        "Choose A Runtime",
        "Pick the single runtime Packet28 should configure in this workspace.",
    );
    for (idx, runtime) in runtimes.iter().enumerate() {
        let availability = format_runtime_detection_badge(runtime.detected);
        let capability = runtime_capability_summary(runtime);
        println!(
            "    {} {}",
            setup_menu_index_badge(idx + 1),
            runtime.name.bold()
        );
        println!("        {}  {}", availability, capability.dimmed());
        println!();
    }
    render_setup_menu_hint();
    println!();

    let selection =
        prompt_menu_selection("  Select a runtime [1]: ", runtimes.len(), 1)?.unwrap_or(1);
    Ok(SetupRuntimeScope::Single(
        runtimes
            .get(selection - 1)
            .map(|runtime| runtime.slug.to_string())
            .unwrap_or_else(|| runtimes[0].slug.to_string()),
    ))
}

fn prompt_menu_selection(prompt: &str, max: usize, default: usize) -> Result<Option<usize>> {
    loop {
        eprint!("{prompt}");
        io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if trimmed == "q" || trimmed == "quit" || trimmed == "cancel" {
            println!();
            return Ok(None);
        }
        if trimmed.is_empty() {
            println!();
            return Ok(Some(default));
        }
        if let Ok(value) = trimmed.parse::<usize>() {
            if (1..=max).contains(&value) {
                println!();
                return Ok(Some(value));
            }
        }
        println!(
            "  {} Choose 1-{max}, press Enter for {default}, or type 'q' to cancel.",
            "hint:".cyan().bold()
        );
    }
}

pub(crate) fn write_mcp_config(
    path: &Path,
    _root: &Path,
    auto_yes: bool,
) -> Result<McpConfigStatus> {
    write_mcp_config_with_root_arg(path, ".", auto_yes, None)
}

pub(crate) fn write_user_mcp_config_with_label(
    path: &Path,
    root: &Path,
    auto_yes: bool,
    runtime_name: Option<&str>,
) -> Result<McpConfigStatus> {
    write_mcp_config_with_root_arg(path, &root.display().to_string(), auto_yes, runtime_name)
}

fn write_mcp_config_with_root_arg(
    path: &Path,
    root_arg: &str,
    auto_yes: bool,
    runtime_name: Option<&str>,
) -> Result<McpConfigStatus> {
    let command = resolve_packet28_mcp_command();
    let packet28_entry = json!({
        "command": command,
        "args": ["--root", root_arg, "--toolset", "core"]
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
    let servers = servers.as_object_mut().with_context(|| {
        format!(
            "refusing to overwrite 'mcpServers' in '{}'; expected a JSON object",
            path.display()
        )
    })?;

    if !auto_yes {
        let target = runtime_name
            .map(|name| format!("{name} MCP config"))
            .unwrap_or_else(|| "MCP config".to_string());
        eprint!(
            "    Write {target} to {}? [Y/n] ",
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
    let needs_write = servers.get("packet28") != Some(&packet28_entry);
    if !needs_write {
        return Ok(McpConfigStatus::AlreadyConfigured);
    }
    servers.insert("packet28".to_string(), packet28_entry);

    // Write back
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&config)?;
    fs::write(path, format!("{content}\n"))?;

    Ok(McpConfigStatus::Written)
}

pub(crate) fn read_toml_config(path: &Path) -> Result<toml::Value> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    toml::from_str(&content).with_context(|| {
        format!(
            "refusing to overwrite invalid TOML in '{}'; fix the file and rerun setup",
            path.display()
        )
    })
}

pub(crate) fn read_toml_config_or_default(path: &Path) -> Result<toml::Value> {
    if path.exists() {
        read_toml_config(path)
    } else {
        Ok(toml::Value::Table(TomlTable::new()))
    }
}

pub(crate) fn toml_table_entry<'a>(
    table: &'a mut TomlTable,
    key: &str,
    path: &Path,
) -> Result<&'a mut TomlTable> {
    let value = table
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Table(TomlTable::new()));
    value.as_table_mut().with_context(|| {
        format!(
            "refusing to overwrite '{}' in '{}'; expected a TOML table",
            key,
            path.display()
        )
    })
}

pub(crate) fn write_toml_config(path: &Path, config: &toml::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rendered = toml::to_string_pretty(config)?;
    fs::write(path, format!("{rendered}\n"))?;
    Ok(())
}

fn write_agent_file(path: &Path, content: &str) -> Result<bool> {
    // If file exists, check if it already contains Packet28 guidance
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if agent_surface::contains_packet28_guidance(&existing) {
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
mod tests;
