use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_go_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let (canonical_kind, operation_kind) = match argv.first()?.as_str() {
        "go" => match argv.get(1)?.as_str() {
            "test" if !contains_any(argv, &["-json", "-x", "-work"]) => {
                ("go_test", suite_packet_core::ToolOperationKind::Test)
            }
            "build" if !contains_any(argv, &["-x", "-work", "-json"]) => {
                ("go_build", suite_packet_core::ToolOperationKind::Build)
            }
            "vet" if !contains_any(argv, &["-json"]) => {
                ("go_vet", suite_packet_core::ToolOperationKind::Build)
            }
            _ => return None,
        },
        "golangci-lint"
            if argv.get(1).is_some_and(|arg| arg == "run")
                && !contains_any(argv, &["--out-format", "--fix"]) =>
        {
            ("golangci_lint", suite_packet_core::ToolOperationKind::Build)
        }
        _ => return None,
    };

    let paths = argv
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && looks_like_path(arg))
        .cloned()
        .collect::<Vec<_>>();

    Some(CommandReducerSpec {
        family: "go".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.go.v2".to_string(),
        operation_kind,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("go", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths,
        equivalence_key: None,
    })
}

pub fn reduce_go_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let combined = format!("{stdout}\n{stderr}");
    let (diagnostic_count, diagnostic_paths) = extract_go_diagnostics(&combined);
    let summary = match spec.canonical_kind.as_str() {
        "go_test" => summarize_go_test(&combined, failed),
        "go_build" => summarize_go_build_like("go build", diagnostic_count, &diagnostic_paths, &combined, failed),
        "go_vet" => summarize_go_build_like("go vet", diagnostic_count, &diagnostic_paths, &combined, failed),
        "golangci_lint" => summarize_go_build_like(
            "golangci-lint",
            diagnostic_count,
            &diagnostic_paths,
            &combined,
            failed,
        ),
        _ => first_nonempty_line(&combined).unwrap_or_else(|| "go command completed".to_string()),
    };

    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        paths: merge_paths(&spec.paths, &diagnostic_paths),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "go_error".to_string()),
        error_message: failed.then(|| compact(&combined, 220)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

fn summarize_go_test(output: &str, failed: bool) -> String {
    let passed = output
        .lines()
        .filter(|line| line.starts_with("ok\t"))
        .count();
    let failed_pkgs = output
        .lines()
        .filter(|line| line.starts_with("FAIL\t"))
        .count();
    let failed_tests = output
        .lines()
        .filter(|line| line.trim_start().starts_with("--- FAIL:"))
        .count();
    if failed || failed_pkgs > 0 {
        if failed_tests > 0 {
            format!(
                "go test: {passed} pkgs passed, {failed_pkgs} failed; {failed_tests} tests failed"
            )
        } else {
            format!("go test: {passed} pkgs passed, {failed_pkgs} failed")
        }
    } else if passed > 0 {
        format!("go test passed ({passed} package(s))")
    } else {
        first_nonempty_line(output).unwrap_or_else(|| "go test completed".to_string())
    }
}

fn summarize_go_build_like(
    label: &str,
    diagnostic_count: usize,
    diagnostic_paths: &[String],
    output: &str,
    failed: bool,
) -> String {
    if failed {
        if diagnostic_count > 0 {
            let lead = diagnostic_paths.first().cloned().unwrap_or_default();
            format!(
                "{label}: {diagnostic_count} diagnostics in {lead}",
            )
        } else {
            first_nonempty_line(output).unwrap_or_else(|| format!("{label} failed"))
        }
    } else {
        format!("{label} passed")
    }
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.starts_with(&format!("{denied}=")))
    })
}

fn looks_like_path(value: &str) -> bool {
    value.ends_with(".go") || value == "." || value == "./..." || value.contains('/')
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
    if compact.len() <= limit {
        compact
    } else {
        format!("{}...", &compact[..limit.saturating_sub(3)])
    }
}

fn extract_go_diagnostics(output: &str) -> (usize, Vec<String>) {
    let mut count = 0usize;
    let mut paths = Vec::new();
    for line in output.lines() {
        let Some((path, rest)) = line.split_once(':') else {
            continue;
        };
        if !path.ends_with(".go") {
            continue;
        }
        let mut parts = rest.split(':');
        let line_no = parts.next().unwrap_or_default();
        if !line_no.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        count += 1;
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.to_string());
        }
    }
    (count, paths)
}

fn merge_paths(base: &[String], extra: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    for path in extra {
        if !merged.iter().any(|existing| existing == path) {
            merged.push(path.clone());
        }
    }
    merged
}

fn fingerprint(family: &str, kind: &str, argv: &[String]) -> String {
    format!("{family}:{kind}:{}", argv.join("\u{1f}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_go_declines_json_variants() {
        let argv = vec!["go", "test", "-json"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_go_command("go test -json", &argv).is_none());
    }

    #[test]
    fn reduce_go_test_summarizes_packages() {
        let argv = vec!["go", "test", "./..."]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_go_command("go test ./...", &argv).unwrap();
        let output = "ok\tgithub.com/acme/core\t0.113s\nFAIL\tgithub.com/acme/api\t0.222s\n";
        let reduction = reduce_go_command(&spec, output, "", 1);
        assert_eq!(reduction.summary, "go test: 1 pkgs passed, 1 failed");
    }

    #[test]
    fn reduce_golangci_lint_mentions_path_and_count() {
        let argv = vec!["golangci-lint", "run"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_go_command("golangci-lint run", &argv).unwrap();
        let stderr = "pkg/service/service.go:17:2: undefined: missingHelper (typecheck)\npkg/service/service.go:22:9: Error return value of `w.Write` is not checked (errcheck)\n";
        let reduction = reduce_go_command(&spec, "", stderr, 1);
        assert_eq!(
            reduction.summary,
            "golangci-lint: 2 diagnostics in pkg/service/service.go"
        );
        assert_eq!(reduction.paths, vec!["pkg/service/service.go"]);
    }
}
