use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const RUN_SAVINGS_FILE: &str = "run-savings.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunSavingsRecord {
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) family: String,
    pub(crate) canonical_kind: String,
    pub(crate) exit_code: i32,
    pub(crate) raw_est_tokens: u64,
    pub(crate) reduced_est_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) estimate_scope: Option<String>,
    pub(crate) savings_percent: f64,
    pub(crate) fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) changed_paths: Vec<String>,
    pub(crate) timestamp_unix_ms: u128,
}

pub(crate) fn record_run_savings(root: &Path, record: &RunSavingsRecord) -> Result<()> {
    let path = run_savings_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open '{}'", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

pub(crate) fn load_run_savings(root: &Path, limit: usize) -> Result<Vec<RunSavingsRecord>> {
    let path = run_savings_path(root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut records = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RunSavingsRecord>(line).ok())
        .collect::<Vec<_>>();
    records.sort_by(|a, b| b.timestamp_unix_ms.cmp(&a.timestamp_unix_ms));
    records.truncate(limit.max(1));
    Ok(records)
}

pub(crate) fn reset_run_savings(root: &Path) -> Result<usize> {
    let path = run_savings_path(root);
    if !path.exists() {
        return Ok(0);
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let count = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    fs::remove_file(&path).with_context(|| format!("failed to remove '{}'", path.display()))?;
    Ok(count)
}

fn run_savings_path(root: &Path) -> PathBuf {
    root.join(".packet28").join(RUN_SAVINGS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_run_savings_records_preserve_unknown_estimate_scope() {
        let record: RunSavingsRecord = serde_json::from_value(serde_json::json!({
            "command": "cargo test", "cwd": "/fixture", "family": "rust",
            "canonical_kind": "rust_test", "exit_code": 0, "raw_est_tokens": 20,
            "reduced_est_tokens": 0, "savings_percent": 100.0,
            "fallback_reason": null, "timestamp_unix_ms": 1
        }))
        .unwrap();
        assert!(record.estimate_scope.is_none());
        assert!(serde_json::to_value(record)
            .unwrap()
            .get("estimate_scope")
            .is_none());
    }
}
