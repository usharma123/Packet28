use std::collections::{BTreeMap, HashMap, HashSet};

use covy_core::diagnostics::{DiagnosticsData, DiagnosticsFormat, Issue, Severity};
use covy_core::CovyError;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct SarifLogRaw<'a> {
    #[serde(borrow, default)]
    runs: Vec<SarifRunRaw<'a>>,
}

#[derive(Deserialize)]
struct SarifRunRaw<'a> {
    tool: Option<SarifTool>,
    #[serde(borrow, default)]
    results: Vec<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct SarifTool {
    driver: Option<SarifDriver>,
}

#[derive(Debug, Deserialize)]
struct SarifDriver {
    name: Option<String>,
    #[serde(default)]
    rules: Vec<SarifRule>,
}

#[derive(Debug, Deserialize)]
struct SarifRule {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    #[serde(rename = "ruleIndex")]
    rule_index: Option<usize>,
    level: Option<String>,
    message: Option<SarifMessage>,
    #[serde(default)]
    locations: Vec<SarifLocation>,
    fingerprints: Option<BTreeMap<String, String>>,
    #[serde(rename = "partialFingerprints")]
    partial_fingerprints: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct SarifMessage {
    text: Option<String>,
    markdown: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical_location: Option<SarifPhysicalLocation>,
}

#[derive(Debug, Deserialize)]
struct SarifPhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact_location: Option<SarifArtifactLocation>,
    region: Option<SarifRegion>,
}

#[derive(Debug, Deserialize)]
struct SarifArtifactLocation {
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: Option<u32>,
    #[serde(rename = "startColumn")]
    start_column: Option<u32>,
    #[serde(rename = "endLine")]
    end_line: Option<u32>,
}

pub fn parse_sarif(content: &[u8]) -> Result<DiagnosticsData, CovyError> {
    if content.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(sarif)".to_string(),
        });
    }

    let text = std::str::from_utf8(content).map_err(|e| CovyError::Parse {
        format: "sarif".to_string(),
        detail: format!("Invalid UTF-8: {e}"),
    })?;

    let log: SarifLogRaw<'_> = serde_json::from_str(text).map_err(|e| CovyError::Parse {
        format: "sarif".to_string(),
        detail: e.to_string(),
    })?;

    let per_run: Vec<Result<DiagnosticsData, CovyError>> =
        log.runs.into_par_iter().map(parse_run_raw).collect();

    let mut diagnostics = DiagnosticsData::new();
    diagnostics.format = Some(DiagnosticsFormat::Sarif);
    for run_data in per_run {
        let run_data = run_data?;
        diagnostics.merge(&run_data);
    }
    Ok(diagnostics)
}

fn parse_run_raw(run: SarifRunRaw<'_>) -> Result<DiagnosticsData, CovyError> {
    let mut diagnostics = DiagnosticsData::new();
    diagnostics.format = Some(DiagnosticsFormat::Sarif);

    let tool_name = run
        .tool
        .as_ref()
        .and_then(|t| t.driver.as_ref())
        .and_then(|d| d.name.as_ref())
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let rules = run
        .tool
        .as_ref()
        .and_then(|t| t.driver.as_ref())
        .map(|d| d.rules.as_slice())
        .unwrap_or(&[]);

    let mut seen_fingerprints_by_file: HashMap<String, HashSet<String>> = HashMap::new();

    for raw_result in run.results {
        let result: SarifResult =
            serde_json::from_str(raw_result.get()).map_err(|e| CovyError::Parse {
                format: "sarif".to_string(),
                detail: e.to_string(),
            })?;

        let Some((path, line, column, end_line)) = extract_location(&result) else {
            continue;
        };
        if line == 0 {
            continue;
        }

        let severity = map_severity(result.level.as_deref());
        let rule_id = resolve_rule_id(&result, rules);
        let message = resolve_message(&result);
        let fingerprint = resolve_fingerprint(&result, &tool_name, &rule_id, &path, line, &message);

        let entry = diagnostics.issues_by_file.entry(path.clone()).or_default();
        let seen = seen_fingerprints_by_file
            .entry(path.clone())
            .or_insert_with(|| {
                let mut seen = HashSet::with_capacity(entry.len().max(16));
                seen.extend(entry.iter().map(|existing| existing.fingerprint.clone()));
                seen
            });

        if !seen.insert(fingerprint.clone()) {
            continue;
        }

        let issue = Issue {
            path,
            line,
            column,
            end_line,
            severity,
            rule_id,
            message,
            source: tool_name.clone(),
            fingerprint,
        };
        entry.push(issue);
    }

    Ok(diagnostics)
}

fn extract_location(result: &SarifResult) -> Option<(String, u32, Option<u32>, Option<u32>)> {
    let physical = result.locations.first()?.physical_location.as_ref()?;
    let uri = physical.artifact_location.as_ref()?.uri.as_deref()?;
    let path = normalize_sarif_path(uri);
    if path.is_empty() {
        return None;
    }

    let region = physical.region.as_ref()?;
    let line = region.start_line?;

    Some((path, line, region.start_column, region.end_line))
}

