use crate::types::{CommandReducerSpec, CommandReduction};

pub fn classify_git_command(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {
    let subcommand = argv.get(1)?.as_str();
    let (canonical_kind, mutation) = match subcommand {
        "status" => ("git_status", false),
        "log" if !contains_any(argv, &["--format", "--pretty", "-p", "--patch", "--raw"]) => {
            ("git_log", false)
        }
        "diff" if !contains_any(argv, &["-p", "--patch", "--raw", "--word-diff"]) => {
            ("git_diff", false)
        }
        "show" => ("git_show", false),
        "fetch" => ("git_fetch", false),
        "stash" => (
            "git_stash",
            !matches!(argv.get(2).map(String::as_str), Some("list" | "show")),
        ),
        "worktree" => (
            "git_worktree",
            !matches!(argv.get(2).map(String::as_str), Some("list")),
        ),
        "add" => ("git_add", true),
        "commit" => ("git_commit", true),
        "push" => ("git_push", false),
        "pull" => ("git_pull", true),
        "branch" => ("git_branch", false),
        "switch" => ("git_switch", true),
        "checkout" => ("git_checkout", true),
        _ => return None,
    };
    let paths = argv
        .iter()
        .skip(2)
        .filter(|arg| !arg.starts_with('-'))
        .cloned()
        .collect::<Vec<_>>();
    Some(CommandReducerSpec {
        family: "git".to_string(),
        canonical_kind: canonical_kind.to_string(),
        packet_type: "packet28.hook.git.v2".to_string(),
        operation_kind: suite_packet_core::ToolOperationKind::Git,
        command: command.to_string(),
        argv: argv.to_vec(),
        cache_fingerprint: fingerprint("git", canonical_kind, argv),
        cacheable: !mutation,
        mutation,
        paths: normalize_paths(paths),
        equivalence_key: None,
    })
}

pub fn reduce_git_command(
    spec: &CommandReducerSpec,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
) -> CommandReduction {
    let failed = exit_code != 0;
    let output = first_nonempty_line(stdout).or_else(|| first_nonempty_line(stderr));
    let compact_preview = if failed {
        String::new()
    } else {
        match spec.canonical_kind.as_str() {
            "git_diff" | "git_show" if stdout.contains("diff --git ") => compact_diff(stdout, 500),
            "git_status" => compact_git_status(stdout),
            "git_log" => compact_git_log(stdout, 20),
            _ => String::new(),
        }
    };
    let summary = match spec.canonical_kind.as_str() {
        "git_status" => {
            if failed {
                output.unwrap_or_else(|| "git status failed".to_string())
            } else {
                summarize_git_status(stdout)
            }
        }
        "git_log" => {
            let commits = stdout
                .lines()
                .filter(|line| looks_like_commit_line(line.trim()))
                .count();
            if failed {
                output.unwrap_or_else(|| "git log failed".to_string())
            } else {
                format!(
                    "git log returned {commits} commit entr{suffix}",
                    suffix = if commits == 1 { "y" } else { "ies" }
                )
            }
        }
        "git_diff" => {
            let files = stdout
                .lines()
                .filter(|line| line.starts_with("diff --git "))
                .count();
            if failed {
                output.unwrap_or_else(|| "git diff failed".to_string())
            } else if files > 0 {
                let (added, removed) = count_diff_changes(stdout);
                format!("git diff: {files} file(s) changed, +{added} -{removed}")
            } else {
                first_nonempty_line(stdout).unwrap_or_else(|| "git diff completed".to_string())
            }
        }
        "git_show" => {
            if failed {
                output.unwrap_or_else(|| "git show failed".to_string())
            } else {
                summarize_git_show(stdout)
            }
        }
        "git_fetch" => {
            if failed {
                output.unwrap_or_else(|| "git fetch failed".to_string())
            } else {
                summarize_git_fetch(stdout, stderr)
            }
        }
        "git_stash" => {
            if failed {
                output.unwrap_or_else(|| "git stash failed".to_string())
            } else {
                summarize_git_stash(spec, stdout)
            }
        }
        "git_worktree" => {
            if failed {
                output.unwrap_or_else(|| "git worktree failed".to_string())
            } else {
                summarize_git_worktree(spec, stdout)
            }
        }
        _ => output.unwrap_or_else(|| format!("{} completed", spec.canonical_kind)),
    };
    CommandReduction {
        family: spec.family.clone(),
        canonical_kind: spec.canonical_kind.clone(),
        packet_type: spec.packet_type.clone(),
        operation_kind: spec.operation_kind,
        summary,
        compact_preview,
        paths: spec.paths.clone(),
        regions: Vec::new(),
        symbols: Vec::new(),
        failed,
        error_class: failed.then(|| "git_error".to_string()),
        error_message: failed.then(|| compact(stderr, 200)),
        retryable: failed.then_some(false),
        exit_code,
        cache_fingerprint: spec.cache_fingerprint.clone(),
        cacheable: spec.cacheable,
        mutation: spec.mutation,
        equivalence_key: spec.equivalence_key.clone(),
    }
}

/// Public entry point for compact diff rendering, used by github.rs.
pub fn compact_diff_public(diff: &str, max_lines: usize) -> String {
    compact_diff(diff, max_lines)
}

/// RTK-style compact diff: file names, hunk headers, truncated changed lines.
fn compact_diff(diff: &str, max_lines: usize) -> String {
    let mut result = Vec::new();
    let mut current_file = String::new();
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    let mut hunk_lines = 0;
    let max_hunk_lines = 30;

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            if !current_file.is_empty() && (added > 0 || removed > 0) {
                result.push(format!("  +{} -{}", added, removed));
            }
            current_file = line.split(" b/").nth(1).unwrap_or("unknown").to_string();
            result.push(format!("\n📄 {}", current_file));
            added = 0;
            removed = 0;
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
            hunk_lines = 0;
            let hunk_info = line.split("@@").nth(1).unwrap_or("").trim();
            result.push(format!("  @@ {} @@", hunk_info));
        } else if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                added += 1;
                if hunk_lines < max_hunk_lines {
                    result.push(format!("  {}", line));
                    hunk_lines += 1;
                }
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed += 1;
                if hunk_lines < max_hunk_lines {
                    result.push(format!("  {}", line));
                    hunk_lines += 1;
                }
            } else if hunk_lines < max_hunk_lines && !line.starts_with('\\') {
                if hunk_lines > 0 {
                    result.push(format!("  {}", line));
                    hunk_lines += 1;
                }
            }

            if hunk_lines == max_hunk_lines {
                result.push("  ... (truncated)".to_string());
                hunk_lines += 1;
            }
        }

        if result.len() >= max_lines {
            result.push("\n... (more changes truncated)".to_string());
            break;
        }
    }

    if !current_file.is_empty() && (added > 0 || removed > 0) {
        result.push(format!("  +{} -{}", added, removed));
    }

    result.join("\n").trim_start().to_string()
}

