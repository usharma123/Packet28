use super::*;

#[derive(Default)]
pub(crate) struct CorrelationRefQuery {
    pub(crate) paths: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) tests: Vec<String>,
}

pub(crate) fn input_packets_ref_query(input_packets: &[KernelPacket]) -> CorrelationRefQuery {
    let workspace_root = kernel_workspace_root();
    let mut paths = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let mut tests = BTreeSet::new();

    for packet in input_packets {
        let value = extract_packet_value(&packet.body);
        collect_packet_refs(
            &value,
            workspace_root.as_deref(),
            &mut paths,
            &mut symbols,
            &mut tests,
        );
    }

    CorrelationRefQuery {
        paths: paths.into_iter().collect(),
        symbols: symbols.into_iter().collect(),
        tests: tests.into_iter().collect(),
    }
}

fn build_context_manage_packet(
    target: &str,
    payload: suite_packet_core::ContextManagePayload,
) -> Result<
    (
        suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload>,
        KernelPacket,
    ),
    KernelError,
> {
    let payload_bytes = serde_json::to_vec(&payload)
        .map(|buf| buf.len())
        .unwrap_or(0);
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "contextq".to_string(),
        kind: "context_manage".to_string(),
        hash: String::new(),
        summary: format!(
            "context manage task={} working_set={} evictions={}",
            payload.task_id,
            payload.working_set.len(),
            payload.eviction_candidates.len()
        ),
        files: payload
            .changed_paths_since_checkpoint
            .iter()
            .map(|path| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(1.0),
                source: Some("contextq.manage".to_string()),
            })
            .collect(),
        symbols: payload
            .changed_symbols_since_checkpoint
            .iter()
            .map(|name| suite_packet_core::SymbolRef {
                name: name.clone(),
                file: None,
                kind: Some("changed_since_checkpoint".to_string()),
                relevance: Some(1.0),
                source: Some("contextq.manage".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(0.88),
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
        payload,
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "contextq-manage-{}",
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
            "reducer": "contextq.manage",
            "kind": "context_manage",
            "hash": envelope.hash,
            "working_set_count": envelope.payload.working_set.len(),
            "eviction_count": envelope.payload.eviction_candidates.len(),
        }),
    };

    Ok((envelope, packet))
}

fn derive_manage_query(
    request: &ContextManageRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> String {
    if let Some(query) = request
        .query
        .as_ref()
        .filter(|query| !query.trim().is_empty())
    {
        return query.clone();
    }

    let mut tokens = Vec::new();
    tokens.extend(snapshot.changed_paths_since_checkpoint.iter().cloned());
    tokens.extend(snapshot.changed_symbols_since_checkpoint.iter().cloned());
    tokens.extend(request.focus_paths.iter().take(4).cloned());
    tokens.extend(request.focus_symbols.iter().take(4).cloned());
    tokens.extend(snapshot.focus_paths.iter().take(4).cloned());
    tokens.extend(snapshot.focus_symbols.iter().take(4).cloned());
    tokens.extend(
        snapshot
            .open_questions
            .iter()
            .take(2)
            .map(|question| question.text.clone()),
    );
    if tokens.is_empty() {
        snapshot.task_id.clone()
    } else {
        tokens.join(" ")
    }
}

fn task_memory_requested(policy_context: &Value) -> bool {
    policy_context
        .get("task_memory")
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            policy_context
                .get("include_task_memory")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
}

fn augment_assemble_with_task_memory(
    ctx: &ExecutionContext,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    assemble_packets: &mut Vec<KernelPacket>,
    budget_tokens: u64,
    budget_bytes: usize,
) -> Result<(), KernelError> {
    if !task_memory_requested(&ctx.policy_context) {
        return Ok(());
    }

    let query = derive_manage_query(
        &ContextManageRequest {
            task_id: snapshot.task_id.clone(),
            query: None,
            budget_tokens,
            budget_bytes,
            scope: RecallScope::TaskFirst,
            checkpoint_id: None,
            focus_paths: Vec::new(),
            focus_symbols: Vec::new(),
        },
        snapshot,
    );
    let reserve_tokens = (budget_tokens / 4).max(256);
    let reserve_bytes = (budget_bytes / 4).max(4_096);
    let hits = ctx.cache_recall(
        &query,
        &RecallOptions {
            limit: 8,
            task_id: Some(snapshot.task_id.clone()),
            scope: RecallScope::TaskFirst,
            ..RecallOptions::default()
        },
    )?;
    if hits.is_empty() {
        return Ok(());
    }

    let mut seen = assemble_packets
        .iter()
        .map(|packet| PacketCache::hash_value(&extract_packet_value(&packet.body)))
        .collect::<BTreeSet<_>>();
    let entries = ctx.cache_entries()?;
    let by_key = entries
        .into_iter()
        .map(|entry| (entry.cache_key.clone(), entry))
        .collect::<HashMap<_, _>>();
    let mut used_tokens = 0_u64;
    let mut used_bytes = 0_usize;

    for hit in hits {
        if used_tokens >= reserve_tokens || used_bytes >= reserve_bytes {
            break;
        }
        let Some(entry) = by_key.get(&hit.cache_key) else {
            continue;
        };
        for packet in &entry.packets {
            let key = PacketCache::hash_value(&extract_packet_value(&packet.body));
            if !seen.insert(key) {
                continue;
            }
            let next_tokens = used_tokens.saturating_add(hit.budget_estimate.est_tokens);
            let next_bytes = used_bytes.saturating_add(hit.budget_estimate.est_bytes as usize);
            if next_tokens > reserve_tokens || next_bytes > reserve_bytes {
                break;
            }
            used_tokens = next_tokens;
            used_bytes = next_bytes;
            assemble_packets.push(KernelPacket {
                packet_id: packet.packet_id.clone(),
                format: default_packet_format(),
                body: packet.body.clone(),
                token_usage: packet.token_usage,
                runtime_ms: packet.runtime_ms,
                metadata: packet.metadata.clone(),
            });
        }
    }

    Ok(())
}

pub(crate) fn run_governed_assemble(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let config_path = ctx
        .policy_context
        .get("config_path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| KernelError::InvalidRequest {
            detail: "governed.assemble requires policy_context.config_path".to_string(),
        })?
        .to_string();

    let reducer = run_contextq_assemble(ctx, input_packets)?;
    ctx.set_shared("governed", Value::Bool(true));
    ctx.set_shared("policy_config_path", Value::String(config_path.clone()));
    Ok(ReducerResult {
        output_packets: reducer.output_packets,
        metadata: merge_json(
            reducer.metadata,
            json!({
                "reducer": "governed.assemble",
                "governed": true,
                "config_path": config_path,
            }),
        ),
    })
}

