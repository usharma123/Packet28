//! Integration with ccusage (npx ccusage) for API usage tracking.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A single usage record from ccusage.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CcusageRecord {
    pub date: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
}

/// Summary of ccusage data.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CcusageSummary {
    pub records: Vec<CcusageRecord>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cost: f64,
}

/// Try to run ccusage and parse its JSON output.
/// Returns None if ccusage is not available.
pub fn fetch_ccusage() -> Result<Option<CcusageSummary>> {
    let output = match std::process::Command::new("npx")
        .args(["ccusage", "--json"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let records: Vec<CcusageRecord> = match serde_json::from_str(&stdout) {
        Ok(records) => records,
        Err(_) => return Ok(None),
    };

    let mut summary = CcusageSummary::default();
    for record in &records {
        summary.total_input_tokens += record.input_tokens;
        summary.total_output_tokens += record.output_tokens;
        summary.total_cache_creation_tokens += record.cache_creation_tokens;
        summary.total_cache_read_tokens += record.cache_read_tokens;
        summary.total_cost += record.cost;
    }
    summary.records = records;

    Ok(Some(summary))
}

/// Try to parse ccusage output from a pre-fetched string.
pub fn parse_ccusage_output(json: &str) -> Option<CcusageSummary> {
    let records: Vec<CcusageRecord> = serde_json::from_str(json).ok()?;
    let mut summary = CcusageSummary::default();
    for record in &records {
        summary.total_input_tokens += record.input_tokens;
        summary.total_output_tokens += record.output_tokens;
        summary.total_cache_creation_tokens += record.cache_creation_tokens;
        summary.total_cache_read_tokens += record.cache_read_tokens;
        summary.total_cost += record.cost;
    }
    summary.records = records;
    Some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ccusage_output_works() {
        let json = r#"[
            {"date":"2026-03-16","input_tokens":50000,"output_tokens":10000,"cache_creation_tokens":5000,"cache_read_tokens":20000,"cost":0.35},
            {"date":"2026-03-17","input_tokens":30000,"output_tokens":8000,"cache_creation_tokens":3000,"cache_read_tokens":15000,"cost":0.22}
        ]"#;
        let summary = parse_ccusage_output(json).unwrap();
        assert_eq!(summary.records.len(), 2);
        assert_eq!(summary.total_input_tokens, 80000);
        assert_eq!(summary.total_output_tokens, 18000);
        assert!((summary.total_cost - 0.57).abs() < 0.01);
    }

    #[test]
    fn parse_ccusage_invalid_returns_none() {
        assert!(parse_ccusage_output("not json").is_none());
    }
}
