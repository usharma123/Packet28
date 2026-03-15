use super::*;
use std::io::Read;

fn truncate_evidence_line(line: &str, max_len: usize) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= max_len {
        trimmed.to_string()
    } else {
        let shortened = trimmed
            .chars()
            .take(max_len.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceMatchKind {
    DefinesSymbol,
    CallsSymbol,
    ReferencesSymbol,
    Signature,
    Fallback,
}

impl EvidenceMatchKind {
    fn priority(self) -> u8 {
        match self {
            EvidenceMatchKind::DefinesSymbol => 6,
            EvidenceMatchKind::CallsSymbol => 5,
            EvidenceMatchKind::ReferencesSymbol => 4,
            EvidenceMatchKind::Signature => 2,
            EvidenceMatchKind::Fallback => 1,
        }
    }
}

impl Default for EvidenceMatchKind {
    fn default() -> Self {
        Self::Fallback
    }
}

#[derive(Debug, Clone, Default)]
struct CodeEvidenceMatch {
    line_no: usize,
    priority: u8,
    match_kind: EvidenceMatchKind,
    #[cfg(test)]
    matched_symbol: Option<String>,
    from_region_hint: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodeEvidenceSummary {
    pub(crate) rendered_lines: Vec<String>,
    #[cfg(test)]
    pub(crate) first_match_line: Option<usize>,
    #[cfg(test)]
    pub(crate) primary_match_symbol: Option<String>,
    pub(crate) primary_match_kind: Option<EvidenceMatchKind>,
    pub(crate) from_region_hint: bool,
    #[cfg(test)]
    pub(crate) from_tool_result_path: bool,
}

impl CodeEvidenceSummary {
    pub(crate) fn has_quality_match(&self) -> bool {
        matches!(
            self.primary_match_kind,
            Some(
                EvidenceMatchKind::DefinesSymbol
                    | EvidenceMatchKind::CallsSymbol
                    | EvidenceMatchKind::ReferencesSymbol
            )
        )
    }
}

fn build_code_evidence_summary(
    rendered_lines: Vec<String>,
    from_region_hint: bool,
    matches: &[CodeEvidenceMatch],
    _provenance: &[ToolResultProvenance],
) -> CodeEvidenceSummary {
    let mut summary = CodeEvidenceSummary {
        rendered_lines,
        ..CodeEvidenceSummary::default()
    };
    summary.from_region_hint = from_region_hint;
    #[cfg(test)]
    {
        summary.first_match_line = matches.first().map(|matched| matched.line_no);
        summary.primary_match_symbol = matches
            .iter()
            .find_map(|matched| matched.matched_symbol.clone());
        summary.from_tool_result_path = !_provenance.is_empty();
    }
    summary.primary_match_kind = matches.first().map(|matched| matched.match_kind);
    summary
}

pub(crate) fn looks_like_signature(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        return false;
    }
    let prefixes = [
        "pub fn ",
        "fn ",
        "pub struct ",
        "struct ",
        "pub enum ",
        "enum ",
        "pub trait ",
        "trait ",
        "impl ",
        "pub mod ",
        "mod ",
        "class ",
        "interface ",
        "export function ",
        "export class ",
        "def ",
    ];
    let looks_like_java_method = trimmed.contains('(')
        && trimmed.contains(')')
        && trimmed.ends_with('{')
        && !trimmed.ends_with(");")
        && !trimmed.starts_with('@')
        && ![
            "if ", "for ", "while ", "switch ", "catch ", "return ", "new ",
        ]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix));
    prefixes.iter().any(|prefix| trimmed.starts_with(prefix))
        || (trimmed.contains(" fn ")
            || trimmed.contains(" class ")
            || trimmed.contains(" interface "))
        || looks_like_java_method
}

fn looks_like_low_signal_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
        || trimmed.starts_with('*')
        || trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("package ")
}

fn is_comment_reference_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
        || trimmed.starts_with('*')
        || trimmed.starts_with("//")
        || trimmed.starts_with('#')
}

