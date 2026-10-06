use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::storage::now_unix;
use packet28_daemon_core::task_store_lease::acquire_task_store_writer_lease;
use packet28_daemon_protocol::broker::{BrokerAction, BrokerGetContextRequest};
use packet28_daemon_protocol::hooks::{
    ActiveTaskRecord, HookBoundaryKind, HookEventKind, HookIngestRequest, HookRuntimeConfig,
};
use packet28_daemon_protocol::paths::hook_runtime_config_path;
#[cfg(test)]
use packet28_reducer_core::classify_command;
use serde_json::{json, Value};

use crate::cmd_hook_http::{ensure_hook_http_server, run_hook_http_server};
use crate::cmd_hook_packets::build_reducer_packet;
#[cfg(test)]
use crate::cmd_hook_packets::{build_bash_packet, build_grep_packet, build_read_packet};
use crate::cmd_hook_runner::{run_reduce_fixture, run_reducer_runner};
#[path = "cmd_hook_runtime.rs"]
mod hook_runtime;
pub(crate) use crate::cmd_hook_support::{
    compact_text, estimate_text_tokens, now_unix_millis, payload_text_len, reduction_pct,
    shell_join,
};
use crate::cmd_hook_support::{hook_output_text, json_nested_string, json_string};
use crate::cmd_wakeup::build_wakeup_pack_for_injection;
use crate::memory_store::{
    append_transcript_message, enqueue_pending_extraction, hook_event_stats, list_hook_events,
    record_hook_event, HookEventInput, PendingExtractionInput, TranscriptAppendInput,
};
use hook_runtime::{
    build_runtime_reducer_packet, external_runtime_name, parse_runtime_event_kind,
    render_runtime_hook_output, runtime_command, runtime_matcher, runtime_session_id,
    runtime_source, ExternalHookRuntime,
};

#[cfg(test)]
use crate::cmd_hook_runner::workspace_cache_fingerprint;

#[derive(Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub command: HookCommands,
}

#[derive(Subcommand)]
pub enum HookCommands {
    Claude(ClaudeHookArgs),
    Codex(ClaudeHookArgs),
    Copilot(RuntimeHookArgs),
    Cursor(RuntimeHookArgs),
    Gemini(RuntimeHookArgs),
    Windsurf(RuntimeHookArgs),
    Log(HookLogArgs),
    Stats(HookStatsArgs),
    ServeHttp(HookHttpServerArgs),
    ReducerRunner(ReducerRunnerArgs),
    ReduceFixture(ReduceFixtureArgs),
    Rewrite(HookRewriteArgs),
}

#[derive(Args, Clone)]
pub struct ClaudeHookArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub event: Option<String>,
}

#[derive(Args, Clone)]
pub struct RuntimeHookArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub event: Option<String>,
}

#[derive(Args, Clone)]
pub struct HookLogArgs {
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args, Clone)]
pub struct HookStatsArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args, Clone)]
pub struct HookRewriteArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[command(subcommand)]
    pub command: HookRewriteCommand,
}

#[derive(Subcommand, Clone)]
pub enum HookRewriteCommand {
    /// Unsupported: automatic rewrites change native host permission matching.
    On,
    /// Clear the legacy rewrite flag; result capture stays enabled.
    Off,
    /// Report capture-only behavior and the stored legacy rewrite flag.
    Status(HookRewriteStatusArgs),
}

#[derive(Args, Clone)]
pub struct HookRewriteStatusArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct HookHttpServerArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub port: u16,
    #[arg(long)]
    pub token: String,
}

#[derive(Args, Clone)]
pub struct ReducerRunnerArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub family: String,
    #[arg(long)]
    pub kind: String,
    #[arg(long)]
    pub fingerprint: String,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    #[arg(trailing_var_arg = true)]
    pub argv: Vec<String>,
}

