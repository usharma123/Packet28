use super::*;

pub(crate) const SEARCH_PLAN_MAX_CANDIDATES: usize = 8;
pub(crate) const SEARCH_PLAN_PHASE1_MAX_CANDIDATES: usize = 5;
pub(crate) const SEARCH_PLAN_TEXT_FALLBACK_LIMIT: usize = 3;
pub(crate) const SEARCH_BROKER_MAX_MATCHES_PER_FILE: usize = 8;
pub(crate) const SEARCH_BROKER_MAX_TOTAL_MATCHES: usize = 32;

fn tokenize_task_text(task_text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    task_text
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| !is_low_signal_query_token(token))
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

fn is_low_signal_query_token(token: &str) -> bool {
    matches!(
        token,
        "about"
            | "across"
            | "all"
            | "and"
            | "are"
            | "code"
            | "codebase"
            | "defined"
            | "do"
            | "does"
            | "file"
            | "files"
            | "find"
            | "for"
            | "from"
            | "function"
            | "functions"
            | "how"
            | "into"
            | "is"
            | "line"
            | "lines"
            | "me"
            | "method"
            | "methods"
            | "not"
            | "show"
            | "symbol"
            | "symbols"
            | "task"
            | "test"
            | "tests"
            | "the"
            | "this"
            | "those"
            | "usage"
            | "usages"
            | "used"
            | "uses"
            | "what"
            | "when"
            | "where"
            | "which"
            | "with"
    )
}

#[derive(Debug, Clone, Default)]
pub(crate) struct QueryFocus {
    pub(crate) raw_query: Option<String>,
    pub(crate) text_tokens: Vec<String>,
    pub(crate) full_symbol_terms: Vec<String>,
    pub(crate) symbol_terms: Vec<String>,
    pub(crate) path_terms: Vec<String>,
}

fn add_focus_symbol_terms(
    full_symbol_terms: &mut Vec<String>,
    symbol_terms: &mut Vec<String>,
    seen_full: &mut HashSet<String>,
    seen_symbols: &mut HashSet<String>,
    raw_symbol: &str,
) {
    let cleaned = trim_query_fragment(raw_symbol);
    if cleaned.is_empty() {
        return;
    }
    let normalized = cleaned.to_ascii_lowercase();
    if !is_low_signal_query_token(&normalized) && seen_full.insert(normalized) {
        full_symbol_terms.push(cleaned.clone());
    }
    for piece in cleaned
        .replace("::", ".")
        .replace(['/', '\\', '.', '_', '-'], " ")
        .split_whitespace()
    {
        let lowered = piece.to_ascii_lowercase();
        if piece.len() >= 3 && !is_low_signal_query_token(&lowered) && seen_symbols.insert(lowered)
        {
            symbol_terms.push(piece.to_string());
        }
    }
}

pub(crate) fn merge_query_focus_with_symbols(
    mut query_focus: QueryFocus,
    focus_symbols: &[String],
) -> QueryFocus {
    let mut seen_full = query_focus
        .full_symbol_terms
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut seen_symbols = query_focus
        .symbol_terms
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for focus_symbol in focus_symbols {
        add_focus_symbol_terms(
            &mut query_focus.full_symbol_terms,
            &mut query_focus.symbol_terms,
            &mut seen_full,
            &mut seen_symbols,
            focus_symbol,
        );
    }
    query_focus
}

