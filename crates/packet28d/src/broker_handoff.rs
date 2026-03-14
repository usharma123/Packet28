use super::*;
use crate::broker_context::compute_broker_response;

pub(crate) fn next_action_summary(
    manage: Option<&suite_packet_core::ContextManagePayload>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Option<String> {
    manage
        .and_then(|payload| payload.recommended_actions.first())
        .map(|action| action.summary.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            snapshot
                .latest_intention
                .as_ref()
                .map(|intention| intention.text.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            snapshot
                .open_questions
                .first()
                .map(|question| format!("Resolve open question: {}", question.text))
        })
}

pub(crate) fn compute_handoff_state(
    task: Option<&TaskRecord>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> (bool, String) {
    let latest_handoff_at = task.and_then(|task| task.latest_handoff_generated_at_unix);
    let latest_hook_boundary_at = task.and_then(|task| task.latest_hook_boundary_at_unix);
    let latest_hook_boundary_kind = task.and_then(|task| task.latest_hook_boundary_kind.as_deref());
    let threshold_exceeded = task.is_some_and(|task| task.hook_threshold_exceeded);
    if snapshot.latest_checkpoint_id.is_none() {
        if snapshot.latest_intention.is_none() {
            return (
                false,
                "Intent required before preparing a handoff.".to_string(),
            );
        }
        if latest_hook_boundary_at.is_some_and(|boundary_at| {
            latest_handoff_at.is_none_or(|handoff_at| boundary_at > handoff_at)
        }) {
            let reason = latest_hook_boundary_kind.unwrap_or("boundary");
            return (
                true,
                format!("Hook boundary '{reason}' is available for handoff."),
            );
        }
        if threshold_exceeded {
            return (
                true,
                "Soft context threshold reached and intent is available.".to_string(),
            );
        }
        return (
            false,
            "Hook boundary or threshold required before preparing a handoff.".to_string(),
        );
    }
    let checkpoint_id = snapshot.latest_checkpoint_id.as_ref().unwrap();
    let latest_handoff_checkpoint_id =
        task.and_then(|task| task.latest_handoff_checkpoint_id.as_ref());
    let has_newer_intention = snapshot.latest_intention.as_ref().is_some_and(|intention| {
        latest_handoff_at.is_none_or(|handoff_at| intention.occurred_at_unix > handoff_at)
    });
    let has_state_delta = !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
        || latest_handoff_checkpoint_id.is_none_or(|previous| previous != checkpoint_id);
    if latest_handoff_at.is_none() {
        return (
            true,
            "Checkpoint is available and no handoff has been prepared yet.".to_string(),
        );
    }
    if has_newer_intention {
        return (
            true,
            "Checkpoint is available and a newer worker intention was recorded.".to_string(),
        );
    }
    if has_state_delta {
        return (
            true,
            "Checkpoint is available and task state changed since the last handoff.".to_string(),
        );
    }
    (
        false,
        "Checkpoint is already handed off and no newer intention or delta is available."
            .to_string(),
    )
}

pub(crate) fn slim_broker_response(
    response: &BrokerGetContextResponse,
    artifact_id: Option<String>,
) -> BrokerGetContextResponse {
    BrokerGetContextResponse {
        context_version: response.context_version.clone(),
        response_mode: BrokerResponseMode::Slim,
        artifact_id,
        latest_intention: response.latest_intention.clone(),
        next_action_summary: response.next_action_summary.clone(),
        handoff_ready: response.handoff_ready,
        stale: response.stale,
        brief: response.brief.clone(),
        supersedes_prior_context: response.supersedes_prior_context,
        supersession_mode: response.supersession_mode,
        superseded_before_version: response.superseded_before_version.clone(),
        sections: Vec::new(),
        est_tokens: response.est_tokens,
        est_bytes: response.est_bytes,
        budget_remaining_tokens: response.budget_remaining_tokens,
        budget_remaining_bytes: response.budget_remaining_bytes,
        section_estimates: Vec::new(),
        eviction_candidates: Vec::new(),
        delta: BrokerDeltaResponse::default(),
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
        invalidates_since_version: response.invalidates_since_version,
        effective_max_sections: response.effective_max_sections,
        effective_default_max_items_per_section: response.effective_default_max_items_per_section,
        effective_section_item_limits: response.effective_section_item_limits.clone(),
        diagnostics_ms: response.diagnostics_ms.clone(),
    }
}

pub(crate) fn write_broker_artifacts(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    since_version: Option<&str>,
    response: &BrokerGetContextResponse,
) -> Result<String> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let brief_md_path = task_brief_markdown_path(&root, task_id);
    let brief_json_path = task_brief_json_path(&root, task_id);
    let state_json_path = task_state_json_path(&root, task_id);
    let version_json_path = task_version_json_path(&root, task_id, &response.context_version);
    let version_snapshot =
        build_version_snapshot_response(&root, task_id, since_version, response)?;
    if let Some(parent) = brief_md_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create task artifact dir '{}'", parent.display())
        })?;
    }
    if let Some(parent) = version_json_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create versioned broker artifact dir '{}'",
                parent.display()
            )
        })?;
    }
    fs::write(&brief_md_path, &response.brief)
        .with_context(|| format!("failed to write '{}'", brief_md_path.display()))?;
    fs::write(&brief_json_path, serde_json::to_vec_pretty(response)?)
        .with_context(|| format!("failed to write '{}'", brief_json_path.display()))?;
    fs::write(
        &version_json_path,
        serde_json::to_vec_pretty(&version_snapshot)?,
    )
    .with_context(|| format!("failed to write '{}'", version_json_path.display()))?;

    let hash = blake3::hash(serde_json::to_string(response)?.as_bytes())
        .to_hex()
        .to_string();
    let generated_at = now_unix_millis();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_brief_path = Some(brief_md_path.to_string_lossy().to_string());
        task.latest_brief_hash = Some(hash.clone());
        task.latest_brief_generated_at_unix = Some(generated_at);
        task.last_context_refresh_at_unix = Some(generated_at);
        let state_value = json!({
            "task_id": task_id,
            "context_version": task.latest_context_version,
            "latest_brief_path": task.latest_brief_path,
            "latest_brief_hash": task.latest_brief_hash,
            "latest_brief_generated_at_unix": task.latest_brief_generated_at_unix,
            "latest_context_reason": task.latest_context_reason,
            "brief_json_path": brief_json_path.to_string_lossy().to_string(),
            "event_path": task_event_log_path(&root, task_id).to_string_lossy().to_string(),
            "supports_push": true,
        });
        persist_state(&guard)?;
        fs::write(&state_json_path, serde_json::to_vec_pretty(&state_value)?)
            .with_context(|| format!("failed to write '{}'", state_json_path.display()))?;
    }
    Ok(hash)
}

