use crate::types::{CommandReducerSpec, CommandReduction};
use serde_json::Value;

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
        "mypy" if classify_mypy(argv) => {
            ("python_mypy", suite_packet_core::ToolOperationKind::Build)
        }
        "python" | "python3" if classify_python_module(argv, "mypy") => {
            ("python_mypy", suite_packet_core::ToolOperationKind::Build)
        }
        "pip" if classify_pip_list(argv) => (
            "python_pip_list",
            suite_packet_core::ToolOperationKind::Fetch,
        ),
        "pip" if classify_pip_outdated(argv) => (
            "python_pip_outdated",
            suite_packet_core::ToolOperationKind::Fetch,
        ),
        "uv" if classify_uv_pip_list(argv) => (
            "python_pip_list",
            suite_packet_core::ToolOperationKind::Fetch,
        ),
        "uv" if classify_uv_pip_outdated(argv) => (
            "python_pip_outdated",
            suite_packet_core::ToolOperationKind::Fetch,
        ),
        "python" | "python3" if classify_python_module_command(argv, "pip", "list") => (
            "python_pip_list",
            suite_packet_core::ToolOperationKind::Fetch,
        ),
        "python" | "python3" if classify_python_module_command(argv, "pip", "outdated") => (
            "python_pip_outdated",
            suite_packet_core::ToolOperationKind::Fetch,
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
        "python_mypy" => summarize_mypy(&combined, failed),
        "python_pip_list" => summarize_pip_list(stdout),
        "python_pip_outdated" => summarize_pip_outdated(stdout),
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
        compact_preview: match spec.canonical_kind.as_str() {
            "python_pytest" if failed => compact_pytest_failures(&combined),
            "python_ruff_check" if failed => compact_ruff_output(&combined),
            "python_mypy" if failed => compact_mypy_output(&combined),
            _ => String::new(),
        },
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

fn classify_python_module_command(argv: &[String], module: &str, subcommand: &str) -> bool {
    argv.len() >= 4
        && argv.get(1).is_some_and(|arg| arg == "-m")
        && argv.get(2).is_some_and(|arg| arg == module)
        && argv.get(3).is_some_and(|arg| arg == subcommand)
        && !contains_any(argv, &["--format=json"])
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

fn classify_mypy(argv: &[String]) -> bool {
    !contains_any(
        argv,
        &[
            "--junit-xml",
            "--html-report",
            "--xml-report",
            "--linecount-report",
            "--any-exprs-report",
            "--json-report",
        ],
    )
}

fn classify_pip_list(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "list") && !contains_any(argv, &["--outdated"])
}

fn classify_pip_outdated(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "list") && contains_any(argv, &["--outdated"])
}

fn classify_uv_pip_list(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "pip")
        && argv.get(2).is_some_and(|arg| arg == "list")
        && !contains_any(argv, &["--outdated"])
}

fn classify_uv_pip_outdated(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "pip")
        && argv.get(2).is_some_and(|arg| arg == "list")
        && contains_any(argv, &["--outdated"])
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

fn summarize_mypy(output: &str, failed: bool) -> String {
    let diagnostics = extract_mypy_diagnostics(output);
    if diagnostics.is_empty() {
        if output.contains("Success: no issues found") || !failed {
            return "mypy passed".to_string();
        }
        return first_nonempty_line(output).unwrap_or_else(|| "mypy failed".to_string());
    }
    let file_count = diagnostics
        .iter()
        .map(|diag| diag.0.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if let Some((path, _, code)) = diagnostics.first() {
        if let Some(code) = code {
            format!(
                "mypy: {} error(s) in {} file(s); first {} ({})",
                diagnostics.len(),
                file_count,
                path,
                code
            )
        } else {
            format!(
                "mypy: {} error(s) in {} file(s); first {}",
                diagnostics.len(),
                file_count,
                path
            )
        }
    } else {
        format!(
            "mypy: {} error(s) in {} file(s)",
            diagnostics.len(),
            file_count
        )
    }
}

fn summarize_pip_list(stdout: &str) -> String {
    if let Some((count, first)) = parse_pip_json(stdout) {
        if let Some(first) = first {
            return format!("pip list: {count} package(s); first {first}");
        }
        return format!("pip list: {count} package(s)");
    }
    let packages = parse_pip_table(stdout);
    if let Some(first) = packages.first() {
        format!("pip list: {} package(s); first {}", packages.len(), first)
    } else {
        "pip list returned 0 package(s)".to_string()
    }
}

fn summarize_pip_outdated(stdout: &str) -> String {
    if let Some((count, first)) = parse_pip_json(stdout) {
        if count == 0 {
            return "pip outdated: all packages up to date".to_string();
        }
        if let Some(first) = first {
            return format!("pip outdated: {count} package(s); first {first}");
        }
        return format!("pip outdated: {count} package(s)");
    }
    let packages = parse_pip_table(stdout);
    if packages.is_empty() {
        "pip outdated: all packages up to date".to_string()
    } else {
        format!(
            "pip outdated: {} package(s); first {}",
            packages.len(),
            packages[0]
        )
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

fn extract_mypy_diagnostics(output: &str) -> Vec<(String, usize, Option<String>)> {
    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Success:") || trimmed.starts_with("Found ") {
            continue;
        }
        let mut parts = trimmed.split(':');
        let Some(path) = parts.next() else {
            continue;
        };
        if !path.ends_with(".py") && !path.ends_with(".pyi") {
            continue;
        }
        let Some(line_no) = parts.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let remainder = parts.collect::<Vec<_>>().join(":");
        let code = remainder
            .split('[')
            .nth(1)
            .and_then(|value| value.split(']').next())
            .map(ToOwned::to_owned);
        diagnostics.push((path.to_string(), line_no, code));
    }
    diagnostics
}

fn parse_pip_json(stdout: &str) -> Option<(usize, Option<String>)> {
    let value = serde_json::from_str::<Value>(stdout.trim()).ok()?;
    let Value::Array(items) = value else {
        return None;
    };
    let first = items.first().and_then(|item| {
        let name = item.get("name").and_then(Value::as_str)?;
        let version = item
            .get("version")
            .or_else(|| item.get("latest_version"))
            .and_then(Value::as_str);
        Some(match version {
            Some(version) => format!("{name} ({version})"),
            None => name.to_string(),
        })
    });
    Some((items.len(), first))
}

fn parse_pip_table(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("Package ")
                && !line.starts_with("-----")
                && !line.starts_with('[')
        })
        .filter_map(|line| line.split_whitespace().next().map(ToOwned::to_owned))
        .collect()
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