pub(crate) fn derive_query_focus(query: Option<&str>) -> QueryFocus {
    let raw_query = query
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let Some(raw_query) = raw_query else {
        return QueryFocus::default();
    };

    let text_tokens = tokenize_task_text(&raw_query);
    let mut full_symbol_terms = Vec::new();
    let mut symbol_terms = Vec::new();
    let mut path_terms = Vec::new();
    let mut seen_full = HashSet::new();
    let mut seen_symbols = HashSet::new();
    let mut seen_paths = HashSet::new();

    for raw_part in raw_query.split_whitespace() {
        let cleaned = trim_query_fragment(raw_part);
        if cleaned.is_empty() {
            continue;
        }
        if looks_like_query_path(&cleaned) && seen_paths.insert(cleaned.to_ascii_lowercase()) {
            path_terms.push(cleaned.clone());
        }
        if looks_like_symbol_term(&cleaned) {
            add_focus_symbol_terms(
                &mut full_symbol_terms,
                &mut symbol_terms,
                &mut seen_full,
                &mut seen_symbols,
                &cleaned,
            );
        }
    }

    for token in &text_tokens {
        if token.len() >= 3 && seen_symbols.insert(token.to_ascii_lowercase()) {
            symbol_terms.push(token.clone());
        }
    }

    QueryFocus {
        raw_query: Some(raw_query),
        text_tokens,
        full_symbol_terms,
        symbol_terms,
        path_terms,
    }
}

fn trim_query_fragment(fragment: &str) -> String {
    fragment
        .trim_matches(|ch: char| {
            !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | '-' | '.' | '/' | '\\' | ':')
        })
        .trim_end_matches("()")
        .to_string()
}

fn looks_like_query_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".json", ".md", ".py", ".java", ".go", ".kt",
        ]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn looks_like_symbol_term(value: &str) -> bool {
    value.contains('.')
        || value.contains("::")
        || value.contains('_')
        || value.chars().any(|ch| ch.is_ascii_uppercase())
}

