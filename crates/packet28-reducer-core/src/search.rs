use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use regex::RegexBuilder;

use crate::types::{SearchEngineStats, SearchGroup, SearchMatch, SearchRequest, SearchResult};

pub(crate) const DEFAULT_MAX_TOTAL_MATCHES: usize = 50;
const MAX_TOTAL_MATCHES_LIMIT: usize = 200;
const DEFAULT_DISPLAYED_MATCHES_PER_FILE: usize = 12;
const DEFAULT_DISPLAYED_GROUPS_IN_PREVIEW: usize = 12;
const DEFAULT_DISPLAYED_MATCH_LINES_IN_PREVIEW: usize = 12;
const DEFAULT_MATCH_PREVIEW_TEXT_LIMIT: usize = 140;
const MAX_FAILED_SEARCH_STDERR_BYTES: usize = 2048;

pub fn normalize_capture_path(root: &Path, text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return String::new();
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(root) {
            return stripped.to_string_lossy().replace('\\', "/");
        }
    }
    trimmed
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/")
}

pub fn format_region(path: &str, start: usize, end: usize) -> String {
    format!("{path}:{start}-{end}")
}

pub fn parse_region_for_path(region: &str, path: &str) -> Option<(usize, usize)> {
    let trimmed = region.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((start, end)) = parse_line_range_spec(trimmed) {
        return Some((start, end));
    }
    let (region_path, range) = trimmed.rsplit_once(':')?;
    if normalize_capture_path(Path::new(""), region_path) != path {
        return None;
    }
    parse_line_range_spec(range)
}

pub fn infer_symbols_from_pattern(pattern: &str) -> Vec<String> {
    let candidate = pattern
        .trim()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | ':' | '.'));
    if candidate.len() < 3 || !candidate.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return Vec::new();
    }
    vec![candidate.to_string()]
}

