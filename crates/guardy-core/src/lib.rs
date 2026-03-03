use glob::Pattern;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use suite_foundation_core::error::CovyError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    #[serde(default = "default_policy_version")]
    pub version: u32,
    pub policy: PolicyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(alias = "tool_allowlist")]
    pub allowed_tools: Vec<String>,
    #[serde(alias = "reducer_allowlist")]
    pub allowed_reducers: Vec<String>,
    #[serde(alias = "path_rules")]
    pub paths: PathPolicy,
    #[serde(alias = "budget_rules")]
    pub budgets: BudgetPolicy,
    #[serde(alias = "redaction_rules")]
    pub redaction: RedactionPolicy,
    #[serde(alias = "human_review_flags")]
    pub human_review: HumanReviewPolicy,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuardPacket {
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub token_usage: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub payload: Value,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditFinding {
    pub rule: String,
    pub subject: String,
    pub message: String,
}

impl ContextConfig {
    pub fn load(path: &Path) -> Result<Self, CovyError> {
        let raw = read_file(path)?;
        parse_context_strict(&raw)
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.version != 1 {
            errors.push(format!(
                "unsupported policy version {} (expected 1)",
                self.version
            ));
        }

        for (idx, tool) in self.policy.allowed_tools.iter().enumerate() {
            if tool.trim().is_empty() {
                errors.push(format!("policy.allowed_tools[{idx}] cannot be empty"));
            }
        }

        for (idx, reducer) in self.policy.allowed_reducers.iter().enumerate() {
            if reducer.trim().is_empty() {
                errors.push(format!("policy.allowed_reducers[{idx}] cannot be empty"));
            }
        }

        if self.policy.budgets.token_cap == Some(0) {
            errors.push("policy.budgets.token_cap must be greater than 0".to_string());
        }

        if self.policy.budgets.runtime_ms_cap == Some(0) {
            errors.push("policy.budgets.runtime_ms_cap must be greater than 0".to_string());
        }

        for (idx, pattern) in self.policy.paths.include.iter().enumerate() {
            if let Err(err) = Pattern::new(pattern) {
                errors.push(format!("policy.paths.include[{idx}] invalid glob: {err}"));
            }
        }

        for (idx, pattern) in self.policy.paths.exclude.iter().enumerate() {
            if let Err(err) = Pattern::new(pattern) {
                errors.push(format!("policy.paths.exclude[{idx}] invalid glob: {err}"));
            }
        }

        for (idx, pattern) in self.policy.redaction.forbidden_patterns.iter().enumerate() {
            if let Err(err) = Regex::new(pattern) {
                errors.push(format!(
                    "policy.redaction.forbidden_patterns[{idx}] invalid regex: {err}"
                ));
            }
        }

        errors
    }
}

impl GuardPacket {
    pub fn load(path: &Path) -> Result<Self, CovyError> {
        let raw = read_file(path)?;
        serde_json::from_str(&raw).map_err(|source| CovyError::Parse {
            format: "packet-json".to_string(),
            detail: source.to_string(),
        })
    }

    fn collect_tools(&self) -> BTreeSet<String> {
        let mut tools = BTreeSet::new();
        if let Some(tool) = non_empty(self.tool.as_deref()) {
            tools.insert(tool.to_string());
        }
        for tool in &self.tools {
            if let Some(tool) = non_empty(Some(tool.as_str())) {
                tools.insert(tool.to_string());
            }
        }
        for invocation in &self.tool_invocations {
            if let Some(name) = non_empty(Some(invocation.name.as_str())) {
                tools.insert(name.to_string());
            }
        }
        tools
    }

    fn collect_reducers(&self) -> BTreeSet<String> {
        let mut reducers = BTreeSet::new();
        if let Some(reducer) = non_empty(self.reducer.as_deref()) {
            reducers.insert(reducer.to_string());
        }
        for reducer in &self.reducers {
            if let Some(reducer) = non_empty(Some(reducer.as_str())) {
                reducers.insert(reducer.to_string());
            }
        }
        for invocation in &self.tool_invocations {
            if let Some(reducer) = non_empty(invocation.reducer.as_deref()) {
                reducers.insert(reducer.to_string());
            }
        }
        for invocation in &self.reducer_invocations {
            if let Some(name) = non_empty(Some(invocation.name.as_str())) {
                reducers.insert(name.to_string());
            }
        }
        reducers
    }

    fn collect_paths(&self) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        for path in &self.paths {
            if let Some(path) = non_empty(Some(path.as_str())) {
                paths.insert(normalize_path(path));
            }
        }
        for invocation in &self.tool_invocations {
            for path in &invocation.paths {
                if let Some(path) = non_empty(Some(path.as_str())) {
                    paths.insert(normalize_path(path));
                }
            }
        }
        paths
    }

    fn total_token_usage(&self) -> u64 {
        let tool_tokens: u64 = self
            .tool_invocations
            .iter()
            .map(|call| call.token_usage.unwrap_or(0))
            .sum();
        let reducer_tokens: u64 = self
            .reducer_invocations
            .iter()
            .map(|call| call.token_usage.unwrap_or(0))
            .sum();
        self.token_usage.unwrap_or(0) + tool_tokens + reducer_tokens
    }

    fn total_runtime_ms(&self) -> u64 {
        let tool_runtime: u64 = self
            .tool_invocations
            .iter()
            .map(|call| call.runtime_ms.unwrap_or(0))
            .sum();
        let reducer_runtime: u64 = self
            .reducer_invocations
            .iter()
            .map(|call| call.runtime_ms.unwrap_or(0))
            .sum();
        self.runtime_ms.unwrap_or(0) + tool_runtime + reducer_runtime
    }

    fn collect_text_for_redaction_scan(&self) -> Vec<TextCandidate> {
        let mut out = Vec::new();

        collect_texts_from_value(&self.payload, "packet.payload", &mut out);
        for (idx, call) in self.tool_invocations.iter().enumerate() {
            collect_texts_from_value(
                &call.input,
                &format!("packet.tool_invocations[{idx}].input"),
                &mut out,
            );
            collect_texts_from_value(
                &call.output,
                &format!("packet.tool_invocations[{idx}].output"),
                &mut out,
            );
        }
        for (idx, call) in self.reducer_invocations.iter().enumerate() {
            collect_texts_from_value(
                &call.output,
                &format!("packet.reducer_invocations[{idx}].output"),
                &mut out,
            );
        }

        for (idx, value) in self.text_blobs.iter().enumerate() {
            if !value.is_empty() {
                out.push(TextCandidate {
                    source: format!("packet.text_blobs[{idx}]"),
                    value: value.clone(),
                });
            }
        }

        if let Some(value) = &self.quality_gate {
            collect_serialized_texts(value, "packet.quality_gate", &mut out);
        }
        if let Some(value) = &self.impact_result {
            collect_serialized_texts(value, "packet.impact_result", &mut out);
        }
        if let Some(value) = &self.shard_plan {
            collect_serialized_texts(value, "packet.shard_plan", &mut out);
        }
        if let Some(value) = &self.merge_summary {
            collect_serialized_texts(value, "packet.merge_summary", &mut out);
        }

        out
    }
}

