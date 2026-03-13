use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use serde_json::json;
use suite_packet_core::{BudgetCost, EnvelopeV1, FileRef, Provenance, SymbolRef};

mod parse;
#[cfg(test)]
mod tests;
mod types;

use parse::{normalize_path, now_unix, parse_failure_block, split_failure_blocks};
pub use types::*;

pub const STACKY_SCHEMA_VERSION: &str = "stacky.slice.v1";

pub fn slice(request: StackSliceRequest) -> StackSliceOutput {
    let source = request.source.clone();
    let blocks = split_failure_blocks(&request.log_text);

    let mut unique = Vec::<FailureSummary>::new();
    let mut by_fingerprint = HashMap::<String, usize>::new();

    for block in blocks {
        let mut parsed = parse_failure_block(&block);
        if parsed.frames.is_empty() {
            continue;
        }

        if let Some(idx) = by_fingerprint.get(&parsed.fingerprint).copied() {
            unique[idx].occurrences = unique[idx].occurrences.saturating_add(1);
        } else {
            let idx = unique.len();
            by_fingerprint.insert(parsed.fingerprint.clone(), idx);
            parsed.occurrences = 1;
            unique.push(parsed);
        }
    }

    if let Some(max) = request.max_failures {
        unique.truncate(max);
    }

    let total_failures = by_fingerprint
        .values()
        .filter_map(|idx| unique.get(*idx))
        .map(|failure| failure.occurrences)
        .sum::<usize>();

    let unique_failures = unique.len();
    let duplicates_removed = total_failures.saturating_sub(unique_failures);

    StackSliceOutput {
        schema_version: STACKY_SCHEMA_VERSION.to_string(),
        source,
        total_failures,
        unique_failures,
        duplicates_removed,
        failures: unique,
    }
}

pub fn slice_to_envelope(request: StackSliceRequest) -> EnvelopeV1<StackSliceOutput> {
    let started = Instant::now();
    let source = request
        .source
        .clone()
        .unwrap_or_else(|| "stdin".to_string());
    let output = slice(request);

    let mut file_counts = HashMap::<String, usize>::new();
    let mut symbol_counts = HashMap::<String, usize>::new();
    for failure in &output.failures {
        for frame in &failure.frames {
            if let Some(path) = frame.file.as_deref() {
                *file_counts.entry(normalize_path(path)).or_insert(0) += 1;
            }
            if let Some(function) = frame.function.as_deref() {
                *symbol_counts
                    .entry(function.trim().to_string())
                    .or_insert(0) += 1;
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
            source: Some("stacky.slice".to_string()),
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
            kind: Some("function".to_string()),
            relevance: Some((count as f64 / max_symbol).clamp(0.0, 1.0)),
            source: Some("stacky.slice".to_string()),
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
        tool: "stacky".to_string(),
        kind: "stack_slice".to_string(),
        hash: String::new(),
        summary: format!(
            "stack failures total={} unique={} duplicates_removed={}",
            output.total_failures, output.unique_failures, output.duplicates_removed
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

pub fn slice_to_packet(request: StackSliceRequest) -> StackPacket {
    let output = slice(request);

    let mut paths = BTreeSet::new();
    let mut refs = Vec::new();
    let mut text_blobs = Vec::new();

    for failure in &output.failures {
        text_blobs.push(format!(
            "{} ({})",
            failure.title,
            failure
                .first_actionable_frame
                .as_ref()
                .and_then(|frame| frame.file.as_deref())
                .unwrap_or("unknown")
        ));

        for frame in &failure.frames {
            if let Some(path) = frame.file.as_ref() {
                let normalized = normalize_path(path);
                paths.insert(normalized.clone());
                refs.push(json!({
                    "kind": "file",
                    "value": normalized,
                    "source": "stacky-slice-v1",
                    "relevance": if frame.actionable { 1.0 } else { 0.5 }
                }));
            }
            if let Some(function) = frame.function.as_ref() {
                refs.push(json!({
                    "kind": "symbol",
                    "value": function,
                    "source": "stacky-slice-v1",
                    "relevance": if frame.actionable { 0.9 } else { 0.4 }
                }));
            }
        }
    }

    refs.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    refs.dedup_by(|a, b| a == b);

    let summary = format!(
        "total_failures: {}\nunique_failures: {}\nduplicates_removed: {}",
        output.total_failures, output.unique_failures, output.duplicates_removed
    );

    let sections = output
        .failures
        .iter()
        .map(|failure| {
            let actionable = failure
                .first_actionable_frame
                .as_ref()
                .and_then(|frame| frame.file.as_deref())
                .unwrap_or("unknown");
            let body = format!(
                "occurrences: {}\nactionable_frame: {}\nfingerprint: {}",
                failure.occurrences, actionable, failure.fingerprint
            );
            json!({
                "id": format!("failure-{}", failure.fingerprint),
                "title": failure.title,
                "body": body,
                "refs": refs,
                "relevance": 1.0,
            })
        })
        .collect::<Vec<_>>();

    StackPacket {
        packet_id: Some("stacky-slice-v1".to_string()),
        tool: Some("stacky".to_string()),
        tools: vec!["stacky".to_string()],
        reducer: Some("slice".to_string()),
        reducers: vec!["slice".to_string()],
        paths: paths.into_iter().collect(),
        payload: serde_json::to_value(&output).unwrap_or_default(),
        sections,
        refs,
        text_blobs: vec![summary],
    }
}
