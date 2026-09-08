use super::*;
#[cfg(test)]
use crate::broker::broker_write_state;
use crate::broker::{
    broker_prepare_handoff, broker_task_status, broker_write_state_batch, ensure_task_record_mut,
    load_agent_snapshot_for_task, load_task_record, now_unix_millis,
};
use packet28_daemon_protocol::hooks::{
    HookBoundaryKind, HookEventKind, HookIngestRequest, HookIngestResponse, HookReducerCacheEntry,
    HookRuntimeConfig, ThresholdLevel,
};
use packet28_daemon_protocol::paths::hook_runtime_config_path;
use std::sync::atomic::{AtomicU64, Ordering};

static HOOK_ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn load_hook_runtime_config(root: &Path) -> Result<HookRuntimeConfig> {
    let path = hook_runtime_config_path(root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
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

fn store_hook_artifact(root: &Path, task_id: &str, prefix: &str, value: &Value) -> Result<String> {
    let storage_id = task_storage_id(task_id)?;
    let dir = task_artifact_dir(root, &storage_id).join("hook-artifacts");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create '{}'", dir.display()))?;
    let id = format!(
        "{prefix}-{}-{:x}",
        now_unix_millis(),
        HOOK_ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(format!("{id}.json"));
    fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to write '{}'", path.display()))?;
    Ok(id)
}

fn hook_task_additional_context(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    session_id: Option<&str>,
) -> Result<Option<String>> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let task = load_task_record(state, task_id);
    let Some(task) = task else {
        return Ok(None);
    };
    let latest_context_version = task.latest_context_version.clone();
    let latest_handoff_artifact_id = task.latest_handoff_artifact_id.clone();
    if task.latest_handoff_artifact_id.is_none() {
        return Ok(None);
    }
    if task.latest_hook_bootstrap_context_version == latest_context_version
        && task.latest_hook_session_id.as_deref() == session_id
    {
        return Ok(None);
    }
    let storage_id = task_storage_id(task_id)?;
    let path = task_brief_markdown_path(&root, &storage_id);
    let brief = fs::read_to_string(path).ok();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_hook_bootstrap_context_version = latest_context_version;
        task.latest_hook_bootstrap_at_unix = Some(now_unix());
        task.latest_hook_session_id = session_id.map(ToOwned::to_owned);
        task.latest_agent_handoff_artifact_id = latest_handoff_artifact_id;
        persist_task(&guard, task_id)?;
    }
    Ok(brief.filter(|value| !value.trim().is_empty()))
}

fn boundary_reason(kind: HookBoundaryKind) -> Option<&'static str> {
    match kind {
        HookBoundaryKind::Stop => Some("stop boundary reached"),
        HookBoundaryKind::SubagentStop => Some("subagent stop boundary reached"),
        HookBoundaryKind::PreCompact => Some("pre-compact boundary reached"),
        HookBoundaryKind::SessionEnd => Some("session end boundary reached"),
        HookBoundaryKind::None => None,
    }
}

