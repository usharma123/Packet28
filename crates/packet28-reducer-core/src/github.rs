use crate::types::{CommandReducerSpec, CommandReduction};
use serde_json::Value;

pub fn classify_github_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    if argv.first()?.as_str() != "gh" {
        return None;
    }
    if contains_any(
        argv,
        &[
            "--json",
            "--jq",
            "--template",
            "--web",
            "--comments",
            "--patch",
            "--verbose",
        ],
    ) {
        return None;
    }
    let group = argv.get(1)?.as_str();
    let action = argv.get(2)?.as_str();
    let canonical_kind = match (group, action) {
        ("pr", "list") => "gh_pr_list",
        ("pr", "view") => "gh_pr_view",
        ("pr", "diff") => "gh_pr_diff",
        ("pr", "checks") => "gh_pr_checks",
        ("issue", "list") => "gh_issue_list",
        ("issue", "view") => "gh_issue_view",
        ("run", "list") => "gh_run_list",
        ("run", "view") => "gh_run_view",
        ("api", _) if argv.get(1).is_some_and(|value| value == "api") => "gh_api",
        _ => return None,
    };
    Some(CommandReducerSpec {
        family: "github".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.github.v2".to_string(),
        operation_kind: suite_packet_core::ToolOperationKind::Fetch,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("github", canonical_kind, argv),
        cacheable: true,
        mutation: false,
        paths: Vec::new(),
        equivalence_key: None,
    })
}

pub fn reduce_github_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let lines = nonempty_lines(stdout);
    let line_count = lines.len();
    let command_name = spec.argv[0..3.min(spec.argv.len())].join(" ");
    let summary = if failed && spec.canonical_kind != "gh_pr_checks" {
        first_nonempty_line(stderr)
            .or_else(|| first_nonempty_line(stdout))
            .map(|line| format!("{command_name} failed: {line}"))
            .unwrap_or_else(|| format!("{command_name} failed"))
    } else {
        match spec.canonical_kind.as_str() {
            "gh_pr_list" => summarize_list_entries("gh pr list", &lines, "PR"),
            "gh_pr_view" => summarize_pr_view(&lines),
            "gh_pr_diff" => format!("gh pr diff returned {line_count} diff line(s)"),
            "gh_pr_checks" => summarize_pr_checks(&lines),
            "gh_issue_list" => summarize_list_entries("gh issue list", &lines, "issue"),
            "gh_issue_view" => lines
                .first()
                .map(|line| format!("gh issue view: {line}"))
                .unwrap_or_else(|| "gh issue view completed".to_string()),
            "gh_run_list" => summarize_list_entries("gh run list", &lines, "run"),
            "gh_run_view" => summarize_run_view(&lines),
            "gh_api" => summarize_api(stdout),
            _ => format!("{command_name} returned {line_count} line(s)"),
        }
    };
    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        compact_preview: match spec.canonical_kind.as_str() {
            "gh_pr_view" => compact_pr_view_preview(stdout),
            "gh_pr_checks" => compact_pr_checks_preview(&lines),
            "gh_run_view" => compact_run_view_preview(&lines),
            "gh_pr_diff" => crate::git::compact_diff_public(stdout, 500),
            _ => String::new(),
        },
        paths: spec.paths.clone(),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "github_error".to_string()),
        error_message: failed.then(|| compact(stderr, 200)),
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

fn summarize_pr_view(lines: &[String]) -> String {
    let title = extract_tab_field(lines, "title");
    let state = extract_tab_field(lines, "state");
    let number = extract_tab_field(lines, "number");
    let author = extract_tab_field(lines, "author");
    match (number, state, title) {
        (Some(number), Some(state), Some(title)) => {
            if let Some(author) = author {
                format!("gh pr view: PR #{number} {state} by {author} - {title}")
            } else {
                format!("gh pr view: PR #{number} {state} - {title}")
            }
        }
        _ => lines
            .first()
            .map(|line| format!("gh pr view: {line}"))
            .unwrap_or_else(|| "gh pr view completed".to_string()),
    }
}

