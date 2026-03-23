use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use regex::Regex;
use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance};

use crate::types::{
    CommandSummaryPayload, DroppedSummary, PacketDetail, ProxyRunRequest, SummaryGroup,
};

const DEFAULT_MAX_OUTPUT_BYTES: usize = 24_000;
const DEFAULT_MAX_LINES: usize = 160;
const DEFAULT_PACKET_BYTE_CAP: usize = 2_500;
const HIGHLIGHT_CAP: usize = 6;
const SAFE_COMMAND_ERROR: &str =
    "proxy.run only allows: ls, find, grep, git status, git log";

#[derive(Debug, Clone)]
struct ScoredLine {
    line: String,
    group: String,
    score: i32,
}

pub fn run_and_reduce(
    req: ProxyRunRequest,
) -> Result<EnvelopeV1<CommandSummaryPayload>, CovyError> {
    if req.argv.is_empty() {
        return Err(CovyError::Other(
            "proxy.run requires at least one command arg".to_string(),
        ));
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

    let output = cmd.output().map_err(CovyError::IoRaw)?;
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

    let selected_lines = selected
        .iter()
        .map(|item| item.line.clone())
        .collect::<Vec<_>>();
    let lines_out = selected_lines.len();
    let bytes_out = selected_lines
        .iter()
        .map(|v| v.len().saturating_add(1))
        .sum::<usize>();
    let bytes_saved = bytes_in.saturating_sub(bytes_out);

    let highlights = selected
        .iter()
        .take(HIGHLIGHT_CAP)
        .map(|item| item.line.clone())
        .collect::<Vec<_>>();

    let highlight_index_by_line = highlights
        .iter()
        .enumerate()
        .map(|(idx, line)| (line.clone(), idx))
        .collect::<BTreeMap<_, _>>();

    let mut groups = BTreeMap::<String, SummaryGroup>::new();
    for item in &selected {
        let entry = groups
            .entry(item.group.clone())
            .or_insert_with(|| SummaryGroup {
                name: item.group.clone(),
                count: 0,
                example_line_indexes: Vec::new(),
            });
        entry.count = entry.count.saturating_add(1);
        if let Some(line_idx) = highlight_index_by_line.get(&item.line).copied() {
            if entry.example_line_indexes.len() < 3
                && !entry.example_line_indexes.contains(&line_idx)
            {
                entry.example_line_indexes.push(line_idx);
            }
        }
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|a, b| {
        score_group(&b.name)
            .cmp(&score_group(&a.name))
            .then_with(|| a.name.cmp(&b.name))
    });

    let output_lines = if req.detail == PacketDetail::Rich {
        selected_lines.clone()
    } else {
        Vec::new()
    };

    let payload = CommandSummaryPayload {
        command: command.clone(),
        exit_code: output.status.code().unwrap_or(1),
        lines_in,
        lines_out,
        bytes_in,
        bytes_out,
        bytes_saved,
        token_saved_est: (bytes_saved / 4) as u64,
        groups,
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
        highlights,
        output_lines,
    };

    let files = extract_file_refs(&selected_lines);
    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();

    let mut envelope = EnvelopeV1 {
        version: "1".to_string(),
        tool: "proxy".to_string(),
        kind: "command_summary".to_string(),
        hash: String::new(),
        summary: format!(
            "command='{}' exit_code={} lines={} -> {} bytes_saved={}",
            payload.command,
            payload.exit_code,
            payload.lines_in,
            payload.lines_out,
            payload.bytes_saved
        ),
        files,
        symbols: Vec::new(),
        risk: None,
        confidence: Some(1.0),
        budget_cost: BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: Provenance {
            inputs: vec![command],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    };
    envelope = envelope.with_canonical_hash_and_real_budget();
    enforce_packet_budget(
        &mut envelope,
        req.packet_byte_cap.unwrap_or(DEFAULT_PACKET_BYTE_CAP),
    );

    Ok(envelope)
}

pub fn command_supported(argv: &[String]) -> bool {
    validate_safe_command(argv).is_ok()
}

fn validate_safe_command(argv: &[String]) -> Result<(), CovyError> {
    let root = argv[0].as_str();
    match root {
        "ls" | "find" | "grep" => Ok(()),
        "git" => {
            if let Some(subcmd) = git_subcommand(argv) {
                if matches!(subcmd.as_str(), "status" | "log") {
                    return Ok(());
                }
            }
            Err(CovyError::Other(SAFE_COMMAND_ERROR.to_string()))
        }
        _ => Err(CovyError::Other(SAFE_COMMAND_ERROR.to_string())),
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
    let mut counts = BTreeMap::<String, usize>::new();
    for line in lines {
        for cap in path_capture_re().captures_iter(line) {
            if let Some(m) = cap.get(1) {
                let path = normalize_path(m.as_str());
                if !path.is_empty() {
                    *counts.entry(path).or_insert(0) += 1;
                }
            }
        }
    }

    let max_count = counts.values().copied().max().unwrap_or(1) as f64;
    let mut out = counts
        .into_iter()
        .map(|(path, count)| FileRef {
            path,
            relevance: Some((count as f64 / max_count).clamp(0.0, 1.0)),
            source: Some("proxy.command_summary".to_string()),
        })
        .collect::<Vec<_>>();

    out.sort_by(|a, b| {
        b.relevance
            .unwrap_or(0.0)
            .total_cmp(&a.relevance.unwrap_or(0.0))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn enforce_packet_budget(envelope: &mut EnvelopeV1<CommandSummaryPayload>, cap: usize) {
    for _ in 0..64 {
        let current_bytes = serde_json::to_vec(envelope).map(|v| v.len()).unwrap_or(0);
        if current_bytes <= cap {
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            return;
        }

        if trim_group_examples(&mut envelope.payload.groups) {
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            continue;
        }
        if trim_highlights(
            &mut envelope.payload.highlights,
            &mut envelope.payload.groups,
        ) {
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            continue;
        }
        if trim_low_priority_group(&mut envelope.payload.groups) {
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            continue;
        }
        if trim_low_relevance_file(&mut envelope.files) {
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            continue;
        }
        if !envelope.payload.output_lines.is_empty() {
            envelope.payload.output_lines.pop();
            *envelope = envelope.clone().with_canonical_hash_and_real_budget();
            continue;
        }
        break;
    }
}

fn trim_group_examples(groups: &mut [SummaryGroup]) -> bool {
    for group in groups.iter_mut().rev() {
        if !group.example_line_indexes.is_empty() {
            group.example_line_indexes.pop();
            return true;
        }
    }
    false
}

fn trim_highlights(highlights: &mut Vec<String>, groups: &mut [SummaryGroup]) -> bool {
    if highlights.is_empty() {
        return false;
    }
    highlights.pop();
    let cap = highlights.len();
    for group in groups {
        group.example_line_indexes.retain(|idx| *idx < cap);
    }
    true
}

fn trim_low_priority_group(groups: &mut Vec<SummaryGroup>) -> bool {
    if groups.is_empty() {
        return false;
    }
    groups.pop();
    true
}

fn trim_low_relevance_file(files: &mut Vec<FileRef>) -> bool {
    if files.is_empty() {
        return false;
    }
    files.pop();
    true
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
