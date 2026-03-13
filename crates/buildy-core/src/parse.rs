use std::sync::OnceLock;

use regex::Regex;

use crate::types::BuildDiagnostic;

pub(crate) fn parse_diagnostics(log_text: &str) -> Vec<BuildDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut pending_rust: Option<(String, Option<String>, String)> = None;

    for line in log_text.lines() {
        if let Some(captures) = colon_diag_re().captures(line) {
            let file = captures
                .name("file")
                .map(|m| normalize_path(m.as_str()))
                .unwrap_or_default();
            let line_num = captures
                .name("line")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let col_num = captures
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("warning"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();

            diagnostics.push(build_diagnostic(
                file, line_num, col_num, severity, code, message,
            ));
            pending_rust = None;
            continue;
        }

        if let Some(captures) = msvc_diag_re().captures(line) {
            let file = captures
                .name("file")
                .map(|m| normalize_path(m.as_str()))
                .unwrap_or_default();
            let line_num = captures
                .name("line")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let col_num = captures
                .name("col")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("warning"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();

            diagnostics.push(build_diagnostic(
                file, line_num, col_num, severity, code, message,
            ));
            pending_rust = None;
            continue;
        }

        if let Some(captures) = rust_header_re().captures(line) {
            let severity = normalize_severity(
                captures
                    .name("severity")
                    .map(|m| m.as_str())
                    .unwrap_or("error"),
            );
            let code = captures.name("code").map(|m| m.as_str().to_string());
            let message = captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            pending_rust = Some((severity, code, message));
            continue;
        }

        if let Some((severity, code, message)) = pending_rust.clone() {
            if let Some(captures) = rust_location_re().captures(line) {
                let file = captures
                    .name("file")
                    .map(|m| normalize_path(m.as_str()))
                    .unwrap_or_default();
                let line_num = captures
                    .name("line")
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .unwrap_or(0);
                let col_num = captures
                    .name("col")
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .unwrap_or(0);
                diagnostics.push(build_diagnostic(
                    file, line_num, col_num, severity, code, message,
                ));
                pending_rust = None;
            }
        }
    }

    diagnostics
}

pub(crate) fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn build_diagnostic(
    file: String,
    line: u32,
    column: u32,
    severity: String,
    code: Option<String>,
    message: String,
) -> BuildDiagnostic {
    let fingerprint_material = format!(
        "{}|{}|{}|{}|{}|{}",
        file,
        line,
        column,
        severity,
        code.clone().unwrap_or_default(),
        normalize_message_for_group(&message)
    );

    BuildDiagnostic {
        file,
        line,
        column,
        severity,
        code,
        message,
        fingerprint: short_hash(&fingerprint_material),
    }
}

pub(crate) fn normalize_severity(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "error" | "err" => "error".to_string(),
        "warning" | "warn" => "warning".to_string(),
        "note" => "note".to_string(),
        "info" | "information" => "info".to_string(),
        _ => "warning".to_string(),
    }
}

pub(crate) fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 4,
        "warning" => 3,
        "note" => 2,
        "info" => 1,
        _ => 0,
    }
}

pub(crate) fn normalize_message_for_group(message: &str) -> String {
    numeric_re()
        .replace_all(&message.to_ascii_lowercase(), "#")
        .trim()
        .to_string()
}

pub(crate) fn short_hash(input: &str) -> String {
    blake3::hash(input.as_bytes())
        .to_hex()
        .to_string()
        .chars()
        .take(16)
        .collect()
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn colon_diag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+):(?P<line>\d+):(?P<col>\d+):\s*(?P<severity>error|warning|note|info):\s*(?P<message>.*?)(?:\s+\[(?P<code>[^\]]+)\])?$",
        )
        .unwrap()
    })
}

fn msvc_diag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<severity>error|warning|note|info)\s*(?P<code>[A-Za-z0-9_]+)?:?\s*(?P<message>.*)$",
        )
        .unwrap()
    })
}

fn rust_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<severity>error|warning|note)(?:\[(?P<code>[^\]]+)\])?:\s*(?P<message>.*)$",
        )
        .unwrap()
    })
}

fn rust_location_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*-->\s+(?P<file>[A-Za-z0-9_./\\-]+\.[A-Za-z0-9_+-]+):(?P<line>\d+):(?P<col>\d+)",
        )
        .unwrap()
    })
}

fn numeric_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+").unwrap())
}