pub fn validate_config_file(path: &Path) -> Result<ConfigValidationResult, CovyError> {
    let raw = read_file(path)?;
    Ok(validate_config_str(&raw))
}

pub fn validate_config_str(raw: &str) -> ConfigValidationResult {
    match serde_yaml::from_str::<ContextConfig>(raw) {
        Ok(config) => {
            let errors = config.validate();
            ConfigValidationResult {
                valid: errors.is_empty(),
                errors,
            }
        }
        Err(source) => ConfigValidationResult {
            valid: false,
            errors: vec![format!("schema parse error: {source}")],
        },
    }
}

pub fn check_packet_file(packet_path: &Path, config_path: &Path) -> Result<AuditResult, CovyError> {
    let config = ContextConfig::load(config_path)?;
    let packet = GuardPacket::load(packet_path)?;
    Ok(check_packet(&config, &packet))
}

pub fn check_packet(config: &ContextConfig, packet: &GuardPacket) -> AuditResult {
    let tools = packet.collect_tools();
    let reducers = packet.collect_reducers();
    let paths = packet.collect_paths();
    let total_token_usage = packet.total_token_usage();
    let total_runtime_ms = packet.total_runtime_ms();

    let mut findings = Vec::new();

    let allowed_tools: BTreeSet<_> = config
        .policy
        .allowed_tools
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect();

    if !allowed_tools.is_empty() {
        for tool in &tools {
            if !allowed_tools.contains(tool.as_str()) {
                findings.push(AuditFinding {
                    rule: "allowed_tools".to_string(),
                    subject: tool.clone(),
                    message: format!("tool '{tool}' is not allowed by policy"),
                });
            }
        }
    }

    let allowed_reducers: BTreeSet<_> = config
        .policy
        .allowed_reducers
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect();

    if !allowed_reducers.is_empty() {
        for reducer in &reducers {
            if !allowed_reducers.contains(reducer.as_str()) {
                findings.push(AuditFinding {
                    rule: "allowed_reducers".to_string(),
                    subject: reducer.clone(),
                    message: format!("reducer '{reducer}' is not allowed by policy"),
                });
            }
        }
    }

    let include_patterns =
        compile_globs(&config.policy.paths.include, "paths.include", &mut findings);
    let exclude_patterns =
        compile_globs(&config.policy.paths.exclude, "paths.exclude", &mut findings);

    for path in &paths {
        if !include_patterns.is_empty() && !matches_any(&include_patterns, path) {
            findings.push(AuditFinding {
                rule: "path_include".to_string(),
                subject: path.clone(),
                message: "path is outside policy.paths.include allowlist".to_string(),
            });
        }

        if matches_any(&exclude_patterns, path) {
            findings.push(AuditFinding {
                rule: "path_exclude".to_string(),
                subject: path.clone(),
                message: "path matched policy.paths.exclude denylist".to_string(),
            });
        }
    }

    if let Some(token_cap) = config.policy.budgets.token_cap {
        if total_token_usage > token_cap {
            findings.push(AuditFinding {
                rule: "token_cap".to_string(),
                subject: "packet".to_string(),
                message: format!(
                    "token usage {} exceeded cap {}",
                    total_token_usage, token_cap
                ),
            });
        }
    }

    if let Some(runtime_cap) = config.policy.budgets.runtime_ms_cap {
        if total_runtime_ms > runtime_cap {
            findings.push(AuditFinding {
                rule: "runtime_ms_cap".to_string(),
                subject: "packet".to_string(),
                message: format!(
                    "runtime {}ms exceeded cap {}ms",
                    total_runtime_ms, runtime_cap
                ),
            });
        }
    }

    let redaction_patterns = compile_regexes(
        &config.policy.redaction.forbidden_patterns,
        "redaction.forbidden_patterns",
        &mut findings,
    );
    let text_candidates = packet.collect_text_for_redaction_scan();
    for (pattern_source, regex) in redaction_patterns {
        for candidate in &text_candidates {
            if regex.is_match(&candidate.value) {
                findings.push(AuditFinding {
                    rule: "redaction".to_string(),
                    subject: candidate.source.clone(),
                    message: format!(
                        "forbidden pattern '{}' detected in packet content",
                        pattern_source
                    ),
                });
                break;
            }
        }
    }

    AuditResult {
        passed: findings.is_empty(),
        policy_version: config.version,
        checked_at_unix: now_unix(),
        totals: AuditTotals {
            tools_seen: tools.len(),
            reducers_seen: reducers.len(),
            paths_seen: paths.len(),
            total_token_usage,
            total_runtime_ms,
        },
        findings,
    }
}

