use super::*;
use packet28_daemon_core::{
    hook_runtime_config_path, HookBoundaryKind, HookEventKind, HookIngestRequest,
    HookIngestResponse, HookReducerCacheEntry, HookRuntimeConfig, RelaunchPreference,
    ThresholdLevel,
};
use std::sync::atomic::{AtomicU64, Ordering};

static HOOK_ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn load_hook_runtime_config(root: &Path) -> HookRuntimeConfig {
    let path = hook_runtime_config_path(root);
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<HookRuntimeConfig>(&raw).ok())
        .unwrap_or_default()
}

fn store_hook_artifact(root: &Path, task_id: &str, prefix: &str, value: &Value) -> Result<String> {
    let dir = task_artifact_dir(root, task_id).join("hook-artifacts");
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
    let path = task_brief_markdown_path(&root, task_id);
    let brief = fs::read_to_string(path).ok();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_hook_bootstrap_context_version = latest_context_version;
        task.latest_hook_bootstrap_at_unix = Some(now_unix());
        task.latest_hook_session_id = session_id.map(ToOwned::to_owned);
        task.latest_agent_handoff_artifact_id = latest_handoff_artifact_id;
        persist_state(&guard)?;
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
) -> Result<HookIngestResponse> {
    let config = {
        let root = state.lock().map_err(lock_err)?.root.clone();
        load_hook_runtime_config(&root)
    };
    let effective_budget = config.effective_budget(host_budget);
    if boundary_kind != HookBoundaryKind::None {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_hook_boundary_at_unix = Some(now_unix());
        task.latest_hook_boundary_kind = Some(format!("{boundary_kind:?}").to_ascii_lowercase());
        task.hook_soft_threshold_tokens =
            config.threshold_tokens_for_level_with_budget(ThresholdLevel::Prepare, effective_budget);
        persist_state(&guard)?;
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
            },
        )?;
        response.handoff_ready = prepared.handoff_ready;
        response.handoff_reason = Some(prepared.handoff_reason.clone());
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
            persist_state(&guard)?;

            // Auto-relaunch: when daemon-managed and at a stop boundary with
            // handoff ready, queue a fresh worker launch.
            let is_stop_boundary = matches!(
                boundary_kind,
                HookBoundaryKind::Stop
                    | HookBoundaryKind::SubagentStop
                    | HookBoundaryKind::SessionEnd
            );
            if is_stop_boundary
                && matches!(config.relaunch_preference, RelaunchPreference::DaemonManaged)
                && !config.relaunch_command.is_empty()
            {
                response.relaunch_requested = true;
                let relaunch_task_id = task_id.to_string();
                let relaunch_command = config.relaunch_command.clone();
                let relaunch_state = state.clone();
                thread::spawn(move || {
                    // Brief delay to let the current session complete its stop.
                    thread::sleep(Duration::from_millis(500));
                    let result = task_launch_agent(
                        relaunch_state,
                        TaskLaunchAgentRequest {
                            task_id: relaunch_task_id.clone(),
                            task: None,
                            wait_for_handoff: false,
                            handoff_timeout_ms: None,
                            handoff_poll_ms: None,
                            command: relaunch_command,
                        },
                    );
                    match result {
                        Ok(launched) => {
                            eprintln!(
                                "packet28: auto-relaunched agent pid={} task={}",
                                launched.pid, relaunch_task_id
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "packet28: auto-relaunch failed for task {}: {err:#}",
                                relaunch_task_id
                            );
                        }
                    }
                });
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
    } else if matches!(threshold_level, ThresholdLevel::Warn)
        && snapshot.latest_intention.is_none()
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

