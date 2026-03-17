use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_rust_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    if argv.first()?.as_str() != "cargo" {
        return None;
    }
    let subcommand = argv.get(1)?.as_str();
    let canonical_kind = match subcommand {
        "check" => "rust_check",
        "build" => "rust_build",
        "test" => "rust_test",
        "nextest" if argv.get(2).is_some_and(|arg| arg == "run") => "rust_test",
        "clippy" => "rust_clippy",
        _ => return None,
    };
    if contains_any(argv, &["--message-format", "--timings", "--json"]) {
        return None;
    }
    let operation_kind = if canonical_kind == "rust_test" {
        suite_packet_core::ToolOperationKind::Test
    } else {
        suite_packet_core::ToolOperationKind::Build
    };
    Some(CommandReducerSpec {
        family: "rust".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.rust.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("rust", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths: Vec::new(),
        equivalence_key: None,
    })
}

pub fn reduce_rust_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let combined = format!("{stdout}\n{stderr}");
    let canonical_kind_str = spec.canonical_kind.as_str();
    let diagnostics = parse_rust_diagnostics(&combined);
    let summary = match spec.canonical_kind.as_str() {
        "rust_test" => {
            if let Some(result) = parse_cargo_test_result(&combined) {
                if failed || result.failed > 0 {
                    format!(
                        "cargo test reported {} passed and {} failed",
                        result.passed, result.failed
                    )
                } else {
                    format!("cargo test passed ({} tests)", result.passed)
                }
            } else if failed {
                format!(
                    "cargo test failed with {} failing line(s)",
                    diagnostics.failed_markers
                )
            } else {
                first_nonempty_line(stdout).unwrap_or_else(|| "cargo test passed".to_string())
            }
        }
        _ => {
            if failed {
                format!(
                    "{} failed with {} error(s) and {} warning(s) across {} file(s)",
                    spec.argv[0..2].join(" "),
                    diagnostics.error_lines,
                    diagnostics.warning_lines,
                    diagnostics.paths.len()
                )
            } else {
                first_nonempty_line(stdout)
                    .or_else(|| first_nonempty_line(stderr))
                    .unwrap_or_else(|| format!("{} completed", spec.argv[0..2].join(" ")))
            }
        }
    };
    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        compact_preview: if canonical_kind_str == "rust_test" && failed {
            compact_cargo_test_failures(&combined)
        } else if canonical_kind_str == "rust_clippy" {
            compact_clippy_output(&combined)
        } else {
            String::new()
        },
        paths: diagnostics.paths,
        regions: diagnostics.regions,
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "rust_error".to_string()),
        error_message: failed.then(|| compact(&combined, 220)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.starts_with(&format!("{denied}=")))
    })
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn compact(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = compact.chars().count();
    if char_count <= limit {
        compact
    } else if limit <= 3 {
        "...".to_string()
    } else {
        let shortened = compact
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
}

#[derive(Default)]
struct RustDiagnostics {
    error_lines: usize,
    warning_lines: usize,
    failed_markers: usize,
    paths: Vec<String>,
    regions: Vec<String>,
}

#[derive(Default)]
struct CargoTestResult {
    passed: usize,
    failed: usize,
}

fn parse_rust_diagnostics(output: &str) -> RustDiagnostics {
    let mut diagnostics = RustDiagnostics::default();
    let mut seen_paths = std::collections::BTreeSet::new();
    let mut seen_regions = std::collections::BTreeSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("error") {
            diagnostics.error_lines += 1;
        }
        if trimmed.starts_with("warning") {
            diagnostics.warning_lines += 1;
        }
        if trimmed.contains("FAILED") {
            diagnostics.failed_markers += 1;
        }
        if let Some((path, region)) = parse_rust_location(trimmed) {
            if seen_paths.insert(path.clone()) {
                diagnostics.paths.push(path);
            }
            if let Some(region) = region {
                if seen_regions.insert(region.clone()) {
                    diagnostics.regions.push(region);
                }
            }
        }
    }

    diagnostics
}

fn parse_rust_location(line: &str) -> Option<(String, Option<String>)> {
    let target = line
        .strip_prefix("--> ")
        .or_else(|| line.strip_prefix("::: "))
        .unwrap_or(line);
    let mut segments = target.split(':');
    let path = segments.next()?.trim();
    if !looks_like_rust_path(path) {
        return None;
    }
    let line_no = segments
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok());
    let region = line_no.map(|line_no| format!("{path}:{line_no}-{line_no}"));
    Some((path.to_string(), region))
}