pub(crate) fn contains_identifier_term(line: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    let line_lower = line.to_ascii_lowercase();
    let term_lower = term.to_ascii_lowercase();
    let mut start_at = 0;
    while let Some(found) = line_lower[start_at..].find(&term_lower) {
        let start = start_at + found;
        let end = start + term_lower.len();
        let prev = line_lower[..start].chars().next_back();
        let next = line_lower[end..].chars().next();
        let prev_is_ident = prev.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        let next_is_ident = next.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if !prev_is_ident && !next_is_ident {
            return true;
        }
        start_at = start + 1;
    }
    false
}

fn looks_like_symbol_call(line: &str, symbol: &str) -> bool {
    if symbol.trim().is_empty() || looks_like_signature(line) {
        return false;
    }
    let line_lower = line.to_ascii_lowercase();
    let symbol_lower = symbol.to_ascii_lowercase();
    let mut start_at = 0;
    while let Some(found) = line_lower[start_at..].find(&symbol_lower) {
        let start = start_at + found;
        let end = start + symbol_lower.len();
        let prev = line_lower[..start].chars().next_back();
        let next = line_lower[end..].chars().next();
        let prev_is_ident = prev.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        let next_is_ident = next.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if !prev_is_ident && !next_is_ident {
            let trailing = line_lower[end..].trim_start();
            if trailing.starts_with('(') {
                return true;
            }
        }
        start_at = start + 1;
    }
    false
}

