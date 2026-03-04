use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use context_memory_core::{CachePacket, DeltaReuseHooks, NoopDeltaReuseHooks, PacketCache};

pub use context_memory_core::PersistConfig;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KernelStepRequest {
    pub id: String,
    pub target: String,
    pub depends_on: Vec<String>,
    pub input_packets: Vec<KernelPacket>,
    pub policy_context: Value,
    pub reducer_input: Value,
    pub budget: ExecutionBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KernelSequenceRequest {
    pub budget: ExecutionBudget,
    pub steps: Vec<KernelStepRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelStepResponse {
    pub id: String,
    pub target: String,
    pub status: String,
    pub response: Option<KernelResponse>,
    pub failure: Option<KernelFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelSequenceResponse {
    pub request_id: u64,
    pub scheduled: Vec<String>,
    pub skipped: Vec<String>,
    pub budget_exhausted: bool,
    pub step_results: Vec<KernelStepResponse>,
    pub metadata: Value,
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
    #[serde(default)]
    pub governance: GovernanceAudit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GovernanceAudit {
    pub enabled: bool,
    pub config_path: Option<String>,
    pub reducer_execution: Option<ReducerExecutionAudit>,
    pub input_audits: Vec<guardy_core::AuditResult>,
    pub output_audits: Vec<guardy_core::AuditResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReducerExecutionAudit {
    pub reducer: String,
    pub allowed: bool,
    pub matched_by: Option<String>,
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

    #[error("scheduler error: {detail}")]
    SchedulerFailed { detail: String },

    #[error("cache lock failed: {detail}")]
    CacheLock { detail: String },

    #[error("policy violation for target '{target}': {detail}")]
    PolicyViolation { target: String, detail: String },
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
            KernelError::SchedulerFailed { .. } => KernelFailure {
                code: "scheduler_failed".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::CacheLock { .. } => KernelFailure {
                code: "cache_lock_failed".to_string(),
                message: self.to_string(),
                target: None,
            },
            KernelError::PolicyViolation { target, .. } => KernelFailure {
                code: "policy_violation".to_string(),
                message: self.to_string(),
                target: Some(target.clone()),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct PolicyGuard {
    config: guardy_core::ContextConfig,
    config_path: String,
    policy_hash: String,
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
    memory: Mutex<PacketCache>,
    persist_config: Option<PersistConfig>,
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
            memory: Mutex::new(PacketCache::new()),
            persist_config: None,
        }
    }

    pub fn with_v1_reducers() -> Self {
        let mut kernel = Self::new();
        register_v1_reducers(&mut kernel);
        kernel
    }

    pub fn with_v1_reducers_and_persistence(config: PersistConfig) -> Self {
        let mut kernel = Self {
            reducers: HashMap::new(),
            next_request_id: AtomicU64::new(1),
            memory: Mutex::new(PacketCache::load_from_disk(&config)),
            persist_config: Some(config),
        };
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
        let mut hooks = NoopDeltaReuseHooks;
        self.execute_with_hooks(req, &mut hooks)
    }

    pub fn execute_with_hooks(
        &self,
        req: KernelRequest,
        hooks: &mut dyn DeltaReuseHooks,
    ) -> Result<KernelResponse, KernelError> {
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
        let policy_guard = load_policy_guard(&req.policy_context)?;
        let mut governance = GovernanceAudit {
            enabled: policy_guard.is_some(),
            config_path: policy_guard.as_ref().map(|p| p.config_path.clone()),
            ..GovernanceAudit::default()
        };

        if let Some(policy_guard) = &policy_guard {
            if should_enforce_policy_for_target(&target) {
                governance.reducer_execution = Some(enforce_reducer_execution_policy(
                    &target,
                    &policy_guard.config,
                )?);
                let input_audits =
                    audit_packets_against_policy(&policy_guard.config, &req.input_packets)?;
                ensure_policy_audits_pass(&target, "input", &input_audits)?;
                governance.input_audits = input_audits;
            }
        }

        enforce_budget(&target, BudgetStage::Input, req.budget, input_usage)?;

        let cache_input = cache_input_for_request(&req, &target, policy_guard.as_ref());
        let cache_lookup = {
            let cache = self
                .memory
                .lock()
                .map_err(|source| KernelError::CacheLock {
                    detail: source.to_string(),
                })?;
            cache.lookup_with_hooks(&target, &cache_input, hooks)
        };

        if let Some(entry) = cache_lookup.entry.clone() {
            let output_packets = entry
                .packets
                .into_iter()
                .map(|packet| KernelPacket {
                    packet_id: packet.packet_id,
                    format: default_packet_format(),
                    body: packet.body,
                    token_usage: packet.token_usage,
                    runtime_ms: packet.runtime_ms,
                    metadata: packet.metadata,
                })
                .collect::<Vec<_>>();
            let output_packet_count = output_packets.len();

            if let Some(policy_guard) = &policy_guard {
                if should_enforce_policy_for_target(&target) {
                    let output_audits =
                        audit_packets_against_policy(&policy_guard.config, &output_packets)?;
                    ensure_policy_audits_pass(&target, "output", &output_audits)?;
                    governance.output_audits = output_audits;
                }
            }

            let output_usage = usage_for_packets(&output_packets);
            let total_usage = BudgetUsage {
                tokens: input_usage.tokens.saturating_add(output_usage.tokens),
                bytes: input_usage.bytes.saturating_add(output_usage.bytes),
                runtime_ms: input_usage
                    .runtime_ms
                    .saturating_add(output_usage.runtime_ms),
            };
            enforce_budget(&target, BudgetStage::Total, req.budget, total_usage)?;
            let entry_age_secs = now_unix().saturating_sub(entry.created_at_unix);

            return Ok(KernelResponse {
                request_id,
                target: target.clone(),
                output_packets,
                audit: KernelAudit {
                    reducer: target,
                    input_packets: req.input_packets.len(),
                    output_packets: output_packet_count,
                    budget: req.budget,
                    input_usage,
                    output_usage,
                    total_usage,
                    governance,
                },
                metadata: merge_json(
                    entry.metadata,
                    json!({
                        "cache": {
                            "hit": true,
                            "key": cache_lookup.cache_key,
                            "entry_age_secs": entry_age_secs,
                            "miss_reason": Value::Null,
                        }
                    }),
                ),
            });
        }

        let cache_lookup = Some(cache_lookup);

        let mut ctx = ExecutionContext {
            request_id,
            target: target.clone(),
            budget: req.budget,
            policy_context: req.policy_context.clone(),
            reducer_input: req.reducer_input,
            shared: Map::new(),
        };

        let started_at = Instant::now();
        let reducer_result = reducer(&mut ctx, &req.input_packets)?;
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        if let Some(policy_guard) = &policy_guard {
            if should_enforce_policy_for_target(&target) {
                let output_audits = audit_packets_against_policy(
                    &policy_guard.config,
                    &reducer_result.output_packets,
                )?;
                ensure_policy_audits_pass(&target, "output", &output_audits)?;
                governance.output_audits = output_audits;
            }
        }
        let output_packet_count = reducer_result.output_packets.len();

        let output_usage = usage_for_packets(&reducer_result.output_packets);
        let total_usage = BudgetUsage {
            tokens: input_usage.tokens.saturating_add(output_usage.tokens),
            bytes: input_usage.bytes.saturating_add(output_usage.bytes),
            runtime_ms: elapsed_ms,
        };

        enforce_budget(&target, BudgetStage::Total, req.budget, total_usage)?;

        let output_packets = reducer_result.output_packets;
        let mut response = KernelResponse {
            request_id,
            target: target.clone(),
            output_packets: output_packets.clone(),
            audit: KernelAudit {
                reducer: target.clone(),
                input_packets: req.input_packets.len(),
                output_packets: output_packet_count,
                budget: req.budget,
                input_usage,
                output_usage,
                total_usage,
                governance,
            },
            metadata: merge_json(
                merge_json(ctx.shared_json(), reducer_result.metadata),
                json!({
                    "cache": {
                        "hit": false,
                        "key": cache_lookup
                            .as_ref()
                            .map(|lookup| lookup.cache_key.clone())
                            .unwrap_or_default(),
                        "entry_age_secs": Value::Null,
                        "miss_reason": "not_found",
                    }
                }),
            ),
        };

        if let Some(cache_lookup) = cache_lookup {
            let mut cache = self
                .memory
                .lock()
                .map_err(|source| KernelError::CacheLock {
                    detail: source.to_string(),
                })?;
            let packets = output_packets
                .iter()
                .map(|packet| CachePacket {
                    packet_id: packet.packet_id.clone(),
                    body: packet.body.clone(),
                    token_usage: packet.token_usage,
                    runtime_ms: packet.runtime_ms,
                    metadata: packet.metadata.clone(),
                })
                .collect();

            let metadata = response.metadata.clone();
            cache.put_with_hooks(&target, &cache_lookup, packets, metadata, hooks);
            if let Some(persist_config) = &self.persist_config {
                cache.evict_expired(persist_config.ttl_secs);
                let _ = cache.save_to_disk(persist_config);
            }
            let stats = cache.stats();
            if let Some(cache_obj) = response
                .metadata
                .as_object_mut()
                .and_then(|metadata| metadata.get_mut("cache"))
                .and_then(Value::as_object_mut)
            {
                cache_obj.insert("evictions".to_string(), json!(stats.evictions));
            } else {
                response.metadata = merge_json(
                    response.metadata,
                    json!({
                        "cache": {
                            "evictions": stats.evictions,
                        }
                    }),
                );
            }
        }

        Ok(response)
    }

    pub fn execute_sequence(
        &self,
        req: KernelSequenceRequest,
    ) -> Result<KernelSequenceResponse, KernelError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut scheduler_steps = Vec::with_capacity(req.steps.len());
        let mut by_id = HashMap::new();

        for step in req.steps {
            let id = step.id.trim().to_string();
            if id.is_empty() {
                return Err(KernelError::InvalidRequest {
                    detail: "sequence step id cannot be empty".to_string(),
                });
            }

            let estimate = usage_for_packets(&step.input_packets);
            scheduler_steps.push(context_scheduler_core::ScheduleStep {
                id: id.clone(),
                target: step.target.clone(),
                depends_on: step.depends_on.clone(),
                estimate: context_scheduler_core::StepEstimate {
                    tokens: estimate.tokens,
                    bytes: estimate.bytes,
                    runtime_ms: estimate.runtime_ms,
                },
            });
            by_id.insert(id, step);
        }

        let schedule = context_scheduler_core::schedule(context_scheduler_core::ScheduleRequest {
            steps: scheduler_steps,
            budget: context_scheduler_core::ScheduleBudget {
                token_cap: req.budget.token_cap,
                byte_cap: req.budget.byte_cap,
                runtime_ms_cap: req.budget.runtime_ms_cap,
            },
        })
        .map_err(|source| KernelError::SchedulerFailed {
            detail: source.to_string(),
        })?;

        let mut step_results = Vec::new();
        let mut scheduled = Vec::new();
        let mut completed = HashMap::<String, bool>::new();
        for scheduled_step in &schedule.ordered_steps {
            let Some(original) = by_id.get(&scheduled_step.id) else {
                continue;
            };

            if original
                .depends_on
                .iter()
                .any(|dep| !completed.get(dep).copied().unwrap_or(false))
            {
                completed.insert(scheduled_step.id.clone(), false);
                step_results.push(KernelStepResponse {
                    id: scheduled_step.id.clone(),
                    target: original.target.clone(),
                    status: "skipped".to_string(),
                    response: None,
                    failure: Some(KernelFailure {
                        code: "dependency_failed".to_string(),
                        message: "step skipped due to failed dependency".to_string(),
                        target: Some(original.target.clone()),
                    }),
                });
                continue;
            }

            let response = self.execute(KernelRequest {
                target: original.target.clone(),
                input_packets: original.input_packets.clone(),
                budget: if original.budget == ExecutionBudget::default() {
                    req.budget
                } else {
                    original.budget
                },
                policy_context: original.policy_context.clone(),
                reducer_input: original.reducer_input.clone(),
            });

            match response {
                Ok(response) => {
                    scheduled.push(scheduled_step.id.clone());
                    completed.insert(scheduled_step.id.clone(), true);
                    step_results.push(KernelStepResponse {
                        id: scheduled_step.id.clone(),
                        target: original.target.clone(),
                        status: "ok".to_string(),
                        response: Some(response),
                        failure: None,
                    });
                }
                Err(err) => {
                    completed.insert(scheduled_step.id.clone(), false);
                    step_results.push(KernelStepResponse {
                        id: scheduled_step.id.clone(),
                        target: original.target.clone(),
                        status: "failed".to_string(),
                        response: None,
                        failure: Some(err.structured()),
                    });
                }
            }
        }

        let skipped = schedule
            .skipped_steps
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>();
        for skipped_step in &schedule.skipped_steps {
            if let Some(step) = by_id.get(&skipped_step.id) {
                step_results.push(KernelStepResponse {
                    id: step.id.clone(),
                    target: step.target.clone(),
                    status: "skipped".to_string(),
                    response: None,
                    failure: Some(KernelFailure {
                        code: skipped_step.reason.clone(),
                        message: format!("step skipped: {}", skipped_step.reason),
                        target: Some(step.target.clone()),
                    }),
                });
            }
        }

        Ok(KernelSequenceResponse {
            request_id,
            scheduled,
            skipped,
            budget_exhausted: schedule.budget_exhausted,
            step_results,
            metadata: json!({
                "estimated_usage": {
                    "tokens": schedule.estimated_usage.tokens,
                    "bytes": schedule.estimated_usage.bytes,
                    "runtime_ms": schedule.estimated_usage.runtime_ms,
                }
            }),
        })
    }
}

pub fn execute(req: KernelRequest) -> Result<KernelResponse, KernelError> {
    Kernel::with_v1_reducers().execute(req)
}

pub fn execute_sequence(req: KernelSequenceRequest) -> Result<KernelSequenceResponse, KernelError> {
    Kernel::with_v1_reducers().execute_sequence(req)
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
    kernel.register_reducer("governed.assemble", run_governed_assemble);
    kernel.register_reducer("guardy.check", run_guardy_check);
    kernel.register_reducer("stacky.slice", run_stacky_slice);
    kernel.register_reducer("buildy.reduce", run_buildy_reduce);
    kernel.register_reducer("proxy.run", run_proxy_run);
    kernel.register_reducer("mapy.repo", run_mapy_repo);
}

fn run_governed_assemble(
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
        detail_mode: parse_contextq_detail_mode(&ctx.policy_context),
        compact_assembly: ctx
            .policy_context
            .get("compact_assembly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
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

    let packet = kernel_packet_to_guard_packet(&input_packets[0])?;

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

fn run_stacky_slice(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: stacky_core::StackSliceRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: format!("invalid reducer input: {source}"),
    })?;

    let packet = stacky_core::slice_to_packet(input);
    let payload: stacky_core::StackSliceOutput = serde_json::from_value(packet.payload.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid stacky payload: {source}"),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: packet.packet_id.clone(),
        format: default_packet_format(),
        body: serde_json::to_value(&packet).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: None,
        runtime_ms: None,
        metadata: json!({
            "tool": "stacky",
            "reducer": "slice",
            "schema_version": payload.schema_version,
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "stacky.slice",
            "schema_version": stacky_core::STACKY_SCHEMA_VERSION,
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
        }),
    })
}

fn run_buildy_reduce(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: buildy_core::BuildReduceRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid reducer input: {source}"),
        })?;

    let packet = buildy_core::reduce_to_packet(input);
    let payload: buildy_core::BuildReduceOutput = serde_json::from_value(packet.payload.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid buildy payload: {source}"),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: packet.packet_id.clone(),
        format: default_packet_format(),
        body: serde_json::to_value(&packet).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?,
        token_usage: None,
        runtime_ms: None,
        metadata: json!({
            "tool": "buildy",
            "reducer": "reduce",
            "schema_version": payload.schema_version,
            "unique_diagnostics": payload.unique_diagnostics,
            "duplicates_removed": payload.duplicates_removed,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "buildy.reduce",
            "schema_version": buildy_core::BUILDY_SCHEMA_VERSION,
            "unique_diagnostics": payload.unique_diagnostics,
            "duplicates_removed": payload.duplicates_removed,
        }),
    })
}

fn run_proxy_run(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: suite_proxy_core::ProxyRunRequest =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let envelope =
        suite_proxy_core::run_and_reduce(input).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "proxy-{}",
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
            "tool": "proxy",
            "reducer": "run",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "lines_out": envelope.payload.lines_out,
            "bytes_saved": envelope.payload.bytes_saved,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "proxy.run",
            "kind": "command_summary",
        }),
    })
}

