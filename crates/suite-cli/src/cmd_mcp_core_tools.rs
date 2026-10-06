use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::storage::{load_task_registry, now_unix};
use packet28_daemon_protocol::hooks::ActiveTaskRecord;
use packet28_daemon_protocol::paths::hook_runtime_config_path;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cmd_mcp::native_tools::{handle_packet28_write_intention, Packet28WriteIntentionArgs};
use crate::cmd_mcp::response::capabilities_payload;
use crate::cmd_mcp::support::{
    broker_task_status_via_session, resolve_live_session_task_id, resolve_session_task_id,
    track_task,
};
use crate::cmd_mcp::tool_args::*;
use crate::cmd_mcp::McpSessionState;
use crate::route_registry::{
    build_route_rewrite, decide_command_route_with_cwd_and_root, NativeToolKind, RouteKind,
};

pub(super) fn handle_core_tool_call(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    name: &str,
    arguments: &Value,
) -> Result<Option<Value>> {
    let payload = match name {
        "packet28.verify_experiments" => {
            let request: VerifyExperimentsToolArgs = serde_json::from_value(arguments.clone())?;
            let manifest = root.join(
                request
                    .manifest
                    .as_deref()
                    .unwrap_or("docs/experiments/manifest.json"),
            );
            crate::cmd_verify::verify_experiments_payload(
                root,
                &manifest,
                &request.require_workflows.unwrap_or_default(),
                false,
            )?
        }
        "packet28.reducer_drift" => {
            let request: ReducerDriftToolArgs = serde_json::from_value(arguments.clone())?;
            let fixture = root.join(
                request
                    .fixture
                    .as_deref()
                    .unwrap_or("docs/reducer-drift/fixtures.json"),
            );
            crate::cmd_verify::verify_reducer_drift_payload(&fixture)?
        }
        "packet28.hypothesis_add" => {
            let mut request: HypothesisAddToolArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = Some(resolve_live_session_task_id(
                session,
                root,
                request.task_id.as_deref().unwrap_or_default(),
                Some(request.text.as_str()),
                name,
            )?);
            let task_id = request.task_id.as_deref().unwrap_or_default();
            track_task(session, root, task_id)?;
            serde_json::to_value(crate::cmd_hypothesis::add_hypothesis_record(
                root,
                task_id,
                request.id,
                &request.text,
                request.paths.unwrap_or_default(),
                request.symbols.unwrap_or_default(),
                request.artifact_id,
            )?)?
        }
        "packet28.hypothesis_list" => {
            let mut request: HypothesisListToolArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = Some(resolve_session_task_id(
                session,
                root,
                request.task_id.as_deref().unwrap_or_default(),
                None,
                name,
            )?);
            let task_id = request.task_id.as_deref().unwrap_or_default();
            track_task(session, root, task_id)?;
            serde_json::to_value(crate::cmd_hypothesis::active_hypotheses(root, task_id)?)?
        }
        "packet28.hypothesis_resolve" => {
            let mut request: HypothesisResolveToolArgs = serde_json::from_value(arguments.clone())?;
            request.task_id = Some(resolve_live_session_task_id(
                session,
                root,
                request.task_id.as_deref().unwrap_or_default(),
                Some(request.id.as_str()),
                name,
            )?);
            let status = match request.status.trim() {
                "confirmed" | "confirm" => "confirmed",
                "rejected" | "reject" => "rejected",
                other => {
                    return Err(anyhow!(
                        "packet28.hypothesis_resolve status must be confirmed or rejected, got '{other}'"
                    ));
                }
            };
            let task_id = request.task_id.as_deref().unwrap_or_default();
            track_task(session, root, task_id)?;
            serde_json::to_value(crate::cmd_hypothesis::resolve_hypothesis_record(
                root,
                task_id,
                &request.id,
                status,
                request.note,
            )?)?
        }
        "packet28.reduce" => {
            let request: ReduceToolArgs = serde_json::from_value(arguments.clone())?;
            handle_packet28_reduce(request)?
        }
        "packet28.rewrite" => {
            let request: RewriteToolArgs = serde_json::from_value(arguments.clone())?;
            handle_packet28_rewrite(root, request)
        }
        "packet28.doctor" => {
            let request: DoctorToolArgs = serde_json::from_value(arguments.clone())?;
            handle_packet28_doctor(root, request)?
        }
        "packet28.write_intention" => {
            let mut request: Packet28WriteIntentionArgs =
                serde_json::from_value(arguments.clone())?;
            request.task_id = resolve_live_session_task_id(
                session,
                root,
                &request.task_id,
                Some(request.text.as_str()),
                "packet28.write_intention",
            )?;
            track_task(session, root, &request.task_id)?;
            crate::task_runtime::store_active_task(
                root,
                &ActiveTaskRecord {
                    task_id: request.task_id.clone(),
                    session_id: None,
                    updated_at_unix: now_unix(),
                },
            )?;
            handle_packet28_write_intention(root, session, request)?
        }
        "packet28.task_status" => {
            let task_id = resolve_live_session_task_id(
                session,
                root,
                arguments
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                None,
                "packet28.task_status",
            )?;
            track_task(session, root, &task_id)?;
            serde_json::to_value(broker_task_status_via_session(root, session, &task_id)?)?
        }
        "packet28.capabilities" => capabilities_payload(),
        _ => return Ok(None),
    };
    Ok(Some(payload))
}