#[derive(Args, Clone)]
pub struct ReduceFixtureArgs {
    #[arg(long)]
    pub command: String,
    #[arg(long)]
    pub stdout_path: String,
    #[arg(long)]
    pub stderr_path: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub exit_code: i32,
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: HookArgs) -> Result<i32> {
    match args.command {
        HookCommands::Claude(args) => run_claude(args),
        HookCommands::Codex(args) => run_claude(args),
        HookCommands::Copilot(args) => run_runtime_hook(args, ExternalHookRuntime::Copilot),
        HookCommands::Cursor(args) => run_runtime_hook(args, ExternalHookRuntime::Cursor),
        HookCommands::Gemini(args) => run_runtime_hook(args, ExternalHookRuntime::Gemini),
        HookCommands::Windsurf(args) => run_runtime_hook(args, ExternalHookRuntime::Windsurf),
        HookCommands::Log(args) => run_hook_log(args),
        HookCommands::Stats(args) => run_hook_stats(args),
        HookCommands::ServeHttp(args) => run_hook_http_server(args),
        HookCommands::ReducerRunner(args) => run_reducer_runner(args),
        HookCommands::ReduceFixture(args) => run_reduce_fixture(args),
        HookCommands::Rewrite(args) => run_hook_rewrite(args),
    }
}

pub(crate) struct ClaudeHookOutcome {
    pub(crate) exit_code: i32,
    pub(crate) body: Option<String>,
}

struct RuntimeHookOutcome {
    exit_code: i32,
    body: Option<String>,
}

fn run_claude(args: ClaudeHookArgs) -> Result<i32> {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;
    let payload = if buffer.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_str(&buffer) {
            Ok(payload) => payload,
            Err(err) => {
                eprintln!("packet28 hook: ignoring malformed JSON payload: {err}");
                return Ok(0);
            }
        }
    };
    let root = resolve_hook_root(&args, &payload);
    match process_claude_hook_payload(&root, args.event.as_deref(), &payload, true) {
        Ok(outcome) => {
            if let Some(body) = outcome.body {
                println!("{body}");
            }
            Ok(outcome.exit_code)
        }
        Err(err) => {
            eprintln!("packet28 hook: allowing runtime action after processing error: {err:#}");
            Ok(0)
        }
    }
}

pub(crate) fn process_claude_hook_payload(
    root: &Path,
    event_override: Option<&str>,
    payload: &Value,
    bootstrap_http_server: bool,
) -> Result<ClaudeHookOutcome> {
    let runtime_config = load_hook_runtime_config(root)?;
    let event_kind = event_override
        .map(|value| parse_event_kind(Some(value)))
        .unwrap_or_else(|| parse_event_kind(json_string(payload, "hook_event_name").as_deref()));

    if !runtime_config.hooks_enabled {
        // Kill switch: never start the HTTP hook server or the daemon when
        // hooks are disabled for this workspace (`Packet28 uninstall`).
        return Ok(ClaudeHookOutcome {
            exit_code: 0,
            body: None,
        });
    }
    if bootstrap_http_server
        && matches!(
            event_kind,
            HookEventKind::SessionStart | HookEventKind::UserPromptSubmit
        )
    {
        ensure_hook_http_server(root, &runtime_config)?;
    }
    crate::broker_client::ensure_daemon(root)?;
    let _writer_lease = acquire_task_store_writer_lease(root)?;

    let session_id = json_string(payload, "session_id");
    let task_id = resolve_task_id(root, payload, session_id.as_deref())?;
    let matcher = json_string(payload, "matcher");
    let source = json_string(payload, "source");
    let reducer_packet = build_reducer_packet(&runtime_config, payload, event_kind);
    let response = crate::broker_client::hook_ingest(
        root,
        HookIngestRequest {
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            event_kind,
            matcher: matcher.clone(),
            source,
            boundary_kind: boundary_for_event(event_kind),
            lifecycle_event: None,
            reducer_packet,
            host_context_budget_tokens: std::env::var("PACKET28_HOST_CONTEXT_BUDGET_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok()),
        },
    )?;
    record_hook_event(HookEventInput {
        runtime: "claude",
        event_kind: hook_event_name(event_kind),
        session_id: session_id.as_deref(),
        task_id: Some(&task_id),
        matcher: matcher.as_deref(),
        payload_json: &serde_json::to_string(payload)?,
    })?;
    let _ = capture_hook_output("claude", root, event_kind, payload, session_id.as_deref());
    let additional_context = build_session_start_additional_context(
        root,
        event_kind,
        payload,
        response.additional_context.as_deref(),
    )?;
    let action_critic = build_pretool_action_critic(root, event_kind, payload, &task_id)
        .unwrap_or_else(|_| Vec::new());
    Ok(ClaudeHookOutcome {
        exit_code: if response.block_stop { 2 } else { 0 },
        body: render_hook_output(event_kind, &response, additional_context, &action_critic)?,
    })
}

fn run_runtime_hook(args: RuntimeHookArgs, runtime: ExternalHookRuntime) -> Result<i32> {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;
    let payload = if buffer.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_str(&buffer) {
            Ok(payload) => payload,
            Err(err) => {
                eprintln!(
                    "packet28 {} hook: ignoring malformed JSON payload: {err}",
                    external_runtime_name(runtime)
                );
                return Ok(0);
            }
        }
    };
    match process_runtime_hook_payload(args, runtime, payload) {
        Ok(outcome) => {
            if let Some(body) = outcome.body {
                println!("{body}");
            }
            Ok(outcome.exit_code)
        }
        Err(err) => {
            eprintln!(
                "packet28 {} hook: allowing runtime action after processing error: {err:#}",
                external_runtime_name(runtime)
            );
            Ok(0)
        }
    }
}