pub(crate) fn build_version_snapshot_response(
    root: &Path,
    task_id: &str,
    since_version: Option<&str>,
    response: &BrokerGetContextResponse,
) -> Result<BrokerGetContextResponse> {
    if !matches!(response.response_mode, BrokerResponseMode::Delta) {
        return Ok(response.clone());
    }

    let previous = match since_version {
        Some(version) if version != response.context_version => {
            load_versioned_broker_response(root, task_id, version)?
        }
        _ => None,
    };

    let mut snapshot = response.clone();
    snapshot.response_mode = BrokerResponseMode::Full;
    snapshot.sections = merge_version_snapshot_sections(previous.as_ref(), response);
    snapshot.brief = render_brief(task_id, &snapshot.context_version, &snapshot.sections);
    let (est_tokens, est_bytes) = estimate_text_cost(&snapshot.brief);
    snapshot.est_tokens = est_tokens;
    snapshot.est_bytes = est_bytes;
    Ok(snapshot)
}

pub(crate) fn merge_version_snapshot_sections(
    previous: Option<&BrokerGetContextResponse>,
    response: &BrokerGetContextResponse,
) -> Vec<BrokerSection> {
    let Some(previous) = previous else {
        return response.delta.changed_sections.clone();
    };

    let changed_by_id = response
        .delta
        .changed_sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();
    let removed_ids = response
        .delta
        .removed_section_ids
        .iter()
        .map(|id| id.as_str())
        .collect::<HashSet<_>>();
    let mut merged = previous
        .sections
        .iter()
        .filter(|section| !removed_ids.contains(section.id.as_str()))
        .map(|section| {
            changed_by_id
                .get(section.id.as_str())
                .cloned()
                .cloned()
                .unwrap_or_else(|| section.clone())
        })
        .collect::<Vec<_>>();

    for section in &response.delta.changed_sections {
        if !previous
            .sections
            .iter()
            .any(|existing| existing.id == section.id)
        {
            merged.push(section.clone());
        }
    }

    merged
}

fn handoff_context_request(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerPrepareHandoffRequest,
) -> BrokerGetContextRequest {
    let fallback_query = state.lock().ok().and_then(|guard| {
        guard
            .tasks
            .tasks
            .get(&request.task_id)
            .and_then(|task| task.latest_broker_request.as_ref())
            .and_then(|previous| previous.query.clone())
    });
    BrokerGetContextRequest {
        task_id: request.task_id.clone(),
        action: Some(BrokerAction::Summarize),
        budget_tokens: Some(2_000),
        budget_bytes: Some(12_000),
        query: request.query.clone().or(fallback_query),
        include_sections: vec![
            "task_objective".to_string(),
            "agent_intention".to_string(),
            "task_memory".to_string(),
            "checkpoint_context".to_string(),
            "checkpoint_deltas".to_string(),
            "active_decisions".to_string(),
            "open_questions".to_string(),
            "resolved_questions".to_string(),
            "recent_tool_activity".to_string(),
            "evidence_cache".to_string(),
            "recommended_actions".to_string(),
        ],
        response_mode: Some(request.response_mode.unwrap_or(BrokerResponseMode::Slim)),
        max_sections: Some(7),
        default_max_items_per_section: Some(4),
        section_item_limits: BTreeMap::from([
            ("agent_intention".to_string(), 5),
            ("task_memory".to_string(), 5),
            ("checkpoint_context".to_string(), 5),
            ("checkpoint_deltas".to_string(), 6),
            ("active_decisions".to_string(), 4),
            ("open_questions".to_string(), 4),
            ("resolved_questions".to_string(), 4),
            ("recent_tool_activity".to_string(), 4),
            ("evidence_cache".to_string(), 4),
            ("recommended_actions".to_string(), 4),
        ]),
        persist_artifacts: Some(true),
        ..BrokerGetContextRequest::default()
    }
}

