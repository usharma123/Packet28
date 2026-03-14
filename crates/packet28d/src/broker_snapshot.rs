use super::*;

pub(crate) fn truncate_lines(lines: Vec<String>, max_lines: usize) -> String {
    lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn render_recent_tool_activity_lines(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    compact: bool,
) -> Vec<String> {
    snapshot
        .recent_tool_invocations
        .iter()
        .rev()
        .map(|invocation| {
            let request = invocation
                .request_summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("no request summary");
            let operation_kind = serde_json::to_string(&invocation.operation_kind)
                .unwrap_or_else(|_| "\"generic\"".to_string())
                .trim_matches('"')
                .to_string();
            if compact {
                let mut metadata = vec![
                    format!("paths={}", invocation.paths.len()),
                    format!("symbols={}", invocation.symbols.len()),
                ];
                if !invocation.regions.is_empty() {
                    metadata.push(format!("regions={}", invocation.regions.len()));
                }
                if let Some(duration_ms) = invocation.duration_ms {
                    metadata.push(format!("duration={}ms", duration_ms));
                }
                format!(
                    "- #{} {} [{}] {} ({})",
                    invocation.sequence,
                    invocation.tool_name,
                    operation_kind,
                    request,
                    metadata.join(", ")
                )
            } else {
                let result = invocation
                    .result_summary
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("no result summary");
                format!(
                    "- #{} {} [{}] {} -> {}",
                    invocation.sequence, invocation.tool_name, operation_kind, request, result
                )
            }
        })
        .collect()
}

pub(crate) fn render_task_memory_lines(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(intention) = snapshot.latest_intention.as_ref() {
        let phase = intention.step_id.as_deref().unwrap_or("unspecified");
        lines.push(format!("- latest intention [{phase}]: {}", intention.text));
        if let Some(note) = intention
            .note
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("- latest intention note: {note}"));
        }
    }
    if let Some(invocation) = snapshot.recent_tool_invocations.last() {
        let request = invocation
            .request_summary
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("no request summary");
        let result = invocation
            .result_summary
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("no result summary");
        let operation_kind = serde_json::to_string(&invocation.operation_kind)
            .unwrap_or_else(|_| "\"generic\"".to_string())
            .trim_matches('"')
            .to_string();
        lines.push(format!(
            "- latest tool: {} [{}] {} -> {}",
            invocation.tool_name, operation_kind, request, result
        ));
        for path in invocation.paths.iter().take(2) {
            lines.push(format!("- latest tool path: {path}"));
        }
        for symbol in invocation.symbols.iter().take(2) {
            lines.push(format!("- latest tool symbol: {symbol}"));
        }
    }
    for path in snapshot.files_read.iter().rev().take(3) {
        lines.push(format!("- recently read: {path}"));
    }
    for path in snapshot.files_edited.iter().rev().take(2) {
        lines.push(format!("- recently edited: {path}"));
    }
    if let Some(checkpoint_id) = snapshot.latest_checkpoint_id.as_ref() {
        lines.push(format!("- latest checkpoint: {checkpoint_id}"));
    }
    if let Some(note) = snapshot
        .checkpoint_note
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("- checkpoint note: {note}"));
    }
    for path in snapshot.checkpoint_focus_paths.iter().take(2) {
        lines.push(format!("- checkpoint focus path: {path}"));
    }
    for symbol in snapshot.checkpoint_focus_symbols.iter().take(2) {
        lines.push(format!("- checkpoint focus symbol: {symbol}"));
    }
    for path in snapshot.changed_paths_since_checkpoint.iter().take(3) {
        lines.push(format!("- changed since checkpoint: {path}"));
    }
    for symbol in snapshot.changed_symbols_since_checkpoint.iter().take(3) {
        lines.push(format!("- changed symbol since checkpoint: {symbol}"));
    }
    for artifact_id in snapshot.evidence_artifact_ids.iter().rev().take(2) {
        lines.push(format!("- evidence artifact: {artifact_id}"));
    }
    lines
}