fn process_runtime_hook_payload(
    args: RuntimeHookArgs,
    runtime: ExternalHookRuntime,
    payload: Value,
) -> Result<RuntimeHookOutcome> {
    let root = resolve_runtime_hook_root(&args, &payload);
    let runtime_config = load_hook_runtime_config(&root)?;
    if !runtime_config.hooks_enabled {
        return Ok(RuntimeHookOutcome {
            exit_code: 0,
            body: None,
        });
    }
    crate::broker_client::ensure_daemon(&root)?;
    let _writer_lease = acquire_task_store_writer_lease(&root)?;

    let event_kind = args
        .event
        .as_deref()
        .map(|value| parse_runtime_event_kind(runtime, Some(value)))
        .unwrap_or_else(|| {
            parse_runtime_event_kind(runtime, json_string(&payload, "hook_event_name").as_deref())
        });
    let session_id = runtime_session_id(runtime, &payload);
    let task_id = resolve_runtime_task_id(&root, &payload, session_id.as_deref(), runtime)?;
    let matcher = runtime_matcher(runtime, &payload, event_kind);
    let reducer_packet = build_runtime_reducer_packet(runtime, &payload, event_kind);

    let _response = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            event_kind,
            matcher: matcher.clone(),
            source: Some(runtime_source(runtime).to_string()),
            boundary_kind: boundary_for_event(event_kind),
            lifecycle_event: None,
            reducer_packet,
            host_context_budget_tokens: None,
        },
    )?;
    record_hook_event(HookEventInput {
        runtime: external_runtime_name(runtime),
        event_kind: hook_event_name(event_kind),
        session_id: session_id.as_deref(),
        task_id: Some(&task_id),
        matcher: matcher.as_deref(),
        payload_json: &serde_json::to_string(&payload)?,
    })?;
    let _ = capture_hook_output(
        external_runtime_name(runtime),
        &root,
        event_kind,
        &payload,
        session_id.as_deref(),
    );
    Ok(RuntimeHookOutcome {
        exit_code: 0,
        body: render_runtime_hook_output(runtime, event_kind)?,
    })
}

