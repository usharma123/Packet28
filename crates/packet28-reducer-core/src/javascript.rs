use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_javascript_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let (canonical_kind, operation_kind) = match argv.first()?.as_str() {
        "npm" if classify_package_manager(argv, "test", None) => (
            "javascript_test",
            suite_packet_core::ToolOperationKind::Test,
        ),
        "pnpm" if classify_package_manager(argv, "test", None) => (
            "javascript_test",
            suite_packet_core::ToolOperationKind::Test,
        ),
        "yarn" if classify_yarn(argv, "test") => (
            "javascript_test",
            suite_packet_core::ToolOperationKind::Test,
        ),
        "npm" if classify_package_manager(argv, "run", Some("lint")) => (
            "javascript_lint",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "pnpm" if classify_package_manager(argv, "lint", None) => (
            "javascript_lint",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "yarn" if classify_yarn(argv, "lint") => (
            "javascript_lint",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "npx" if classify_npx_tsc(argv) => (
            "javascript_tsc",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "tsc" if classify_tsc(argv) => (
            "javascript_tsc",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "eslint" if classify_eslint(argv) => (
            "javascript_eslint",
            suite_packet_core::ToolOperationKind::Build,
        ),
        "vitest" if classify_vitest(argv) => (
            "javascript_vitest",
            suite_packet_core::ToolOperationKind::Test,
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
        family: "javascript".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.javascript.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("javascript", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths,
        equivalence_key: None,
    })
}

pub fn reduce_javascript_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let combined = format!("{stdout}\n{stderr}");
    let summary = match spec.canonical_kind.as_str() {
        "javascript_test" | "javascript_vitest" => summarize_js_tests(&combined, failed),
        "javascript_lint" | "javascript_eslint" => summarize_js_lint(&combined, failed),
        "javascript_tsc" => summarize_tsc(&combined, failed),
        _ => first_nonempty_line(&combined)
            .unwrap_or_else(|| "javascript command completed".to_string()),
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
        error_class: failed.then(|| "javascript_error".to_string()),
        error_message: failed.then(|| compact(&combined, 220)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn classify_package_manager(argv: &[String], first: &str, second: Option<&str>) -> bool {
    if !matches!(argv.get(1), Some(arg) if arg == first) {
        return false;
    }
    if let Some(second) = second {
        argv.get(2).is_some_and(|arg| arg == second)
    } else {
        !contains_any(argv, &["--json"])
    }
}

fn classify_yarn(argv: &[String], script: &str) -> bool {
    argv.get(1).is_some_and(|arg| arg == script) && !contains_any(argv, &["--json"])
}

fn classify_npx_tsc(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|arg| arg == "tsc") && classify_tsc(&argv[1..])
}

fn classify_tsc(argv: &[String]) -> bool {
    contains_any(argv, &["--noEmit"]) && !contains_any(argv, &["--pretty", "--watch", "--build"])
}

fn classify_eslint(argv: &[String]) -> bool {
    !contains_any(argv, &["--format", "-f", "--fix", "--output-file"])
}

fn classify_vitest(argv: &[String]) -> bool {
    !contains_any(argv, &["--reporter", "--ui", "--watch"])
}

fn summarize_js_tests(output: &str, failed: bool) -> String {
    if let Some((passed, failed_count)) = parse_test_result(output) {
        if failed || failed_count > 0 {
            if let Some(failing_file) = extract_vitest_failure(output) {
                return format!(
                    "vitest: {failed_count} failed, {passed} passed; {failing_file}"
                );
            }
            return format!("vitest: {failed_count} failed, {passed} passed");
        }
        return format!("javascript tests passed ({passed} tests)");
    }
    if failed {
        first_nonempty_line(output).unwrap_or_else(|| "javascript tests failed".to_string())
    } else {
        first_nonempty_line(output).unwrap_or_else(|| "javascript tests completed".to_string())
    }
}

fn summarize_js_lint(output: &str, failed: bool) -> String {
    let diagnostics = parse_eslint_problem_count(output).unwrap_or_else(|| {
        nonempty_lines(output)
            .into_iter()
            .filter(|line| !line.starts_with('(') && !line.starts_with("Done"))
            .count()
    });
    if failed {
        if diagnostics > 0 {
            if let Some(path) = extract_js_diagnostic_path(output) {
                if let Some(rule) = extract_eslint_rule(output) {
                    format!("eslint: {diagnostics} diagnostics in {path} ({rule})")
                } else {
                    format!("eslint: {diagnostics} diagnostics in {path}")
                }
            } else {
                format!("eslint: {diagnostics} diagnostic line(s)")
            }
        } else {
            first_nonempty_line(output).unwrap_or_else(|| "javascript lint failed".to_string())
        }
    } else {
        first_nonempty_line(output).unwrap_or_else(|| "javascript lint passed".to_string())
    }
}

fn summarize_tsc(output: &str, failed: bool) -> String {
    let errors = output
        .lines()
        .filter(|line| line.contains("error TS") || line.trim_start().starts_with("error "))
        .count();
    if failed {
        if let Some((path, code)) = extract_tsc_error(output) {
            format!("tsc: {errors} errors in {path} ({code})")
        } else {
            format!("tsc: {errors} error(s)")
        }
    } else {
        "tsc passed".to_string()
    }
}

fn parse_test_result(output: &str) -> Option<(usize, usize)> {
    for line in output.lines().filter(|line| line.contains("Tests")) {
        if line.contains("passed") || line.contains("failed") {
            let passed = extract_count(line, "passed").unwrap_or(0);
            let failed = extract_count(line, "failed").unwrap_or(0);
            if passed > 0 || failed > 0 {
                return Some((passed, failed));
            }
        }
    }
    for line in output.lines() {
        if line.contains("passed") || line.contains("failed") {
            let passed = extract_count(line, "passed").unwrap_or(0);
            let failed = extract_count(line, "failed").unwrap_or(0);
            if passed > 0 || failed > 0 {
                return Some((passed, failed));
            }
        }
    }
    None
}

fn parse_eslint_problem_count(output: &str) -> Option<usize> {
    output.lines().find_map(|line| {
        let normalized = line.replace(['✖', '(', ')', ','], " ");
        let tokens = normalized.split_whitespace().collect::<Vec<_>>();
        tokens.windows(2).find_map(|pair| match pair {
            [count, label] if *label == "problems" => count.parse::<usize>().ok(),
            _ => None,
        })
    })
}

fn extract_count(line: &str, label: &str) -> Option<usize> {
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
    value.ends_with(".js")
        || value.ends_with(".ts")
        || value.ends_with(".tsx")
        || value.ends_with(".jsx")
        || value.ends_with("package.json")
        || value.contains('/')
        || value == "."
        || value == "src"
        || value == "test"
        || value == "tests"
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

fn extract_js_diagnostic_path(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') && !trimmed.starts_with('.') {
            return None;
        }
        let path = trimmed.split_whitespace().next()?;
        if path.ends_with(".js")
            || path.ends_with(".jsx")
            || path.ends_with(".ts")
            || path.ends_with(".tsx")
        {
            Some(path.to_string())
        } else {
            None
        }
    })
}

fn extract_eslint_rule(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        if !trimmed
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit())
        {
            return None;
        }
        trimmed.split_whitespace().last().map(ToOwned::to_owned)
    })
}

fn extract_tsc_error(output: &str) -> Option<(String, String)> {
    output.lines().find_map(|line| {
        let (path, rest) = line.split_once(":")?;
        if !path.ends_with(".ts") && !path.ends_with(".tsx") && !path.ends_with(".js") {
            return None;
        }
        let code = rest
            .split_whitespace()
            .find(|token| token.starts_with("TS"))
            .map(|token| token.trim_end_matches(':').to_string())?;
        Some((path.to_string(), code))
    })
}

fn extract_vitest_failure(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("FAIL  ")
            .and_then(|value| value.split(" > ").next())
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
    fn classify_javascript_declines_json_and_fix_variants() {
        let argv = vec!["eslint", "--format", "json", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_javascript_command("eslint --format json src", &argv).is_none());

        let argv = vec!["eslint", "--fix", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_javascript_command("eslint --fix src", &argv).is_none());
    }

    #[test]
    fn classify_javascript_accepts_tsc() {
        let argv = vec!["npx", "tsc", "--noEmit"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_javascript_command("npx tsc --noEmit", &argv).unwrap();
        assert_eq!(spec.canonical_kind, "javascript_tsc");
    }

    #[test]
    fn reduce_vitest_summarizes_results() {
        let argv = vec!["vitest", "run"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_javascript_command("vitest run", &argv).unwrap();
        let reduction = reduce_javascript_command(
            &spec,
            "Test Files  2 passed | 1 failed (3)\nTests  7 passed | 1 failed (8)\n",
            "",
            1,
        );
        assert_eq!(reduction.summary, "vitest: 1 failed, 7 passed");
    }

    #[test]
    fn reduce_tsc_summarizes_errors() {
        let argv = vec!["tsc", "--noEmit"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_javascript_command("tsc --noEmit", &argv).unwrap();
        let reduction = reduce_javascript_command(
            &spec,
            "",
            "src/index.ts:4:1 - error TS2322: Type 'string' is not assignable\n",
            2,
        );
        assert_eq!(reduction.summary, "tsc: 1 errors in src/index.ts (TS2322)");
    }

    #[test]
    fn reduce_eslint_uses_problem_count() {
        let argv = vec!["eslint", "src"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_javascript_command("eslint src", &argv).unwrap();
        let reduction = reduce_javascript_command(
            &spec,
            "/workspace/src/app.ts\n  4:7  error  bad\n\n✖ 2 problems (2 errors, 0 warnings)\n",
            "",
            1,
        );
        assert_eq!(
            reduction.summary,
            "eslint: 2 diagnostics in /workspace/src/app.ts (bad)"
        );
    }
}
