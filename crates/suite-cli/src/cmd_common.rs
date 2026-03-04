use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};
use suite_packet_core::{CoverageData, CoverageFormat};

pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None => "terminal".to_string(),
    }
}

pub fn resolve_json_output(
    json_flag: bool,
    legacy_format: Option<&str>,
    legacy_flag_name: &str,
) -> Result<bool> {
    if json_flag {
        if let Some(fmt) = legacy_format {
            if !fmt.eq_ignore_ascii_case("json") {
                anyhow::bail!(
                    "Conflicting output flags: --json and {} {}",
                    legacy_flag_name,
                    fmt
                );
            }
        }
        return Ok(true);
    }

    Ok(legacy_format.is_some_and(|fmt| fmt.eq_ignore_ascii_case("json")))
}

pub fn default_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

fn ingest_coverage_auto(path: &Path) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, None).map_err(Into::into)
}

fn ingest_coverage_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, Some(format)).map_err(Into::into)
}

fn ingest_coverage_stdin(format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_stdin(format).map_err(Into::into)
}

fn ingest_diagnostics(path: &Path) -> Result<suite_packet_core::diagnostics::DiagnosticsData> {
    suite_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

pub fn cache_summary_line(metadata: &Value) -> Option<String> {
    let cache = metadata.get("cache")?;
    let hit = cache.get("hit").and_then(Value::as_bool).unwrap_or(false);
    let key = cache
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let age = cache.get("entry_age_secs").and_then(Value::as_u64);
    let miss_reason = cache.get("miss_reason").and_then(Value::as_str);

    if hit {
        Some(format!(
            "cache: hit key={} age={}s",
            key,
            age.unwrap_or_default()
        ))
    } else if let Some(reason) = miss_reason {
        Some(format!("cache: miss key={} reason={}", key, reason))
    } else {
        Some(format!("cache: miss key={}", key))
    }
}

pub fn budget_retry_hint(
    governed_metadata: &Value,
    current_tokens: u64,
    current_bytes: usize,
    retry_command: &str,
) -> Option<Value> {
    let trim = governed_metadata.get("budget_trim")?;
    let truncated = trim
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !truncated {
        return None;
    }

    let sections_dropped = trim
        .get("sections_dropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let refs_dropped = trim
        .get("refs_dropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let dropped_total = sections_dropped.saturating_add(refs_dropped);

    let sections_input = trim
        .get("sections_input")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let refs_input = trim.get("refs_input").and_then(Value::as_u64).unwrap_or(0);
    let inputs_total = sections_input.saturating_add(refs_input);
    let dropped_ratio = if inputs_total == 0 {
        1.0
    } else {
        dropped_total as f64 / inputs_total as f64
    };

    if dropped_total < 3 && dropped_ratio < 0.30 {
        return None;
    }

    let est_tokens = trim
        .get("estimated_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(current_tokens);
    let est_bytes = trim
        .get("estimated_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(current_bytes as u64);

    let suggested_tokens = round_up_u64(
        ((current_tokens as f64 * 1.5).max(est_tokens as f64 * 1.2)).ceil() as u64,
        250,
    );
    let suggested_bytes = round_up_usize(
        ((current_bytes as f64 * 1.5).max(est_bytes as f64 * 1.2)).ceil() as usize,
        1024,
    );

    Some(json!({
        "reason": "high_truncation",
        "dropped_total": dropped_total,
        "dropped_ratio": dropped_ratio,
        "suggested_context_budget_tokens": suggested_tokens,
        "suggested_context_budget_bytes": suggested_bytes,
        "retry_command": format!(
            "{} --context-budget-tokens {} --context-budget-bytes {}",
            retry_command, suggested_tokens, suggested_bytes
        ),
    }))
}

fn round_up_u64(value: u64, step: u64) -> u64 {
    if step == 0 {
        return value;
    }
    value.div_ceil(step) * step
}

fn round_up_usize(value: usize, step: usize) -> usize {
    if step == 0 {
        return value;
    }
    value.div_ceil(step) * step
}
