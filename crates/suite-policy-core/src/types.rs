use serde::{Deserialize, Serialize};

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
pub struct ConfigValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
}