fn scope_group(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some((prefix, _)) = normalized.split_once("/src/") {
        return prefix.to_string();
    }
    Path::new(&normalized)
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn parent_dir(path: &str) -> String {
    Path::new(path)
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn role_file_weight(path: &str) -> usize {
    let Some(file_name) = Path::new(path).file_name().and_then(|value| value.to_str()) else {
        return 0;
    };
    if file_name.starts_with("cmd_") || file_name == "report.rs" {
        3
    } else if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        2
    } else {
        0
    }
}

pub(crate) fn expand_scope_paths(
    task_text: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    primary_paths: &[String],
    explicit_symbols: &[String],
    max_paths: usize,
) -> Vec<String> {
    if primary_paths.is_empty() {
        return Vec::new();
    }

    let primary_set = primary_paths
        .iter()
        .map(|path| path.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let primary_scopes = primary_paths
        .iter()
        .map(|path| scope_group(path))
        .collect::<HashSet<_>>();
    let primary_dirs = primary_paths
        .iter()
        .map(|path| parent_dir(path))
        .collect::<HashSet<_>>();
    let task_tokens = tokenize_task_text(task_text);
    let explicit_symbols = explicit_symbols
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    let mut edge_counts = HashMap::<String, usize>::new();
    for edge in &rich_map.edges {
        let from_is_primary = primary_set.contains(&edge.from.to_ascii_lowercase());
        let to_is_primary = primary_set.contains(&edge.to.to_ascii_lowercase());
        if from_is_primary && !to_is_primary {
            *edge_counts.entry(edge.to.clone()).or_insert(0) += 1;
        }
        if to_is_primary && !from_is_primary {
            *edge_counts.entry(edge.from.clone()).or_insert(0) += 1;
        }
    }

    let mut symbol_hits = HashMap::<String, usize>::new();
    for symbol in &rich_map.symbols_ranked {
        let symbol_name = symbol.name.to_ascii_lowercase();
        if task_tokens
            .iter()
            .any(|token| symbol_name.contains(token.as_str()))
            || explicit_symbols
                .iter()
                .any(|token| symbol_name.contains(token.as_str()))
        {
            *symbol_hits.entry(symbol.file.clone()).or_insert(0) += 1;
        }
    }

    let mut scored = rich_map
        .files_ranked
        .iter()
        .map(|file| {
            let lower_path = file.path.to_ascii_lowercase();
            let scope = scope_group(&file.path);
            let dir = parent_dir(&file.path);
            let path_token_hits = task_tokens
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();
            let explicit_symbol_hits = explicit_symbols
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();

            let mut score = 0usize;
            if primary_set.contains(&lower_path) {
                score += 1000;
            }
            score += edge_counts.get(&file.path).copied().unwrap_or(0) * 220;
            if primary_scopes.contains(&scope) {
                score += 120;
            }
            if primary_dirs.contains(&dir) {
                score += 60;
            }
            score += (path_token_hits + explicit_symbol_hits) * 35;
            score += symbol_hits.get(&file.path).copied().unwrap_or(0) * 30;
            score += role_file_weight(&file.path)
                * if primary_scopes.contains(&scope) { 25 } else { 10 };

            (score, file.score, file.path.clone())
        })
        .filter(|(score, _, _)| *score > 0)
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .map(|(_, _, path)| path)
        .take(max_paths.max(primary_paths.len()))
        .collect()
}

pub(crate) fn derive_broker_focus_symbols(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
) -> Vec<String> {
    let query_focus = derive_query_focus(request.query.as_deref());
    let snapshot_symbols = if request.focus_symbols.is_empty() {
        merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols)
    } else {
        snapshot.focus_symbols.clone()
    };
    let explicit = merged_unique(&snapshot_symbols, &request.focus_symbols);
    merged_unique(&explicit, &query_focus.symbol_terms)
}

pub(crate) fn derive_broker_focus_paths(
    _state: &Arc<Mutex<DaemonState>>,
    _root: &Path,
    objective: Option<&str>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
    max_paths: usize,
) -> Result<Vec<String>> {
    let query_focus = derive_query_focus(objective.or(request.query.as_deref()));
    let snapshot_paths = if request.focus_paths.is_empty() {
        merged_unique(&snapshot.focus_paths, &snapshot.checkpoint_focus_paths)
    } else {
        snapshot.focus_paths.clone()
    };
    let explicit_paths = merged_unique(
        &merged_unique(&snapshot_paths, &request.focus_paths),
        &query_focus.path_terms,
    );
    let explicit_symbols = derive_broker_focus_symbols(snapshot, request);
    if explicit_paths.is_empty() && explicit_symbols.is_empty() && objective.is_none() {
        return Ok(Vec::new());
    }
    let mut merged = explicit_paths;
    if merged.len() > max_paths {
        merged.truncate(max_paths);
    }
    Ok(merged)
}

pub(crate) fn infer_scope_paths(
    task_text: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    explicit_paths: &[String],
    explicit_symbols: &[String],
) -> Vec<String> {
    if !explicit_paths.is_empty() {
        return merged_unique(&[], explicit_paths);
    }

    let tokens = tokenize_task_text(task_text);
    let explicit_symbol_set = explicit_symbols
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut scored = rich_map
        .files_ranked
        .iter()
        .map(|file| {
            let lower_path = file.path.to_ascii_lowercase();
            let token_matches = tokens
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();
            let symbol_matches = rich_map
                .symbols_ranked
                .iter()
                .filter(|symbol| {
                    symbol.file == file.path
                        && explicit_symbol_set.contains(&symbol.name.to_ascii_lowercase())
                })
                .count();
            let score = token_matches + symbol_matches;
            (score, file.score, file.path.clone())
        })
        .filter(|(score, _, _)| *score > 0)
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .map(|(_, _, path)| path)
        .take(6)
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolResultProvenance {
    pub(crate) regions: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReducerSearchFile {
    pub(crate) path: String,
    pub(crate) match_count: usize,
    pub(crate) matched_terms: BTreeSet<String>,
    pub(crate) matched_phase_indexes: BTreeSet<usize>,
    pub(crate) symbols: BTreeSet<String>,
    pub(crate) regions: BTreeSet<String>,
    pub(crate) exact_symbol_regions: BTreeSet<String>,
    pub(crate) definition_regions: BTreeSet<String>,
    pub(crate) exact_full_symbol_definition_regions: BTreeSet<String>,
    pub(crate) exact_full_symbol_definition_hits: usize,
    pub(crate) exact_full_symbol_hits: usize,
    pub(crate) exact_symbol_hits: usize,
    pub(crate) best_exact_full_symbol_definition_len: usize,
    pub(crate) best_exact_full_symbol_len: usize,
    pub(crate) definition_hits: usize,
    pub(crate) call_hits: usize,
    pub(crate) reference_hits: usize,
    pub(crate) text_token_only_hits: usize,
    pub(crate) preview_matches: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchTermKind {
    FullSymbol,
    Identifier,
    ExpandedSymbol,
    TextToken,
}

#[derive(Debug, Clone)]
struct SearchCandidate {
    term: String,
    phase_index: usize,
    kind: SearchTermKind,
    whole_word: bool,
    case_sensitive: bool,
}

#[derive(Debug, Clone, Default)]
struct SearchPhase {
    candidates: Vec<SearchCandidate>,
}

#[derive(Debug, Clone, Default)]
struct SearchPlan {
    phases: Vec<SearchPhase>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SearchExecution {
    pub(crate) files: Vec<ReducerSearchFile>,
    pub(crate) evidence_by_file: BTreeMap<String, CodeEvidenceSummary>,
    #[cfg(test)]
    pub(crate) used_fallback: bool,
}

fn collect_tool_result_provenance(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    path: &str,
) -> Vec<ToolResultProvenance> {
    snapshot
        .recent_tool_invocations
        .iter()
        .rev()
        .filter(|invocation| invocation.paths.iter().any(|candidate| candidate == path))
        .map(|invocation| ToolResultProvenance {
            regions: invocation.regions.clone(),
        })
        .collect()
}

fn is_identifier_like_query(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':'))
}

fn split_camel_case_term(value: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let chars = value.chars().collect::<Vec<_>>();
    for (idx, ch) in chars.iter().enumerate() {
        let prev = idx.checked_sub(1).and_then(|pos| chars.get(pos)).copied();
        let next = chars.get(idx + 1).copied();
        let boundary = !current.is_empty()
            && ch.is_ascii_uppercase()
            && prev.is_some_and(|candidate| {
                candidate.is_ascii_lowercase()
                    || (candidate.is_ascii_uppercase()
                        && next.is_some_and(|upcoming| upcoming.is_ascii_lowercase()))
            });
        if boundary {
            pieces.push(current);
            current = String::new();
        }
        current.push(*ch);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

fn expanded_symbol_terms(value: &str) -> Vec<String> {
    let mut expanded = Vec::new();
    let mut seen = HashSet::new();
    for chunk in trim_query_fragment(value)
        .replace("::", ".")
        .split(['.', '/', '\\', '_', '-'])
    {
        for piece in split_camel_case_term(chunk) {
            let trimmed = piece.trim();
            if trimmed.len() < 3 {
                continue;
            }
            let lowered = trimmed.to_ascii_lowercase();
            if is_low_signal_query_token(&lowered) || !seen.insert(lowered) {
                continue;
            }
            expanded.push(trimmed.to_string());
        }
    }
    expanded
}

fn push_search_candidate(
    candidates: &mut Vec<SearchCandidate>,
    seen: &mut HashSet<String>,
    term: &str,
    phase_index: usize,
    kind: SearchTermKind,
    case_sensitive: bool,
    max_candidates: usize,
) {
    let trimmed = term.trim();
    if trimmed.is_empty() || candidates.len() >= max_candidates {
        return;
    }
    let key = trimmed.to_ascii_lowercase();
    if !seen.insert(key) {
        return;
    }
    candidates.push(SearchCandidate {
        term: trimmed.to_string(),
        phase_index,
        kind,
        whole_word: matches!(
            kind,
            SearchTermKind::FullSymbol | SearchTermKind::Identifier
        ) && is_identifier_like_query(trimmed),
        case_sensitive,
    });
}

fn build_search_plan(query_focus: &QueryFocus) -> SearchPlan {
    let mut seen = HashSet::new();
    let mut precision = Vec::new();
    for term in &query_focus.full_symbol_terms {
        push_search_candidate(
            &mut precision,
            &mut seen,
            term,
            0,
            SearchTermKind::FullSymbol,
            true,
            SEARCH_PLAN_PHASE1_MAX_CANDIDATES,
        );
    }
    for term in &query_focus.symbol_terms {
        push_search_candidate(
            &mut precision,
            &mut seen,
            term,
            0,
            SearchTermKind::Identifier,
            true,
            SEARCH_PLAN_PHASE1_MAX_CANDIDATES,
        );
    }

    let mut fallback = Vec::new();
    let fallback_capacity = SEARCH_PLAN_MAX_CANDIDATES.saturating_sub(precision.len());
    if fallback_capacity > 0 {
        for term in query_focus
            .full_symbol_terms
            .iter()
            .chain(query_focus.symbol_terms.iter())
        {
            for expanded in expanded_symbol_terms(term) {
                push_search_candidate(
                    &mut fallback,
                    &mut seen,
                    &expanded,
                    1,
                    SearchTermKind::ExpandedSymbol,
                    false,
                    fallback_capacity,
                );
            }
        }
    }
    if fallback.len() < fallback_capacity {
        for token in query_focus
            .text_tokens
            .iter()
            .take(SEARCH_PLAN_TEXT_FALLBACK_LIMIT)
        {
            push_search_candidate(
                &mut fallback,
                &mut seen,
                token,
                1,
                SearchTermKind::TextToken,
                false,
                fallback_capacity,
            );
        }
    }

    let mut phases = Vec::new();
    if !precision.is_empty() {
        phases.push(SearchPhase {
            candidates: precision,
        });
    }
    if !fallback.is_empty() {
        phases.push(SearchPhase {
            candidates: fallback,
        });
    }
    SearchPlan { phases }
}

fn uses_staged_search_planner(action: BrokerAction) -> bool {
    matches!(action, BrokerAction::Inspect | BrokerAction::ChooseTool)
}

fn classify_search_candidate_match(
    candidate: &SearchCandidate,
    line: &str,
) -> Option<EvidenceMatchKind> {
    if matches!(
        candidate.kind,
        SearchTermKind::FullSymbol | SearchTermKind::Identifier
    ) && contains_identifier_term(line, &candidate.term)
    {
        Some(classify_symbol_match(line, &candidate.term))
    } else if matches!(
        candidate.kind,
        SearchTermKind::ExpandedSymbol | SearchTermKind::TextToken
    ) && line
        .to_ascii_lowercase()
        .contains(&candidate.term.to_ascii_lowercase())
    {
        Some(if looks_like_signature(line) {
            EvidenceMatchKind::DefinesSymbol
        } else {
            EvidenceMatchKind::ReferencesSymbol
        })
    } else {
        None
    }
}

fn requested_search_paths(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
    query_focus: &QueryFocus,
) -> Vec<String> {
    merged_unique(
        &merged_unique(
            &merged_unique(&snapshot.focus_paths, &snapshot.checkpoint_focus_paths),
            &request.focus_paths,
        ),
        &query_focus.path_terms,
    )
}

fn apply_search_candidate_results(
    root: &Path,
    requested_paths: &[String],
    candidate: &SearchCandidate,
    files: &mut BTreeMap<String, ReducerSearchFile>,
) {
    let search = packet28_reducer_core::search(
        root,
        &packet28_reducer_core::SearchRequest {
            query: candidate.term.clone(),
            requested_paths: requested_paths.to_vec(),
            fixed_string: true,
            case_sensitive: Some(candidate.case_sensitive),
            whole_word: candidate.whole_word,
            context_lines: None,
            max_matches_per_file: Some(SEARCH_BROKER_MAX_MATCHES_PER_FILE),
            max_total_matches: Some(SEARCH_BROKER_MAX_TOTAL_MATCHES),
        },
    );
    let Ok(search) = search else {
        return;
    };
    for group in search.groups {
        let entry = files
            .entry(group.path.clone())
            .or_insert_with(|| ReducerSearchFile {
                path: group.path.clone(),
                ..ReducerSearchFile::default()
            });
        entry.match_count = entry.match_count.saturating_add(group.match_count);
        entry.matched_terms.insert(candidate.term.clone());
        entry.matched_phase_indexes.insert(candidate.phase_index);
        for symbol in &search.symbols {
            entry.symbols.insert(symbol.clone());
        }
        if matches!(
            candidate.kind,
            SearchTermKind::FullSymbol | SearchTermKind::Identifier
        ) {
            entry.symbols.insert(candidate.term.clone());
        }
        for item in group.matches {
            let region = packet28_reducer_core::format_region(&item.path, item.line, item.line);
            entry.regions.insert(region.clone());
            if let Some(match_kind) = classify_search_candidate_match(candidate, &item.text) {
                match candidate.kind {
                    SearchTermKind::FullSymbol => {
                        entry.exact_full_symbol_hits =
                            entry.exact_full_symbol_hits.saturating_add(1);
                        entry.exact_symbol_hits = entry.exact_symbol_hits.saturating_add(1);
                        entry.best_exact_full_symbol_len =
                            entry.best_exact_full_symbol_len.max(candidate.term.len());
                        entry.exact_symbol_regions.insert(region.clone());
                        if matches!(match_kind, EvidenceMatchKind::DefinesSymbol) {
                            entry.exact_full_symbol_definition_hits =
                                entry.exact_full_symbol_definition_hits.saturating_add(1);
                            entry.best_exact_full_symbol_definition_len = entry
                                .best_exact_full_symbol_definition_len
                                .max(candidate.term.len());
                            entry
                                .exact_full_symbol_definition_regions
                                .insert(region.clone());
                        }
                    }
                    SearchTermKind::Identifier => {
                        entry.exact_symbol_hits = entry.exact_symbol_hits.saturating_add(1);
                        entry.exact_symbol_regions.insert(region.clone());
                    }
                    SearchTermKind::ExpandedSymbol | SearchTermKind::TextToken => {
                        entry.text_token_only_hits = entry.text_token_only_hits.saturating_add(1);
                    }
                }
                match match_kind {
                    EvidenceMatchKind::DefinesSymbol => {
                        entry.definition_hits = entry.definition_hits.saturating_add(1);
                        entry.definition_regions.insert(region.clone());
                    }
                    EvidenceMatchKind::CallsSymbol => {
                        entry.call_hits = entry.call_hits.saturating_add(1);
                    }
                    EvidenceMatchKind::ReferencesSymbol => {
                        entry.reference_hits = entry.reference_hits.saturating_add(1);
                    }
                    EvidenceMatchKind::Signature | EvidenceMatchKind::Fallback => {}
                }
            }
            if entry
                .preview_matches
                .iter()
                .all(|(line, _)| *line != item.line)
                && entry.preview_matches.len() < 6
            {
                entry.preview_matches.push((item.line, item.text));
            }
        }
    }
}

fn rank_reducer_search_files(
    mut files: Vec<ReducerSearchFile>,
    max_files: usize,
) -> Vec<ReducerSearchFile> {
    files.sort_by(|a, b| {
        b.best_exact_full_symbol_definition_len
            .cmp(&a.best_exact_full_symbol_definition_len)
            .then_with(|| {
                b.exact_full_symbol_definition_hits
                    .cmp(&a.exact_full_symbol_definition_hits)
            })
            .then_with(|| {
                b.best_exact_full_symbol_len
                    .cmp(&a.best_exact_full_symbol_len)
            })
            .then_with(|| b.exact_full_symbol_hits.cmp(&a.exact_full_symbol_hits))
            .then_with(|| {
                b.matched_phase_indexes
                    .len()
                    .cmp(&a.matched_phase_indexes.len())
            })
            .then_with(|| b.definition_hits.cmp(&a.definition_hits))
            .then_with(|| b.match_count.cmp(&a.match_count))
            .then_with(|| a.path.cmp(&b.path))
    });
    files.truncate(max_files.max(1));
    files
}

pub(crate) fn preferred_search_regions(file: &ReducerSearchFile) -> Vec<String> {
    if !file.exact_full_symbol_definition_regions.is_empty() {
        return file
            .exact_full_symbol_definition_regions
            .iter()
            .cloned()
            .collect();
    }
    if !file.exact_symbol_regions.is_empty() {
        return file.exact_symbol_regions.iter().cloned().collect();
    }
    if !file.definition_regions.is_empty() {
        return file.definition_regions.iter().cloned().collect();
    }
    file.regions.iter().cloned().collect()
}

pub(crate) fn build_reducer_search_execution(
    state: Option<&Arc<Mutex<DaemonState>>>,
    root: &Path,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
    query_focus: &QueryFocus,
    action: BrokerAction,
    max_files: usize,
    max_evidence_lines: usize,
) -> SearchExecution {
    let requested_paths = requested_search_paths(snapshot, request, query_focus);
    let plan = build_search_plan(query_focus);
    let mut files_by_path = BTreeMap::<String, ReducerSearchFile>::new();
    let mut used_fallback = false;
    if let Some(phase) = plan.phases.first() {
        for candidate in &phase.candidates {
            apply_search_candidate_results(root, &requested_paths, candidate, &mut files_by_path);
        }
    }
    let mut reducer_files =
        rank_reducer_search_files(files_by_path.into_values().collect(), max_files);
    let mut evidence_by_file = build_reducer_search_evidence(
        state,
        root,
        snapshot,
        request,
        query_focus,
        &reducer_files,
        max_evidence_lines,
    );
    if uses_staged_search_planner(action)
        && plan.phases.len() > 1
        && phase_results_are_weak(&reducer_files, &evidence_by_file)
    {
        let mut files_by_path = reducer_files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect::<BTreeMap<_, _>>();
        for candidate in &plan.phases[1].candidates {
            apply_search_candidate_results(root, &requested_paths, candidate, &mut files_by_path);
        }
        reducer_files = rank_reducer_search_files(files_by_path.into_values().collect(), max_files);
        evidence_by_file = build_reducer_search_evidence(
            state,
            root,
            snapshot,
            request,
            query_focus,
            &reducer_files,
            max_evidence_lines,
        );
        used_fallback = true;
    }
    #[cfg(not(test))]
    let _ = used_fallback;
    SearchExecution {
        files: reducer_files,
        evidence_by_file,
        #[cfg(test)]
        used_fallback,
    }
}

pub(crate) fn phase_results_are_weak(
    reducer_files: &[ReducerSearchFile],
    evidence_by_file: &BTreeMap<String, CodeEvidenceSummary>,
) -> bool {
    if reducer_files
        .iter()
        .any(|file| file.exact_full_symbol_definition_hits > 0)
    {
        return false;
    }
    let no_definition_biased_hits = reducer_files.iter().all(|file| file.definition_hits == 0);
    let exact_hit_files = reducer_files
        .iter()
        .filter(|file| file.exact_symbol_hits > 0)
        .count();
    let has_quality_evidence = reducer_files.iter().any(|file| {
        evidence_by_file
            .get(&file.path)
            .is_some_and(CodeEvidenceSummary::has_quality_match)
    });
    no_definition_biased_hits || exact_hit_files < 2 || !has_quality_evidence
}

pub(crate) fn collect_tool_result_provenance_for_path(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    path: &str,
) -> Vec<ToolResultProvenance> {
    collect_tool_result_provenance(snapshot, path)
}