fn summarize_run_view(lines: &[String]) -> String {
    let title = lines
        .first()
        .and_then(|line| line.strip_prefix('✓').or_else(|| line.strip_prefix('X')))
        .map(str::trim)
        .and_then(|line| line.split('·').next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned);
    let jobs = extract_section_count(lines, "JOBS");
    let annotations = extract_section_count(lines, "ANNOTATIONS");
    match title {
        Some(title) => format!(
            "gh run view: {title} ({jobs} job{}, {annotations} annotation{})",
            if jobs == 1 { "" } else { "s" },
            if annotations == 1 { "" } else { "s" }
        ),
        None => lines
            .first()
            .map(|line| format!("gh run view: {line}"))
            .unwrap_or_else(|| "gh run view completed".to_string()),
    }
}

fn summarize_list_entries(label: &str, lines: &[String], noun: &str) -> String {
    let count = lines.len();
    if let Some(first) = lines.first() {
        let fields = first.split('\t').collect::<Vec<_>>();
        let preview = fields.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let state = fields.get(3).copied().filter(|value| !value.is_empty());
        if !preview.is_empty() {
            if let Some(state) = state {
                return format!("{label}: {count} {noun}(s); first {preview} [{state}]");
            }
            return format!("{label}: {count} {noun}(s); first {preview}");
        }
    }
    format!("{label} returned {count} {noun}(s)")
}

fn summarize_pr_checks(lines: &[String]) -> String {
    if lines.is_empty() {
        return "gh pr checks returned 0 checks".to_string();
    }
    let mut passing = 0usize;
    let mut failing = 0usize;
    let mut pending = 0usize;
    let mut first_failing = None::<String>;
    for line in lines {
        let fields = line
            .split('\t')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        let name = fields.first().copied().unwrap_or_default();
        let status = fields
            .iter()
            .find(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "pass" | "fail" | "pending" | "cancel" | "skipping" | "skip"
                )
            })
            .map(|value| value.to_ascii_lowercase());
        match status.as_deref() {
            Some("pass") => passing += 1,
            Some("fail") | Some("cancel") => {
                failing += 1;
                if first_failing.is_none() && !name.is_empty() {
                    first_failing = Some(name.to_string());
                }
            }
            Some("pending") | Some("skip") | Some("skipping") => pending += 1,
            _ => pending += 1,
        }
    }
    if let Some(name) = first_failing {
        format!(
            "gh pr checks: {passing} pass, {failing} fail, {pending} pending; first failing {name}"
        )
    } else {
        format!("gh pr checks: {passing} pass, {failing} fail, {pending} pending")
    }
}

fn summarize_api(stdout: &str) -> String {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return "gh api returned empty payload".to_string();
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        let lines = nonempty_lines(stdout);
        return format!("gh api returned {} line(s)", lines.len());
    };
    match value {
        Value::Array(items) => {
            if let Some(first) = items.first() {
                if let Some(label) = json_label(first) {
                    format!("gh api returned {} item(s); first {label}", items.len())
                } else {
                    format!("gh api returned {} item(s)", items.len())
                }
            } else {
                "gh api returned 0 item(s)".to_string()
            }
        }
        Value::Object(map) => {
            if let Some(label) = json_label(&Value::Object(map.clone())) {
                format!("gh api returned object; {label}")
            } else {
                format!("gh api returned object with {} key(s)", map.len())
            }
        }
        _ => "gh api returned scalar payload".to_string(),
    }
}

fn json_label(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    for key in ["full_name", "name", "title", "status", "conclusion"] {
        if let Some(label) = object.get(key).and_then(Value::as_str) {
            return Some(format!("{key}={label}"));
        }
    }
    Some(format!("{} key(s)", object.len()))
}

fn extract_tab_field(lines: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}:\t");
    lines
        .iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(ToOwned::to_owned)
}

fn extract_section_count(lines: &[String], heading: &str) -> usize {
    let mut in_section = false;
    let mut count = 0;
    for line in lines {
        if line == heading {
            in_section = true;
            continue;
        }
        if in_section {
            if line.trim().is_empty() {
                if count > 0 {
                    break;
                }
                continue;
            }
            if line
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch == ' ' || ch == '_')
            {
                break;
            }
            count += 1;
        }
    }
    count
}

