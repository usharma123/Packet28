use std::sync::OnceLock;

use regex::Regex;

use crate::types::{FailureSummary, StackFrame};

pub(crate) fn split_failure_blocks(log_text: &str) -> Vec<Vec<String>> {
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

pub(crate) fn parse_failure_block(lines: &[String]) -> FailureSummary {
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

pub(crate) fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

pub(crate) fn short_fingerprint(material: &str) -> String {
    let hash = blake3::hash(material.as_bytes());
    hash.to_hex().to_string().chars().take(16).collect()
}

pub(crate) fn now_unix() -> u64 {
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