fn contains_any(argv: &[String], denied: &[&str]) -> bool {
    argv.iter().any(|arg| {
        denied
            .iter()
            .any(|denied| arg == denied || arg.strip_prefix(&format!("{denied}=")).is_some())
    })
}

fn normalize_paths(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().filter(|path| !path.is_empty()).collect()
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

fn looks_like_commit_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    (7..=40).contains(&first.len()) && first.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn summarize_git_status(stdout: &str) -> String {
    if stdout.contains("nothing to commit") {
        return "git status clean".to_string();
    }
    let modified = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("modified:"))
        .count();
    let new_files = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("new file:"))
        .count();
    let deleted = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("deleted:"))
        .count();
    let renamed = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("renamed:"))
        .count();
    let untracked = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with('\t') && !line.contains(':'))
        .count();
    let mut parts = Vec::new();
    if modified > 0 {
        parts.push(format!("{modified} modified"));
    }
    if new_files > 0 {
        parts.push(format!("{new_files} new"));
    }
    if deleted > 0 {
        parts.push(format!("{deleted} deleted"));
    }
    if renamed > 0 {
        parts.push(format!("{renamed} renamed"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} untracked"));
    }
    if parts.is_empty() {
        "git status has pending changes".to_string()
    } else {
        format!("git status: {}", parts.join(", "))
    }
}