fn compact_pr_view_preview(stdout: &str) -> String {
    let mut parts = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        // Skip markdown images, badges, HTML comments
        if trimmed.starts_with("![")
            || trimmed.starts_with("<!--")
            || trimmed.starts_with("<img")
            || trimmed.starts_with("[![")
        {
            continue;
        }
        parts.push(trimmed.to_string());
    }
    // Limit to 30 lines
    parts.truncate(30);
    parts.join("\n")
}

fn compact_pr_checks_preview(lines: &[String]) -> String {
    let mut result = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 2 {
            let name = fields[0].trim();
            let status = fields
                .iter()
                .find(|f| {
                    matches!(
                        f.trim().to_ascii_lowercase().as_str(),
                        "pass" | "fail" | "pending" | "cancel" | "skip"
                    )
                })
                .map(|s| s.trim())
                .unwrap_or("?");
            result.push(format!("{status} {name}"));
        }
    }
    result.join("\n")
}

fn compact_run_view_preview(lines: &[String]) -> String {
    let mut result = Vec::new();
    let mut in_jobs = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "JOBS" {
            in_jobs = true;
            continue;
        }
        if trimmed == "ANNOTATIONS" {
            in_jobs = false;
            continue;
        }
        if in_jobs && !trimmed.is_empty() {
            result.push(trimmed.to_string());
        }
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_github_declines_json_and_patch_variants() {
        let argv = vec!["gh", "pr", "list", "--json", "title"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_github_command("gh pr list --json title", &argv).is_none());

        let argv = vec!["gh", "pr", "diff", "--patch"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(classify_github_command("gh pr diff --patch", &argv).is_none());
    }

    #[test]
    fn reduce_github_list_summarizes_entries() {
        let argv = vec!["gh", "pr", "list"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh pr list", &argv).unwrap();
        let stdout = "123\tFix reducer path\tmain\tOPEN\n124\tTrim docs\tmain\tOPEN\n";
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "gh pr list: 2 PR(s); first 123 Fix reducer path [OPEN]"
        );
    }

    #[test]
    fn reduce_github_pr_view_summarizes_metadata() {
        let argv = vec!["gh", "pr", "view", "8"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh pr view 8", &argv).unwrap();
        let stdout = "title:\tAlign Packet28 runtimes and documentation\nstate:\tMERGED\nauthor:\tusharma123\nnumber:\t8\n";
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "gh pr view: PR #8 MERGED by usharma123 - Align Packet28 runtimes and documentation"
        );
    }

    #[test]
    fn reduce_github_run_view_summarizes_jobs_and_annotations() {
        let argv = vec!["gh", "run", "view", "23079602872"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh run view 23079602872", &argv).unwrap();
        let stdout = "\n✓ v0.2.24 Release · 23079602872\nTriggered via push about 17 hours ago\n\nJOBS\n✓ test in 2m19s\n✓ publish in 47s\n\nANNOTATIONS\n! Node.js 20 actions are deprecated.\n! Another warning.\n";
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "gh run view: v0.2.24 Release (2 jobs, 2 annotations)"
        );
    }

    #[test]
    fn reduce_github_pr_checks_summarizes_status_counts() {
        let argv = vec!["gh", "pr", "checks", "12"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh pr checks 12", &argv).unwrap();
        let stdout = "build\tpass\t14s\ntest\tfail\t22s\nlint\tpending\t-\n";
        let reduction = reduce_github_command(&spec, stdout, "", 1);
        assert_eq!(
            reduction.summary,
            "gh pr checks: 1 pass, 1 fail, 1 pending; first failing test"
        );
    }

    #[test]
    fn reduce_github_api_summarizes_json_payload() {
        let argv = vec!["gh", "api", "repos/packet28/coverage/pulls"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh api repos/packet28/coverage/pulls", &argv).unwrap();
        let stdout = r#"[{"title":"Add compact parity"},{"title":"Trim reducers"}]"#;
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "gh api returned 2 item(s); first title=Add compact parity"
        );
    }
}