fn run_mapy_repo(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: mapy_core::RepoMapRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid reducer input: {source}"),
        })?;

    let envelope =
        mapy_core::build_repo_map(input).map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        })?;

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "mapy-{}",
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
            "tool": "mapy",
            "reducer": "repo",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "files_ranked": envelope.payload.files_ranked.len(),
            "symbols_ranked": envelope.payload.symbols_ranked.len(),
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "mapy.repo",
            "kind": "repo_map",
        }),
    })
}

fn load_policy_guard(policy_context: &Value) -> Result<Option<PolicyGuard>, KernelError> {
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

fn should_enforce_policy_for_target(target: &str) -> bool {
    target != "guardy.check"
}

fn enforce_reducer_execution_policy(
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

fn audit_packets_against_policy(
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

fn kernel_packet_to_guard_packet(
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

fn parse_contextq_detail_mode(policy_context: &Value) -> contextq_core::DetailMode {
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

fn ensure_policy_audits_pass(
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

fn cache_input_for_request(
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

fn estimate_json_bytes(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
}

fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn write_policy_file(path: &Path, tools: &[&str], reducers: &[&str]) {
        let tools_yaml = if tools.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                tools
                    .iter()
                    .map(|tool| format!("\"{tool}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let reducers_yaml = if reducers.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                reducers
                    .iter()
                    .map(|reducer| format!("\"{reducer}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        std::fs::write(
            path,
            format!(
                r#"
version: 1
policy:
  allowed_tools: {tools_yaml}
  allowed_reducers: {reducers_yaml}
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 2000
    runtime_ms_cap: 2000
  redaction:
    forbidden_patterns: []
"#
            ),
        )
        .unwrap();
    }

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
    fn policy_enforcement_rejects_disallowed_packet_before_contextq() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("context.yaml");
        write_policy_file(&config_path, &["contextq"], &["assemble"]);

        let kernel = Kernel::with_v1_reducers();
        let packet = KernelPacket::from_value(
            json!({
                "tool": "diffy",
                "reducer": "analyze",
                "paths": ["src/lib.rs"],
                "payload": {"gate_result": {"passed": true}}
            }),
            None,
        );

        let err = kernel
            .execute(KernelRequest {
                target: "contextq.assemble".to_string(),
                input_packets: vec![packet],
                policy_context: json!({
                    "config_path": config_path.to_string_lossy().to_string()
                }),
                ..KernelRequest::default()
            })
            .unwrap_err();

        assert!(matches!(err, KernelError::PolicyViolation { .. }));
    }

    #[test]
    fn governed_assemble_surfaces_governance_audit() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("context.yaml");
        write_policy_file(
            &config_path,
            &["diffy", "contextq"],
            &["analyze", "assemble", "contextq.assemble"],
        );

        let kernel = Kernel::with_v1_reducers();
        let packet = KernelPacket::from_value(
            json!({
                "packet_id": "diffy-analyze-v1",
                "tool": "diffy",
                "reducer": "analyze",
                "paths": ["src/lib.rs"],
                "payload": {"summary": "ok"},
                "sections": [{
                    "title": "Diff Gate Summary",
                    "body": "passed: true",
                    "refs": [{"kind":"file","value":"src/lib.rs"}],
                    "relevance": 1.0
                }]
            }),
            None,
        );

        let response = kernel
            .execute(KernelRequest {
                target: "governed.assemble".to_string(),
                input_packets: vec![packet],
                policy_context: json!({
                    "config_path": config_path.to_string_lossy().to_string()
                }),
                budget: ExecutionBudget {
                    token_cap: Some(1200),
                    byte_cap: Some(24_000),
                    runtime_ms_cap: Some(1_000),
                },
                ..KernelRequest::default()
            })
            .unwrap();

        assert_eq!(response.output_packets.len(), 1);
        assert!(response.audit.governance.enabled);
        assert!(response
            .audit
            .governance
            .reducer_execution
            .as_ref()
            .is_some_and(|audit| audit.allowed));
        assert_eq!(response.audit.governance.input_audits.len(), 1);
        assert_eq!(response.audit.governance.output_audits.len(), 1);
        assert!(response.audit.governance.input_audits[0].passed);
        assert!(response.audit.governance.output_audits[0].passed);
    }

    #[test]
    fn contextq_assemble_exposes_budget_trim_metadata() {
        let kernel = Kernel::with_v1_reducers();
        let packet = KernelPacket::from_value(
            json!({
                "packet_id": "large-packet",
                "tool": "diffy",
                "reducer": "analyze",
                "sections": [{
                    "title": "Large section",
                    "body": "X".repeat(8_000),
                    "refs": [{"kind":"file","value":"src/lib.rs"}],
                    "relevance": 1.0
                }]
            }),
            None,
        );
        let mut packet = packet;
        packet.token_usage = Some(1);

        let response = kernel
            .execute(KernelRequest {
                target: "contextq.assemble".to_string(),
                input_packets: vec![packet],
                budget: ExecutionBudget {
                    token_cap: Some(1300),
                    byte_cap: Some(200_000),
                    runtime_ms_cap: None,
                },
                ..KernelRequest::default()
            })
            .unwrap();

        let truncated = response
            .metadata
            .get("budget_trim")
            .and_then(|trim| trim.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(truncated);
        assert!(response
            .metadata
            .get("budget_trim")
            .and_then(|trim| trim.get("sections_dropped"))
            .and_then(Value::as_u64)
            .is_some());
        assert!(response
            .metadata
            .get("budget_trim")
            .and_then(|trim| trim.get("refs_dropped"))
            .and_then(Value::as_u64)
            .is_some());
    }

    #[test]
    fn policy_enforcement_rejects_disallowed_reducer_execution() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("context.yaml");
        write_policy_file(&config_path, &[], &["assemble"]);

        let mut kernel = Kernel::new();
        kernel.register_reducer("custom.run", |_ctx, _packets| Ok(ReducerResult::default()));

        let err = kernel
            .execute(KernelRequest {
                target: "custom.run".to_string(),
                policy_context: json!({
                    "config_path": config_path.to_string_lossy().to_string()
                }),
                ..KernelRequest::default()
            })
            .unwrap_err();

        match err {
            KernelError::PolicyViolation { detail, .. } => {
                assert!(detail.contains("reducer execution 'custom.run'"));
            }
            other => panic!("unexpected error: {other}"),
        }
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
    fn guardy_reducer_scans_wrapped_packet_payloads() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("context.yaml");
        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  paths:
    include: ["**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#,
        )
        .unwrap();

        let kernel = Kernel::with_v1_reducers();
        let packet = KernelPacket::from_value(
            json!({
                "schema_version": "suite.proxy.run.v1",
                "packet": {
                    "tool": "proxy",
                    "payload": {
                        "highlights": ["my_password_is_secret123"]
                    }
                }
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
        assert!(!passed);
    }

    #[test]
    fn caches_reducer_packets_by_request_hash() {
        let mut kernel = Kernel::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"same"}),
            ..KernelRequest::default()
        };

        let first = kernel.execute(request.clone()).unwrap();
        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(first.output_packets.len(), 1);
        assert_eq!(second.output_packets.len(), 1);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn persistent_kernel_reuses_cache_across_instances() {
        let dir = tempdir().unwrap();
        let config = PersistConfig::new(dir.path().to_path_buf());

        let first_calls = Arc::new(AtomicU64::new(0));
        let first_calls_ref = first_calls.clone();
        let mut first_kernel = Kernel::with_v1_reducers_and_persistence(config.clone());
        first_kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            first_calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"persisted"}),
            ..KernelRequest::default()
        };

        let first = first_kernel.execute(request.clone()).unwrap();
        assert_eq!(first_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        drop(first_kernel);

        let second_calls = Arc::new(AtomicU64::new(0));
        let second_calls_ref = second_calls.clone();
        let mut second_kernel = Kernel::with_v1_reducers_and_persistence(config);
        second_kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            second_calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: json!({"source":"reducer"}),
            })
        });

        let second = second_kernel.execute(request).unwrap();
        assert_eq!(second_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(dir.path().join(".packet28/packet-cache-v1.bin").exists());
    }

    #[test]
    fn governed_cache_reuses_entries_for_same_policy_content_across_paths() {
        let dir = tempdir().unwrap();
        let persist = PersistConfig::new(dir.path().to_path_buf());
        let config_a = dir.path().join("policy-a.yaml");
        let config_b = dir.path().join("policy-b.yaml");
        write_policy_file(&config_a, &["diffy"], &[]);
        write_policy_file(&config_b, &["diffy"], &[]);

        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        let mut kernel = Kernel::with_v1_reducers_and_persistence(persist);
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(
                    json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"message": "ok"},
                    }),
                    None,
                )],
                metadata: json!({"source":"reducer"}),
            })
        });

        let mut request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"governed-cache"}),
            policy_context: json!({
                "config_path": config_a.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let first = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert!(first.audit.governance.enabled);

        request.policy_context = json!({
            "config_path": config_b.to_string_lossy().to_string()
        });
        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn governed_cache_misses_when_policy_content_changes() {
        let dir = tempdir().unwrap();
        let persist = PersistConfig::new(dir.path().to_path_buf());
        let config_path = dir.path().join("policy.yaml");
        write_policy_file(&config_path, &["diffy"], &[]);

        let calls = Arc::new(AtomicU64::new(0));
        let calls_ref = calls.clone();
        let mut kernel = Kernel::with_v1_reducers_and_persistence(persist);
        kernel.register_reducer("count.reducer", move |_ctx, _packets| {
            calls_ref.fetch_add(1, Ordering::Relaxed);
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(
                    json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"message": "ok"},
                    }),
                    None,
                )],
                metadata: json!({"source":"reducer"}),
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"governed-cache"}),
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let first = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            first
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );

        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  allowed_tools: ["diffy"]
  allowed_reducers: []
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 9000
    runtime_ms_cap: 2000
  redaction:
    forbidden_patterns: []