fn maybe_prepare_handoff_from_hooks(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    boundary_kind: HookBoundaryKind,
    host_budget: Option<u64>,
    config: &HookRuntimeConfig,
) -> Result<HookIngestResponse> {
    let effective_budget = config.effective_budget(host_budget);
    if boundary_kind != HookBoundaryKind::None {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_hook_boundary_at_unix = Some(now_unix_millis());
        task.latest_hook_boundary_kind = Some(format!("{boundary_kind:?}").to_ascii_lowercase());
        task.hook_soft_threshold_tokens = config
            .threshold_tokens_for_level_with_budget(ThresholdLevel::Prepare, effective_budget);
        persist_task(&guard, task_id)?;
    }
    let status = broker_task_status(
        state.clone(),
        BrokerTaskStatusRequest {
            task_id: task_id.to_string(),
        },
    )?;
    let snapshot = load_agent_snapshot_for_task(&state, task_id)?;
    let task = load_task_record(&state, task_id);

    // Compute graduated threshold level from accumulated tokens.
    let window_tokens = task.as_ref().map_or(0, |t| t.hook_window_est_tokens);
    let threshold_level = config.compute_threshold_level(window_tokens, effective_budget);
    let threshold_exceeded = matches!(
        threshold_level,
        ThresholdLevel::Prepare | ThresholdLevel::Force
    );
    let threshold_reason = if threshold_exceeded {
        Some(match threshold_level {
            ThresholdLevel::Force => "force context threshold reached",
            _ => "prepare context threshold reached",
        })
    } else {
        None
    };
    let boundary_reason = boundary_reason(boundary_kind);
    let should_prepare = snapshot.latest_intention.is_some()
        && (threshold_reason.is_some() || boundary_reason.is_some());

    let mut response = HookIngestResponse {
        task_id: task_id.to_string(),
        accepted: true,
        handoff_ready: status.handoff_ready,
        handoff_reason: status.handoff_reason.clone(),
        handoff: status.handoff.clone(),
        latest_handoff_artifact_id: status.latest_handoff_artifact_id.clone(),
        latest_context_version: status.latest_context_version.clone(),
        additional_context: None,
        block_stop: false,
        stop_reason: None,
        cache_hit: false,
        threshold_level,
        relaunch_requested: false,
        relaunch_preference: config.relaunch_preference,
    };

    if should_prepare {
        let prepared = broker_prepare_handoff(
            state.clone(),
            BrokerPrepareHandoffRequest {
                task_id: task_id.to_string(),
                query: None,
                response_mode: Some(BrokerResponseMode::Slim),
                include_debug_memory: false,
            },
        )?;
        response.handoff_ready = prepared.handoff_ready;
        response.handoff_reason = Some(prepared.handoff_reason.clone());
        response.handoff = prepared.handoff.clone();
        response.latest_handoff_artifact_id = prepared.latest_handoff_artifact_id.clone();
        response.latest_context_version = prepared
            .context
            .as_ref()
            .map(|context| context.context_version.clone())
            .or(status.latest_context_version);
        if prepared.handoff_ready {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = ensure_task_record_mut(&mut guard.tasks, task_id);
            task.latest_hook_handoff_reason = response.handoff_reason.clone();
            task.hook_threshold_exceeded = false;
            task.hook_window_est_tokens = 0;
            task.hook_window_est_bytes = 0;
            persist_task(&guard, task_id)?;

            // Auto-relaunch: only when the user explicitly opted into
            // daemon-managed relaunch with their own command, and only when
            // the *session* is ending. `SubagentStop` is deliberately excluded:
            // the parent session is still running, so relaunching there both
            // duplicates work and spawns a new agent per finished subagent.
            let is_stop_boundary = matches!(
                boundary_kind,
                HookBoundaryKind::Stop | HookBoundaryKind::SessionEnd
            );

            if is_stop_boundary && config.daemon_relaunch_enabled() {
                match guard
                    .background_tx
                    .try_send(BackgroundCommand::RelaunchAgent {
                        task_id: task_id.to_string(),
                        command: config.relaunch_command.clone(),
                    }) {
                    Ok(()) => response.relaunch_requested = true,
                    Err(error) => daemon_log(&format!(
                        "auto-relaunch queue rejected task {task_id}: {error}"
                    )),
                }
            }
        }
    } else if threshold_exceeded && snapshot.latest_intention.is_none() {
        response.block_stop = matches!(
            boundary_kind,
            HookBoundaryKind::Stop | HookBoundaryKind::SubagentStop
        );
        response.stop_reason = Some(
            "Packet28 threshold reached. Record the current task objective with packet28.write_intention before stopping."
                .to_string(),
        );
    } else if matches!(threshold_level, ThresholdLevel::Warn) && snapshot.latest_intention.is_none()
    {
        // At warn level, nudge the agent to record intent but don't block.
        response.stop_reason = Some(
            "Packet28 context usage at warn level. Consider recording intent with packet28.write_intention."
                .to_string(),
        );
    }
    Ok(response)
}

fn github_cache_ttl_secs() -> u64 {
    300
}

fn remote_state_cache_ttl_secs(family: &str, kind: &str) -> Option<u64> {
    match family {
        "github" => Some(github_cache_ttl_secs()),
        "infra"
            if kind.starts_with("aws_")
                || kind == "psql_query"
                || kind.starts_with("docker_")
                || kind.starts_with("docker_compose_")
                || kind.starts_with("kubectl_")
                || kind == "curl_fetch" =>
        {
            Some(github_cache_ttl_secs())
        }
        _ => None,
    }
}

