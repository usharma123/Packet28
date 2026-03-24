mod weights;

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use ignore::WalkBuilder;
use memmap2::Mmap;
use packet28_reducer_core::{
    infer_symbols_from_pattern, SearchEngineStats, SearchGroup, SearchMatch, SearchRequest,
    SearchResult,
};
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::literal::{ExtractKind, Extractor, Seq};
use regex_syntax::hir::{Hir, HirKind};
use serde::{Deserialize, Serialize};

use crate::weights::{pair_weight, WEIGHT_TABLE_VERSION};

const REGEX_INDEX_SCHEMA_VERSION: u32 = 2;
const REGEX_DIR_NAME: &str = "regex-v1";
const MANIFEST_FILE_NAME: &str = "manifest.json";
const BASE_LOOKUP_FILE_NAME: &str = "base.lookup.dat";
const BASE_POSTINGS_FILE_NAME: &str = "base.postings.dat";
const BASE_DOCS_FILE_NAME: &str = "docs.dat";
const OVERLAY_LOOKUP_FILE_NAME: &str = "overlay.lookup.dat";
const OVERLAY_POSTINGS_FILE_NAME: &str = "overlay.postings.dat";
const OVERLAY_DOCS_FILE_NAME: &str = "overlay.docs.dat";
const OVERLAY_STATE_FILE_NAME: &str = "overlay.state.json";
const LOOKUP_ROW_BYTES: usize = 24;
const SHORT_GRAM_BYTES: usize = 2;
const MIN_GRAM_BYTES: usize = 3;
const MAX_GRAM_BYTES: usize = 24;
const MAX_LITERAL_COVER: usize = 3;
const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;
const SEGMENT_DOC_BATCH_SIZE: usize = 256;
const SEGMENT_RECORD_BYTES: usize = 13;
const MAX_INDEX_VERIFY_CANDIDATES: usize = 96;
const MAX_INDEX_VERIFY_NUMERATOR: usize = 1;
const MAX_INDEX_VERIFY_DENOMINATOR: usize = 3;
const POSITION_BUCKET_COUNT: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RegexIndexManifest {
    pub schema_version: u32,
    pub weight_table_version: u32,
    pub generation: u64,
    pub include_tests: bool,
    pub status: String,
    pub total_files: usize,
    pub indexed_files: usize,
    pub overlay_files: usize,
    pub base_commit: Option<String>,
    pub stale_reason: Option<String>,
    pub last_build_started_at_unix: Option<u64>,
    pub last_build_completed_at_unix: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct OverlayState {
    shadowed_paths: BTreeSet<String>,
    deleted_paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct DocRecord {
    doc_id: u32,
    path: String,
    size: u64,
    mtime_secs: u64,
    fingerprint: String,
}

#[derive(Debug, Clone, Default)]
pub struct RegexIndexRuntime {
    pub manifest: RegexIndexManifest,
    loaded: Option<Arc<LoadedIndex>>,
}

impl RegexIndexRuntime {
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }
}

#[derive(Debug)]
struct LoadedIndex {
    base: LoadedLayer,
    overlay: LoadedLayer,
    overlay_state: OverlayState,
}

#[derive(Debug)]
struct LoadedLayer {
    docs: Vec<DocRecord>,
    doc_ids_by_path: HashMap<String, u32>,
    lookup: Option<Mmap>,
    postings: Option<Mmap>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SparseCandidate {
    hash: u64,
    score: u32,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SearchPlan {
    All,
    Literal(Vec<u8>),
    And(Vec<SearchPlan>),
    Or(Vec<SearchPlan>),
}

impl SearchPlan {
    fn kind_str(&self) -> &'static str {
        match self {
            Self::All => "prefiltered_all",
            Self::Literal(_) => "literal",
            Self::And(_) => "and",
            Self::Or(_) => "or",
        }
    }
}

#[derive(Clone)]
struct CompiledSearch {
    verifier: Verifier,
    plan: SearchPlan,
    plan_kind: String,
    planner_fallback: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HeapItem {
    hash: u64,
    doc_id: u32,
    summary: u8,
    segment_idx: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PositionSummary(u8);

impl PositionSummary {
    fn new(bucket: u8) -> Self {
        Self(((bucket & 0x0f) << 4) | (bucket & 0x0f))
    }

    fn first_bucket(self) -> u8 {
        self.0 >> 4
    }

    fn last_bucket(self) -> u8 {
        self.0 & 0x0f
    }

    fn update(&mut self, bucket: u8) {
        let bucket = bucket & 0x0f;
        let first = self.first_bucket().min(bucket);
        let last = self.last_bucket().max(bucket);
        self.0 = (first << 4) | last;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PostingEntry {
    doc_id: u32,
    summary: PositionSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LookupPostingMeta {
    offset: u64,
    len: u32,
    doc_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiteralWindow {
    earliest_bucket: u8,
    latest_bucket: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexedGram {
    hash: u64,
    summary: PositionSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LayerKind {
    Base,
    Overlay,
}

#[derive(Default)]
struct QueryCache {
    postings: HashMap<(LayerKind, u64), Option<Vec<PostingEntry>>>,
    literal_candidates: HashMap<Vec<u8>, BTreeSet<String>>,
    literal_windows: HashMap<(String, Vec<u8>), Option<LiteralWindow>>,
}

#[derive(Clone)]
enum Verifier {
    Regex(Regex),
    FixedBytes {
        needle: Vec<u8>,
        case_insensitive: bool,
    },
}

pub fn load_runtime(root: &Path) -> Result<RegexIndexRuntime> {
    let mut manifest = load_manifest(root);
    if manifest.schema_version == 0 {
        return Ok(RegexIndexRuntime {
            manifest,
            loaded: None,
        });
    }
    if manifest.schema_version != REGEX_INDEX_SCHEMA_VERSION
        || manifest.weight_table_version != WEIGHT_TABLE_VERSION
    {
        let found_schema = manifest.schema_version;
        let found_weight = manifest.weight_table_version;
        mark_manifest_unloaded(
            &mut manifest,
            "stale",
            format!(
                "regex index weight/schema mismatch (found schema={}, weight={}, expected schema={}, weight={})",
                found_schema,
                found_weight,
                REGEX_INDEX_SCHEMA_VERSION,
                WEIGHT_TABLE_VERSION
            ),
        );
        return Ok(RegexIndexRuntime {
            manifest,
            loaded: None,
        });
    }
    if let Some(expected) = manifest.base_commit.as_deref() {
        if current_git_commit(root).as_deref() != Some(expected) {
            let expected_commit = expected.to_string();
            let current_commit =
                current_git_commit(root).unwrap_or_else(|| "<unknown>".to_string());
            mark_manifest_unloaded(
                &mut manifest,
                "stale",
                format!(
                    "regex index base commit changed (indexed={}, current={})",
                    expected_commit, current_commit
                ),
            );
            return Ok(RegexIndexRuntime {
                manifest,
                loaded: None,
            });
        }
    }
    let base = match load_layer(
        root,
        BASE_LOOKUP_FILE_NAME,
        BASE_POSTINGS_FILE_NAME,
        BASE_DOCS_FILE_NAME,
    )
    .context("failed to load base regex index layer")
    {
        Ok(base) => base,
        Err(err) => {
            mark_manifest_unloaded(&mut manifest, "corrupt", err.to_string());
            return Ok(RegexIndexRuntime {
                manifest,
                loaded: None,
            });
        }
    };
    let overlay = match load_layer(
        root,
        OVERLAY_LOOKUP_FILE_NAME,
        OVERLAY_POSTINGS_FILE_NAME,
        OVERLAY_DOCS_FILE_NAME,
    )
    .context("failed to load overlay regex index layer")
    {
        Ok(overlay) => overlay,
        Err(err) => {
            mark_manifest_unloaded(&mut manifest, "corrupt", err.to_string());
            return Ok(RegexIndexRuntime {
                manifest,
                loaded: None,
            });
        }
    };
    let overlay_state = load_overlay_state(root);
    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(LoadedIndex {
            base,
            overlay,
            overlay_state,
        })),
    })
}

pub fn rebuild_full_index(root: &Path, include_tests: bool) -> Result<RegexIndexRuntime> {
    let started = now_unix();
    let mut manifest = load_manifest(root);
    manifest.schema_version = REGEX_INDEX_SCHEMA_VERSION;
    manifest.weight_table_version = WEIGHT_TABLE_VERSION;
    manifest.include_tests = include_tests;
    manifest.status = "building".to_string();
    manifest.last_build_started_at_unix = Some(started);
    manifest.stale_reason = None;
    manifest.last_error = None;
    save_manifest(root, &manifest)?;

    let docs = scan_documents(root)?;
    let base_layer = build_layer(
        root,
        &docs,
        BASE_LOOKUP_FILE_NAME,
        BASE_POSTINGS_FILE_NAME,
        BASE_DOCS_FILE_NAME,
    )?;
    let overlay_docs = Vec::<IndexedDocument>::new();
    let overlay_layer = build_layer(
        root,
        &overlay_docs,
        OVERLAY_LOOKUP_FILE_NAME,
        OVERLAY_POSTINGS_FILE_NAME,
        OVERLAY_DOCS_FILE_NAME,
    )?;
    let overlay_state = OverlayState::default();
    save_overlay_state(root, &overlay_state)?;

    manifest.generation = manifest.generation.saturating_add(1);
    manifest.status = "ready".to_string();
    manifest.total_files = docs.len();
    manifest.indexed_files = docs.len();
    manifest.overlay_files = 0;
    manifest.base_commit = current_git_commit(root);
    manifest.stale_reason = None;
    manifest.last_build_completed_at_unix = Some(now_unix());
    manifest.last_error = None;
    save_manifest(root, &manifest)?;

    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(LoadedIndex {
            base: base_layer,
            overlay: overlay_layer,
            overlay_state,
        })),
    })
}

pub fn update_overlay_index(
    root: &Path,
    current: Option<&RegexIndexRuntime>,
    changed_paths: &[String],
) -> Result<RegexIndexRuntime> {
    if current.is_none() || changed_paths.is_empty() {
        return rebuild_full_index(root, true);
    }
    let current = current.expect("checked above");
    let loaded = current
        .loaded
        .as_ref()
        .ok_or_else(|| anyhow!("regex index not loaded"))?;
    let mut overlay_state = loaded.overlay_state.clone();
    let normalized = normalize_paths(root, changed_paths);
    let mut overlay_by_path = HashMap::<String, IndexedDocument>::new();
    for doc in &loaded.overlay.docs {
        if overlay_state.deleted_paths.contains(&doc.path) {
            continue;
        }
        let full_path = root.join(&doc.path);
        if let Some(indexed) = index_document(root, &full_path)? {
            overlay_by_path.insert(doc.path.clone(), indexed);
        }
    }
    for path in normalized {
        overlay_state.shadowed_paths.insert(path.clone());
        let full_path = root.join(&path);
        if !full_path.exists() {
            overlay_state.deleted_paths.insert(path.clone());
            overlay_by_path.remove(&path);
            continue;
        }
        overlay_state.deleted_paths.remove(&path);
        if let Some(indexed) = index_document(root, &full_path)? {
            overlay_by_path.insert(path, indexed);
        }
    }
    let mut overlay_docs = overlay_by_path.into_values().collect::<Vec<_>>();
    overlay_docs.sort_by(|left, right| left.path.cmp(&right.path));
    for (idx, doc) in overlay_docs.iter_mut().enumerate() {
        doc.doc_id = idx as u32;
    }
    let overlay_layer = build_layer(
        root,
        &overlay_docs,
        OVERLAY_LOOKUP_FILE_NAME,
        OVERLAY_POSTINGS_FILE_NAME,
        OVERLAY_DOCS_FILE_NAME,
    )?;
    save_overlay_state(root, &overlay_state)?;

    let mut manifest = load_manifest(root);
    manifest.status = "ready".to_string();
    manifest.overlay_files = overlay_docs.len();
    manifest.stale_reason = None;
    manifest.last_build_completed_at_unix = Some(now_unix());
    save_manifest(root, &manifest)?;

    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(LoadedIndex {
            base: load_layer(
                root,
                BASE_LOOKUP_FILE_NAME,
                BASE_POSTINGS_FILE_NAME,
                BASE_DOCS_FILE_NAME,
            )?,
            overlay: overlay_layer,
            overlay_state,
        })),
    })
}

pub fn clear_index(root: &Path) -> Result<()> {
    let path = regex_index_dir(root);
    if path.exists() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove regex index dir '{}'", path.display()))?;
    }
    Ok(())
}