fn handle_packet28_reduce(request: ReduceToolArgs) -> Result<Value> {
    let spec = packet28_reducer_core::classify_command(&request.command)
        .ok_or_else(|| anyhow!("unsupported command for packet28.reduce"))?;
    let reduction = packet28_reducer_core::reduce_command_output(
        &spec,
        request.stdout.as_deref().unwrap_or_default(),
        request.stderr.as_deref().unwrap_or_default(),
        request.exit_code.unwrap_or(0),
    )?;
    Ok(json!({
        "command": request.command,
        "reduction": reduction,
        "reducer_family": spec.family,
        "reducer_kind": spec.canonical_kind,
    }))
}

fn handle_packet28_rewrite(root: &Path, request: RewriteToolArgs) -> Value {
    let cwd = request
        .cwd
        .clone()
        .unwrap_or_else(|| root.display().to_string());
    let decision = decide_command_route_with_cwd_and_root(&request.command, Path::new(&cwd), root);
    let task_id = request.task_id.as_deref().unwrap_or("packet28-mcp-rewrite");
    let rewritten = build_route_rewrite(
        root,
        task_id,
        request.session_id.as_deref(),
        &cwd,
        &decision,
    );
    let native_tool = decision.native_tool.as_ref().map(|tool| match tool.kind {
        NativeToolKind::Tree => "tree",
        NativeToolKind::Read => "read",
        NativeToolKind::Grep => "grep",
        NativeToolKind::Env => "env",
    });
    json!({
        "command": request.command,
        "route": match decision.kind {
            RouteKind::ReducerRewrite => "reducer_rewrite",
            RouteKind::NativeTool => "native_tool",
            RouteKind::TomlFilterRewrite => "toml_filter_rewrite",
            RouteKind::CompoundRewrite => "compound_rewrite",
            RouteKind::ProxyPassthrough => "proxy_passthrough",
            RouteKind::RawPassthrough => "raw_passthrough",
        },
        "reason": decision.reason,
        "env_assignments": decision.env_assignments,
        "native_tool": native_tool,
        "rewritten_command": rewritten,
        "reducer_family": decision.reducer_spec.as_ref().map(|spec| spec.family.clone()),
        "reducer_kind": decision
            .reducer_spec
            .as_ref()
            .map(|spec| spec.canonical_kind.clone()),
    })
}

fn handle_packet28_doctor(root: &Path, request: DoctorToolArgs) -> Result<Value> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("doctor").arg("--root").arg(root).arg("--json");
    if let Some(agent) = request.agent {
        command.arg("--agent").arg(agent);
    }
    let output = command.output().context("failed to run Packet28 doctor")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Packet28 doctor failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).context("Packet28 doctor did not return JSON")
}

pub(super) fn handle_packet28_agent_status(root: &Path, arguments: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    struct AgentStatusArgs {
        task_id: Option<String>,
    }

    let request = serde_json::from_value::<AgentStatusArgs>(arguments).unwrap_or_default();
    let active = crate::task_runtime::load_active_task(root)?;
    let task_id = request
        .task_id
        .or_else(|| active.as_ref().map(|record| record.task_id.clone()));
    let registry = load_task_registry(root).ok();
    let task = task_id.as_ref().and_then(|id| {
        registry
            .as_ref()
            .and_then(|registry| registry.tasks.get(id))
    });
    let cache_entries = task
        .map(|task| task.hook_reducer_cache.len())
        .unwrap_or_default();
    let workspace_guarded_entries = task
        .map(|task| {
            task.hook_reducer_cache
                .values()
                .filter(|entry| entry.workspace_fingerprint.is_some())
                .count()
        })
        .unwrap_or_default();

    Ok(json!({
        "status": "ok",
        "root": root.display().to_string(),
        "active_task_id": active.as_ref().map(|record| record.task_id.clone()),
        "task_id": task_id,
        "hook_config_present": hook_runtime_config_path(root).exists(),
        "reducer_cache_safety": {
            "workspace_fingerprint_enabled": true,
            "policy": "safe_by_default",
            "cache_entries": cache_entries,
            "workspace_guarded_entries": workspace_guarded_entries
        },
        "mcp": {
            "manual_json_rpc_required": false,
            "recommended_path": "Packet28 setup --runtime all --yes"
        },
        "task": task.map(|task| json!({
            "latest_context_version": task.latest_context_version,
            "latest_hook_command_kind": task.latest_hook_command_kind,
            "hook_window_est_tokens": task.hook_window_est_tokens,
            "hook_threshold_exceeded": task.hook_threshold_exceeded
        }))
    }))
}