fn run_hook_log(args: HookLogArgs) -> Result<i32> {
    let events = list_hook_events(args.limit)?;
    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(events)?, args.pretty)?;
    } else {
        for event in events {
            println!(
                "id={} runtime={} event={} session={} task={} matcher={}",
                event.id,
                event.runtime,
                event.event_kind,
                event.session_id.unwrap_or_else(|| "n/a".to_string()),
                event.task_id.unwrap_or_else(|| "n/a".to_string()),
                event.matcher.unwrap_or_else(|| "n/a".to_string())
            );
        }
    }
    Ok(0)
}

fn run_hook_stats(args: HookStatsArgs) -> Result<i32> {
    let stats = hook_event_stats()?;
    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(stats)?, args.pretty)?;
    } else {
        for stat in stats {
            println!(
                "runtime={} event={} count={}",
                stat.runtime, stat.event_kind, stat.event_count
            );
        }
    }
    Ok(0)
}

fn capture_hook_output(
    runtime: &str,
    root: &Path,
    event_kind: HookEventKind,
    payload: &Value,
    session_id: Option<&str>,
) -> Result<()> {
    if !matches!(
        event_kind,
        HookEventKind::PostToolUse | HookEventKind::PostToolUseFailure
    ) {
        return Ok(());
    }
    let raw_output = hook_pending_raw_output(payload, event_kind);
    let raw_output = raw_output.trim();
    if raw_output.is_empty() {
        return Ok(());
    }
    let raw_output = compact_text(raw_output, 8 * 1024);
    enqueue_pending_extraction(PendingExtractionInput {
        project: Some(&hook_project_name(root, payload)),
        tool_name: hook_tool_name(runtime, payload).as_deref(),
        raw_output: &raw_output,
    })?;
    let fallback_session = json_string(payload, "task_id");
    let transcript_session = session_id.or(fallback_session.as_deref());
    let project = hook_project_name(root, payload);
    append_transcript_message(TranscriptAppendInput {
        session: transcript_session,
        agent: Some(runtime),
        role: Some("tool"),
        content: &raw_output,
        source: Some("packet28-hook"),
        project: Some(&project),
    })?;
    Ok(())
}

fn hook_pending_raw_output(payload: &Value, event_kind: HookEventKind) -> String {
    match event_kind {
        HookEventKind::PostToolUse => payload
            .get("tool_response")
            .map(hook_output_text)
            .or_else(|| json_string(payload, "output"))
            .or_else(|| json_string(payload, "stdout"))
            .or_else(|| json_string(payload, "result"))
            .unwrap_or_default(),
        HookEventKind::PostToolUseFailure => {
            json_string(payload, "error").unwrap_or_else(|| hook_output_text(payload))
        }
        _ => String::new(),
    }
}

