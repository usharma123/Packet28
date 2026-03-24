use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    hook_runtime_config_path, load_task_registry, now_unix, task_artifact_dir, ActiveTaskRecord,
    HookBoundaryKind, HookEventKind, HookIngestRequest, HookLifecycleEvent, HookLifecycleKind,
    HookReducerCacheEntry, HookReducerPacket, HookRuntimeConfig, TaskRecord,
};
use packet28_reducer_core::{
    classify_command, classify_command_argv, reduce_command_output, CommandReducerSpec,
};
use serde_json::{json, Value};

#[derive(Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub command: HookCommands,
}

#[derive(Subcommand)]
pub enum HookCommands {
    Claude(ClaudeHookArgs),
    ReducerRunner(ReducerRunnerArgs),
    ReduceFixture(ReduceFixtureArgs),
}

#[derive(Args, Clone)]
pub struct ClaudeHookArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub event: Option<String>,
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
        HookCommands::ReducerRunner(args) => run_reducer_runner(args),
        HookCommands::ReduceFixture(args) => run_reduce_fixture(args),
    }
}

fn run_claude(args: ClaudeHookArgs) -> Result<i32> {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;
    let payload = if buffer.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&buffer)?
    };
    let root = resolve_hook_root(&args, &payload);
    crate::broker_client::ensure_daemon(&root)?;

    let runtime_config = load_hook_runtime_config(&root);
    let event_kind = args
        .event
        .as_deref()
        .map(|value| parse_event_kind(Some(value)))
        .unwrap_or_else(|| parse_event_kind(json_string(&payload, "hook_event_name").as_deref()));
    let session_id = json_string(&payload, "session_id");
    let task_id = resolve_task_id(&root, &payload, session_id.as_deref())?;
    let matcher = json_string(&payload, "matcher");
    let source = json_string(&payload, "source");

    let rewrite = build_pretool_rewrite(
        &runtime_config,
        &root,
        &payload,
        event_kind,
        &task_id,
        session_id.as_deref(),
    )?;
    let reducer_packet = build_reducer_packet(&runtime_config, &payload, event_kind);
    let response = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id,
            session_id,
            event_kind,
            matcher,
            source,
            boundary_kind: boundary_for_event(event_kind),
            lifecycle_event: None,
            reducer_packet,
            host_context_budget_tokens: std::env::var("PACKET28_HOST_CONTEXT_BUDGET_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok()),
        },
    )?;

    emit_hook_output(event_kind, rewrite, &response)?;
    Ok(if response.block_stop { 2 } else { 0 })
}