fn parse_context_strict(raw: &str) -> Result<ContextConfig, CovyError> {
    let config: ContextConfig = serde_yaml::from_str(raw)
        .map_err(|source| CovyError::Config(format!("invalid context.yaml: {source}")))?;

    let validation_errors = config.validate();
    if !validation_errors.is_empty() {
        return Err(CovyError::Config(format!(
            "invalid context.yaml: {}",
            validation_errors.join("; ")
        )));
    }

    Ok(config)
}

fn default_policy_version() -> u32 {
    1
}

fn non_empty(input: Option<&str>) -> Option<&str> {
    let value = input?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn compile_globs(
    patterns: &[String],
    rule_name: &str,
    findings: &mut Vec<AuditFinding>,
) -> Vec<Pattern> {
    patterns
        .iter()
        .filter_map(|pattern| match Pattern::new(pattern) {
            Ok(pattern) => Some(pattern),
            Err(source) => {
                findings.push(AuditFinding {
                    rule: rule_name.to_string(),
                    subject: pattern.clone(),
                    message: format!("invalid glob pattern: {source}"),
                });
                None
            }
        })
        .collect()
}

fn compile_regexes(
    patterns: &[String],
    rule_name: &str,
    findings: &mut Vec<AuditFinding>,
) -> Vec<(String, Regex)> {
    patterns
        .iter()
        .filter_map(|pattern| match Regex::new(pattern) {
            Ok(regex) => Some((pattern.clone(), regex)),
            Err(source) => {
                findings.push(AuditFinding {
                    rule: rule_name.to_string(),
                    subject: pattern.clone(),
                    message: format!("invalid regex pattern: {source}"),
                });
                None
            }
        })
        .collect()
}

fn matches_any(patterns: &[Pattern], value: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(value))
}