fn summarize_git_show(stdout: &str) -> String {
    let files = stdout
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .count();
    if let Some(subject) = extract_git_show_subject(stdout) {
        if let Some(commit) = extract_git_show_commit(stdout) {
            let short = commit.chars().take(8).collect::<String>();
            if files > 0 {
                return format!("git show {short}: {subject} ({files} file(s) changed)");
            }
            return format!("git show {short}: {subject}");
        }
        if files > 0 {
            return format!("git show: {subject} ({files} file(s) changed)");
        }
        return format!("git show: {subject}");
    }
    if files > 0 {
        format!("git show returned {files} diff file(s)")
    } else {
        "git show completed".to_string()
    }
}

fn summarize_git_fetch(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    let ref_updates = combined.lines().filter(|line| line.contains("->")).count();
    let tag_updates = combined
        .lines()
        .filter(|line| line.contains("[new tag]"))
        .count();
    if ref_updates > 0 {
        if tag_updates > 0 {
            format!("git fetch updated {ref_updates} ref(s), {tag_updates} tag(s)")
        } else {
            format!("git fetch updated {ref_updates} ref(s)")
        }
    } else if combined.contains("up to date") {
        "git fetch up to date".to_string()
    } else {
        first_nonempty_line(&combined).unwrap_or_else(|| "git fetch completed".to_string())
    }
}

fn summarize_git_stash(spec: &CommandReducerSpec, stdout: &str) -> String {
    if matches!(spec.argv.get(2).map(String::as_str), Some("list")) {
        let lines = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if let Some(first) = lines.first() {
            return format!("git stash list: {} entr(y/ies); first {first}", lines.len());
        }
        return "git stash list empty".to_string();
    }
    first_nonempty_line(stdout).unwrap_or_else(|| "git stash completed".to_string())
}

fn summarize_git_worktree(spec: &CommandReducerSpec, stdout: &str) -> String {
    if matches!(spec.argv.get(2).map(String::as_str), Some("list")) {
        let lines = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if let Some(first) = lines.first() {
            return format!(
                "git worktree list: {} worktree(s); first {first}",
                lines.len()
            );
        }
        return "git worktree list empty".to_string();
    }
    first_nonempty_line(stdout).unwrap_or_else(|| "git worktree completed".to_string())
}

fn extract_git_show_commit(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("commit "))
        .map(ToOwned::to_owned)
}