fn run_reducer_runner(args: ReducerRunnerArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    crate::broker_client::ensure_daemon(&root)?;
    if args.argv.is_empty() {
        return Err(anyhow!("reducer-runner requires a command after '--'"));
    }

    let task_id = if let Some(task_id) = args
        .task_id
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        task_id
    } else if let Some(active) = crate::task_runtime::load_active_task(&root) {
        active.task_id
    } else {
        crate::broker_client::derive_task_id("claude-hook-runner")
    };
    crate::task_runtime::store_active_task(
        &root,
        &ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: args.session_id.clone(),
            updated_at_unix: now_unix(),
        },
    )?;

    let cwd = args
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let command_text = shell_join(&args.argv);
    let spec = classify_command_argv(&command_text, &args.argv)
        .ok_or_else(|| anyhow!("command is not eligible for reducer rewrite"))?;
    if spec.family != args.family
        || spec.canonical_kind != args.kind
        || spec.cache_fingerprint != args.fingerprint
    {
        return Err(anyhow!("reducer-runner classification mismatch"));
    }

    if let Some((cached_packet, exit_code)) =
        cached_reducer_packet(&root, &task_id, &spec, &command_text)
    {
        let command_id = format!("runner-cache-{}", now_unix_millis());
        let _ = crate::broker_client::hook_ingest(
            &root,
            HookIngestRequest {
                task_id,
                session_id: args.session_id,
                event_kind: HookEventKind::CommandFinished,
                matcher: None,
                source: Some("packet28-reducer-runner-cache".to_string()),
                boundary_kind: HookBoundaryKind::None,
                lifecycle_event: Some(HookLifecycleEvent {
                    kind: HookLifecycleKind::CommandFinished,
                    command_id: Some(command_id),
                    reducer_family: cached_packet.reducer_family.clone(),
                    canonical_command_kind: cached_packet.canonical_command_kind.clone(),
                    cache_fingerprint: cached_packet.cache_fingerprint.clone(),
                    elapsed_ms: Some(0),
                    exit_code: cached_packet.exit_code,
                    ..HookLifecycleEvent::default()
                }),
                reducer_packet: Some(cached_packet.clone()),
                host_context_budget_tokens: None,
            },
        )?;
        println!("{}", cached_packet.summary);
        return Ok(exit_code);
    }

    let command_id = format!("runner-{}", now_unix_millis());
    let spool_dir = task_artifact_dir(&root, &task_id).join("hook-spool");
    fs::create_dir_all(&spool_dir)?;
    let stdout_path = spool_dir.join(format!("{command_id}-stdout.log"));
    let stderr_path = spool_dir.join(format!("{command_id}-stderr.log"));
    let stdout_file = File::create(&stdout_path)
        .with_context(|| format!("failed to create '{}'", stdout_path.display()))?;
    let stderr_file = File::create(&stderr_path)
        .with_context(|| format!("failed to create '{}'", stderr_path.display()))?;

    let _ = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id: task_id.clone(),
            session_id: args.session_id.clone(),
            event_kind: HookEventKind::CommandStarted,
            matcher: None,
            source: Some("packet28-reducer-runner".to_string()),
            boundary_kind: HookBoundaryKind::None,
            lifecycle_event: Some(HookLifecycleEvent {
                kind: HookLifecycleKind::CommandStarted,
                command_id: Some(command_id.clone()),
                reducer_family: Some(spec.family.clone()),
                canonical_command_kind: Some(spec.canonical_kind.clone()),
                cache_fingerprint: Some(spec.cache_fingerprint.clone()),
                stdout_spool_path: Some(stdout_path.display().to_string()),
                stderr_spool_path: Some(stderr_path.display().to_string()),
                ..HookLifecycleEvent::default()
            }),
            reducer_packet: None,
            host_context_budget_tokens: None,
        },
    )?;

    let started = Instant::now();
    let mut child = Command::new(&args.argv[0])
        .args(&args.argv[1..])
        .current_dir(&cwd)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .envs(args.env.iter().filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        }))
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", args.argv[0]))?;

    let mut last_stdout_bytes = 0_u64;
    let mut last_stderr_bytes = 0_u64;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let stdout_bytes = fs::metadata(&stdout_path)
            .map(|meta| meta.len())
            .unwrap_or(last_stdout_bytes);
        let stderr_bytes = fs::metadata(&stderr_path)
            .map(|meta| meta.len())
            .unwrap_or(last_stderr_bytes);
        if stdout_bytes != last_stdout_bytes || stderr_bytes != last_stderr_bytes {
            last_stdout_bytes = stdout_bytes;
            last_stderr_bytes = stderr_bytes;
            let _ = crate::broker_client::hook_ingest(
                &root,
                HookIngestRequest {
                    task_id: task_id.clone(),
                    session_id: args.session_id.clone(),
                    event_kind: HookEventKind::CommandProgress,
                    matcher: None,
                    source: Some("packet28-reducer-runner".to_string()),
                    boundary_kind: HookBoundaryKind::None,
                    lifecycle_event: Some(HookLifecycleEvent {
                        kind: HookLifecycleKind::CommandProgress,
                        command_id: Some(command_id.clone()),
                        reducer_family: Some(spec.family.clone()),
                        canonical_command_kind: Some(spec.canonical_kind.clone()),
                        cache_fingerprint: Some(spec.cache_fingerprint.clone()),
                        stdout_spool_path: Some(stdout_path.display().to_string()),
                        stderr_spool_path: Some(stderr_path.display().to_string()),
                        stdout_bytes: Some(stdout_bytes),
                        stderr_bytes: Some(stderr_bytes),
                        elapsed_ms: Some(started.elapsed().as_millis() as u64),
                        ..HookLifecycleEvent::default()
                    }),
                    reducer_packet: None,
                    host_context_budget_tokens: None,
                },
            );
        }
        thread::sleep(Duration::from_millis(200));
    };

    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let exit_code = status.code().unwrap_or(1);
    let reduced = reduce_command_output(&spec, &stdout, &stderr, exit_code)?;
    let artifact = json!({
        "command_id": command_id,
        "command": command_text,
        "argv": args.argv,
        "cwd": cwd.display().to_string(),
        "stdout_spool_path": stdout_path.display().to_string(),
        "stderr_spool_path": stderr_path.display().to_string(),
        "stdout_preview": compact_text(&stdout, 400),
        "stderr_preview": compact_text(&stderr, 400),
        "stdout_bytes": fs::metadata(&stdout_path).map(|meta| meta.len()).unwrap_or(0),
        "stderr_bytes": fs::metadata(&stderr_path).map(|meta| meta.len()).unwrap_or(0),
        "exit_code": exit_code,
    });
    let est_bytes = reduced.summary.len() as u64;
    let est_tokens = ((est_bytes as f64) / 4.0).ceil() as u64;
    let response = crate::broker_client::hook_ingest(
        &root,
        HookIngestRequest {
            task_id,
            session_id: args.session_id,
            event_kind: HookEventKind::CommandFinished,
            matcher: None,
            source: Some("packet28-reducer-runner".to_string()),
            boundary_kind: HookBoundaryKind::None,
            lifecycle_event: Some(HookLifecycleEvent {
                kind: HookLifecycleKind::CommandFinished,
                command_id: Some(command_id),
                reducer_family: Some(reduced.family.clone()),
                canonical_command_kind: Some(reduced.canonical_kind.clone()),
                cache_fingerprint: Some(reduced.cache_fingerprint.clone()),
                stdout_spool_path: Some(stdout_path.display().to_string()),
                stderr_spool_path: Some(stderr_path.display().to_string()),
                stdout_bytes: Some(
                    fs::metadata(&stdout_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ),
                stderr_bytes: Some(
                    fs::metadata(&stderr_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0),
                ),
                elapsed_ms: Some(started.elapsed().as_millis() as u64),
                exit_code: Some(exit_code),
            }),
            reducer_packet: Some(HookReducerPacket {
                packet_type: reduced.packet_type,
                tool_name: "Bash".to_string(),
                operation_kind: reduced.operation_kind,
                reducer_family: Some(reduced.family),
                canonical_command_kind: Some(reduced.canonical_kind),
                summary: reduced.summary.clone(),
                compact_preview: (!reduced.compact_preview.is_empty())
                    .then_some(reduced.compact_preview.clone()),
                command: Some(command_text),
                search_query: None,
                compact_path: Some("reducer_rewrite".to_string()),
                passthrough_reason: None,
                raw_est_tokens: Some((((stdout.len() + stderr.len()) as f64) / 4.0).ceil() as u64),
                reduced_est_tokens: Some(est_tokens),
                paths: reduced.paths,
                regions: reduced.regions,
                symbols: reduced.symbols,
                equivalence_key: reduced.equivalence_key,
                est_tokens,
                est_bytes,
                failed: reduced.failed,
                error_class: reduced.error_class,
                error_message: reduced.error_message,
                retryable: reduced.retryable,
                duration_ms: Some(started.elapsed().as_millis() as u64),
                exit_code: Some(reduced.exit_code),
                cache_fingerprint: Some(reduced.cache_fingerprint),
                cacheable: Some(reduced.cacheable),
                mutation: Some(reduced.mutation),
                raw_artifact_handle: Some(stdout_path.display().to_string()),
                raw_artifact_available: true,
                artifact: Some(artifact),
            }),
            host_context_budget_tokens: None,
        },
    )?;
    let _ = response;
    println!("{}", reduced.summary);
    Ok(exit_code)
}