pub(crate) fn run_contextq_correlate(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let task_id = ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
        .map(ToOwned::to_owned);
    let scope = parse_recall_scope(ctx.policy_context.get("scope"), RecallScope::TaskFirst);
    let snapshot = load_agent_snapshot(ctx)?;
    let mut packets = input_packets.to_vec();
    let mut correlation_debug = None;
    if let Some(task_id) = task_id.as_deref() {
        if scope != RecallScope::Global {
            let (task_packets, debug) = load_task_scoped_packets(ctx, task_id, input_packets)?;
            packets.extend(task_packets);
            correlation_debug = Some(debug);
            let mut seen = BTreeSet::new();
            packets.retain(|packet| {
                let key = PacketCache::hash_value(&extract_packet_value(&packet.body));
                seen.insert(key)
            });
        }
    }
    let findings = correlate_packets(&packets, snapshot.as_ref());
    let (envelope, packet) = build_context_correlation_packet(
        &ctx.target,
        task_id.clone(),
        findings,
        correlation_debug,
    )?;

    if let Some(task_id) = task_id {
        ctx.set_shared("task_id", Value::String(task_id));
    }
    ctx.set_shared(
        "correlation_findings",
        Value::from(envelope.payload.finding_count as u64),
    );

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "contextq.correlate",
            "kind": "context_correlate",
            "finding_count": envelope.payload.finding_count,
        }),
    })
}

