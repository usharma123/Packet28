use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_python_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let (canonical_kind, operation_kind) = match argv.first()?.as_str() {
        "pytest" if classify_pytest(argv) => {
            ("python_pytest", suite_packet_core::ToolOperationKind::Test)
        }
        "python" | "python3" if classify_python_module(argv, "pytest") => {
            ("python_pytest", suite_packet_core::ToolOperationKind::Test)
        }
        "ruff" if classify_ruff_check(argv) => (
            "python_ruff_check",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "ruff" if classify_ruff_format_check(argv) => (
            "python_ruff_format",
            suite_packet_core::ToolOperationKind::Build,
        ),
        _ => return None,
    };

    let paths = argv
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && looks_like_path(arg))
        .cloned()
        .collect::<Vec<_>>();

    Some(CommandReducerSpec {
        family: "python".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.python.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("python", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths,
        equivalence_key: None,
    })
}

pub fn reduce_python_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let combined = format!("{stdout}\n{stderr}");
    let summary = match spec.canonical_kind.as_str() {
        "python_pytest" => summarize_pytest(&combined, failed),
        "python_ruff_check" => summarize_ruff_check(&combined, failed),
        "python_ruff_format" => summarize_ruff_format(&combined, failed),
        _ => {
            first_nonempty_line(&combined).unwrap_or_else(|| "python command completed".to_string())
        }
    };

    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        paths: spec.paths.clone(),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "python_error".to_string()),
        error_message: failed.then(|| compact(&combined, 220)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn classify_pytest(argv: &[String]) -> bool {
    !contains_any(
        argv,
        &[
            "--json-report",
            "--junitxml",
            "--collect-only",
            "--fixtures",
        ],
    )
}

fn classify_python_module(argv: &[String], module: &str) -> bool {
    argv.len() >= 3
        && argv.get(1).is_some_and(|arg| arg == "-m")
        && argv.get(2).is_some_and(|arg| arg == module)
}

fn classify_ruff_check(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "check")
        && !contains_any(argv, &["--output-format", "--format", "--fix"])
}

fn classify_ruff_format_check(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "format")
        && contains_any(argv, &["--check"])
        && !contains_any(argv, &["--diff"])
}

fn summarize_pytest(output: &str, failed: bool) -> String {
    if let Some((passed, failed_count)) = parse_pytest_result(output) {
        if failed || failed_count > 0 {
            if let Some(failed_test) = extract_pytest_failed_test(output) {
                return format!(
                    "pytest: {failed_count} failed, {passed} passed; first {failed_test}"
                );
            }
            return format!("pytest: {failed_count} failed, {passed} passed");
        }
        return format!("pytest passed ({passed} tests)");
    }
    if failed {
        first_nonempty_line(output).unwrap_or_else(|| "pytest failed".to_string())
    } else {
        first_nonempty_line(output).unwrap_or_else(|| "pytest completed".to_string())
    }
}

fn summarize_ruff_check(output: &str, failed: bool) -> String {
    let lines = nonempty_lines(output);
    if failed {
        let diagnostics = lines
            .iter()
            .filter(|line| !line.starts_with("Found"))
            .count();
        if diagnostics > 0 {
            if let Some(path) = extract_python_diagnostic_path(output) {
                if let Some(code) = extract_python_diagnostic_code(output) {
                    format!("ruff: {diagnostics} diagnostics in {path} ({code})")
                } else {
                    format!("ruff: {diagnostics} diagnostics in {path}")
                }
            } else {
                format!("ruff: {diagnostics} diagnostic line(s)")
            }
        } else {
            first_nonempty_line(output).unwrap_or_else(|| "ruff check failed".to_string())
        }
    } else if output.contains("All checks passed") {
        "ruff check passed".to_string()
    } else {
        format!(
            "ruff check completed with {} non-empty line(s)",
            lines.len()
        )
    }
}

fn summarize_ruff_format(output: &str, failed: bool) -> String {
    if failed {
        if output.contains("would be reformatted") {
            let files = output
                .lines()
                .filter(|line| line.contains("would be reformatted"))
                .count();
            return format!("ruff format --check would reformat {files} file(s)");
        }
        return first_nonempty_line(output)
            .unwrap_or_else(|| "ruff format --check failed".to_string());
    }
    if output.contains("files already formatted") {
        "ruff format --check passed".to_string()
    } else {
        first_nonempty_line(output).unwrap_or_else(|| "ruff format --check completed".to_string())
    }
}

