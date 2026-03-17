use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct AgentSnapshotRequest {
    task_id: String,
}

pub(crate) fn build_agent_state_packet(
    target: &str,
    event: &suite_packet_core::AgentStateEventPayload,
    source: &str,
) -> Result<
    (
        suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload>,
        KernelPacket,
    ),
    KernelError,
> {
    let payload_bytes = serde_json::to_vec(event).unwrap_or_default().len();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "agenty".to_string(),
        kind: "agent_state".to_string(),
        hash: String::new(),
        summary: summarize_agent_state_event(event),
        files: event
            .paths
            .iter()
            .map(|path| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(1.0),
                source: Some(source.to_string()),
            })
            .collect(),
        symbols: event
            .symbols
            .iter()
            .map(|name| suite_packet_core::SymbolRef {
                name: name.clone(),
                file: None,
                kind: Some("focus_symbol".to_string()),
                relevance: Some(1.0),
                source: Some(source.to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(1.0),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![format!("task:{}", event.task_id)],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload: event.clone(),
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "agenty-state-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: target.to_string(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": "agenty",
            "reducer": source,
            "kind": "agent_state",
            "task_id": event.task_id,
            "event_id": event.event_id,
            "event_kind": event.kind,
            "hash": envelope.hash,
        }),
    };

    Ok((envelope, packet))
}

pub(crate) fn run_agenty_state_write(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let event: suite_packet_core::AgentStateEventPayload =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;
    validate_agent_state_event(&event).map_err(|detail| KernelError::InvalidRequest { detail })?;
    let (envelope, packet) = build_agent_state_packet(&ctx.target, &event, "agenty.state.write")?;

    ctx.set_shared("task_id", Value::String(event.task_id.clone()));
    ctx.set_shared("event_id", Value::String(event.event_id.clone()));

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "agenty.state.write",
            "task_id": envelope.payload.task_id,
            "event_kind": envelope.payload.kind,
        }),
    })
}

pub(crate) fn run_agenty_state_snapshot(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: AgentSnapshotRequest =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;
    if input.task_id.trim().is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "agenty.state.snapshot requires reducer_input.task_id".to_string(),
        });
    }

    let entries = ctx.cache_entries()?;
    let payload = derive_agent_snapshot(&entries, &input.task_id);
    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "agenty".to_string(),
        kind: "agent_snapshot".to_string(),
        hash: String::new(),
        summary: format!(
            "agent snapshot task={} events={} questions={}",
            payload.task_id,
            payload.event_count,
            payload.open_questions.len()
        ),
        files: payload
            .focus_paths
            .iter()
            .chain(payload.files_read.iter())
            .map(|path| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(1.0),
                source: Some("agenty.state.snapshot".to_string()),
            })
            .collect(),
        symbols: payload
            .focus_symbols
            .iter()
            .map(|name| suite_packet_core::SymbolRef {
                name: name.clone(),
                file: None,
                kind: Some("focus_symbol".to_string()),
                relevance: Some(1.0),
                source: Some("agenty.state.snapshot".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(1.0),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![format!("task:{}", payload.task_id)],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload: payload.clone(),
    }
    .with_canonical_hash_and_real_budget();

    ctx.set_shared("task_id", Value::String(payload.task_id.clone()));
    ctx.set_shared(
        "state_events",
        Value::from(envelope.payload.event_count as u64),
    );

    let packet = KernelPacket {
        packet_id: Some(format!(
            "agenty-snapshot-{}",
            envelope.hash.chars().take(12).collect::<String>()
        )),
        format: default_packet_format(),
        body: serde_json::to_value(&envelope).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: Some(envelope.budget_cost.est_tokens),
        runtime_ms: Some(envelope.budget_cost.runtime_ms),
        metadata: json!({
            "tool": "agenty",
            "reducer": "state.snapshot",
            "kind": "agent_snapshot",
            "task_id": payload.task_id,
            "event_count": payload.event_count,
            "hash": envelope.hash,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "agenty.state.snapshot",
            "task_id": envelope.payload.task_id,
            "event_count": envelope.payload.event_count,
        }),
    })
}

