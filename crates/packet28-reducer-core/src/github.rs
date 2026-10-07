use crate::types::{CommandReducerSpec, CommandReduction};
use serde_json::Value;

pub fn classify_github_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let program = argv.first()?.as_str();
    if !matches!(program, "gh" | "glab") {
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
    let canonical_kind = match (program, group, action) {
        ("gh", "pr", "list") => "gh_pr_list",
        ("gh", "pr", "view") => "gh_pr_view",
        ("gh", "pr", "diff") => "gh_pr_diff",
        ("gh", "pr", "checks") => "gh_pr_checks",
        ("gh", "issue", "list") => "gh_issue_list",
        ("gh", "issue", "view") => "gh_issue_view",
        ("gh", "run", "list") => "gh_run_list",
        ("gh", "run", "view") => "gh_run_view",
        ("gh", "release", "list") => "gh_release_list",
        ("gh", "api", _) if argv.get(1).is_some_and(|value| value == "api") => "gh_api",
        ("glab", "mr", "list") => "glab_mr_list",
        ("glab", "mr", "view") => "glab_mr_view",
        ("glab", "mr", "diff") => "glab_mr_diff",
        ("glab", "ci", "status") => "glab_ci_status",
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
            "gh_run_view" => summarize_run_view(&lines, stdout),
            "gh_release_list" => summarize_list_entries("gh release list", &lines, "release"),
            "gh_api" => summarize_api(stdout),
            "glab_mr_list" => summarize_list_entries("glab mr list", &lines, "MR"),
            "glab_mr_view" => lines
                .first()
                .map(|line| format!("glab mr view: {line}"))
                .unwrap_or_else(|| "glab mr view completed".to_string()),
            "glab_mr_diff" => format!("glab mr diff returned {line_count} diff line(s)"),
            "glab_ci_status" => summarize_glab_ci_status(&lines),
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
            "gh_pr_view" if failed => format!("{stdout}{stderr}"),
            "gh_pr_view" => compact_pr_view_preview(stdout),
            "gh_pr_checks" => compact_pr_checks_preview(&lines),
            "gh_run_view" => compact_run_view_preview(stdout),
            "gh_pr_diff" | "glab_mr_diff" => crate::git::compact_diff_public(stdout, 500),
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
    crate::cache_fingerprint(family, kind, argv)
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
        format!("{}...", utf8_prefix(&compact, limit.saturating_sub(3)))
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

fn summarize_run_view(lines: &[String], stdout: &str) -> String {
    let title = lines
        .first()
        .and_then(|line| line.strip_prefix('✓').or_else(|| line.strip_prefix('X')))
        .map(str::trim)
        .and_then(|line| line.split('·').next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned);
    let raw_lines = stdout.lines().map(str::trim_end).collect::<Vec<_>>();
    // Counts come from the untrimmed output: step lines are only
    // distinguishable from jobs by their indentation. A section that does not
    // match gh's layout yields no count rather than a wrong one.
    let jobs = count_run_view_jobs(&raw_lines);
    let annotations = if jobs.is_some() {
        count_run_view_annotations(&raw_lines)
    } else {
        None
    };
    let mut counts = Vec::new();
    if let Some(jobs) = jobs {
        counts.push(format!("{jobs} job{}", if jobs == 1 { "" } else { "s" }));
    }
    if let Some(annotations) = annotations {
        counts.push(format!(
            "{annotations} annotation{}",
            if annotations == 1 { "" } else { "s" }
        ));
    }
    match title {
        Some(title) if counts.is_empty() => format!("gh run view: {title}"),
        Some(title) => format!("gh run view: {title} ({})", counts.join(", ")),
        None => lines
            .first()
            .map(|line| format!("gh run view: {line}"))
            .unwrap_or_else(|| "gh run view completed".to_string()),
    }
}

/// Counts `<symbol> <name>[ in <elapsed>] (ID <id>)` lines in the JOBS
/// section, skipping their two-space-indented step lines.
fn count_run_view_jobs(lines: &[&str]) -> Option<usize> {
    let start = lines.iter().position(|line| *line == "JOBS")? + 1;
    let mut jobs = 0;
    for line in lines[start..].iter().take_while(|line| !line.is_empty()) {
        if line.starts_with("  ") && jobs > 0 {
            continue;
        }
        if !is_run_view_job_line(line) {
            return None;
        }
        jobs += 1;
    }
    Some(jobs)
}

fn is_run_view_job_line(line: &str) -> bool {
    let Some((symbol, rest)) = line.split_once(' ') else {
        return false;
    };
    let id = rest
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" (ID "))
        .map(|(_, id)| id);
    !symbol.is_empty()
        && !symbol.starts_with(char::is_whitespace)
        && id.is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Counts annotations in gh's `<symbol> <message>` / `<job>: <path>#<line>`
/// blocks. Messages may span lines, so a block ends only at a location line
/// followed by a blank line; the section ends at the next blank line.
fn count_run_view_annotations(lines: &[&str]) -> Option<usize> {
    let Some(heading) = lines.iter().position(|line| *line == "ANNOTATIONS") else {
        // gh omits the heading when the run has no annotations.
        return Some(0);
    };
    let mut annotations = 0;
    let mut index = heading + 1;
    while lines.get(index).is_some_and(|line| !line.is_empty()) {
        let first = lines[index];
        if !matches!(
            first.split_once(' ').map(|(symbol, _)| symbol),
            Some("X" | "!" | "-")
        ) {
            return None;
        }
        let end = (index + 1..lines.len()).find(|&candidate| {
            is_run_view_annotation_location(lines[candidate])
                && lines.get(candidate + 1).is_none_or(|next| next.is_empty())
        })?;
        annotations += 1;
        index = end + 2;
    }
    Some(annotations)
}

fn is_run_view_annotation_location(line: &str) -> bool {
    line.rsplit_once('#').is_some_and(|(location, number)| {
        location.contains(": ")
            && !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn summarize_list_entries(label: &str, lines: &[String], noun: &str) -> String {
    let count = lines.len();
    if let Some(first) = lines.first() {
        let fields = first.split('\t').collect::<Vec<_>>();
        let preview = fields.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let preview = if preview.chars().count() > 80 {
            format!("{}...", preview.chars().take(77).collect::<String>())
        } else {
            preview
        };
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

fn compact_pr_view_preview(stdout: &str) -> String {
    let separator = stdout.lines().position(|line| line.trim() == "--");
    let header = stdout
        .lines()
        .take(separator.unwrap_or(0))
        .collect::<Vec<_>>();
    let recognized_header = separator.is_some()
        && header
            .first()
            .is_some_and(|line| line.starts_with("title:\t"))
        && header.iter().all(|line| line.split_once(":\t").is_some())
        && ["title:\t", "state:\t", "number:\t"]
            .iter()
            .all(|prefix| header.iter().any(|line| line.starts_with(prefix)));
    let mut parts = Vec::new();
    if recognized_header {
        if let Some(url) = header.iter().find_map(|line| line.strip_prefix("url:\t")) {
            parts.push(format!("url: {url}"));
        }
    }
    let body_start = if recognized_header {
        separator.map_or(0, |index| index + 1)
    } else {
        0
    };
    let (body, body_omitted) = compact_pr_body(stdout.lines().skip(body_start));
    if !body.is_empty() {
        parts.push(body);
    }
    let metadata_omitted = recognized_header
        && header.iter().any(|line| {
            line.split_once(":\t").is_some_and(|(key, value)| {
                !value.trim().is_empty()
                    && !matches!(key, "title" | "state" | "number" | "author" | "url")
            })
        });
    if body_omitted || metadata_omitted {
        parts.push("[content omitted; use original gh command for full output]".to_string());
    }
    parts.join("\n")
}

fn compact_pr_body<'a>(lines: impl Iterator<Item = &'a str>) -> (String, bool) {
    const MAX_BYTES: usize = 320;
    const MAX_LINES: usize = 8;
    let mut body = String::new();
    let mut rendered_lines = 0;
    let mut omitted = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("![")
            || trimmed.starts_with("<!--")
            || trimmed.starts_with("<img")
            || trimmed.starts_with("[![")
        {
            omitted = true;
            continue;
        }
        let separator = if rendered_lines > 0 { "\n" } else { "" };
        let remaining = MAX_BYTES.saturating_sub(body.len());
        if rendered_lines == MAX_LINES || separator.len() > remaining {
            omitted = true;
            break;
        }
        let prefix = utf8_prefix(trimmed, remaining - separator.len());
        body.push_str(separator);
        body.push_str(prefix);
        rendered_lines += 1;
        if prefix.len() != trimmed.len() {
            omitted = true;
            break;
        }
    }
    (body, omitted)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    &value[..end]
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

fn summarize_glab_ci_status(lines: &[String]) -> String {
    let first = lines
        .first()
        .map(String::as_str)
        .unwrap_or("no status lines");
    format!("glab ci status: {first}")
}

fn compact_run_view_preview(stdout: &str) -> String {
    let mut result = Vec::new();
    let mut in_jobs = false;
    for line in stdout.lines() {
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
            // Keep step indentation so steps are not mistaken for jobs.
            result.push(line.trim_end().to_string());
        }
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_view_spec() -> CommandReducerSpec {
        let argv = ["gh", "pr", "view", "71"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        classify_github_command("gh pr view 71", &argv).unwrap()
    }

    fn pr_view_stdout(body: &str) -> String {
        format!(
            "title:\tIntegrate native lifecycle hooks\nstate:\tOPEN\nauthor:\tusharma123\nlabels:\t\nreviewers:\tconnector (Commented)\nnumber:\t71\nurl:\thttps://github.com/usharma123/Packet28/pull/71\nadditions:\t1207\ndeletions:\t33\n--\n{body}"
        )
    }

    #[test]
    fn pr_view_preview_preserves_identity_without_duplicate_header() {
        let output = pr_view_stdout("## Problem\nPreserve native commands and permissions.\n");
        let reduction = reduce_github_command(&pr_view_spec(), &output, "", 0);
        assert_eq!(
            reduction.summary,
            "gh pr view: PR #71 OPEN by usharma123 - Integrate native lifecycle hooks"
        );
        assert!(reduction
            .compact_preview
            .contains("url: https://github.com/usharma123/Packet28/pull/71"));
        assert!(reduction.compact_preview.contains("## Problem"));
        for field in ["title:\t", "state:\t", "author:\t", "number:\t"] {
            assert!(!reduction.compact_preview.contains(field));
        }
        assert!(reduction.compact_preview.contains("content omitted"));
        assert!(reduction.compact_preview.contains("original gh command"));
    }

    #[test]
    fn pr_view_preview_bounds_long_unicode_body_without_cutting_characters() {
        let body = "🦀修".repeat(1000);
        let output = pr_view_stdout(&body);
        let reduction = reduce_github_command(&pr_view_spec(), &output, "", 0);
        let rendered_body = reduction.compact_preview.lines().nth(1).unwrap();
        assert!(rendered_body.len() <= 320);
        assert!(rendered_body.starts_with("🦀修"));
        assert!(body.starts_with(rendered_body));
        assert!(reduction
            .compact_preview
            .ends_with("[content omitted; use original gh command for full output]"));
    }

    #[test]
    fn pr_view_preview_bounds_body_lines_and_discloses_filtered_content() {
        let body = format!(
            "![badge](badge.png)\n{}",
            (0..12)
                .map(|index| format!("body line {index}\n"))
                .collect::<String>()
        );
        let reduction = reduce_github_command(&pr_view_spec(), &pr_view_stdout(&body), "", 0);
        assert!(reduction.compact_preview.contains("body line 7"));
        assert!(!reduction.compact_preview.contains("body line 8"));
        assert!(!reduction.compact_preview.contains("badge.png"));
        assert!(reduction.compact_preview.contains("content omitted"));
    }

    #[test]
    fn pr_view_preview_keeps_plain_body_separator_without_recognized_header() {
        let body = "ordinary heading\n--\nordinary body";
        assert_eq!(compact_pr_view_preview(body), body);
        let malformed = "title:\tHeading\n--\nstate:\tOPEN\nnumber:\t71\nbody";
        assert_eq!(compact_pr_view_preview(malformed), malformed);
        let ordinary = "ordinary heading\ntitle:\tBody title\nstate:\tOPEN\nnumber:\t71\n--\nbody";
        assert_eq!(compact_pr_view_preview(ordinary), ordinary);
    }

    #[test]
    fn pr_view_preview_keeps_metadata_when_header_separator_is_absent() {
        let output =
            "title:\tHeading\nstate:\tOPEN\nnumber:\t71\nurl:\thttps://example.test/pr/71\nbody";
        assert_eq!(compact_pr_view_preview(output), output);
    }

    #[test]
    fn pr_view_reduction_preserves_complete_failed_output_and_exit() {
        let stdout = pr_view_stdout(&"partial output\n".repeat(40));
        let stderr = format!(
            "request failed: {}\nadditional diagnostic\n",
            "🦀".repeat(100)
        );
        let reduction = reduce_github_command(&pr_view_spec(), &stdout, &stderr, 7);
        assert!(reduction.failed);
        assert_eq!(reduction.exit_code, 7);
        assert_eq!(reduction.compact_preview, format!("{stdout}{stderr}"));
        assert!(reduction.summary.contains(stderr.lines().next().unwrap()));
        assert!(reduction.error_message.as_ref().unwrap().len() <= 200);
    }

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
    fn list_summary_bounds_long_unicode_titles() {
        let title = "修".repeat(200);
        let summary =
            summarize_list_entries("gh pr list", &[format!("42\t{title}\tbranch\tOPEN")], "PR");
        assert!(summary.chars().count() < 120);
        assert!(summary.starts_with("gh pr list: 1 PR(s); first 42 "));
        assert!(summary.ends_with("... [OPEN]"));
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

    fn run_view_reduction(stdout: &str) -> CommandReduction {
        let argv = vec!["gh", "run", "view", "23079602872"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh run view 23079602872", &argv).unwrap();
        reduce_github_command(&spec, stdout, "", 0)
    }

    #[test]
    fn reduce_github_run_view_summarizes_jobs_and_annotations() {
        let stdout = "\n✓ v0.2.24 Release · 23079602872\nTriggered via push about 17 hours ago\n\nJOBS\n✓ test in 2m19s (ID 1)\n✓ publish in 47s (ID 2)\n\nANNOTATIONS\n! Node.js 20 actions are deprecated.\ntest: .github#1\n\n! Another warning.\npublish: .github#1\n\n\nFor more information about a job, try: gh run view --job=<job-id>\nView this run on GitHub: https://github.com/o/r/actions/runs/23079602872\n";
        assert_eq!(
            run_view_reduction(stdout).summary,
            "gh run view: v0.2.24 Release (2 jobs, 2 annotations)"
        );
    }

    #[test]
    fn run_view_counts_failed_job_without_its_steps_or_annotation_locations() {
        // Real `gh run view 37565793313 --repo usharma123/Packet28` output.
        let stdout = include_str!("../tests/fixtures/github/run_view_37565793313.stdout");
        let reduction = run_view_reduction(stdout);
        assert_eq!(
            reduction.summary,
            "gh run view: fix/scope-closure-registry-recovery Hook Benchmark Suite usharma123/Packet28#74 (1 job, 2 annotations)"
        );
        let preview = reduction.compact_preview.lines().collect::<Vec<_>>();
        assert_eq!(preview[0], "X benchmark in 4m24s (ID 112613078067)");
        assert_eq!(preview.len(), 19);
        assert!(preview[1..].iter().all(|line| line.starts_with("  ")));
        assert!(preview.contains(&"  X Validate hook benchmark thresholds"));
    }

    #[test]
    fn run_view_counts_successful_jobs_and_annotations() {
        // Real `gh run view 37565793390 --repo usharma123/Packet28` output.
        let stdout = include_str!("../tests/fixtures/github/run_view_37565793390.stdout");
        let reduction = run_view_reduction(stdout);
        assert_eq!(
            reduction.summary,
            "gh run view: fix/scope-closure-registry-recovery Build usharma123/Packet28#74 (2 jobs, 2 annotations)"
        );
        assert_eq!(
            reduction.compact_preview,
            "✓ quality in 21m9s (ID 112613078285)\n✓ msrv in 3m48s (ID 112613078536)"
        );
    }

    #[test]
    fn run_view_counts_multiline_annotations_artifacts_and_jobs_without_elapsed() {
        let stdout = "\nX main CI · 7\nTriggered via push about 1 hour ago\n\nJOBS\nX build in 8s (ID 11)\n  ✓ Set up job\n  X Publish\n- test-plugins (ID 12)\n* lint (ID 13)\n\nANNOTATIONS\nX NpmCallError: publish failed\nnpm error code E404\n\nnpm error 404 Not Found\nbuild: .github#6\n\n- \"The ubuntu-latest label will migrate\"\nbuild: .github#1\n\n\nARTIFACTS\nreport\n\nTo see what failed, try: gh run view 7 --log-failed\nView this run on GitHub: https://github.com/o/r/actions/runs/7\n";
        assert_eq!(
            run_view_reduction(stdout).summary,
            "gh run view: main CI (3 jobs, 2 annotations)"
        );
    }

    #[test]
    fn run_view_counts_zero_annotations_when_section_is_absent() {
        let stdout = "\n✓ main CI · 7\nTriggered via push about 1 hour ago\n\nJOBS\n✓ build in 8s (ID 11)\n\nFor more information about the job, try: gh run view --job=11\nView this run on GitHub: https://github.com/o/r/actions/runs/7\n";
        assert_eq!(
            run_view_reduction(stdout).summary,
            "gh run view: main CI (1 job, 0 annotations)"
        );
        let crlf = stdout.replace('\n', "\r\n");
        assert_eq!(
            run_view_reduction(&crlf).summary,
            "gh run view: main CI (1 job, 0 annotations)"
        );
    }

    #[test]
    fn run_view_omits_counts_it_cannot_parse() {
        // Steps whose indentation was lost, an unknown job line, or a missing
        // JOBS heading must not be counted as jobs.
        for jobs in [
            "X build in 8s (ID 11)\n✓ Set up job\n",
            "build passed\n",
            "  ✓ Set up job\nX build in 8s (ID 11)\n",
        ] {
            let stdout = format!(
                "\nX main CI · 7\n\nJOBS\n{jobs}\nANNOTATIONS\nX failed\nbuild: .github#1\n\n"
            );
            assert_eq!(run_view_reduction(&stdout).summary, "gh run view: main CI");
        }
        assert_eq!(
            run_view_reduction(
                "\nX main CI · 7\n\nX This run likely failed because of a workflow file issue.\n"
            )
            .summary,
            "gh run view: main CI"
        );
        // Annotations without gh's location lines are not guessed at.
        for annotations in [
            "! one warning\n! another warning\n",
            "X failed\nno location\n",
        ] {
            let stdout =
                format!("\nX main CI · 7\n\nJOBS\nX build (ID 11)\n\nANNOTATIONS\n{annotations}\n");
            assert_eq!(
                run_view_reduction(&stdout).summary,
                "gh run view: main CI (1 job)"
            );
        }
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

    #[test]
    fn reduce_glab_mr_list_summarizes_entries() {
        let argv = vec!["glab", "mr", "list"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("glab mr list", &argv).unwrap();
        let stdout = "42\tFix GitLab reducer\tmain\topened\n43\tUpdate docs\tmain\topened\n";
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(reduction.canonical_kind, "glab_mr_list");
        assert_eq!(
            reduction.summary,
            "glab mr list: 2 MR(s); first 42 Fix GitLab reducer [opened]"
        );
    }

    #[test]
    fn reduce_rtk_release_and_glab_ci_status_forms() {
        let argv = vec!["gh", "release", "list"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("gh release list", &argv).unwrap();
        assert_eq!(spec.canonical_kind, "gh_release_list");
        let stdout =
            "v0.2.52\tLatest\t2026-05-12T08:00:00Z\nv0.2.51\tPrevious\t2026-05-11T08:00:00Z\n";
        let reduction = reduce_github_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "gh release list: 2 release(s); first v0.2.52 Latest"
        );

        let argv = vec!["glab", "ci", "status"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_github_command("glab ci status", &argv).unwrap();
        assert_eq!(spec.canonical_kind, "glab_ci_status");
        let reduction = reduce_github_command(&spec, "success: pipeline passed\n", "", 0);
        assert_eq!(
            reduction.summary,
            "glab ci status: success: pipeline passed"
        );
    }
}