fn parse_pytest_result(output: &str) -> Option<(usize, usize)> {
    let summary_line = output
        .lines()
        .find(|line| line.contains(" passed") || line.contains(" failed"))?;
    let passed = extract_result_count(summary_line, "passed").unwrap_or(0);
    let failed = extract_result_count(summary_line, "failed").unwrap_or(0);
    Some((passed, failed))
}

fn extract_result_count(line: &str, label: &str) -> Option<usize> {
    let cleaned = line.replace(['=', ',', '|', ';'], " ");
    let tokens = cleaned.split_whitespace().collect::<Vec<_>>();
    tokens.windows(2).find_map(|pair| match pair {
        [count, found_label] if *found_label == label => count.parse::<usize>().ok(),
        _ => None,
    })
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.starts_with(&format!("{denied}=")))
    })
}

fn looks_like_path(value: &str) -> bool {
    value.ends_with(".py")
        || value.ends_with(".pyi")
        || value.ends_with("pyproject.toml")
        || value.contains('/')
        || value.starts_with("tests")
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn nonempty_lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn compact(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= limit {
        compact
    } else {
        format!("{}...", &compact[..limit.saturating_sub(3)])
    }
}

fn extract_python_diagnostic_path(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (path, rest) = line.split_once(':')?;
        if !path.ends_with(".py") && !path.ends_with(".pyi") {
            return None;
        }
        let line_no = rest.split(':').next().unwrap_or_default();
        if !line_no.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        Some(path.to_string())
    })
}

fn extract_pytest_failed_test(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("FAILED ")
            .and_then(|rest| rest.split(" - ").next())
            .map(ToOwned::to_owned)
    })
}

fn extract_python_diagnostic_code(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut parts = line.split(':');
        let path = parts.next()?;
        if !path.ends_with(".py") && !path.ends_with(".pyi") {
            return None;
        }
        let line_no = parts.next()?;
        let col_no = parts.next()?;
        if !line_no.chars().all(|ch| ch.is_ascii_digit())
            || !col_no.chars().all(|ch| ch.is_ascii_digit())
        {
            return None;
        }
        parts
            .next()?
            .split_whitespace()
            .next()
            .map(ToOwned::to_owned)
    })
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_python_declines_json_pytest() {
        let argv = vec!["pytest", "--json-report"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_python_command("pytest --json-report", &argv).is_none());
    }

    #[test]
    fn classify_python_accepts_python_module_pytest() {
        let argv = vec!["python3", "-m", "pytest", "tests"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("python3 -m pytest tests", &argv).unwrap();
        assert_eq!(spec.canonical_kind, "python_pytest");
    }

    #[test]
    fn reduce_pytest_summarizes_results() {
        let argv = vec!["pytest"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("pytest", &argv).unwrap();
        let reduction = reduce_python_command(
            &spec,
            "================== 4 passed, 1 failed in 0.42s ==================\n",
            "",
            1,
        );
        assert_eq!(reduction.summary, "pytest: 1 failed, 4 passed");
    }

    #[test]
    fn reduce_ruff_format_summarizes_reformat() {
        let argv = vec!["ruff", "format", "--check", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("ruff format --check src", &argv).unwrap();
        let reduction = reduce_python_command(
            &spec,
            "Would reformat: src/demo.py\n1 file would be reformatted\n",
            "",
            1,
        );
        assert_eq!(
            reduction.summary,
            "ruff format --check would reformat 1 file(s)"
        );
    }

    #[test]
    fn reduce_ruff_check_mentions_primary_path() {
        let argv = vec!["ruff", "check", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("ruff check src", &argv).unwrap();
        let reduction = reduce_python_command(
            &spec,
            "src/demo.py:4:5: F841 Local variable `unused` is assigned to but never used\nsrc/demo.py:8:1: E402 Module level import not at top of file\nFound 2 errors.\n",
            "",
            1,
        );
        assert_eq!(
            reduction.summary,
            "ruff: 2 diagnostics in src/demo.py (F841)"
        );
    }
}