pub fn infer_symbols_from_lines(lines: &[String]) -> Vec<String> {
    let mut symbols = BTreeMap::<String, ()>::new();
    for line in lines {
        let trimmed = line.trim();
        let token = if let Some(rest) = trimmed.strip_prefix("pub struct ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("struct ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("pub enum ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("enum ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("pub trait ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("trait ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("pub fn ") {
            rest.split('(').next()
        } else if let Some(rest) = trimmed.strip_prefix("fn ") {
            rest.split('(').next()
        } else if let Some(rest) = trimmed.strip_prefix("class ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = trimmed.strip_prefix("interface ") {
            rest.split_whitespace().next()
        } else if trimmed.contains('(') && trimmed.ends_with('{') {
            trimmed
                .split('(')
                .next()
                .and_then(|prefix| prefix.split_whitespace().last())
        } else {
            None
        };
        if let Some(token) = token {
            let cleaned = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
            if cleaned.len() >= 3 && cleaned.chars().any(|ch| ch.is_ascii_alphabetic()) {
                symbols.insert(cleaned.to_string(), ());
            }
        }
    }
    symbols.into_keys().take(8).collect()
}

pub fn search(root: &Path, request: &SearchRequest) -> Result<SearchResult> {
    search_with_rg_binary(root, request, "rg")
}

fn search_with_rg_binary(
    root: &Path,
    request: &SearchRequest,
    rg_binary: &str,
) -> Result<SearchResult> {
    let query = request.query.trim();
    anyhow::ensure!(!query.is_empty(), "search query cannot be empty");

    let (resolved_paths, mut diagnostics) = resolve_requested_paths(root, &request.requested_paths);
    let output = collect_matches_with_rg(
        root,
        request,
        query,
        &resolved_paths,
        &mut diagnostics,
        rg_binary,
    )?;
    let matches = if !request.requested_paths.is_empty() && resolved_paths.is_empty() {
        Vec::new()
    } else if let Some(output) = output {
        output
    } else {
        collect_matches_without_rg(root, request, query, &resolved_paths)?
    };
    let resolved_path_count = resolved_paths.len();
    let mut groups = BTreeMap::<String, Vec<SearchMatch>>::new();
    let total_match_count = matches.len();
    for item in matches {
        groups.entry(item.path.clone()).or_default().push(item);
    }

    let paths = groups.keys().cloned().collect::<Vec<_>>();
    let regions = groups
        .values()
        .flat_map(|items| {
            items
                .iter()
                .map(|item| format_region(&item.path, item.line, item.line))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let max_total_matches = request
        .max_total_matches
        .unwrap_or(DEFAULT_MAX_TOTAL_MATCHES)
        .clamp(1, MAX_TOTAL_MATCHES_LIMIT);
    let mut returned_matches = Vec::new();
    let grouped = groups
        .into_iter()
        .map(|(path, items)| {
            let displayed = items
                .iter()
                .take(DEFAULT_DISPLAYED_MATCHES_PER_FILE)
                .cloned()
                .collect::<Vec<_>>();
            let truncated = items.len() > displayed.len();
            for item in displayed.iter().cloned() {
                if returned_matches.len() < max_total_matches {
                    returned_matches.push(item);
                }
            }
            SearchGroup {
                path,
                match_count: items.len(),
                displayed_match_count: displayed.len(),
                truncated,
                matches: displayed,
            }
        })
        .collect::<Vec<_>>();

    let returned_match_count = returned_matches.len().min(max_total_matches);
    if returned_matches.len() > returned_match_count {
        returned_matches.truncate(returned_match_count);
    }
    let compact_preview =
        render_search_compact_preview(total_match_count, &grouped, max_total_matches);
    Ok(SearchResult {
        query: query.to_string(),
        requested_paths: request.requested_paths.clone(),
        resolved_paths,
        match_count: total_match_count,
        returned_match_count,
        truncated: total_match_count > returned_match_count,
        paths,
        regions,
        symbols: infer_symbols_from_pattern(query),
        groups: grouped,
        compact_preview,
        diagnostics,
        engine: Some(SearchEngineStats {
            engine: "legacy_rg".to_string(),
            index_generation: None,
            base_commit: None,
            plan_kind: None,
            planner_fallback: None,
            stale_reason: None,
            candidates_examined: resolved_path_count,
            candidate_files: resolved_path_count,
            verified_files: resolved_path_count,
            index_lookups: 0,
            postings_bytes_read: 0,
            fallback_reason: Some("daemon indexed search unavailable".to_string()),
        }),
    })
}

fn collect_matches_with_rg(
    root: &Path,
    request: &SearchRequest,
    query: &str,
    resolved_paths: &[String],
    diagnostics: &mut Vec<String>,
    rg_binary: &str,
) -> Result<Option<Vec<SearchMatch>>> {
    let mut command = Command::new(rg_binary);
    command
        .current_dir(root)
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color")
        .arg("never");
    if request.fixed_string {
        command.arg("-F");
    }
    if matches!(request.case_sensitive, Some(false)) {
        command.arg("-i");
    }
    if request.whole_word {
        command.arg("-w");
    }
    if let Some(context_lines) = request.context_lines {
        command.arg("-C").arg(context_lines.to_string());
    }
    if let Some(max_matches_per_file) = request.max_matches_per_file {
        command
            .arg("--max-count")
            .arg(max_matches_per_file.to_string());
    }
    command.arg(query);
    for path in resolved_paths {
        command.arg(path);
    }

    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("search command failed"),
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    // Status 1 means no match; anything else is a failure whose stderr (such
    // as a regex parse location) is the only actionable detail.
    if !matches!(output.status.code(), Some(0 | 1)) {
        anyhow::bail!(
            "search command exited with status {}{}",
            output.status,
            failed_search_stderr(&stderr)
        );
    }
    if !stderr.is_empty() {
        diagnostics.push(stderr);
    }
    let single_resolved_path = (resolved_paths.len() == 1
        && root.join(&resolved_paths[0]).is_file())
    .then(|| resolved_paths[0].clone());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() || line == "--" {
            continue;
        }
        let Some((path, line_no, text)) =
            parse_grep_output_line(root, line, single_resolved_path.as_deref())
        else {
            continue;
        };
        matches.push(SearchMatch {
            path,
            line: line_no,
            text,
        });
    }
    Ok(Some(matches))
}

/// Formats child stderr for a failed search, bounded to
/// [`MAX_FAILED_SEARCH_STDERR_BYTES`] at a UTF-8 character boundary.
fn failed_search_stderr(stderr: &str) -> String {
    if stderr.is_empty() {
        return String::new();
    }
    if stderr.len() <= MAX_FAILED_SEARCH_STDERR_BYTES {
        return format!("; stderr:\n{stderr}");
    }
    let mut end = MAX_FAILED_SEARCH_STDERR_BYTES;
    while !stderr.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "; stderr:\n{}\n[stderr truncated to {end} of {} bytes]",
        &stderr[..end],
        stderr.len()
    )
}

fn collect_matches_without_rg(
    root: &Path,
    request: &SearchRequest,
    query: &str,
    resolved_paths: &[String],
) -> Result<Vec<SearchMatch>> {
    let matcher = compile_fallback_matcher(request, query)?;
    let max_matches_per_file = request.max_matches_per_file;
    let mut matches = Vec::new();
    for path in fallback_file_paths(root, resolved_paths)? {
        let bytes = fs::read(root.join(&path))
            .with_context(|| format!("failed to read '{}'", root.join(&path).display()))?;
        if bytes.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut matches_in_file = 0usize;
        for (line_idx, line) in text.lines().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            matches.push(SearchMatch {
                path: path.clone(),
                line: line_idx + 1,
                text: line.to_string(),
            });
            matches_in_file = matches_in_file.saturating_add(1);
            if max_matches_per_file.is_some_and(|limit| matches_in_file >= limit) {
                break;
            }
        }
    }
    Ok(matches)
}

fn compile_fallback_matcher(request: &SearchRequest, query: &str) -> Result<regex::Regex> {
    let pattern = if request.fixed_string {
        regex::escape(query)
    } else {
        query.to_string()
    };
    let pattern = if request.whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(matches!(request.case_sensitive, Some(false)))
        .build()
        .with_context(|| format!("unsupported regex syntax for fallback search: {query}"))
}

fn fallback_file_paths(root: &Path, resolved_paths: &[String]) -> Result<Vec<String>> {
    let mut files = BTreeSet::new();
    if resolved_paths.is_empty() {
        collect_walk_files(root, root, &mut files)?;
    } else {
        for path in resolved_paths {
            collect_walk_files(root, &root.join(path), &mut files)?;
        }
    }
    Ok(files.into_iter().collect())
}

fn collect_walk_files(root: &Path, target: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    let walker = ignore::WalkBuilder::new(target).build();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            files.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn search_with_test_rg(
    root: &Path,
    request: &SearchRequest,
    rg_binary: &Path,
) -> Result<SearchResult> {
    let rg_binary = rg_binary.to_str().context("test rg path must be UTF-8")?;
    search_with_rg_binary(root, request, rg_binary)
}

#[cfg(test)]
pub(crate) fn search_without_rg(root: &Path, request: &SearchRequest) -> Result<SearchResult> {
    search_with_rg_binary(root, request, "__packet28_missing_rg_binary__")
}

pub(crate) fn render_search_compact_preview(
    total_match_count: usize,
    groups: &[SearchGroup],
    _max_total_matches: usize,
) -> String {
    if total_match_count == 0 {
        return "Search found 0 matches.".to_string();
    }
    let mut lines = vec![format!(
        "Search found {} matches in {} files.",
        total_match_count,
        groups.len()
    )];
    for group in groups.iter().take(DEFAULT_DISPLAYED_GROUPS_IN_PREVIEW) {
        lines.push(format!("- {} ({})", group.path, group.match_count));
        for item in group
            .matches
            .iter()
            .take(DEFAULT_DISPLAYED_MATCH_LINES_IN_PREVIEW)
        {
            lines.push(format!(
                "  {}:{}:{}",
                item.path,
                item.line,
                compact_match_preview_text(&item.text)
            ));
        }
        if group.matches.len() > DEFAULT_DISPLAYED_MATCH_LINES_IN_PREVIEW {
            lines.push(format!(
                "  +{} more match(es)",
                group.matches.len() - DEFAULT_DISPLAYED_MATCH_LINES_IN_PREVIEW
            ));
        }
    }
    if groups.len() > DEFAULT_DISPLAYED_GROUPS_IN_PREVIEW {
        lines.push(format!(
            "+{} more files",
            groups.len() - DEFAULT_DISPLAYED_GROUPS_IN_PREVIEW
        ));
    }
    lines.join("\n")
}

fn compact_match_preview_text(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= DEFAULT_MATCH_PREVIEW_TEXT_LIMIT {
        return compact;
    }
    compact
        .chars()
        .take(DEFAULT_MATCH_PREVIEW_TEXT_LIMIT.saturating_sub(3))
        .collect::<String>()
        + "..."
}

fn parse_line_range_spec(value: &str) -> Option<(usize, usize)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (start, end) = if let Some((start, end)) = trimmed.split_once('-') {
        (
            start.trim().parse::<usize>().ok()?,
            end.trim().parse::<usize>().ok()?,
        )
    } else {
        let line = trimmed.parse::<usize>().ok()?;
        (line, line)
    };
    if start == 0 || end == 0 {
        return None;
    }
    Some((start.min(end), start.max(end)))
}

fn path_exists_under_root(root: &Path, path: &str) -> bool {
    !path.is_empty() && root.join(path).exists()
}

fn resolve_capture_path_suffix(root: &Path, path: &str) -> Option<String> {
    let needle = normalize_capture_path(root, path);
    if needle.is_empty() {
        return None;
    }
    let mut matches = BTreeSet::new();
    collect_suffix_matches(root, root, &needle, &mut matches);
    if matches.len() > 1 {
        return None;
    }
    matches.into_iter().next()
}

fn collect_suffix_matches(
    root: &Path,
    current: &Path,
    needle: &str,
    matches: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_suffix_matches(root, &path, needle, matches);
            if matches.len() > 1 {
                return;
            }
            continue;
        }
        let Ok(stripped) = path.strip_prefix(root) else {
            continue;
        };
        let normalized = stripped.to_string_lossy().replace('\\', "/");
        if normalized == needle || normalized.ends_with(&format!("/{needle}")) {
            matches.insert(normalized);
            if matches.len() > 1 {
                return;
            }
        }
    }
}