pub(crate) fn load_agent_snapshot(
    ctx: &ExecutionContext,
) -> Result<Option<suite_packet_core::AgentSnapshotPayload>, KernelError> {
    let Some(task_id) = ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
    else {
        return Ok(None);
    };

    let entries = ctx.cache_entries()?;
    Ok(Some(derive_agent_snapshot(&entries, task_id)))
}

pub(crate) fn parse_recall_scope(value: Option<&Value>, default: RecallScope) -> RecallScope {
    match value.and_then(Value::as_str).unwrap_or_default() {
        "task_first" => RecallScope::TaskFirst,
        "task_only" => RecallScope::TaskOnly,
        "global" => RecallScope::Global,
        _ => default,
    }
}

pub(crate) fn load_task_scoped_packets(
    ctx: &ExecutionContext,
    task_id: &str,
    input_packets: &[KernelPacket],
) -> Result<(Vec<KernelPacket>, Value), KernelError> {
    let workspace_root = kernel_workspace_root();
    let refs = input_packets_ref_query(input_packets);
    let cache = ctx.memory.lock().map_err(|source| KernelError::CacheLock {
        detail: source.to_string(),
    })?;
    let matches = cache.related_entries(Some(task_id), &refs.paths, &refs.symbols, &refs.tests);
    drop(cache);

    let mut packets = Vec::new();
    let mut debug_matches = Vec::new();
    for related in matches {
        for packet in related.entry.packets {
            packets.push(KernelPacket {
                packet_id: packet.packet_id,
                format: default_packet_format(),
                body: packet.body,
                token_usage: packet.token_usage,
                runtime_ms: packet.runtime_ms,
                metadata: packet.metadata,
            });
        }
        debug_matches.push(json!({
            "cache_key": related.entry.cache_key,
            "canonical_path_matches": related.canonical_path_matches,
            "basename_path_matches": related.basename_path_matches,
            "symbol_matches": related.symbol_matches,
            "test_matches": related.test_matches,
        }));
    }
    Ok((
        packets,
        json!({
            "task_id": task_id,
            "workspace_root": workspace_root.map(|path| path.to_string_lossy().to_string()),
            "related_cache_entries": debug_matches,
        }),
    ))
}