fn read_file(path: &Path) -> Result<String, CovyError> {
    std::fs::read_to_string(path).map_err(|source| CovyError::Io {
        path: PathBuf::from(path),
        source,
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
struct TextCandidate {
    source: String,
    value: String,
}

fn collect_serialized_texts<T: Serialize>(value: &T, root: &str, out: &mut Vec<TextCandidate>) {
    if let Ok(serialized) = serde_json::to_value(value) {
        collect_texts_from_value(&serialized, root, out);
    }
}

fn collect_texts_from_value(value: &Value, path: &str, out: &mut Vec<TextCandidate>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                out.push(TextCandidate {
                    source: path.to_string(),
                    value: text.clone(),
                });
            }
        }
        Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                collect_texts_from_value(item, &format!("{path}[{idx}]"), out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                collect_texts_from_value(value, &format!("{path}.{key}"), out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn validate_config_rejects_invalid_schema_and_rules() {
        let yaml = r#"
version: 2
policy:
  allowed_tools: [""]
  paths:
    include: ["[broken"]
  budgets:
    token_cap: 0
    runtime_ms_cap: 0
  redaction:
    forbidden_patterns: ["("]
"#;

        let result = validate_config_str(yaml);
        assert!(!result.valid);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("unsupported policy version")));
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("allowed_tools[0] cannot be empty")));
        assert!(result.errors.iter().any(|e| e.contains("invalid glob")));
        assert!(result.errors.iter().any(|e| e.contains("token_cap")));
        assert!(result.errors.iter().any(|e| e.contains("runtime_ms_cap")));
        assert!(result.errors.iter().any(|e| e.contains("invalid regex")));
    }

    #[test]
    fn validate_config_accepts_canonical_policy_aliases() {
        let yaml = r#"
version: 1
policy:
  tool_allowlist: ["diffy"]
  reducer_allowlist: ["analyze"]
  path_rules:
    include: ["src/**"]
    exclude: []
  budget_rules:
    token_cap: 300
    runtime_ms_cap: 2000
  redaction_rules:
    forbidden_patterns: ["(?i)secret"]
  human_review:
    required: true
    on_policy_violation: true
    on_budget_violation: false
    on_redaction_violation: true
"#;

        let result = validate_config_str(yaml);
        assert!(result.valid);

        let config = parse_context_strict(yaml).unwrap();
        assert_eq!(config.policy.allowed_tools, vec!["diffy".to_string()]);
        assert_eq!(config.policy.allowed_reducers, vec!["analyze".to_string()]);
        assert!(config.policy.human_review.required);
        assert!(config.policy.human_review.on_policy_violation);
        assert!(!config.policy.human_review.on_budget_violation);
        assert!(config.policy.human_review.on_redaction_violation);
    }

    #[test]
    fn check_packet_reports_policy_violations() {
        let yaml = r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 100
    runtime_ms_cap: 500
  redaction:
    forbidden_patterns: ["(?i)password"]
"#;

        let config = parse_context_strict(yaml).unwrap();
        let packet: GuardPacket = serde_json::from_str(
            r#"{
  "tool": "unknown-tool",
  "reducer": "bad-reducer",
  "paths": ["src/private/secret.txt"],
  "token_usage": 130,
  "runtime_ms": 800,
  "payload": {"note": "password=123"}
}"#,
        )
        .unwrap();

        let result = check_packet(&config, &packet);
        assert!(!result.passed);
        assert!(result.findings.iter().any(|f| f.rule == "allowed_tools"));
        assert!(result.findings.iter().any(|f| f.rule == "allowed_reducers"));
        assert!(result.findings.iter().any(|f| f.rule == "path_exclude"));
        assert!(result.findings.iter().any(|f| f.rule == "token_cap"));
        assert!(result.findings.iter().any(|f| f.rule == "runtime_ms_cap"));
        assert!(result.findings.iter().any(|f| f.rule == "redaction"));
    }

    #[test]
    fn check_packet_passes_for_compliant_input() {
        let yaml = r#"
version: 1
policy:
  allowed_tools: ["covy", "diffy"]
  allowed_reducers: ["merge"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  budgets:
    token_cap: 200
    runtime_ms_cap: 1000
  redaction:
    forbidden_patterns: ["(?i)secret"]
"#;

        let config = parse_context_strict(yaml).unwrap();
        let packet: GuardPacket = serde_json::from_str(
            r#"{
  "tool": "covy",
  "reducer": "merge",
  "paths": ["src/lib.rs"],
  "token_usage": 120,
  "runtime_ms": 300,
  "payload": {"note": "all clear"}
}"#,
        )
        .unwrap();

        let result = check_packet(&config, &packet);
        assert!(result.passed);
        assert!(result.findings.is_empty());
        assert_eq!(result.totals.tools_seen, 1);
        assert_eq!(result.totals.reducers_seen, 1);
        assert_eq!(result.totals.paths_seen, 1);
    }

    #[test]
    fn file_based_validate_and_check_roundtrip() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("context.yaml");
        let packet_path = dir.path().join("packet.json");

        fs::write(
            &config_path,
            r#"
version: 1
policy:
  allowed_tools: ["covy"]
  allowed_reducers: []
  paths:
    include: ["src/**"]
    exclude: []
  budgets:
    token_cap: 100
    runtime_ms_cap: 400
  redaction:
    forbidden_patterns: []
"#,
        )
        .unwrap();

        fs::write(
            &packet_path,
            r#"{
  "tool": "covy",
  "paths": ["src/main.rs"],
  "token_usage": 10,
  "runtime_ms": 20,
  "payload": {"ok": "yes"}
}"#,
        )
        .unwrap();

        let validate = validate_config_file(&config_path).unwrap();
        assert!(validate.valid);

        let audit = check_packet_file(&packet_path, &config_path).unwrap();
        assert!(audit.passed);
    }
}
