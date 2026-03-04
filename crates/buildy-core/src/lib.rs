use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use suite_packet_core::{BudgetCost, EnvelopeV1, FileRef, Provenance, SymbolRef};

pub const BUILDY_SCHEMA_VERSION: &str = "buildy.reduce.v1";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildReduceRequest {
    pub log_text: String,
    pub source: Option<String>,
    pub max_diagnostics: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BuildDiagnostic {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String,
    pub code: Option<String>,
    pub message: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RootCauseGroup {
    pub root_cause: String,
    pub severity: String,
    pub count: usize,
    pub diagnostics: Vec<BuildDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildReduceOutput {
    pub schema_version: String,
    pub source: Option<String>,
    pub total_diagnostics: usize,
    pub unique_diagnostics: usize,
    pub duplicates_removed: usize,
    pub groups: Vec<RootCauseGroup>,
    pub ordered_fixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildPacket {
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub payload: serde_json::Value,
    pub sections: Vec<serde_json::Value>,
    pub refs: Vec<serde_json::Value>,
    pub text_blobs: Vec<String>,
}

pub fn reduce(request: BuildReduceRequest) -> BuildReduceOutput {
    let mut parsed = parse_diagnostics(&request.log_text);
    if let Some(max) = request.max_diagnostics {
        parsed.truncate(max);
    }

    let total_diagnostics = parsed.len();
    let mut deduped = Vec::new();
    let mut seen = BTreeSet::new();
    for diagnostic in parsed {
        let key = diagnostic.fingerprint.clone();
        if seen.insert(key) {
            deduped.push(diagnostic);
        }
    }

    deduped.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.message.cmp(&b.message))
    });

    let unique_diagnostics = deduped.len();
    let duplicates_removed = total_diagnostics.saturating_sub(unique_diagnostics);

    let mut grouped: HashMap<String, RootCauseGroup> = HashMap::new();
    for diagnostic in deduped {
        let root_cause = diagnostic
            .code
            .as_ref()
            .map(|code| format!("{}:{}", diagnostic.severity, code))
            .unwrap_or_else(|| {
                format!(
                    "{}:{}",
                    diagnostic.severity,
                    normalize_message_for_group(&diagnostic.message)
                )
            });

        grouped
            .entry(root_cause.clone())
            .and_modify(|group| {
                group.count = group.count.saturating_add(1);
                group.diagnostics.push(diagnostic.clone());
            })
            .or_insert_with(|| RootCauseGroup {
                root_cause,
                severity: diagnostic.severity.clone(),
                count: 1,
                diagnostics: vec![diagnostic],
            });
    }

    let mut groups: Vec<_> = grouped.into_values().collect();
    groups.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.root_cause.cmp(&b.root_cause))
    });

    for group in &mut groups {
        group.diagnostics.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
                .then_with(|| a.message.cmp(&b.message))
        });
    }

    let ordered_fixes = groups
        .iter()
        .map(|group| {
            let first = group
                .diagnostics
                .first()
                .map(|diag| format!("{}:{}:{}", diag.file, diag.line, diag.column))
                .unwrap_or_else(|| "unknown:0:0".to_string());
            format!(
                "{} ({}, count={}) first_at={}",
                group.root_cause, group.severity, group.count, first
            )
        })
        .collect::<Vec<_>>();

    BuildReduceOutput {
        schema_version: BUILDY_SCHEMA_VERSION.to_string(),
        source: request.source,
        total_diagnostics,
        unique_diagnostics,
        duplicates_removed,
        groups,
        ordered_fixes,
    }
}

