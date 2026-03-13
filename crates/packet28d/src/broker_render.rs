use super::*;

pub(crate) fn estimate_text_cost(text: &str) -> (u64, u64) {
    let est_bytes = text.len() as u64;
    let est_tokens = est_bytes.saturating_add(3) / 4;
    (est_tokens.max(1), est_bytes)
}

fn packet_source_kind(packet: &suite_packet_core::ContextManagePacketRef) -> BrokerSourceKind {
    if packet.target.starts_with("agenty.state.") {
        BrokerSourceKind::SelfAuthored
    } else if packet.target.starts_with("contextq.")
        || packet.target.starts_with("mapy.")
        || packet.target.starts_with("context.")
    {
        BrokerSourceKind::Derived
    } else {
        BrokerSourceKind::External
    }
}

fn section_ids_for_action(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "active_decisions",
            "open_questions",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "recommended_actions",
        ],
        BrokerAction::Inspect => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "checkpoint_deltas",
            "active_decisions",
            "open_questions",
        ],
        BrokerAction::ChooseTool => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "discovered_scope",
            "search_evidence",
            "code_evidence",
            "recommended_actions",
            "relevant_context",
            "open_questions",
            "active_decisions",
        ],
        BrokerAction::Interpret => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "code_evidence",
            "recommended_actions",
            "relevant_context",
            "active_decisions",
            "open_questions",
            "resolved_questions",
        ],
        BrokerAction::Edit => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "evidence_cache",
            "checkpoint_deltas",
            "active_decisions",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "resolved_questions",
        ],
        BrokerAction::Summarize => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "progress",
            "recent_tool_activity",
            "tool_failures",
            "active_decisions",
            "resolved_questions",
            "open_questions",
            "checkpoint_deltas",
        ],
    }
}

fn default_limits_for_action(action: BrokerAction) -> BrokerEffectiveLimits {
    let mut section_item_limits = BTreeMap::new();
    match action {
        BrokerAction::Plan => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("active_decisions".to_string(), 8);
            section_item_limits.insert("open_questions".to_string(), 8);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 6);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Inspect => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::ChooseTool => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 4);
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("discovered_scope".to_string(), 6);
            section_item_limits.insert("search_evidence".to_string(), 6);
            section_item_limits.insert("code_evidence".to_string(), 4);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("open_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 6,
                default_max_items_per_section: 5,
                section_item_limits,
            }
        }
        BrokerAction::Interpret => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 5);
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 4,
                section_item_limits,
            }
        }
        BrokerAction::Edit => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("evidence_cache".to_string(), 4);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 5);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Summarize => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 5);
            section_item_limits.insert("progress".to_string(), 3);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
    }
}

fn legacy_verbosity_limits(
    action: BrokerAction,
    verbosity: BrokerVerbosity,
) -> BrokerEffectiveLimits {
    let mut limits = default_limits_for_action(action);
    match verbosity {
        BrokerVerbosity::Compact => {
            limits.max_sections = limits.max_sections.min(4);
            limits.default_max_items_per_section = 3;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).min(3);
            }
        }
        BrokerVerbosity::Standard => {}
        BrokerVerbosity::Rich => {
            limits.max_sections = limits.max_sections.max(8);
            limits.default_max_items_per_section = 12;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).max(10);
            }
        }
    }
    limits
}

pub(crate) fn resolve_effective_limits(
    action: BrokerAction,
    verbosity: Option<BrokerVerbosity>,
    max_sections: Option<usize>,
    default_max_items_per_section: Option<usize>,
    section_item_limits: &BTreeMap<String, usize>,
) -> BrokerEffectiveLimits {
    let has_explicit_limits = max_sections.is_some()
        || default_max_items_per_section.is_some()
        || !section_item_limits.is_empty();
    let mut limits = if has_explicit_limits {
        default_limits_for_action(action)
    } else {
        legacy_verbosity_limits(action, verbosity.unwrap_or(BrokerVerbosity::Standard))
    };
    if let Some(value) = max_sections.filter(|value| *value > 0) {
        limits.max_sections = value;
    }
    if let Some(value) = default_max_items_per_section.filter(|value| *value > 0) {
        limits.default_max_items_per_section = value;
    }
    for (section_id, limit) in section_item_limits {
        if *limit > 0 {
            limits
                .section_item_limits
                .insert(section_id.clone(), *limit);
        }
    }
    limits
}