fn run_reduce_fixture(args: ReduceFixtureArgs) -> Result<i32> {
    let stdout = fs::read_to_string(&args.stdout_path)
        .with_context(|| format!("failed to read fixture '{}'", args.stdout_path))?;
    let stderr = if let Some(stderr_path) = args.stderr_path.as_ref() {
        fs::read_to_string(stderr_path)
            .with_context(|| format!("failed to read fixture '{}'", stderr_path))?
    } else {
        String::new()
    };
    let spec = classify_command(&args.command)
        .ok_or_else(|| anyhow!("fixture command is not eligible for reducer classification"))?;
    let reduced = reduce_command_output(&spec, &stdout, &stderr, args.exit_code)?;
    let raw_visible = format!("{stdout}{stderr}");
    let raw_tokens = estimate_text_tokens(&raw_visible);
    let reduced_tokens = estimate_text_tokens(&reduced.summary);
    let payload = json!({
        "command": args.command,
        "family": reduced.family,
        "canonical_kind": reduced.canonical_kind,
        "summary": reduced.summary,
        "failed": reduced.failed,
        "exit_code": reduced.exit_code,
        "raw_bytes": raw_visible.len(),
        "raw_est_tokens": raw_tokens,
        "reduced_bytes": payload_text_len(&reduced.summary),
        "reduced_est_tokens": reduced_tokens,
        "raw_preview": compact_text(&raw_visible, 400),
        "reduced_preview": reduced.summary,
        "token_reduction_pct": reduction_pct(raw_tokens, reduced_tokens),
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "{}",
            payload["reduced_preview"].as_str().unwrap_or_default()
        );
    }
    Ok(0)
}

