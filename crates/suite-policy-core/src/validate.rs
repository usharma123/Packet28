use glob::Pattern;
use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use suite_packet_core::CovyError;

use crate::types::{ConfigValidationResult, ContextConfig, PolicyConfig};

impl ContextConfig {
    pub fn load(path: &Path) -> Result<Self, CovyError> {
        let raw = std::fs::read_to_string(path).map_err(|source| CovyError::Io {
            path: PathBuf::from(path),
            source,
        })?;
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

        if let Some(message) = validate_allowlist_conflict(
            &self.policy.tools.allowlist,
            &self.policy.allowed_tools,
            "policy.tools.allowlist",
            "policy.allowed_tools",
        ) {
            errors.push(message);
        }

        if let Some(message) = validate_allowlist_conflict(
            &self.policy.reducers.allowlist,
            &self.policy.allowed_reducers,
            "policy.reducers.allowlist",
            "policy.allowed_reducers",
        ) {
            errors.push(message);
        }

        if let Some(message) = validate_cap_conflict(
            self.policy.token_budget.cap,
            self.policy.budgets.token_cap,
            "policy.token_budget.cap",
            "policy.budgets.token_cap",
        ) {
            errors.push(message);
        }

        if let Some(message) = validate_cap_conflict(
            self.policy.runtime_budget.cap_ms,
            self.policy.budgets.runtime_ms_cap,
            "policy.runtime_budget.cap_ms",
            "policy.budgets.runtime_ms_cap",
        ) {
            errors.push(message);
        }

        for (idx, tool) in self.policy.tools.allowlist.iter().enumerate() {
            if tool.trim().is_empty() {
                errors.push(format!("policy.tools.allowlist[{idx}] cannot be empty"));
            }
        }
        for (idx, reducer) in self.policy.reducers.allowlist.iter().enumerate() {
            if reducer.trim().is_empty() {
                errors.push(format!("policy.reducers.allowlist[{idx}] cannot be empty"));
            }
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

        if self.policy.token_budget.cap == Some(0) {
            errors.push("policy.token_budget.cap must be greater than 0".to_string());
        }
        if self.policy.budgets.token_cap == Some(0) {
            errors.push("policy.budgets.token_cap must be greater than 0".to_string());
        }
        if self.policy.runtime_budget.cap_ms == Some(0) {
            errors.push("policy.runtime_budget.cap_ms must be greater than 0".to_string());
        }
        if self.policy.budgets.runtime_ms_cap == Some(0) {
            errors.push("policy.budgets.runtime_ms_cap must be greater than 0".to_string());
        }
        if self.policy.tool_call_budget.cap == Some(0) {
            errors.push("policy.tool_call_budget.cap must be greater than 0".to_string());
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

        for (idx, pattern) in self.policy.human_review.paths.iter().enumerate() {
            if let Err(err) = Pattern::new(pattern) {
                errors.push(format!(
                    "policy.human_review.paths[{idx}] invalid glob: {err}"
                ));
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

impl PolicyConfig {
    pub fn effective_allowed_tools(&self) -> Vec<String> {
        let canonical = normalize_non_empty_list(&self.tools.allowlist);
        if !canonical.is_empty() {
            return canonical;
        }
        normalize_non_empty_list(&self.allowed_tools)
    }

    pub fn effective_allowed_reducers(&self) -> Vec<String> {
        let canonical = normalize_non_empty_list(&self.reducers.allowlist);
        if !canonical.is_empty() {
            return canonical;
        }
        normalize_non_empty_list(&self.allowed_reducers)
    }

    pub fn effective_token_cap(&self) -> Option<u64> {
        self.token_budget.cap.or(self.budgets.token_cap)
    }

    pub fn effective_runtime_ms_cap(&self) -> Option<u64> {
        self.runtime_budget.cap_ms.or(self.budgets.runtime_ms_cap)
    }

    pub fn effective_tool_call_cap(&self) -> Option<u64> {
        self.tool_call_budget.cap
    }
}

pub fn validate_config_file(path: &Path) -> Result<ConfigValidationResult, CovyError> {
    let raw = std::fs::read_to_string(path).map_err(|source| CovyError::Io {
        path: PathBuf::from(path),
        source,
    })?;
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

pub fn parse_context_strict(raw: &str) -> Result<ContextConfig, CovyError> {
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

pub fn reducer_candidates(target: &str) -> Vec<String> {
    let mut candidates = vec![target.to_string()];
    if let Some(leaf) = target.rsplit('.').next() {
        if !leaf.is_empty() && leaf != target {
            candidates.push(leaf.to_string());
        }
    }

    if target == "governed.assemble" {
        candidates.push("contextq.assemble".to_string());
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn match_reducer_allowlist(target: &str, allowlist: &[String]) -> Option<String> {
    let allowset = allowlist
        .iter()
        .filter_map(|v| {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect::<BTreeSet<_>>();

    let candidates = reducer_candidates(target);
    candidates
        .into_iter()
        .find(|candidate| allowset.contains(candidate))
}

fn validate_allowlist_conflict(
    canonical: &[String],
    legacy: &[String],
    canonical_label: &str,
    legacy_label: &str,
) -> Option<String> {
    let canonical = normalize_non_empty_list(canonical)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let legacy = normalize_non_empty_list(legacy)
        .into_iter()
        .collect::<BTreeSet<_>>();

    if canonical.is_empty() || legacy.is_empty() || canonical == legacy {
        return None;
    }

    Some(format!(
        "{canonical_label} conflicts with {legacy_label}; set only one or keep both identical"
    ))
}

fn validate_cap_conflict(
    canonical: Option<u64>,
    legacy: Option<u64>,
    canonical_label: &str,
    legacy_label: &str,
) -> Option<String> {
    match (canonical, legacy) {
        (Some(canonical), Some(legacy)) if canonical != legacy => Some(format!(
            "{canonical_label} conflicts with {legacy_label}; set only one or keep both identical"
        )),
        _ => None,
    }
}

pub(crate) fn default_policy_version() -> u32 {
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

fn normalize_non_empty_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| non_empty(Some(item.as_str())).map(ToOwned::to_owned))
        .collect()
}
