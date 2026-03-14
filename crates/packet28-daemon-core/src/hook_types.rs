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
    #[default]
    ClearOrNewSession,
    NewSessionOnly,
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
    pub command: Option<String>,
    pub search_query: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HookIngestResponse {
    pub task_id: String,
    pub accepted: bool,
    pub handoff_ready: bool,
    pub handoff_reason: Option<String>,
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_context_version: Option<String>,
    pub additional_context: Option<String>,
    pub block_stop: bool,
    pub stop_reason: Option<String>,
    pub cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HookRuntimeConfig {
    pub hooks_enabled: bool,
    pub rewrite_enabled: bool,
    pub fallback_post_tool_capture: bool,
    pub context_budget_tokens: u64,
    pub soft_threshold_fraction: String,
    pub relaunch_preference: RelaunchPreference,
    pub reducer_allowlist: Vec<String>,
}

impl HookRuntimeConfig {
    pub fn soft_threshold_fraction_value(&self) -> f64 {
        self.soft_threshold_fraction
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0)
            .unwrap_or(0.5)
    }

    pub fn soft_threshold_tokens(&self) -> u64 {
        let budget = self.context_budget_tokens.max(1);
        ((budget as f64) * self.soft_threshold_fraction_value()).round() as u64
    }
}

impl Default for HookRuntimeConfig {
    fn default() -> Self {
        Self {
            hooks_enabled: true,
            rewrite_enabled: true,
            fallback_post_tool_capture: true,
            context_budget_tokens: 10_000,
            soft_threshold_fraction: "0.5".to_string(),
            relaunch_preference: RelaunchPreference::ClearOrNewSession,
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
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub artifact_id: Option<String>,
    pub raw_artifact_handle: Option<String>,
    pub occurred_at_unix: u64,
    pub git_epoch: u64,
    pub fs_epoch: u64,
    pub rust_epoch: u64,
}