fn lifecycle_kind(lifecycle: &packet28_daemon_protocol::hooks::HookLifecycleEvent) -> Option<&str> {
    lifecycle
        .canonical_command_kind
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_family(packet: &packet28_daemon_protocol::hooks::HookReducerPacket) -> Option<&str> {
    packet
        .reducer_family
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_kind(packet: &packet28_daemon_protocol::hooks::HookReducerPacket) -> Option<&str> {
    packet
        .canonical_command_kind
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_is_mutation(packet: &packet28_daemon_protocol::hooks::HookReducerPacket) -> bool {
    packet.mutation.unwrap_or(false)
        || matches!(
            packet_kind(packet),
            Some("git_add" | "git_commit" | "git_pull" | "git_switch" | "git_checkout")
        )
        || packet.operation_kind == suite_packet_core::ToolOperationKind::Edit
}

fn packet_touches_rust(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
    })
}

fn invalidate_epochs_for_packet(
    task: &mut TaskRecord,
    packet: &packet28_daemon_protocol::hooks::HookReducerPacket,
) {
    if packet.failed {
        return;
    }
    match packet_family(packet) {
        Some("git") if packet_is_mutation(packet) => {
            task.hook_git_epoch = task.hook_git_epoch.saturating_add(1);
            task.hook_fs_epoch = task.hook_fs_epoch.saturating_add(1);
            if packet_touches_rust(&packet.paths)
                || matches!(
                    packet_kind(packet),
                    Some("git_pull" | "git_switch" | "git_checkout")
                )
            {
                task.hook_rust_epoch = task.hook_rust_epoch.saturating_add(1);
            }
        }
        Some("rust") if packet_touches_rust(&packet.paths) || packet_is_mutation(packet) => {
            task.hook_rust_epoch = task.hook_rust_epoch.saturating_add(1);
        }
        Some("fs") if packet_is_mutation(packet) => {
            task.hook_fs_epoch = task.hook_fs_epoch.saturating_add(1);
            task.hook_git_epoch = task.hook_git_epoch.saturating_add(1);
            if packet_touches_rust(&packet.paths) {
                task.hook_rust_epoch = task.hook_rust_epoch.saturating_add(1);
            }
        }
        _ if packet_is_mutation(packet)
            || packet.operation_kind == suite_packet_core::ToolOperationKind::Edit =>
        {
            task.hook_fs_epoch = task.hook_fs_epoch.saturating_add(1);
            task.hook_git_epoch = task.hook_git_epoch.saturating_add(1);
            if packet_touches_rust(&packet.paths) {
                task.hook_rust_epoch = task.hook_rust_epoch.saturating_add(1);
            }
        }
        _ => {}
    }
}

fn cache_hit_for_packet(
    task: &TaskRecord,
    packet: &packet28_daemon_protocol::hooks::HookReducerPacket,
) -> bool {
    if packet_is_mutation(packet) {
        return false;
    }
    let Some(fingerprint) = packet.cache_fingerprint.as_deref() else {
        return false;
    };
    let Some(entry) = task.hook_reducer_cache.get(fingerprint) else {
        return false;
    };
    if entry.reducer_family != packet_family(packet).unwrap_or_default() {
        return false;
    }
    let packet_workspace_fingerprint = packet_workspace_fingerprint(packet);
    if packet_workspace_fingerprint.is_some()
        && entry.workspace_fingerprint.as_deref() != packet_workspace_fingerprint
    {
        return false;
    }
    if entry.git_epoch != task.hook_git_epoch
        || entry.fs_epoch != task.hook_fs_epoch
        || entry.rust_epoch != task.hook_rust_epoch
    {
        return false;
    }
    if let Some(ttl_secs) =
        remote_state_cache_ttl_secs(&entry.reducer_family, &entry.canonical_command_kind)
    {
        let age = now_unix().saturating_sub(entry.occurred_at_unix);
        return age <= ttl_secs;
    }
    true
}

fn update_cache_for_packet(
    task: &mut TaskRecord,
    packet: &packet28_daemon_protocol::hooks::HookReducerPacket,
    artifact_id: Option<String>,
) {
    if packet.cacheable != Some(true) {
        return;
    }
    if packet_is_mutation(packet) {
        return;
    }
    let Some(fingerprint) = packet.cache_fingerprint.as_ref() else {
        return;
    };
    let Some(family) = packet_family(packet) else {
        return;
    };
    let Some(kind) = packet_kind(packet) else {
        return;
    };
    task.hook_reducer_cache.insert(
        fingerprint.clone(),
        HookReducerCacheEntry {
            reducer_family: family.to_string(),
            canonical_command_kind: kind.to_string(),
            cache_fingerprint: fingerprint.clone(),
            workspace_fingerprint: packet_workspace_fingerprint(packet).map(ToOwned::to_owned),
            summary: packet.summary.clone(),
            compact_preview: packet.compact_preview.clone(),
            paths: packet.paths.clone(),
            regions: packet.regions.clone(),
            symbols: packet.symbols.clone(),
            artifact_id,
            raw_artifact_handle: packet.raw_artifact_handle.clone(),
            failed: packet.failed,
            error_message: packet.error_message.clone(),
            exit_code: packet.exit_code,
            occurred_at_unix: now_unix(),
            git_epoch: task.hook_git_epoch,
            fs_epoch: task.hook_fs_epoch,
            rust_epoch: task.hook_rust_epoch,
        },
    );
    prune_hook_reducer_cache(task);
}

/// Upper bound on retained per-task hook reducer cache entries.
///
/// A long-lived session can otherwise accumulate thousands of distinct
/// fingerprints, growing the task record past the paginated record-size limit
/// and poisoning registry listing (see the daemon pagination bound). The cache
/// is a best-effort dedup aid, so evicting the oldest entries is safe.
const HOOK_REDUCER_CACHE_MAX_ENTRIES: usize = 256;

/// Evicts the oldest hook reducer cache entries (by `occurred_at_unix`) once the
/// per-task cache exceeds [`HOOK_REDUCER_CACHE_MAX_ENTRIES`], keeping the most
/// recent entries. Ties are broken by fingerprint for deterministic pruning.
fn prune_hook_reducer_cache(task: &mut TaskRecord) {
    let len = task.hook_reducer_cache.len();
    if len <= HOOK_REDUCER_CACHE_MAX_ENTRIES {
        return;
    }
    let mut ordered: Vec<(u64, String)> = task
        .hook_reducer_cache
        .iter()
        .map(|(key, entry)| (entry.occurred_at_unix, key.clone()))
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let excess = len - HOOK_REDUCER_CACHE_MAX_ENTRIES;
    for (_, key) in ordered.into_iter().take(excess) {
        task.hook_reducer_cache.remove(&key);
    }
}

fn packet_workspace_fingerprint(
    packet: &packet28_daemon_protocol::hooks::HookReducerPacket,
) -> Option<&str> {
    packet
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.get("workspace_fingerprint"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn apply_lifecycle_event(
    task: &mut TaskRecord,
    lifecycle: &packet28_daemon_protocol::hooks::HookLifecycleEvent,
) {
    task.latest_hook_progress_at_unix = Some(now_unix());
    if let Some(command_id) = lifecycle.command_id.as_ref() {
        task.latest_hook_command_id = Some(command_id.clone());
    }
    if let Some(kind) = lifecycle_kind(lifecycle) {
        task.latest_hook_command_kind = Some(kind.to_string());
    }
}

pub(crate) fn hook_ingest(
    state: Arc<Mutex<DaemonState>>,
    request: HookIngestRequest,
) -> Result<HookIngestResponse> {
    let task_id = request.task_id.trim();
    if task_id.is_empty() {
        anyhow::bail!("hook ingest requires task_id");
    }
    let root = state.lock().map_err(lock_err)?.root.clone();
    let config = load_hook_runtime_config(&root)?;
    if !config.hooks_enabled {
        return Ok(HookIngestResponse {
            task_id: task_id.to_string(),
            accepted: false,
            ..HookIngestResponse::default()
        });
    }

    let effective_budget = config.effective_budget(request.host_context_budget_tokens);
    let prepare_threshold =
        config.threshold_tokens_for_level_with_budget(ThresholdLevel::Prepare, effective_budget);

    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_hook_session_id = request.session_id.clone();
        task.latest_hook_event_at_unix = Some(now_unix());
        task.hook_soft_threshold_tokens = prepare_threshold;
        if let Some(lifecycle) = request.lifecycle_event.as_ref() {
            apply_lifecycle_event(task, lifecycle);
        }
        persist_task(&guard, task_id)?;
    }

    let host_budget = request.host_context_budget_tokens;

    if matches!(request.event_kind, HookEventKind::SessionStart) {
        let additional_context =
            hook_task_additional_context(&state, task_id, request.session_id.as_deref())?;
        return Ok(HookIngestResponse {
            task_id: task_id.to_string(),
            accepted: true,
            additional_context,
            ..maybe_prepare_handoff_from_hooks(
                state,
                task_id,
                HookBoundaryKind::None,
                host_budget,
                &config,
            )?
        });
    }

    let mut cache_hit = false;
    if let Some(packet) = request.reducer_packet.as_ref() {
        let artifact_id = if let Some(artifact) = packet.artifact.as_ref() {
            fence_task_namespace_admission(&state, task_id)?;
            Some(store_hook_artifact(&root, task_id, "hook", artifact)?)
        } else {
            None
        };
        {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = ensure_task_record_mut(&mut guard.tasks, task_id);
            cache_hit = cache_hit_for_packet(task, packet);
            if !cache_hit {
                update_cache_for_packet(task, packet, artifact_id.clone());
            }
            invalidate_epochs_for_packet(task, packet);
            if let Some(kind) = packet_kind(packet) {
                task.latest_hook_command_kind = Some(kind.to_string());
            }
            persist_task(&guard, task_id)?;
        }

        if !cache_hit {
            let op = if packet.failed {
                BrokerWriteOp::ToolInvocationFailed
            } else {
                BrokerWriteOp::ToolResult
            };
            let request_summary = packet
                .command
                .clone()
                .or_else(|| packet.search_query.clone())
                .or_else(|| Some(packet.tool_name.clone()));
            let mut requests = vec![BrokerWriteStateRequest {
                task_id: task_id.to_string(),
                op: Some(op),
                tool_name: Some(packet.tool_name.clone()),
                operation_kind: Some(packet.operation_kind),
                request_summary,
                result_summary: Some(packet.summary.clone()),
                compact_preview: packet.compact_preview.clone(),
                request_fingerprint: packet.cache_fingerprint.clone(),
                compact_path: packet.compact_path.clone(),
                passthrough_reason: packet.passthrough_reason.clone(),
                raw_est_tokens: packet.raw_est_tokens,
                reduced_est_tokens: packet.reduced_est_tokens,
                search_query: packet.search_query.clone(),
                command: packet.command.clone(),
                paths: packet.paths.clone(),
                regions: packet.regions.clone(),
                symbols: packet.symbols.clone(),
                artifact_id,
                raw_artifact_handle: packet.raw_artifact_handle.clone(),
                raw_artifact_available: Some(packet.raw_artifact_available),
                duration_ms: packet.duration_ms,
                error_class: packet.error_class.clone(),
                error_message: packet.error_message.clone(),
                retryable: packet.retryable,
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            }];
            if !packet.failed && packet.operation_kind == suite_packet_core::ToolOperationKind::Read
            {
                requests.push(BrokerWriteStateRequest {
                    task_id: task_id.to_string(),
                    op: Some(BrokerWriteOp::FileRead),
                    paths: packet.paths.clone(),
                    symbols: packet.symbols.clone(),
                    regions: packet.regions.clone(),
                    refresh_context: Some(false),
                    ..BrokerWriteStateRequest::default()
                });
            }
            if !packet.failed
                && matches!(
                    packet.operation_kind,
                    suite_packet_core::ToolOperationKind::Edit
                        | suite_packet_core::ToolOperationKind::Diff
                )
            {
                requests.push(BrokerWriteStateRequest {
                    task_id: task_id.to_string(),
                    op: Some(BrokerWriteOp::FileEdit),
                    paths: packet.paths.clone(),
                    symbols: packet.symbols.clone(),
                    regions: packet.regions.clone(),
                    refresh_context: Some(false),
                    ..BrokerWriteStateRequest::default()
                });
            }
            let _ =
                broker_write_state_batch(state.clone(), BrokerWriteStateBatchRequest { requests })?;
            {
                let mut guard = state.lock().map_err(lock_err)?;
                let task = ensure_task_record_mut(&mut guard.tasks, task_id);
                task.hook_window_est_tokens = task
                    .hook_window_est_tokens
                    .saturating_add(packet.est_tokens);
                task.hook_window_est_bytes =
                    task.hook_window_est_bytes.saturating_add(packet.est_bytes);
                // Use graduated threshold: exceeded at Prepare level or above.
                task.hook_threshold_exceeded = task.hook_window_est_tokens >= prepare_threshold;
                persist_task(&guard, task_id)?;
            }
        }
    }

    let mut response = maybe_prepare_handoff_from_hooks(
        state,
        task_id,
        request.boundary_kind,
        host_budget,
        &config,
    )?;
    response.cache_hit = cache_hit;
    Ok(response)
}

#[cfg(test)]
#[path = "hooks/tests.rs"]
mod tests;
