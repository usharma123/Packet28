use super::*;

pub(crate) fn kernel_for_request(
    state: &Arc<Mutex<DaemonState>>,
    request: &KernelRequest,
) -> Result<Kernel> {
    if let Some(root) = persist_root_override(&request.target, &request.policy_context) {
        return Ok(Kernel::with_v1_reducers_and_persistence(
            PersistConfig::new(resolve_root(Path::new(&root))),
        ));
    }

    Ok(Kernel::with_v1_reducers_and_persistence(
        PersistConfig::new(state.lock().map_err(lock_err)?.root.clone()),
    ))
}

fn persist_root_override(target: &str, policy_context: &Value) -> Option<String> {
    if !matches!(target, "agenty.state.write" | "agenty.state.snapshot") {
        return None;
    }

    policy_context
        .get("persist_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn build_status(state: &DaemonState) -> Result<DaemonStatus> {
    Ok(DaemonStatus {
        pid: state.runtime.pid,
        socket_path: state.runtime.socket_path.clone(),
        workspace_root: state.runtime.workspace_root.clone(),
        started_at_unix: state.runtime.started_at_unix,
        ready_at_unix: state.runtime.ready_at_unix,
        log_path: state.runtime.log_path.clone(),
        uptime_secs: now_unix().saturating_sub(state.runtime.started_at_unix),
        tasks: state.tasks.tasks.values().cloned().collect(),
        watches: state.watches.watches.clone(),
        index: Some(build_index_status(&state.interactive_index)),
    })
}

pub(crate) fn emit_task_event(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    kind: &str,
    data: Value,
) -> Result<()> {
    let (root, frame, subscribers) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = guard
            .tasks
            .tasks
            .entry(task_id.to_string())
            .or_insert_with(|| TaskRecord {
                task_id: task_id.to_string(),
                ..TaskRecord::default()
            });
        task.last_event_seq = task.last_event_seq.saturating_add(1);
        let frame = DaemonEventFrame {
            seq: task.last_event_seq,
            task_id: task_id.to_string(),
            event: DaemonEvent {
                kind: kind.to_string(),
                occurred_at_unix: now_unix(),
                data,
            },
        };
        let subscribers = guard.subscribers.get(task_id).cloned().unwrap_or_default();
        (guard.root.clone(), frame, subscribers)
    };
    append_task_event(&root, &frame)?;
    let mut still_open = Vec::new();
    for subscriber in subscribers {
        if subscriber.send(frame.clone()).is_ok() {
            still_open.push(subscriber);
        }
    }
    let mut guard = state.lock().map_err(lock_err)?;
    if still_open.is_empty() {
        guard.subscribers.remove(task_id);
    } else {
        guard.subscribers.insert(task_id.to_string(), still_open);
    }
    persist_state(&guard)?;
    Ok(())
}

pub(crate) fn refresh_task_context_summary(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<Option<Value>> {
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let response = match kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": task_id,
            "budget_tokens": DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS,
            "budget_bytes": DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES,
            "scope": "task_first",
        }),
        ..KernelRequest::default()
    }) {
        Ok(response) => response,
        Err(err) => {
            daemon_log(&format!(
                "context manage refresh failed task_id={task_id}: {err}"
            ));
            return Ok(None);
        }
    };
    let Some(packet) = response.output_packets.first() else {
        return Ok(None);
    };
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!(source.to_string()))?;
    let summary = json!({
        "working_set_tokens": envelope.payload.budget.working_set_tokens,
        "evictable_tokens": envelope.payload.budget.evictable_tokens,
        "changed_paths_since_checkpoint": envelope.payload.changed_paths_since_checkpoint.len(),
        "changed_symbols_since_checkpoint": envelope.payload.changed_symbols_since_checkpoint.len(),
    });
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(task) = guard.tasks.tasks.get_mut(task_id) {
        task.last_context_refresh_at_unix = Some(now_unix());
        task.working_set_est_tokens = envelope.payload.budget.working_set_tokens;
        task.evictable_est_tokens = envelope.payload.budget.evictable_tokens;
        task.changed_since_checkpoint_paths = envelope.payload.changed_paths_since_checkpoint.len();
        task.changed_since_checkpoint_symbols =
            envelope.payload.changed_symbols_since_checkpoint.len();
    }
    persist_state(&guard)?;
    Ok(Some(summary))
}

