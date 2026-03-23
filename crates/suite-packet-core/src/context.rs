use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceTier {
    CuratedMemory,
    Telemetry,
    #[default]
    Standard,
}

impl MemorySourceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            MemorySourceTier::CuratedMemory => "curated_memory",
            MemorySourceTier::Telemetry => "telemetry",
            MemorySourceTier::Standard => "standard",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Brief,
    Handoff,
    RecommendedAction,
    Evidence,
    ToolTrace,
    FocusInference,
    StateWrite,
    #[default]
    Other,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Brief => "brief",
            MemoryKind::Handoff => "handoff",
            MemoryKind::RecommendedAction => "recommended_action",
            MemoryKind::Evidence => "evidence",
            MemoryKind::ToolTrace => "tool_trace",
            MemoryKind::FocusInference => "focus_inference",
            MemoryKind::StateWrite => "state_write",
            MemoryKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct CorrelationEvidenceRef {
    pub packet_id: Option<String>,
    pub packet_type: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextCorrelationFinding {
    pub rule: String,
    pub relation: String,
    pub confidence: f64,
    pub summary: String,
    pub evidence_refs: Vec<CorrelationEvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextCorrelationPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub finding_count: usize,
    pub findings: Vec<ContextCorrelationFinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextManagePacketRef {
    pub cache_key: String,
    pub target: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_tier: Option<MemorySourceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_kind: Option<MemoryKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packet_types: Vec<String>,
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ContextManageBudgetSummary {
    pub requested_tokens: u64,
    pub requested_bytes: usize,
    pub working_set_tokens: u64,
    pub working_set_bytes: usize,
    pub evictable_tokens: u64,
    pub evictable_bytes: usize,
    pub reserved_headroom_tokens: u64,
    pub reserved_headroom_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextManageRecommendedAction {
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ContextManagePayload {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub budget: ContextManageBudgetSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_set: Vec<ContextManagePacketRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eviction_candidates: Vec<ContextManagePacketRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended_packets: Vec<ContextManagePacketRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended_actions: Vec<ContextManageRecommendedAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths_since_checkpoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_symbols_since_checkpoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<crate::AgentQuestion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_decisions: Vec<crate::AgentDecision>,
}