pub(crate) fn run_contextq_manage(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let request: ContextManageRequest =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;
    if request.task_id.trim().is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "contextq.manage requires reducer_input.task_id".to_string(),
        });
    }

    let snapshot = derive_agent_snapshot(&ctx.cache_entries()?, &request.task_id);
    let query = derive_manage_query(&request, &snapshot);
    let hits = ctx.cache_recall(
        &query,
        &RecallOptions {
            limit: 32,
            task_id: Some(request.task_id.clone()),
            scope: request.scope,
            path_filters: request.focus_paths.clone(),
            symbol_filters: request.focus_symbols.clone(),
            ..RecallOptions::default()
        },
    )?;

    let mut working_set = Vec::new();
    let mut recommended_packets = Vec::new();
    let mut used_tokens = 0_u64;
    let mut used_bytes = 0_usize;
    for hit in &hits {
        let packet_ref = suite_packet_core::ContextManagePacketRef {
            cache_key: hit.cache_key.clone(),
            target: hit.target.clone(),
            score: hit.score,
            summary: hit.summary.clone(),
            reason: hit.match_reasons.first().cloned(),
            packet_types: hit.packet_types.clone(),
            est_tokens: hit.budget_estimate.est_tokens,
            est_bytes: hit.budget_estimate.est_bytes,
            runtime_ms: hit.budget_estimate.runtime_ms,
        };
        if recommended_packets.len() < 5 {
            recommended_packets.push(packet_ref.clone());
        }
        let next_tokens = used_tokens.saturating_add(hit.budget_estimate.est_tokens);
        let next_bytes = used_bytes.saturating_add(hit.budget_estimate.est_bytes as usize);
        if next_tokens <= request.budget_tokens && next_bytes <= request.budget_bytes {
            used_tokens = next_tokens;
            used_bytes = next_bytes;
            working_set.push(packet_ref);
        }
    }

    let eviction_candidates = hits
        .iter()
        .skip(working_set.len())
        .take(8)
        .map(|hit| suite_packet_core::ContextManagePacketRef {
            cache_key: hit.cache_key.clone(),
            target: hit.target.clone(),
            score: hit.score,
            summary: hit.summary.clone(),
            reason: Some("outside_working_set_budget".to_string()),
            packet_types: hit.packet_types.clone(),
            est_tokens: hit.budget_estimate.est_tokens,
            est_bytes: hit.budget_estimate.est_bytes,
            runtime_ms: hit.budget_estimate.runtime_ms,
        })
        .collect::<Vec<_>>();

    let mut recommended_actions = Vec::new();
    if !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
    {
        recommended_actions.push(suite_packet_core::ContextManageRecommendedAction {
            kind: "rerun".to_string(),
            summary: "rerun correlate and assemble for checkpoint deltas".to_string(),
            related_paths: snapshot.changed_paths_since_checkpoint.clone(),
            related_symbols: snapshot.changed_symbols_since_checkpoint.clone(),
        });
    }
    if !snapshot.open_questions.is_empty() {
        recommended_actions.push(suite_packet_core::ContextManageRecommendedAction {
            kind: "question".to_string(),
            summary: format!("resolve {} open question(s)", snapshot.open_questions.len()),
            related_paths: Vec::new(),
            related_symbols: Vec::new(),
        });
    }

    let payload = suite_packet_core::ContextManagePayload {
        task_id: request.task_id.clone(),
        query: Some(query),
        budget: suite_packet_core::ContextManageBudgetSummary {
            requested_tokens: request.budget_tokens,
            requested_bytes: request.budget_bytes,
            working_set_tokens: used_tokens,
            working_set_bytes: used_bytes,
            evictable_tokens: eviction_candidates.iter().map(|item| item.est_tokens).sum(),
            evictable_bytes: eviction_candidates
                .iter()
                .map(|item| item.est_bytes as usize)
                .sum(),
            reserved_headroom_tokens: request.budget_tokens.saturating_sub(used_tokens),
            reserved_headroom_bytes: request.budget_bytes.saturating_sub(used_bytes),
        },
        working_set,
        eviction_candidates,
        recommended_packets,
        recommended_actions,
        changed_paths_since_checkpoint: snapshot.changed_paths_since_checkpoint.clone(),
        changed_symbols_since_checkpoint: snapshot.changed_symbols_since_checkpoint.clone(),
        open_questions: snapshot.open_questions.clone(),
        active_decisions: snapshot.active_decisions.clone(),
    };

    let (envelope, packet) = build_context_manage_packet(&ctx.target, payload)?;
    ctx.set_shared("task_id", Value::String(request.task_id));
    ctx.set_shared(
        "context_manage",
        json!({
            "working_set_tokens": envelope.payload.budget.working_set_tokens,
            "working_set_bytes": envelope.payload.budget.working_set_bytes,
            "evictable_tokens": envelope.payload.budget.evictable_tokens,
            "evictable_bytes": envelope.payload.budget.evictable_bytes,
        }),
    );

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "contextq.manage",
            "kind": "context_manage",
            "task_id": envelope.payload.task_id,
            "working_set_count": envelope.payload.working_set.len(),
            "eviction_count": envelope.payload.eviction_candidates.len(),
        }),
    })
}

