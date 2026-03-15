use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser};
use packet28_daemon_core::{
    task_brief_json_path, task_brief_markdown_path, task_state_json_path, BrokerGetContextResponse,
    BrokerPrepareHandoffRequest, BrokerResponseMode, BrokerSupersessionMode,
    TaskAwaitHandoffRequest,
};

const BOOTSTRAP_MODE_FRESH: &str = "fresh";
const BOOTSTRAP_MODE_HANDOFF: &str = "handoff";

#[derive(Debug, Parser)]
#[command(
    name = "packet28-agent",
    version,
    about = "Run Packet28 checkpointed handoff bootstrap before delegating to an agent runtime",
    trailing_var_arg = true,
    after_help = "Examples:\n  packet28-agent --task-id task-auth-broker --wait-for-handoff -- codex exec \"continue the task\"\n  packet28-agent --task \"continue auth broker\" --wait-for-handoff -- codex exec \"continue the task\""
)]
pub struct Packet28AgentCli {
    /// Optional query to steer handoff assembly when resuming from an existing task.
    #[arg(long)]
    pub task: Option<String>,

    /// Root path for repo-aware handoff artifacts
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Optional task identifier for recall scoping
    #[arg(long)]
    pub task_id: Option<String>,

    /// Wait for a task handoff to become ready before launching the delegated agent.
    #[arg(long, default_value_t = false)]
    pub wait_for_handoff: bool,

    /// Maximum seconds to wait for handoff readiness when `--wait-for-handoff` is enabled.
    #[arg(long, default_value_t = 300)]
    pub handoff_timeout_secs: u64,

    /// Poll interval in milliseconds for handoff readiness checks.
    #[arg(long, default_value_t = 250)]
    pub handoff_poll_ms: u64,

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
    let bootstrap_path = crate::agent_surface::latest_bootstrap_path(&root);
    let handoff_path = crate::agent_surface::latest_handoff_path(&root);
    let bootstrap_parent = bootstrap_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid bootstrap output path"))?;
    fs::create_dir_all(bootstrap_parent).with_context(|| {
        format!(
            "failed to create Packet28 agent directory '{}'",
            bootstrap_parent.display()
        )
    })?;

    let bootstrap = prepare_bootstrap(&root, &cli, &bootstrap_path, &handoff_path)?;
    fs::write(&bootstrap_path, serde_json::to_vec(&bootstrap.response)?).with_context(|| {
        format!(
            "failed to persist bootstrap payload to '{}'",
            bootstrap_path.display()
        )
    })?;
    let brief_json_path = task_brief_json_path(&root, &bootstrap.task_id);
    let brief_md_path = task_brief_markdown_path(&root, &bootstrap.task_id);
    let state_json_path = task_state_json_path(&root, &bootstrap.task_id);
    if let Some(parent) = brief_md_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Packet28 task artifact directory '{}'",
                parent.display()
            )
        })?;
    }
    fs::write(&brief_md_path, &bootstrap.response.brief).with_context(|| {
        format!(
            "failed to persist broker brief to '{}'",
            brief_md_path.display()
        )
    })?;
    // Reuse the serialized bootstrap payload instead of encoding the same response twice.
    if brief_json_path != bootstrap_path {
        let _ = fs::remove_file(&brief_json_path);
        fs::hard_link(&bootstrap_path, &brief_json_path)
            .or_else(|_| fs::copy(&bootstrap_path, &brief_json_path).map(|_| ()))
            .with_context(|| {
                format!(
                    "failed to persist broker brief json to '{}'",
                    brief_json_path.display()
                )
            })?;
    }
    fs::write(
        &state_json_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "task_id": &bootstrap.task_id,
            "context_version": &bootstrap.response.context_version,
            "latest_brief_path": brief_md_path,
            "brief_json_path": brief_json_path,
            "supports_push": true,
        }))?,
    )
    .with_context(|| {
        format!(
            "failed to persist broker state json to '{}'",
            state_json_path.display()
        )
    })?;
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
            bootstrap.task_id
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
        .env("PACKET28_BOOTSTRAP_MODE", bootstrap.mode)
        .env("PACKET28_BOOTSTRAP_PATH", &bootstrap.bootstrap_path)
        .env("PACKET28_TASK_ID", &bootstrap.task_id)
        .env(
            "PACKET28_BROKER_CONTEXT_VERSION",
            &bootstrap.response.context_version,
        )
        .env(
            "PACKET28_BROKER_BUDGET_REMAINING_TOKENS",
            bootstrap.response.budget_remaining_tokens.to_string(),
        )
        .env("PACKET28_BROKER_BRIEF_PATH", &brief_md_path)
        .env("PACKET28_BROKER_BRIEF_JSON_PATH", &brief_json_path)
        .env("PACKET28_BROKER_STATE_PATH", &state_json_path)
        .env("PACKET28_BROKER_SUPPORTS_PUSH", "1")
        .env(
            "PACKET28_BROKER_PREPARE_HANDOFF_TOOL",
            "packet28.prepare_handoff",
        )
        .env("PACKET28_BROKER_WINDOW_MODE", "replace")
        .env("PACKET28_BROKER_SUPERSESSION", "1")
        .env("PACKET28_BROKER_SECTION_CACHE_KEY", "sections_by_id")
        .env("PACKET28_BROKER_REPLACE_PACKET28_CONTEXT", "1")
        .env(
            "PACKET28_HANDOFF_PATH",
            bootstrap.handoff_path.unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_ARTIFACT_ID",
            bootstrap.handoff_artifact_id.unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_CHECKPOINT_ID",
            bootstrap.handoff_checkpoint_id.unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_REASON",
            bootstrap.handoff_reason.unwrap_or_default(),
        )
        .env(
            "PACKET28_MCP_NOTIFICATION_METHOD",
            "notifications/packet28.context_updated",
        )
        .env("PACKET28_MCP_COMMAND", mcp_command)
        .env("PACKET28_MCP_PROXY_TASK_ID", &bootstrap.task_id)
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

