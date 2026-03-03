use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct ExecutionBudget {
    pub token_cap: Option<u64>,
    pub byte_cap: Option<usize>,
    pub runtime_ms_cap: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct BudgetUsage {
    pub tokens: u64,
    pub bytes: usize,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KernelPacket {
    pub packet_id: Option<String>,
    pub format: String,
    pub body: Value,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub metadata: Value,
}

impl Default for KernelPacket {
    fn default() -> Self {
        Self {
            packet_id: None,
            format: default_packet_format(),
            body: Value::Null,
            token_usage: None,
            runtime_ms: None,
            metadata: Value::Null,
        }
    }
}

impl KernelPacket {
    pub fn from_value(value: Value, fallback_packet_id: Option<String>) -> Self {
        let packet_id = value
            .get("packet_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or(fallback_packet_id);

        Self {
            packet_id,
            format: default_packet_format(),
            body: value,
            token_usage: None,
            runtime_ms: None,
            metadata: Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KernelRequest {
    pub target: String,
    pub input_packets: Vec<KernelPacket>,
    pub budget: ExecutionBudget,
    pub policy_context: Value,
    pub reducer_input: Value,
}

impl Default for KernelRequest {
    fn default() -> Self {
        Self {
            target: String::new(),
            input_packets: Vec::new(),
            budget: ExecutionBudget::default(),
            policy_context: Value::Null,
            reducer_input: Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelResponse {
    pub request_id: u64,
    pub target: String,
    pub output_packets: Vec<KernelPacket>,
    pub audit: KernelAudit,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelAudit {
    pub reducer: String,
    pub input_packets: usize,
    pub output_packets: usize,
    pub budget: ExecutionBudget,
    pub input_usage: BudgetUsage,
    pub output_usage: BudgetUsage,
    pub total_usage: BudgetUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReducerResult {
    pub output_packets: Vec<KernelPacket>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelFailure {
    pub code: String,
    pub message: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStage {
    Input,
    Total,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMetric {
    Tokens,
    Bytes,
    RuntimeMs,
}

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("kernel target cannot be empty")]
    EmptyTarget,

    #[error("unknown kernel target '{target}'")]
    UnknownTarget {
        target: String,
        registered: Vec<String>,
    },

    #[error("invalid request: {detail}")]
    InvalidRequest { detail: String },

    #[error("budget exceeded for {metric:?} at {stage:?}: used {used} > cap {cap}")]
    BudgetExceeded {
        target: String,
        stage: BudgetStage,
        metric: BudgetMetric,
        used: u64,
        cap: u64,
    },

    #[error("failed to read packet file '{path}': {detail}")]
    PacketRead { path: String, detail: String },

    #[error("failed to parse packet JSON from '{path}': {detail}")]
    PacketParse { path: String, detail: String },

    #[error("reducer '{target}' failed: {detail}")]
    ReducerFailed { target: String, detail: String },
}

impl KernelError {
    pub fn structured(&self) -> KernelFailure {
        match self {
            KernelError::EmptyTarget => KernelFailure {
                code: "empty_target".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::UnknownTarget { target, .. } => KernelFailure {
                code: "unknown_target".to_string(),
                message: self.to_string(),
                target: Some(target.clone()),
            },
            KernelError::InvalidRequest { .. } => KernelFailure {
                code: "invalid_request".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::BudgetExceeded { target, .. } => KernelFailure {
                code: "budget_exceeded".to_string(),
                message: self.to_string(),
                target: Some(target.clone()),
            },
            KernelError::PacketRead { .. } => KernelFailure {
                code: "packet_read_failed".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::PacketParse { .. } => KernelFailure {
                code: "packet_parse_failed".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::ReducerFailed { target, .. } => KernelFailure {
                code: "reducer_failed".to_string(),
                message: self.to_string(),
                target: Some(target.clone()),
            },
        }
    }
}

pub struct ExecutionContext {
    pub request_id: u64,
    pub target: String,
    pub budget: ExecutionBudget,
    pub policy_context: Value,
    pub reducer_input: Value,
    shared: Map<String, Value>,
}

impl ExecutionContext {
    pub fn set_shared(&mut self, key: impl Into<String>, value: Value) {
        self.shared.insert(key.into(), value);
    }

    pub fn shared_value(&self, key: &str) -> Option<&Value> {
        self.shared.get(key)
    }

    pub fn shared_json(&self) -> Value {
        Value::Object(self.shared.clone())
    }
}

type ReducerFn = dyn Fn(&mut ExecutionContext, &[KernelPacket]) -> Result<ReducerResult, KernelError>
    + Send
    + Sync;

pub struct Kernel {
    reducers: HashMap<String, Arc<ReducerFn>>,
    next_request_id: AtomicU64,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            reducers: HashMap::new(),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn with_v1_reducers() -> Self {
        let mut kernel = Self::new();
        register_v1_reducers(&mut kernel);
        kernel
    }

    pub fn register_reducer<F>(&mut self, target: impl Into<String>, reducer: F)
    where
        F: Fn(&mut ExecutionContext, &[KernelPacket]) -> Result<ReducerResult, KernelError>
            + Send
            + Sync
            + 'static,
    {
        self.reducers.insert(target.into(), Arc::new(reducer));
    }

    pub fn reducer_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.reducers.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn execute(&self, req: KernelRequest) -> Result<KernelResponse, KernelError> {
        let target = req.target.trim().to_string();
        if target.is_empty() {
            return Err(KernelError::EmptyTarget);
        }

        let reducer = self
            .reducers
            .get(&target)
            .ok_or_else(|| KernelError::UnknownTarget {
                target: target.clone(),
                registered: self.reducer_names(),
            })?;

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let input_usage = usage_for_packets(&req.input_packets);

        enforce_budget(&target, BudgetStage::Input, req.budget, input_usage)?;

        let mut ctx = ExecutionContext {
            request_id,
            target: target.clone(),
            budget: req.budget,
            policy_context: req.policy_context,
            reducer_input: req.reducer_input,
            shared: Map::new(),
        };

        let started_at = Instant::now();
        let reducer_result = reducer(&mut ctx, &req.input_packets)?;
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        let output_packet_count = reducer_result.output_packets.len();

        let output_usage = usage_for_packets(&reducer_result.output_packets);
        let total_usage = BudgetUsage {
            tokens: input_usage.tokens.saturating_add(output_usage.tokens),
            bytes: input_usage.bytes.saturating_add(output_usage.bytes),
            runtime_ms: elapsed_ms,
        };

        enforce_budget(&target, BudgetStage::Total, req.budget, total_usage)?;

        Ok(KernelResponse {
            request_id,
            target: target.clone(),
            output_packets: reducer_result.output_packets,
            audit: KernelAudit {
                reducer: target,
                input_packets: req.input_packets.len(),
                output_packets: output_packet_count,
                budget: req.budget,
                input_usage,
                output_usage,
                total_usage,
            },
            metadata: merge_json(ctx.shared_json(), reducer_result.metadata),
        })
    }
}

pub fn execute(req: KernelRequest) -> Result<KernelResponse, KernelError> {
    Kernel::with_v1_reducers().execute(req)
}

pub fn load_packet_file(path: &Path) -> Result<KernelPacket, KernelError> {
    let raw = std::fs::read_to_string(path).map_err(|source| KernelError::PacketRead {
        path: path.to_string_lossy().to_string(),
        detail: source.to_string(),
    })?;

    let value: Value = serde_json::from_str(&raw).map_err(|source| KernelError::PacketParse {
        path: path.to_string_lossy().to_string(),
        detail: source.to_string(),
    })?;

    Ok(KernelPacket::from_value(
        value,
        Some(path.to_string_lossy().to_string()),
    ))
}

pub fn register_v1_reducers(kernel: &mut Kernel) {
    kernel.register_reducer("contextq.assemble", run_contextq_assemble);
    kernel.register_reducer("guardy.check", run_guardy_check);
}

fn run_contextq_assemble(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    if input_packets.is_empty() {
        return Err(KernelError::InvalidRequest {
            detail: "contextq.assemble requires at least one input packet".to_string(),
        });
    }

    let options = contextq_core::AssembleOptions {
        budget_tokens: ctx
            .budget
            .token_cap
            .unwrap_or(contextq_core::DEFAULT_BUDGET_TOKENS),
        budget_bytes: ctx
            .budget
            .byte_cap
            .unwrap_or(contextq_core::DEFAULT_BUDGET_BYTES),
    };

    let packets: Vec<contextq_core::InputPacket> = input_packets
        .iter()
        .enumerate()
        .map(|(idx, packet)| {
            let fallback = packet
                .packet_id
                .clone()
                .unwrap_or_else(|| format!("packet-{}", idx + 1));
            contextq_core::InputPacket::from_value(packet.body.clone(), &fallback)
        })
        .collect();

    let assembled = contextq_core::assemble_packets(packets, options);

    ctx.set_shared("truncated", Value::Bool(assembled.assembly.truncated));
    ctx.set_shared(
        "sections_kept",
        Value::from(assembled.assembly.sections_kept as u64),
    );

    let packet = KernelPacket {
        packet_id: assembled.packet_id.clone(),
        format: default_packet_format(),
        body: serde_json::to_value(&assembled).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: assembled.token_usage,
        runtime_ms: assembled.runtime_ms,
        metadata: json!({
            "tool": assembled.tool,
            "reducer": assembled.reducer,
            "truncated": assembled.assembly.truncated,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "contextq.assemble",
        }),
    })
}

fn run_guardy_check(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    if input_packets.len() != 1 {
        return Err(KernelError::InvalidRequest {
            detail: "guardy.check requires exactly one input packet".to_string(),
        });
    }

    let config_path = ctx
        .policy_context
        .get("config_path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| KernelError::InvalidRequest {
            detail: "guardy.check requires policy_context.config_path".to_string(),
        })?;

    let config = guardy_core::ContextConfig::load(Path::new(config_path)).map_err(|source| {
        KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;

    let packet: guardy_core::GuardPacket = serde_json::from_value(input_packets[0].body.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid guard packet: {source}"),
        })?;

    let audit = guardy_core::check_packet(&config, &packet);

    ctx.set_shared("passed", Value::Bool(audit.passed));
    ctx.set_shared("policy_version", Value::from(audit.policy_version));

    let packet = KernelPacket {
        packet_id: Some("guardy-audit-v1".to_string()),
        format: default_packet_format(),
        body: serde_json::to_value(&audit).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: None,
        runtime_ms: None,
        metadata: json!({
            "reducer": "guardy.check",
            "passed": audit.passed,
            "findings": audit.findings.len(),
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "guardy.check",
            "passed": audit.passed,
        }),
    })
}

fn usage_for_packets(packets: &[KernelPacket]) -> BudgetUsage {
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

fn enforce_budget(
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

fn default_packet_format() -> String {
    "packet-json".to_string()
}

fn estimate_json_bytes(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
}

fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn merge_json(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, value) in right {
                left.insert(key, value);
            }
            Value::Object(left)
        }
        (value, Value::Null) => value,
        (_, value) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn errors_for_unknown_target() {
        let kernel = Kernel::new();
        let err = kernel
            .execute(KernelRequest {
                target: "missing.reducer".to_string(),
                ..KernelRequest::default()
            })
            .unwrap_err();

        match err {
            KernelError::UnknownTarget { target, registered } => {
                assert_eq!(target, "missing.reducer");
                assert!(registered.is_empty());
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn enforces_input_token_budget() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("noop", |_ctx, _packets| Ok(ReducerResult::default()));

        let packet = KernelPacket {
            body: json!({"text": "this payload should exceed tiny token budget"}),
            ..KernelPacket::default()
        };

        let err = kernel
            .execute(KernelRequest {
                target: "noop".to_string(),
                input_packets: vec![packet],
                budget: ExecutionBudget {
                    token_cap: Some(1),
                    byte_cap: None,
                    runtime_ms_cap: None,
                },
                ..KernelRequest::default()
            })
            .unwrap_err();

        assert!(matches!(
            err,
            KernelError::BudgetExceeded {
                stage: BudgetStage::Input,
                metric: BudgetMetric::Tokens,
                ..
            }
        ));
    }

    #[test]
    fn contextq_reducer_assembles_packets() {
        let kernel = Kernel::with_v1_reducers();
        let packet_a = KernelPacket::from_value(
            json!({
                "packet_id": "diffy",
                "tool": "diffy",
                "reducer": "reduce",
                "sections": [{
                    "title": "Diff",
                    "body": "critical regression",
                    "refs": [{"kind": "file", "value": "src/lib.rs"}],
                    "relevance": 0.9
                }]
            }),
            None,
        );
        let packet_b = KernelPacket::from_value(
            json!({
                "packet_id": "impact",
                "tool": "testy",
                "reducer": "reduce",
                "sections": [{
                    "title": "Impact",
                    "body": "selected tests",
                    "refs": [{"kind": "symbol", "value": "foo::bar"}],
                    "relevance": 0.8
                }]
            }),
            None,
        );

        let response = kernel
            .execute(KernelRequest {
                target: "contextq.assemble".to_string(),
                input_packets: vec![packet_a, packet_b],
                budget: ExecutionBudget {
                    token_cap: Some(1200),
                    byte_cap: Some(24_000),
                    runtime_ms_cap: Some(1_000),
                },
                ..KernelRequest::default()
            })
            .unwrap();

        assert_eq!(response.output_packets.len(), 1);
        let reducer = response.output_packets[0]
            .body
            .get("reducer")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(reducer, "assemble");
    }

    #[test]
    fn guardy_reducer_runs_policy_check() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("context.yaml");
        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 200
    runtime_ms_cap: 1000
  redaction:
    forbidden_patterns: []
"#,
        )
        .unwrap();

        let kernel = Kernel::with_v1_reducers();
        let packet = KernelPacket::from_value(
            json!({
                "tool": "covy",
                "reducer": "merge",
                "paths": ["src/lib.rs"],
                "token_usage": 50,
                "runtime_ms": 10,
                "payload": {"message": "ok"}
            }),
            None,
        );

        let response = kernel
            .execute(KernelRequest {
                target: "guardy.check".to_string(),
                input_packets: vec![packet],
                policy_context: json!({
                    "config_path": config_path.to_string_lossy().to_string()
                }),
                ..KernelRequest::default()
            })
            .unwrap();

        let passed = response.output_packets[0]
            .body
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap();
        assert!(passed);
    }

    #[test]
    fn loads_packet_file() {
        let dir = tempdir().unwrap();
        let packet_path = dir.path().join("packet.json");
        std::fs::write(&packet_path, r#"{"packet_id":"a","payload":{"k":"v"}}"#).unwrap();

        let packet = load_packet_file(&packet_path).unwrap();
        assert_eq!(packet.packet_id.as_deref(), Some("a"));
    }
}