fn normalize_sarif_path(uri: &str) -> String {
    let mut normalized = uri.trim().to_string();

    if let Some(rest) = normalized.strip_prefix("file://") {
        normalized = rest.to_string();
        // Handle file:///C:/... on Windows.
        let bytes = normalized.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
            normalized = normalized[1..].to_string();
        }
    }

    normalized = normalized.replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }

    normalized
}

fn map_severity(level: Option<&str>) -> Severity {
    match level.unwrap_or("warning") {
        "error" => Severity::Error,
        "note" | "none" => Severity::Note,
        _ => Severity::Warning,
    }
}

fn resolve_rule_id(result: &SarifResult, rules: &[SarifRule]) -> String {
    if let Some(rule_id) = result.rule_id.as_ref().filter(|s| !s.is_empty()) {
        return rule_id.clone();
    }

    if let Some(idx) = result.rule_index {
        if let Some(rule_id) = rules
            .get(idx)
            .and_then(|rule| rule.id.as_ref())
            .filter(|s| !s.is_empty())
        {
            return rule_id.clone();
        }
    }

    "unknown".to_string()
}

fn resolve_message(result: &SarifResult) -> String {
    result
        .message
        .as_ref()
        .and_then(|msg| msg.text.as_ref().or(msg.markdown.as_ref()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<no message>".to_string())
}

fn resolve_fingerprint(
    result: &SarifResult,
    source: &str,
    rule_id: &str,
    path: &str,
    line: u32,
    message: &str,
) -> String {
    if let Some(fp) = first_fingerprint_value(&result.fingerprints) {
        return fp;
    }
    if let Some(fp) = first_fingerprint_value(&result.partial_fingerprints) {
        return fp;
    }

    let normalized_message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let key = format!("{source}:{rule_id}:{path}:{line}:{normalized_message}");
    let digest = blake3::hash(key.as_bytes()).to_hex().to_string();
    digest[..32].to_string()
}

fn first_fingerprint_value(map: &Option<BTreeMap<String, String>>) -> Option<String> {
    map.as_ref()
        .and_then(|m| m.values().find(|v| !v.is_empty()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(rel: &str) -> PathBuf {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        workspace.join("tests").join("fixtures").join(rel)
    }

    #[test]
    fn parse_basic_fixture() {
        let content = std::fs::read(fixture("sarif/basic.sarif")).unwrap();
        let data = parse_sarif(&content).unwrap();

        assert_eq!(data.total_issues(), 5);
        assert!(data.issues_by_file.contains_key("src/main.rs"));
        assert!(data.issues_by_file.contains_key("src/lib.rs"));

        let main = &data.issues_by_file["src/main.rs"];
        assert_eq!(main.len(), 2);
        assert!(main.iter().any(|i| i.fingerprint == "eslint-fp-1"));
        assert!(main.iter().any(|i| i.fingerprint == "eslint-partial-1"));
    }

    #[test]
    fn parse_uses_rule_index() {
        let content = br#"{
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {"name": "x", "rules": [{"id": "rule-a"}] }},
                "results": [{
                    "ruleIndex": 0,
                    "message": {"text": "hi"},
                    "locations": [{"physicalLocation": {
                        "artifactLocation": {"uri": "src/a.rs"},
                        "region": {"startLine": 1}
                    }}]
                }]
            }]
        }"#;

        let data = parse_sarif(content).unwrap();
        let issue = &data.issues_by_file["src/a.rs"][0];
        assert_eq!(issue.rule_id, "rule-a");
    }

    #[test]
    fn parse_skips_missing_location_or_line() {
        let content = br#"{
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {"name": "x"}},
                "results": [
                    {"message": {"text": "no location"}},
                    {"message": {"text": "zero line"}, "locations": [{"physicalLocation": {
                        "artifactLocation": {"uri": "src/a.rs"},
                        "region": {"startLine": 0}
                    }}]}
                ]
            }]
        }"#;

        let data = parse_sarif(content).unwrap();
        assert_eq!(data.total_issues(), 0);
    }

    #[test]
    fn parse_empty_runs() {
        let content = std::fs::read(fixture("sarif/empty.sarif")).unwrap();
        let data = parse_sarif(&content).unwrap();
        assert_eq!(data.total_issues(), 0);
    }

    #[test]
    fn fingerprint_fallback_hash_is_used() {
        let content = br#"{
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {"name": "x"}},
                "results": [{
                    "ruleId": "r1",
                    "level": "warning",
                    "message": {"text": "Message with   spacing"},
                    "locations": [{"physicalLocation": {
                        "artifactLocation": {"uri": "src/a.rs"},
                        "region": {"startLine": 2}
                    }}]
                }]
            }]
        }"#;

        let data = parse_sarif(content).unwrap();
        let fp = &data.issues_by_file["src/a.rs"][0].fingerprint;
        assert_eq!(fp.len(), 32);
    }

    #[test]
    fn parse_strips_file_uri_prefix() {
        let content = br#"{
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {"name": "x"}},
                "results": [{
                    "ruleId": "r1",
                    "message": {"text": "msg"},
                    "locations": [{"physicalLocation": {
                        "artifactLocation": {"uri": "file:///repo/src/a.rs"},
                        "region": {"startLine": 7}
                    }}]
                }]
            }]
        }"#;

        let data = parse_sarif(content).unwrap();
        assert!(data.issues_by_file.contains_key("/repo/src/a.rs"));
    }
}