fn compact_pytest_failures(output: &str) -> String {
    let mut failures = Vec::new();
    let mut in_failure = false;
    let mut current_name = String::new();
    let mut current_lines = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("FAILED ") {
            let name = trimmed.strip_prefix("FAILED ").unwrap_or(trimmed);
            let name = name.split(" - ").next().unwrap_or(name);
            failures.push(format!("FAIL {name}"));
        } else if trimmed.starts_with("___") && trimmed.contains("FAILED") {
            in_failure = false;
        } else if trimmed.starts_with("___") && trimmed.ends_with("___") {
            if !current_name.is_empty() && !current_lines.is_empty() {
                let preview = current_lines
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                failures.push(format!("FAIL {current_name}\n{preview}"));
            }
            in_failure = true;
            current_name = trimmed.replace('_', "").trim().to_string();
            current_lines.clear();
        } else if in_failure && !trimmed.is_empty() {
            current_lines.push(trimmed.to_string());
        }
    }
    if !current_name.is_empty() && !current_lines.is_empty() {
        let preview = current_lines
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        failures.push(format!("FAIL {current_name}\n{preview}"));
    }

    failures.join("\n\n")
}

fn compact_ruff_output(output: &str) -> String {
    let mut by_rule: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Found ") || trimmed.is_empty() {
            continue;
        }
        // Format: file.py:line:col: RULE message
        let parts: Vec<&str> = trimmed.splitn(4, ':').collect();
        if parts.len() >= 4 {
            let location = format!("{}:{}", parts[0], parts[1]);
            let rule_msg = parts[3].trim();
            let rule = rule_msg.split_whitespace().next().unwrap_or("");
            by_rule
                .entry(rule.to_string())
                .or_default()
                .push(location);
        }
    }
    if by_rule.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for (rule, locations) in &by_rule {
        let first = locations.first().map(String::as_str).unwrap_or("");
        lines.push(format!("{rule}: {}x (first: {first})", locations.len()));
    }
    lines.join("\n")
}

fn compact_mypy_output(output: &str) -> String {
    let mut by_file: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Success:") || trimmed.starts_with("Found ") {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(3, ':').collect();
        if parts.len() >= 3 && (parts[0].ends_with(".py") || parts[0].ends_with(".pyi")) {
            let file = parts[0].to_string();
            let msg = parts[2..].join(":").trim().to_string();
            by_file.entry(file).or_default().push(msg);
        }
    }
    if by_file.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for (file, errors) in &by_file {
        lines.push(format!("{file}: {} error(s)", errors.len()));
        for err in errors.iter().take(3) {
            lines.push(format!("  {err}"));
        }
        if errors.len() > 3 {
            lines.push(format!("  +{} more", errors.len() - 3));
        }
    }
    lines.join("\n")
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

    #[test]
    fn classify_python_supports_mypy_and_uv_pip() {
        let argv = vec!["mypy", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            classify_python_command("mypy src", &argv)
                .unwrap()
                .canonical_kind,
            "python_mypy"
        );

        let argv = vec!["uv", "pip", "list", "--outdated"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            classify_python_command("uv pip list --outdated", &argv)
                .unwrap()
                .canonical_kind,
            "python_pip_outdated"
        );
    }

    #[test]
    fn reduce_mypy_summarizes_first_error_code() {
        let argv = vec!["mypy", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("mypy src", &argv).unwrap();
        let output = "src/auth.py:12: error: Incompatible return value type  [return-value]\nsrc/auth.py:18: error: Name \"x\" is not defined  [name-defined]\n";
        let reduction = reduce_python_command(&spec, output, "", 1);
        assert_eq!(
            reduction.summary,
            "mypy: 2 error(s) in 1 file(s); first src/auth.py (return-value)"
        );
    }

    #[test]
    fn reduce_pip_outdated_summarizes_json_rows() {
        let argv = vec!["pip", "list", "--outdated"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_python_command("pip list --outdated", &argv).unwrap();
        let output = r#"[{"name":"pytest","version":"8.1.0","latest_version":"8.2.0"}]"#;
        let reduction = reduce_python_command(&spec, output, "", 0);
        assert_eq!(
            reduction.summary,
            "pip outdated: 1 package(s); first pytest (8.1.0)"
        );
    }
}