pub(crate) fn broker_default_budget_tokens() -> u64 {
    DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS
}

pub(crate) fn broker_default_budget_bytes() -> usize {
    DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES
}

pub(crate) fn ensure_task_record_mut<'a>(
    tasks: &'a mut TaskRegistry,
    task_id: &str,
) -> &'a mut TaskRecord {
    tasks
        .tasks
        .entry(task_id.to_string())
        .or_insert_with(|| TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        })
}

fn next_context_version(current: Option<&str>) -> String {
    current
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1)
        .to_string()
}

pub(crate) fn ensure_context_version(task: &mut TaskRecord) -> String {
    let version = task
        .latest_context_version
        .clone()
        .unwrap_or_else(|| next_context_version(None));
    task.latest_context_version = Some(version.clone());
    version
}

pub(crate) fn bump_context_version(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, task_id);
    let version = next_context_version(task.latest_context_version.as_deref());
    task.latest_context_version = Some(version.clone());
    persist_state(&guard)?;
    Ok(version)
}

pub(crate) fn set_context_reason(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, task_id);
    task.latest_context_reason = Some(reason.into());
    persist_state(&guard)?;
    Ok(())
}

pub(crate) fn current_context_version(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let version = ensure_context_version(ensure_task_record_mut(&mut guard.tasks, task_id));
    persist_state(&guard)?;
    Ok(version)
}

pub(crate) fn update_broker_link_state(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerWriteStateRequest,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
    let mut changed = false;
    match request.op.unwrap_or(BrokerWriteOp::FileRead) {
        BrokerWriteOp::QuestionOpen => {
            if let (Some(question_id), Some(text)) = (&request.question_id, &request.text) {
                task.question_texts
                    .insert(question_id.clone(), text.clone());
                task.resolved_questions.remove(question_id);
                changed = true;
            }
        }
        BrokerWriteOp::QuestionResolve => {
            if let Some(question_id) = &request.question_id {
                task.question_texts
                    .entry(question_id.clone())
                    .or_insert_with(|| "resolved question".to_string());
                if let Some(decision_id) = &request.resolution_decision_id {
                    task.resolved_questions
                        .insert(question_id.clone(), decision_id.clone());
                    task.linked_decisions
                        .insert(decision_id.clone(), question_id.clone());
                } else {
                    task.resolved_questions
                        .entry(question_id.clone())
                        .or_insert_with(String::new);
                }
                changed = true;
            }
        }
        BrokerWriteOp::DecisionAdd => {
            if let (Some(decision_id), Some(question_id)) =
                (&request.decision_id, &request.resolves_question_id)
            {
                task.linked_decisions
                    .insert(decision_id.clone(), question_id.clone());
                task.resolved_questions
                    .insert(question_id.clone(), decision_id.clone());
                changed = true;
            }
        }
        BrokerWriteOp::DecisionSupersede => {
            if let Some(decision_id) = &request.decision_id {
                task.linked_decisions.remove(decision_id);
                task.resolved_questions
                    .retain(|_, linked_decision_id| linked_decision_id != decision_id);
                changed = true;
            }
        }
        _ => {}
    }
    if changed {
        persist_state(&guard)?;
    }
    Ok(())
}