pub fn reduce_to_envelope(request: BuildReduceRequest) -> EnvelopeV1<BuildReduceOutput> {
    let started = Instant::now();
    let source = request
        .source
        .clone()
        .unwrap_or_else(|| "stdin".to_string());
    let output = reduce(request);

    let mut file_counts = HashMap::<String, usize>::new();
    let mut symbol_counts = HashMap::<String, usize>::new();
    for group in &output.groups {
        for diagnostic in &group.diagnostics {
            *file_counts
                .entry(normalize_path(&diagnostic.file))
                .or_insert(0) += 1;
            if let Some(code) = diagnostic.code.as_deref() {
                *symbol_counts.entry(code.trim().to_string()).or_insert(0) += 1;
            }
        }
    }

    let max_file = file_counts.values().copied().max().unwrap_or(1) as f64;
    let max_symbol = symbol_counts.values().copied().max().unwrap_or(1) as f64;
    let mut files = file_counts
        .into_iter()
        .map(|(path, count)| FileRef {
            path,
            relevance: Some((count as f64 / max_file).clamp(0.0, 1.0)),
            source: Some("buildy.reduce".to_string()),
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| {
        b.relevance
            .unwrap_or(0.0)
            .total_cmp(&a.relevance.unwrap_or(0.0))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut symbols = symbol_counts
        .into_iter()
        .map(|(name, count)| SymbolRef {
            name,
            file: None,
            kind: Some("diagnostic_code".to_string()),
            relevance: Some((count as f64 / max_symbol).clamp(0.0, 1.0)),
            source: Some("buildy.reduce".to_string()),
        })
        .collect::<Vec<_>>();
    symbols.sort_by(|a, b| {
        b.relevance
            .unwrap_or(0.0)
            .total_cmp(&a.relevance.unwrap_or(0.0))
            .then_with(|| a.name.cmp(&b.name))
    });

    let payload_bytes = serde_json::to_vec(&output).unwrap_or_default().len();
    EnvelopeV1 {
        version: "1".to_string(),
        tool: "buildy".to_string(),
        kind: "build_reduce".to_string(),
        hash: String::new(),
        summary: format!(
            "build diagnostics total={} unique={} duplicates_removed={}",
            output.total_diagnostics, output.unique_diagnostics, output.duplicates_removed
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(1.0),
        budget_cost: BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: started.elapsed().as_millis() as u64,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: Provenance {
            inputs: vec![source],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload: output,
    }
    .with_canonical_hash_and_real_budget()
}

pub fn reduce_to_packet(request: BuildReduceRequest) -> BuildPacket {
    let output = reduce(request);

    let mut paths = BTreeSet::new();
    let mut refs = Vec::new();

    for group in &output.groups {
        for diagnostic in &group.diagnostics {
            paths.insert(normalize_path(&diagnostic.file));
            refs.push(json!({
                "kind": "file",
                "value": normalize_path(&diagnostic.file),
                "source": "buildy-reduce-v1",
                "relevance": if diagnostic.severity == "error" { 1.0 } else { 0.7 },
            }));
            if let Some(code) = &diagnostic.code {
                refs.push(json!({
                    "kind": "symbol",
                    "value": code,
                    "source": "buildy-reduce-v1",
                    "relevance": 0.8,
                }));
            }
        }
    }

    refs.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    refs.dedup_by(|a, b| a == b);

    let summary = format!(
        "total_diagnostics: {}\nunique_diagnostics: {}\nduplicates_removed: {}",
        output.total_diagnostics, output.unique_diagnostics, output.duplicates_removed
    );

    let sections = output
        .groups
        .iter()
        .map(|group| {
            json!({
                "id": short_hash(&group.root_cause),
                "title": group.root_cause,
                "body": format!("severity: {}\ncount: {}", group.severity, group.count),
                "refs": refs,
                "relevance": if group.severity == "error" { 1.0 } else { 0.7 },
            })
        })
        .collect::<Vec<_>>();

    BuildPacket {
        packet_id: Some("buildy-reduce-v1".to_string()),
        tool: Some("buildy".to_string()),
        tools: vec!["buildy".to_string()],
        reducer: Some("reduce".to_string()),
        reducers: vec!["reduce".to_string()],
        paths: paths.into_iter().collect(),
        payload: serde_json::to_value(&output).unwrap_or_default(),
        sections,
        refs,
        text_blobs: vec![summary],
    }
}

fn parse_diagnostics(log_text: &str) -> Vec<BuildDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut pending_rust: Option<(String, Option<String>, String)> = None;

    for line in log_text.lines() {
        if let Some(captures) = colon_diag_re().captures(line) {
            let file = captures
                .name("file")
                .map(|m| normalize_path(m.as_str()))
                .unwrap_or_default();
            let line_num = captures
                .name("line")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let col_num = captures
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("warning"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();

            diagnostics.push(build_diagnostic(
                file, line_num, col_num, severity, code, message,
            ));
            pending_rust = None;
            continue;
        }

        if let Some(captures) = msvc_diag_re().captures(line) {
            let file = captures
                .name("file")
                .map(|m| normalize_path(m.as_str()))
                .unwrap_or_default();
            let line_num = captures
                .name("line")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let col_num = captures
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("warning"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();

            diagnostics.push(build_diagnostic(
                file, line_num, col_num, severity, code, message,
            ));
            pending_rust = None;
            continue;
        }

        if let Some(captures) = rust_header_re().captures(line) {
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("error"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            pending_rust = Some((severity, code, message));
            continue;
        }

        if let Some((severity, code, message)) = pending_rust.clone() {
            if let Some(captures) = rust_location_re().captures(line) {
                let file = captures
                    .name("file")
                    .map(|m| normalize_path(m.as_str()))
                    .unwrap_or_default();
                let line_num = captures
                    .name("line")
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .unwrap_or(0);
                let col_num = captures
                    .name("col")
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .unwrap_or(0);
                diagnostics.push(build_diagnostic(
                    file, line_num, col_num, severity, code, message,
                ));
                pending_rust = None;
            }
        }
    }

    diagnostics
}

fn build_diagnostic(
    file: String,
    line: u32,
    column: u32,
    severity: String,
    code: Option<String>,
    message: String,
) -> BuildDiagnostic {
    let fingerprint_material = format!(
        "{}|{}|{}|{}|{}|{}",
        file,
        line,
        column,
        severity,
        code.clone().unwrap_or_default(),
        normalize_message_for_group(&message)
    );

    BuildDiagnostic {
        file,
        line,
        column,
        severity,
        code,
        message,
        fingerprint: short_hash(&fingerprint_material),
    }
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn normalize_severity(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "error" | "err" => "error".to_string(),
        "warning" | "warn" => "warning".to_string(),
        "note" => "note".to_string(),
        "info" | "information" => "info".to_string(),
        _ => "warning".to_string(),
    }
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 4,
        "warning" => 3,
        "note" => 2,
        "info" => 1,
        _ => 0,
    }
}

fn normalize_message_for_group(message: &str) -> String {
    numeric_re()
        .replace_all(&message.to_ascii_lowercase(), "#")
        .trim()
        .to_string()
}

fn short_hash(input: &str) -> String {
    blake3::hash(input.as_bytes())
        .to_hex()
        .to_string()
        .chars()
        .take(16)
        .collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn colon_diag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+):(?P<line>\d+):(?P<col>\d+):\s*(?P<severity>error|warning|note|info):\s*(?P<message>.*?)(?:\s+\[(?P<code>[^\]]+)\])?$",
        )
        .unwrap()
    })
}

fn msvc_diag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<severity>error|warning|note|info)\s*(?P<code>[A-Za-z0-9_]+)?:?\s*(?P<message>.*)$",
        )
        .unwrap()
    })
}