fn section_item_limit(limits: &BrokerEffectiveLimits, section_id: &str) -> usize {
    limits
        .section_item_limits
        .get(section_id)
        .copied()
        .unwrap_or(limits.default_max_items_per_section)
}

fn truncate_lines(lines: Vec<String>, max_lines: usize) -> String {
    lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_recent_tool_activity_lines(
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
            .and_modify(|saved| *saved = (*saved).max(candidate.est_tokens))
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

pub(crate) fn filter_requested_section_ids(
    action: BrokerAction,
    include_sections: &[String],
    exclude_sections: &[String],
) -> HashSet<String> {
    let mut allowed = section_ids_for_action(action)
        .iter()
        .map(|value| (*value).to_string())
        .collect::<HashSet<_>>();
    if !include_sections.is_empty() {
        allowed = include_sections
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .collect();
    }
    for excluded in exclude_sections {
        allowed.remove(excluded.trim());
    }
    allowed
}

pub(crate) fn should_run_reducer_search(allowed_sections: &HashSet<String>) -> bool {
    allowed_sections.contains("search_evidence") || allowed_sections.contains("code_evidence")
}

pub(crate) fn load_task_record(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Option<TaskRecord> {
    state.lock().ok()?.tasks.tasks.get(task_id).cloned()
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

fn shrink_section_to_budget(
    section: &BrokerSection,
    remaining_tokens: u64,
    remaining_bytes: u64,
) -> Option<BrokerSection> {
    if remaining_tokens == 0 || remaining_bytes == 0 {
        return None;
    }
    let lines = section.body.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    for line_count in (1..=lines.len()).rev() {
        let candidate_body = lines[..line_count].join("\n");
        let (est_tokens, est_bytes) = estimate_text_cost(&candidate_body);
        if est_tokens <= remaining_tokens && est_bytes <= remaining_bytes {
            let mut candidate = section.clone();
            candidate.body = candidate_body;
            return Some(candidate);
        }
    }
    let mut candidate = section.clone();
    let max_chars = remaining_bytes.min((remaining_tokens.saturating_mul(4)).max(1)) as usize;
    let truncated = section
        .body
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    if truncated.is_empty() {
        return None;
    }
    candidate.body = format!("{truncated}...");
    Some(candidate)
}

fn action_critical_section_ids(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "search_evidence",
            "relevant_context",
            "recommended_actions",
        ],
        BrokerAction::Inspect => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "code_evidence",
            "search_evidence",
            "relevant_context",
        ],
        BrokerAction::ChooseTool => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "recommended_actions",
        ],
        BrokerAction::Interpret => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "code_evidence",
        ],
        BrokerAction::Edit => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "code_evidence",
            "current_focus",
            "checkpoint_deltas",
            "evidence_cache",
        ],
        BrokerAction::Summarize => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "progress",
            "recent_tool_activity",
            "tool_failures",
        ],
    }
}