pub(crate) fn load_agent_snapshot_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<suite_packet_core::AgentSnapshotPayload> {
    if let Some(snapshot) = state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .get(task_id)
        .cloned()
    {
        return Ok(snapshot);
    }
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let response = kernel.execute(KernelRequest {
        target: "agenty.state.snapshot".to_string(),
        reducer_input: json!({ "task_id": task_id }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no agent snapshot packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid agent snapshot packet: {source}"))?;
    let snapshot = envelope.payload;
    state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .insert(task_id.to_string(), snapshot.clone());
    Ok(snapshot)
}

pub(crate) fn load_context_manage_for_task(
    kernel: &Arc<context_kernel_core::Kernel>,
    request: &BrokerGetContextRequest,
    focus_paths: &[String],
    focus_symbols: &[String],
) -> Result<suite_packet_core::ContextManagePayload> {
    let response = kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": request.task_id,
            "query": request.query,
            "budget_tokens": request.budget_tokens.unwrap_or_else(broker_default_budget_tokens),
            "budget_bytes": request.budget_bytes.unwrap_or_else(broker_default_budget_bytes),
            "scope": "task_first",
            "focus_paths": focus_paths,
            "focus_symbols": focus_symbols,
        }),
        policy_context: json!({
            "task_id": request.task_id,
        }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no context manage packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid context manage packet: {source}"))?;
    Ok(envelope.payload)
}

pub(crate) fn metadata_mtime_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn build_repo_map_envelope(
    root: &Path,
    focus_paths: &[String],
    focus_symbols: &[String],
    max_files: usize,
    max_symbols: usize,
) -> Result<suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>> {
    mapy_core::build_repo_map(mapy_core::RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        focus_paths: focus_paths.to_vec(),
        focus_symbols: focus_symbols.to_vec(),
        max_files,
        max_symbols,
        include_tests: true,
    })
    .map_err(|source| anyhow!(source.to_string()))
}

pub(crate) fn load_cached_coverage(
    root: &Path,
) -> Result<Option<suite_packet_core::CoverageData>> {
    let path = root.join(".covy").join("state").join("latest.bin");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cached coverage state '{}'", path.display()))?;
    let coverage = suite_foundation_core::cache::deserialize_coverage(&bytes)
        .map_err(|source| anyhow!(source.to_string()))?;
    Ok(Some(coverage))
}

pub(crate) fn load_cached_testmap(
    root: &Path,
) -> Result<Option<suite_packet_core::TestMapIndex>> {
    let path = root.join(".covy").join("state").join("testmap.bin");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(testy_core::pipeline_testmap::load_testmap(&path)?))
}

pub(crate) fn broker_objective(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
) -> Option<String> {
    if let Some(query) = request
        .query
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return Some(query.to_string());
    }
    let guard = state.lock().ok()?;
    guard
        .tasks
        .tasks
        .get(&request.task_id)
        .and_then(|task| task.latest_broker_request.as_ref())
        .and_then(|previous| previous.query.as_ref())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn request_query_missing(request: &BrokerGetContextRequest) -> bool {
    request
        .query
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
}

pub(crate) fn inherit_broker_request_defaults(
    request: &mut BrokerGetContextRequest,
    previous: Option<&BrokerGetContextRequest>,
) {
    let Some(previous) = previous else {
        return;
    };
    let action_was_explicit = request.action.is_some();

    if request.action.is_none() {
        request.action = previous.action;
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = previous.budget_tokens;
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = previous.budget_bytes;
    }
    if request.focus_paths.is_empty() {
        request.focus_paths = previous.focus_paths.clone();
    }
    if request.focus_symbols.is_empty() {
        request.focus_symbols = previous.focus_symbols.clone();
    }
    if request.tool_name.is_none() {
        request.tool_name = previous.tool_name.clone();
    }
    if request.tool_result_kind.is_none() {
        request.tool_result_kind = previous.tool_result_kind;
    }
    if request_query_missing(request) {
        request.query = previous
            .query
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }
    if !action_was_explicit && request.include_sections.is_empty() {
        request.include_sections = previous.include_sections.clone();
    }
    if !action_was_explicit && request.exclude_sections.is_empty() {
        request.exclude_sections = previous.exclude_sections.clone();
    }
    if request.verbosity.is_none() {
        request.verbosity = previous.verbosity;
    }
    if request.response_mode.is_none() {
        request.response_mode = previous.response_mode;
    }
    if !action_was_explicit && request.max_sections.is_none() {
        request.max_sections = previous.max_sections;
    }
    if !action_was_explicit && request.default_max_items_per_section.is_none() {
        request.default_max_items_per_section = previous.default_max_items_per_section;
    }
    if !action_was_explicit && request.section_item_limits.is_empty() {
        request.section_item_limits = previous.section_item_limits.clone();
    }
    if request.persist_artifacts.is_none() {
        request.persist_artifacts = previous.persist_artifacts;
    }
}

pub(crate) fn broker_request_response_mode(
    request: &BrokerGetContextRequest,
) -> BrokerResponseMode {
    request.response_mode.unwrap_or(BrokerResponseMode::Full)
}