"#,
        )
        .unwrap();

        let second = kernel.execute(request).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn governed_cache_hit_rechecks_output_policy_audits() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("policy.yaml");
        std::fs::write(
            &config_path,
            r#"
version: 1
policy:
  paths:
    include: ["src/**"]
    exclude: []
  redaction:
    forbidden_patterns: ["secret123"]
"#,
        )
        .unwrap();

        let mut kernel = Kernel::new();
        kernel.register_reducer("count.reducer", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"ok": true}), None)],
                metadata: Value::Null,
            })
        });

        let request = KernelRequest {
            target: "count.reducer".to_string(),
            policy_context: json!({
                "config_path": config_path.to_string_lossy().to_string()
            }),
            ..KernelRequest::default()
        };
        let policy_guard = load_policy_guard(&request.policy_context).unwrap().unwrap();
        let cache_input = cache_input_for_request(&request, &request.target, Some(&policy_guard));
        let mut hooks = NoopDeltaReuseHooks;

        let lookup = {
            let cache = kernel.memory.lock().unwrap();
            cache.lookup_with_hooks(&request.target, &cache_input, &mut hooks)
        };
        {
            let mut cache = kernel.memory.lock().unwrap();
            cache.put_with_hooks(
                &request.target,
                &lookup,
                vec![CachePacket {
                    packet_id: Some("cached-bad".to_string()),
                    body: json!({
                        "tool": "diffy",
                        "reducer": "analyze",
                        "paths": ["src/lib.rs"],
                        "payload": {"secret": "secret123"},
                    }),
                    token_usage: None,
                    runtime_ms: None,
                    metadata: Value::Null,
                }],
                Value::Null,
                &mut hooks,
            );
        }

        let err = kernel.execute(request).unwrap_err();
        assert!(matches!(err, KernelError::PolicyViolation { .. }));
    }

    #[test]
    fn executes_sequence_in_dependency_order() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.a", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"step":"a"}), None)],
                metadata: Value::Null,
            })
        });
        kernel.register_reducer("step.b", |_ctx, _packets| {
            Ok(ReducerResult {
                output_packets: vec![KernelPacket::from_value(json!({"step":"b"}), None)],
                metadata: Value::Null,
            })
        });

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget {
                    token_cap: Some(100),
                    byte_cap: None,
                    runtime_ms_cap: None,
                },
                steps: vec![
                    KernelStepRequest {
                        id: "b".to_string(),
                        target: "step.b".to_string(),
                        depends_on: vec!["a".to_string()],
                        input_packets: vec![],
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "a".to_string(),
                        target: "step.a".to_string(),
                        depends_on: vec![],
                        input_packets: vec![],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert_eq!(response.scheduled, vec!["a".to_string(), "b".to_string()]);
        assert!(response.skipped.is_empty());
    }

    #[test]
    fn sequence_respects_scheduler_budget_cutoff() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.a", |_ctx, _packets| Ok(ReducerResult::default()));
        kernel.register_reducer("step.b", |_ctx, _packets| Ok(ReducerResult::default()));

        let packet = KernelPacket {
            body: json!({"size":"large"}),
            token_usage: Some(90),
            ..KernelPacket::default()
        };
        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget {
                    token_cap: Some(100),
                    byte_cap: None,
                    runtime_ms_cap: None,
                },
                steps: vec![
                    KernelStepRequest {
                        id: "a".to_string(),
                        target: "step.a".to_string(),
                        input_packets: vec![packet.clone()],
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "b".to_string(),
                        target: "step.b".to_string(),
                        input_packets: vec![packet],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        assert!(response.budget_exhausted);
        assert_eq!(response.scheduled, vec!["a".to_string()]);
        assert_eq!(response.skipped, vec!["b".to_string()]);
    }

    #[test]
    fn sequence_skips_dependent_step_after_failure() {
        let mut kernel = Kernel::new();
        kernel.register_reducer("step.fail", |_ctx, _packets| {
            Err(KernelError::ReducerFailed {
                target: "step.fail".to_string(),
                detail: "boom".to_string(),
            })
        });
        kernel.register_reducer("step.after", |_ctx, _packets| Ok(ReducerResult::default()));

        let response = kernel
            .execute_sequence(KernelSequenceRequest {
                budget: ExecutionBudget::default(),
                steps: vec![
                    KernelStepRequest {
                        id: "fail".to_string(),
                        target: "step.fail".to_string(),
                        ..KernelStepRequest::default()
                    },
                    KernelStepRequest {
                        id: "after".to_string(),
                        target: "step.after".to_string(),
                        depends_on: vec!["fail".to_string()],
                        ..KernelStepRequest::default()
                    },
                ],
            })
            .unwrap();

        let after = response
            .step_results
            .iter()
            .find(|step| step.id == "after")
            .unwrap();
        assert_eq!(after.status, "skipped");
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
