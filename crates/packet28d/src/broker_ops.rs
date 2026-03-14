use super::*;

pub(crate) fn broker_write_to_event(
    request: &BrokerWriteStateRequest,
) -> Result<suite_packet_core::AgentStateEventPayload> {
    let task_id = request.task_id.trim();
    if task_id.is_empty() {
        anyhow::bail!("broker write_state requires task_id");
    }
    let op = request.op.unwrap_or(BrokerWriteOp::FileRead);
    let (kind, data) = match op {
        BrokerWriteOp::FocusSet => (
            suite_packet_core::AgentStateEventKind::FocusSet,
            suite_packet_core::AgentStateEventData::FocusSet {
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::FocusClear => (
            suite_packet_core::AgentStateEventKind::FocusCleared,
            suite_packet_core::AgentStateEventData::FocusCleared {
                clear_all: request.paths.is_empty() && request.symbols.is_empty(),
            },
        ),
        BrokerWriteOp::FileRead => (
            suite_packet_core::AgentStateEventKind::FileRead,
            suite_packet_core::AgentStateEventData::FileRead {},
        ),
        BrokerWriteOp::FileEdit => (
            suite_packet_core::AgentStateEventKind::FileEdited,
            suite_packet_core::AgentStateEventData::FileEdited {
                regions: request.regions.clone(),
            },
        ),
        BrokerWriteOp::Intention => (
            suite_packet_core::AgentStateEventKind::IntentionRecorded,
            suite_packet_core::AgentStateEventData::IntentionRecorded {
                text: request
                    .text
                    .clone()
                    .ok_or_else(|| anyhow!("intention requires text"))?,
                note: request.note.clone(),
                step_id: request.step_id.clone(),
                question_id: request.question_id.clone(),
            },
        ),
        BrokerWriteOp::CheckpointSave => (
            suite_packet_core::AgentStateEventKind::CheckpointSaved,
            suite_packet_core::AgentStateEventData::CheckpointSaved {
                checkpoint_id: request
                    .checkpoint_id
                    .clone()
                    .ok_or_else(|| anyhow!("checkpoint_save requires checkpoint_id"))?,
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::DecisionAdd => (
            suite_packet_core::AgentStateEventKind::DecisionAdded,
            suite_packet_core::AgentStateEventData::DecisionAdded {
                decision_id: request
                    .decision_id
                    .clone()
                    .ok_or_else(|| anyhow!("decision_add requires decision_id"))?,
                text: request
                    .text
                    .clone()
                    .ok_or_else(|| anyhow!("decision_add requires text"))?,
                supersedes: None,
            },
        ),
        BrokerWriteOp::DecisionSupersede => (
            suite_packet_core::AgentStateEventKind::DecisionSuperseded,
            suite_packet_core::AgentStateEventData::DecisionSuperseded {
                decision_id: request
                    .decision_id
                    .clone()
                    .ok_or_else(|| anyhow!("decision_supersede requires decision_id"))?,
                superseded_by: request.note.clone(),
            },
        ),
        BrokerWriteOp::StepComplete => (
            suite_packet_core::AgentStateEventKind::StepCompleted,
            suite_packet_core::AgentStateEventData::StepCompleted {
                step_id: request
                    .step_id
                    .clone()
                    .ok_or_else(|| anyhow!("step_complete requires step_id"))?,
            },
        ),
        BrokerWriteOp::QuestionOpen => (
            suite_packet_core::AgentStateEventKind::QuestionOpened,
            suite_packet_core::AgentStateEventData::QuestionOpened {
                question_id: request
                    .question_id
                    .clone()
                    .ok_or_else(|| anyhow!("question_open requires question_id"))?,
                text: request
                    .text
                    .clone()
                    .ok_or_else(|| anyhow!("question_open requires text"))?,
            },
        ),
        BrokerWriteOp::QuestionResolve => (
            suite_packet_core::AgentStateEventKind::QuestionResolved,
            suite_packet_core::AgentStateEventData::QuestionResolved {
                question_id: request
                    .question_id
                    .clone()
                    .ok_or_else(|| anyhow!("question_resolve requires question_id"))?,
            },
        ),
        BrokerWriteOp::ToolInvocationStarted => (
            suite_packet_core::AgentStateEventKind::ToolInvocationStarted,
            suite_packet_core::AgentStateEventData::ToolInvocationStarted {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_started requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_started requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_started requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
            },
        ),
        BrokerWriteOp::ToolInvocationCompleted => (
            suite_packet_core::AgentStateEventKind::ToolInvocationCompleted,
            suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                result_summary: request.result_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                search_query: request.search_query.clone(),
                command: request.command.clone(),
                artifact_id: request.artifact_id.clone(),
                regions: request.regions.clone(),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::ToolResult => (
            suite_packet_core::AgentStateEventKind::ToolInvocationCompleted,
            suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
                invocation_id: derived_tool_invocation_id(request),
                sequence: derived_tool_sequence(request),
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_result requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                result_summary: request.result_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                search_query: request.search_query.clone(),
                command: request.command.clone(),
                artifact_id: request.artifact_id.clone(),
                regions: request.regions.clone(),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::ToolInvocationFailed => (
            suite_packet_core::AgentStateEventKind::ToolInvocationFailed,
            suite_packet_core::AgentStateEventData::ToolInvocationFailed {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                error_class: request.error_class.clone(),
                error_message: request.error_message.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                retryable: request.retryable.unwrap_or(false),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::FocusInferred => (
            suite_packet_core::AgentStateEventKind::FocusInferred,
            suite_packet_core::AgentStateEventData::FocusInferred {
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::EvidenceCaptured => (
            suite_packet_core::AgentStateEventKind::EvidenceCaptured,
            suite_packet_core::AgentStateEventData::EvidenceCaptured {
                artifact_id: request
                    .artifact_id
                    .clone()
                    .ok_or_else(|| anyhow!("evidence_captured requires artifact_id"))?,
                summary: request.note.clone(),
            },
        ),
    };
    Ok(suite_packet_core::AgentStateEventPayload {
        task_id: request.task_id.clone(),
        event_id: event_id_for_write(request),
        occurred_at_unix: now_unix_millis(),
        actor: "packet28.broker".to_string(),
        kind,
        paths: request.paths.clone(),
        symbols: request.symbols.clone(),
        data,
    })
}

pub(crate) fn broker_write_state(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerWriteStateRequest,
) -> Result<BrokerWriteStateResponse> {
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    if material_write_is_noop(&request, &snapshot) {
        update_broker_link_state(&state, &request)?;
        let version = current_context_version(&state, &request.task_id)?;
        return Ok(BrokerWriteStateResponse {
            event_id: event_id_for_write(&request),
            context_version: version,
            accepted: true,
        });
    }

    let event = broker_write_to_event(&request)?;
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    kernel.execute(KernelRequest {
        target: "agenty.state.write".to_string(),
        reducer_input: serde_json::to_value(&event)?,
        policy_context: json!({
            "task_id": request.task_id,
        }),
        ..KernelRequest::default()
    })?;
    apply_agent_snapshot_event_to_cache(&state, &event)?;
    if matches!(request.op, Some(BrokerWriteOp::ToolResult)) {
        if !request.paths.is_empty() || !request.symbols.is_empty() {
            let focus_event = suite_packet_core::AgentStateEventPayload {
                task_id: request.task_id.clone(),
                event_id: format!("{}-focus", event.event_id),
                occurred_at_unix: now_unix_millis(),
                actor: "packet28.broker".to_string(),
                kind: suite_packet_core::AgentStateEventKind::FocusInferred,
                paths: request.paths.clone(),
                symbols: request.symbols.clone(),
                data: suite_packet_core::AgentStateEventData::FocusInferred {
                    note: Some(format!(
                        "inferred from {}",
                        request
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool_result".to_string())
                    )),
                },
            };
            kernel.execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: serde_json::to_value(&focus_event)?,
                policy_context: json!({
                    "task_id": request.task_id,
                }),
                ..KernelRequest::default()
            })?;
            apply_agent_snapshot_event_to_cache(&state, &focus_event)?;
        }
        if let Some(artifact_id) = request
            .artifact_id
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            let evidence_event = suite_packet_core::AgentStateEventPayload {
                task_id: request.task_id.clone(),
                event_id: format!("{}-evidence", event.event_id),
                occurred_at_unix: now_unix_millis(),
                actor: "packet28.broker".to_string(),
                kind: suite_packet_core::AgentStateEventKind::EvidenceCaptured,
                paths: Vec::new(),
                symbols: Vec::new(),
                data: suite_packet_core::AgentStateEventData::EvidenceCaptured {
                    artifact_id,
                    summary: Some(format!(
                        "captured from {}",
                        request
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool_result".to_string())
                    )),
                },
            };
            kernel.execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: serde_json::to_value(&evidence_event)?,
                policy_context: json!({
                    "task_id": request.task_id,
                }),
                ..KernelRequest::default()
            })?;
            apply_agent_snapshot_event_to_cache(&state, &evidence_event)?;
        }
    }
    invalidate_broker_caches(&state, &request)?;
    if matches!(request.op, Some(BrokerWriteOp::FileEdit)) && !request.paths.is_empty() {
        let _ = enqueue_incremental_index_paths(&state, &request.paths);
    }
    if let Some(question_id) = &request.resolves_question_id {
        let question_resolved_event = suite_packet_core::AgentStateEventPayload {
            task_id: request.task_id.clone(),
            event_id: format!("{}-resolve", event.event_id),
            occurred_at_unix: now_unix_millis(),
            actor: "packet28.broker".to_string(),
            kind: suite_packet_core::AgentStateEventKind::QuestionResolved,
            paths: Vec::new(),
            symbols: Vec::new(),
            data: suite_packet_core::AgentStateEventData::QuestionResolved {
                question_id: question_id.clone(),
            },
        };
        kernel.execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: serde_json::to_value(&question_resolved_event)?,
            policy_context: json!({
                "task_id": request.task_id,
            }),
            ..KernelRequest::default()
        })?;
        apply_agent_snapshot_event_to_cache(&state, &question_resolved_event)?;
    }
    update_broker_link_state(&state, &request)?;
    let reason = format!(
        "state_write:{}",
        serde_json::to_string(&request.op.unwrap_or(BrokerWriteOp::FileRead))?.trim_matches('"')
    );
    let _ = set_context_reason(&state, &request.task_id, reason);

    let previous_version = current_context_version(&state, &request.task_id)?;
    let version = bump_context_version(&state, &request.task_id)?;
    if request.refresh_context.unwrap_or(true) {
        if let Some(response) =
            refresh_broker_context_for_task(&state, &request.task_id, Some(previous_version))?
        {
            let changed_section_ids = response
                .delta
                .changed_sections
                .iter()
                .map(|section| section.id.clone())
                .collect::<Vec<_>>();
            let _ = emit_task_event(
                state.clone(),
                &request.task_id,
                "context_updated",
                json!({
                    "context_version": response.context_version,
                    "changed_section_ids": changed_section_ids,
                    "removed_section_ids": response.delta.removed_section_ids,
                    "reason": state.lock().ok()
                        .and_then(|guard| guard.tasks.tasks.get(&request.task_id).and_then(|task| task.latest_context_reason.clone()))
                        .unwrap_or_else(|| "state_write".to_string()),
                    "summary": response
                        .sections
                        .first()
                        .map(|section| section.title.clone())
                        .unwrap_or_else(|| "broker refresh".to_string()),
                }),
            );
        }
    }

    Ok(BrokerWriteStateResponse {
        event_id: event.event_id,
        context_version: version,
        accepted: true,
    })
}

pub(crate) fn invalidate_broker_caches(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerWriteStateRequest,
) -> Result<()> {
    let invalidate_repo_map = matches!(request.op, Some(BrokerWriteOp::FileEdit))
        || (matches!(
            request.op,
            Some(BrokerWriteOp::ToolResult | BrokerWriteOp::ToolInvocationCompleted)
        ) && request.operation_kind == Some(suite_packet_core::ToolOperationKind::Edit));
    if invalidate_repo_map {
        if request.paths.is_empty() {
            enqueue_full_index_rebuild(state)?;
        } else {
            let _ = enqueue_incremental_index_paths(state, &request.paths)?;
        }
    } else if request.paths.is_empty() {
        return Ok(());
    }
    let mut guard = state.lock().map_err(lock_err)?;
    for path in &request.paths {
        guard.source_file_cache.remove(path);
    }
    Ok(())
}

pub(crate) fn broker_write_state_batch(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerWriteStateBatchRequest,
) -> Result<BrokerWriteStateBatchResponse> {
    let responses = request
        .requests
        .into_iter()
        .map(|item| broker_write_state(state.clone(), item))
        .collect::<Result<Vec<_>>>()?;
    Ok(BrokerWriteStateBatchResponse {
        accepted: responses.iter().all(|response| response.accepted),
        responses,
    })
}

pub(crate) fn broker_task_status(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerTaskStatusRequest,
) -> Result<BrokerTaskStatusResponse> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let task = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&request.task_id)
        .cloned();
    let (handoff_needed, handoff_reason) = compute_handoff_state(task.as_ref(), &snapshot);
    let handoff_available = task.as_ref().is_some_and(|task| {
        task.latest_handoff_artifact_id.is_some() && task.latest_context_version.is_some()
    });
    let handoff_ready = handoff_needed || handoff_available;
    let handoff_reason = if handoff_needed {
        handoff_reason
    } else if handoff_available {
        "Latest handoff artifact is available for resume.".to_string()
    } else {
        handoff_reason
    };
    Ok(BrokerTaskStatusResponse {
        latest_context_version: task
            .as_ref()
            .and_then(|task| task.latest_context_version.clone()),
        last_refresh_at_unix: task
            .as_ref()
            .and_then(|task| task.last_context_refresh_at_unix),
        latest_context_reason: task
            .as_ref()
            .and_then(|task| task.latest_context_reason.clone()),
        handoff_ready,
        handoff_reason: Some(handoff_reason),
        latest_handoff_artifact_id: task
            .as_ref()
            .and_then(|task| task.latest_handoff_artifact_id.clone()),
        latest_handoff_generated_at_unix: task
            .as_ref()
            .and_then(|task| task.latest_handoff_generated_at_unix),
        latest_handoff_checkpoint_id: task
            .as_ref()
            .and_then(|task| task.latest_handoff_checkpoint_id.clone()),
        supports_push: true,
        task,
        brief_path: task_brief_markdown_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_brief_markdown_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
        state_path: task_state_json_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_state_json_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
        event_path: task_event_log_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_event_log_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
    })
}
