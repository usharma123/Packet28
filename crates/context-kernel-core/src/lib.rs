use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use roaring::RoaringBitmap;
use serde::de::DeserializeOwned;
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
    memory: Arc<Mutex<PacketCache>>,
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

    pub fn cache_entries(&self) -> Result<Vec<context_memory_core::PacketCacheEntry>, KernelError> {
        let cache = self
            .memory
            .lock()
            .map_err(|source| KernelError::CacheLock {
                detail: source.to_string(),
            })?;
        Ok(cache.entries())
    }
}

type ReducerFn = dyn Fn(&mut ExecutionContext, &[KernelPacket]) -> Result<ReducerResult, KernelError>
    + Send
    + Sync;

pub struct Kernel {
    reducers: HashMap<String, Arc<ReducerFn>>,
    next_request_id: AtomicU64,
    memory: Arc<Mutex<PacketCache>>,
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
            memory: Arc::new(Mutex::new(PacketCache::new())),
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
            memory: Arc::new(Mutex::new(PacketCache::load_from_disk(&config))),
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

        let cache_lookup = if cache_enabled_for_request(&target, &req.policy_context) {
            let cache_input = cache_input_for_request(&req, &target, policy_guard.as_ref());
            Some({
                let cache = self
                    .memory
                    .lock()
                    .map_err(|source| KernelError::CacheLock {
                        detail: source.to_string(),
                    })?;
                cache.lookup_with_hooks(&target, &cache_input, hooks)
            })
        } else {
            None
        };

