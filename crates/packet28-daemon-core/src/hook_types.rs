use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    CommandStarted,
    CommandProgress,
    CommandFinished,
    Stop,
    SubagentStop,
    PreCompact,
    SessionEnd,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HookBoundaryKind {
    Stop,
    SubagentStop,
    PreCompact,
    SessionEnd,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RelaunchPreference {
    /// Daemon will auto-relaunch the agent when handoff is ready at a stop boundary.
    #[default]
    DaemonManaged,
    /// Disable auto-relaunch; the host is responsible for restarting.
    HostManaged,
}

/// Graduated context pressure level. The daemon computes this from the
/// accumulated hook-window tokens relative to the configured budget.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdLevel {
    /// Below warn threshold — no action needed.
    #[default]
    Normal,
    /// Warn threshold crossed — agent should start recording intent.
    Warn,
    /// Prepare threshold crossed — handoff will be assembled at next boundary.
    Prepare,
    /// Force threshold crossed — handoff assembled and relaunch requested.
    Force,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct HookReducerPacket {
    pub packet_type: String,
    pub tool_name: String,
    pub operation_kind: suite_packet_core::ToolOperationKind,
    pub reducer_family: Option<String>,
    pub canonical_command_kind: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_preview: Option<String>,
    pub command: Option<String>,
    pub search_query: Option<String>,
    pub compact_path: Option<String>,
    pub passthrough_reason: Option<String>,
    pub raw_est_tokens: Option<u64>,
    pub reduced_est_tokens: Option<u64>,
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub equivalence_key: Option<String>,
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub failed: bool,
    pub error_class: Option<String>,
    pub error_message: Option<String>,
    pub retryable: Option<bool>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub cache_fingerprint: Option<String>,
    pub cacheable: Option<bool>,
    pub mutation: Option<bool>,
    pub raw_artifact_handle: Option<String>,
    pub raw_artifact_available: bool,
    pub artifact: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HookLifecycleKind {
    #[default]
    CommandStarted,
    CommandProgress,
    CommandFinished,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HookLifecycleEvent {
    pub kind: HookLifecycleKind,
    pub command_id: Option<String>,
    pub reducer_family: Option<String>,
    pub canonical_command_kind: Option<String>,
    pub cache_fingerprint: Option<String>,
    pub stdout_spool_path: Option<String>,
    pub stderr_spool_path: Option<String>,
    pub stdout_bytes: Option<u64>,
    pub stderr_bytes: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HookIngestRequest {
    pub task_id: String,
    pub session_id: Option<String>,
    pub event_kind: HookEventKind,
    pub matcher: Option<String>,
    pub source: Option<String>,
    pub boundary_kind: HookBoundaryKind,
    pub lifecycle_event: Option<HookLifecycleEvent>,
    pub reducer_packet: Option<HookReducerPacket>,
    /// Host-provided context budget in tokens. When set, overrides the
    /// daemon's `context_budget_tokens` for threshold calculations, allowing
    /// the budget to track the actual model context window.
    pub host_context_budget_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HookIngestResponse {
    pub task_id: String,
    pub accepted: bool,
    pub handoff_ready: bool,
    pub handoff_reason: Option<String>,
    pub handoff: Option<crate::BrokerHandoffDescriptor>,
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_context_version: Option<String>,
    pub additional_context: Option<String>,
    pub block_stop: bool,
    pub stop_reason: Option<String>,
    pub cache_hit: bool,
    /// Current graduated context pressure level.
    pub threshold_level: ThresholdLevel,
    /// When true, the daemon has queued an auto-relaunch of the agent from the
    /// prepared handoff. The host should allow the stop to proceed.
    pub relaunch_requested: bool,
    /// Which relaunch preference was applied.
    pub relaunch_preference: RelaunchPreference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HookRuntimeConfig {
    pub hooks_enabled: bool,
    pub rewrite_enabled: bool,
    pub fallback_post_tool_capture: bool,
    /// Base context budget in tokens. Overridden at runtime when the host
    /// passes `host_context_budget_tokens` in the ingest request.
    pub context_budget_tokens: u64,
    /// Legacy single-threshold fraction (kept for backwards compatibility).
    /// Mapped to `warn_threshold_fraction` when graduated fractions are unset.
    #[serde(deserialize_with = "deserialize_soft_threshold_fraction")]
    pub soft_threshold_fraction: f64,
    /// Graduated threshold: agent should start recording intent.
    #[serde(
        default = "default_warn_fraction",
        deserialize_with = "deserialize_opt_fraction"
    )]
    pub warn_threshold_fraction: f64,
    /// Graduated threshold: handoff will be assembled at next boundary.
    #[serde(
        default = "default_prepare_fraction",
        deserialize_with = "deserialize_opt_fraction"
    )]
    pub prepare_threshold_fraction: f64,
    /// Graduated threshold: handoff assembled and relaunch requested.
    #[serde(
        default = "default_force_fraction",
        deserialize_with = "deserialize_opt_fraction"
    )]
    pub force_threshold_fraction: f64,
    pub relaunch_preference: RelaunchPreference,
    /// Command the daemon will use to auto-relaunch the agent.
    /// Example: `["claude", "--task-id", "{{task_id}}", "--resume"]`
    pub relaunch_command: Vec<String>,
    /// Tee mode for raw output capture: "never", "failures", or "always".
    #[serde(default)]
    pub tee_mode: Option<String>,
    /// Directory for tee output files.
    #[serde(default)]
    pub tee_directory: Option<String>,
    /// Default filter level for read operations: "none", "minimal", or "aggressive".
    #[serde(default)]
    pub filter_level: Option<String>,
    /// Whether to run integrity checks on hooks.
    #[serde(default)]
    pub integrity_check: Option<bool>,
    pub reducer_allowlist: Vec<String>,
}

