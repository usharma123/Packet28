use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use serde_json::json;
use suite_packet_core::{BudgetCost, EnvelopeV1, FileRef, Provenance, SymbolRef};

mod parse;
#[cfg(test)]
mod tests;
mod types;

use parse::{
    normalize_message_for_group, normalize_path, now_unix, parse_diagnostics, severity_rank,
    short_hash,
};
pub use types::*;

pub const BUILDY_SCHEMA_VERSION: &str = "buildy.reduce.v1";

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