fn looks_like_type_declaration(line: &str) -> bool {
    let trimmed = line.trim_start();
    [
        "class ",
        "interface ",
        "enum ",
        "struct ",
        "trait ",
        "public class ",
        "public interface ",
        "public enum ",
        "public record ",
        "final class ",
        "abstract class ",
        "record ",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
        || trimmed.contains(" class ")
        || trimmed.contains(" interface ")
        || trimmed.contains(" enum ")
        || trimmed.contains(" struct ")
        || trimmed.contains(" trait ")
}

fn signature_search_slice(line: &str) -> &str {
    let trimmed = line.trim();
    if let Some(index) = trimmed.find('{') {
        &trimmed[..index]
    } else {
        trimmed
    }
}

pub(crate) fn classify_symbol_match(line: &str, symbol: &str) -> EvidenceMatchKind {
    if looks_like_signature(line) && contains_identifier_term(signature_search_slice(line), symbol)
    {
        EvidenceMatchKind::DefinesSymbol
    } else if looks_like_symbol_call(line, symbol) {
        EvidenceMatchKind::CallsSymbol
    } else {
        EvidenceMatchKind::ReferencesSymbol
    }
}

fn match_query_focus_line(line: &str, query_focus: &QueryFocus) -> Option<CodeEvidenceMatch> {
    if let Some(symbol) = query_focus
        .symbol_terms
        .iter()
        .find(|symbol| contains_identifier_term(line, symbol))
    {
        let match_kind = classify_symbol_match(line, symbol);
        return Some(CodeEvidenceMatch {
            line_no: 0,
            priority: match_kind.priority(),
            match_kind,
            from_region_hint: false,
            #[cfg(test)]
            matched_symbol: Some(symbol.clone()),
        });
    }
    if let Some(symbol) = query_focus
        .full_symbol_terms
        .iter()
        .find(|symbol| contains_identifier_term(line, symbol))
    {
        let match_kind = classify_symbol_match(line, symbol);
        return Some(CodeEvidenceMatch {
            line_no: 0,
            priority: match_kind.priority(),
            match_kind,
            from_region_hint: false,
            #[cfg(test)]
            matched_symbol: Some(symbol.clone()),
        });
    }
    if looks_like_signature(line)
        && query_focus.symbol_terms.is_empty()
        && query_focus.full_symbol_terms.is_empty()
    {
        return Some(CodeEvidenceMatch {
            line_no: 0,
            priority: EvidenceMatchKind::Signature.priority(),
            match_kind: EvidenceMatchKind::Signature,
            from_region_hint: false,
            #[cfg(test)]
            matched_symbol: None,
        });
    }
    None
}

fn collapse_evidence_windows(
    matches: &[CodeEvidenceMatch],
    total_lines: usize,
) -> Vec<(usize, usize)> {
    let windows = matches
        .iter()
        .map(|matched| {
            let start = if matched.priority >= 4 {
                matched.line_no.max(1)
            } else {
                matched.line_no.saturating_sub(1).max(1)
            };
            let end = (matched.line_no + 1).min(total_lines.max(1));
            (start, end)
        })
        .collect::<Vec<_>>();
    let mut collapsed: Vec<(usize, usize)> = Vec::new();
    for (start, end) in windows {
        if let Some((_, current_end)) = collapsed.last_mut() {
            if start <= *current_end + 1 {
                *current_end = (*current_end).max(end);
                continue;
            }
        }
        collapsed.push((start, end));
    }
    collapsed
}

fn parse_region_line_range(value: &str) -> Option<(usize, usize)> {
    let trimmed = value.trim().trim_start_matches('L');
    if trimmed.is_empty() {
        return None;
    }
    let (start, end) = if let Some((start, end)) = trimmed.split_once('-') {
        (
            start.trim().trim_start_matches('L'),
            end.trim().trim_start_matches('L'),
        )
    } else {
        (trimmed, trimmed)
    };
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    if start == 0 || end == 0 {
        return None;
    }
    Some((start.min(end), start.max(end)))
}

fn parse_region_hint_for_path(region: &str, path: &str) -> Option<(usize, usize)> {
    let trimmed = region.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(range) = parse_region_line_range(trimmed) {
        return Some(range);
    }
    let (maybe_path, maybe_range) = trimmed.rsplit_once(':')?;
    let normalized_path = maybe_path.trim().replace('\\', "/");
    let current_path = path.replace('\\', "/");
    if normalized_path != current_path {
        return None;
    }
    parse_region_line_range(maybe_range)
}

pub(crate) fn build_reducer_search_evidence(
    state: Option<&Arc<Mutex<DaemonState>>>,
    root: &Path,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
    query_focus: &QueryFocus,
    reducer_files: &[ReducerSearchFile],
    max_lines: usize,
) -> BTreeMap<String, CodeEvidenceSummary> {
    let candidate_paths = reducer_files
        .iter()
        .map(|file| file.path.clone())
        .chain(
            snapshot
                .recent_tool_invocations
                .iter()
                .flat_map(|invocation| invocation.paths.iter().cloned()),
        )
        .chain(snapshot.focus_paths.iter().cloned())
        .chain(snapshot.checkpoint_focus_paths.iter().cloned())
        .chain(request.focus_paths.iter().cloned())
        .collect::<BTreeSet<_>>();
    let search_file_map = reducer_files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    candidate_paths
        .iter()
        .map(|path| {
            let mut provenance = collect_tool_result_provenance_for_path(snapshot, path);
            if let Some(file) = search_file_map.get(path) {
                let preferred_regions = preferred_search_regions(file);
                if !preferred_regions.is_empty() {
                    provenance.push(ToolResultProvenance {
                        regions: preferred_regions,
                    });
                }
            }
            (
                path.clone(),
                extract_code_evidence_cached(
                    state,
                    root,
                    path,
                    query_focus,
                    provenance.as_slice(),
                    3,
                    max_lines,
                ),
            )
        })
        .collect()
}

fn collect_region_hint_lines(
    provenance: &[ToolResultProvenance],
    path: &str,
    total_lines: usize,
) -> BTreeSet<usize> {
    let mut hinted = BTreeSet::new();
    for hint in provenance {
        for region in &hint.regions {
            if let Some((start, end)) = parse_region_hint_for_path(region, path) {
                for line_no in start.min(total_lines)..=end.min(total_lines) {
                    hinted.insert(line_no);
                }
            }
        }
    }
    hinted
}

fn collect_evidence_matches(
    lines: &[&str],
    query_focus: &QueryFocus,
    candidate_lines: Option<&BTreeSet<usize>>,
    from_region_hint: bool,
) -> Vec<CodeEvidenceMatch> {
    let mut matches = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        if candidate_lines.is_some_and(|allowed| !allowed.contains(&line_no)) {
            continue;
        }
        let mut matched = match_query_focus_line(line, query_focus);
        if let Some(current) = matched.as_mut() {
            current.line_no = line_no;
            current.from_region_hint = from_region_hint;
            if from_region_hint {
                current.priority = current.priority.saturating_add(1);
            }
        } else if !query_focus.symbol_terms.is_empty() || !query_focus.full_symbol_terms.is_empty()
        {
            continue;
        }
        if looks_like_low_signal_line(line)
            && matched.as_ref().is_some_and(|current| {
                !matches!(
                    current.match_kind,
                    EvidenceMatchKind::DefinesSymbol | EvidenceMatchKind::CallsSymbol
                ) && !is_comment_reference_line(line)
            })
        {
            continue;
        }
        if let Some(matched) = matched {
            matches.push(matched);
        }
    }
    let has_symbol_focus =
        !query_focus.symbol_terms.is_empty() || !query_focus.full_symbol_terms.is_empty();
    let has_non_type_symbol_match = has_symbol_focus
        && matches.iter().any(|matched| {
            !looks_like_type_declaration(
                lines
                    .get(matched.line_no.saturating_sub(1))
                    .copied()
                    .unwrap_or_default(),
            ) && matches!(
                matched.match_kind,
                EvidenceMatchKind::DefinesSymbol
                    | EvidenceMatchKind::CallsSymbol
                    | EvidenceMatchKind::ReferencesSymbol
            )
        });
    if has_non_type_symbol_match {
        matches.retain(|matched| {
            !looks_like_type_declaration(
                lines
                    .get(matched.line_no.saturating_sub(1))
                    .copied()
                    .unwrap_or_default(),
            ) || !matches!(
                matched.match_kind,
                EvidenceMatchKind::DefinesSymbol | EvidenceMatchKind::ReferencesSymbol
            )
        });
    }
    matches
}