pub(crate) fn prune_sections_for_budget(
    action: BrokerAction,
    sections: Vec<BrokerSection>,
    budget_tokens: u64,
    budget_bytes: u64,
    max_sections: usize,
) -> (Vec<BrokerSection>, Vec<BrokerEvictionCandidate>) {
    if sections.is_empty() {
        return (sections, Vec::new());
    }

    let critical_ids = action_critical_section_ids(action)
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut selected = Vec::new();
    let mut pruned = Vec::new();
    let mut used_tokens = 0_u64;
    let mut used_bytes = 0_u64;
    let min_remaining_tokens_for_optional = ((budget_tokens as f64) * 0.2).ceil() as u64;
    let min_remaining_bytes_for_optional = ((budget_bytes as f64) * 0.2).ceil() as u64;

    let consider = |section: BrokerSection,
                    must_keep: bool,
                    selected: &mut Vec<BrokerSection>,
                    pruned: &mut Vec<BrokerEvictionCandidate>,
                    used_tokens: &mut u64,
                    used_bytes: &mut u64| {
        let (est_tokens, est_bytes) = estimate_text_cost(&section.body);
        if est_tokens + *used_tokens <= budget_tokens && est_bytes + *used_bytes <= budget_bytes {
            *used_tokens = (*used_tokens).saturating_add(est_tokens);
            *used_bytes = (*used_bytes).saturating_add(est_bytes);
            selected.push(section);
            return;
        }
        let remaining_tokens = budget_tokens.saturating_sub(*used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(*used_bytes);
        if must_keep {
            if let Some(shrunk) =
                shrink_section_to_budget(&section, remaining_tokens, remaining_bytes)
            {
                let (shrunk_tokens, shrunk_bytes) = estimate_text_cost(&shrunk.body);
                *used_tokens = (*used_tokens).saturating_add(shrunk_tokens);
                *used_bytes = (*used_bytes).saturating_add(shrunk_bytes);
                selected.push(shrunk);
                return;
            }
        }
        pruned.push(BrokerEvictionCandidate {
            section_id: section.id.clone(),
            reason: "budget_pruned".to_string(),
            est_tokens,
        });
    };

    let mut objective = sections
        .iter()
        .find(|section| section.id == "task_objective")
        .cloned();
    if let Some(objective) = objective.take() {
        consider(
            objective,
            true,
            &mut selected,
            &mut pruned,
            &mut used_tokens,
            &mut used_bytes,
        );
    }

    for section_id in action_critical_section_ids(action) {
        if let Some(section) = sections
            .iter()
            .find(|section| section.id == *section_id)
            .cloned()
        {
            consider(
                section,
                true,
                &mut selected,
                &mut pruned,
                &mut used_tokens,
                &mut used_bytes,
            );
        }
    }

    for section in sections {
        if section.id == "task_objective" || critical_ids.contains(section.id.as_str()) {
            continue;
        }
        let remaining_tokens = budget_tokens.saturating_sub(used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(used_bytes);
        if remaining_tokens < min_remaining_tokens_for_optional
            || remaining_bytes < min_remaining_bytes_for_optional
        {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
            continue;
        }
        consider(
            section,
            false,
            &mut selected,
            &mut pruned,
            &mut used_tokens,
            &mut used_bytes,
        );
    }

    if selected.len() > max_sections {
        for section in selected.drain(max_sections..) {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id,
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
        }
    }

    (selected, pruned)
}

pub(crate) fn build_broker_sections(
    root: &Path,
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    manage: Option<&suite_packet_core::ContextManagePayload>,
    _repo_map: Option<&suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>>,
) -> Vec<BrokerSection> {
    let action = request.action.unwrap_or(BrokerAction::Plan);
    let effective_limits = resolve_effective_limits(
        action,
        request.verbosity,
        request.max_sections,
        request.default_max_items_per_section,
        &request.section_item_limits,
    );
    let allowed_sections =
        filter_requested_section_ids(action, &request.include_sections, &request.exclude_sections);
    let task = load_task_record(state, &request.task_id);
    let resolved_questions = build_resolved_questions(task.as_ref(), snapshot);
    let focus_symbols = if request.focus_symbols.is_empty() {
        merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols)
    } else {
        request.focus_symbols.clone()
    };
    let mut query_focus = derive_query_focus(broker_objective(state, request).as_deref());
    if !focus_symbols.is_empty() {
        query_focus.full_symbol_terms.clear();
        query_focus.symbol_terms.clear();
    }
    let query_focus = merge_query_focus_with_symbols(query_focus, &focus_symbols);
    let mut sections = Vec::new();

    if let Some(objective) = query_focus.raw_query.clone() {
        sections.push(BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: objective,
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let task_memory_lines = render_task_memory_lines(snapshot);
    if !task_memory_lines.is_empty() {
        sections.push(BrokerSection {
            id: "task_memory".to_string(),
            title: "Task Memory".to_string(),
            body: truncate_lines(
                task_memory_lines,
                section_item_limit(&effective_limits, "task_memory"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Plan
                    | BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Interpret
                    | BrokerAction::Edit
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let intention_lines = latest_intention_lines(snapshot);
    if !intention_lines.is_empty() {
        sections.push(BrokerSection {
            id: "agent_intention".to_string(),
            title: "Latest Intention".to_string(),
            body: truncate_lines(
                intention_lines,
                section_item_limit(&effective_limits, "agent_intention"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let checkpoint_context_lines = render_checkpoint_context_lines(snapshot);
    if !checkpoint_context_lines.is_empty() {
        sections.push(BrokerSection {
            id: "checkpoint_context".to_string(),
            title: "Checkpoint Context".to_string(),
            body: truncate_lines(
                checkpoint_context_lines,
                section_item_limit(&effective_limits, "checkpoint_context"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    if !snapshot.active_decisions.is_empty() {
        sections.push(BrokerSection {
            id: "active_decisions".to_string(),
            title: "Active Decisions".to_string(),
            body: truncate_lines(
                snapshot
                    .active_decisions
                    .iter()
                    .map(|decision| {
                        let suffix = task
                            .as_ref()
                            .and_then(|task| task.linked_decisions.get(&decision.id))
                            .map(|question_id| format!(" (answers {question_id})"))
                            .unwrap_or_default();
                        format!("- {}: {}{}", decision.id, decision.text, suffix)
                    })
                    .collect(),
                section_item_limit(&effective_limits, "active_decisions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.open_questions.is_empty() {
        sections.push(BrokerSection {
            id: "open_questions".to_string(),
            title: "Open Questions".to_string(),
            body: truncate_lines(
                snapshot
                    .open_questions
                    .iter()
                    .map(|question| format!("- {}: {}", question.id, question.text))
                    .collect(),
                section_item_limit(&effective_limits, "open_questions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !resolved_questions.is_empty() {
        sections.push(BrokerSection {
            id: "resolved_questions".to_string(),
            title: "Resolved Questions".to_string(),
            body: truncate_lines(
                resolved_questions
                    .iter()
                    .map(|question| {
                        match (&question.resolved_by_decision_id, &question.resolution_text) {
                            (Some(decision_id), Some(text)) => {
                                format!(
                                    "- {}: {} -> {} ({})",
                                    question.id, question.text, decision_id, text
                                )
                            }
                            (Some(decision_id), None) => {
                                format!("- {}: {} -> {}", question.id, question.text, decision_id)
                            }
                            _ => format!("- {}: {}", question.id, question.text),
                        }
                    })
                    .collect(),
                section_item_limit(&effective_limits, "resolved_questions"),
            ),
            priority: if matches!(action, BrokerAction::Interpret | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let focus_lines = merged_unique(
        &merged_unique(&snapshot.focus_paths, &snapshot.checkpoint_focus_paths),
        &request.focus_paths,
    )
    .into_iter()
    .map(|path| format!("- path: {path}"))
    .chain(
        merged_unique(
            &merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols),
            &request.focus_symbols,
        )
        .into_iter()
        .map(|symbol| format!("- symbol: {symbol}")),
    )
    .collect::<Vec<_>>();
    if !focus_lines.is_empty() {
        sections.push(BrokerSection {
            id: "current_focus".to_string(),
            title: "Current Focus".to_string(),
            body: truncate_lines(
                focus_lines,
                section_item_limit(&effective_limits, "current_focus"),
            ),
            priority: if matches!(action, BrokerAction::Inspect | BrokerAction::Edit) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let discovered_scope_lines = snapshot
        .read_paths_by_tool
        .iter()
        .flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- read via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        })
        .chain(snapshot.edited_paths_by_tool.iter().flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- edited via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        }))
        .chain(
            merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols)
                .into_iter()
                .map(|symbol| format!("- symbol: {symbol}")),
        )
        .collect::<Vec<_>>();
    if !discovered_scope_lines.is_empty() {
        sections.push(BrokerSection {
            id: "discovered_scope".to_string(),
            title: "Discovered Scope".to_string(),
            body: truncate_lines(
                discovered_scope_lines,
                section_item_limit(&effective_limits, "discovered_scope"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Plan
                    | BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Edit
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.recent_tool_invocations.is_empty() {
        let lines = render_recent_tool_activity_lines(snapshot, false);
        sections.push(BrokerSection {
            id: "recent_tool_activity".to_string(),
            title: "Recent Tool Activity".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "recent_tool_activity"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Interpret
                    | BrokerAction::Edit
                    | BrokerAction::Summarize
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.tool_failures.is_empty() {
        let lines = snapshot
            .tool_failures
            .iter()
            .rev()
            .map(|failure| {
                format!(
                    "- #{} {} [{}] {}",
                    failure.sequence,
                    failure.tool_name,
                    serde_json::to_string(&failure.operation_kind)
                        .unwrap_or_else(|_| "\"generic\"".to_string())
                        .trim_matches('"'),
                    failure
                        .error_message
                        .as_deref()
                        .or(failure.error_class.as_deref())
                        .unwrap_or("tool failed")
                )
            })
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "tool_failures".to_string(),
            title: "Tool Failures".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "tool_failures"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.evidence_artifact_ids.is_empty() {
        sections.push(BrokerSection {
            id: "evidence_cache".to_string(),
            title: "Evidence Cache".to_string(),
            body: truncate_lines(
                snapshot
                    .evidence_artifact_ids
                    .iter()
                    .map(|artifact_id| format!("- artifact: {artifact_id}"))
                    .collect(),
                section_item_limit(&effective_limits, "evidence_cache"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
    {
        let body = snapshot
            .changed_paths_since_checkpoint
            .iter()
            .map(|path| format!("- changed path: {path}"))
            .chain(
                snapshot
                    .changed_symbols_since_checkpoint
                    .iter()
                    .map(|symbol| format!("- changed symbol: {symbol}")),
            )
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "checkpoint_deltas".to_string(),
            title: "Checkpoint Deltas".to_string(),
            body: truncate_lines(
                body,
                section_item_limit(&effective_limits, "checkpoint_deltas"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if should_run_reducer_search(&allowed_sections) {
        let search_execution = build_reducer_search_execution(
            Some(state),
            root,
            snapshot,
            request,
            &query_focus,
            action,
            section_item_limit(&effective_limits, "search_evidence").max(8),
            section_item_limit(&effective_limits, "code_evidence").min(15),
        );
        let reducer_files = search_execution.files;
        if !reducer_files.is_empty() {
            let evidence_by_file = search_execution.evidence_by_file;
            let lines = reducer_files
                .iter()
                .map(|file| {
                    let line_hint = file
                        .preview_matches
                        .first()
                        .map(|(line, _)| format!(":{line}"))
                        .unwrap_or_default();
                    let terms = file
                        .matched_terms
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "- {}{} [matches={}] — direct reducer hit for {}",
                        file.path, line_hint, file.match_count, terms
                    )
                })
                .collect::<Vec<_>>();
            if !lines.is_empty() {
                sections.push(BrokerSection {
                    id: "search_evidence".to_string(),
                    title: "Relevant Files".to_string(),
                    body: truncate_lines(
                        lines,
                        section_item_limit(&effective_limits, "search_evidence"),
                    ),
                    priority: if matches!(
                        action,
                        BrokerAction::Plan | BrokerAction::Inspect | BrokerAction::ChooseTool
                    ) {
                        1
                    } else {
                        2
                    },
                    source_kind: BrokerSourceKind::Derived,
                });
            }

            let evidence_lines = reducer_files
                .iter()
                .flat_map(|file| {
                    evidence_by_file
                        .get(&file.path)
                        .map(|summary| summary.rendered_lines.clone())
                        .unwrap_or_default()
                })
                .take(15)
                .collect::<Vec<_>>();
            if !evidence_lines.is_empty() {
                sections.push(BrokerSection {
                    id: "code_evidence".to_string(),
                    title: "Code Evidence".to_string(),
                    body: evidence_lines.join("\n"),
                    priority: if matches!(
                        action,
                        BrokerAction::Inspect
                            | BrokerAction::Interpret
                            | BrokerAction::Edit
                            | BrokerAction::ChooseTool
                    ) {
                        1
                    } else {
                        2
                    },
                    source_kind: BrokerSourceKind::Derived,
                });
            }
        }
    }

    if manage.is_some_and(|manage| {
        !manage.working_set.is_empty() || !manage.recommended_packets.is_empty()
    }) {
        let manage = manage.expect("manage checked above");
        let packets = if !manage.working_set.is_empty() {
            &manage.working_set
        } else {
            &manage.recommended_packets
        };
        let visible_packets = packets
            .iter()
            .filter(|packet| {
                request.include_self_context
                    || packet_source_kind(packet) != BrokerSourceKind::SelfAuthored
            })
            .map(|packet| {
                let summary = packet.summary.as_deref().unwrap_or("no summary");
                format!("- {} [{}] {}", packet.target, packet.cache_key, summary)
            })
            .collect::<Vec<_>>();
        if !visible_packets.is_empty() {
            sections.push(BrokerSection {
                id: "relevant_context".to_string(),
                title: "Relevant Context".to_string(),
                body: truncate_lines(
                    visible_packets,
                    section_item_limit(&effective_limits, "relevant_context"),
                ),
                priority: if matches!(
                    action,
                    BrokerAction::Plan | BrokerAction::Interpret | BrokerAction::ChooseTool
                ) {
                    1
                } else {
                    2
                },
                source_kind: BrokerSourceKind::External,
            });
        }
    }

    if manage.is_some_and(|manage| !manage.recommended_actions.is_empty()) {
        let manage = manage.expect("manage checked above");
        let title = match request
            .tool_result_kind
            .unwrap_or(BrokerToolResultKind::Generic)
        {
            BrokerToolResultKind::Build => "Build Guidance",
            BrokerToolResultKind::Stack => "Stack Guidance",
            BrokerToolResultKind::Test => "Test Guidance",
            BrokerToolResultKind::Diff => "Diff Guidance",
            BrokerToolResultKind::Generic => "Recommended Actions",
        };
        sections.push(BrokerSection {
            id: "recommended_actions".to_string(),
            title: title.to_string(),
            body: truncate_lines(
                manage
                    .recommended_actions
                    .iter()
                    .map(|action| format!("- {}: {}", action.kind, action.summary))
                    .collect(),
                section_item_limit(&effective_limits, "recommended_actions"),
            ),
            priority: if matches!(action, BrokerAction::ChooseTool | BrokerAction::Interpret) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if matches!(action, BrokerAction::Summarize) {
        let progress = vec![
            format!("- completed steps: {}", snapshot.completed_steps.len()),
            format!("- files read: {}", snapshot.files_read.len()),
            format!("- files edited: {}", snapshot.files_edited.len()),
        ];
        sections.push(BrokerSection {
            id: "progress".to_string(),
            title: "Progress".to_string(),
            body: progress.join("\n"),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    sections.retain(|section| allowed_sections.contains(&section.id));
    sections
}

pub(crate) fn render_brief(
    task_id: &str,
    context_version: &str,
    sections: &[BrokerSection],
) -> String {
    let mut blocks = vec![format!(
        "[Packet28 Context v{context_version} — current Packet28 context for task {task_id}; supersedes all prior Packet28 context for this task]"
    )];
    blocks.extend(
        sections
            .iter()
            .map(|section| format!("## {}\n{}", section.title, section.body)),
    );
    blocks.join("\n\n")
}

pub(crate) fn load_versioned_broker_response(
    root: &Path,
    task_id: &str,
    context_version: &str,
) -> Result<Option<BrokerGetContextResponse>> {
    let path = task_version_json_path(root, task_id, context_version);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read versioned broker response '{}'",
            path.display()
        )
    })?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(crate) fn build_delta(
    current: &[BrokerSection],
    previous: Option<&BrokerGetContextResponse>,
) -> BrokerDeltaResponse {
    let Some(previous) = previous else {
        return BrokerDeltaResponse {
            changed_sections: current.to_vec(),
            removed_section_ids: Vec::new(),
            unchanged_section_ids: Vec::new(),
            full_refresh_required: true,
        };
    };
    let current_by_id = current
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();
    let previous_by_id = previous
        .sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();

    let mut changed_sections = Vec::new();
    let mut unchanged_section_ids = Vec::new();
    for section in current {
        match previous_by_id.get(section.id.as_str()) {
            Some(old) if *old == section => unchanged_section_ids.push(section.id.clone()),
            _ => changed_sections.push(section.clone()),
        }
    }
    let removed_section_ids = previous
        .sections
        .iter()
        .filter(|section| !current_by_id.contains_key(section.id.as_str()))
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    BrokerDeltaResponse {
        changed_sections,
        removed_section_ids,
        unchanged_section_ids,
        full_refresh_required: false,
    }
}

pub(crate) fn build_section_estimates(
    sections: &[BrokerSection],
    changed_ids: &HashSet<String>,
) -> Vec<BrokerSectionEstimate> {
    sections
        .iter()
        .map(|section| {
            let (est_tokens, est_bytes) = estimate_text_cost(&section.body);
            BrokerSectionEstimate {
                id: section.id.clone(),
                est_tokens,
                est_bytes,
                source_kind: section.source_kind,
                changed: changed_ids.contains(&section.id),
            }
        })
        .collect()
}

pub(crate) fn build_eviction_candidates(
    sections: &[BrokerSection],
) -> Vec<BrokerEvictionCandidate> {
    sections
        .iter()
        .filter(|section| {
            matches!(
                section.id.as_str(),
                "relevant_context"
                    | "search_evidence"
                    | "checkpoint_deltas"
                    | "recommended_actions"
            )
        })
        .map(|section| {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            let reason = match section.id.as_str() {
                "relevant_context" => "refreshable evidence".to_string(),
                "search_evidence" => "search evidence can be regenerated".to_string(),
                "checkpoint_deltas" => "checkpoint state can be recomputed".to_string(),
                "recommended_actions" => "guidance can be regenerated".to_string(),
                _ => "refreshable section".to_string(),
            };
            BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason,
                est_tokens,
            }
        })
        .collect()
}

pub(crate) fn should_use_delta_view(
    request: &BrokerGetContextRequest,
    delta: &BrokerDeltaResponse,
    full_sections_len: usize,
) -> bool {
    match broker_request_response_mode(request) {
        BrokerResponseMode::Full => false,
        BrokerResponseMode::Delta => request.since_version.is_some(),
        BrokerResponseMode::Slim | BrokerResponseMode::Auto => {
            request.since_version.is_some()
                && !delta.full_refresh_required
                && !delta.changed_sections.is_empty()
                && delta.changed_sections.len() < full_sections_len
        }
    }
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