fn cached_reducer_packet(
    root: &Path,
    task_id: &str,
    spec: &CommandReducerSpec,
    command_text: &str,
) -> Option<(HookReducerPacket, i32)> {
    let registry = load_task_registry(root).ok()?;
    let task = registry.tasks.get(task_id)?;
    let entry = task.hook_reducer_cache.get(&spec.cache_fingerprint)?;
    if !cache_entry_matches(task, entry, spec) {
        return None;
    }
    let est_bytes = entry.summary.len() as u64;
    let est_tokens = ((est_bytes as f64) / 4.0).ceil() as u64;
    let exit_code = entry.exit_code.unwrap_or(if entry.failed { 1 } else { 0 });
    Some((
        HookReducerPacket {
            packet_type: spec.packet_type.clone(),
            tool_name: "Bash".to_string(),
            operation_kind: spec.operation_kind,
            reducer_family: Some(spec.family.clone()),
            canonical_command_kind: Some(spec.canonical_kind.clone()),
            summary: entry.summary.clone(),
            compact_preview: entry.compact_preview.clone(),
            command: Some(command_text.to_string()),
            search_query: None,
            compact_path: Some("reducer_rewrite".to_string()),
            passthrough_reason: None,
            raw_est_tokens: None,
            reduced_est_tokens: Some(est_tokens),
            paths: entry.paths.clone(),
            regions: entry.regions.clone(),
            symbols: entry.symbols.clone(),
            equivalence_key: spec.equivalence_key.clone(),
            est_tokens,
            est_bytes,
            failed: entry.failed,
            error_class: entry.failed.then_some("cached_tool_error".to_string()),
            error_message: entry.error_message.clone(),
            retryable: entry.failed.then_some(false),
            duration_ms: Some(0),
            exit_code: Some(exit_code),
            cache_fingerprint: Some(spec.cache_fingerprint.clone()),
            cacheable: Some(spec.cacheable),
            mutation: Some(spec.mutation),
            raw_artifact_handle: entry.raw_artifact_handle.clone(),
            raw_artifact_available: entry.raw_artifact_handle.is_some(),
            artifact: None,
        },
        exit_code,
    ))
}

fn cache_entry_matches(
    task: &TaskRecord,
    entry: &HookReducerCacheEntry,
    spec: &CommandReducerSpec,
) -> bool {
    if entry.reducer_family != spec.family || entry.canonical_command_kind != spec.canonical_kind {
        return false;
    }
    if entry.git_epoch != task.hook_git_epoch
        || entry.fs_epoch != task.hook_fs_epoch
        || entry.rust_epoch != task.hook_rust_epoch
    {
        return false;
    }
    if entry.reducer_family == "github" {
        let age = now_unix().saturating_sub(entry.occurred_at_unix);
        return age <= 300;
    }
    true
}

fn resolve_hook_root(args: &ClaudeHookArgs, payload: &Value) -> PathBuf {
    if args.root.trim() != "." {
        return crate::broker_client::resolve_root(&args.root);
    }
    json_string(payload, "cwd")
        .map(|cwd| crate::broker_client::resolve_root(&cwd))
        .unwrap_or_else(|| crate::broker_client::resolve_root("."))
}