pub fn guarded_fallback_reason(
    root: &Path,
    runtime: &RegexIndexRuntime,
    request: &SearchRequest,
) -> Result<Option<String>> {
    if !runtime.is_loaded() || runtime.manifest.status != "ready" {
        let reason = runtime
            .manifest
            .stale_reason
            .clone()
            .or_else(|| runtime.manifest.last_error.clone())
            .unwrap_or_else(|| "regex search index is not ready".to_string());
        return Ok(Some(reason));
    }
    let compiled = compile_request(request)?;
    if matches!(compiled.plan, SearchPlan::All) {
        return Ok(Some(compiled.planner_fallback.unwrap_or_else(|| {
            "planner could not derive a selective index plan".to_string()
        })));
    }
    let loaded = runtime
        .loaded
        .as_ref()
        .ok_or_else(|| anyhow!("regex index not loaded"))?;
    let (resolved_paths, _) = resolve_requested_paths(root, &request.requested_paths);
    let requested_filter = requested_filter_set(&resolved_paths);
    let all_paths = all_indexed_paths(loaded.as_ref(), requested_filter.as_ref());
    let mut engine = SearchEngineStats {
        engine: "sparse_regex_index".to_string(),
        index_generation: Some(runtime.manifest.generation),
        base_commit: runtime.manifest.base_commit.clone(),
        plan_kind: Some(compiled.plan_kind.clone()),
        planner_fallback: compiled.planner_fallback.clone(),
        stale_reason: runtime.manifest.stale_reason.clone(),
        candidates_examined: 0,
        candidate_files: 0,
        verified_files: 0,
        index_lookups: 0,
        postings_bytes_read: 0,
        fallback_reason: None,
    };
    let mut cache = QueryCache::default();
    let candidates = candidate_paths_for_plan(
        loaded.as_ref(),
        &compiled.plan,
        requested_filter.as_ref(),
        &all_paths,
        &mut cache,
        &mut engine,
    )?;
    let pruned_candidates =
        prune_candidates_with_positions(loaded.as_ref(), &compiled.plan, &candidates, &mut cache);
    if should_fallback_to_rg(pruned_candidates.len(), all_paths.len()) {
        return Ok(Some(format!(
            "candidate set remained too broad for indexed verification ({}/{} files)",
            pruned_candidates.len(),
            all_paths.len()
        )));
    }
    Ok(None)
}

pub fn indexed_search(
    root: &Path,
    runtime: &RegexIndexRuntime,
    request: &SearchRequest,
) -> Result<SearchResult> {
    let loaded = runtime
        .loaded
        .as_ref()
        .ok_or_else(|| anyhow!("regex index not loaded"))?;
    let query = request.query.trim();
    anyhow::ensure!(!query.is_empty(), "search query cannot be empty");

    let (resolved_paths, mut diagnostics) = resolve_requested_paths(root, &request.requested_paths);
    let requested_filter = requested_filter_set(&resolved_paths);
    let compiled = compile_request(request)?;
    let mut engine = SearchEngineStats {
        engine: "sparse_regex_index".to_string(),
        index_generation: Some(runtime.manifest.generation),
        base_commit: runtime.manifest.base_commit.clone(),
        plan_kind: Some(compiled.plan_kind.clone()),
        planner_fallback: compiled.planner_fallback.clone(),
        stale_reason: runtime.manifest.stale_reason.clone(),
        candidates_examined: 0,
        candidate_files: 0,
        verified_files: 0,
        index_lookups: 0,
        postings_bytes_read: 0,
        fallback_reason: None,
    };
    let mut cache = QueryCache::default();

    let all_paths = all_indexed_paths(loaded.as_ref(), requested_filter.as_ref());
    if let Some(reason) = compiled.planner_fallback.clone() {
        diagnostics.push(reason);
    }
    let candidate_paths = candidate_paths_for_plan(
        loaded.as_ref(),
        &compiled.plan,
        requested_filter.as_ref(),
        &all_paths,
        &mut cache,
        &mut engine,
    )?;
    let pruned_candidate_paths = prune_candidates_with_positions(
        loaded.as_ref(),
        &compiled.plan,
        &candidate_paths,
        &mut cache,
    );

    engine.candidates_examined = candidate_paths.len();
    engine.candidate_files = candidate_paths.len();
    engine.verified_files = pruned_candidate_paths.len();

    let mut groups = Vec::new();
    let mut total_match_count = 0usize;
    for path in &pruned_candidate_paths {
        let file_groups =
            verify_path(root, path, &compiled.verifier, request.max_matches_per_file)?;
        if file_groups.is_empty() {
            continue;
        }
        total_match_count = total_match_count.saturating_add(file_groups.len());
        let displayed = file_groups.iter().take(12).cloned().collect::<Vec<_>>();
        groups.push(SearchGroup {
            path: path.clone(),
            match_count: file_groups.len(),
            displayed_match_count: displayed.len(),
            truncated: file_groups.len() > displayed.len(),
            matches: displayed,
        });
    }

    let paths = groups
        .iter()
        .map(|group| group.path.clone())
        .collect::<Vec<_>>();
    let regions = groups
        .iter()
        .flat_map(|group| {
            group
                .matches
                .iter()
                .map(|item| format!("{}:{}-{}", item.path, item.line, item.line))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let max_total_matches = request.max_total_matches.unwrap_or(50).clamp(1, 200);
    let mut returned_matches = Vec::new();
    for group in &groups {
        for item in &group.matches {
            if returned_matches.len() >= max_total_matches {
                break;
            }
            returned_matches.push(item.clone());
        }
        if returned_matches.len() >= max_total_matches {
            break;
        }
    }
    let returned_match_count = returned_matches.len();
    let compact_preview = render_compact_preview(total_match_count, &groups);

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
        groups,
        compact_preview,
        diagnostics,
        engine: Some(engine),
    })
}

