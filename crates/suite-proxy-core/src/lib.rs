use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};
use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance};

const DEFAULT_MAX_OUTPUT_BYTES: usize = 24_000;
const DEFAULT_MAX_LINES: usize = 160;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProxyRunRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env_allowlist: Vec<String>,
    pub max_output_bytes: Option<usize>,
    pub max_lines: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SummaryGroup {
    pub name: String,
    pub count: usize,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DroppedSummary {
    pub reason: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CommandSummaryPayload {
    pub command: String,
    pub exit_code: i32,
    pub lines_in: usize,
    pub lines_out: usize,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub bytes_saved: usize,
    pub token_saved_est: u64,
    pub groups: Vec<SummaryGroup>,
    pub dropped: Vec<DroppedSummary>,
    pub output_lines: Vec<String>,
}

#[derive(Debug, Clone)]
struct ScoredLine {
    line: String,
    group: String,
    score: i32,
}

pub fn run_and_reduce(req: ProxyRunRequest) -> Result<EnvelopeV1<CommandSummaryPayload>, CovyError> {
    if req.argv.is_empty() {
        return Err(CovyError::Other("proxy.run requires at least one command arg".to_string()));
    }
    validate_safe_command(&req.argv)?;

    let started = Instant::now();
    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(req.argv.iter().skip(1));

    if let Some(cwd) = req.cwd.as_ref() {
        cmd.current_dir(PathBuf::from(cwd));
    }

    if !req.env_allowlist.is_empty() {
        let allowset = req
            .env_allowlist
            .iter()
            .map(|v| v.trim().to_string())
            .collect::<BTreeSet<_>>();
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if allowset.contains(&k) || k == "PATH" {
                cmd.env(k, v);
            }
        }
    }

    let output = cmd.output().map_err(|source| CovyError::IoRaw(source))?;
    let runtime_ms = started.elapsed().as_millis() as u64;

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !combined.ends_with('\n') && !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    let command = req.argv.join(" ");
    let lines_raw = normalize_lines(&combined);
    let lines_in = lines_raw.len();
    let bytes_in = combined.len();

    let mut seen = BTreeSet::new();
    let mut scored = Vec::new();
    let mut deduped_count = 0usize;

    for line in lines_raw {
        if !seen.insert(line.clone()) {
            deduped_count = deduped_count.saturating_add(1);
            continue;
        }
        let group = classify_line(&line);
        let score = score_group(&group);
        scored.push(ScoredLine { line, group, score });
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.line.cmp(&b.line)));

    let max_lines = req.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = req.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

    let mut selected = Vec::new();
    let mut used_bytes = 0usize;
    let mut dropped_by_budget = 0usize;
    for item in scored {
        let added = item.line.len().saturating_add(1);
        if selected.len() >= max_lines || used_bytes.saturating_add(added) > max_bytes {
            dropped_by_budget = dropped_by_budget.saturating_add(1);
            continue;
        }
        used_bytes = used_bytes.saturating_add(added);
        selected.push(item);
    }

    let mut groups = BTreeMap::<String, SummaryGroup>::new();
    for item in &selected {
        let entry = groups.entry(item.group.clone()).or_insert_with(|| SummaryGroup {
            name: item.group.clone(),
            count: 0,
            examples: Vec::new(),
        });
        entry.count = entry.count.saturating_add(1);
        if entry.examples.len() < 3 {
            entry.examples.push(item.line.clone());
        }
    }

    let output_lines = selected.iter().map(|item| item.line.clone()).collect::<Vec<_>>();
    let lines_out = output_lines.len();
    let bytes_out = output_lines.iter().map(|v| v.len().saturating_add(1)).sum::<usize>();
    let bytes_saved = bytes_in.saturating_sub(bytes_out);

    let payload = CommandSummaryPayload {
        command: command.clone(),
        exit_code: output.status.code().unwrap_or(1),
        lines_in,
        lines_out,
        bytes_in,
        bytes_out,
        bytes_saved,
        token_saved_est: (bytes_saved / 4) as u64,
        groups: groups.into_values().collect(),
        dropped: vec![
            DroppedSummary {
                reason: "deduplicated".to_string(),
                count: deduped_count,
            },
            DroppedSummary {
                reason: "budget_trim".to_string(),
                count: dropped_by_budget,
            },
        ],
        output_lines,
    };

    let files = extract_file_refs(&payload.output_lines);

    let envelope = EnvelopeV1 {
        version: "1".to_string(),
        tool: "proxy".to_string(),
        kind: "command_summary".to_string(),
        hash: String::new(),
        summary: format!(
            "command='{}' exit_code={} lines={} -> {} bytes_saved={}",
            payload.command, payload.exit_code, payload.lines_in, payload.lines_out, payload.bytes_saved
        ),
        files,
        symbols: Vec::new(),
        risk: None,
        confidence: Some(1.0),
        budget_cost: BudgetCost {
            est_tokens: (bytes_out / 4) as u64,
            est_bytes: bytes_out,
            runtime_ms,
            tool_calls: 1,
        },
        provenance: Provenance {
            inputs: vec![command],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash();

    Ok(envelope)
}

fn validate_safe_command(argv: &[String]) -> Result<(), CovyError> {
    let root = argv[0].as_str();
    match root {
        "ls" | "find" | "grep" => Ok(()),
        "git" => {
            if let Some(subcmd) = git_subcommand(argv) {
                if subcmd == "status" || subcmd == "log" {
                    return Ok(());
                }
            }
            Err(CovyError::Other(
                "proxy.run only allows: ls, find, grep, git status, git log".to_string(),
            ))
        }
        _ => Err(CovyError::Other(
            "proxy.run only allows: ls, find, grep, git status, git log".to_string(),
        )),
    }
}

fn git_subcommand(argv: &[String]) -> Option<String> {
    let mut idx = 1usize;
    while idx < argv.len() {
        let tok = argv[idx].as_str();
        if tok == "-C" || tok == "--git-dir" || tok == "--work-tree" {
            idx = idx.saturating_add(2);
            continue;
        }
        if tok.starts_with('-') {
            idx = idx.saturating_add(1);
            continue;
        }
        return Some(tok.to_string());
    }
    None
}

fn normalize_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| line.replace('\t', " ").trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn classify_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("fatal") {
        return "error".to_string();
    }
    if lower.contains("warn") {
        return "warning".to_string();
    }
    if line.starts_with('M') || line.starts_with('A') || line.starts_with('D') {
        return "git_change".to_string();
    }
    if pathish_re().is_match(line) {
        return "path".to_string();
    }
    "other".to_string()
}

fn score_group(group: &str) -> i32 {
    match group {
        "error" => 100,
        "warning" => 90,
        "git_change" => 80,
        "path" => 70,
        _ => 50,
    }
}

fn extract_file_refs(lines: &[String]) -> Vec<FileRef> {
    let mut out = BTreeSet::<String>::new();
    for line in lines {
        for cap in path_capture_re().captures_iter(line) {
            if let Some(m) = cap.get(1) {
                let path = normalize_path(m.as_str());
                if !path.is_empty() {
                    out.insert(path);
                }
            }
        }
    }

    out.into_iter()
        .map(|path| FileRef {
            path,
            relevance: Some(0.7),
            source: Some("proxy.command_summary".to_string()),
        })
        .collect()
}

fn normalize_path(input: &str) -> String {
    input.trim_matches('"').replace('\\', "/")
}

fn pathish_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9_./\\-]+\.[A-Za-z0-9]+|[A-Za-z0-9_./\\-]+/[A-Za-z0-9_./\\-]+)")
            .expect("valid regex")
    })
}

fn path_capture_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9_./\\-]+\.[A-Za-z0-9]+|[A-Za-z0-9_./\\-]+/[A-Za-z0-9_./\\-]+)")
            .expect("valid regex")
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_commands() {
        let err = run_and_reduce(ProxyRunRequest {
            argv: vec!["cat".to_string(), "/etc/passwd".to_string()],
            ..ProxyRunRequest::default()
        })
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("proxy.run only allows: ls, find, grep, git status, git log"));
    }

    #[test]
    fn deterministic_reduction_for_same_input() {
        let req = ProxyRunRequest {
            argv: vec!["ls".to_string()],
            max_lines: Some(40),
            max_output_bytes: Some(4_000),
            ..ProxyRunRequest::default()
        };

        let left = run_and_reduce(req.clone()).unwrap();
        let right = run_and_reduce(req).unwrap();

        assert_eq!(left.hash, right.hash);
        assert_eq!(left.payload.lines_out, right.payload.lines_out);
    }
}