fn looks_like_rust_path(path: &str) -> bool {
    path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
}

fn parse_cargo_test_result(output: &str) -> Option<CargoTestResult> {
    let mut matched = false;
    let mut passed = 0usize;
    let mut failed = 0usize;
    for line in output
        .lines()
        .filter(|line| line.trim_start().starts_with("test result:"))
    {
        matched = true;
        passed += extract_result_count(line.trim(), "passed").unwrap_or(0);
        failed += extract_result_count(line.trim(), "failed").unwrap_or(0);
    }
    matched.then_some(CargoTestResult { passed, failed })
}

fn extract_result_count(line: &str, label: &str) -> Option<usize> {
    line.split(';')
        .map(str::trim)
        .find_map(|segment| segment.strip_suffix(label))
        .and_then(|prefix| prefix.split_whitespace().last())
        .and_then(|value| value.parse::<usize>().ok())
}

fn compact_cargo_test_failures(output: &str) -> String {
    let mut failures = Vec::new();
    let mut in_failure = false;
    let mut current_test = String::new();
    let mut current_output = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----") {
            in_failure = true;
            current_test = trimmed
                .trim_start_matches("---- ")
                .trim_end_matches(" stdout ----")
                .to_string();
            current_output.clear();
            continue;
        }
        if in_failure {
            if trimmed.starts_with("---- ")
                || trimmed == "failures:"
                || (trimmed.is_empty() && current_output.len() > 5)
            {
                if !current_test.is_empty() {
                    let preview = current_output
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n");
                    failures.push(format!("FAIL {current_test}\n{preview}"));
                }
                in_failure = false;
                current_test.clear();
                current_output.clear();
            } else {
                current_output.push(trimmed.to_string());
            }
        }
    }
    if !current_test.is_empty() && !current_output.is_empty() {
        let preview = current_output
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        failures.push(format!("FAIL {current_test}\n{preview}"));
    }

    failures.join("\n\n")
}

fn compact_clippy_output(output: &str) -> String {
    let mut by_rule: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut first_example: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("warning: ") {
            if let Some(rule) = extract_clippy_rule(trimmed) {
                *by_rule.entry(rule.clone()).or_insert(0) += 1;
                first_example
                    .entry(rule)
                    .or_insert_with(|| rest.to_string());
            }
        }
    }

    if by_rule.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for (rule, count) in &by_rule {
        let example = first_example.get(rule).map(String::as_str).unwrap_or("");
        lines.push(format!("{rule}: {count}x — {example}"));
    }
    lines.join("\n")
}

fn extract_clippy_rule(line: &str) -> Option<String> {
    // warning: ... #[warn(clippy::rule_name)] ...
    let start = line.find("#[warn(")? + 7;
    let end = line[start..].find(")]")? + start;
    Some(line[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rust_declines_json_shaping() {
        let argv = vec!["cargo", "test", "--message-format=json"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_rust_command("cargo test --message-format=json", &argv).is_none());
    }

    #[test]
    fn reduce_rust_test_extracts_result_summary() {
        let argv = vec!["cargo", "test"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_rust_command("cargo test", &argv).unwrap();
        let stdout = "running 2 tests\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let reduction = reduce_rust_command(&spec, stdout, "", 0);
        assert_eq!(reduction.summary, "cargo test passed (2 tests)");
    }

    #[test]
    fn reduce_rust_build_extracts_paths_and_regions() {
        let argv = vec!["cargo", "build"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_rust_command("cargo build", &argv).unwrap();
        let stderr = "error[E0425]: cannot find value `x` in this scope\n  --> src/main.rs:12:5\nwarning: unused variable\n";
        let reduction = reduce_rust_command(&spec, "", stderr, 101);
        assert_eq!(
            reduction.summary,
            "cargo build failed with 1 error(s) and 1 warning(s) across 1 file(s)"
        );
        assert_eq!(reduction.paths, vec!["src/main.rs".to_string()]);
        assert_eq!(reduction.regions, vec!["src/main.rs:12-12".to_string()]);
    }
}