fn default_warn_fraction() -> f64 {
    0.6
}
fn default_prepare_fraction() -> f64 {
    0.75
}
fn default_force_fraction() -> f64 {
    0.9
}

impl HookRuntimeConfig {
    pub fn soft_threshold_fraction_value(&self) -> f64 {
        Some(self.soft_threshold_fraction)
            .filter(|value| *value > 0.0)
            .unwrap_or(0.5)
    }

    /// Legacy soft threshold (backward compat). Prefer `threshold_tokens_for_level`.
    pub fn soft_threshold_tokens(&self) -> u64 {
        self.threshold_tokens_for_level(ThresholdLevel::Prepare)
    }

    /// Effective budget, optionally overridden by the host at runtime.
    pub fn effective_budget(&self, host_override: Option<u64>) -> u64 {
        host_override
            .filter(|v| *v > 0)
            .unwrap_or(self.context_budget_tokens)
            .max(1)
    }

    /// Compute the token threshold for a given graduated level.
    pub fn threshold_tokens_for_level(&self, level: ThresholdLevel) -> u64 {
        self.threshold_tokens_for_level_with_budget(level, self.context_budget_tokens)
    }

    /// Compute the token threshold for a given level using a specific budget.
    pub fn threshold_tokens_for_level_with_budget(
        &self,
        level: ThresholdLevel,
        budget: u64,
    ) -> u64 {
        let budget = budget.max(1);
        let fraction = match level {
            ThresholdLevel::Normal => return 0,
            ThresholdLevel::Warn => self.warn_threshold_fraction,
            ThresholdLevel::Prepare => self.prepare_threshold_fraction,
            ThresholdLevel::Force => self.force_threshold_fraction,
        };
        let fraction = if fraction > 0.0 { fraction } else { 0.5 };
        ((budget as f64) * fraction).round() as u64
    }

    /// Determine the current threshold level for a given token count.
    pub fn compute_threshold_level(&self, tokens: u64, budget: u64) -> ThresholdLevel {
        let force = self.threshold_tokens_for_level_with_budget(ThresholdLevel::Force, budget);
        let prepare = self.threshold_tokens_for_level_with_budget(ThresholdLevel::Prepare, budget);
        let warn = self.threshold_tokens_for_level_with_budget(ThresholdLevel::Warn, budget);
        if tokens >= force {
            ThresholdLevel::Force
        } else if tokens >= prepare {
            ThresholdLevel::Prepare
        } else if tokens >= warn {
            ThresholdLevel::Warn
        } else {
            ThresholdLevel::Normal
        }
    }
}

impl Default for HookRuntimeConfig {
    fn default() -> Self {
        Self {
            hooks_enabled: true,
            rewrite_enabled: true,
            fallback_post_tool_capture: true,
            context_budget_tokens: 200_000,
            soft_threshold_fraction: 0.5,
            warn_threshold_fraction: default_warn_fraction(),
            prepare_threshold_fraction: default_prepare_fraction(),
            force_threshold_fraction: default_force_fraction(),
            relaunch_preference: RelaunchPreference::DaemonManaged,
            relaunch_command: Vec::new(),
            reducer_allowlist: vec![
                "claude_native".to_string(),
                "git".to_string(),
                "fs".to_string(),
                "rust".to_string(),
                "github".to_string(),
                "python".to_string(),
                "javascript".to_string(),
                "go".to_string(),
                "infra".to_string(),
            ],
            tee_mode: None,
            tee_directory: None,
            filter_level: None,
            integrity_check: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ActiveTaskRecord {
    pub task_id: String,
    pub session_id: Option<String>,
    pub updated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HookReducerCacheEntry {
    pub reducer_family: String,
    pub canonical_command_kind: String,
    pub cache_fingerprint: String,
    pub summary: String,
    #[serde(default)]
    pub compact_preview: Option<String>,
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub artifact_id: Option<String>,
    pub raw_artifact_handle: Option<String>,
    pub failed: bool,
    pub error_message: Option<String>,
    pub exit_code: Option<i32>,
    pub occurred_at_unix: u64,
    pub git_epoch: u64,
    pub fs_epoch: u64,
    pub rust_epoch: u64,
}

fn deserialize_soft_threshold_fraction<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FractionValue {
        Number(f64),
        String(String),
    }

    match Option::<FractionValue>::deserialize(deserializer)? {
        Some(FractionValue::Number(value)) if value > 0.0 => Ok(value),
        Some(FractionValue::String(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|parsed| *parsed > 0.0)
            .ok_or_else(|| serde::de::Error::custom("soft_threshold_fraction must be > 0")),
        _ => Ok(0.5),
    }
}

fn deserialize_opt_fraction<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FractionValue {
        Number(f64),
        String(String),
    }

    match Option::<FractionValue>::deserialize(deserializer)? {
        Some(FractionValue::Number(value)) if value > 0.0 => Ok(value),
        Some(FractionValue::String(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|parsed| *parsed > 0.0)
            .ok_or_else(|| serde::de::Error::custom("threshold fraction must be > 0")),
        _ => Ok(0.0), // 0.0 signals "use default" to callers
    }
}