fn resolve_task_id(root: &Path, payload: &Value, session_id: Option<&str>) -> Result<String> {
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
    if let Some(active) = crate::task_runtime::load_active_task(root) {
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

fn parse_event_kind(value: Option<&str>) -> HookEventKind {
    match value.unwrap_or_default().trim() {
        "SessionStart" => HookEventKind::SessionStart,
        "UserPromptSubmit" => HookEventKind::UserPromptSubmit,
        "PreToolUse" => HookEventKind::PreToolUse,
        "PostToolUse" => HookEventKind::PostToolUse,
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

fn emit_hook_output(
    event_kind: HookEventKind,
    rewrite: Option<Value>,
    response: &packet28_daemon_core::HookIngestResponse,
) -> Result<()> {
    match event_kind {
        HookEventKind::SessionStart => {
            if let Some(additional_context) = response.additional_context.as_ref() {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "hookSpecificOutput": {
                            "hookEventName": "SessionStart",
                            "additionalContext": additional_context,
                        }
                    }))?
                );
            }
        }
        HookEventKind::PreToolUse => {
            if let Some(updated_input) = rewrite {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "allow",
                            "updatedInput": updated_input,
                        }
                    }))?
                );
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
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "decision": "block",
                        "reason": response.stop_reason.clone().unwrap_or_else(|| "Packet28 requires an intention before stop".to_string()),
                    }))?
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn build_pretool_rewrite(
    runtime_config: &HookRuntimeConfig,
    root: &Path,
    payload: &Value,
    event_kind: HookEventKind,
    task_id: &str,
    session_id: Option<&str>,
) -> Result<Option<Value>> {
    if !matches!(event_kind, HookEventKind::PreToolUse) || !runtime_config.rewrite_enabled {
        return Ok(None);
    }
    if json_string(payload, "tool_name").as_deref() != Some("Bash") {
        return Ok(None);
    }
    let Some(tool_input) = payload.get("tool_input") else {
        return Ok(None);
    };
    let Some(command) = json_string(tool_input, "command") else {
        return Ok(None);
    };
    let hook_cwd = json_string(payload, "cwd").unwrap_or_else(|| root.display().to_string());
    let hook_cwd_path = std::path::Path::new(&hook_cwd);
    let mut decision =
        crate::route_registry::decide_command_route_with_cwd(&command, hook_cwd_path);

    // In hook context, promote NativeTool → ReducerRewrite when the reducer-core
    // also classifies the command (e.g. head/cat/sed → fs family).
    if matches!(decision.kind, crate::route_registry::RouteKind::NativeTool) {
        if let Some(spec) = packet28_reducer_core::classify_command_argv(&command, &decision.argv) {
            decision = crate::route_registry::RouteDecision {
                kind: crate::route_registry::RouteKind::ReducerRewrite,
                reason: None,
                argv: decision.argv,
                env_assignments: decision.env_assignments,
                reducer_spec: Some(spec),
                native_tool: None,
                original_argv: decision.original_argv,
            };
        }
    }

    // Only allow ReducerRewrite (when family is in allowlist) and NativeTool
    // through hook rewrites. ProxyPassthrough and RawPassthrough are not
    // rewritten in the hook path.
    let proceed = match &decision.kind {
        crate::route_registry::RouteKind::ReducerRewrite => {
            decision.reducer_spec.as_ref().is_some_and(|spec| {
                runtime_config
                    .reducer_allowlist
                    .iter()
                    .any(|entry| entry == &spec.family)
            })
        }
        crate::route_registry::RouteKind::NativeTool => true,
        _ => false,
    };
    if !proceed {
        return Ok(None);
    }

    let mut updated_input = tool_input.clone();
    let Some(rewritten) =
        crate::route_registry::build_route_rewrite(root, task_id, session_id, &hook_cwd, &decision)
    else {
        return Ok(None);
    };
    if let Some(object) = updated_input.as_object_mut() {
        object.insert("command".to_string(), Value::String(rewritten));
    } else {
        updated_input = json!({ "command": rewritten });
    }
    Ok(Some(updated_input))
}

fn build_reducer_packet(
    runtime_config: &HookRuntimeConfig,
    payload: &Value,
    event_kind: HookEventKind,
) -> Option<HookReducerPacket> {
    if !matches!(event_kind, HookEventKind::PostToolUse)
        || !runtime_config.fallback_post_tool_capture
    {
        return None;
    }
    let tool_name = json_string(payload, "tool_name")?;
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let response = payload.get("tool_response").cloned().unwrap_or(Value::Null);
    if tool_name == "Bash"
        && json_string(&input, "command")
            .as_deref()
            .is_some_and(|command| command.contains(" hook reducer-runner "))
    {
        return None;
    }
    match tool_name.as_str() {
        "Bash" => build_bash_packet(&input, &response),
        "Read" => build_read_packet(&input, &response),
        "Grep" => build_grep_packet(&input, &response),
        "Glob" => build_glob_packet(&input, &response),
        "Edit" | "MultiEdit" | "Write" => build_edit_packet(&tool_name, &input, &response),
        _ => Some(build_generic_packet(&tool_name, &input, &response)),
    }
}