pub(crate) fn render_checkpoint_context_lines(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(checkpoint_id) = snapshot.latest_checkpoint_id.as_ref() else {
        return lines;
    };
    lines.push(format!("- checkpoint: {checkpoint_id}"));
    if let Some(note) = snapshot
        .checkpoint_note
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("- note: {note}"));
    }
    for path in snapshot.checkpoint_focus_paths.iter().take(4) {
        lines.push(format!("- focus path: {path}"));
    }
    for symbol in snapshot.checkpoint_focus_symbols.iter().take(4) {
        lines.push(format!("- focus symbol: {symbol}"));
    }
    lines
}

pub(crate) fn postprocess_selected_sections(
    mut sections: Vec<BrokerSection>,
    pruned: &[BrokerEvictionCandidate],
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    effective_limits: &BrokerEffectiveLimits,
) -> Vec<BrokerSection> {
    let has_code_evidence = sections.iter().any(|section| section.id == "code_evidence");
    if has_code_evidence {
        if let Some(section) = sections
            .iter_mut()
            .find(|section| section.id == "recent_tool_activity")
        {
            section.body = truncate_lines(
                render_recent_tool_activity_lines(snapshot, true),
                section_item_limit(effective_limits, "recent_tool_activity"),
            );
        }
    }

    if let Some(note) = build_budget_notes_section(pruned, effective_limits) {
        if sections.len() >= effective_limits.max_sections {
            if let Some(idx) = sections
                .iter()
                .rposition(|section| section.priority > 1 && section.id != "task_objective")
            {
                sections.remove(idx);
            } else if let Some(idx) = sections
                .iter()
                .rposition(|section| section.id != "task_objective")
            {
                sections.remove(idx);
            }
        }
        let insert_at = sections
            .iter()
            .position(|section| section.id == "task_objective")
            .map(|idx| idx + 1)
            .unwrap_or(0);
        sections.insert(insert_at, note);
    }

    sections
}

pub(crate) fn apply_agent_snapshot_event_to_cache(
    state: &Arc<Mutex<DaemonState>>,
    event: &suite_packet_core::AgentStateEventPayload,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let snapshot = guard
        .agent_snapshots
        .entry(event.task_id.clone())
        .or_insert_with(|| suite_packet_core::AgentSnapshotPayload {
            task_id: event.task_id.clone(),
            ..suite_packet_core::AgentSnapshotPayload::default()
        });
    apply_agent_snapshot_event(snapshot, event);
    Ok(())
}

