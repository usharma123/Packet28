use std::path::Path;

use super::*;

pub(crate) fn load_policy_guard(policy_context: &Value) -> Result<Option<PolicyGuard>, KernelError> {
    let Some(config_path) = policy_context
        .get("config_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(None);
    };

    let config = guardy_core::ContextConfig::load(Path::new(config_path)).map_err(|source| {
        KernelError::InvalidRequest {
            detail: format!("invalid policy_context.config_path: {source}"),
        }
    })?;

    let policy_hash =
        policy_config_hash(&config).map_err(|source| KernelError::InvalidRequest {
            detail: format!("failed to hash policy config: {source}"),
        })?;

    Ok(Some(PolicyGuard {
        config,
        config_path: config_path.to_string(),
        policy_hash,
    }))
}

fn policy_config_hash(config: &guardy_core::ContextConfig) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(config)?;
    normalize_semantic_set_arrays(&mut value);
    Ok(suite_packet_core::canonical_hash_json(&value))
}

fn normalize_semantic_set_arrays(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                normalize_semantic_set_arrays(item);
            }
            if items.iter().all(Value::is_string) {
                let mut normalized = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                normalized.sort();
                normalized.dedup();
                *items = normalized.into_iter().map(Value::String).collect();
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                normalize_semantic_set_arrays(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn should_enforce_policy_for_target(target: &str) -> bool {
    target != "guardy.check"
}

pub(crate) fn enforce_reducer_execution_policy(
    target: &str,
    config: &guardy_core::ContextConfig,
) -> Result<ReducerExecutionAudit, KernelError> {
    let allowlist = config.policy.effective_allowed_reducers();
    if allowlist.is_empty() {
        return Ok(ReducerExecutionAudit {
            reducer: target.to_string(),
            allowed: true,
            matched_by: None,
        });
    }

    if let Some(matched_by) = suite_policy_core::match_reducer_allowlist(target, &allowlist) {
        return Ok(ReducerExecutionAudit {
            reducer: target.to_string(),
            allowed: true,
            matched_by: Some(matched_by),
        });
    }

    Err(KernelError::PolicyViolation {
        target: target.to_string(),
        detail: format!(
            "reducer execution '{target}' is not allowed by policy; allowed reducers: {}",
            allowlist.join(", ")
        ),
    })
}

pub(crate) fn audit_packets_against_policy(
    config: &guardy_core::ContextConfig,
    packets: &[KernelPacket],
) -> Result<Vec<guardy_core::AuditResult>, KernelError> {
    let mut audits = Vec::with_capacity(packets.len());
    for packet in packets {
        let guard_packet = kernel_packet_to_guard_packet(packet)?;
        audits.push(guardy_core::check_packet(config, &guard_packet));
    }
    Ok(audits)
}

pub(crate) fn kernel_packet_to_guard_packet(
    packet: &KernelPacket,
) -> Result<guardy_core::GuardPacket, KernelError> {
    let guard_candidate = extract_guard_candidate(&packet.body);
    let mut guard_packet = if guard_candidate.is_object() {
        serde_json::from_value::<guardy_core::GuardPacket>(guard_candidate.clone()).map_err(
            |source| KernelError::InvalidRequest {
                detail: format!("packet is not guard-compatible JSON: {source}"),
            },
        )?
    } else {
        guardy_core::GuardPacket {
            payload: guard_candidate.clone(),
            ..guardy_core::GuardPacket::default()
        }
    };

    if guard_packet.packet_id.is_none() {
        guard_packet.packet_id = packet.packet_id.clone();
    }
    if guard_packet.token_usage.is_none() {
        guard_packet.token_usage = packet.token_usage;
    }
    if guard_packet.runtime_ms.is_none() {
        guard_packet.runtime_ms = packet.runtime_ms;
    }
    if guard_packet.tool_call_count.is_none() {
        guard_packet.tool_call_count = packet
            .body
            .get("budget_cost")
            .and_then(|v| v.get("tool_calls"))
            .or_else(|| {
                guard_candidate
                    .get("budget_cost")
                    .and_then(|v| v.get("tool_calls"))
            })
            .and_then(Value::as_u64);
    }
    if guard_packet.tool.is_none() {
        guard_packet.tool = packet
            .body
            .get("tool")
            .or_else(|| guard_candidate.get("tool"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if guard_packet.reducer.is_none() {
        guard_packet.reducer = packet
            .metadata
            .get("reducer")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if guard_packet.paths.is_empty() {
        if let Some(paths) = guard_candidate.get("paths").and_then(Value::as_array) {
            for path in paths.iter().filter_map(Value::as_str) {
                guard_packet.paths.push(path.to_string());
            }
        }
    }
    if guard_packet.paths.is_empty() {
        if let Some(files) = guard_candidate.get("files").and_then(Value::as_array) {
            for file in files {
                if let Some(path) = file.get("path").and_then(Value::as_str) {
                    guard_packet.paths.push(path.to_string());
                }
            }
        }
    }
    if guard_packet.payload.is_null() {
        guard_packet.payload = guard_candidate
            .get("payload")
            .cloned()
            .unwrap_or_else(|| guard_candidate.clone());
    }

    Ok(guard_packet)
}

fn extract_guard_candidate(value: &Value) -> Value {
    if !value.is_object() {
        return value.clone();
    }

    for key in ["packet", "envelope_v1", "final_packet"] {
        if let Some(candidate) = value.get(key) {
            if candidate.is_object() {
                return candidate.clone();
            }
        }
    }

    value.clone()
}

pub(crate) fn extract_packet_value(value: &Value) -> Value {
    if !value.is_object() {
        return value.clone();
    }

    let is_machine_wrapper = value
        .get("schema_version")
        .and_then(Value::as_str)
        .is_some_and(|schema| schema == suite_packet_core::MACHINE_SCHEMA_VERSION);

    if is_machine_wrapper {
        if let Some(packet) = value.get("packet") {
            return packet.clone();
        }
    }

    value.clone()
}

pub(crate) fn parse_contextq_detail_mode(policy_context: &Value) -> contextq_core::DetailMode {
    let mode = policy_context
        .get("detail_mode")
        .and_then(Value::as_str)
        .unwrap_or("compact");

    if mode.eq_ignore_ascii_case("rich") {
        contextq_core::DetailMode::Rich
    } else {
        contextq_core::DetailMode::Compact
    }
}

pub(crate) fn ensure_policy_audits_pass(
    target: &str,
    stage: &str,
    audits: &[guardy_core::AuditResult],
) -> Result<(), KernelError> {
    let mut violations = Vec::new();

    for (idx, audit) in audits.iter().enumerate() {
        if audit.passed {
            continue;
        }

        let finding_summary = audit
            .findings
            .iter()
            .take(3)
            .map(|f| format!("{}:{} ({})", f.rule, f.subject, f.message))
            .collect::<Vec<_>>()
            .join(", ");

        violations.push(format!("packet#{} [{}]", idx + 1, finding_summary));
    }

    if violations.is_empty() {
        return Ok(());
    }

    Err(KernelError::PolicyViolation {
        target: target.to_string(),
        detail: format!(
            "policy audit failed during {stage}: {}",
            violations.join("; ")
        ),
    })
}

pub(crate) fn usage_for_packets(packets: &[KernelPacket]) -> BudgetUsage {
    let mut usage = BudgetUsage::default();

    for packet in packets {
        let body_bytes = estimate_json_bytes(&packet.body);
        usage.tokens = usage.tokens.saturating_add(
            packet
                .token_usage
                .unwrap_or_else(|| estimate_tokens(body_bytes)),
        );
        usage.bytes = usage.bytes.saturating_add(body_bytes);
        usage.runtime_ms = usage
            .runtime_ms
            .saturating_add(packet.runtime_ms.unwrap_or(0));
    }

    usage
}

pub(crate) fn enforce_budget(
    target: &str,
    stage: BudgetStage,
    budget: ExecutionBudget,
    usage: BudgetUsage,
) -> Result<(), KernelError> {
    if let Some(cap) = budget.token_cap {
        if usage.tokens > cap {
            return Err(KernelError::BudgetExceeded {
                target: target.to_string(),
                stage,
                metric: BudgetMetric::Tokens,
                used: usage.tokens,
                cap,
            });
        }
    }

    if let Some(cap) = budget.byte_cap {
        if usage.bytes > cap {
            return Err(KernelError::BudgetExceeded {
                target: target.to_string(),
                stage,
                metric: BudgetMetric::Bytes,
                used: usage.bytes as u64,
                cap: cap as u64,
            });
        }
    }

    if let Some(cap) = budget.runtime_ms_cap {
        if usage.runtime_ms > cap {
            return Err(KernelError::BudgetExceeded {
                target: target.to_string(),
                stage,
                metric: BudgetMetric::RuntimeMs,
                used: usage.runtime_ms,
                cap,
            });
        }
    }

    Ok(())
}

pub(crate) fn default_packet_format() -> String {
    "packet-json".to_string()
}

pub(crate) fn cache_input_for_request(
    req: &KernelRequest,
    target: &str,
    policy_guard: Option<&PolicyGuard>,
) -> Value {
    let inputs = req
        .input_packets
        .iter()
        .map(|packet| {
            json!({
                "packet_id": packet.packet_id.clone(),
                "format": packet.format.clone(),
                "body": packet.body.clone(),
                "token_usage": packet.token_usage,
                "runtime_ms": packet.runtime_ms,
                "metadata": packet.metadata.clone(),
            })
        })
        .collect::<Vec<_>>();

    json!({
        "target": target,
        "input_packets": inputs,
        "budget": {
            "token_cap": req.budget.token_cap,
            "byte_cap": req.budget.byte_cap,
            "runtime_ms_cap": req.budget.runtime_ms_cap,
        },
        "governance": {
            "enabled": policy_guard.is_some(),
            "policy_hash": policy_guard.map(|policy| policy.policy_hash.clone()),
            "context_overrides": cache_policy_context_overrides(&req.policy_context),
        },
        "reducer_input": req.reducer_input.clone(),
    })
}

fn cache_policy_context_overrides(policy_context: &Value) -> Value {
    let mut value = policy_context.clone();
    if let Value::Object(map) = &mut value {
        map.remove("config_path");
    }
    value
}

pub(crate) fn cache_enabled_for_request(target: &str, policy_context: &Value) -> bool {
    if policy_context
        .get("disable_cache")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }

    target != "agenty.state.snapshot"
}

fn estimate_json_bytes(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
}
