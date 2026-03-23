use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct BrokerMemoryRequest {
    task_id: String,
    memory_kind: String,
    summary: String,
    brief: String,
    context_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_intention_text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recommended_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence_artifact_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    symbols: Vec<String>,
}

pub(crate) fn run_broker_memory_write(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let request: BrokerMemoryRequest =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;
    if request.task_id.trim().is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "packet28.broker_memory.write requires reducer_input.task_id".to_string(),
        });
    }
    if request.summary.trim().is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "packet28.broker_memory.write requires reducer_input.summary".to_string(),
        });
    }

    let payload = json!({
        "packet_type": "suite.packet28.broker_memory.v1",
        "task_id": request.task_id,
        "memory_kind": request.memory_kind,
        "summary": request.summary,
        "brief": request.brief,
        "context_version": request.context_version,
        "artifact_id": request.artifact_id,
        "next_action_summary": request.next_action_summary,
        "latest_intention_text": request.latest_intention_text,
        "recommended_actions": request.recommended_actions,
        "evidence_artifact_ids": request.evidence_artifact_ids,
        "files": request
            .paths
            .iter()
            .map(|path| json!({"path": path}))
            .collect::<Vec<_>>(),
        "symbols": request
            .symbols
            .iter()
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>(),
        "source_tier": "curated_memory",
    });
    let payload_bytes = serde_json::to_vec(&payload)
        .map(|buf| buf.len())
        .unwrap_or(0);
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "packet28".to_string(),
        kind: "broker_memory".to_string(),
        hash: String::new(),
        summary: request.summary.clone(),
        files: request
            .paths
            .iter()
            .map(|path| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(1.0),
                source: Some("packet28.broker_memory.write".to_string()),
            })
            .collect(),
        symbols: request
            .symbols
            .iter()
            .map(|name| suite_packet_core::SymbolRef {
                name: name.clone(),
                file: None,
                kind: Some("broker_context_symbol".to_string()),
                relevance: Some(1.0),
                source: Some("packet28.broker_memory.write".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(0.95),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![format!("task:{}", request.task_id)],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "packet28-broker-memory-{}",
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
            "tool": "packet28",
            "reducer": "packet28.broker_memory.write",
            "kind": "broker_memory",
            "task_id": request.task_id,
            "memory_kind": request.memory_kind,
            "context_version": request.context_version,
            "artifact_id": request.artifact_id,
            "hash": envelope.hash,
            "source_tier": "curated_memory",
        }),
    };

    ctx.set_shared("task_id", Value::String(request.task_id.clone()));
    ctx.set_shared("memory_kind", Value::String(request.memory_kind.clone()));

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "packet28.broker_memory.write",
            "task_id": request.task_id,
            "memory_kind": request.memory_kind,
            "context_version": request.context_version,
        }),
    })
}
