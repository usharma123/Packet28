use glob::Pattern;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

use suite_foundation_core::error::CovyError;

use crate::validate::{non_empty, read_file};
use crate::{
    AuditFinding, AuditResult, AuditTotals, ContextConfig, GuardPacket, PacketFileRef,
    PacketSymbolRef, ReducerInvocation, ToolInvocation,
};

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
        for PacketFileRef { path, .. } in &self.files {
            if let Some(path) = non_empty(Some(path.as_str())) {
                paths.insert(normalize_path(path));
            }
        }
        for PacketSymbolRef { file, .. } in &self.symbols {
            if let Some(file) = file.as_deref().and_then(|v| non_empty(Some(v))) {
                paths.insert(normalize_path(file));
            }
        }
        for ToolInvocation {
            paths: tool_paths, ..
        } in &self.tool_invocations
        {
            for path in tool_paths {
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

    fn total_tool_calls(&self) -> u64 {
        let direct = self.tool_call_count.unwrap_or(0);
        let inferred = self.tool_invocations.len() as u64;
        direct.max(inferred)
    }

    fn collect_text_for_redaction_scan(&self) -> Vec<TextCandidate> {
        let mut out = Vec::new();

        collect_texts_from_value(&self.payload, "packet.payload", &mut out);
        for (idx, path) in self.paths.iter().enumerate() {
            push_text_candidate(&mut out, format!("packet.paths[{idx}]"), path);
        }
        for (idx, file) in self.files.iter().enumerate() {
            push_text_candidate(&mut out, format!("packet.files[{idx}].path"), &file.path);
            if let Some(source) = file.source.as_deref() {
                push_text_candidate(&mut out, format!("packet.files[{idx}].source"), source);
            }
        }
        for (idx, symbol) in self.symbols.iter().enumerate() {
            push_text_candidate(
                &mut out,
                format!("packet.symbols[{idx}].name"),
                &symbol.name,
            );
            if let Some(file) = symbol.file.as_deref() {
                push_text_candidate(&mut out, format!("packet.symbols[{idx}].file"), file);
            }
            if let Some(source) = symbol.source.as_deref() {
                push_text_candidate(&mut out, format!("packet.symbols[{idx}].source"), source);
            }
        }
        for (idx, call) in self.tool_invocations.iter().enumerate() {
            push_text_candidate(
                &mut out,
                format!("packet.tool_invocations[{idx}].name"),
                &call.name,
            );
            if let Some(reducer) = call.reducer.as_deref() {
                push_text_candidate(
                    &mut out,
                    format!("packet.tool_invocations[{idx}].reducer"),
                    reducer,
                );
            }
            for (path_idx, path) in call.paths.iter().enumerate() {
                push_text_candidate(
                    &mut out,
                    format!("packet.tool_invocations[{idx}].paths[{path_idx}]"),
                    path,
                );
            }
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
        for (idx, ReducerInvocation { name, output, .. }) in
            self.reducer_invocations.iter().enumerate()
        {
            push_text_candidate(
                &mut out,
                format!("packet.reducer_invocations[{idx}].name"),
                name,
            );
            collect_texts_from_value(
                output,
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
        if let Some(summary) = self.summary.as_deref() {
            push_text_candidate(&mut out, "packet.summary".to_string(), summary);
        }
        if let Some(provenance) = &self.provenance {
            for (idx, input) in provenance.inputs.iter().enumerate() {
                push_text_candidate(&mut out, format!("packet.provenance.inputs[{idx}]"), input);
            }
            if let Some(git_base) = provenance.git_base.as_deref() {
                push_text_candidate(&mut out, "packet.provenance.git_base".to_string(), git_base);
            }
            if let Some(git_head) = provenance.git_head.as_deref() {
                push_text_candidate(&mut out, "packet.provenance.git_head".to_string(), git_head);
            }
        }

        out
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
    let total_tool_calls = packet.total_tool_calls();

    let mut findings = Vec::new();

    let allowed_tools: BTreeSet<_> = config
        .policy
        .effective_allowed_tools()
        .into_iter()
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
        .effective_allowed_reducers()
        .into_iter()
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

    if let Some(token_cap) = config.policy.effective_token_cap() {
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

    if let Some(runtime_cap) = config.policy.effective_runtime_ms_cap() {
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

    if let Some(tool_call_cap) = config.policy.effective_tool_call_cap() {
        if total_tool_calls > tool_call_cap {
            findings.push(AuditFinding {
                rule: "tool_call_cap".to_string(),
                subject: "packet".to_string(),
                message: format!(
                    "tool call count {} exceeded cap {}",
                    total_tool_calls, tool_call_cap
                ),
            });
        }
    }

    let human_review_paths = compile_globs(
        &config.policy.human_review.paths,
        "human_review.paths",
        &mut findings,
    );
    if !human_review_paths.is_empty() {
        for path in &paths {
            if matches_any(&human_review_paths, path) {
                findings.push(AuditFinding {
                    rule: "human_review_required".to_string(),
                    subject: path.clone(),
                    message: "path matched policy.human_review.paths and requires human review"
                        .to_string(),
                });
            }
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
            total_tool_calls,
        },
        findings,
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

fn push_text_candidate(out: &mut Vec<TextCandidate>, source: String, value: &str) {
    if !value.is_empty() {
        out.push(TextCandidate {
            source,
            value: value.to_string(),
        });
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