fn build_bash_packet(input: &Value, response: &Value) -> Option<HookReducerPacket> {
    let command = json_string(input, "command")?;
    let output = hook_output_text(response);
    let summary = first_nonempty_line(&output)
        .unwrap_or_else(|| format!("command completed: {}", compact_text(&command, 100)));
    let spec = classify_command(&command);
    let (packet_type, operation_kind, family, canonical_kind, fingerprint, paths, equivalence_key) =
        if let Some(spec) = spec {
            (
                format!("packet28.hook.fallback.{}.v1", spec.family),
                spec.operation_kind,
                Some(spec.family),
                Some(spec.canonical_kind),
                Some(spec.cache_fingerprint),
                spec.paths,
                spec.equivalence_key,
            )
        } else {
            (
                "packet28.hook.command.v1".to_string(),
                suite_packet_core::ToolOperationKind::Generic,
                Some("generic".to_string()),
                None,
                None,
                extract_command_paths(&command),
                None,
            )
        };
    Some(packet_from_parts(
        &packet_type,
        "Bash",
        operation_kind,
        family,
        canonical_kind,
        summary,
        Some(command),
        None,
        paths,
        Vec::new(),
        Vec::new(),
        equivalence_key,
        fingerprint,
        Some(false),
        response.clone(),
        response_failed(response),
    ))
}

fn build_read_packet(input: &Value, response: &Value) -> Option<HookReducerPacket> {
    let path = json_string(input, "file_path")
        .or_else(|| json_string(input, "path"))
        .or_else(|| json_string(input, "target"))?;
    let line_start = input.get("offset").and_then(Value::as_u64).unwrap_or(1);
    let count = input.get("limit").and_then(Value::as_u64).unwrap_or(1);
    let line_end = line_start.saturating_add(count.saturating_sub(1));
    let summary = format!("Read {} lines from {}", count, path);
    let mut regions = json_array_strings(response, "regions");
    if regions.is_empty() {
        regions.push(format!("{path}:{line_start}-{line_end}"));
    }
    Some(packet_from_parts(
        "packet28.hook.read.v1",
        "Read",
        suite_packet_core::ToolOperationKind::Read,
        Some("claude_native".to_string()),
        Some("read".to_string()),
        summary,
        None,
        None,
        vec![path.clone()],
        regions,
        json_array_strings(response, "symbols"),
        Some(format!("read:{path}")),
        Some(format!("read:{}:{}:{}", path, line_start, line_end)),
        Some(true),
        response.clone(),
        response_failed(response),
    ))
}

fn build_grep_packet(input: &Value, response: &Value) -> Option<HookReducerPacket> {
    let query = json_string(input, "pattern")
        .or_else(|| json_string(input, "query"))
        .or_else(|| json_string(input, "search"))?;
    let paths = json_array_strings(response, "files")
        .into_iter()
        .chain(json_array_strings(input, "include").into_iter())
        .collect::<Vec<_>>();
    let count = json_array_len(response, "matches")
        .unwrap_or_else(|| hook_output_text(response).lines().count());
    let summary = format!("Grep found {count} matches for '{query}'");
    Some(packet_from_parts(
        "packet28.hook.grep.v1",
        "Grep",
        suite_packet_core::ToolOperationKind::Search,
        Some("claude_native".to_string()),
        Some("grep".to_string()),
        summary,
        None,
        Some(query.clone()),
        paths.clone(),
        Vec::new(),
        Vec::new(),
        Some(format!("grep:{}:{}", query, paths.join(","))),
        Some(format!("grep:{}:{}", query, paths.join(","))),
        Some(true),
        response.clone(),
        response_failed(response),
    ))
}

