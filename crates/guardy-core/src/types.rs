use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    #[serde(default = "crate::validate::default_policy_version")]
    pub version: u32,
    pub policy: PolicyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub tools: AllowlistPolicy,
    pub reducers: AllowlistPolicy,
    #[serde(alias = "tool_allowlist")]
    pub allowed_tools: Vec<String>,
    #[serde(alias = "reducer_allowlist")]
    pub allowed_reducers: Vec<String>,
    #[serde(alias = "path_rules")]
    pub paths: PathPolicy,
    pub token_budget: TokenBudgetPolicy,
    pub runtime_budget: RuntimeBudgetPolicy,
    pub tool_call_budget: ToolCallBudgetPolicy,
    #[serde(alias = "budget_rules")]
    pub budgets: BudgetPolicy,
    #[serde(alias = "redaction_rules")]
    pub redaction: RedactionPolicy,
    #[serde(alias = "human_review_flags")]
    pub human_review: HumanReviewPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AllowlistPolicy {
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PathPolicy {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetPolicy {
    pub token_cap: Option<u64>,
    pub runtime_ms_cap: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TokenBudgetPolicy {
    #[serde(alias = "token_cap")]
    pub cap: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeBudgetPolicy {
    #[serde(alias = "runtime_ms_cap")]
    pub cap_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ToolCallBudgetPolicy {
    pub cap: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RedactionPolicy {
    pub forbidden_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HumanReviewPolicy {
    pub required: bool,
    pub on_policy_violation: bool,
    pub on_budget_violation: bool,
    pub on_redaction_violation: bool,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuardPacket {
    pub packet_id: Option<String>,
    pub summary: Option<String>,
    pub kind: Option<String>,
    pub version: Option<String>,
    pub hash: Option<String>,
    pub provenance: Option<GuardProvenance>,
    pub risk: Option<String>,
    pub confidence: Option<f64>,
    pub budget_cost: Option<Value>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub tool_call_count: Option<u64>,
    pub payload: Value,
    pub files: Vec<PacketFileRef>,
    pub symbols: Vec<PacketSymbolRef>,
    pub tool_invocations: Vec<ToolInvocation>,
    pub reducer_invocations: Vec<ReducerInvocation>,
    pub text_blobs: Vec<String>,
    #[serde(default)]
    pub quality_gate: Option<suite_packet_core::QualityGateResult>,
    #[serde(default)]
    pub impact_result: Option<suite_packet_core::ImpactResult>,
    #[serde(default)]
    pub shard_plan: Option<suite_packet_core::ShardPlan>,
    #[serde(default)]
    pub merge_summary: Option<suite_packet_core::MergeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuardProvenance {
    pub inputs: Vec<String>,
    pub git_base: Option<String>,
    pub git_head: Option<String>,
    pub generated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketFileRef {
    pub path: String,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketSymbolRef {
    pub name: String,
    pub file: Option<String>,
    pub kind: Option<String>,
    pub relevance: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolInvocation {
    pub name: String,
    pub reducer: Option<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub input: Value,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReducerInvocation {
    pub name: String,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditResult {
    pub passed: bool,
    pub policy_version: u32,
    pub checked_at_unix: u64,
    pub totals: AuditTotals,
    pub findings: Vec<AuditFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditTotals {
    pub tools_seen: usize,
    pub reducers_seen: usize,
    pub paths_seen: usize,
    pub total_token_usage: u64,
    pub total_runtime_ms: u64,
    pub total_tool_calls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditFinding {
    pub rule: String,
    pub subject: String,
    pub message: String,
}