fn apply_agent_snapshot_event(
    snapshot: &mut suite_packet_core::AgentSnapshotPayload,
    event: &suite_packet_core::AgentStateEventPayload,
) {
    snapshot.task_id = event.task_id.clone();
    snapshot.event_count = snapshot.event_count.saturating_add(1);
    snapshot.last_event_at_unix = Some(event.occurred_at_unix);

    match &event.data {
        suite_packet_core::AgentStateEventData::FocusSet { .. }
        | suite_packet_core::AgentStateEventData::FocusInferred { .. } => {
            extend_sorted_unique(&mut snapshot.focus_paths, &event.paths);
            extend_sorted_unique(&mut snapshot.focus_symbols, &event.symbols);
        }
        suite_packet_core::AgentStateEventData::FocusCleared { clear_all } => {
            if *clear_all {
                snapshot.focus_paths.clear();
                snapshot.focus_symbols.clear();
            } else {
                remove_many(&mut snapshot.focus_paths, &event.paths);
                remove_many(&mut snapshot.focus_symbols, &event.symbols);
            }
        }
        suite_packet_core::AgentStateEventData::FileRead {} => {
            extend_sorted_unique(&mut snapshot.files_read, &event.paths);
        }
        suite_packet_core::AgentStateEventData::FileEdited { .. } => {
            extend_sorted_unique(&mut snapshot.files_edited, &event.paths);
            extend_sorted_unique(&mut snapshot.changed_paths_since_checkpoint, &event.paths);
            extend_sorted_unique(
                &mut snapshot.changed_symbols_since_checkpoint,
                &event.symbols,
            );
        }
        suite_packet_core::AgentStateEventData::IntentionRecorded {
            text,
            note,
            step_id,
            question_id,
        } => {
            extend_sorted_unique(&mut snapshot.focus_paths, &event.paths);
            extend_sorted_unique(&mut snapshot.focus_symbols, &event.symbols);
            snapshot.latest_intention = Some(suite_packet_core::AgentIntention {
                text: text.clone(),
                note: note.clone(),
                step_id: step_id.clone(),
                question_id: question_id.clone(),
                paths: event.paths.clone(),
                symbols: event.symbols.clone(),
                occurred_at_unix: event.occurred_at_unix,
            });
        }
        suite_packet_core::AgentStateEventData::CheckpointSaved {
            checkpoint_id,
            note,
        } => {
            snapshot.latest_checkpoint_id = Some(checkpoint_id.clone());
            snapshot.latest_checkpoint_at_unix = Some(event.occurred_at_unix);
            snapshot.checkpoint_note = note.clone();
            snapshot.checkpoint_focus_paths = event.paths.clone();
            snapshot.checkpoint_focus_symbols = event.symbols.clone();
            snapshot.changed_paths_since_checkpoint.clear();
            snapshot.changed_symbols_since_checkpoint.clear();
        }
        suite_packet_core::AgentStateEventData::DecisionAdded {
            decision_id,
            text,
            supersedes,
        } => {
            if let Some(previous) = supersedes {
                snapshot
                    .active_decisions
                    .retain(|decision| decision.id != *previous);
            }
            snapshot
                .active_decisions
                .retain(|decision| decision.id != *decision_id);
            snapshot
                .active_decisions
                .push(suite_packet_core::AgentDecision {
                    id: decision_id.clone(),
                    text: text.clone(),
                });
            snapshot
                .active_decisions
                .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.text.cmp(&b.text)));
        }
        suite_packet_core::AgentStateEventData::DecisionSuperseded { decision_id, .. } => {
            snapshot
                .active_decisions
                .retain(|decision| decision.id != *decision_id);
        }
        suite_packet_core::AgentStateEventData::StepCompleted { step_id } => {
            insert_sorted_unique(&mut snapshot.completed_steps, step_id.clone());
        }
        suite_packet_core::AgentStateEventData::QuestionOpened { question_id, text } => {
            snapshot
                .open_questions
                .retain(|question| question.id != *question_id);
            snapshot
                .open_questions
                .push(suite_packet_core::AgentQuestion {
                    id: question_id.clone(),
                    text: text.clone(),
                });
            snapshot
                .open_questions
                .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.text.cmp(&b.text)));
        }
        suite_packet_core::AgentStateEventData::QuestionResolved { question_id } => {
            snapshot
                .open_questions
                .retain(|question| question.id != *question_id);
        }
        suite_packet_core::AgentStateEventData::ToolInvocationStarted { .. } => {}
        suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
            invocation_id,
            sequence,
            tool_name,
            server_name,
            operation_kind,
            request_summary,
            result_summary,
            request_fingerprint,
            search_query,
            command,
            artifact_id,
            regions,
            duration_ms,
        } => {
            extend_sorted_unique(&mut snapshot.focus_paths, &event.paths);
            extend_sorted_unique(&mut snapshot.focus_symbols, &event.symbols);
            match operation_kind {
                suite_packet_core::ToolOperationKind::Read => {
                    extend_sorted_unique(&mut snapshot.files_read, &event.paths);
                    merge_tool_path_summary(
                        &mut snapshot.read_paths_by_tool,
                        tool_name,
                        *operation_kind,
                        &event.paths,
                    );
                }
                suite_packet_core::ToolOperationKind::Edit => {
                    extend_sorted_unique(&mut snapshot.files_edited, &event.paths);
                    extend_sorted_unique(
                        &mut snapshot.changed_paths_since_checkpoint,
                        &event.paths,
                    );
                    extend_sorted_unique(
                        &mut snapshot.changed_symbols_since_checkpoint,
                        &event.symbols,
                    );
                    merge_tool_path_summary(
                        &mut snapshot.edited_paths_by_tool,
                        tool_name,
                        *operation_kind,
                        &event.paths,
                    );
                }
                _ => {}
            }
            if let Some(query) = search_query
                .as_ref()
                .filter(|value| !value.trim().is_empty())
            {
                snapshot
                    .search_queries
                    .retain(|item| !(item.tool_name == *tool_name && item.query == *query));
                snapshot
                    .search_queries
                    .push(suite_packet_core::SearchQuerySummary {
                        tool_name: tool_name.clone(),
                        query: query.clone(),
                    });
                snapshot.search_queries.sort_by(|a, b| {
                    a.tool_name
                        .cmp(&b.tool_name)
                        .then_with(|| a.query.cmp(&b.query))
                });
            }
            if let Some(artifact_id) = artifact_id
                .as_ref()
                .filter(|value| !value.trim().is_empty())
            {
                insert_sorted_unique(&mut snapshot.evidence_artifact_ids, artifact_id.clone());
            }
            snapshot
                .recent_tool_invocations
                .retain(|item| item.invocation_id != *invocation_id);
            snapshot
                .recent_tool_invocations
                .push(suite_packet_core::ToolInvocationSummary {
                    invocation_id: invocation_id.clone(),
                    sequence: *sequence,
                    tool_name: tool_name.clone(),
                    server_name: server_name.clone(),
                    operation_kind: *operation_kind,
                    request_summary: request_summary.clone(),
                    result_summary: result_summary.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    search_query: search_query.clone(),
                    command: command.clone(),
                    artifact_id: artifact_id.clone(),
                    paths: event.paths.clone(),
                    regions: regions.clone(),
                    symbols: event.symbols.clone(),
                    duration_ms: *duration_ms,
                    occurred_at_unix: event.occurred_at_unix,
                });
            snapshot.recent_tool_invocations.sort_by(|a, b| {
                a.sequence
                    .cmp(&b.sequence)
                    .then_with(|| a.occurred_at_unix.cmp(&b.occurred_at_unix))
                    .then_with(|| a.invocation_id.cmp(&b.invocation_id))
            });
            trim_front(&mut snapshot.recent_tool_invocations, 12);
            snapshot
                .last_successful_tool_by_kind
                .retain(|item| item.operation_kind != *operation_kind);
            snapshot
                .last_successful_tool_by_kind
                .push(suite_packet_core::ToolKindSuccess {
                    operation_kind: *operation_kind,
                    tool_name: tool_name.clone(),
                    invocation_id: invocation_id.clone(),
                });
            snapshot
                .last_successful_tool_by_kind
                .sort_by(|a, b| a.operation_kind.cmp(&b.operation_kind));
        }
        suite_packet_core::AgentStateEventData::ToolInvocationFailed {
            invocation_id,
            sequence,
            tool_name,
            server_name,
            operation_kind,
            request_summary,
            error_class,
            error_message,
            request_fingerprint,
            retryable,
            duration_ms,
        } => {
            snapshot
                .tool_failures
                .retain(|item| item.invocation_id != *invocation_id);
            snapshot
                .tool_failures
                .push(suite_packet_core::ToolFailureSummary {
                    invocation_id: invocation_id.clone(),
                    sequence: *sequence,
                    tool_name: tool_name.clone(),
                    server_name: server_name.clone(),
                    operation_kind: *operation_kind,
                    request_summary: request_summary.clone(),
                    error_class: error_class.clone(),
                    error_message: error_message.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    retryable: *retryable,
                    duration_ms: *duration_ms,
                    occurred_at_unix: event.occurred_at_unix,
                });
            snapshot.tool_failures.sort_by(|a, b| {
                a.sequence
                    .cmp(&b.sequence)
                    .then_with(|| a.occurred_at_unix.cmp(&b.occurred_at_unix))
                    .then_with(|| a.invocation_id.cmp(&b.invocation_id))
            });
            trim_front(&mut snapshot.tool_failures, 8);
        }
        suite_packet_core::AgentStateEventData::EvidenceCaptured { artifact_id, .. } => {
            insert_sorted_unique(&mut snapshot.evidence_artifact_ids, artifact_id.clone());
        }
    }
}