struct BootstrapContext {
    mode: &'static str,
    task_id: String,
    response: BrokerGetContextResponse,
    bootstrap_path: PathBuf,
    handoff_path: Option<String>,
    handoff_artifact_id: Option<String>,
    handoff_checkpoint_id: Option<String>,
    handoff_reason: Option<String>,
}

fn prepare_bootstrap(
    root: &std::path::Path,
    cli: &Packet28AgentCli,
    bootstrap_path: &std::path::Path,
    handoff_path: &std::path::Path,
) -> Result<BootstrapContext> {
    let task_id = cli
        .task_id
        .clone()
        .or_else(|| {
            cli.task
                .as_ref()
                .map(|task| crate::broker_client::derive_task_id(task))
        })
        .ok_or_else(|| {
            anyhow!(
                "packet28-agent requires a checkpointed task via --task-id or a derivable --task"
            )
        })?;
    crate::task_runtime::store_active_task(
        root,
        &packet28_daemon_core::ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: None,
            updated_at_unix: packet28_daemon_core::now_unix(),
        },
    )?;
    if cli.wait_for_handoff {
        let task_id = maybe_wait_for_handoff(root, cli, task_id)?;
        return prepare_handoff_bootstrap(
            root,
            task_id,
            cli.task.clone(),
            bootstrap_path,
            handoff_path,
        );
    }
    Ok(prepare_fresh_bootstrap(task_id, bootstrap_path))
}

fn prepare_fresh_bootstrap(task_id: String, bootstrap_path: &std::path::Path) -> BootstrapContext {
    let response = BrokerGetContextResponse {
        context_version: format!("fresh-{}", packet28_daemon_core::now_unix()),
        response_mode: BrokerResponseMode::Full,
        artifact_id: None,
        latest_intention: None,
        next_action_summary: None,
        handoff_ready: false,
        stale: false,
        brief: "Packet28 fresh session bootstrap.\n- Claude hooks will capture reducer packets automatically.\n- Use packet28.write_intention when the objective changes materially.\n- Prepare handoff only after threshold or stop boundaries.".to_string(),
        supersedes_prior_context: true,
        supersession_mode: BrokerSupersessionMode::Replace,
        superseded_before_version: String::new(),
        sections: Vec::new(),
        est_tokens: 0,
        est_bytes: 0,
        budget_remaining_tokens: 0,
        budget_remaining_bytes: 0,
        section_estimates: Vec::new(),
        eviction_candidates: Vec::new(),
        delta: Default::default(),
        working_set: Vec::new(),
        recommended_actions: Vec::new(),
        active_decisions: Vec::new(),
        open_questions: Vec::new(),
        resolved_questions: Vec::new(),
        changed_paths_since_checkpoint: Vec::new(),
        changed_symbols_since_checkpoint: Vec::new(),
        recent_tool_invocations: Vec::new(),
        tool_failures: Vec::new(),
        discovered_paths: Vec::new(),
        discovered_symbols: Vec::new(),
        evidence_artifact_ids: Vec::new(),
        invalidates_since_version: false,
        effective_max_sections: 0,
        effective_default_max_items_per_section: 0,
        effective_section_item_limits: Default::default(),
        diagnostics_ms: Default::default(),
    };
    BootstrapContext {
        mode: BOOTSTRAP_MODE_FRESH,
        task_id,
        response,
        bootstrap_path: bootstrap_path.to_path_buf(),
        handoff_path: None,
        handoff_artifact_id: None,
        handoff_checkpoint_id: None,
        handoff_reason: None,
    }
}