fn hook_project_name(root: &Path, payload: &Value) -> String {
    json_string(payload, "project")
        .or_else(|| {
            json_string(payload, "cwd").and_then(|cwd| {
                Path::new(&cwd)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "project".to_string())
}

fn hook_tool_name(runtime: &str, payload: &Value) -> Option<String> {
    json_string(payload, "tool_name")
        .or_else(|| json_string(payload, "tool"))
        .or_else(|| json_string(payload, "name"))
        .or_else(|| json_string(payload, "command").map(|_| "Bash".to_string()))
        .or_else(|| Some(runtime.to_string()))
}

fn resolve_hook_root(args: &ClaudeHookArgs, payload: &Value) -> PathBuf {
    if args.root.trim() != "." {
        return crate::broker_client::resolve_root(&args.root);
    }
    json_string(payload, "cwd")
        .map(|cwd| crate::broker_client::resolve_root(&cwd))
        .unwrap_or_else(|| crate::broker_client::resolve_root("."))
}

// Claude Code and Codex hooks share this resolver. A task superseded by
// history recovery continues under its linked successor.
fn resolve_task_id(root: &Path, payload: &Value, session_id: Option<&str>) -> Result<String> {
    let task_id = select_task_id(root, payload, session_id)?;
    crate::task_runtime::adopt_task_continuation(root, task_id, session_id)
}

fn select_task_id(root: &Path, payload: &Value, session_id: Option<&str>) -> Result<String> {
    if let Some(task_id) = json_string(payload, "task_id").filter(|value| !value.trim().is_empty())
    {
        crate::task_runtime::store_active_task(
            root,
            &ActiveTaskRecord {
                task_id: task_id.clone(),
                session_id: session_id.map(ToOwned::to_owned),
                updated_at_unix: now_unix(),
            },
        )?;
        return Ok(task_id);
    }
    if let Some(active) = crate::task_runtime::load_active_task(root)? {
        if session_id.is_none() || active.session_id.as_deref() == session_id {
            return Ok(active.task_id);
        }
    }
    let task_id = session_id
        .map(crate::task_runtime::derive_claude_task_id)
        .unwrap_or_else(|| crate::broker_client::derive_task_id("claude-project"));
    crate::task_runtime::store_active_task(
        root,
        &ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: session_id.map(ToOwned::to_owned),
            updated_at_unix: now_unix(),
        },
    )?;
    Ok(task_id)
}

fn resolve_runtime_task_id(
    root: &Path,
    payload: &Value,
    session_id: Option<&str>,
    runtime: ExternalHookRuntime,
) -> Result<String> {
    let task_id = select_runtime_task_id(root, payload, session_id, runtime)?;
    crate::task_runtime::adopt_task_continuation(root, task_id, session_id)
}

fn select_runtime_task_id(
    root: &Path,
    payload: &Value,
    session_id: Option<&str>,
    runtime: ExternalHookRuntime,
) -> Result<String> {
    if let Some(task_id) = json_string(payload, "task_id").filter(|value| !value.trim().is_empty())
    {
        crate::task_runtime::store_active_task(
            root,
            &ActiveTaskRecord {
                task_id: task_id.clone(),
                session_id: session_id.map(ToOwned::to_owned),
                updated_at_unix: now_unix(),
            },
        )?;
        return Ok(task_id);
    }
    if let Some(active) = crate::task_runtime::load_active_task(root)? {
        return Ok(active.task_id);
    }
    let seed = match runtime {
        ExternalHookRuntime::Copilot => "copilot-project",
        ExternalHookRuntime::Cursor => "cursor-project",
        ExternalHookRuntime::Gemini => "gemini-project",
        ExternalHookRuntime::Windsurf => "windsurf-project",
    };
    let task_id = session_id
        .map(crate::task_runtime::derive_claude_task_id)
        .unwrap_or_else(|| crate::broker_client::derive_task_id(seed));
    crate::task_runtime::store_active_task(
        root,
        &ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: session_id.map(ToOwned::to_owned),
            updated_at_unix: now_unix(),
        },
    )?;
    Ok(task_id)
}

fn resolve_runtime_hook_root(args: &RuntimeHookArgs, payload: &Value) -> PathBuf {
    if args.root.trim() != "." {
        return crate::broker_client::resolve_root(&args.root);
    }
    if let Some(root) = json_string(payload, "cwd")
        .or_else(|| json_nested_string(payload, &["tool_info", "cwd"]))
        .or_else(|| json_nested_string(payload, &["workspace_root"]))
    {
        return crate::broker_client::resolve_root(&root);
    }
    if let Some(root) = payload
        .get("workspace_roots")
        .and_then(Value::as_array)
        .and_then(|roots| roots.first())
        .and_then(Value::as_str)
    {
        return crate::broker_client::resolve_root(root);
    }
    crate::broker_client::resolve_root(".")
}

fn parse_event_kind(value: Option<&str>) -> HookEventKind {
    match value.unwrap_or_default().trim() {
        "SessionStart" => HookEventKind::SessionStart,
        "UserPromptSubmit" => HookEventKind::UserPromptSubmit,
        "PreToolUse" => HookEventKind::PreToolUse,
        "PostToolUse" => HookEventKind::PostToolUse,
        "PostToolUseFailure" => HookEventKind::PostToolUseFailure,
        "CommandStarted" => HookEventKind::CommandStarted,
        "CommandProgress" => HookEventKind::CommandProgress,
        "CommandFinished" => HookEventKind::CommandFinished,
        "Stop" => HookEventKind::Stop,
        "SubagentStop" => HookEventKind::SubagentStop,
        "PreCompact" => HookEventKind::PreCompact,
        "SessionEnd" => HookEventKind::SessionEnd,
        _ => HookEventKind::Unknown,
    }
}

fn boundary_for_event(kind: HookEventKind) -> HookBoundaryKind {
    match kind {
        HookEventKind::Stop => HookBoundaryKind::Stop,
        HookEventKind::SubagentStop => HookBoundaryKind::SubagentStop,
        HookEventKind::PreCompact => HookBoundaryKind::PreCompact,
        HookEventKind::SessionEnd => HookBoundaryKind::SessionEnd,
        _ => HookBoundaryKind::None,
    }
}

fn hook_event_name(kind: HookEventKind) -> &'static str {
    match kind {
        HookEventKind::SessionStart => "session_start",
        HookEventKind::UserPromptSubmit => "user_prompt_submit",
        HookEventKind::PreToolUse => "pre_tool_use",
        HookEventKind::PostToolUse => "post_tool_use",
        HookEventKind::PostToolUseFailure => "post_tool_use_failure",
        HookEventKind::CommandStarted => "command_started",
        HookEventKind::CommandProgress => "command_progress",
        HookEventKind::CommandFinished => "command_finished",
        HookEventKind::Stop => "stop",
        HookEventKind::SubagentStop => "subagent_stop",
        HookEventKind::PreCompact => "pre_compact",
        HookEventKind::SessionEnd => "session_end",
        HookEventKind::Unknown => "unknown",
    }
}