pub(crate) fn run_contextq_assemble(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    if input_packets.is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "contextq.assemble requires at least one input packet".to_string(),
        });
    }

    let agent_snapshot = load_agent_snapshot(ctx)?;

    let options = contextq_core::AssembleOptions {
        budget_tokens: ctx
            .budget
            .token_cap
            .unwrap_or(contextq_core::DEFAULT_BUDGET_TOKENS),
        budget_bytes: ctx
            .budget
            .byte_cap
            .unwrap_or(contextq_core::DEFAULT_BUDGET_BYTES),
        detail_mode: parse_contextq_detail_mode(&ctx.policy_context),
        compact_assembly: ctx
            .policy_context
            .get("compact_assembly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        agent_snapshot: agent_snapshot.clone(),
    };

    let mut assemble_packets = input_packets.to_vec();
    if let Some(snapshot) = agent_snapshot.as_ref() {
        augment_assemble_with_task_memory(
            ctx,
            snapshot,
            &mut assemble_packets,
            options.budget_tokens,
            options.budget_bytes,
        )?;
    }
    if ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
        .is_some()
        && input_packets.len() > 1
    {
        let correlation = run_contextq_correlate(ctx, input_packets)?;
        if let Some(packet) = correlation.output_packets.first() {
            let finding_count = packet
                .body
                .get("payload")
                .and_then(|payload| payload.get("finding_count"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if finding_count > 0 {
                assemble_packets.push(packet.clone());
            }
        }
    }

    let packets: Vec<contextq_core::InputPacket> = assemble_packets
        .iter()
        .enumerate()
        .map(|(idx, packet)| {
            let fallback = packet
                .packet_id
                .clone()
                .unwrap_or_else(|| format!("packet-{}", idx + 1));
            contextq_core::InputPacket::from_value(extract_packet_value(&packet.body), &fallback)
        })
        .collect();

    let assembled = contextq_core::assemble_packets(packets, options);
    let assembled_payload: contextq_core::AssembledPayload =
        serde_json::from_value(assembled.payload.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid assembled payload: {source}"),
            }
        })?;

    ctx.set_shared("truncated", Value::Bool(assembled.assembly.truncated));
    ctx.set_shared(
        "sections_kept",
        Value::from(assembled.assembly.sections_kept as u64),
    );
    if let Some(snapshot) = agent_snapshot {
        ctx.set_shared("task_id", Value::String(snapshot.task_id));
        ctx.set_shared("state_events", Value::from(snapshot.event_count as u64));
    }

    let mut files = Vec::new();
    let mut symbols = Vec::new();
    for reference in &assembled_payload.refs {
        match reference.kind.as_str() {
            "file" | "path" => files.push(suite_packet_core::FileRef {
                path: reference.value.clone(),
                relevance: reference.relevance,
                source: reference.source.clone(),
            }),
            "symbol" => symbols.push(suite_packet_core::SymbolRef {
                name: reference.value.clone(),
                file: None,
                kind: Some("symbol".to_string()),
                relevance: reference.relevance,
                source: reference.source.clone(),
            }),
            _ => {}
        }
    }

    let payload = ContextAssembleEnvelopePayload {
        sources: assembled_payload.sources,
        sections: assembled_payload.sections,
        refs: assembled_payload.refs,
        truncated: assembled_payload.truncated,
        assembly: assembled.assembly.clone(),
        tool_invocations: assembled.tool_invocations.clone(),
        reducer_invocations: assembled.reducer_invocations.clone(),
        text_blobs: assembled.text_blobs.clone(),
        debug: None,
    };
    let payload_bytes = serde_json::to_vec(&payload)
        .map(|buf| buf.len())
        .unwrap_or_default();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "contextq".to_string(),
        kind: "context_assemble".to_string(),
        hash: String::new(),
        summary: format!(
            "context assemble sections={} refs={} truncated={}",
            payload.assembly.sections_kept, payload.assembly.refs_kept, payload.truncated
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(if payload.truncated { 0.8 } else { 1.0 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: assembled.runtime_ms.unwrap_or(0),
            tool_calls: payload.tool_invocations.len() as u64,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: payload.sources.clone(),
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "contextq-{}",
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
            "tool": envelope.tool,
            "reducer": "assemble",
            "truncated": assembled.assembly.truncated,
            "kind": envelope.kind,
            "hash": envelope.hash,
            "schema_version": contextq_core::CONTEXTQ_SCHEMA_VERSION,
            "budget_trim": {
                "truncated": assembled.assembly.truncated,
                "sections_input": assembled.assembly.sections_input,
                "sections_dropped": assembled.assembly.sections_dropped,
                "refs_input": assembled.assembly.refs_input,
                "refs_dropped": assembled.assembly.refs_dropped,
                "estimated_tokens": assembled.assembly.estimated_tokens,
                "estimated_bytes": assembled.assembly.estimated_bytes,
                "budget_tokens": assembled.assembly.budget_tokens,
                "budget_bytes": assembled.assembly.budget_bytes,
            },
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "contextq.assemble",
            "kind": "context_assemble",
            "schema_version": contextq_core::CONTEXTQ_SCHEMA_VERSION,
            "budget_trim": {
                "truncated": assembled.assembly.truncated,
                "sections_input": assembled.assembly.sections_input,
                "sections_dropped": assembled.assembly.sections_dropped,
                "refs_input": assembled.assembly.refs_input,
                "refs_dropped": assembled.assembly.refs_dropped,
                "estimated_tokens": assembled.assembly.estimated_tokens,
                "estimated_bytes": assembled.assembly.estimated_bytes,
                "budget_tokens": assembled.assembly.budget_tokens,
                "budget_bytes": assembled.assembly.budget_bytes,
            },
        }),
    })
}