        if let Some(entry) = cache_lookup
            .as_ref()
            .and_then(|lookup| lookup.entry.clone())
        {
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
                            "key": cache_lookup
                                .as_ref()
                                .map(|lookup| lookup.cache_key.clone())
                                .unwrap_or_default(),
                            "entry_age_secs": entry_age_secs,
                            "miss_reason": Value::Null,
                        }
                    }),
                ),
            });
        }

        let mut ctx = ExecutionContext {
            request_id,
            target: target.clone(),
            budget: req.budget,
            policy_context: req.policy_context.clone(),
            reducer_input: req.reducer_input,
            memory: self.memory.clone(),
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
    kernel.register_reducer("agenty.state.write", run_agenty_state_write);
    kernel.register_reducer("agenty.state.snapshot", run_agenty_state_snapshot);
    kernel.register_reducer("contextq.correlate", run_contextq_correlate);
    kernel.register_reducer("contextq.assemble", run_contextq_assemble);
    kernel.register_reducer("governed.assemble", run_governed_assemble);
    kernel.register_reducer("guardy.check", run_guardy_check);
    kernel.register_reducer("diffy.analyze", run_diffy_analyze_reducer);
    kernel.register_reducer("testy.impact", run_testy_impact_reducer);
    kernel.register_reducer("stacky.slice", run_stacky_slice);
    kernel.register_reducer("buildy.reduce", run_buildy_reduce);
    kernel.register_reducer("proxy.run", run_proxy_run);
    kernel.register_reducer("mapy.repo", run_mapy_repo);
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct ContextAssembleEnvelopePayload {
    sources: Vec<String>,
    sections: Vec<contextq_core::ContextSection>,
    refs: Vec<contextq_core::ContextRef>,
    truncated: bool,
    assembly: contextq_core::AssemblySummary,
    tool_invocations: Vec<contextq_core::ToolInvocation>,
    reducer_invocations: Vec<contextq_core::ReducerInvocation>,
    text_blobs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    debug: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct AgentSnapshotRequest {
    task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffAnalyzeKernelInput {
    pub base: String,
    pub head: String,
    pub fail_under_changed: Option<f64>,
    pub fail_under_total: Option<f64>,
    pub fail_under_new: Option<f64>,
    pub max_new_errors: Option<u32>,
    pub max_new_warnings: Option<u32>,
    pub max_new_issues: Option<u32>,
    pub issues: Vec<String>,
    pub issues_state: Option<String>,
    pub no_issues_state: bool,
    pub coverage: Vec<String>,
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffAnalyzeKernelOutput {
    pub gate_result: suite_packet_core::QualityGateResult,
    pub diagnostics: Option<suite_packet_core::DiagnosticsData>,
    pub diffs: Vec<SerializableFileDiff>,
}

impl Default for DiffAnalyzeKernelOutput {
    fn default() -> Self {
        Self {
            gate_result: suite_packet_core::QualityGateResult {
                passed: false,
                total_coverage_pct: None,
                changed_coverage_pct: None,
                new_file_coverage_pct: None,
                violations: Vec::new(),
                issue_counts: None,
            },
            diagnostics: None,
            diffs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableFileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: suite_packet_core::DiffStatus,
    pub changed_lines: Vec<u32>,
}

impl SerializableFileDiff {
    pub fn from_file_diff(diff: &suite_packet_core::FileDiff) -> Self {
        Self {
            path: diff.path.clone(),
            old_path: diff.old_path.clone(),
            status: diff.status,
            changed_lines: diff.changed_lines.iter().collect(),
        }
    }

    pub fn into_file_diff(self) -> suite_packet_core::FileDiff {
        let mut bitmap = RoaringBitmap::new();
        for line in self.changed_lines {
            bitmap.insert(line);
        }

        suite_packet_core::FileDiff {
            path: self.path,
            old_path: self.old_path,
            status: self.status,
            changed_lines: bitmap,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactKernelInput {
    pub base: Option<String>,
    pub head: Option<String>,
    pub testmap: String,
    pub print_command: bool,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImpactKernelOutput {
    pub result: suite_packet_core::ImpactResult,
    pub known_tests: usize,
    pub print_command: Option<String>,
}

fn format_pct(value: Option<f64>) -> String {
    value
        .map(|pct| format!("{pct:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn default_diff_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

fn ingest_coverage_auto(path: &Path) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_path(path).map_err(Into::into)
}

fn ingest_coverage_with_format(
    path: &Path,
    format: diffy_core::model::CoverageFormat,
) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
}

fn ingest_coverage_stdin(
    format: diffy_core::model::CoverageFormat,
) -> anyhow::Result<diffy_core::model::CoverageData> {
    covy_ingest::ingest_reader(std::io::stdin().lock(), format).map_err(Into::into)
}

fn ingest_diagnostics(
    path: &Path,
) -> anyhow::Result<diffy_core::diagnostics::DiagnosticsData> {
    covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

pub fn build_diff_pipeline_request(
    input: &DiffAnalyzeKernelInput,
) -> diffy_core::pipeline::PipelineRequest {
    diffy_core::pipeline::PipelineRequest {
        base: input.base.clone(),
        head: input.head.clone(),
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: input.coverage.clone(),
            format: None,
            stdin: false,
            input_state_path: input.input.clone(),
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes: Vec::new(),
            reject_paths_with_input: false,
            no_inputs_error: "No coverage data found. Run `covy ingest` first or use --coverage."
                .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: input.issues.clone(),
            issues_state_path: input.issues_state.clone(),
            no_issues_state: input.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: suite_foundation_core::config::GateConfig {
            fail_under_total: input.fail_under_total,
            fail_under_changed: input.fail_under_changed,
            fail_under_new: input.fail_under_new,
            issues: suite_foundation_core::config::IssueGateConfig {
                max_new_errors: input.max_new_errors,
                max_new_warnings: input.max_new_warnings,
                max_new_issues: input.max_new_issues,
            },
        },
    }
}

pub fn build_diff_analyze_envelope(
    output: &diffy_core::pipeline::PipelineOutput,
    base: &str,
    head: &str,
) -> suite_packet_core::EnvelopeV1<DiffAnalyzeKernelOutput> {
    let kernel_output = DiffAnalyzeKernelOutput {
        gate_result: output.gate_result.clone(),
        diagnostics: output.diagnostics.clone(),
        diffs: output
            .changed_line_context
            .diffs
            .iter()
            .map(SerializableFileDiff::from_file_diff)
            .collect(),
    };

    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();

    let gate_summary = format!(
        "passed: {}\nchanged_coverage_pct: {}\ntotal_coverage_pct: {}\nnew_file_coverage_pct: {}\nviolations: {}",
        kernel_output.gate_result.passed,
        format_pct(kernel_output.gate_result.changed_coverage_pct),
        format_pct(kernel_output.gate_result.total_coverage_pct),
        format_pct(kernel_output.gate_result.new_file_coverage_pct),
        if kernel_output.gate_result.violations.is_empty() {
            "none".to_string()
        } else {
            kernel_output.gate_result.violations.join("; ")
        }
    );

    let changed_file_body = if changed_paths.is_empty() {
        "No changed files".to_string()
    } else {
        changed_paths.join("\n")
    };

    let files = changed_paths
        .iter()
        .map(|path| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.75),
            source: Some("diffy.analyze".to_string()),
        })
        .collect::<Vec<_>>();
    let payload_bytes = serde_json::to_vec(&kernel_output).unwrap_or_default().len();

    suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "diffy".to_string(),
        kind: "diff_analyze".to_string(),
        hash: String::new(),
        summary: format!("{gate_summary}\nchanged_files: {changed_file_body}"),
        files,
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: now_unix(),
        },
        payload: kernel_output,
    }
    .with_canonical_hash_and_real_budget()
}

pub fn build_test_impact_envelope(
    output: &testy_core::command_impact::ImpactLegacyOutput,
    testmap_path: &str,
    git_base: Option<&str>,
    git_head: Option<&str>,
) -> suite_packet_core::EnvelopeV1<ImpactKernelOutput> {
    let impact_output = ImpactKernelOutput {
        result: output.result.clone(),
        known_tests: output.known_tests,
        print_command: output.print_command.clone(),
    };

    let mut paths = output.result.missing_mappings.clone();
    paths.sort();
    paths.dedup();

    let mut symbol_refs = output.result.selected_tests.clone();
    symbol_refs.extend(output.result.smoke_tests.clone());
    symbol_refs.sort();
    symbol_refs.dedup();

    let summary = format!(
        "selected: {}\nknown: {}\nmissing: {}\nconfidence: {:.2}\nstale: {}\nescalate_full_suite: {}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite,
    );

    let files = paths
        .iter()
        .map(|path: &String| suite_packet_core::FileRef {
            path: path.clone(),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();
    let symbols = symbol_refs
        .iter()
        .map(|symbol: &String| suite_packet_core::SymbolRef {
            name: symbol.clone(),
            file: None,
            kind: Some("test_id".to_string()),
            relevance: Some(0.8),
            source: Some("testy.impact".to_string()),
        })
        .collect::<Vec<_>>();

    let payload_bytes = serde_json::to_vec(&impact_output).unwrap_or_default().len();

    suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "testy".to_string(),
        kind: "test_impact".to_string(),
        hash: String::new(),
        summary,
        files,
        symbols,
        risk: None,
        confidence: Some(output.result.confidence.clamp(0.0, 1.0)),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![testmap_path.to_string()],
            git_base: git_base.map(ToOwned::to_owned),
            git_head: git_head.map(ToOwned::to_owned),
            generated_at_unix: now_unix(),
        },
        payload: impact_output,
    }
    .with_canonical_hash_and_real_budget()
}

fn build_context_correlation_packet(
    target: &str,
    task_id: Option<String>,
    findings: Vec<suite_packet_core::ContextCorrelationFinding>,
) -> Result<
    (
        suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload>,
        KernelPacket,
    ),
    KernelError,
> {
    let payload = suite_packet_core::ContextCorrelationPayload {
        task_id,
        finding_count: findings.len(),
        findings,
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();
    let mut files = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for finding in &payload.findings {
        for evidence in &finding.evidence_refs {
            if evidence.kind == "file" {
                files.insert(evidence.value.clone());
            } else {
                symbols.insert((evidence.kind.clone(), evidence.value.clone()));
            }
        }
    }

    let summary = if payload.findings.is_empty() {
        "correlation findings=0".to_string()
    } else {
        let preview = payload
            .findings
            .iter()
            .take(3)
            .map(|finding| finding.summary.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        format!("correlation findings={} :: {preview}", payload.findings.len())
    };

    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "contextq".to_string(),
        kind: "context_correlate".to_string(),
        hash: String::new(),
        summary,
        files: files
            .into_iter()
            .map(|path| suite_packet_core::FileRef {
                path,
                relevance: Some(1.0),
                source: Some("contextq.correlate".to_string()),
            })
            .collect(),
        symbols: symbols
            .into_iter()
            .map(|(kind, name)| suite_packet_core::SymbolRef {
                name,
                file: None,
                kind: Some(kind),
                relevance: Some(1.0),
                source: Some("contextq.correlate".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(if payload.findings.is_empty() { 1.0 } else { 0.85 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: payload
                .task_id
                .as_ref()
                .map(|task_id| vec![format!("task:{task_id}")])
                .unwrap_or_default(),
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "contextq-correlate-{}",
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
            "reducer": "contextq.correlate",
            "kind": "context_correlate",
            "hash": envelope.hash,
            "finding_count": envelope.payload.finding_count,
        }),
    };

    Ok((envelope, packet))
}

#[derive(Clone)]
struct CorrelatablePacket<T> {
    packet_id: Option<String>,
    packet_type: &'static str,
    envelope: suite_packet_core::EnvelopeV1<T>,
}

fn parse_correlatable_packet<T: DeserializeOwned + Default>(
    packet: &KernelPacket,
    packet_type: &'static str,
    tool: &str,
    kind: &str,
) -> Option<CorrelatablePacket<T>> {
    let value = extract_packet_value(&packet.body);
    let envelope = serde_json::from_value::<suite_packet_core::EnvelopeV1<T>>(value).ok()?;
    (envelope.tool == tool && envelope.kind == kind).then_some(CorrelatablePacket {
        packet_id: packet.packet_id.clone(),
        packet_type,
        envelope,
    })
}

fn diff_changed_files(
    packet: &CorrelatablePacket<DiffAnalyzeKernelOutput>,
) -> BTreeSet<String> {
    let from_payload = packet
        .envelope
        .payload
        .diffs
        .iter()
        .map(|diff| diff.path.clone())
        .collect::<BTreeSet<_>>();
    if from_payload.is_empty() {
        packet
            .envelope
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect()
    } else {
        from_payload
    }
}

fn packet_files<T>(packet: &CorrelatablePacket<T>) -> BTreeSet<String> {
    packet
        .envelope
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect()
}

fn map_has_edge(
    packet: &CorrelatablePacket<mapy_core::RepoMapPayload>,
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
) -> bool {
    packet.envelope.payload.edges.iter().any(|edge| {
        let Some(from) = packet.envelope.files.get(edge.from_file_idx).map(|file| &file.path) else {
            return false;
        };
        let Some(to) = packet.envelope.files.get(edge.to_file_idx).map(|file| &file.path) else {
            return false;
        };
        (left.contains(from) && right.contains(to)) || (left.contains(to) && right.contains(from))
    })
}

fn evidence_file_refs(
    packet_id: &Option<String>,
    packet_type: &'static str,
    values: impl IntoIterator<Item = String>,
) -> Vec<suite_packet_core::CorrelationEvidenceRef> {
    values
        .into_iter()
        .map(|value| suite_packet_core::CorrelationEvidenceRef {
            packet_id: packet_id.clone(),
            packet_type: packet_type.to_string(),
            kind: "file".to_string(),
            value,
        })
        .collect()
}

fn correlate_packets(
    input_packets: &[KernelPacket],
    task_id: Option<String>,
) -> Vec<suite_packet_core::ContextCorrelationFinding> {
    let diffs = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<DiffAnalyzeKernelOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
                "diffy",
                "diff_analyze",
            )
        })
        .collect::<Vec<_>>();
    let impacts = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<ImpactKernelOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                "testy",
                "test_impact",
            )
        })
        .collect::<Vec<_>>();
    let stacks = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<stacky_core::StackSliceOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_STACK_SLICE,
                "stacky",
                "stack_slice",
            )
        })
        .collect::<Vec<_>>();
    let builds = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<buildy_core::BuildReduceOutput>(
                packet,
                suite_packet_core::PACKET_TYPE_BUILD_REDUCE,
                "buildy",
                "build_reduce",
            )
        })
        .collect::<Vec<_>>();
    let maps = input_packets
        .iter()
        .filter_map(|packet| {
            parse_correlatable_packet::<mapy_core::RepoMapPayload>(
                packet,
                suite_packet_core::PACKET_TYPE_MAP_REPO,
                "mapy",
                "repo_map",
            )
        })
        .collect::<Vec<_>>();

    let mut findings = Vec::new();

    for diff in &diffs {
        let changed = diff_changed_files(diff);
        if changed.is_empty() {
            continue;
        }

        for stack in &stacks {
            let stack_files = packet_files(stack);
            if stack_files.is_empty() {
                continue;
            }
            let overlap = changed.intersection(&stack_files).cloned().collect::<Vec<_>>();
            let map_connected = maps.iter().any(|map| map_has_edge(map, &changed, &stack_files));
            let relation = if !overlap.is_empty() {
                "related"
            } else if !map_connected && !maps.is_empty() {
                "unrelated"
            } else {
                "possibly_related"
            };
            let summary = if !overlap.is_empty() {
                format!("Stack failures touch changed files: {}", overlap.join(", "))
            } else if relation == "unrelated" {
                format!(
                    "Stack failures in {} appear unrelated to diff in {}",
                    stack_files.iter().cloned().collect::<Vec<_>>().join(", "),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            } else {
                format!(
                    "Stack failures in {} may relate indirectly to diff in {}",
                    stack_files.iter().cloned().collect::<Vec<_>>().join(", "),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            let mut evidence_refs = evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(evidence_file_refs(
                &stack.packet_id,
                stack.packet_type,
                stack_files.clone(),
            ));
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_stack".to_string(),
                relation: relation.to_string(),
                confidence: if relation == "related" { 0.92 } else if relation == "unrelated" { 0.86 } else { 0.62 },
                summary,
                evidence_refs,
            });
        }

        for impact in &impacts {
            if impact.envelope.payload.result.selected_tests.is_empty() {
                continue;
            }
            let mut evidence_refs =
                evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(
                impact
                    .envelope
                    .payload
                    .result
                    .selected_tests
                    .iter()
                    .map(|test_id: &String| suite_packet_core::CorrelationEvidenceRef {
                        packet_id: impact.packet_id.clone(),
                        packet_type: impact.packet_type.to_string(),
                        kind: "test".to_string(),
                        value: test_id.clone(),
                    }),
            );
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_impact".to_string(),
                relation: "supports".to_string(),
                confidence: 0.78,
                summary: format!(
                    "Test impact selected {} tests for changed files {}",
                    impact.envelope.payload.result.selected_tests.len(),
                    changed.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
                evidence_refs,
            });
        }

        for build in &builds {
            let build_files = packet_files(build);
            let untouched = build_files
                .difference(&changed)
                .cloned()
                .collect::<Vec<_>>();
            if untouched.is_empty() {
                continue;
            }
            let mut evidence_refs =
                evidence_file_refs(&diff.packet_id, diff.packet_type, changed.clone());
            evidence_refs.extend(evidence_file_refs(
                &build.packet_id,
                build.packet_type,
                untouched.clone(),
            ));
            findings.push(suite_packet_core::ContextCorrelationFinding {
                rule: "diff_vs_build".to_string(),
                relation: "pre_existing_or_unrelated".to_string(),
                confidence: 0.84,
                summary: format!(
                    "Build diagnostics touch untouched files: {}",
                    untouched.join(", ")
                ),
                evidence_refs,
            });
        }
    }

    if findings.is_empty() && task_id.is_some() {
        Vec::new()
    } else {
        findings
    }
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

fn build_agent_state_packet(
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

fn run_agenty_state_write(
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

fn run_agenty_state_snapshot(
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

fn load_agent_snapshot(
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

fn run_contextq_correlate(
    ctx: &mut ExecutionContext,
    input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let task_id = ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
        .map(ToOwned::to_owned);
    let findings = correlate_packets(input_packets, task_id.clone());
    let (envelope, packet) = build_context_correlation_packet(&ctx.target, task_id.clone(), findings)?;

    if let Some(task_id) = task_id {
        ctx.set_shared("task_id", Value::String(task_id));
    }
    ctx.set_shared("correlation_findings", Value::from(envelope.payload.finding_count as u64));

    Ok(ReducerResult {
        output_packets: vec![packet],
        metadata: json!({
            "reducer": "contextq.correlate",
            "kind": "context_correlate",
            "finding_count": envelope.payload.finding_count,
        }),
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
        })?
        .to_string();

    let config = guardy_core::ContextConfig::load(Path::new(&config_path)).map_err(|source| {
        KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;

    let packet = kernel_packet_to_guard_packet(&input_packets[0])?;

    let audit = guardy_core::check_packet(&config, &packet);

    ctx.set_shared("passed", Value::Bool(audit.passed));
    ctx.set_shared("policy_version", Value::from(audit.policy_version));

    let mut files = Vec::new();
    let mut symbols = Vec::new();
    for finding in &audit.findings {
        if finding.subject.contains('/') || finding.subject.contains('.') {
            files.push(suite_packet_core::FileRef {
                path: finding.subject.clone(),
                relevance: Some(1.0),
                source: Some("guardy.check".to_string()),
            });
        } else {
            symbols.push(suite_packet_core::SymbolRef {
                name: finding.subject.clone(),
                file: None,
                kind: Some("policy_subject".to_string()),
                relevance: Some(1.0),
                source: Some("guardy.check".to_string()),
            });
        }
    }

    let payload_bytes = serde_json::to_vec(&audit)
        .map(|buf| buf.len())
        .unwrap_or_default();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "guardy".to_string(),
        kind: "guard_check".to_string(),
        hash: String::new(),
        summary: format!(
            "guard check passed={} findings={}",
            audit.passed,
            audit.findings.len()
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(if audit.passed { 1.0 } else { 0.75 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: vec![config_path.to_string()],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload: audit.clone(),
    }
    .with_canonical_hash_and_real_budget();

    let packet = KernelPacket {
        packet_id: Some(format!(
            "guardy-{}",
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
            "reducer": "guardy.check",
            "passed": audit.passed,
            "findings": audit.findings.len(),
            "kind": "guard_check",
            "hash": envelope.hash,
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

fn run_diffy_analyze_reducer(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: DiffAnalyzeKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let request = build_diff_pipeline_request(&input);
    let adapters = default_diff_pipeline_ingest_adapters();
    let output = diffy_core::pipeline::run_analysis(request, &adapters).map_err(|source| {
        KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: source.to_string(),
        }
    })?;
    let envelope = build_diff_analyze_envelope(&output, &git_base, &git_head);

    let mut output_packets = vec![KernelPacket {
        packet_id: Some(format!(
            "diffy-{}",
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
            "reducer": "diffy.analyze",
            "kind": "diff_analyze",
            "hash": envelope.hash,
            "passed": output.gate_result.passed,
        }),
    }];

    if let Some(task_id) = ctx
        .policy_context
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.trim().is_empty())
    {
        let mut changed_paths = output
            .changed_line_context
            .changed_paths
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        changed_paths.sort();

        let clear_focus = suite_packet_core::AgentStateEventPayload {
            task_id: task_id.to_string(),
            event_id: format!("system-diff-focus-clear-{}", envelope.hash),
            occurred_at_unix: envelope.provenance.generated_at_unix,
            actor: "system:diffy.analyze".to_string(),
            paths: Vec::new(),
            symbols: Vec::new(),
            kind: suite_packet_core::AgentStateEventKind::FocusCleared,
            data: suite_packet_core::AgentStateEventData::FocusCleared { clear_all: true },
        };
        validate_agent_state_event(&clear_focus)
            .map_err(|detail| KernelError::InvalidRequest { detail })?;
        let (_, focus_clear_packet) =
            build_agent_state_packet(&ctx.target, &clear_focus, "diffy.analyze")?;
        output_packets.push(focus_clear_packet);

        if !changed_paths.is_empty() {
            let focus_event = suite_packet_core::AgentStateEventPayload {
                task_id: task_id.to_string(),
                event_id: format!("system-diff-focus-set-{}", envelope.hash),
                occurred_at_unix: envelope.provenance.generated_at_unix,
                actor: "system:diffy.analyze".to_string(),
                paths: changed_paths.clone(),
                symbols: Vec::new(),
                kind: suite_packet_core::AgentStateEventKind::FocusSet,
                data: suite_packet_core::AgentStateEventData::FocusSet { note: None },
            };
            validate_agent_state_event(&focus_event)
                .map_err(|detail| KernelError::InvalidRequest { detail })?;
            let (_, focus_packet) =
                build_agent_state_packet(&ctx.target, &focus_event, "diffy.analyze")?;
            output_packets.push(focus_packet);
        }

        let step_event = suite_packet_core::AgentStateEventPayload {
            task_id: task_id.to_string(),
            event_id: format!("system-diff-step-{}", envelope.hash),
            occurred_at_unix: envelope.provenance.generated_at_unix,
            actor: "system:diffy.analyze".to_string(),
            paths: changed_paths,
            symbols: Vec::new(),
            kind: suite_packet_core::AgentStateEventKind::StepCompleted,
            data: suite_packet_core::AgentStateEventData::StepCompleted {
                step_id: "diff.analyze".to_string(),
            },
        };
        validate_agent_state_event(&step_event)
            .map_err(|detail| KernelError::InvalidRequest { detail })?;
        let (_, step_packet) = build_agent_state_packet(&ctx.target, &step_event, "diffy.analyze")?;
        output_packets.push(step_packet);

        ctx.set_shared("task_id", Value::String(task_id.to_string()));
    }

    Ok(ReducerResult {
        output_packets,
        metadata: json!({
            "reducer": "diffy.analyze",
            "kind": "diff_analyze",
            "passed": output.gate_result.passed,
        }),
    })
}

fn run_testy_impact_reducer(
    ctx: &mut ExecutionContext,
    _input_packets: &[KernelPacket],
) -> Result<ReducerResult, KernelError> {
    let input: ImpactKernelInput =
        serde_json::from_value(ctx.reducer_input.clone()).map_err(|source| {
            KernelError::ReducerFailed {
                target: ctx.target.clone(),
                detail: format!("invalid reducer input: {source}"),
            }
        })?;

    let testmap_path = input.testmap.clone();
    let git_base = input.base.clone();
    let git_head = input.head.clone();
    let adapters = testy_cli_common::adapters::default_impact_adapters();
    let output = testy_core::command_impact::run_legacy_impact(
        testy_core::command_impact::LegacyImpactArgs {
            base: input.base.clone(),
            head: input.head.clone(),
            testmap: input.testmap.clone(),
            print_command: input.print_command,
        },
        &input.config_path,
        &adapters,
    )
    .map_err(|source| KernelError::ReducerFailed {
        target: ctx.target.clone(),
        detail: source.to_string(),
    })?;

    let envelope =
        build_test_impact_envelope(&output, &testmap_path, git_base.as_deref(), git_head.as_deref());

    Ok(ReducerResult {
        output_packets: vec![KernelPacket {
            packet_id: Some(format!(
                "testy-{}",
                envelope.hash.chars().take(12).collect::<String>()
            )),
            format: "packet-json".to_string(),
            body: serde_json::to_value(&envelope).map_err(|source| {
                KernelError::ReducerFailed {
                    target: ctx.target.clone(),
                    detail: source.to_string(),
                }
            })?,
            token_usage: Some(envelope.budget_cost.est_tokens),
            runtime_ms: Some(envelope.budget_cost.runtime_ms),
            metadata: json!({
                "reducer": "testy.impact",
                "kind": "test_impact",
                "hash": envelope.hash,
                "selected_tests": output.result.selected_tests.len(),
            }),
        }],
        metadata: json!({
            "reducer": "testy.impact",
            "kind": "test_impact",
            "selected_tests": output.result.selected_tests.len(),
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

    let agent_snapshot = load_agent_snapshot(ctx)?;
    let envelope = stacky_core::slice_to_envelope(input);
    let payload = envelope.payload.clone();
    let focus_paths = agent_snapshot
        .as_ref()
        .map(|snapshot| snapshot.focus_paths.clone())
        .unwrap_or_default();
    let mut matching_files = BTreeSet::new();
    let mut unrelated_files = BTreeSet::new();
    for file in envelope.files.iter().map(|file| file.path.as_str()) {
        if path_matches_any(&focus_paths, file) {
            matching_files.insert(file.to_string());
        } else {
            unrelated_files.insert(file.to_string());
        }
    }

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "stacky-{}",
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
            "reducer": "slice",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
            "task_state": {
                "focus_paths": focus_paths,
                "matching_files": matching_files,
                "unrelated_files": unrelated_files,
            },
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "stacky.slice",
            "kind": "stack_slice",
            "unique_failures": payload.unique_failures,
            "duplicates_removed": payload.duplicates_removed,
            "task_state": {
                "matching_failures": matching_files.len(),
                "unrelated_failures": unrelated_files.len(),
            },
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

    let envelope = buildy_core::reduce_to_envelope(input);
    let payload = envelope.payload.clone();

    let kernel_packet = KernelPacket {
        packet_id: Some(format!(
            "buildy-{}",
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
            "reducer": "reduce",
            "kind": envelope.kind,
            "hash": envelope.hash,
            "unique_diagnostics": payload.unique_diagnostics,
            "duplicates_removed": payload.duplicates_removed,
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "buildy.reduce",
            "kind": "build_reduce",
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
    let mut input: mapy_core::RepoMapRequest = serde_json::from_value(ctx.reducer_input.clone())
        .map_err(|source| KernelError::ReducerFailed {
            target: ctx.target.clone(),
            detail: format!("invalid reducer input: {source}"),
        })?;
    if let Some(snapshot) = load_agent_snapshot(ctx)? {
        for path in snapshot.focus_paths {
            if !input.focus_paths.iter().any(|existing| existing == &path) {
                input.focus_paths.push(path);
            }
        }
        for symbol in snapshot.focus_symbols {
            if !input.focus_symbols.iter().any(|existing| existing == &symbol) {
                input.focus_symbols.push(symbol);
            }
        }
    }
    let effective_focus_paths = input.focus_paths.clone();
    let effective_focus_symbols = input.focus_symbols.clone();

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
            "focus_paths": effective_focus_paths,
            "focus_symbols": effective_focus_symbols,
            "focus_hits": envelope.payload.focus_hits.clone(),
        }),
    };

    Ok(ReducerResult {
        output_packets: vec![kernel_packet],
        metadata: json!({
            "reducer": "mapy.repo",
            "kind": "repo_map",
            "focus_paths": effective_focus_paths,
            "focus_symbols": effective_focus_symbols,
            "focus_hits": envelope.payload.focus_hits.clone(),
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

fn extract_packet_value(value: &Value) -> Value {
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

fn cache_enabled_for_request(target: &str, policy_context: &Value) -> bool {
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

fn validate_agent_state_event(
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
        _ => {
            return Err(format!(
                "event kind '{:?}' does not match payload variant",
                event.kind
            ));
        }
    }

    Ok(())
}

fn summarize_agent_state_event(event: &suite_packet_core::AgentStateEventPayload) -> String {
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
    }
}

fn derive_agent_snapshot(
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
                }
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
        }
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
    }
}

fn extract_agent_state_events(
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

fn path_matches_any(patterns: &[String], candidate: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty()
            && (candidate == pattern
                || candidate.starts_with(pattern)
                || pattern.starts_with(candidate)
                || candidate.contains(pattern))
    })
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
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex, OnceLock};
    use tempfile::tempdir;

    fn fixture(rel: &str) -> String {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        workspace
            .join("tests")
            .join("fixtures")
            .join(rel)
            .to_string_lossy()
            .to_string()
    }

    fn git_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed with {status}", args);
    }

    fn setup_diff_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
        std::fs::write(dir.join("src/beta.rs"), "pub fn beta() -> i32 { 2 }\n").unwrap();

        git(dir, &["init"]);
        git(dir, &["add", "src/alpha.rs", "src/beta.rs"]);
        git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
        );

        std::fs::write(dir.join("src/alpha.rs"), "pub fn alpha() -> i32 { 3 }\n").unwrap();
        git(dir, &["add", "src/alpha.rs"]);
        git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "change alpha",
            ],
        );
    }

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
        let kind = response.output_packets[0]
            .body
            .get("kind")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(kind, "context_assemble");
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
            .get("payload")
            .and_then(|payload| payload.get("passed"))
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
                "schema_version": "suite.packet.v1",
                "packet_type": "suite.proxy.run.v1",
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
            .get("payload")
            .and_then(|payload| payload.get("passed"))
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
    fn cache_fingerprint_changes_force_cache_miss() {
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

        let mut request = KernelRequest {
            target: "count.reducer".to_string(),
            reducer_input: json!({"task":"same"}),
            policy_context: json!({"cache_fingerprint":"fp-1"}),
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

        let second = kernel.execute(request.clone()).unwrap();
        assert_eq!(
            second
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(true)
        );

        request.policy_context = json!({"cache_fingerprint":"fp-2"});
        let third = kernel.execute(request).unwrap();
        assert_eq!(
            third
                .metadata
                .get("cache")
                .and_then(|v| v.get("hit"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
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
    fn agenty_state_write_rejects_invalid_event_shape() {
        let kernel = Kernel::with_v1_reducers();
        let err = kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: json!({
                    "task_id": "task-a",
                    "event_id": "evt-1",
                    "occurred_at_unix": 1,
                    "actor": "agent",
                    "kind": "focus_set",
                    "data": {"type": "focus_set"}
                }),
                ..KernelRequest::default()
            })
            .unwrap_err();

        assert!(matches!(err, KernelError::InvalidRequest { .. }));
    }

    #[test]
    fn agenty_state_snapshot_derives_current_task_state() {
        let dir = tempdir().unwrap();
        let kernel =
            Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
        let events = [
            json!({
                "task_id": "task-a",
                "event_id": "evt-1",
                "occurred_at_unix": 1,
                "actor": "agent",
                "kind": "focus_set",
                "paths": ["src/time/StopWatch.java"],
                "symbols": ["split"],
                "data": {"type": "focus_set"}
            }),
            json!({
                "task_id": "task-a",
                "event_id": "evt-2",
                "occurred_at_unix": 2,
                "actor": "agent",
                "kind": "decision_added",
                "data": {
                    "type": "decision_added",
                    "decision_id": "d1",
                    "text": "Bug is in split()",
                    "supersedes": null
                }
            }),
            json!({
                "task_id": "task-a",
                "event_id": "evt-3",
                "occurred_at_unix": 3,
                "actor": "agent",
                "kind": "question_opened",
                "data": {
                    "type": "question_opened",
                    "question_id": "q1",
                    "text": "Does DateUtils call split()?"
                }
            }),
            json!({
                "task_id": "task-a",
                "event_id": "evt-4",
                "occurred_at_unix": 4,
                "actor": "agent",
                "kind": "question_resolved",
                "data": {
                    "type": "question_resolved",
                    "question_id": "q1"
                }
            }),
            json!({
                "task_id": "task-a",
                "event_id": "evt-5",
                "occurred_at_unix": 5,
                "actor": "agent",
                "kind": "step_completed",
                "data": {
                    "type": "step_completed",
                    "step_id": "read_diff"
                }
            }),
        ];

        for event in events {
            kernel
                .execute(KernelRequest {
                    target: "agenty.state.write".to_string(),
                    reducer_input: event,
                    ..KernelRequest::default()
                })
                .unwrap();
        }

        let response = kernel
            .execute(KernelRequest {
                target: "agenty.state.snapshot".to_string(),
                reducer_input: json!({
                    "task_id": "task-a"
                }),
                policy_context: json!({
                    "disable_cache": true
                }),
                ..KernelRequest::default()
            })
            .unwrap();
        let packet = response.output_packets.first().unwrap();
        let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
            serde_json::from_value(packet.body.clone()).unwrap();

        assert_eq!(envelope.payload.task_id, "task-a");
        assert_eq!(envelope.payload.event_count, 5);
        assert_eq!(
            envelope.payload.focus_paths,
            vec!["src/time/StopWatch.java".to_string()]
        );
        assert_eq!(envelope.payload.focus_symbols, vec!["split".to_string()]);
        assert_eq!(
            envelope.payload.completed_steps,
            vec!["read_diff".to_string()]
        );
        assert!(envelope.payload.open_questions.is_empty());
        assert_eq!(envelope.payload.active_decisions.len(), 1);
        assert_eq!(envelope.payload.active_decisions[0].id, "d1");
    }

    #[test]
    fn diffy_analyze_emits_task_state_focus_packets() {
        let _lock = git_test_lock().lock().unwrap();
        let dir = tempdir().unwrap();
        setup_diff_repo(dir.path());
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let kernel =
            Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
        let response = kernel
            .execute(KernelRequest {
                target: "diffy.analyze".to_string(),
                reducer_input: json!({
                    "base": "HEAD~1",
                    "head": "HEAD",
                    "fail_under_changed": null,
                    "fail_under_total": null,
                    "fail_under_new": null,
                    "max_new_errors": null,
                    "max_new_warnings": null,
                    "max_new_issues": null,
                    "issues": [],
                    "issues_state": null,
                    "no_issues_state": true,
                    "coverage": [fixture("lcov/basic.info")],
                    "input": null
                }),
                policy_context: json!({
                    "task_id": "task-diff"
                }),
                ..KernelRequest::default()
            })
            .unwrap();

        std::env::set_current_dir(original_dir).unwrap();

        assert_eq!(response.output_packets.len(), 4);
        let focus_packet = response
            .output_packets
            .iter()
            .find(|packet| {
                packet
                    .metadata
                    .get("event_kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "focus_set")
            })
            .expect("focus_set packet should be emitted");
        let focus_envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload> =
            serde_json::from_value(focus_packet.body.clone()).unwrap();
        assert_eq!(focus_envelope.payload.paths, vec!["src/alpha.rs"]);

        let snapshot = kernel
            .execute(KernelRequest {
                target: "agenty.state.snapshot".to_string(),
                reducer_input: json!({
                    "task_id": "task-diff"
                }),
                policy_context: json!({
                    "disable_cache": true
                }),
                ..KernelRequest::default()
            })
            .unwrap();
        let snapshot_envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
            serde_json::from_value(snapshot.output_packets[0].body.clone()).unwrap();
        assert_eq!(snapshot_envelope.payload.focus_paths, vec!["src/alpha.rs"]);
        assert!(snapshot_envelope
            .payload
            .completed_steps
            .iter()
            .any(|step| step == "diff.analyze"));
    }

    #[test]
    fn contextq_assemble_includes_correlation_findings_for_task() {
        let kernel = Kernel::with_v1_reducers();

        let diff_packet = KernelPacket::from_value(
            serde_json::to_value(
                suite_packet_core::EnvelopeV1 {
                    version: "1".to_string(),
                    tool: "diffy".to_string(),
                    kind: "diff_analyze".to_string(),
                    hash: String::new(),
                    summary: "changed StopWatch".to_string(),
                    files: vec![suite_packet_core::FileRef {
                        path: "src/StopWatch.java".to_string(),
                        relevance: Some(1.0),
                        source: Some("diffy.analyze".to_string()),
                    }],
                    symbols: Vec::new(),
                    risk: None,
                    confidence: Some(1.0),
                    budget_cost: suite_packet_core::BudgetCost::default(),
                    provenance: suite_packet_core::Provenance {
                        inputs: vec!["diff".to_string()],
                        git_base: Some("HEAD~1".to_string()),
                        git_head: Some("HEAD".to_string()),
                        generated_at_unix: 1,
                    },
                    payload: DiffAnalyzeKernelOutput {
                        gate_result: suite_packet_core::QualityGateResult {
                            passed: true,
                            total_coverage_pct: None,
                            changed_coverage_pct: None,
                            new_file_coverage_pct: None,
                            violations: Vec::new(),
                            issue_counts: None,
                        },
                        diagnostics: None,
                        diffs: vec![SerializableFileDiff {
                            path: "src/StopWatch.java".to_string(),
                            old_path: None,
                            status: suite_packet_core::DiffStatus::Modified,
                            changed_lines: vec![10, 11],
                        }],
                    },
                }
                .with_canonical_hash_and_real_budget(),
            )
            .unwrap(),
            Some("diff".to_string()),
        );

        let stack_packet = KernelPacket::from_value(
            serde_json::to_value(stacky_core::slice_to_envelope(stacky_core::StackSliceRequest {
                log_text: r#"
java.lang.IllegalStateException: boom
  at org.example.ArrayUtils.run(src/ArrayUtils.java:42)
"#
                .to_string(),
                source: Some("stack.log".to_string()),
                max_failures: None,
            }))
            .unwrap(),
            Some("stack".to_string()),
        );

        let map_packet = KernelPacket::from_value(
            serde_json::to_value(
                suite_packet_core::EnvelopeV1 {
                    version: "1".to_string(),
                    tool: "mapy".to_string(),
                    kind: "repo_map".to_string(),
                    hash: String::new(),
                    summary: "repo map".to_string(),
                    files: vec![
                        suite_packet_core::FileRef {
                            path: "src/StopWatch.java".to_string(),
                            relevance: Some(1.0),
                            source: Some("mapy.repo".to_string()),
                        },
                        suite_packet_core::FileRef {
                            path: "src/ArrayUtils.java".to_string(),
                            relevance: Some(0.8),
                            source: Some("mapy.repo".to_string()),
                        },
                    ],
                    symbols: Vec::new(),
                    risk: None,
                    confidence: Some(1.0),
                    budget_cost: suite_packet_core::BudgetCost::default(),
                    provenance: suite_packet_core::Provenance {
                        inputs: vec!["repo".to_string()],
                        git_base: None,
                        git_head: None,
                        generated_at_unix: 1,
                    },
                    payload: mapy_core::RepoMapPayload {
                        files_ranked: vec![
                            mapy_core::RankedFile {
                                file_idx: 0,
                                score: 1.0,
                                symbol_count: 1,
                                import_count: 0,
                            },
                            mapy_core::RankedFile {
                                file_idx: 1,
                                score: 0.8,
                                symbol_count: 1,
                                import_count: 0,
                            },
                        ],
                        symbols_ranked: Vec::new(),
                        edges: Vec::new(),
                        focus_hits: Vec::new(),
                        truncation: mapy_core::TruncationSummary::default(),
                    },
                }
                .with_canonical_hash_and_real_budget(),
            )
            .unwrap(),
            Some("map".to_string()),
        );

        let response = kernel
            .execute(KernelRequest {
                target: "contextq.assemble".to_string(),
                input_packets: vec![diff_packet, stack_packet, map_packet],
                budget: ExecutionBudget {
                    token_cap: Some(1500),
                    byte_cap: Some(100_000),
                    runtime_ms_cap: None,
                },
                policy_context: json!({
                    "task_id": "task-correlation",
                    "disable_cache": true
                }),
                ..KernelRequest::default()
            })
            .unwrap();

        let envelope: suite_packet_core::EnvelopeV1<ContextAssembleEnvelopePayload> =
            serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
        let bodies = envelope
            .payload
            .sections
            .iter()
            .map(|section| section.body.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(bodies.contains("appear unrelated to diff"));
    }

    #[test]
    fn contextq_assemble_uses_task_snapshot_to_compress_read_sections() {
        let dir = tempdir().unwrap();
        let kernel =
            Kernel::with_v1_reducers_and_persistence(PersistConfig::new(dir.path().to_path_buf()));
        kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: json!({
                    "task_id": "task-a",
                    "event_id": "evt-1",
                    "occurred_at_unix": 1,
                    "actor": "agent",
                    "kind": "file_read",
                    "paths": ["src/time/StopWatch.java"],
                    "data": {"type": "file_read"}
                }),
                ..KernelRequest::default()
            })
            .unwrap();

        let packet = KernelPacket::from_value(
            json!({
                "packet_id": "diffy",
                "sections": [{
                    "title": "Diff",
                    "body": "StopWatch.java changed on lines 10-20",
                    "refs": [{"kind": "file", "value": "src/time/StopWatch.java"}],
                    "relevance": 0.9
                }]
            }),
            None,
        );
        let response = kernel
            .execute(KernelRequest {
                target: "contextq.assemble".to_string(),
                input_packets: vec![packet],
                policy_context: json!({
                    "task_id": "task-a",
                    "disable_cache": true
                }),
                ..KernelRequest::default()
            })
            .unwrap();
        let envelope: suite_packet_core::EnvelopeV1<ContextAssembleEnvelopePayload> =
            serde_json::from_value(response.output_packets[0].body.clone()).unwrap();
        assert!(envelope.payload.sections[0]
            .body
            .starts_with("Reminder: already reviewed"));
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