fn build_verifier(request: &SearchRequest, query: &str) -> Result<Verifier> {
    if request.fixed_string && !request.whole_word && !matches!(request.case_sensitive, Some(false))
    {
        return Ok(Verifier::FixedBytes {
            needle: query.as_bytes().to_vec(),
            case_insensitive: matches!(request.case_sensitive, Some(false)),
        });
    }
    let pattern = if request.fixed_string {
        regex::escape(query)
    } else {
        query.to_string()
    };
    let pattern = if request.whole_word {
        format!(r"\b(?:{})\b", pattern)
    } else {
        pattern
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(matches!(request.case_sensitive, Some(false)))
        .build()
        .with_context(|| format!("unsupported regex syntax for packet28.search: {query}"))?;
    Ok(Verifier::Regex(regex))
}

fn compile_request(request: &SearchRequest) -> Result<CompiledSearch> {
    let query = request.query.trim();
    let verifier = build_verifier(request, query)?;
    let (plan, planner_fallback) = build_search_plan(request, query)?;
    Ok(CompiledSearch {
        verifier,
        plan_kind: plan.kind_str().to_string(),
        plan,
        planner_fallback,
    })
}

fn build_search_plan(request: &SearchRequest, query: &str) -> Result<(SearchPlan, Option<String>)> {
    if request.fixed_string {
        if matches!(request.case_sensitive, Some(false)) && !query.is_ascii() {
            return Ok((
                SearchPlan::All,
                Some(
                    "unicode ignore-case fixed-string queries use regex fallback instead of ASCII-only index normalization"
                        .to_string(),
                ),
            ));
        }
        let literal = normalize_for_index(query.as_bytes());
        if build_covering_hashes(&literal).is_empty() {
            return Ok((
                SearchPlan::All,
                Some(
                    "fixed string query is too short to derive a selective index plan".to_string(),
                ),
            ));
        }
        return Ok((SearchPlan::Literal(literal), None));
    }

    let hir = regex_syntax::parse(query)
        .with_context(|| format!("unsupported regex syntax for packet28.search: {query}"))?;
    let plan = normalize_plan(plan_from_hir(&hir));
    let planner_fallback = matches!(plan, SearchPlan::All).then(|| {
        "planner could not derive required literals; verifying all indexed candidates".to_string()
    });
    Ok((plan, planner_fallback))
}

fn plan_from_hir(hir: &Hir) -> SearchPlan {
    match hir.kind() {
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => SearchPlan::All,
        HirKind::Literal(literal) => literal_plan_from_bytes(&literal.0),
        HirKind::Capture(capture) => combine_required_plan(plan_from_hir(&capture.sub), hir),
        HirKind::Concat(subs) => {
            combine_required_plan(normalize_and(subs.iter().map(plan_from_hir).collect()), hir)
        }
        HirKind::Alternation(subs) => {
            combine_required_plan(normalize_or(subs.iter().map(plan_from_hir).collect()), hir)
        }
        HirKind::Repetition(repetition) => plan_from_repetition(repetition, hir),
    }
}

fn plan_from_repetition(repetition: &regex_syntax::hir::Repetition, hir: &Hir) -> SearchPlan {
    if repetition.min == 0 {
        return SearchPlan::All;
    }
    let child_plan = plan_from_hir(&repetition.sub);
    let repeated_plan = if repetition.min > 1 {
        match &child_plan {
            SearchPlan::Literal(literal) if !literal.is_empty() => {
                let repeats_to_materialize = (repetition.min as usize)
                    .min((MAX_GRAM_BYTES / literal.len()).saturating_add(1));
                literal_plan_from_bytes(&literal.repeat(repeats_to_materialize))
            }
            _ => child_plan.clone(),
        }
    } else {
        child_plan
    };
    combine_required_plan(repeated_plan, hir)
}

fn combine_required_plan(structural: SearchPlan, hir: &Hir) -> SearchPlan {
    let extracted = plan_from_extractors(hir);
    match (normalize_plan(structural), normalize_plan(extracted)) {
        (SearchPlan::All, SearchPlan::All) => SearchPlan::All,
        (SearchPlan::All, other) | (other, SearchPlan::All) => other,
        (left, right) if left == right => left,
        (left, right) => normalize_and(vec![left, right]),
    }
}

fn plan_from_extractors(hir: &Hir) -> SearchPlan {
    let prefix = plan_from_extractor(hir, ExtractKind::Prefix);
    let suffix = plan_from_extractor(hir, ExtractKind::Suffix);
    combine_extractor_plans(prefix, suffix)
}

fn combine_extractor_plans(prefix: SearchPlan, suffix: SearchPlan) -> SearchPlan {
    match (prefix, suffix) {
        (SearchPlan::All, SearchPlan::All) => SearchPlan::All,
        (SearchPlan::All, other) | (other, SearchPlan::All) => other,
        (left, right) if left == right => left,
        (left, right) => normalize_and(vec![left, right]),
    }
}

fn plan_from_extractor(hir: &Hir, kind: ExtractKind) -> SearchPlan {
    let mut extractor = Extractor::new();
    extractor.limit_class(6).limit_repeat(8).limit_total(64);
    extractor.kind(kind.clone());
    let mut seq = extractor.extract(hir);
    if !seq.is_finite() || seq.is_empty() {
        return SearchPlan::All;
    }
    seq.minimize_by_preference();
    match kind {
        ExtractKind::Prefix => seq.keep_first_bytes(MAX_GRAM_BYTES),
        ExtractKind::Suffix => seq.keep_last_bytes(MAX_GRAM_BYTES),
        _ => return SearchPlan::All,
    }
    plan_from_literal_seq(&seq, kind)
}

fn plan_from_literal_seq(seq: &Seq, kind: ExtractKind) -> SearchPlan {
    let Some(literals) = seq.literals() else {
        return SearchPlan::All;
    };
    if let Some(common) = common_literal_from_seq(seq, kind) {
        return SearchPlan::Literal(common);
    }
    let mut normalized = Vec::<Vec<u8>>::new();
    for literal in literals {
        let bytes = normalize_for_index(literal.as_bytes());
        if is_poisonous_literal(&bytes) || build_covering_hashes(&bytes).is_empty() {
            continue;
        }
        if !normalized.iter().any(|existing| existing == &bytes) {
            normalized.push(bytes);
        }
    }
    match normalized.len() {
        0 => SearchPlan::All,
        1 => SearchPlan::Literal(normalized.into_iter().next().unwrap_or_default()),
        _ => SearchPlan::Or(normalized.into_iter().map(SearchPlan::Literal).collect()),
    }
}

fn common_literal_from_seq(seq: &Seq, kind: ExtractKind) -> Option<Vec<u8>> {
    let common = match kind {
        ExtractKind::Prefix => seq.longest_common_prefix(),
        ExtractKind::Suffix => seq.longest_common_suffix(),
        _ => None,
    }?;
    let bytes = normalize_for_index(common);
    if is_poisonous_literal(&bytes) || build_covering_hashes(&bytes).is_empty() {
        return None;
    }
    Some(bytes)
}

fn literal_plan_from_bytes(bytes: &[u8]) -> SearchPlan {
    let normalized = normalize_for_index(bytes);
    if is_poisonous_literal(&normalized) || build_covering_hashes(&normalized).is_empty() {
        SearchPlan::All
    } else {
        SearchPlan::Literal(normalized)
    }
}

fn is_poisonous_literal(bytes: &[u8]) -> bool {
    bytes.len() < SHORT_GRAM_BYTES
}

fn normalize_plan(plan: SearchPlan) -> SearchPlan {
    match plan {
        SearchPlan::And(children) => normalize_and(children),
        SearchPlan::Or(children) => normalize_or(children),
        other => other,
    }
}

fn normalize_and(children: Vec<SearchPlan>) -> SearchPlan {
    let mut normalized = Vec::new();
    for child in children {
        match normalize_plan(child) {
            SearchPlan::All => {}
            SearchPlan::And(nested) => normalized.extend(nested),
            other if !normalized.iter().any(|existing| existing == &other) => {
                normalized.push(other)
            }
            _ => {}
        }
    }
    if normalized.is_empty() {
        SearchPlan::All
    } else if normalized.len() == 1 {
        normalized.into_iter().next().unwrap_or(SearchPlan::All)
    } else {
        SearchPlan::And(normalized)
    }
}

fn normalize_or(children: Vec<SearchPlan>) -> SearchPlan {
    let mut normalized = Vec::new();
    for child in children {
        match normalize_plan(child) {
            SearchPlan::All => return SearchPlan::All,
            SearchPlan::Or(nested) => normalized.extend(nested),
            other if !normalized.iter().any(|existing| existing == &other) => {
                normalized.push(other)
            }
            _ => {}
        }
    }
    if normalized.is_empty() {
        SearchPlan::All
    } else if normalized.len() == 1 {
        normalized.into_iter().next().unwrap_or(SearchPlan::All)
    } else {
        SearchPlan::Or(normalized)
    }
}

fn candidate_paths_for_plan(
    loaded: &LoadedIndex,
    plan: &SearchPlan,
    requested_filter: Option<&BTreeSet<String>>,
    all_paths: &BTreeSet<String>,
    cache: &mut QueryCache,
    engine: &mut SearchEngineStats,
) -> Result<BTreeSet<String>> {
    match plan {
        SearchPlan::All => Ok(all_paths.clone()),
        SearchPlan::Literal(literal) => {
            if let Some(cached) = cache.literal_candidates.get(literal) {
                return Ok(cached.clone());
            }
            let hashes = select_covering_hashes(loaded, literal);
            if hashes.is_empty() {
                return Ok(all_paths.clone());
            }
            let mut literal_paths: Option<BTreeSet<String>> = None;
            for hash in hashes {
                let current = paths_for_hash(loaded, hash, requested_filter, cache, engine)?;
                literal_paths = Some(match literal_paths {
                    Some(existing) => existing.intersection(&current).cloned().collect(),
                    None => current,
                });
            }
            let resolved = literal_paths.unwrap_or_else(|| all_paths.clone());
            cache
                .literal_candidates
                .insert(literal.clone(), resolved.clone());
            Ok(resolved)
        }
        SearchPlan::And(children) => {
            let mut current: Option<BTreeSet<String>> = None;
            let mut ordered = children.iter().collect::<Vec<_>>();
            ordered.sort_by_key(|child| {
                estimate_plan_cardinality(loaded, child, requested_filter, all_paths.len())
            });
            for child in ordered {
                let child_paths = candidate_paths_for_plan(
                    loaded,
                    child,
                    requested_filter,
                    all_paths,
                    cache,
                    engine,
                )?;
                current = Some(match current {
                    Some(existing) => existing.intersection(&child_paths).cloned().collect(),
                    None => child_paths,
                });
            }
            Ok(current.unwrap_or_else(|| all_paths.clone()))
        }
        SearchPlan::Or(children) => {
            let mut union = BTreeSet::new();
            for child in children {
                union.extend(candidate_paths_for_plan(
                    loaded,
                    child,
                    requested_filter,
                    all_paths,
                    cache,
                    engine,
                )?);
            }
            Ok(union)
        }
    }
}

fn estimate_plan_cardinality(
    loaded: &LoadedIndex,
    plan: &SearchPlan,
    requested_filter: Option<&BTreeSet<String>>,
    all_path_count: usize,
) -> usize {
    match plan {
        SearchPlan::All => all_path_count,
        SearchPlan::Literal(literal) => {
            let hashes = select_covering_hashes(loaded, literal);
            if hashes.is_empty() {
                return all_path_count;
            }
            hashes
                .into_iter()
                .map(|hash| {
                    estimate_hash_cardinality(loaded, hash, requested_filter, all_path_count)
                })
                .min()
                .unwrap_or(all_path_count)
        }
        SearchPlan::And(children) => children
            .iter()
            .map(|child| estimate_plan_cardinality(loaded, child, requested_filter, all_path_count))
            .min()
            .unwrap_or(all_path_count),
        SearchPlan::Or(children) => children
            .iter()
            .map(|child| estimate_plan_cardinality(loaded, child, requested_filter, all_path_count))
            .sum::<usize>()
            .min(all_path_count),
    }
}

fn estimate_hash_cardinality(
    loaded: &LoadedIndex,
    hash: u64,
    requested_filter: Option<&BTreeSet<String>>,
    all_path_count: usize,
) -> usize {
    if let Some(filter) = requested_filter {
        let mut estimate = 0usize;
        if let Some(entries) = lookup_doc_ids_quiet(&loaded.base, hash) {
            for entry in entries {
                if let Some(doc) = loaded.base.docs.get(entry.doc_id as usize) {
                    if loaded.overlay_state.shadowed_paths.contains(&doc.path) {
                        continue;
                    }
                    if filter.contains(&doc.path) {
                        estimate = estimate.saturating_add(1);
                    }
                }
            }
        }
        if let Some(entries) = lookup_doc_ids_quiet(&loaded.overlay, hash) {
            for entry in entries {
                if let Some(doc) = loaded.overlay.docs.get(entry.doc_id as usize) {
                    if loaded.overlay_state.deleted_paths.contains(&doc.path) {
                        continue;
                    }
                    if filter.contains(&doc.path) {
                        estimate = estimate.saturating_add(1);
                    }
                }
            }
        }
        return estimate.min(all_path_count);
    }
    lookup_posting_count(&loaded.base, hash)
        .unwrap_or(0)
        .saturating_add(lookup_posting_count(&loaded.overlay, hash).unwrap_or(0)) as usize
}

fn select_covering_hashes(loaded: &LoadedIndex, literal: &[u8]) -> Vec<u64> {
    let mut candidates = build_covering_candidates(literal);
    candidates.sort_by(|left, right| {
        let left_docs = lookup_posting_count(&loaded.base, left.hash)
            .unwrap_or(0)
            .saturating_add(lookup_posting_count(&loaded.overlay, left.hash).unwrap_or(0));
        let right_docs = lookup_posting_count(&loaded.base, right.hash)
            .unwrap_or(0)
            .saturating_add(lookup_posting_count(&loaded.overlay, right.hash).unwrap_or(0));
        left_docs
            .cmp(&right_docs)
            .then_with(|| (right.end - right.start).cmp(&(left.end - left.start)))
            .then_with(|| left.score.cmp(&right.score).reverse())
            .then_with(|| left.hash.cmp(&right.hash))
    });
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        if seen.insert(candidate.hash) {
            selected.push(candidate.hash);
            if selected.len() >= MAX_LITERAL_COVER {
                break;
            }
        }
    }
    selected
}

