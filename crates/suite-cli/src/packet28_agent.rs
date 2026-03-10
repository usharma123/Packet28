use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use packet28_daemon_core::{
    task_brief_json_path, task_brief_markdown_path, task_state_json_path, BrokerAction,
    BrokerGetContextRequest,
};

#[derive(Debug, Parser)]
#[command(
    name = "packet28-agent",
    version,
    about = "Run Packet28 preflight before delegating to an agent runtime",
    trailing_var_arg = true,
    after_help = "Example:\n  packet28-agent --task \"investigate flaky parser test\" -- codex exec \"review the failure\""
)]
pub struct Packet28AgentCli {
    /// Natural-language task description to send to Packet28 preflight
    #[arg(long)]
    pub task: String,

    /// Root path for repo-aware preflight reducers
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Optional task identifier for recall scoping
    #[arg(long)]
    pub task_id: Option<String>,

    /// Preflight JSON profile to persist for the delegated agent
    #[arg(long, value_enum, default_value_t = crate::agent_surface::DEFAULT_PREFLIGHT_PROFILE)]
    pub json: crate::cmd_common::JsonProfileArg,

    /// Delegated agent command. Pass it after `--` so wrapper flags do not leak into the child.
    #[arg(allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn main_entry() {
    let cli = Packet28AgentCli::parse();
    if cli.command.is_empty() {
        let mut command = Packet28AgentCli::command();
        let _ = command.print_help();
        eprintln!();
        std::process::exit(2);
    }

    match run(cli) {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => {
            crate::display_error(&err);
            std::process::exit(2);
        }
    }
}

pub fn run(cli: Packet28AgentCli) -> Result<i32> {
    let root = resolve_root_arg(&cli.root)?;
    let preflight_path = crate::agent_surface::latest_preflight_path(&root);
    let preflight_parent = preflight_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid preflight output path"))?;
    fs::create_dir_all(preflight_parent).with_context(|| {
        format!(
            "failed to create Packet28 agent directory '{}'",
            preflight_parent.display()
        )
    })?;

    let task_id = cli
        .task_id
        .clone()
        .unwrap_or_else(|| crate::broker_client::derive_task_id(&cli.task));
    let broker_response = crate::broker_client::get_context(
        &root,
        BrokerGetContextRequest {
            task_id: task_id.clone(),
            action: Some(BrokerAction::Plan),
            budget_tokens: Some(5_000),
            budget_bytes: Some(32_000),
            since_version: None,
            focus_paths: Vec::new(),
            focus_symbols: Vec::new(),
            tool_name: None,
            tool_result_kind: None,
            query: Some(cli.task.clone()),
            include_sections: Vec::new(),
            exclude_sections: Vec::new(),
            verbosity: None,
            response_mode: None,
            include_self_context: false,
            max_sections: None,
            default_max_items_per_section: None,
            section_item_limits: std::collections::BTreeMap::new(),
            persist_artifacts: None,
        },
    )?;
    fs::write(&preflight_path, serde_json::to_vec(&broker_response)?).with_context(|| {
        format!(
            "failed to persist broker payload to '{}'",
            preflight_path.display()
        )
    })?;
    let brief_json_path = task_brief_json_path(&root, &task_id);
    let brief_md_path = task_brief_markdown_path(&root, &task_id);
    let state_json_path = task_state_json_path(&root, &task_id);
    let proxy_config = std::env::var_os("PACKET28_MCP_UPSTREAM_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            let candidate = root.join(".mcp.proxy.json");
            candidate.exists().then_some(candidate)
        });
    let proxy_command = proxy_config.as_ref().map(|config| {
        format!(
            "Packet28 mcp proxy --root {} --upstream-config {} --task-id {}",
            root.display(),
            config.display(),
            task_id
        )
    });
    let mcp_command = proxy_command
        .clone()
        .unwrap_or_else(|| format!("Packet28 mcp serve --root {}", root.display()));

    let mut child = Command::new(&cli.command[0]);
    child
        .args(&cli.command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("PACKET28_PREFLIGHT_PATH", &preflight_path)
        .env("PACKET28_TASK_ID", &task_id)
        .env(
            "PACKET28_BROKER_CONTEXT_VERSION",
            &broker_response.context_version,
        )
        .env(
            "PACKET28_BROKER_BUDGET_REMAINING_TOKENS",
            broker_response.budget_remaining_tokens.to_string(),
        )
        .env("PACKET28_BROKER_BRIEF_PATH", &brief_md_path)
        .env("PACKET28_BROKER_BRIEF_JSON_PATH", &brief_json_path)
        .env("PACKET28_BROKER_STATE_PATH", &state_json_path)
        .env("PACKET28_BROKER_SUPPORTS_PUSH", "1")
        .env("PACKET28_BROKER_ESTIMATE_TOOL", "packet28.estimate_context")
        .env("PACKET28_BROKER_GET_CONTEXT_TOOL", "packet28.get_context")
        .env(
            "PACKET28_BROKER_VALIDATE_PLAN_TOOL",
            "packet28.validate_plan",
        )
        .env("PACKET28_BROKER_DECOMPOSE_TOOL", "packet28.decompose")
        .env("PACKET28_BROKER_RESPONSE_MODE", "auto")
        .env("PACKET28_BROKER_POLL_FIELD", "since_version")
        .env("PACKET28_BROKER_WINDOW_MODE", "replace")
        .env("PACKET28_BROKER_SUPERSESSION", "1")
        .env("PACKET28_BROKER_SECTION_CACHE_KEY", "sections_by_id")
        .env("PACKET28_BROKER_REPLACE_PACKET28_CONTEXT", "1")
        .env(
            "PACKET28_MCP_NOTIFICATION_METHOD",
            "notifications/packet28.context_updated",
        )
        .env("PACKET28_MCP_COMMAND", mcp_command)
        .env("PACKET28_MCP_PROXY_TASK_ID", &task_id)
        .env(
            "PACKET28_MCP_PROXY_COMMAND",
            proxy_command.unwrap_or_default(),
        )
        .env("PACKET28_ROOT", &root);
    let status = child
        .status()
        .with_context(|| format!("failed to execute delegated command '{}'", cli.command[0]))?;
    Ok(status.code().unwrap_or(1))
}

fn resolve_root_arg(root: &str) -> Result<PathBuf> {
    let cwd = crate::cmd_common::caller_cwd()?;
    Ok(PathBuf::from(crate::cmd_common::resolve_path_from_cwd(
        root, &cwd,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_parser_keeps_child_args_after_separator() {
        let cli = Packet28AgentCli::try_parse_from([
            "packet28-agent",
            "--task",
            "investigate parser regression",
            "--",
            "codex",
            "--model",
            "gpt-5",
        ])
        .unwrap();
        assert_eq!(cli.command, vec!["codex", "--model", "gpt-5"]);
        assert_eq!(cli.json, crate::cmd_common::JsonProfileArg::Compact);
    }

    #[test]
    fn derived_task_id_is_stable() {
        let task_id = crate::broker_client::derive_task_id("investigate parser regression");
        assert!(task_id.starts_with("task-"));
    }
}