fn resolve_requested_paths(root: &Path, requested_paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    for original in requested_paths {
        let normalized = normalize_capture_path(root, original);
        if normalized.is_empty() {
            diagnostics.push(format!("ignored invalid path input: {}", original.trim()));
            continue;
        }
        let final_path = if path_exists_under_root(root, &normalized) {
            normalized
        } else if let Some(candidate) = resolve_capture_path_suffix(root, &normalized) {
            diagnostics.push(format!(
                "resolved missing path '{}' to '{}'",
                original.trim(),
                candidate
            ));
            candidate
        } else {
            diagnostics.push(format!(
                "path '{}' does not exist under daemon root {}",
                original.trim(),
                root.display()
            ));
            continue;
        };
        if seen.insert(final_path.clone()) {
            resolved.push(final_path);
        }
    }
    (resolved, diagnostics)
}

pub(crate) fn parse_grep_output_line(
    root: &Path,
    line: &str,
    single_resolved_path: Option<&str>,
) -> Option<(String, usize, String)> {
    let mut path_parts = line.splitn(3, ':');
    let first = path_parts.next()?;
    let second = path_parts.next()?;
    if let Ok(line_no) = second.parse::<usize>() {
        let text = path_parts.next().unwrap_or_default().to_string();
        let normalized_path = normalize_capture_path(root, first);
        if !normalized_path.is_empty() {
            return Some((normalized_path, line_no, text));
        }
    }
    if let Some(only_path) = single_resolved_path {
        let mut parts = line.splitn(2, ':');
        let line_no = parts.next()?.parse::<usize>().ok()?;
        let text = parts.next().unwrap_or_default().to_string();
        return Some((only_path.to_string(), line_no, text));
    }
    let mut parts = line.splitn(3, ':');
    let path = parts.next()?;
    let line_no = parts.next()?.parse::<usize>().ok()?;
    let text = parts.next().unwrap_or_default().to_string();
    let normalized_path = normalize_capture_path(root, path);
    if normalized_path.is_empty() {
        return None;
    }
    Some((normalized_path, line_no, text))
}