fn prepare_handoff_bootstrap(
    root: &std::path::Path,
    task_id: String,
    query: Option<String>,
    bootstrap_path: &std::path::Path,
    handoff_path: &std::path::Path,
) -> Result<BootstrapContext> {
    let handoff = crate::broker_client::prepare_handoff(
        root,
        BrokerPrepareHandoffRequest {
            task_id: task_id.clone(),
            query,
            response_mode: Some(packet28_daemon_core::BrokerResponseMode::Full),
        },
    )?;
    if !handoff.handoff_ready {
        return Err(anyhow!(
            "Packet28 handoff is not ready for task '{}': {}",
            task_id,
            handoff.handoff_reason
        ));
    }
    let response = handoff.context.ok_or_else(|| {
        anyhow!(
            "Packet28 returned a ready handoff for task '{}' without context payload",
            task_id
        )
    })?;
    fs::write(handoff_path, serde_json::to_vec(&response)?).with_context(|| {
        format!(
            "failed to persist handoff broker payload to '{}'",
            handoff_path.display()
        )
    })?;
    Ok(BootstrapContext {
        mode: BOOTSTRAP_MODE_HANDOFF,
        task_id,
        response,
        bootstrap_path: bootstrap_path.to_path_buf(),
        handoff_path: Some(handoff_path.to_string_lossy().to_string()),
        handoff_artifact_id: handoff.latest_handoff_artifact_id,
        handoff_checkpoint_id: handoff.latest_handoff_checkpoint_id,
        handoff_reason: Some(handoff.handoff_reason),
    })
}

fn maybe_wait_for_handoff(
    root: &std::path::Path,
    cli: &Packet28AgentCli,
    task_id: String,
) -> Result<String> {
    if !cli.wait_for_handoff {
        return Ok(task_id);
    }
    let after_context_version = crate::broker_client::task_status(root, &task_id)?
        .task
        .and_then(|task| {
            task.latest_agent_bootstrap_mode
                .as_deref()
                .filter(|mode| *mode == BOOTSTRAP_MODE_HANDOFF)
                .and(task.latest_agent_context_version.clone())
        });
    crate::broker_client::await_handoff(
        root,
        TaskAwaitHandoffRequest {
            task_id: task_id.clone(),
            timeout_ms: Some(cli.handoff_timeout_secs.saturating_mul(1_000)),
            poll_ms: Some(cli.handoff_poll_ms),
            after_context_version,
        },
    )?;
    Ok(task_id)
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
        assert!(!cli.wait_for_handoff);
    }

    #[test]
    fn derived_task_id_is_stable() {
        let task_id = crate::broker_client::derive_task_id("investigate parser regression");
        assert!(task_id.starts_with("task-"));
    }

    #[test]
    fn wrapper_parser_accepts_task_id_without_task_text() {
        let cli = Packet28AgentCli::try_parse_from([
            "packet28-agent",
            "--task-id",
            "task-123",
            "--",
            "codex",
            "exec",
        ])
        .unwrap();
        assert_eq!(cli.task_id.as_deref(), Some("task-123"));
        assert!(cli.task.is_none());
    }

    #[test]
    fn wrapper_parser_accepts_task_text() {
        let cli = Packet28AgentCli::try_parse_from([
            "packet28-agent",
            "--task",
            "continue auth broker",
            "--",
            "codex",
            "exec",
        ])
        .unwrap();
        assert_eq!(cli.task.as_deref(), Some("continue auth broker"));
    }

    #[test]
    fn wrapper_parser_accepts_wait_for_handoff() {
        let cli = Packet28AgentCli::try_parse_from([
            "packet28-agent",
            "--wait-for-handoff",
            "--handoff-timeout-secs",
            "42",
            "--handoff-poll-ms",
            "50",
            "--task-id",
            "task-123",
            "--",
            "codex",
            "exec",
        ])
        .unwrap();
        assert!(cli.wait_for_handoff);
        assert_eq!(cli.handoff_timeout_secs, 42);
        assert_eq!(cli.handoff_poll_ms, 50);
    }
}