fn lifecycle_kind(lifecycle: &packet28_daemon_core::HookLifecycleEvent) -> Option<&str> {
    lifecycle
        .canonical_command_kind
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_family(packet: &packet28_daemon_core::HookReducerPacket) -> Option<&str> {
    packet
        .reducer_family
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_kind(packet: &packet28_daemon_core::HookReducerPacket) -> Option<&str> {
    packet
        .canonical_command_kind
        .as_deref()
        .filter(|value| !value.trim().is_empty())
}

fn packet_is_mutation(packet: &packet28_daemon_core::HookReducerPacket) -> bool {
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
    packet: &packet28_daemon_core::HookReducerPacket,
) {
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
        _ if packet.operation_kind == suite_packet_core::ToolOperationKind::Edit => {
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
    packet: &packet28_daemon_core::HookReducerPacket,
) -> bool {
    let Some(fingerprint) = packet.cache_fingerprint.as_deref() else {
        return false;
    };
    let Some(entry) = task.hook_reducer_cache.get(fingerprint) else {
        return false;
    };
    if entry.reducer_family != packet_family(packet).unwrap_or_default() {
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
        return age <= github_cache_ttl_secs();
    }
    true
}

fn update_cache_for_packet(
    task: &mut TaskRecord,
    packet: &packet28_daemon_core::HookReducerPacket,
    artifact_id: Option<String>,
) {
    if packet.cacheable != Some(true) {
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
            summary: packet.summary.clone(),
            paths: packet.paths.clone(),
            regions: packet.regions.clone(),
            symbols: packet.symbols.clone(),
            artifact_id,
            raw_artifact_handle: packet.raw_artifact_handle.clone(),
            occurred_at_unix: now_unix(),
            git_epoch: task.hook_git_epoch,
            fs_epoch: task.hook_fs_epoch,
            rust_epoch: task.hook_rust_epoch,
        },
    );
}

fn apply_lifecycle_event(
    task: &mut TaskRecord,
    lifecycle: &packet28_daemon_core::HookLifecycleEvent,
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
    let config = load_hook_runtime_config(&root);
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
        persist_state(&guard)?;
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
            )?
        });
    }

    let mut cache_hit = false;
    if let Some(packet) = request.reducer_packet.as_ref() {
        let artifact_id = packet
            .artifact
            .as_ref()
            .map(|artifact| store_hook_artifact(&root, task_id, "hook", artifact))
            .transpose()?;
        {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = ensure_task_record_mut(&mut guard.tasks, task_id);
            cache_hit = cache_hit_for_packet(task, packet);
            if !cache_hit {
                update_cache_for_packet(task, packet, artifact_id.clone());
            }
            invalidate_epochs_for_packet(task, packet);
            persist_state(&guard)?;
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
                request_fingerprint: packet.cache_fingerprint.clone(),
                search_query: packet.search_query.clone(),
                command: packet.command.clone(),
                paths: packet.paths.clone(),
                regions: packet.regions.clone(),
                symbols: packet.symbols.clone(),
                artifact_id: artifact_id.clone(),
                duration_ms: packet.duration_ms,
                error_class: packet.error_class.clone(),
                error_message: packet.error_message.clone(),
                retryable: packet.retryable,
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            }];
            if packet.operation_kind == suite_packet_core::ToolOperationKind::Read {
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
            if matches!(
                packet.operation_kind,
                suite_packet_core::ToolOperationKind::Edit
                    | suite_packet_core::ToolOperationKind::Diff
            ) {
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
                persist_state(&guard)?;
            }
        }
    }

    let mut response =
        maybe_prepare_handoff_from_hooks(state, task_id, request.boundary_kind, host_budget)?;
    response.cache_hit = cache_hit;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_state() -> Arc<Mutex<DaemonState>> {
        let root = std::env::temp_dir().join(format!(
            "packet28-hook-test-{}-{}",
            now_unix_millis(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        ensure_daemon_dir(&root).unwrap();
        let kernel = Arc::new(Kernel::with_v1_reducers_and_persistence(
            PersistConfig::new(root.clone()),
        ));
        let (index_tx, index_rx) = mpsc::channel();
        thread::spawn(move || while index_rx.recv().is_ok() {});
        Arc::new(Mutex::new(DaemonState {
            root,
            kernel,
            runtime: DaemonRuntimeInfo::default(),
            tasks: TaskRegistry::default(),
            agent_snapshots: BTreeMap::new(),
            watches: WatchRegistry::default(),
            watcher_handles: HashMap::new(),
            subscribers: HashMap::new(),
            source_file_cache: BTreeMap::new(),
            interactive_index: InteractiveIndexRuntime::default(),
            index_tx,
            shutting_down: false,
        }))
    }

    fn packet(summary: &str) -> packet28_daemon_core::HookReducerPacket {
        packet28_daemon_core::HookReducerPacket {
            packet_type: "packet28.hook.fs.v2".to_string(),
            tool_name: "Bash".to_string(),
            operation_kind: suite_packet_core::ToolOperationKind::Read,
            reducer_family: Some("fs".to_string()),
            canonical_command_kind: Some("fs_cat".to_string()),
            summary: summary.to_string(),
            command: Some("cat src/lib.rs".to_string()),
            search_query: None,
            paths: vec!["src/lib.rs".to_string()],
            regions: vec!["src/lib.rs:1-3".to_string()],
            symbols: Vec::new(),
            equivalence_key: Some("read:src/lib.rs".to_string()),
            est_tokens: 10,
            est_bytes: 40,
            failed: false,
            error_class: None,
            error_message: None,
            retryable: Some(false),
            duration_ms: Some(12),
            exit_code: Some(0),
            cache_fingerprint: Some("fs:fs_cat:src/lib.rs".to_string()),
            cacheable: Some(true),
            mutation: Some(false),
            raw_artifact_handle: None,
            artifact: None,
        }
    }

    #[test]
    fn duplicate_cached_packet_does_not_grow_hook_window() {
        let state = test_state();
        let first = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-cache".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(!first.cache_hit);

        let second = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-cache".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(second.cache_hit);

        let task = load_task_record(&state, "task-cache").unwrap();
        assert_eq!(task.hook_window_est_tokens, 10);
        assert_eq!(task.hook_window_est_bytes, 40);
    }

    #[test]
    fn edit_invalidation_busts_fs_cache() {
        let state = test_state();
        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-edit".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        let cached = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-edit".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(cached.cache_hit);

        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-edit".to_string(),
                reducer_packet: Some(packet28_daemon_core::HookReducerPacket {
                    packet_type: "packet28.hook.edit.v1".to_string(),
                    tool_name: "Edit".to_string(),
                    operation_kind: suite_packet_core::ToolOperationKind::Edit,
                    reducer_family: Some("claude_native".to_string()),
                    canonical_command_kind: Some("edit".to_string()),
                    summary: "edited src/lib.rs".to_string(),
                    command: None,
                    search_query: None,
                    paths: vec!["src/lib.rs".to_string()],
                    regions: vec!["src/lib.rs:1-1".to_string()],
                    symbols: Vec::new(),
                    equivalence_key: None,
                    est_tokens: 5,
                    est_bytes: 20,
                    failed: false,
                    error_class: None,
                    error_message: None,
                    retryable: Some(false),
                    duration_ms: Some(5),
                    exit_code: Some(0),
                    cache_fingerprint: None,
                    cacheable: Some(false),
                    mutation: Some(true),
                    raw_artifact_handle: None,
                    artifact: None,
                }),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();

        let after_edit = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-edit".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(!after_edit.cache_hit);
    }

    #[test]
    fn edit_invalidation_busts_git_cache() {
        let state = test_state();
        let git_packet = packet28_daemon_core::HookReducerPacket {
            packet_type: "packet28.hook.git.v2".to_string(),
            tool_name: "Bash".to_string(),
            operation_kind: suite_packet_core::ToolOperationKind::Git,
            reducer_family: Some("git".to_string()),
            canonical_command_kind: Some("git_status".to_string()),
            summary: "git status reported 1 changed entry".to_string(),
            command: Some("git status --short src/lib.rs".to_string()),
            search_query: None,
            paths: vec!["src/lib.rs".to_string()],
            regions: Vec::new(),
            symbols: Vec::new(),
            equivalence_key: None,
            est_tokens: 8,
            est_bytes: 32,
            failed: false,
            error_class: None,
            error_message: None,
            retryable: Some(false),
            duration_ms: Some(5),
            exit_code: Some(0),
            cache_fingerprint: Some(
                "git:git_status:git\u{1f}status\u{1f}--short\u{1f}src/lib.rs".to_string(),
            ),
            cacheable: Some(true),
            mutation: Some(false),
            raw_artifact_handle: None,
            artifact: None,
        };
        let first = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-git-edit".to_string(),
                reducer_packet: Some(git_packet.clone()),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(!first.cache_hit);

        let second = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-git-edit".to_string(),
                reducer_packet: Some(git_packet.clone()),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(second.cache_hit);

        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-git-edit".to_string(),
                reducer_packet: Some(packet28_daemon_core::HookReducerPacket {
                    packet_type: "packet28.hook.edit.v1".to_string(),
                    tool_name: "Edit".to_string(),
                    operation_kind: suite_packet_core::ToolOperationKind::Edit,
                    reducer_family: Some("claude_native".to_string()),
                    canonical_command_kind: Some("edit".to_string()),
                    summary: "edited src/lib.rs".to_string(),
                    command: None,
                    search_query: None,
                    paths: vec!["src/lib.rs".to_string()],
                    regions: vec!["src/lib.rs:1-1".to_string()],
                    symbols: Vec::new(),
                    equivalence_key: None,
                    est_tokens: 5,
                    est_bytes: 20,
                    failed: false,
                    error_class: None,
                    error_message: None,
                    retryable: Some(false),
                    duration_ms: Some(5),
                    exit_code: Some(0),
                    cache_fingerprint: None,
                    cacheable: Some(false),
                    mutation: Some(true),
                    raw_artifact_handle: None,
                    artifact: None,
                }),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();

        let after_edit = hook_ingest(
            state,
            HookIngestRequest {
                task_id: "task-git-edit".to_string(),
                reducer_packet: Some(git_packet),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert!(!after_edit.cache_hit);
    }

    #[test]
    fn successful_handoff_preparation_clears_hook_window() {
        let state = test_state();
        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-handoff-reset".to_string(),
                reducer_packet: Some(packet("first read")),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        let _ = broker_write_state(
            state.clone(),
            BrokerWriteStateRequest {
                task_id: "task-handoff-reset".to_string(),
                op: Some(BrokerWriteOp::Intention),
                text: Some("Investigate hook handoff reset".to_string()),
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            },
        )
        .unwrap();

        let response = maybe_prepare_handoff_from_hooks(
            state.clone(),
            "task-handoff-reset",
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();

        assert!(response.handoff_ready);
        let task = load_task_record(&state, "task-handoff-reset").unwrap();
        assert_eq!(task.hook_window_est_tokens, 0);
        assert_eq!(task.hook_window_est_bytes, 0);
        assert!(!task.hook_threshold_exceeded);
    }

    #[test]
    fn graduated_threshold_levels_are_computed_correctly() {
        let config = HookRuntimeConfig {
            context_budget_tokens: 1000,
            warn_threshold_fraction: 0.6,
            prepare_threshold_fraction: 0.75,
            force_threshold_fraction: 0.9,
            ..HookRuntimeConfig::default()
        };
        assert_eq!(
            config.compute_threshold_level(0, 1000),
            ThresholdLevel::Normal
        );
        assert_eq!(
            config.compute_threshold_level(599, 1000),
            ThresholdLevel::Normal
        );
        assert_eq!(
            config.compute_threshold_level(600, 1000),
            ThresholdLevel::Warn
        );
        assert_eq!(
            config.compute_threshold_level(749, 1000),
            ThresholdLevel::Warn
        );
        assert_eq!(
            config.compute_threshold_level(750, 1000),
            ThresholdLevel::Prepare
        );
        assert_eq!(
            config.compute_threshold_level(899, 1000),
            ThresholdLevel::Prepare
        );
        assert_eq!(
            config.compute_threshold_level(900, 1000),
            ThresholdLevel::Force
        );
        assert_eq!(
            config.compute_threshold_level(1500, 1000),
            ThresholdLevel::Force
        );
    }

    #[test]
    fn host_budget_override_changes_effective_budget() {
        let config = HookRuntimeConfig {
            context_budget_tokens: 1000,
            ..HookRuntimeConfig::default()
        };
        assert_eq!(config.effective_budget(None), 1000);
        assert_eq!(config.effective_budget(Some(5000)), 5000);
        // Zero is ignored (falls back to config).
        assert_eq!(config.effective_budget(Some(0)), 1000);
    }

    #[test]
    fn threshold_accumulation_triggers_exceeded_without_stop_boundary() {
        let state = test_state();
        // Write a hook runtime config with small budget so threshold fires.
        let root = state.lock().unwrap().root.clone();
        let config = HookRuntimeConfig {
            context_budget_tokens: 100,
            warn_threshold_fraction: 0.6,
            prepare_threshold_fraction: 0.75,
            force_threshold_fraction: 0.9,
            ..HookRuntimeConfig::default()
        };
        let config_path = packet28_daemon_core::hook_runtime_config_path(&root);
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        // Ingest packets totaling 80 tokens (above prepare=75 threshold).
        for i in 0..8 {
            let mut pkt = packet(&format!("read {i}"));
            pkt.est_tokens = 10;
            pkt.cache_fingerprint = Some(format!("unique-fp-{i}"));
            let _ = hook_ingest(
                state.clone(),
                HookIngestRequest {
                    task_id: "task-threshold".to_string(),
                    reducer_packet: Some(pkt),
                    ..HookIngestRequest::default()
                },
            )
            .unwrap();
        }

        let task = load_task_record(&state, "task-threshold").unwrap();
        assert_eq!(task.hook_window_est_tokens, 80);
        assert!(task.hook_threshold_exceeded);

        // Without intention, stop should be blocked.
        let response = maybe_prepare_handoff_from_hooks(
            state.clone(),
            "task-threshold",
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(response.block_stop);
        assert!(!response.handoff_ready);

        // Write intention, then handoff should fire without a boundary.
        let _ = broker_write_state(
            state.clone(),
            BrokerWriteStateRequest {
                task_id: "task-threshold".to_string(),
                op: Some(BrokerWriteOp::Intention),
                text: Some("Continue investigating".to_string()),
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            },
        )
        .unwrap();

        // Now at Stop boundary with intention → handoff should be ready.
        let response = maybe_prepare_handoff_from_hooks(
            state.clone(),
            "task-threshold",
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(response.handoff_ready);
        assert!(matches!(
            response.threshold_level,
            ThresholdLevel::Prepare | ThresholdLevel::Force
        ));

        // Window should be cleared after successful handoff.
        let task = load_task_record(&state, "task-threshold").unwrap();
        assert_eq!(task.hook_window_est_tokens, 0);
        assert!(!task.hook_threshold_exceeded);
    }

    #[test]
    fn threshold_level_returned_in_response() {
        let state = test_state();
        let root = state.lock().unwrap().root.clone();
        let config = HookRuntimeConfig {
            context_budget_tokens: 100,
            warn_threshold_fraction: 0.6,
            prepare_threshold_fraction: 0.75,
            force_threshold_fraction: 0.9,
            ..HookRuntimeConfig::default()
        };
        let config_path = packet28_daemon_core::hook_runtime_config_path(&root);
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        // Under warn threshold.
        let mut pkt = packet("small read");
        pkt.est_tokens = 50;
        pkt.cache_fingerprint = Some("unique-level-1".to_string());
        let response = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-level".to_string(),
                reducer_packet: Some(pkt),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(response.threshold_level, ThresholdLevel::Normal);

        // Push past warn (60 = 0.6 * 100) → total 65 tokens.
        let mut pkt2 = packet("more read");
        pkt2.est_tokens = 15;
        pkt2.cache_fingerprint = Some("unique-level-2".to_string());
        let response = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-level".to_string(),
                reducer_packet: Some(pkt2),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(response.threshold_level, ThresholdLevel::Warn);

        // Push past force (90 = 0.9 * 100) → total 95 tokens.
        let mut pkt3 = packet("big read");
        pkt3.est_tokens = 30;
        pkt3.cache_fingerprint = Some("unique-level-3".to_string());
        let response = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-level".to_string(),
                reducer_packet: Some(pkt3),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(response.threshold_level, ThresholdLevel::Force);
    }

    #[test]
    fn host_budget_override_affects_threshold_calculation() {
        let state = test_state();
        // Default config has budget=200_000 so threshold is very high.
        // But host override sets budget=100 → threshold fires at 75 tokens.
        let mut pkt = packet("big read");
        pkt.est_tokens = 80;
        pkt.cache_fingerprint = Some("unique-host-1".to_string());
        let response = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-host-budget".to_string(),
                reducer_packet: Some(pkt),
                host_context_budget_tokens: Some(100),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        // 80 >= 75 (0.75 * 100), so threshold is exceeded at Prepare level.
        assert!(matches!(
            response.threshold_level,
            ThresholdLevel::Prepare | ThresholdLevel::Force
        ));

        let task = load_task_record(&state, "task-host-budget").unwrap();
        assert!(task.hook_threshold_exceeded);
    }

    #[test]
    fn relaunch_preference_daemon_managed_is_default() {
        let config = HookRuntimeConfig::default();
        assert_eq!(config.relaunch_preference, RelaunchPreference::DaemonManaged);
    }

    #[test]
    fn relaunch_requested_when_daemon_managed_with_command() {
        let state = test_state();
        let root = state.lock().unwrap().root.clone();
        let config = HookRuntimeConfig {
            context_budget_tokens: 100,
            relaunch_preference: RelaunchPreference::DaemonManaged,
            // Use a harmless command that will fail quickly (fine for test).
            relaunch_command: vec!["true".to_string()],
            ..HookRuntimeConfig::default()
        };
        let config_path = packet28_daemon_core::hook_runtime_config_path(&root);
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        // Ingest enough to exceed threshold.
        let mut pkt = packet("big read");
        pkt.est_tokens = 80;
        pkt.cache_fingerprint = Some("unique-relaunch-1".to_string());
        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-relaunch".to_string(),
                reducer_packet: Some(pkt),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();

        // Write intention so handoff can proceed.
        let _ = broker_write_state(
            state.clone(),
            BrokerWriteStateRequest {
                task_id: "task-relaunch".to_string(),
                op: Some(BrokerWriteOp::Intention),
                text: Some("Continue work".to_string()),
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            },
        )
        .unwrap();

        // Stop boundary should trigger handoff + relaunch.
        let response = maybe_prepare_handoff_from_hooks(
            state.clone(),
            "task-relaunch",
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(response.handoff_ready);
        assert!(response.relaunch_requested);
        assert_eq!(response.relaunch_preference, RelaunchPreference::DaemonManaged);
    }

    /// End-to-end test: hook ingest → graduated thresholds → intention write
    /// → stop boundary handoff → window reset → brief artifact persisted.
    #[test]
    fn e2e_hook_threshold_handoff_cycle() {
        let state = test_state();
        let root = state.lock().unwrap().root.clone();
        let config = HookRuntimeConfig {
            context_budget_tokens: 100,
            warn_threshold_fraction: 0.6,
            prepare_threshold_fraction: 0.75,
            force_threshold_fraction: 0.9,
            relaunch_preference: RelaunchPreference::DaemonManaged,
            relaunch_command: vec!["true".to_string()],
            ..HookRuntimeConfig::default()
        };
        let config_path = packet28_daemon_core::hook_runtime_config_path(&root);
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let task_id = "task-e2e-cycle";

        // Phase 1: Ingest hooks, observe graduated threshold levels.
        // 30 tokens → Normal.
        let mut pkt1 = packet("read file A");
        pkt1.est_tokens = 30;
        pkt1.cache_fingerprint = Some("e2e-fp-1".to_string());
        let r1 = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: task_id.to_string(),
                reducer_packet: Some(pkt1),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(r1.threshold_level, ThresholdLevel::Normal);
        assert!(!r1.handoff_ready);

        // 65 tokens total → Warn.
        let mut pkt2 = packet("read file B");
        pkt2.est_tokens = 35;
        pkt2.cache_fingerprint = Some("e2e-fp-2".to_string());
        let r2 = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: task_id.to_string(),
                reducer_packet: Some(pkt2),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(r2.threshold_level, ThresholdLevel::Warn);

        // 95 tokens total → Force.
        let mut pkt3 = packet("read file C");
        pkt3.est_tokens = 30;
        pkt3.cache_fingerprint = Some("e2e-fp-3".to_string());
        let r3 = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: task_id.to_string(),
                reducer_packet: Some(pkt3),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();
        assert_eq!(r3.threshold_level, ThresholdLevel::Force);

        // Phase 2: Stop without intention → blocked.
        let blocked = maybe_prepare_handoff_from_hooks(
            state.clone(),
            task_id,
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(blocked.block_stop);
        assert!(!blocked.handoff_ready);

        // Phase 3: Write intention.
        let _ = broker_write_state(
            state.clone(),
            BrokerWriteStateRequest {
                task_id: task_id.to_string(),
                op: Some(BrokerWriteOp::Intention),
                text: Some("Refactor auth middleware for compliance".to_string()),
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            },
        )
        .unwrap();

        // Phase 4: Stop with intention → handoff fires, relaunch queued.
        let handoff = maybe_prepare_handoff_from_hooks(
            state.clone(),
            task_id,
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(handoff.handoff_ready);
        assert!(handoff.relaunch_requested);
        assert_eq!(handoff.relaunch_preference, RelaunchPreference::DaemonManaged);
        assert!(matches!(
            handoff.threshold_level,
            ThresholdLevel::Force
        ));

        // Phase 5: Verify window reset.
        let task = load_task_record(&state, task_id).unwrap();
        assert_eq!(task.hook_window_est_tokens, 0);
        assert_eq!(task.hook_window_est_bytes, 0);
        assert!(!task.hook_threshold_exceeded);

        // Phase 6: Verify brief artifact was persisted.
        let brief_path = crate::task_brief_markdown_path(&root, task_id);
        assert!(brief_path.exists(), "brief.md should be written after handoff");
        let brief_content = std::fs::read_to_string(&brief_path).unwrap();
        assert!(!brief_content.is_empty(), "brief should not be empty");
    }

    #[test]
    fn relaunch_not_requested_when_host_managed() {
        let state = test_state();
        let root = state.lock().unwrap().root.clone();
        let config = HookRuntimeConfig {
            context_budget_tokens: 100,
            relaunch_preference: RelaunchPreference::HostManaged,
            relaunch_command: vec!["true".to_string()],
            ..HookRuntimeConfig::default()
        };
        let config_path = packet28_daemon_core::hook_runtime_config_path(&root);
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let mut pkt = packet("big read");
        pkt.est_tokens = 80;
        pkt.cache_fingerprint = Some("unique-host-managed-1".to_string());
        let _ = hook_ingest(
            state.clone(),
            HookIngestRequest {
                task_id: "task-host-managed".to_string(),
                reducer_packet: Some(pkt),
                ..HookIngestRequest::default()
            },
        )
        .unwrap();

        let _ = broker_write_state(
            state.clone(),
            BrokerWriteStateRequest {
                task_id: "task-host-managed".to_string(),
                op: Some(BrokerWriteOp::Intention),
                text: Some("Continue work".to_string()),
                refresh_context: Some(false),
                ..BrokerWriteStateRequest::default()
            },
        )
        .unwrap();

        let response = maybe_prepare_handoff_from_hooks(
            state.clone(),
            "task-host-managed",
            HookBoundaryKind::Stop,
            None,
        )
        .unwrap();
        assert!(response.handoff_ready);
        assert!(!response.relaunch_requested);
        assert_eq!(response.relaunch_preference, RelaunchPreference::HostManaged);
    }
}
