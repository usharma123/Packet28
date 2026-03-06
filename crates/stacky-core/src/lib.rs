use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use suite_packet_core::{BudgetCost, EnvelopeV1, FileRef, Provenance, SymbolRef};

pub const STACKY_SCHEMA_VERSION: &str = "stacky.slice.v1";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct StackSliceRequest {
    pub log_text: String,
    pub source: Option<String>,
    pub max_failures: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct StackFrame {
    pub raw: String,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub normalized: String,
    pub actionable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FailureSummary {
    pub fingerprint: String,
    pub title: String,
    pub message: String,
    pub occurrences: usize,
    pub frames: Vec<StackFrame>,
    pub first_actionable_frame: Option<StackFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StackSliceOutput {
    pub schema_version: String,
    pub source: Option<String>,
    pub total_failures: usize,
    pub unique_failures: usize,
    pub duplicates_removed: usize,
    pub failures: Vec<FailureSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StackPacket {
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

fn split_failure_blocks(log_text: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();

    for line in log_text.lines() {
        if is_failure_header(line) {
            if !current.is_empty() {
                blocks.push(current);
                current = Vec::new();
            }
            current.push(line.to_string());
            continue;
        }

        if current.is_empty() {
            continue;
        }

        if line.trim().is_empty() {
            blocks.push(current);
            current = Vec::new();
            continue;
        }

        current.push(line.to_string());
    }

    if !current.is_empty() {
        blocks.push(current);
    }

    if blocks.is_empty() && !log_text.trim().is_empty() {
        return vec![log_text.lines().map(ToOwned::to_owned).collect()];
    }

    blocks
}

fn parse_failure_block(lines: &[String]) -> FailureSummary {
    let title = lines
        .first()
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "unknown failure".to_string());

    let mut message_lines = Vec::new();
    let mut frames = Vec::new();

    for line in lines.iter().skip(1) {
        if let Some(frame) = parse_frame(line) {
            frames.push(frame);
        } else if !line.trim().is_empty() {
            message_lines.push(line.trim().to_string());
        }
    }

    if frames.is_empty() {
        for line in lines {
            if let Some(frame) = parse_frame(line) {
                frames.push(frame);
            }
        }
    }

    let message = if message_lines.is_empty() {
        title.clone()
    } else {
        message_lines.join("\\n")
    };

    let first_actionable_frame = frames
        .iter()
        .find(|frame| frame.actionable)
        .cloned()
        .or_else(|| frames.first().cloned());

    let mut fingerprint_material = title.to_ascii_lowercase();
    for frame in frames.iter().take(3) {
        fingerprint_material.push('|');
        fingerprint_material.push_str(&frame.normalized);
    }

    FailureSummary {
        fingerprint: short_fingerprint(&fingerprint_material),
        title,
        message,
        occurrences: 0,
        frames,
        first_actionable_frame,
    }
}

fn is_failure_header(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("at ") || lower.starts_with("file ") {
        return false;
    }

    lower.contains("exception")
        || lower.contains("panic")
        || lower.contains("fatal")
        || lower.contains("traceback")
        || lower.contains("error:")
        || lower.contains("failed")
}

fn parse_frame(line: &str) -> Option<StackFrame> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(captures) = java_frame_re().captures(trimmed) {
        let function = captures.name("func").map(|m| m.as_str().to_string());
        let file = captures
            .name("file")
            .map(|m| normalize_path(m.as_str()))
            .filter(|value| !value.is_empty());
        let line = captures
            .name("line")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        return Some(build_frame(trimmed, function, file, line));
    }

    if let Some(captures) = python_frame_re().captures(trimmed) {
        let function = captures.name("func").map(|m| m.as_str().to_string());
        let file = captures
            .name("file")
            .map(|m| normalize_path(m.as_str()))
            .filter(|value| !value.is_empty());
        let line = captures
            .name("line")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        return Some(build_frame(trimmed, function, file, line));
    }

    if let Some(captures) = generic_path_re().captures(trimmed) {
        let function = captures.name("func").map(|m| m.as_str().trim().to_string());
        let file = captures
            .name("file")
            .map(|m| normalize_path(m.as_str()))
            .filter(|value| !value.is_empty());
        let line = captures
            .name("line")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        return Some(build_frame(trimmed, function, file, line));
    }

    None
}

fn build_frame(
    raw: &str,
    function: Option<String>,
    file: Option<String>,
    line: Option<u32>,
) -> StackFrame {
    let actionable = file
        .as_ref()
        .map(|value| is_actionable_file(value))
        .unwrap_or(false);

    let normalized = format!(
        "{}:{}:{}",
        function
            .as_ref()
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_else(|| "unknown".to_string()),
        file.as_ref()
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_else(|| "unknown".to_string()),
        line.map(|value| value.to_string())
            .unwrap_or_else(|| "0".to_string())
    );

    StackFrame {
        raw: raw.to_string(),
        function,
        file,
        line,
        normalized,
        actionable,
    }
}

fn is_actionable_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !lower.contains("/rustc/")
        && !lower.contains("site-packages")
        && !lower.contains("node_modules")
        && !lower.contains("/usr/lib/")
        && !lower.contains("/lib/")
        && !lower.contains("java.base")
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn short_fingerprint(material: &str) -> String {
    let hash = blake3::hash(material.as_bytes());
    hash.to_hex().to_string().chars().take(16).collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn java_frame_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^at\s+(?P<func>[\w.$<>]+)\((?P<file>[^:()]+):(?P<line>\d+)\)").unwrap()
    })
}

fn python_frame_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^File\s+\"(?P<file>[^\"]+)\",\s+line\s+(?P<line>\d+),\s+in\s+(?P<func>.+)$"#)
            .unwrap()
    })
}

fn generic_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?P<file>[A-Za-z0-9_./-]+\.[A-Za-z0-9_+-]+):(?P<line>\d+)(?::\d+)?(?:\s+in\s+(?P<func>[\w.$<>:]+))?",
        )
        .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_dedupes_repeated_failures() {
        let input = r#"
java.lang.IllegalStateException: boom
  at com.example.Service.run(Service.java:42)
  at com.example.Main.main(Main.java:10)

java.lang.IllegalStateException: boom
  at com.example.Service.run(Service.java:42)
  at com.example.Main.main(Main.java:10)

thread 'main' panicked at src/lib.rs:11:2
  0: app::core::run at src/lib.rs:11:2
  1: std::rt::lang_start_internal at /rustc/abc/std/src/rt.rs:95:18
"#;

        let output = slice(StackSliceRequest {
            log_text: input.to_string(),
            source: Some("fixture".to_string()),
            max_failures: None,
        });

        assert_eq!(output.total_failures, 3);
        assert_eq!(output.unique_failures, 2);
        assert_eq!(output.duplicates_removed, 1);
        assert_eq!(output.failures[0].occurrences, 2);
    }

    #[test]
    fn packet_output_is_deterministic() {
        let input = r#"
panic: failed to connect
  at service::dial src/net.rs:40:3
"#;

        let packet_a = slice_to_packet(StackSliceRequest {
            log_text: input.to_string(),
            source: Some("a".to_string()),
            max_failures: None,
        });
        let packet_b = slice_to_packet(StackSliceRequest {
            log_text: input.to_string(),
            source: Some("a".to_string()),
            max_failures: None,
        });

        assert_eq!(
            serde_json::to_string(&packet_a).unwrap(),
            serde_json::to_string(&packet_b).unwrap()
        );
        assert_eq!(packet_a.packet_id.as_deref(), Some("stacky-slice-v1"));
    }
}
