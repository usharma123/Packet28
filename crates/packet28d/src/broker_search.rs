use super::*;
use std::io::Read;

const SEARCH_PLAN_MAX_CANDIDATES: usize = 8;
const SEARCH_PLAN_PHASE1_MAX_CANDIDATES: usize = 5;
const SEARCH_PLAN_TEXT_FALLBACK_LIMIT: usize = 3;
const SEARCH_BROKER_MAX_MATCHES_PER_FILE: usize = 8;
const SEARCH_BROKER_MAX_TOTAL_MATCHES: usize = 32;

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
    // Keep symbol classification narrow so plain lowercase prose stays in text search.
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
                * if primary_scopes.contains(&scope) {
                    25
                } else {
                    10
                };

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
pub(crate) struct ToolResultProvenance {
    pub(crate) regions: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReducerSearchFile {
    pub(crate) path: String,
    pub(crate) match_count: usize,
    pub(crate) matched_terms: BTreeSet<String>,
    matched_phase_indexes: BTreeSet<usize>,
    symbols: BTreeSet<String>,
    regions: BTreeSet<String>,
    exact_symbol_regions: BTreeSet<String>,
    definition_regions: BTreeSet<String>,
    exact_full_symbol_definition_regions: BTreeSet<String>,
    exact_full_symbol_definition_hits: usize,
    exact_full_symbol_hits: usize,
    pub(crate) exact_symbol_hits: usize,
    best_exact_full_symbol_definition_len: usize,
    best_exact_full_symbol_len: usize,
    pub(crate) definition_hits: usize,
    call_hits: usize,
    reference_hits: usize,
    text_token_only_hits: usize,
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
    fn has_quality_match(&self) -> bool {
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

fn looks_like_signature(line: &str) -> bool {
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

fn contains_identifier_term(line: &str, term: &str) -> bool {
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

fn classify_symbol_match(line: &str, symbol: &str) -> EvidenceMatchKind {
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

fn preferred_search_regions(file: &ReducerSearchFile) -> Vec<String> {
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

fn build_reducer_search_evidence(
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
            let mut provenance = collect_tool_result_provenance(snapshot, path);
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

fn phase_results_are_weak(
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