pub(crate) fn insert_sorted_unique(values: &mut Vec<String>, value: String) {
    if values.binary_search(&value).is_err() {
        values.push(value);
        values.sort();
    }
}

fn extend_sorted_unique(values: &mut Vec<String>, incoming: &[String]) {
    for value in incoming {
        insert_sorted_unique(values, value.clone());
    }
}

fn remove_many(values: &mut Vec<String>, incoming: &[String]) {
    values.retain(|value| !incoming.iter().any(|candidate| candidate == value));
}

fn merge_tool_path_summary(
    entries: &mut Vec<suite_packet_core::ToolPathSummary>,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    paths: &[String],
) {
    let mut found = false;
    for entry in entries.iter_mut() {
        if entry.tool_name == tool_name && entry.operation_kind == operation_kind {
            extend_sorted_unique(&mut entry.paths, paths);
            found = true;
            break;
        }
    }
    if !found {
        let mut summary = suite_packet_core::ToolPathSummary {
            tool_name: tool_name.to_string(),
            operation_kind,
            paths: Vec::new(),
        };
        extend_sorted_unique(&mut summary.paths, paths);
        entries.push(summary);
        entries.sort_by(|a, b| {
            a.tool_name
                .cmp(&b.tool_name)
                .then_with(|| a.operation_kind.cmp(&b.operation_kind))
        });
    }
}