#[cfg(test)]
pub(crate) fn extract_code_evidence(
    root: &Path,
    relative_path: &str,
    query_focus: &QueryFocus,
    provenance: &[ToolResultProvenance],
    max_windows: usize,
    max_lines: usize,
) -> CodeEvidenceSummary {
    extract_code_evidence_cached(
        None,
        root,
        relative_path,
        query_focus,
        provenance,
        max_windows,
        max_lines,
    )
}

fn extract_code_evidence_cached(
    state: Option<&Arc<Mutex<DaemonState>>>,
    root: &Path,
    relative_path: &str,
    query_focus: &QueryFocus,
    provenance: &[ToolResultProvenance],
    max_windows: usize,
    max_lines: usize,
) -> CodeEvidenceSummary {
    let Ok(lines) = load_source_file_lines(state, root, relative_path) else {
        return CodeEvidenceSummary::default();
    };
    let lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
    let hint_lines = collect_region_hint_lines(provenance, relative_path, lines.len());
    let mut matches = if !hint_lines.is_empty() {
        collect_evidence_matches(lines.as_slice(), query_focus, Some(&hint_lines), true)
    } else {
        Vec::new()
    };
    if matches.is_empty() {
        matches = collect_evidence_matches(lines.as_slice(), query_focus, None, false);
    }
    if matches.is_empty() && !hint_lines.is_empty() {
        for line_no in &hint_lines {
            let Some(line) = lines.get(line_no.saturating_sub(1)) else {
                continue;
            };
            if looks_like_low_signal_line(line) {
                continue;
            }
            matches.push(CodeEvidenceMatch {
                line_no: *line_no,
                priority: EvidenceMatchKind::Signature.priority(),
                match_kind: if looks_like_signature(line) {
                    EvidenceMatchKind::Signature
                } else {
                    EvidenceMatchKind::Fallback
                },
                from_region_hint: true,
                #[cfg(test)]
                matched_symbol: None,
            });
            break;
        }
    }

    if matches.is_empty()
        && (query_focus.symbol_terms.is_empty() && query_focus.full_symbol_terms.is_empty())
    {
        let fallback_candidates = if hint_lines.is_empty() {
            None
        } else {
            Some(&hint_lines)
        };
        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if fallback_candidates.is_some_and(|allowed| !allowed.contains(&line_no)) {
                continue;
            }
            if looks_like_low_signal_line(line) {
                continue;
            }
            matches.push(CodeEvidenceMatch {
                line_no,
                priority: EvidenceMatchKind::Fallback.priority(),
                match_kind: EvidenceMatchKind::Fallback,
                from_region_hint: fallback_candidates.is_some(),
                #[cfg(test)]
                matched_symbol: None,
            });
            break;
        }
        if matches.is_empty() && fallback_candidates.is_some() {
            for (idx, line) in lines.iter().enumerate() {
                if looks_like_low_signal_line(line) {
                    continue;
                }
                matches.push(CodeEvidenceMatch {
                    line_no: idx + 1,
                    priority: EvidenceMatchKind::Fallback.priority(),
                    match_kind: EvidenceMatchKind::Fallback,
                    from_region_hint: false,
                    #[cfg(test)]
                    matched_symbol: None,
                });
                break;
            }
        }
    }

    if matches.is_empty() {
        return CodeEvidenceSummary::default();
    }

    matches.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.from_region_hint.cmp(&a.from_region_hint))
            .then_with(|| a.line_no.cmp(&b.line_no))
    });
    let primary_from_region_hint = matches
        .first()
        .is_some_and(|matched| matched.from_region_hint);
    let windows = collapse_evidence_windows(&matches, lines.len())
        .into_iter()
        .take(max_windows)
        .collect::<Vec<_>>();
    let mut rendered_lines = Vec::new();
    for (start, end) in windows {
        for line_no in start..=end {
            let Some(line) = lines.get(line_no - 1) else {
                continue;
            };
            if looks_like_low_signal_line(line)
                && !matches.iter().any(|matched| matched.line_no == line_no)
            {
                continue;
            }
            rendered_lines.push(format!(
                "- {relative_path}:{} {}",
                line_no,
                truncate_evidence_line(line, 120)
            ));
            if rendered_lines.len() >= max_lines {
                return build_code_evidence_summary(
                    rendered_lines,
                    primary_from_region_hint,
                    &matches,
                    provenance,
                );
            }
        }
    }

    build_code_evidence_summary(
        rendered_lines,
        primary_from_region_hint,
        &matches,
        provenance,
    )
}