fn extract_git_show_subject(stdout: &str) -> Option<String> {
    let mut saw_header = false;
    let mut saw_blank_after_header = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("commit ") {
            saw_header = true;
            continue;
        }
        if !saw_header {
            continue;
        }
        if trimmed.is_empty() {
            if saw_blank_after_header {
                continue;
            }
            saw_blank_after_header = true;
            continue;
        }
        if saw_blank_after_header {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn compact_git_status(stdout: &str) -> String {
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut in_staged = false;
    let mut in_unstaged = false;
    let mut in_untracked = false;

    for line in stdout.lines() {
        if line.contains("Changes to be committed") {
            in_staged = true;
            in_unstaged = false;
            in_untracked = false;
            continue;
        }
        if line.contains("Changes not staged") {
            in_staged = false;
            in_unstaged = true;
            in_untracked = false;
            continue;
        }
        if line.contains("Untracked files") {
            in_staged = false;
            in_unstaged = false;
            in_untracked = true;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('(') {
            continue;
        }
        if in_staged && trimmed.contains(':') {
            if let Some(path) = trimmed.split(':').nth(1) {
                staged.push(path.trim().to_string());
            }
        } else if in_unstaged && trimmed.contains(':') {
            if let Some(path) = trimmed.split(':').nth(1) {
                unstaged.push(path.trim().to_string());
            }
        } else if in_untracked && !trimmed.starts_with('(') {
            untracked.push(trimmed.to_string());
        }
    }

    let mut parts = Vec::new();
    if !staged.is_empty() {
        parts.push(format!("Staged ({}):", staged.len()));
        for path in staged.iter().take(10) {
            parts.push(format!("  {path}"));
        }
        if staged.len() > 10 {
            parts.push(format!("  +{} more", staged.len() - 10));
        }
    }
    if !unstaged.is_empty() {
        parts.push(format!("Unstaged ({}):", unstaged.len()));
        for path in unstaged.iter().take(10) {
            parts.push(format!("  {path}"));
        }
        if unstaged.len() > 10 {
            parts.push(format!("  +{} more", unstaged.len() - 10));
        }
    }
    if !untracked.is_empty() {
        parts.push(format!("Untracked ({}):", untracked.len()));
        for path in untracked.iter().take(5) {
            parts.push(format!("  {path}"));
        }
        if untracked.len() > 5 {
            parts.push(format!("  +{} more", untracked.len() - 5));
        }
    }
    parts.join("\n")
}

fn compact_git_log(stdout: &str, max_entries: usize) -> String {
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if looks_like_commit_line(trimmed) {
            let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
            if let Some(hash) = parts.first() {
                let short = hash.chars().take(8).collect::<String>();
                let subject = parts.get(1).unwrap_or(&"").trim();
                entries.push(format!("{short} {subject}"));
            }
        }
        if entries.len() >= max_entries {
            break;
        }
    }
    if entries.is_empty() {
        return String::new();
    }
    entries.join("\n")
}

fn count_diff_changes(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_git_show_supports_compact_summary() {
        let argv = vec!["git", "show", "HEAD"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_git_command("git show HEAD", &argv).unwrap();
        assert_eq!(spec.canonical_kind, "git_show");
        assert!(spec.cacheable);
    }

    #[test]
    fn reduce_git_show_extracts_subject_and_diff_count() {
        let argv = vec!["git", "show", "HEAD"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_git_command("git show HEAD", &argv).unwrap();
        let stdout = "commit 1234567890abcdef\nAuthor: Packet28\nDate: Tue Mar 17 10:00:00 2026 +0000\n\n    Tighten rewrite routing\n\ndiff --git a/src/lib.rs b/src/lib.rs\ndiff --git a/src/main.rs b/src/main.rs\n";
        let reduction = reduce_git_command(&spec, stdout, "", 0);
        assert_eq!(
            reduction.summary,
            "git show 12345678: Tighten rewrite routing (2 file(s) changed)"
        );
    }

    #[test]
    fn reduce_git_diff_produces_compact_preview() {
        let argv = vec!["git", "diff"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_git_command("git diff", &argv).unwrap();
        let stdout = r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!("hello");
 }
"#;
        let reduction = reduce_git_command(&spec, stdout, "", 0);
        assert_eq!(reduction.summary, "git diff: 1 file(s) changed, +1 -0");
        assert!(reduction.compact_preview.contains("src/main.rs"));
        assert!(reduction.compact_preview.contains("println"));
    }

    #[test]
    fn reduce_git_fetch_counts_updated_refs() {
        let argv = vec!["git", "fetch", "origin"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let spec = classify_git_command("git fetch origin", &argv).unwrap();
        let stderr = "From github.com:packet28/coverage\n   abc1234..def5678  main       -> origin/main\n * [new tag]         v0.2.30    -> v0.2.30\n";
        let reduction = reduce_git_command(&spec, "", stderr, 0);
        assert_eq!(reduction.summary, "git fetch updated 2 ref(s), 1 tag(s)");
    }
}