fn trim_front<T>(values: &mut Vec<T>, keep: usize) {
    if values.len() > keep {
        let drop_count = values.len() - keep;
        values.drain(0..drop_count);
    }
}

pub(crate) fn build_resolved_questions(
    task: Option<&TaskRecord>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<BrokerResolvedQuestion> {
    let Some(task) = task else {
        return Vec::new();
    };
    let active_decisions = snapshot
        .active_decisions
        .iter()
        .map(|decision| (decision.id.as_str(), decision.text.as_str()))
        .collect::<BTreeMap<_, _>>();
    task.resolved_questions
        .iter()
        .map(|(question_id, decision_id)| BrokerResolvedQuestion {
            id: question_id.clone(),
            text: task
                .question_texts
                .get(question_id)
                .cloned()
                .unwrap_or_else(|| "resolved question".to_string()),
            resolved_by_decision_id: (!decision_id.trim().is_empty()).then(|| decision_id.clone()),
            resolution_text: (!decision_id.trim().is_empty())
                .then(|| {
                    active_decisions
                        .get(decision_id.as_str())
                        .map(|value| (*value).to_string())
                })
                .flatten(),
        })
        .collect()
}

pub(crate) fn latest_intention_lines(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<String> {
    let Some(intention) = snapshot.latest_intention.as_ref() else {
        return Vec::new();
    };
    let mut lines = vec![format!("- objective: {}", intention.text)];
    if let Some(step_id) = intention
        .step_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("- phase: {step_id}"));
    }
    if let Some(note) = intention
        .note
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("- why now: {note}"));
    }
    if let Some(question_id) = intention
        .question_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("- blocker: {question_id}"));
    }
    for path in intention.paths.iter().take(3) {
        lines.push(format!("- focus path: {path}"));
    }
    for symbol in intention.symbols.iter().take(3) {
        lines.push(format!("- focus symbol: {symbol}"));
    }
    lines
}

pub(crate) fn build_budget_notes_section(
    pruned: &[BrokerEvictionCandidate],
    effective_limits: &BrokerEffectiveLimits,
) -> Option<BrokerSection> {
    let mut saved_by_section = BTreeMap::<String, u64>::new();
    for candidate in pruned
        .iter()
        .filter(|candidate| candidate.reason == "budget_pruned")
    {
        saved_by_section
            .entry(candidate.section_id.clone())
            .and_modify(|saved| *saved = saved.saturating_add(candidate.est_tokens))
            .or_insert(candidate.est_tokens);
    }
    let lines = saved_by_section
        .into_iter()
        .map(|(section_id, est_tokens)| {
            format!(
                "- {} omitted due to budget (saved ~{} tokens)",
                section_id, est_tokens
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    Some(BrokerSection {
        id: "budget_notes".to_string(),
        title: "Budget Notes".to_string(),
        body: truncate_lines(lines, section_item_limit(effective_limits, "budget_notes")),
        priority: 1,
        source_kind: BrokerSourceKind::Derived,
    })
}