pub(crate) fn should_persist_broker_artifacts(request: &BrokerGetContextRequest) -> bool {
    matches!(
        broker_request_response_mode(request),
        BrokerResponseMode::Slim
    ) || request.persist_artifacts.unwrap_or(true)
}

#[derive(Debug, Clone)]
pub(crate) struct BrokerEffectiveLimits {
    pub(crate) max_sections: usize,
    pub(crate) default_max_items_per_section: usize,
    pub(crate) section_item_limits: BTreeMap<String, usize>,
}

pub(crate) fn event_id_for_write(request: &BrokerWriteStateRequest) -> String {
    let payload = serde_json::to_string(request).unwrap_or_else(|_| request.task_id.clone());
    let hash = blake3::hash(payload.as_bytes()).to_hex().to_string();
    format!("broker-{}", &hash[..16])
}

pub(crate) fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn derived_tool_invocation_id(request: &BrokerWriteStateRequest) -> String {
    request
        .invocation_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| event_id_for_write(request))
}

pub(crate) fn derived_tool_sequence(request: &BrokerWriteStateRequest) -> u64 {
    request.sequence.unwrap_or_else(now_unix_millis)
}

pub(crate) fn material_write_is_noop(
    request: &BrokerWriteStateRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> bool {
    let op = request.op.unwrap_or(BrokerWriteOp::FileRead);
    match op {
        BrokerWriteOp::FocusSet => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::FocusClear => {
            if request.paths.is_empty() && request.symbols.is_empty() {
                snapshot.focus_paths.is_empty() && snapshot.focus_symbols.is_empty()
            } else {
                request
                    .paths
                    .iter()
                    .all(|path| !snapshot.focus_paths.iter().any(|existing| existing == path))
                    && request.symbols.iter().all(|symbol| {
                        !snapshot
                            .focus_symbols
                            .iter()
                            .any(|existing| existing == symbol)
                    })
            }
        }
        BrokerWriteOp::FileRead => request
            .paths
            .iter()
            .all(|path| snapshot.files_read.iter().any(|existing| existing == path)),
        BrokerWriteOp::FileEdit => request
            .paths
            .iter()
            .all(|path| snapshot.files_edited.iter().any(|existing| existing == path)),
        BrokerWriteOp::Intention => snapshot.latest_intention.as_ref().is_some_and(|intention| {
            intention.text == request.text.clone().unwrap_or_default()
                && intention.note == request.note
                && intention.step_id == request.step_id
                && intention.question_id == request.question_id
                && intention.paths == request.paths
                && intention.symbols == request.symbols
        }),
        BrokerWriteOp::CheckpointSave => snapshot
            .latest_checkpoint_id
            .as_ref()
            .zip(request.checkpoint_id.as_ref())
            .is_some_and(|(current, requested)| current == requested)
            && snapshot.checkpoint_note == request.note
            && snapshot.checkpoint_focus_paths == request.paths
            && snapshot.checkpoint_focus_symbols == request.symbols,
        BrokerWriteOp::QuestionOpen => request.question_id.as_ref().is_some_and(|question_id| {
            snapshot
                .open_questions
                .iter()
                .any(|question| question.id == *question_id)
        }),
        BrokerWriteOp::QuestionResolve => request.question_id.as_ref().is_some_and(|question_id| {
            !snapshot
                .open_questions
                .iter()
                .any(|question| question.id == *question_id)
        }),
        BrokerWriteOp::DecisionAdd => request.decision_id.as_ref().is_some_and(|decision_id| {
            snapshot
                .active_decisions
                .iter()
                .any(|decision| decision.id == *decision_id)
        }),
        BrokerWriteOp::DecisionSupersede => request
            .decision_id
            .as_ref()
            .is_some_and(|decision_id| {
                !snapshot
                    .active_decisions
                    .iter()
                    .any(|decision| decision.id == *decision_id)
            }),
        BrokerWriteOp::StepComplete => request.step_id.as_ref().is_some_and(|step_id| {
            snapshot
                .completed_steps
                .iter()
                .any(|existing| existing == step_id)
        }),
        BrokerWriteOp::FocusInferred => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::ToolInvocationStarted
        | BrokerWriteOp::ToolInvocationCompleted
        | BrokerWriteOp::ToolInvocationFailed
        | BrokerWriteOp::ToolResult
        | BrokerWriteOp::EvidenceCaptured => false,
    }
}