fn paths_for_hash(
    loaded: &LoadedIndex,
    hash: u64,
    requested_filter: Option<&BTreeSet<String>>,
    cache: &mut QueryCache,
    engine: &mut SearchEngineStats,
) -> Result<BTreeSet<String>> {
    engine.index_lookups = engine.index_lookups.saturating_add(1);
    let mut paths = BTreeSet::new();

    if let Some(entries) =
        lookup_doc_ids_cached(&loaded.base, LayerKind::Base, hash, cache, engine)?
    {
        for entry in entries {
            if let Some(doc) = loaded.base.docs.get(entry.doc_id as usize) {
                if loaded.overlay_state.shadowed_paths.contains(&doc.path) {
                    continue;
                }
                if path_allowed(&doc.path, requested_filter) {
                    paths.insert(doc.path.clone());
                }
            }
        }
    }
    if let Some(entries) =
        lookup_doc_ids_cached(&loaded.overlay, LayerKind::Overlay, hash, cache, engine)?
    {
        for entry in entries {
            if let Some(doc) = loaded.overlay.docs.get(entry.doc_id as usize) {
                if loaded.overlay_state.deleted_paths.contains(&doc.path) {
                    continue;
                }
                if path_allowed(&doc.path, requested_filter) {
                    paths.insert(doc.path.clone());
                }
            }
        }
    }
    Ok(paths)
}

fn lookup_doc_ids_quiet(layer: &LoadedLayer, hash: u64) -> Option<Vec<PostingEntry>> {
    let lookup = layer.lookup.as_ref()?;
    let postings = layer.postings.as_ref()?;
    let meta = lookup_posting_range(lookup, hash)?;
    let offset = meta.offset as usize;
    let len = meta.len as usize;
    if postings.len() < offset + len {
        return None;
    }
    decode_postings(&postings[offset..offset + len]).ok()
}

fn lookup_doc_ids_cached(
    layer: &LoadedLayer,
    layer_kind: LayerKind,
    hash: u64,
    cache: &mut QueryCache,
    engine: &mut SearchEngineStats,
) -> Result<Option<Vec<PostingEntry>>> {
    if let Some(cached) = cache.postings.get(&(layer_kind, hash)) {
        return Ok(cached.clone());
    }
    let value = lookup_doc_ids(layer, hash, engine)?;
    cache.postings.insert((layer_kind, hash), value.clone());
    Ok(value)
}

fn lookup_posting_count(layer: &LoadedLayer, hash: u64) -> Option<u32> {
    let lookup = layer.lookup.as_ref()?;
    Some(lookup_posting_range(lookup, hash)?.doc_count)
}

fn lookup_doc_ids(
    layer: &LoadedLayer,
    hash: u64,
    engine: &mut SearchEngineStats,
) -> Result<Option<Vec<PostingEntry>>> {
    let Some(lookup) = layer.lookup.as_ref() else {
        return Ok(None);
    };
    let Some(postings) = layer.postings.as_ref() else {
        return Ok(None);
    };
    let Some(meta) = lookup_posting_range(lookup, hash) else {
        return Ok(None);
    };
    let offset = meta.offset as usize;
    let len = meta.len as usize;
    if postings.len() < offset + len {
        return Ok(None);
    }
    engine.postings_bytes_read = engine.postings_bytes_read.saturating_add(len as u64);
    Ok(Some(decode_postings(&postings[offset..offset + len])?))
}

fn verify_path(
    root: &Path,
    path: &str,
    verifier: &Verifier,
    max_matches_per_file: Option<usize>,
) -> Result<Vec<SearchMatch>> {
    let bytes = fs::read(root.join(path)).with_context(|| {
        format!(
            "failed to read candidate file '{}'",
            root.join(path).display()
        )
    })?;
    match verifier {
        Verifier::Regex(regex) => {
            let text = String::from_utf8_lossy(&bytes);
            if !regex.is_match(&text) {
                return Ok(Vec::new());
            }
            collect_line_matches(path, &text, max_matches_per_file, |line| {
                regex.is_match(line)
            })
        }
        Verifier::FixedBytes {
            needle,
            case_insensitive,
        } => {
            if !contains_fixed_bytes(&bytes, needle, *case_insensitive) {
                return Ok(Vec::new());
            }
            let text = String::from_utf8_lossy(&bytes);
            let normalized_needle = case_insensitive.then(|| normalize_for_index(needle));
            collect_line_matches(path, &text, max_matches_per_file, |line| {
                if *case_insensitive {
                    normalize_for_index(line.as_bytes())
                        .windows(needle.len())
                        .any(|window| window == normalized_needle.as_deref().unwrap_or_default())
                } else {
                    line.as_bytes()
                        .windows(needle.len())
                        .any(|window| window == needle.as_slice())
                }
            })
        }
    }
}

