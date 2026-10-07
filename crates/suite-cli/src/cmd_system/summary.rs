#![warn(clippy::needless_collect)]

use super::json::filter_json_schema;
use super::{analyze_logs, command_status_line, compact_for_log};

pub(super) fn summarize_command_output(output: &str, command: &str, success: bool) -> String {
    let mut rendered = vec![
        command_status_line(command, success),
        format!("   {} lines of output", output.lines().count()),
        String::new(),
    ];
    match detect_summary_output_type(output, command) {
        SummaryOutputType::Test => summarize_tests_for_command(output, &mut rendered),
        SummaryOutputType::Build => summarize_build_for_command(output, &mut rendered),
        SummaryOutputType::Json => summarize_json_for_command(output, &mut rendered),
        SummaryOutputType::Log => {
            let logs = analyze_logs(output).unwrap_or_else(|_| output.to_string());
            rendered.push(logs);
        }
        SummaryOutputType::List => summarize_list_for_command(output, &mut rendered),
        SummaryOutputType::Generic => summarize_generic_for_command(output, &mut rendered),
    }
    rendered.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryOutputType {
    Test,
    Build,
    Json,
    Log,
    List,
    Generic,
}

fn detect_summary_output_type(output: &str, command: &str) -> SummaryOutputType {
    let command = command.to_lowercase();
    let lower = output.to_lowercase();
    let trimmed = output.trim_start();
    if command.contains("test") || lower.contains("passed") && lower.contains("failed") {
        SummaryOutputType::Test
    } else if command.contains("build")
        || command.contains("compile")
        || lower.contains("compiling")
    {
        SummaryOutputType::Build
    } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
        SummaryOutputType::Json
    } else if lower.contains("error:")
        || lower.contains("warn:")
        || lower.contains("fatal")
        || lower.contains("panic")
    {
        SummaryOutputType::Log
    } else if output
        .lines()
        .all(|line| line.len() < 200 && line.split_whitespace().count() < 10)
    {
        SummaryOutputType::List
    } else {
        SummaryOutputType::Generic
    }
}

fn summarize_tests_for_command(output: &str, rendered: &mut Vec<String>) {
    let lower = output.to_lowercase();
    rendered.push("Test Results:".to_string());
    let passed = extract_labeled_number(&lower, "passed").unwrap_or(0);
    let failed = extract_labeled_number(&lower, "failed").unwrap_or(0);
    let skipped = extract_labeled_number(&lower, "skipped")
        .or_else(|| extract_labeled_number(&lower, "ignored"));
    rendered.push(format!("   [ok] {passed} passed"));
    if failed > 0 {
        rendered.push(format!("   [FAIL] {failed} failed"));
    }
    if let Some(skipped) = skipped {
        rendered.push(format!("   skip {skipped} skipped"));
    }
    for line in output
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            (lower.contains("failed") || lower.contains("failure")) && !lower.contains("0 failed")
        })
        .take(5)
    {
        rendered.push(format!("   {}", compact_for_log(line, 90)));
    }
}

fn summarize_build_for_command(output: &str, rendered: &mut Vec<String>) {
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut compiled = 0usize;
    let mut examples = Vec::new();
    for line in output.lines() {
        let lower = line.to_lowercase();
        if lower.contains("error") && !lower.contains("0 error") {
            errors += 1;
            if examples.len() < 5 {
                examples.push(line);
            }
        } else if lower.contains("warning") && !lower.contains("0 warning") {
            warnings += 1;
        } else if lower.contains("compiling") || lower.contains("compiled") {
            compiled += 1;
        }
    }
    rendered.push("Build Summary:".to_string());
    if compiled > 0 {
        rendered.push(format!("   {compiled} crates/files compiled"));
    }
    rendered.push(format!("   [error] {errors} errors"));
    rendered.push(format!("   [warn] {warnings} warnings"));
    for example in examples {
        rendered.push(format!("   {}", compact_for_log(example, 90)));
    }
}

fn summarize_json_for_command(output: &str, rendered: &mut Vec<String>) {
    rendered.push("JSON Summary:".to_string());
    match filter_json_schema(output, 3) {
        Ok(schema) => rendered.push(schema),
        Err(_) => summarize_generic_for_command(output, rendered),
    }
}

fn summarize_list_for_command(output: &str, rendered: &mut Vec<String>) {
    let lines = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    rendered.push(format!("List Output: {} entries", lines.len()));
    for line in lines.iter().take(10) {
        rendered.push(format!("   {}", compact_for_log(line, 100)));
    }
    if lines.len() > 10 {
        rendered.push(format!("   ... +{} more", lines.len() - 10));
    }
}

fn summarize_generic_for_command(output: &str, rendered: &mut Vec<String>) {
    rendered.push("Output Preview:".to_string());
    for line in output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(10)
    {
        rendered.push(format!("   {}", compact_for_log(line, 100)));
    }
}

fn extract_labeled_number(value: &str, label: &str) -> Option<usize> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| {
            if pair.get(1).copied() == Some(label) {
                pair.first()?.parse().ok()
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::summarize_command_output;

    #[test]
    fn summary_counts_lines_without_materializing_them() {
        let summary = summarize_command_output("alpha\nbeta\ngamma\n", "echo fixture", true);
        assert_eq!(summary.lines().nth(1), Some("   3 lines of output"));
    }
}