pub(crate) fn broker_prepare_handoff(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerPrepareHandoffRequest,
) -> Result<BrokerPrepareHandoffResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker prepare_handoff requires task_id");
    }
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let task = load_task_record(&state, &request.task_id);
    let (handoff_ready, handoff_reason) = compute_handoff_state(task.as_ref(), &snapshot);
    let latest_intention = snapshot.latest_intention.clone();
    let next_action_summary = next_action_summary(None, &snapshot);
    if !handoff_ready {
        if let Some(existing_task) = task.as_ref() {
            if let Some(existing_context_version) = existing_task.latest_context_version.as_deref()
            {
                let root = state.lock().map_err(lock_err)?.root.clone();
                if let Some(existing_context) = load_versioned_broker_response(
                    &root,
                    &request.task_id,
                    existing_context_version,
                )? {
                    let context = if matches!(
                        request.response_mode.unwrap_or(BrokerResponseMode::Slim),
                        BrokerResponseMode::Slim
                    ) {
                        slim_broker_response(
                            &existing_context,
                            existing_task.latest_handoff_artifact_id.clone(),
                        )
                    } else {
                        existing_context
                    };
                    return Ok(BrokerPrepareHandoffResponse {
                        task_id: request.task_id,
                        handoff_ready: true,
                        handoff_reason: "Latest handoff artifact is available for resume."
                            .to_string(),
                        latest_checkpoint_id: snapshot.latest_checkpoint_id,
                        latest_handoff_artifact_id: existing_task
                            .latest_handoff_artifact_id
                            .clone(),
                        latest_handoff_generated_at_unix: existing_task
                            .latest_handoff_generated_at_unix,
                        latest_handoff_checkpoint_id: existing_task
                            .latest_handoff_checkpoint_id
                            .clone(),
                        latest_intention,
                        next_action_summary: context.next_action_summary.clone(),
                        context: Some(context),
                    });
                }
            }
        }
        return Ok(BrokerPrepareHandoffResponse {
            task_id: request.task_id,
            handoff_ready,
            handoff_reason,
            latest_checkpoint_id: snapshot.latest_checkpoint_id,
            latest_handoff_artifact_id: task
                .as_ref()
                .and_then(|task| task.latest_handoff_artifact_id.clone()),
            latest_handoff_generated_at_unix: task
                .as_ref()
                .and_then(|task| task.latest_handoff_generated_at_unix),
            latest_handoff_checkpoint_id: task
                .as_ref()
                .and_then(|task| task.latest_handoff_checkpoint_id.clone()),
            latest_intention,
            next_action_summary,
            context: None,
        });
    }

    let get_request = handoff_context_request(&state, &request);
    let _ = set_context_reason(&state, &request.task_id, "prepare_handoff");
    let mut context = compute_broker_response(&state, &get_request)?;
    context.artifact_id = Some(context.context_version.clone());
    write_broker_artifacts(
        &state,
        &request.task_id,
        get_request.since_version.as_deref(),
        &context,
    )?;
    let generated_at = now_unix_millis();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
        task.latest_handoff_artifact_id = context.artifact_id.clone();
        task.latest_handoff_generated_at_unix = Some(generated_at);
        task.latest_handoff_checkpoint_id = snapshot.latest_checkpoint_id.clone();
        persist_state(&guard)?;
    }
    let context = if matches!(
        get_request
            .response_mode
            .unwrap_or(BrokerResponseMode::Slim),
        BrokerResponseMode::Slim
    ) {
        slim_broker_response(&context, context.artifact_id.clone())
    } else {
        context
    };
    Ok(BrokerPrepareHandoffResponse {
        task_id: request.task_id,
        handoff_ready: true,
        handoff_reason,
        latest_checkpoint_id: snapshot.latest_checkpoint_id.clone(),
        latest_handoff_artifact_id: context.artifact_id.clone(),
        latest_handoff_generated_at_unix: Some(generated_at),
        latest_handoff_checkpoint_id: snapshot.latest_checkpoint_id.clone(),
        latest_intention,
        next_action_summary: context.next_action_summary.clone(),
        context: Some(context),
    })
}