fn render_hook_output(
    event_kind: HookEventKind,
    response: &packet28_daemon_protocol::hooks::HookIngestResponse,
    session_start_context: Option<String>,
    action_critic: &[String],
) -> Result<Option<String>> {
    match event_kind {
        HookEventKind::SessionStart => {
            if let Some(additional_context) = session_start_context {
                return Ok(Some(serde_json::to_string(&json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": additional_context,
                    }
                }))?));
            }
        }
        HookEventKind::PreToolUse => {
            if let Some(context) = render_action_critic_reason(action_critic) {
                return Ok(Some(serde_json::to_string(&json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "additionalContext": context,
                    }
                }))?));
            }
        }
        HookEventKind::Stop | HookEventKind::SubagentStop => {
            if response.relaunch_requested {
                // Daemon is handling relaunch — allow the stop to proceed.
                // The next session will bootstrap from the handoff artifact.
                eprintln!(
                    "packet28: context threshold reached, daemon relaunch queued (artifact={})",
                    response
                        .latest_handoff_artifact_id
                        .as_deref()
                        .unwrap_or("pending")
                );
            } else if response.block_stop {
                return Ok(Some(serde_json::to_string(&json!({
                    "decision": "block",
                    "reason": response.stop_reason.clone().unwrap_or_else(|| "Packet28 requires an intention before stop".to_string()),
                }))?));
            } else if let Some(stop_reason) = response.stop_reason.as_ref() {
                return Ok(Some(serde_json::to_string(&json!({
                    "systemMessage": stop_reason,
                }))?));
            }
        }
        _ => {}
    }
    Ok(None)
}