fn load_source_file_lines(
    state: Option<&Arc<Mutex<DaemonState>>>,
    root: &Path,
    relative_path: &str,
) -> Result<Vec<String>> {
    let full_path = root.join(relative_path);
    let mut file = fs::File::open(&full_path)
        .with_context(|| format!("failed to open '{}'", full_path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to read metadata for '{}'", full_path.display()))?;
    let size = metadata.len();
    let mtime_secs = metadata_mtime_secs(&metadata);
    if let Some(state) = state {
        if let Some(cached) = state
            .lock()
            .map_err(lock_err)?
            .source_file_cache
            .get(relative_path)
            .cloned()
        {
            if cached.size == size && cached.mtime_secs == mtime_secs {
                return Ok(cached.lines);
            }
        }
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("failed to read '{}'", full_path.display()))?;
        let lines = contents
            .lines()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let refreshed = fs::metadata(&full_path)
            .with_context(|| format!("failed to refresh metadata for '{}'", full_path.display()))?;
        if refreshed.len() == size && metadata_mtime_secs(&refreshed) == mtime_secs {
            state.lock().map_err(lock_err)?.source_file_cache.insert(
                relative_path.to_string(),
                CachedSourceFile {
                    size,
                    mtime_secs,
                    lines: lines.clone(),
                },
            );
        }
        return Ok(lines);
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read '{}'", full_path.display()))?;
    Ok(contents.lines().map(|line| line.to_string()).collect())
}