fn collect_line_matches<F>(
    path: &str,
    text: &str,
    max_matches_per_file: Option<usize>,
    mut predicate: F,
) -> Result<Vec<SearchMatch>>
where
    F: FnMut(&str) -> bool,
{
    let mut matches = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if !predicate(line) {
            continue;
        }
        matches.push(SearchMatch {
            path: path.to_string(),
            line: idx + 1,
            text: line.to_string(),
        });
        if max_matches_per_file.is_some_and(|limit| matches.len() >= limit) {
            break;
        }
    }
    Ok(matches)
}

fn contains_fixed_bytes(bytes: &[u8], needle: &[u8], case_insensitive: bool) -> bool {
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    if case_insensitive {
        let haystack = normalize_for_index(bytes);
        let normalized_needle = normalize_for_index(needle);
        haystack
            .windows(normalized_needle.len())
            .any(|window| window == normalized_needle.as_slice())
    } else {
        bytes.windows(needle.len()).any(|window| window == needle)
    }
}

fn scan_documents(root: &Path) -> Result<Vec<IndexedDocument>> {
    let mut docs = Vec::new();
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if let Some(indexed) = index_document(root, path)? {
            docs.push(indexed);
        }
    }
    docs.sort_by(|left, right| left.path.cmp(&right.path));
    for (idx, doc) in docs.iter_mut().enumerate() {
        doc.doc_id = idx as u32;
    }
    Ok(docs)
}

struct IndexedDocument {
    doc_id: u32,
    path: String,
    size: u64,
    mtime_secs: u64,
    fingerprint: String,
    grams: Vec<IndexedGram>,
}