fn build_pretool_action_critic(
    root: &Path,
    event_kind: HookEventKind,
    payload: &Value,
    task_id: &str,
) -> Result<Vec<String>> {
    if !matches!(event_kind, HookEventKind::PreToolUse) {
        return Ok(Vec::new());
    }
    let Some(command) = runtime_command(payload).filter(|command| !command.trim().is_empty())
    else {
        return Ok(Vec::new());
    };
    let response = crate::broker_client::get_context(
        root,
        BrokerGetContextRequest {
            task_id: task_id.to_string(),
            action: Some(BrokerAction::ChooseTool),
            query: Some(command),
            tool_name: json_string(payload, "tool_name"),
            include_sections: vec!["action_critic".to_string()],
            max_sections: Some(1),
            default_max_items_per_section: Some(4),
            persist_artifacts: Some(false),
            ..BrokerGetContextRequest::default()
        },
    )?;
    Ok(response
        .sections
        .iter()
        .find(|section| section.id == "action_critic")
        .map(|section| {
            section
                .body
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| line.trim_start_matches("- ").to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default())
}

fn render_action_critic_reason(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let joined = lines
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!("Packet28 action critic: {joined}"))
}

fn build_session_start_additional_context(
    root: &Path,
    event_kind: HookEventKind,
    payload: &Value,
    daemon_context: Option<&str>,
) -> Result<Option<String>> {
    if !matches!(event_kind, HookEventKind::SessionStart) {
        return Ok(daemon_context.map(ToOwned::to_owned));
    }
    let project = hook_project_name(root, payload);
    let max_tokens = std::env::var("PACKET28_HOOK_WAKEUP_TOKENS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500);
    let wakeup = build_wakeup_pack_for_injection(None, Some(&project), 8, max_tokens)?;
    let context = match (daemon_context, wakeup) {
        (Some(daemon), Some(wakeup)) if !daemon.trim().is_empty() => {
            format!("{daemon}\n\n{wakeup}")
        }
        (Some(daemon), _) if !daemon.trim().is_empty() => daemon.to_string(),
        (_, Some(wakeup)) => wakeup,
        _ => return Ok(None),
    };
    Ok(Some(context))
}

fn load_hook_runtime_config(root: &Path) -> Result<HookRuntimeConfig> {
    let path = hook_runtime_config_path(root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(HookRuntimeConfig::default());
        }
        Err(source) => {
            return Err(source).with_context(|| {
                format!("failed to read hook runtime config '{}'", path.display())
            });
        }
    };
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse hook runtime config '{}'", path.display()))
}

fn run_hook_rewrite(args: HookRewriteArgs) -> Result<i32> {
    let root = PathBuf::from(args.root);
    match args.command {
        HookRewriteCommand::On => {
            anyhow::bail!(
                "automatic host-hook command rewriting is unsupported because it changes native permission matching; hooks capture results without modifying commands. Use explicit Packet28 CLI/MCP reduction instead"
            );
        }
        HookRewriteCommand::Off => {
            set_hook_rewrite_enabled(&root, false)?;
            println!(
                "Packet28 hook command rewriting is disabled for {}; PostToolUse capture remains enabled",
                root.display()
            );
        }
        HookRewriteCommand::Status(args) => {
            let config = load_hook_runtime_config(&root)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "root": root,
                        "rewrite_enabled": false,
                        "legacy_rewrite_enabled": config.rewrite_enabled,
                        "mode": "capture_only",
                        "hooks_enabled": config.hooks_enabled,
                        "fallback_post_tool_capture": config.fallback_post_tool_capture
                    }))?
                );
            } else {
                println!(
                    "Packet28 host hooks capture results without rewriting commands for {} (legacy rewrite_enabled={}; inactive). Use explicit Packet28 CLI/MCP reduction instead.",
                    root.display(), config.rewrite_enabled
                );
            }
        }
    }
    Ok(0)
}

fn set_hook_rewrite_enabled(root: &Path, enabled: bool) -> Result<()> {
    let mut config = load_hook_runtime_config(root)?;
    config.rewrite_enabled = enabled;
    write_hook_runtime_config(root, &config)
}

fn write_hook_runtime_config(root: &Path, config: &HookRuntimeConfig) -> Result<()> {
    let path = hook_runtime_config_path(root);
    let bytes = format!("{}\n", serde_json::to_string_pretty(config)?);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hook-runtime-v1.json");
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        now_unix_millis()
    ));
    fs::write(&temp_path, bytes.as_bytes())
        .with_context(|| format!("failed to write '{}'", temp_path.display()))?;
    fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "failed to atomically replace '{}' with '{}'",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "cmd_hook_tests.rs"]
mod tests;