fn build_glob_packet(input: &Value, response: &Value) -> Option<HookReducerPacket> {
    let pattern = json_string(input, "pattern")?;
    let paths = if let Some(array) = response.as_array() {
        array
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let summary = format!("Glob matched {} path(s) for '{}'", paths.len(), pattern);
    Some(packet_from_parts(
        "packet28.hook.glob.v1",
        "Glob",
        suite_packet_core::ToolOperationKind::Search,
        Some("claude_native".to_string()),
        Some("glob".to_string()),
        summary,
        None,
        Some(pattern.clone()),
        paths,
        Vec::new(),
        Vec::new(),
        Some(format!("glob:{pattern}")),
        Some(format!("glob:{pattern}")),
        Some(true),
        response.clone(),
        response_failed(response),
    ))
}

fn build_edit_packet(
    tool_name: &str,
    input: &Value,
    response: &Value,
) -> Option<HookReducerPacket> {
    let path = json_string(input, "file_path")
        .or_else(|| json_string(input, "path"))
        .or_else(|| json_string(input, "target"))?;
    let summary = format!("{tool_name} updated {path}");
    Some(packet_from_parts(
        "packet28.hook.edit.v1",
        tool_name,
        suite_packet_core::ToolOperationKind::Edit,
        Some("claude_native".to_string()),
        Some("edit".to_string()),
        summary,
        None,
        None,
        vec![path],
        Vec::new(),
        Vec::new(),
        None,
        None,
        Some(false),
        json!({
            "tool_input": input,
            "tool_response": response,
        }),
        response_failed(response),
    ))
}

fn build_generic_packet(tool_name: &str, input: &Value, response: &Value) -> HookReducerPacket {
    packet_from_parts(
        "packet28.hook.generic.v1",
        tool_name,
        suite_packet_core::ToolOperationKind::Generic,
        Some("claude_native".to_string()),
        None,
        format!("{tool_name} completed"),
        None,
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        None,
        Some(false),
        json!({
            "tool_input": input,
            "tool_response": response,
        }),
        response_failed(response),
    )
}

#[allow(clippy::too_many_arguments)]
fn packet_from_parts(
    packet_type: &str,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    reducer_family: Option<String>,
    canonical_command_kind: Option<String>,
    summary: String,
    command: Option<String>,
    search_query: Option<String>,
    paths: Vec<String>,
    regions: Vec<String>,
    symbols: Vec<String>,
    equivalence_key: Option<String>,
    cache_fingerprint: Option<String>,
    cacheable: Option<bool>,
    artifact: Value,
    failed: bool,
) -> HookReducerPacket {
    let raw_text = hook_output_text(&artifact);
    let raw_est_tokens = (((raw_text.len()) as f64) / 4.0).ceil() as u64;
    let est_bytes = summary.len() as u64;
    let est_tokens = ((est_bytes as f64) / 4.0).ceil() as u64;
    let compact_path = if reducer_family.as_deref() == Some("claude_native") {
        Some("native_tool".to_string())
    } else if tool_name == "Bash" {
        Some("raw_passthrough".to_string())
    } else {
        Some("native_tool".to_string())
    };
    let passthrough_reason = (tool_name == "Bash").then(|| "post_tool_capture".to_string());
    HookReducerPacket {
        packet_type: packet_type.to_string(),
        tool_name: tool_name.to_string(),
        operation_kind,
        reducer_family,
        canonical_command_kind,
        summary,
        compact_preview: None,
        command,
        search_query,
        compact_path,
        passthrough_reason,
        raw_est_tokens: Some(raw_est_tokens),
        reduced_est_tokens: Some(est_tokens),
        paths,
        regions,
        symbols,
        equivalence_key,
        est_tokens,
        est_bytes,
        failed,
        error_class: failed.then(|| "tool_error".to_string()),
        error_message: failed.then(|| compact_text(&hook_output_text(&artifact), 200)),
        retryable: failed.then_some(false),
        duration_ms: None,
        exit_code: None,
        cache_fingerprint,
        cacheable,
        mutation: Some(false),
        raw_artifact_handle: None,
        raw_artifact_available: false,
        artifact: Some(artifact),
    }
}

fn load_hook_runtime_config(root: &Path) -> HookRuntimeConfig {
    fs::read_to_string(hook_runtime_config_path(root))
        .ok()
        .and_then(|raw| serde_json::from_str::<HookRuntimeConfig>(&raw).ok())
        .unwrap_or_default()
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
        .collect()
}

fn json_array_len(value: &Value, key: &str) -> Option<usize> {
    value.get(key).and_then(Value::as_array).map(Vec::len)
}

fn hook_output_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    for key in ["stdout", "stderr", "output", "text", "content"] {
        if let Some(text) = json_string(value, key) {
            return text;
        }
    }
    serde_json::to_string(value).unwrap_or_else(|_| String::new())
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| compact_text(line, 160))
}