fn rust_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<severity>error|warning|note)(?:\[(?P<code>[^\]]+)\])?:\s*(?P<message>.*)$",
        )
        .unwrap()
    })
}

fn rust_location_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*-->\s+(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+):(?P<line>\d+):(?P<col>\d+)",
        )
        .unwrap()
    })
}

fn numeric_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_groups_diagnostics() {
        let input = r#"
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
src/lib.rs:10:5: error: cannot find value `x` in this scope [E0425]
main.c(40,2): warning C4996: use of deprecated function
error[E0308]: mismatched types
  --> src/main.rs:22:13
"#;

        let output = reduce(BuildReduceRequest {
            log_text: input.to_string(),
            source: Some("fixture".to_string()),
            max_diagnostics: None,
        });

        assert_eq!(output.total_diagnostics, 4);
        assert_eq!(output.unique_diagnostics, 3);
        assert_eq!(output.duplicates_removed, 1);
        assert!(!output.groups.is_empty());
        assert!(output
            .ordered_fixes
            .first()
            .is_some_and(|entry| entry.contains("error")));
    }

    #[test]
    fn packet_output_is_deterministic() {
        let input = "src/lib.rs:1:1: warning: unused import [W100]";

        let packet_a = reduce_to_packet(BuildReduceRequest {
            log_text: input.to_string(),
            source: Some("a".to_string()),
            max_diagnostics: None,
        });
        let packet_b = reduce_to_packet(BuildReduceRequest {
            log_text: input.to_string(),
            source: Some("a".to_string()),
            max_diagnostics: None,
        });

        assert_eq!(
            serde_json::to_string(&packet_a).unwrap(),
            serde_json::to_string(&packet_b).unwrap()
        );
        assert_eq!(packet_a.packet_id.as_deref(), Some("buildy-reduce-v1"));
    }
}