fn index_document(root: &Path, path: &Path) -> Result<Option<IndexedDocument>> {
    let Some(relative) = path.strip_prefix(root).ok() else {
        return Ok(None);
    };
    let normalized = relative.to_string_lossy().replace('\\', "/");
    if normalized.starts_with(".git/")
        || normalized.starts_with(".packet28/")
        || normalized.starts_with("target/")
        || normalized.starts_with("node_modules/")
    {
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() as usize > MAX_INDEXED_FILE_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() || bytes.contains(&0) {
        return Ok(None);
    }
    let grams = build_indexed_grams(&bytes);
    let fingerprint = blake3::hash(&bytes).to_hex().to_string();
    Ok(Some(IndexedDocument {
        doc_id: 0,
        path: normalized,
        size: metadata.len(),
        mtime_secs: mtime_secs(&metadata),
        fingerprint,
        grams,
    }))
}

fn build_layer(
    root: &Path,
    docs: &[IndexedDocument],
    lookup_name: &str,
    postings_name: &str,
    docs_name: &str,
) -> Result<LoadedLayer> {
    fs::create_dir_all(regex_index_dir(root))?;
    let segment_paths = write_segment_files(root, lookup_name, docs)?;
    let (rows, postings) = merge_segment_files(&segment_paths)?;
    cleanup_segment_files(&segment_paths);
    let mut lookup = Vec::with_capacity(rows.len() * LOOKUP_ROW_BYTES);
    for (hash, offset, len, doc_count) in rows {
        lookup.extend_from_slice(&hash.to_le_bytes());
        lookup.extend_from_slice(&offset.to_le_bytes());
        lookup.extend_from_slice(&len.to_le_bytes());
        lookup.extend_from_slice(&doc_count.to_le_bytes());
    }
    let serialized_docs = docs
        .iter()
        .map(|doc| DocRecord {
            doc_id: doc.doc_id,
            path: doc.path.clone(),
            size: doc.size,
            mtime_secs: doc.mtime_secs,
            fingerprint: doc.fingerprint.clone(),
        })
        .collect::<Vec<_>>();
    write_atomic(regex_index_dir(root).join(lookup_name), &lookup)?;
    write_atomic(regex_index_dir(root).join(postings_name), &postings)?;
    write_atomic(
        regex_index_dir(root).join(docs_name),
        &bincode::serialize(&serialized_docs)?,
    )?;
    load_layer(root, lookup_name, postings_name, docs_name)
}

fn write_segment_files(
    root: &Path,
    lookup_name: &str,
    docs: &[IndexedDocument],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for (segment_idx, batch) in docs.chunks(SEGMENT_DOC_BATCH_SIZE).enumerate() {
        let mut pairs = Vec::<(u64, u32, u8)>::new();
        for doc in batch {
            for gram in &doc.grams {
                pairs.push((gram.hash, doc.doc_id, gram.summary.0));
            }
        }
        pairs.sort_unstable();
        pairs.dedup();
        let path = regex_index_dir(root).join(format!("{lookup_name}.{segment_idx:05}.segment"));
        write_segment_file(&path, &pairs)?;
        paths.push(path);
    }
    Ok(paths)
}

fn write_segment_file(path: &Path, pairs: &[(u64, u32, u8)]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    for (hash, doc_id, summary) in pairs {
        file.write_all(&hash.to_le_bytes())?;
        file.write_all(&doc_id.to_le_bytes())?;
        file.write_all(&[*summary])?;
    }
    file.flush()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn merge_segment_files(segment_paths: &[PathBuf]) -> Result<(Vec<(u64, u64, u32, u32)>, Vec<u8>)> {
    let mut readers = Vec::new();
    let mut heap = BinaryHeap::<Reverse<HeapItem>>::new();
    for (segment_idx, path) in segment_paths.iter().enumerate() {
        let mut reader = BufReader::new(File::open(path)?);
        if let Some((hash, doc_id, summary)) = read_segment_pair(&mut reader)? {
            heap.push(Reverse(HeapItem {
                hash,
                doc_id,
                summary,
                segment_idx,
            }));
        }
        readers.push(reader);
    }

    let mut rows = Vec::<(u64, u64, u32, u32)>::new();
    let mut postings = Vec::new();
    let mut current_hash = None::<u64>;
    let mut current_docs = Vec::<PostingEntry>::new();

    while let Some(Reverse(item)) = heap.pop() {
        if current_hash != Some(item.hash) {
            flush_posting_group(&mut rows, &mut postings, current_hash, &current_docs);
            current_hash = Some(item.hash);
            current_docs.clear();
        }
        match current_docs.last_mut() {
            Some(last) if last.doc_id == item.doc_id => last
                .summary
                .update(PositionSummary(item.summary).last_bucket()),
            _ => current_docs.push(PostingEntry {
                doc_id: item.doc_id,
                summary: PositionSummary(item.summary),
            }),
        }
        if let Some((next_hash, next_doc_id, next_summary)) =
            read_segment_pair(&mut readers[item.segment_idx])?
        {
            heap.push(Reverse(HeapItem {
                hash: next_hash,
                doc_id: next_doc_id,
                summary: next_summary,
                segment_idx: item.segment_idx,
            }));
        }
    }
    flush_posting_group(&mut rows, &mut postings, current_hash, &current_docs);
    Ok((rows, postings))
}

fn read_segment_pair(reader: &mut BufReader<File>) -> Result<Option<(u64, u32, u8)>> {
    let mut record = [0u8; SEGMENT_RECORD_BYTES];
    match reader.read_exact(&mut record) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some((
        u64::from_le_bytes(record[0..8].try_into().expect("segment hash width")),
        u32::from_le_bytes(record[8..12].try_into().expect("segment doc id width")),
        record[12],
    )))
}

fn flush_posting_group(
    rows: &mut Vec<(u64, u64, u32, u32)>,
    postings: &mut Vec<u8>,
    current_hash: Option<u64>,
    current_docs: &[PostingEntry],
) {
    let Some(hash) = current_hash else {
        return;
    };
    if current_docs.is_empty() {
        return;
    }
    let offset = postings.len() as u64;
    let encoded = encode_postings(current_docs);
    postings.extend_from_slice(&encoded);
    rows.push((
        hash,
        offset,
        encoded.len() as u32,
        current_docs.len() as u32,
    ));
}

fn cleanup_segment_files(segment_paths: &[PathBuf]) {
    for path in segment_paths {
        let _ = fs::remove_file(path);
    }
}

fn load_layer(
    root: &Path,
    lookup_name: &str,
    postings_name: &str,
    docs_name: &str,
) -> Result<LoadedLayer> {
    let dir = regex_index_dir(root);
    let docs_path = dir.join(docs_name);
    let lookup_path = dir.join(lookup_name);
    let postings_path = dir.join(postings_name);
    let docs_exists = docs_path.exists();
    let lookup_exists = lookup_path.exists();
    let postings_exists = postings_path.exists();
    let present_files = docs_exists as u8 + lookup_exists as u8 + postings_exists as u8;
    if present_files > 0 && present_files < 3 {
        return Err(anyhow!(
            "partial regex index layer detected for '{}'",
            docs_name
        ));
    }
    let docs = if docs_path.exists() {
        let raw = fs::read(&docs_path)?;
        bincode::deserialize::<Vec<DocRecord>>(&raw)?
    } else {
        Vec::new()
    };
    let doc_ids_by_path = docs
        .iter()
        .map(|doc| (doc.path.clone(), doc.doc_id))
        .collect::<HashMap<_, _>>();
    let lookup = mmap_optional(&lookup_path)?;
    let postings = mmap_optional(&postings_path)?;
    Ok(LoadedLayer {
        docs,
        doc_ids_by_path,
        lookup,
        postings,
    })
}

fn write_atomic(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.flush()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn mark_manifest_unloaded(manifest: &mut RegexIndexManifest, status: &str, reason: String) {
    manifest.status = status.to_string();
    manifest.stale_reason = Some(reason.clone());
    manifest.last_error = Some(reason);
}

fn mmap_optional(path: &Path) -> Result<Option<Mmap>> {
    if !path.exists() || fs::metadata(path)?.len() == 0 {
        return Ok(None);
    }
    let file = File::open(path)?;
    let map = unsafe { Mmap::map(&file)? };
    Ok(Some(map))
}

fn encode_postings(entries: &[PostingEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut previous = 0u32;
    for entry in entries {
        let delta = entry.doc_id.saturating_sub(previous);
        encode_varint(delta, &mut out);
        previous = entry.doc_id;
    }
    for entry in entries {
        out.push(entry.summary.0);
    }
    out
}

fn decode_postings(bytes: &[u8]) -> Result<Vec<PostingEntry>> {
    if bytes.len() < 4 {
        return Err(anyhow!("invalid posting block"));
    }
    let count = u32::from_le_bytes(bytes[0..4].try_into().expect("length checked")) as usize;
    let mut doc_ids = Vec::with_capacity(count);
    let mut index = 4usize;
    let mut current = 0u32;
    while index < bytes.len() && doc_ids.len() < count {
        let (delta, consumed) = decode_varint(&bytes[index..])?;
        current = current.saturating_add(delta);
        doc_ids.push(current);
        index += consumed;
    }
    let summary_end = index.saturating_add(count);
    if bytes.len() < summary_end {
        return Err(anyhow!("posting block missing positional summaries"));
    }
    Ok(doc_ids
        .into_iter()
        .zip(bytes[index..summary_end].iter().copied())
        .map(|(doc_id, summary)| PostingEntry {
            doc_id,
            summary: PositionSummary(summary),
        })
        .collect())
}

fn encode_varint(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn decode_varint(bytes: &[u8]) -> Result<(u32, usize)> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for (idx, byte) in bytes.iter().enumerate() {
        let value = u32::from(byte & 0x7f);
        result |= value << shift;
        if byte & 0x80 == 0 {
            return Ok((result, idx + 1));
        }
        shift += 7;
    }
    Err(anyhow!("unterminated varint"))
}

fn lookup_posting_range(lookup: &[u8], hash: u64) -> Option<LookupPostingMeta> {
    let rows = lookup.len() / LOOKUP_ROW_BYTES;
    let mut low = 0usize;
    let mut high = rows;
    while low < high {
        let mid = low + (high - low) / 2;
        let start = mid * LOOKUP_ROW_BYTES;
        let current = u64::from_le_bytes(lookup[start..start + 8].try_into().ok()?);
        if current == hash {
            let offset = u64::from_le_bytes(lookup[start + 8..start + 16].try_into().ok()?);
            let len = u32::from_le_bytes(lookup[start + 16..start + 20].try_into().ok()?);
            let doc_count = u32::from_le_bytes(lookup[start + 20..start + 24].try_into().ok()?);
            return Some(LookupPostingMeta {
                offset,
                len,
                doc_count,
            });
        }
        if current < hash {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    None
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_all_hashes(bytes: &[u8]) -> Vec<u64> {
    build_indexed_grams(bytes)
        .into_iter()
        .map(|gram| gram.hash)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn build_indexed_grams(bytes: &[u8]) -> Vec<IndexedGram> {
    let normalized = normalize_for_index(bytes);
    let mut by_hash = HashMap::<u64, PositionSummary>::new();
    for (start, gram) in contiguous_short_grams(&normalized) {
        add_indexed_gram(&mut by_hash, hash_bytes(&gram), start, normalized.len());
    }
    for (start, gram) in contiguous_trigrams(&normalized) {
        add_indexed_gram(&mut by_hash, hash_bytes(&gram), start, normalized.len());
    }
    for candidate in collect_sparse_candidates(&normalized) {
        add_indexed_gram(
            &mut by_hash,
            candidate.hash,
            candidate.start,
            normalized.len(),
        );
    }
    let mut grams = by_hash
        .into_iter()
        .map(|(hash, summary)| IndexedGram { hash, summary })
        .collect::<Vec<_>>();
    grams.sort_by_key(|gram| gram.hash);
    grams
}

fn add_indexed_gram(
    by_hash: &mut HashMap<u64, PositionSummary>,
    hash: u64,
    start: usize,
    byte_len: usize,
) {
    let bucket = bucket_for_offset(start, byte_len);
    by_hash
        .entry(hash)
        .and_modify(|summary| summary.update(bucket))
        .or_insert_with(|| PositionSummary::new(bucket));
}

fn build_covering_hashes(literal: &[u8]) -> Vec<u64> {
    build_covering_candidates(literal)
        .into_iter()
        .map(|candidate| candidate.hash)
        .collect()
}

fn build_covering_candidates(literal: &[u8]) -> Vec<SparseCandidate> {
    let normalized = normalize_for_index(literal);
    if normalized.len() == SHORT_GRAM_BYTES {
        return vec![SparseCandidate {
            hash: hash_bytes(&normalized),
            score: literal_score(&normalized),
            start: 0,
            end: normalized.len(),
        }];
    }
    if normalized.len() < MIN_GRAM_BYTES {
        return Vec::new();
    }
    let mut candidates = collect_sparse_candidates(&normalized);
    if candidates.is_empty() {
        candidates = contiguous_trigrams(&normalized)
            .into_iter()
            .map(|(start, gram)| SparseCandidate {
                hash: hash_bytes(&gram),
                score: literal_score(&gram),
                start,
                end: start + gram.len(),
            })
            .collect();
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.hash.cmp(&right.hash))
    });
    candidates
        .into_iter()
        .fold(
            (BTreeSet::new(), Vec::new()),
            |(mut seen, mut items), candidate| {
                if seen.insert(candidate.hash) {
                    items.push(candidate);
                }
                (seen, items)
            },
        )
        .1
}

fn collect_sparse_candidates(bytes: &[u8]) -> Vec<SparseCandidate> {
    if bytes.len() < MIN_GRAM_BYTES + 1 {
        return Vec::new();
    }
    let weights = pair_weights_for_bytes(bytes);
    let prefixes = pair_weight_prefix_sums(&weights);
    let mut grams = Vec::new();
    for start in 0..=bytes.len() - MIN_GRAM_BYTES {
        let limit = (start + MAX_GRAM_BYTES).min(bytes.len());
        for end in (start + MIN_GRAM_BYTES + 1)..=limit {
            if !is_sparse_candidate_range(&weights, start, end) {
                continue;
            }
            grams.push(SparseCandidate {
                hash: hash_bytes(&bytes[start..end]),
                score: literal_score_range(&prefixes, start, end),
                start,
                end,
            });
        }
    }
    grams
}

fn contiguous_trigrams(bytes: &[u8]) -> Vec<(usize, Vec<u8>)> {
    if bytes.len() < MIN_GRAM_BYTES {
        return Vec::new();
    }
    bytes
        .windows(MIN_GRAM_BYTES)
        .enumerate()
        .map(|(start, window)| (start, window.to_vec()))
        .collect()
}

fn contiguous_short_grams(bytes: &[u8]) -> Vec<(usize, Vec<u8>)> {
    if bytes.len() < SHORT_GRAM_BYTES {
        return Vec::new();
    }
    bytes
        .windows(SHORT_GRAM_BYTES)
        .enumerate()
        .map(|(start, window)| (start, window.to_vec()))
        .collect()
}

fn pair_weights_for_bytes(bytes: &[u8]) -> Vec<u32> {
    bytes
        .windows(2)
        .map(|pair| pair_weight(pair[0], pair[1]))
        .collect()
}

fn pair_weight_prefix_sums(weights: &[u32]) -> Vec<u32> {
    let mut prefix = Vec::with_capacity(weights.len() + 1);
    prefix.push(0u32);
    for weight in weights {
        prefix.push(
            prefix
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(*weight),
        );
    }
    prefix
}

fn is_sparse_candidate_range(weights: &[u32], start: usize, end: usize) -> bool {
    if end.saturating_sub(start) < MIN_GRAM_BYTES + 1 {
        return false;
    }
    let edge_left = weights[start];
    let edge_right = weights[end - 2];
    let interior_max = weights[start + 1..end - 2]
        .iter()
        .copied()
        .max()
        .unwrap_or(0);
    edge_left > interior_max && edge_right > interior_max
}

fn literal_score_range(prefixes: &[u32], start: usize, end: usize) -> u32 {
    let pair_score = prefixes[end - 1].saturating_sub(prefixes[start]);
    pair_score.saturating_add((end - start) as u32 * 32)
}

fn literal_score(bytes: &[u8]) -> u32 {
    let pair_score = bytes
        .windows(2)
        .map(|pair| pair_weight(pair[0], pair[1]))
        .sum::<u32>();
    pair_score.saturating_add((bytes.len() as u32) * 32)
}

fn normalize_for_index(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|byte| byte.to_ascii_lowercase()).collect()
}

fn should_fallback_to_rg(candidate_count: usize, all_path_count: usize) -> bool {
    if candidate_count == 0 {
        return true;
    }
    if all_path_count == 0 {
        return false;
    }
    candidate_count > MAX_INDEX_VERIFY_CANDIDATES
        || candidate_count.saturating_mul(MAX_INDEX_VERIFY_DENOMINATOR)
            > all_path_count.saturating_mul(MAX_INDEX_VERIFY_NUMERATOR)
}

fn bucket_for_offset(offset: usize, byte_len: usize) -> u8 {
    if byte_len <= 1 {
        return 0;
    }
    ((offset.saturating_mul(POSITION_BUCKET_COUNT)) / byte_len)
        .min(POSITION_BUCKET_COUNT.saturating_sub(1)) as u8
}

fn prune_candidates_with_positions(
    loaded: &LoadedIndex,
    plan: &SearchPlan,
    candidates: &BTreeSet<String>,
    cache: &mut QueryCache,
) -> BTreeSet<String> {
    candidates
        .iter()
        .filter(|path| plan_window_for_path(loaded, path, plan, cache).is_some())
        .cloned()
        .collect()
}

fn plan_window_for_path(
    loaded: &LoadedIndex,
    path: &str,
    plan: &SearchPlan,
    cache: &mut QueryCache,
) -> Option<LiteralWindow> {
    match plan {
        SearchPlan::All => Some(LiteralWindow {
            earliest_bucket: 0,
            latest_bucket: (POSITION_BUCKET_COUNT as u8).saturating_sub(1),
        }),
        SearchPlan::Literal(literal) => literal_window_for_path(loaded, path, literal, cache),
        SearchPlan::Or(children) => {
            let mut earliest = u8::MAX;
            let mut latest = 0u8;
            let mut matched = false;
            for child in children {
                let Some(window) = plan_window_for_path(loaded, path, child, cache) else {
                    continue;
                };
                earliest = earliest.min(window.earliest_bucket);
                latest = latest.max(window.latest_bucket);
                matched = true;
            }
            matched.then_some(LiteralWindow {
                earliest_bucket: earliest,
                latest_bucket: latest,
            })
        }
        SearchPlan::And(children) => {
            let mut current: Option<LiteralWindow> = None;
            for child in children {
                let child_window = plan_window_for_path(loaded, path, child, cache)?;
                current = Some(match current {
                    None => child_window,
                    Some(existing) => {
                        if existing.earliest_bucket > child_window.latest_bucket {
                            return None;
                        }
                        LiteralWindow {
                            earliest_bucket: existing
                                .earliest_bucket
                                .min(child_window.earliest_bucket),
                            latest_bucket: existing.latest_bucket.max(child_window.latest_bucket),
                        }
                    }
                });
            }
            current
        }
    }
}

fn literal_window_for_path(
    loaded: &LoadedIndex,
    path: &str,
    literal: &[u8],
    cache: &mut QueryCache,
) -> Option<LiteralWindow> {
    let cache_key = (path.to_string(), literal.to_vec());
    if let Some(cached) = cache.literal_windows.get(&cache_key) {
        return *cached;
    }
    let hashes = select_covering_hashes(loaded, literal);
    if hashes.is_empty() {
        cache.literal_windows.insert(cache_key, None);
        return None;
    }
    let mut overlap_earliest = 0u8;
    let mut overlap_latest = (POSITION_BUCKET_COUNT as u8).saturating_sub(1);
    let mut union_earliest = u8::MAX;
    let mut union_latest = 0u8;
    for hash in hashes {
        let summary = lookup_summary_for_path(loaded, hash, path, cache)?;
        overlap_earliest = overlap_earliest.max(summary.first_bucket());
        overlap_latest = overlap_latest.min(summary.last_bucket());
        union_earliest = union_earliest.min(summary.first_bucket());
        union_latest = union_latest.max(summary.last_bucket());
    }
    let (earliest_bucket, latest_bucket) = if overlap_earliest <= overlap_latest {
        (overlap_earliest, overlap_latest)
    } else {
        (union_earliest, union_latest)
    };
    let window = Some(LiteralWindow {
        earliest_bucket,
        latest_bucket,
    });
    cache.literal_windows.insert(cache_key, window);
    window
}

fn lookup_summary_for_path(
    loaded: &LoadedIndex,
    hash: u64,
    path: &str,
    cache: &mut QueryCache,
) -> Option<PositionSummary> {
    if loaded.overlay_state.deleted_paths.contains(path) {
        return None;
    }
    if let Some(doc_id) = loaded.overlay.doc_ids_by_path.get(path).copied() {
        return lookup_posting_entry(&loaded.overlay, LayerKind::Overlay, hash, doc_id, cache)
            .map(|entry| entry.summary);
    }
    if loaded.overlay_state.shadowed_paths.contains(path) {
        return None;
    }
    let doc_id = loaded.base.doc_ids_by_path.get(path).copied()?;
    lookup_posting_entry(&loaded.base, LayerKind::Base, hash, doc_id, cache)
        .map(|entry| entry.summary)
}

fn lookup_posting_entry(
    layer: &LoadedLayer,
    layer_kind: LayerKind,
    hash: u64,
    doc_id: u32,
    cache: &mut QueryCache,
) -> Option<PostingEntry> {
    let entries = cache
        .postings
        .entry((layer_kind, hash))
        .or_insert_with(|| lookup_doc_ids_quiet(layer, hash))
        .clone()?;
    entries.into_iter().find(|entry| entry.doc_id == doc_id)
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let digest = blake3::hash(bytes);
    u64::from_le_bytes(digest.as_bytes()[0..8].try_into().expect("slice length"))
}

fn regex_index_dir(root: &Path) -> PathBuf {
    root.join(".packet28").join("index").join(REGEX_DIR_NAME)
}

fn overlay_state_path(root: &Path) -> PathBuf {
    regex_index_dir(root).join(OVERLAY_STATE_FILE_NAME)
}

fn manifest_path(root: &Path) -> PathBuf {
    regex_index_dir(root).join(MANIFEST_FILE_NAME)
}

fn load_manifest(root: &Path) -> RegexIndexManifest {
    let path = manifest_path(root);
    let Ok(raw) = fs::read(path) else {
        return RegexIndexManifest::default();
    };
    serde_json::from_slice(&raw).unwrap_or_default()
}

fn save_manifest(root: &Path, manifest: &RegexIndexManifest) -> Result<()> {
    fs::create_dir_all(regex_index_dir(root))?;
    write_atomic(manifest_path(root), &serde_json::to_vec_pretty(manifest)?)
}

fn load_overlay_state(root: &Path) -> OverlayState {
    let Ok(raw) = fs::read(overlay_state_path(root)) else {
        return OverlayState::default();
    };
    serde_json::from_slice(&raw).unwrap_or_default()
}

fn save_overlay_state(root: &Path, overlay: &OverlayState) -> Result<()> {
    write_atomic(
        overlay_state_path(root),
        &serde_json::to_vec_pretty(overlay)?,
    )
}

fn current_git_commit(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn requested_filter_set(paths: &[String]) -> Option<BTreeSet<String>> {
    (!paths.is_empty()).then(|| paths.iter().cloned().collect())
}

fn all_indexed_paths(
    loaded: &LoadedIndex,
    requested_filter: Option<&BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for doc in &loaded.base.docs {
        if loaded.overlay_state.shadowed_paths.contains(&doc.path) {
            continue;
        }
        if path_allowed(&doc.path, requested_filter) {
            paths.insert(doc.path.clone());
        }
    }
    for doc in &loaded.overlay.docs {
        if loaded.overlay_state.deleted_paths.contains(&doc.path) {
            continue;
        }
        if path_allowed(&doc.path, requested_filter) {
            paths.insert(doc.path.clone());
        }
    }
    paths
}

fn path_allowed(path: &str, requested_filter: Option<&BTreeSet<String>>) -> bool {
    requested_filter.map_or(true, |filters| {
        filters
            .iter()
            .any(|filter| path == filter || path.starts_with(&format!("{filter}/")))
    })
}

fn normalize_paths(root: &Path, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| normalize_capture_path(root, path))
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_capture_path(root: &Path, text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return String::new();
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(root) {
            return stripped
                .to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_string();
        }
    }
    let normalized = trimmed
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/");
    normalized.trim_end_matches('/').to_string()
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
        let direct = root.join(&normalized);
        let final_path = if direct.exists() {
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

fn resolve_capture_path_suffix(root: &Path, needle: &str) -> Option<String> {
    let mut matches = BTreeSet::new();
    collect_suffix_matches(root, root, needle, &mut matches);
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
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

fn render_compact_preview(total_match_count: usize, groups: &[SearchGroup]) -> String {
    if total_match_count == 0 {
        return "Search found 0 matches.".to_string();
    }
    let mut lines = vec![format!(
        "Search found {} matches in {} files.",
        total_match_count,
        groups.len()
    )];
    for group in groups.iter().take(12) {
        lines.push(format!("- {} ({})", group.path, group.match_count));
    }
    if groups.len() > 12 {
        lines.push(format!("+{} more files", groups.len() - 12));
    }
    lines.join("\n")
}

fn mtime_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_fixture_index(root: &Path) -> RegexIndexRuntime {
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct Alpha;\npub fn alpha_service() {}\nconst ALPHA: &str = \"Alpha\";\n",
        )
        .unwrap();
        fs::write(
            root.join("src/nested/mod.rs"),
            "pub enum Beta { AlphaVariant }\nfn handle_value() { println!(\"beta\"); }\n",
        )
        .unwrap();
        rebuild_full_index(root, true).unwrap()
    }

    fn assert_parity(root: &Path, runtime: &RegexIndexRuntime, request: SearchRequest) {
        let indexed = indexed_search(root, runtime, &request).unwrap();
        let reducer = packet28_reducer_core::search(root, &request).unwrap();
        assert_eq!(
            indexed.match_count, reducer.match_count,
            "query={}",
            request.query
        );
        assert_eq!(indexed.paths, reducer.paths, "query={}", request.query);
        assert_eq!(indexed.regions, reducer.regions, "query={}", request.query);
    }

    #[test]
    fn sparse_grams_fall_back_to_trigrams() {
        let hashes = build_covering_hashes(b"Packet28");
        assert!(!hashes.is_empty());
    }

    #[test]
    fn build_all_hashes_cover_literal_coverings() {
        let hashes = build_all_hashes(b"pub(crate) fn handle_packet28_search(")
            .into_iter()
            .collect::<BTreeSet<_>>();
        for hash in build_covering_hashes(b"handle_packet28_search") {
            assert!(hashes.contains(&hash));
        }
        for hash in build_covering_hashes(b"fn") {
            assert!(hashes.contains(&hash));
        }
    }

    #[test]
    fn full_rebuild_and_overlay_search_shadow_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Alpha;\n").unwrap();

        let runtime = rebuild_full_index(root, true).unwrap();
        let request = SearchRequest {
            query: "Alpha".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        };
        let result = indexed_search(root, &runtime, &request).unwrap();
        assert_eq!(result.match_count, 1);

        fs::write(root.join("src/lib.rs"), "pub struct Beta;\n").unwrap();
        let updated =
            update_overlay_index(root, Some(&runtime), &[String::from("src/lib.rs")]).unwrap();
        let result = indexed_search(root, &updated, &request).unwrap();
        assert_eq!(result.match_count, 0);
    }

    #[test]
    fn regex_search_builds_and_plan_for_concat_literals() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: "foo.*bar".to_string(),
                ..SearchRequest::default()
            },
            "foo.*bar",
        )
        .unwrap();
        assert_eq!(
            plan,
            SearchPlan::And(vec![
                SearchPlan::Literal(b"foo".to_vec()),
                SearchPlan::Literal(b"bar".to_vec())
            ])
        );
        assert_eq!(fallback, None);
    }

    #[test]
    fn regex_search_builds_or_plan_for_alternation() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: "(foo|bar)baz".to_string(),
                ..SearchRequest::default()
            },
            "(foo|bar)baz",
        )
        .unwrap();
        assert_eq!(
            plan,
            SearchPlan::And(vec![
                SearchPlan::Or(vec![
                    SearchPlan::Literal(b"foo".to_vec()),
                    SearchPlan::Literal(b"bar".to_vec())
                ]),
                SearchPlan::Literal(b"baz".to_vec()),
                SearchPlan::Or(vec![
                    SearchPlan::Literal(b"foobaz".to_vec()),
                    SearchPlan::Literal(b"barbaz".to_vec())
                ])
            ])
        );
        assert_eq!(fallback, None);
    }

    #[test]
    fn regex_search_keeps_short_alternation_branch_selective() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: r"pub\s+(?:fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*".to_string(),
                ..SearchRequest::default()
            },
            r"pub\s+(?:fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*",
        )
        .unwrap();
        assert_eq!(
            plan,
            SearchPlan::And(vec![
                SearchPlan::Literal(b"pub".to_vec()),
                SearchPlan::Or(vec![
                    SearchPlan::Literal(b"fn".to_vec()),
                    SearchPlan::Literal(b"struct".to_vec()),
                    SearchPlan::Literal(b"enum".to_vec())
                ])
            ])
        );
        assert_eq!(fallback, None);
    }

    #[test]
    fn regex_search_extracts_common_prefix_from_alternation_subtree() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: r"(packet28_search|packet28_read_regions)".to_string(),
                ..SearchRequest::default()
            },
            r"(packet28_search|packet28_read_regions)",
        )
        .unwrap();
        assert_eq!(
            plan,
            SearchPlan::And(vec![
                SearchPlan::Or(vec![
                    SearchPlan::Literal(b"packet28_search".to_vec()),
                    SearchPlan::Literal(b"packet28_read_regions".to_vec())
                ]),
                SearchPlan::Literal(b"packet28_".to_vec())
            ])
        );
        assert_eq!(fallback, None);
    }

    #[test]
    fn regex_search_materializes_bounded_repetition_literals() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: r"(ab){3}".to_string(),
                ..SearchRequest::default()
            },
            r"(ab){3}",
        )
        .unwrap();
        assert_eq!(plan, SearchPlan::Literal(b"ababab".to_vec()));
        assert_eq!(fallback, None);
    }

    #[test]
    fn lookup_rows_record_doc_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let runtime = build_fixture_index(root);
        let loaded = runtime.loaded.as_ref().expect("loaded index");
        let hash = build_covering_hashes(b"Alpha")
            .into_iter()
            .next()
            .expect("covering hash");
        let meta = lookup_posting_range(loaded.base.lookup.as_ref().expect("base lookup"), hash)
            .expect("lookup row");
        assert!(meta.doc_count >= 1);
    }

    #[test]
    fn weak_regex_plan_falls_back_to_all() {
        let (plan, fallback) = build_search_plan(
            &SearchRequest {
                query: ".+".to_string(),
                ..SearchRequest::default()
            },
            ".+",
        )
        .unwrap();
        assert_eq!(plan, SearchPlan::All);
        assert!(fallback.is_some());
    }

    #[test]
    fn load_runtime_marks_weight_mismatch_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Alpha;\n").unwrap();
        let runtime = rebuild_full_index(root, true).unwrap();
        let mut manifest = runtime.manifest.clone();
        manifest.weight_table_version = manifest.weight_table_version.saturating_sub(1);
        save_manifest(root, &manifest).unwrap();
        let loaded = load_runtime(root).unwrap();
        assert!(!loaded.is_loaded());
        assert_eq!(loaded.manifest.status, "stale");
        assert!(loaded.manifest.stale_reason.is_some());
    }

    #[test]
    fn load_runtime_marks_partial_layer_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Alpha;\n").unwrap();
        let _runtime = rebuild_full_index(root, true).unwrap();
        fs::remove_file(regex_index_dir(root).join(BASE_POSTINGS_FILE_NAME)).unwrap();
        let loaded = load_runtime(root).unwrap();
        assert!(!loaded.is_loaded());
        assert_eq!(loaded.manifest.status, "corrupt");
        assert!(loaded.manifest.stale_reason.is_some());
    }

    #[test]
    fn guarded_fallback_triggers_for_broad_candidate_sets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        for idx in 0..128 {
            fs::write(
                root.join("src").join(format!("item_{idx}.rs")),
                format!("pub fn item_{idx}() {{}}\n"),
            )
            .unwrap();
        }
        let runtime = rebuild_full_index(root, true).unwrap();
        let request = SearchRequest {
            query: r"pub\s+fn\s+[A-Za-z_][A-Za-z0-9_]*".to_string(),
            ..SearchRequest::default()
        };
        let reason = guarded_fallback_reason(root, &runtime, &request).unwrap();
        assert!(reason.is_some());
    }

    #[test]
    fn guarded_fallback_triggers_when_query_hits_only_skipped_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Alpha;\n").unwrap();
        let large = format!(
            "{}needle_only_in_large_file\n",
            "x".repeat(MAX_INDEXED_FILE_BYTES + 32)
        );
        fs::write(root.join("src/large.txt"), large).unwrap();

        let runtime = rebuild_full_index(root, true).unwrap();
        let request = SearchRequest {
            query: "needle_only_in_large_file".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        };
        let reason = guarded_fallback_reason(root, &runtime, &request).unwrap();
        assert!(reason.is_some());
    }

    #[test]
    fn positional_pruning_respects_literal_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/good.rs"),
            "fn sample() { let _ = foo(); bar(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/bad.rs"),
            "fn sample() { let _ = bar(); foo(); }\n",
        )
        .unwrap();
        let runtime = rebuild_full_index(root, true).unwrap();
        let request = SearchRequest {
            query: "foo.*bar".to_string(),
            ..SearchRequest::default()
        };
        let result = indexed_search(root, &runtime, &request).unwrap();
        assert_eq!(result.paths, vec!["src/good.rs".to_string()]);
    }

    #[test]
    fn indexed_search_matches_directory_filters_with_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let runtime = build_fixture_index(root);
        let request = SearchRequest {
            query: "AlphaVariant".to_string(),
            fixed_string: true,
            requested_paths: vec!["src/nested/".to_string()],
            ..SearchRequest::default()
        };
        let result = indexed_search(root, &runtime, &request).unwrap();
        assert_eq!(result.paths, vec!["src/nested/mod.rs".to_string()]);
    }

    #[test]
    fn indexed_search_handles_non_ascii_ignore_case_fixed_queries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "const CAFE: &str = \"café\";\n").unwrap();

        let runtime = rebuild_full_index(root, true).unwrap();
        let request = SearchRequest {
            query: "CAFÉ".to_string(),
            fixed_string: true,
            case_sensitive: Some(false),
            ..SearchRequest::default()
        };
        let result = indexed_search(root, &runtime, &request).unwrap();
        assert_eq!(result.match_count, 1);
        assert_eq!(result.paths, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn indexed_search_matches_reducer_for_common_queries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let runtime = build_fixture_index(root);

        let requests = vec![
            SearchRequest {
                query: "Alpha".to_string(),
                fixed_string: true,
                ..SearchRequest::default()
            },
            SearchRequest {
                query: "alpha".to_string(),
                fixed_string: true,
                case_sensitive: Some(false),
                ..SearchRequest::default()
            },
            SearchRequest {
                query: r"Alpha|Beta".to_string(),
                ..SearchRequest::default()
            },
            SearchRequest {
                query: "alpha_service".to_string(),
                fixed_string: true,
                whole_word: true,
                ..SearchRequest::default()
            },
            SearchRequest {
                query: "AlphaVariant".to_string(),
                fixed_string: true,
                requested_paths: vec!["src/nested".to_string()],
                ..SearchRequest::default()
            },
        ];

        for request in requests {
            assert_parity(root, &runtime, request);
        }
    }
}