fn compact_text(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = compact.chars().count();
    if char_count <= limit {
        compact
    } else if limit <= 3 {
        "...".to_string()
    } else {
        let shortened = compact
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
}

fn response_failed(response: &Value) -> bool {
    response
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || response.get("error").is_some()
}

fn extract_command_paths(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .filter(|part| {
            part.contains('/')
                || part.ends_with(".rs")
                || part.ends_with(".md")
                || part.ends_with(".json")
                || part.ends_with(".toml")
        })
        .map(|part| {
            part.trim_matches(|ch| ch == '"' || ch == '\'' || ch == ',')
                .to_string()
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn payload_text_len(text: &str) -> usize {
    text.len()
}

fn estimate_text_tokens(text: &str) -> u64 {
    let bytes = text.len() as u64;
    if bytes == 0 {
        0
    } else {
        (bytes + 3) / 4
    }
}

fn reduction_pct(raw_tokens: u64, reduced_tokens: u64) -> f64 {
    if raw_tokens == 0 {
        0.0
    } else {
        ((raw_tokens.saturating_sub(reduced_tokens)) as f64 * 100.0 / raw_tokens as f64 * 10.0)
            .round()
            / 10.0
    }
}

fn now_unix_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretool_rewrites_strict_git_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"git status --short src/lib.rs"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family git"));
        assert!(command.contains("--kind git_status"));
    }

    #[test]
    fn pretool_declines_composed_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"cargo test 2>&1 | grep FAILED"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_fs_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"head -n 5 README.md"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family fs"));
        assert!(command.contains("--kind fs_head"));
    }

    #[test]
    fn pretool_rewrites_strict_rust_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"cargo test -p packet28-reducer-core"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family rust"));
        assert!(command.contains("--kind rust_test"));
    }

    #[test]
    fn pretool_declines_ambiguous_fs_sed_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"sed -i 1,4p README.md"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_github_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"gh pr list --limit 5"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family github"));
        assert!(command.contains("--kind gh_pr_list"));
    }

    #[test]
    fn pretool_declines_ambiguous_github_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"gh pr list --json title"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_python_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"python3 -m pytest tests"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family python"));
        assert!(command.contains("--kind python_pytest"));
    }

    #[test]
    fn pretool_declines_ambiguous_python_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"ruff check --output-format json src"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_javascript_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"npx tsc --noEmit"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family javascript"));
        assert!(command.contains("--kind javascript_tsc"));
    }

    #[test]
    fn pretool_declines_ambiguous_javascript_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"eslint --format json src"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_go_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"go test ./..."}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family go"));
        assert!(command.contains("--kind go_test"));
    }

    #[test]
    fn pretool_declines_ambiguous_go_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"go test -json ./..."}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn pretool_rewrites_strict_infra_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"kubectl get pods"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            Some("session-1"),
        )
        .unwrap()
        .unwrap();
        let command = rewrite["command"].as_str().unwrap();
        assert!(command.contains("hook reducer-runner"));
        assert!(command.contains("--family infra"));
        assert!(command.contains("--kind kubectl_get"));
    }

    #[test]
    fn pretool_declines_ambiguous_infra_command() {
        let root = PathBuf::from("/tmp/demo");
        let payload = json!({
            "tool_name":"Bash",
            "tool_input":{"command":"curl -o out.txt https://example.com"}
        });
        let rewrite = build_pretool_rewrite(
            &HookRuntimeConfig::default(),
            &root,
            &payload,
            HookEventKind::PreToolUse,
            "task-123",
            None,
        )
        .unwrap();
        assert!(rewrite.is_none());
    }

    #[test]
    fn post_tool_skips_reducer_runner_command() {
        let packet = build_reducer_packet(
            &HookRuntimeConfig::default(),
            &json!({
                "tool_name":"Bash",
                "tool_input":{"command":"Packet28 hook reducer-runner --root . -- task"},
                "tool_response":{"stdout":"done"}
            }),
            HookEventKind::PostToolUse,
        );
        assert!(packet.is_none());
    }

    #[test]
    fn read_reducer_marks_read_operation() {
        let packet = build_read_packet(
            &json!({"file_path":"src/lib.rs","offset":10,"limit":5}),
            &json!({"content":"demo"}),
        )
        .unwrap();
        assert_eq!(
            packet.operation_kind,
            suite_packet_core::ToolOperationKind::Read
        );
        assert_eq!(packet.paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(
            packet.cache_fingerprint.as_deref(),
            Some("read:src/lib.rs:10:14")
        );
    }
}