pub(crate) fn validate_agent_state_event(
    event: &suite_packet_core::AgentStateEventPayload,
) -> Result<(), String> {
    if event.task_id.trim().is_empty() {
        return Err("task_id cannot be empty".to_string());
    }
    if event.event_id.trim().is_empty() {
        return Err("event_id cannot be empty".to_string());
    }
    if event.actor.trim().is_empty() {
        return Err("actor cannot be empty".to_string());
    }

    match (&event.kind, &event.data) {
        (
            suite_packet_core::AgentStateEventKind::FocusSet,
            suite_packet_core::AgentStateEventData::FocusSet { .. },
        ) => {
            if event.paths.is_empty() && event.symbols.is_empty() {
                return Err("focus_set requires paths or symbols".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::FocusCleared,
            suite_packet_core::AgentStateEventData::FocusCleared { clear_all },
        ) => {
            if !*clear_all && event.paths.is_empty() && event.symbols.is_empty() {
                return Err(
                    "focus_cleared requires clear_all=true or explicit paths/symbols".to_string(),
                );
            }
        }
        (
            suite_packet_core::AgentStateEventKind::FileRead,
            suite_packet_core::AgentStateEventData::FileRead {},
        ) => {
            if event.paths.is_empty() {
                return Err("file_read requires at least one path".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::FileEdited,
            suite_packet_core::AgentStateEventData::FileEdited { .. },
        ) => {
            if event.paths.is_empty() {
                return Err("file_edited requires at least one path".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::CheckpointSaved,
            suite_packet_core::AgentStateEventData::CheckpointSaved { checkpoint_id, .. },
        ) => {
            if checkpoint_id.trim().is_empty() {
                return Err("checkpoint_saved requires a non-empty checkpoint_id".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::DecisionAdded,
            suite_packet_core::AgentStateEventData::DecisionAdded {
                decision_id, text, ..
            },
        ) => {
            if decision_id.trim().is_empty() || text.trim().is_empty() {
                return Err("decision_added requires non-empty decision_id and text".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::DecisionSuperseded,
            suite_packet_core::AgentStateEventData::DecisionSuperseded { decision_id, .. },
        ) => {
            if decision_id.trim().is_empty() {
                return Err("decision_superseded requires a non-empty decision_id".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::StepCompleted,
            suite_packet_core::AgentStateEventData::StepCompleted { step_id },
        ) => {
            if step_id.trim().is_empty() {
                return Err("step_completed requires a non-empty step_id".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::QuestionOpened,
            suite_packet_core::AgentStateEventData::QuestionOpened { question_id, text },
        ) => {
            if question_id.trim().is_empty() || text.trim().is_empty() {
                return Err("question_opened requires non-empty question_id and text".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::QuestionResolved,
            suite_packet_core::AgentStateEventData::QuestionResolved { question_id },
        ) => {
            if question_id.trim().is_empty() {
                return Err("question_resolved requires a non-empty question_id".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::ToolInvocationStarted,
            suite_packet_core::AgentStateEventData::ToolInvocationStarted {
                invocation_id,
                sequence,
                tool_name,
                ..
            },
        ) => {
            if invocation_id.trim().is_empty() || tool_name.trim().is_empty() || *sequence == 0 {
                return Err(
                    "tool_invocation_started requires invocation_id, tool_name, and sequence"
                        .to_string(),
                );
            }
        }
        (
            suite_packet_core::AgentStateEventKind::ToolInvocationCompleted,
            suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
                invocation_id,
                sequence,
                tool_name,
                ..
            },
        ) => {
            if invocation_id.trim().is_empty() || tool_name.trim().is_empty() || *sequence == 0 {
                return Err(
                    "tool_invocation_completed requires invocation_id, tool_name, and sequence"
                        .to_string(),
                );
            }
        }
        (
            suite_packet_core::AgentStateEventKind::ToolInvocationFailed,
            suite_packet_core::AgentStateEventData::ToolInvocationFailed {
                invocation_id,
                sequence,
                tool_name,
                ..
            },
        ) => {
            if invocation_id.trim().is_empty() || tool_name.trim().is_empty() || *sequence == 0 {
                return Err(
                    "tool_invocation_failed requires invocation_id, tool_name, and sequence"
                        .to_string(),
                );
            }
        }
        (
            suite_packet_core::AgentStateEventKind::FocusInferred,
            suite_packet_core::AgentStateEventData::FocusInferred { .. },
        ) => {
            if event.paths.is_empty() && event.symbols.is_empty() {
                return Err("focus_inferred requires paths or symbols".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::EvidenceCaptured,
            suite_packet_core::AgentStateEventData::EvidenceCaptured { artifact_id, .. },
        ) => {
            if artifact_id.trim().is_empty() {
                return Err("evidence_captured requires a non-empty artifact_id".to_string());
            }
        }
        (
            suite_packet_core::AgentStateEventKind::IntentionRecorded,
            suite_packet_core::AgentStateEventData::IntentionRecorded { text, .. },
        ) => {
            if text.trim().is_empty() {
                return Err("intention_recorded requires non-empty text".to_string());
            }
        }
        _ => {
            return Err(format!(
                "event kind '{:?}' does not match payload variant",
                event.kind
            ));
        }
    }

    Ok(())
}

pub(crate) fn summarize_agent_state_event(
    event: &suite_packet_core::AgentStateEventPayload,
) -> String {
    match &event.data {
        suite_packet_core::AgentStateEventData::FocusSet { .. } => format!(
            "focus set task={} paths={} symbols={}",
            event.task_id,
            event.paths.len(),
            event.symbols.len()
        ),
        suite_packet_core::AgentStateEventData::FocusCleared { clear_all } => format!(
            "focus cleared task={} all={} paths={} symbols={}",
            event.task_id,
            clear_all,
            event.paths.len(),
            event.symbols.len()
        ),
        suite_packet_core::AgentStateEventData::FileRead {} => format!(
            "file read task={} paths={}",
            event.task_id,
            event.paths.join(", ")
        ),
        suite_packet_core::AgentStateEventData::FileEdited { .. } => format!(
            "file edited task={} paths={}",
            event.task_id,
            event.paths.join(", ")
        ),
        suite_packet_core::AgentStateEventData::CheckpointSaved { checkpoint_id, .. } => format!(
            "checkpoint saved task={} checkpoint={}",
            event.task_id, checkpoint_id
        ),
        suite_packet_core::AgentStateEventData::DecisionAdded {
            decision_id, text, ..
        } => format!(
            "decision added task={} id={} text={}",
            event.task_id, decision_id, text
        ),
        suite_packet_core::AgentStateEventData::DecisionSuperseded { decision_id, .. } => format!(
            "decision superseded task={} id={}",
            event.task_id, decision_id
        ),
        suite_packet_core::AgentStateEventData::StepCompleted { step_id } => {
            format!("step completed task={} step={}", event.task_id, step_id)
        }
        suite_packet_core::AgentStateEventData::QuestionOpened {
            question_id, text, ..
        } => format!(
            "question opened task={} id={} text={}",
            event.task_id, question_id, text
        ),
        suite_packet_core::AgentStateEventData::QuestionResolved { question_id } => format!(
            "question resolved task={} id={}",
            event.task_id, question_id
        ),
        suite_packet_core::AgentStateEventData::ToolInvocationStarted {
            tool_name,
            sequence,
            compact_path,
            passthrough_reason,
            ..
        } => format!(
            "tool invocation started task={} seq={} tool={} route={} reason={}",
            event.task_id,
            sequence,
            tool_name,
            compact_path.as_deref().unwrap_or("unknown"),
            passthrough_reason.as_deref().unwrap_or("n/a")
        ),
        suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
            tool_name,
            sequence,
            operation_kind,
            compact_path,
            ..
        } => format!(
            "tool invocation completed task={} seq={} tool={} kind={:?} route={}",
            event.task_id,
            sequence,
            tool_name,
            operation_kind,
            compact_path.as_deref().unwrap_or("unknown")
        ),
        suite_packet_core::AgentStateEventData::ToolInvocationFailed {
            tool_name,
            sequence,
            error_class,
            compact_path,
            ..
        } => format!(
            "tool invocation failed task={} seq={} tool={} route={} error={}",
            event.task_id,
            sequence,
            tool_name,
            compact_path.as_deref().unwrap_or("unknown"),
            error_class.as_deref().unwrap_or("unknown")
        ),
        suite_packet_core::AgentStateEventData::FocusInferred { .. } => format!(
            "focus inferred task={} paths={} symbols={}",
            event.task_id,
            event.paths.len(),
            event.symbols.len()
        ),
        suite_packet_core::AgentStateEventData::EvidenceCaptured { artifact_id, .. } => format!(
            "evidence captured task={} artifact={}",
            event.task_id, artifact_id
        ),
        suite_packet_core::AgentStateEventData::IntentionRecorded { text, step_id, .. } => {
            let phase = step_id.as_deref().unwrap_or("unspecified");
            format!(
                "intention recorded task={} phase={} text={}",
                event.task_id, phase, text
            )
        }
    }
}

pub(crate) fn derive_agent_snapshot(
    entries: &[context_memory_core::PacketCacheEntry],
    task_id: &str,
) -> suite_packet_core::AgentSnapshotPayload {
    let mut events = entries
        .iter()
        .flat_map(extract_agent_state_events)
        .filter(|event| event.task_id == task_id)
        .collect::<Vec<_>>();

    events.sort_by(|a, b| {
        a.occurred_at_unix
            .cmp(&b.occurred_at_unix)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });

    let mut focus_paths = std::collections::BTreeSet::new();
    let mut focus_symbols = std::collections::BTreeSet::new();
    let mut files_read = std::collections::BTreeSet::new();
    let mut files_edited = std::collections::BTreeSet::new();
    let mut decisions = std::collections::BTreeMap::<String, String>::new();
    let mut completed_steps = std::collections::BTreeSet::new();
    let mut open_questions = std::collections::BTreeMap::<String, String>::new();
    let mut last_event_at_unix = None;
    let mut latest_checkpoint_id = None;
    let mut latest_checkpoint_at_unix = None;
    let mut checkpoint_note = None;
    let mut checkpoint_focus_paths = std::collections::BTreeSet::new();
    let mut checkpoint_focus_symbols = std::collections::BTreeSet::new();
    let mut changed_paths_since_checkpoint = std::collections::BTreeSet::new();
    let mut changed_symbols_since_checkpoint = std::collections::BTreeSet::new();
    let mut recent_tool_invocations = Vec::new();
    let mut tool_failures = Vec::new();
    let mut read_paths_by_tool = std::collections::BTreeMap::<
        (String, suite_packet_core::ToolOperationKind),
        std::collections::BTreeSet<String>,
    >::new();
    let mut edited_paths_by_tool = std::collections::BTreeMap::<
        (String, suite_packet_core::ToolOperationKind),
        std::collections::BTreeSet<String>,
    >::new();
    let mut search_queries = std::collections::BTreeSet::<(String, String)>::new();
    let mut evidence_artifact_ids = std::collections::BTreeSet::new();
    let mut last_successful_tool_by_kind = std::collections::BTreeMap::<
        suite_packet_core::ToolOperationKind,
        suite_packet_core::ToolKindSuccess,
    >::new();
    let mut latest_intention = None;

    for event in &events {
        last_event_at_unix = Some(event.occurred_at_unix);
        match &event.data {
            suite_packet_core::AgentStateEventData::FocusSet { .. } => {
                for path in &event.paths {
                    focus_paths.insert(path.clone());
                }
                for symbol in &event.symbols {
                    focus_symbols.insert(symbol.clone());
                }
            }
            suite_packet_core::AgentStateEventData::FocusCleared { clear_all } => {
                if *clear_all {
                    focus_paths.clear();
                    focus_symbols.clear();
                } else {
                    for path in &event.paths {
                        focus_paths.remove(path);
                    }
                    for symbol in &event.symbols {
                        focus_symbols.remove(symbol);
                    }
                }
            }
            suite_packet_core::AgentStateEventData::FileRead {} => {
                for path in &event.paths {
                    files_read.insert(path.clone());
                }
            }
            suite_packet_core::AgentStateEventData::FileEdited { .. } => {
                for path in &event.paths {
                    files_edited.insert(path.clone());
                    changed_paths_since_checkpoint.insert(path.clone());
                }
                for symbol in &event.symbols {
                    changed_symbols_since_checkpoint.insert(symbol.clone());
                }
            }
            suite_packet_core::AgentStateEventData::CheckpointSaved {
                checkpoint_id,
                note,
            } => {
                latest_checkpoint_id = Some(checkpoint_id.clone());
                latest_checkpoint_at_unix = Some(event.occurred_at_unix);
                checkpoint_note = note.clone();
                checkpoint_focus_paths = event.paths.iter().cloned().collect();
                checkpoint_focus_symbols = event.symbols.iter().cloned().collect();
                changed_paths_since_checkpoint.clear();
                changed_symbols_since_checkpoint.clear();
            }
            suite_packet_core::AgentStateEventData::DecisionAdded {
                decision_id,
                text,
                supersedes,
            } => {
                if let Some(previous) = supersedes {
                    decisions.remove(previous);
                }
                decisions.insert(decision_id.clone(), text.clone());
            }
            suite_packet_core::AgentStateEventData::DecisionSuperseded { decision_id, .. } => {
                decisions.remove(decision_id);
            }
            suite_packet_core::AgentStateEventData::StepCompleted { step_id } => {
                completed_steps.insert(step_id.clone());
            }
            suite_packet_core::AgentStateEventData::QuestionOpened { question_id, text } => {
                open_questions.insert(question_id.clone(), text.clone());
            }
            suite_packet_core::AgentStateEventData::QuestionResolved { question_id } => {
                open_questions.remove(question_id);
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
                compact_preview,
                request_fingerprint,
                compact_path,
                passthrough_reason,
                raw_est_tokens,
                reduced_est_tokens,
                search_query,
                command,
                artifact_id,
                raw_artifact_handle,
                raw_artifact_available,
                regions,
                duration_ms,
            } => {
                if !event.paths.is_empty() || !event.symbols.is_empty() {
                    for path in &event.paths {
                        focus_paths.insert(path.clone());
                    }
                    for symbol in &event.symbols {
                        focus_symbols.insert(symbol.clone());
                    }
                }
                match operation_kind {
                    suite_packet_core::ToolOperationKind::Read => {
                        for path in &event.paths {
                            files_read.insert(path.clone());
                        }
                        let entry = read_paths_by_tool
                            .entry((tool_name.clone(), *operation_kind))
                            .or_default();
                        for path in &event.paths {
                            entry.insert(path.clone());
                        }
                    }
                    suite_packet_core::ToolOperationKind::Edit => {
                        for path in &event.paths {
                            files_edited.insert(path.clone());
                            changed_paths_since_checkpoint.insert(path.clone());
                        }
                        for symbol in &event.symbols {
                            changed_symbols_since_checkpoint.insert(symbol.clone());
                        }
                        let entry = edited_paths_by_tool
                            .entry((tool_name.clone(), *operation_kind))
                            .or_default();
                        for path in &event.paths {
                            entry.insert(path.clone());
                        }
                    }
                    _ => {}
                }
                if let Some(query) = search_query
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                {
                    search_queries.insert((tool_name.clone(), query.clone()));
                }
                if let Some(artifact_id) = artifact_id
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                {
                    evidence_artifact_ids.insert(artifact_id.clone());
                }
                recent_tool_invocations.push(suite_packet_core::ToolInvocationSummary {
                    invocation_id: invocation_id.clone(),
                    sequence: *sequence,
                    tool_name: tool_name.clone(),
                    server_name: server_name.clone(),
                    operation_kind: *operation_kind,
                    request_summary: request_summary.clone(),
                    result_summary: result_summary.clone(),
                    compact_preview: compact_preview.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    compact_path: compact_path.clone(),
                    passthrough_reason: passthrough_reason.clone(),
                    raw_est_tokens: *raw_est_tokens,
                    reduced_est_tokens: *reduced_est_tokens,
                    search_query: search_query.clone(),
                    command: command.clone(),
                    artifact_id: artifact_id.clone(),
                    raw_artifact_handle: raw_artifact_handle.clone(),
                    raw_artifact_available: *raw_artifact_available,
                    paths: event.paths.clone(),
                    regions: regions.clone(),
                    symbols: event.symbols.clone(),
                    duration_ms: *duration_ms,
                    occurred_at_unix: event.occurred_at_unix,
                });
                last_successful_tool_by_kind.insert(
                    *operation_kind,
                    suite_packet_core::ToolKindSuccess {
                        operation_kind: *operation_kind,
                        tool_name: tool_name.clone(),
                        invocation_id: invocation_id.clone(),
                    },
                );
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
                compact_path,
                passthrough_reason,
                raw_est_tokens,
                reduced_est_tokens,
                raw_artifact_handle,
                raw_artifact_available,
                retryable,
                duration_ms,
            } => {
                tool_failures.push(suite_packet_core::ToolFailureSummary {
                    invocation_id: invocation_id.clone(),
                    sequence: *sequence,
                    tool_name: tool_name.clone(),
                    server_name: server_name.clone(),
                    operation_kind: *operation_kind,
                    request_summary: request_summary.clone(),
                    error_class: error_class.clone(),
                    error_message: error_message.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    compact_path: compact_path.clone(),
                    passthrough_reason: passthrough_reason.clone(),
                    raw_est_tokens: *raw_est_tokens,
                    reduced_est_tokens: *reduced_est_tokens,
                    raw_artifact_handle: raw_artifact_handle.clone(),
                    raw_artifact_available: *raw_artifact_available,
                    retryable: *retryable,
                    duration_ms: *duration_ms,
                    occurred_at_unix: event.occurred_at_unix,
                });
            }
            suite_packet_core::AgentStateEventData::FocusInferred { .. } => {
                for path in &event.paths {
                    focus_paths.insert(path.clone());
                }
                for symbol in &event.symbols {
                    focus_symbols.insert(symbol.clone());
                }
            }
            suite_packet_core::AgentStateEventData::EvidenceCaptured { artifact_id, .. } => {
                evidence_artifact_ids.insert(artifact_id.clone());
            }
            suite_packet_core::AgentStateEventData::IntentionRecorded {
                text,
                note,
                step_id,
                question_id,
            } => {
                for path in &event.paths {
                    focus_paths.insert(path.clone());
                }
                for symbol in &event.symbols {
                    focus_symbols.insert(symbol.clone());
                }
                latest_intention = Some(suite_packet_core::AgentIntention {
                    text: text.clone(),
                    note: note.clone(),
                    step_id: step_id.clone(),
                    question_id: question_id.clone(),
                    paths: event.paths.clone(),
                    symbols: event.symbols.clone(),
                    occurred_at_unix: event.occurred_at_unix,
                });
            }
        }
    }

    recent_tool_invocations.sort_by(|a, b| {
        a.sequence
            .cmp(&b.sequence)
            .then_with(|| a.occurred_at_unix.cmp(&b.occurred_at_unix))
            .then_with(|| a.invocation_id.cmp(&b.invocation_id))
    });
    if recent_tool_invocations.len() > 12 {
        let keep_from = recent_tool_invocations.len() - 12;
        recent_tool_invocations = recent_tool_invocations.split_off(keep_from);
    }
    tool_failures.sort_by(|a, b| {
        a.sequence
            .cmp(&b.sequence)
            .then_with(|| a.occurred_at_unix.cmp(&b.occurred_at_unix))
            .then_with(|| a.invocation_id.cmp(&b.invocation_id))
    });
    if tool_failures.len() > 8 {
        let keep_from = tool_failures.len() - 8;
        tool_failures = tool_failures.split_off(keep_from);
    }

    suite_packet_core::AgentSnapshotPayload {
        task_id: task_id.to_string(),
        focus_paths: focus_paths.into_iter().collect(),
        focus_symbols: focus_symbols.into_iter().collect(),
        files_read: files_read.into_iter().collect(),
        files_edited: files_edited.into_iter().collect(),
        active_decisions: decisions
            .into_iter()
            .map(|(id, text)| suite_packet_core::AgentDecision { id, text })
            .collect(),
        completed_steps: completed_steps.into_iter().collect(),
        open_questions: open_questions
            .into_iter()
            .map(|(id, text)| suite_packet_core::AgentQuestion { id, text })
            .collect(),
        event_count: events.len(),
        last_event_at_unix,
        latest_checkpoint_id,
        latest_checkpoint_at_unix,
        checkpoint_note,
        checkpoint_focus_paths: checkpoint_focus_paths.into_iter().collect(),
        checkpoint_focus_symbols: checkpoint_focus_symbols.into_iter().collect(),
        changed_paths_since_checkpoint: changed_paths_since_checkpoint.into_iter().collect(),
        changed_symbols_since_checkpoint: changed_symbols_since_checkpoint.into_iter().collect(),
        recent_tool_invocations,
        tool_failures,
        read_paths_by_tool: read_paths_by_tool
            .into_iter()
            .map(
                |((tool_name, operation_kind), paths)| suite_packet_core::ToolPathSummary {
                    tool_name,
                    operation_kind,
                    paths: paths.into_iter().collect(),
                },
            )
            .collect(),
        edited_paths_by_tool: edited_paths_by_tool
            .into_iter()
            .map(
                |((tool_name, operation_kind), paths)| suite_packet_core::ToolPathSummary {
                    tool_name,
                    operation_kind,
                    paths: paths.into_iter().collect(),
                },
            )
            .collect(),
        search_queries: search_queries
            .into_iter()
            .map(|(tool_name, query)| suite_packet_core::SearchQuerySummary { tool_name, query })
            .collect(),
        evidence_artifact_ids: evidence_artifact_ids.into_iter().collect(),
        last_successful_tool_by_kind: last_successful_tool_by_kind.into_values().collect(),
        latest_intention,
    }
}

pub(crate) fn extract_agent_state_events(
    entry: &context_memory_core::PacketCacheEntry,
) -> Vec<suite_packet_core::AgentStateEventPayload> {
    entry
        .packets
        .iter()
        .filter_map(|packet| {
            serde_json::from_value::<
                suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload>,
            >(packet.body.clone())
            .ok()
            .and_then(|envelope| {
                (envelope.tool == "agenty" && envelope.kind == "agent_state")
                    .then_some(envelope.payload)
            })
        })
        .collect()
}

pub(crate) fn load_agent_snapshot_for_task(
    kernel: &Kernel,
    task_id: &str,
) -> Result<suite_packet_core::AgentSnapshotPayload, KernelError> {
    let entries = kernel
        .memory
        .lock()
        .map_err(|source| KernelError::CacheLock {
            detail: source.to_string(),
        })?
        .entries();
    Ok(derive_agent_snapshot(&entries, task_id))
}
