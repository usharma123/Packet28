use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RepoMapRequest {
    pub repo_root: String,
    pub focus_paths: Vec<String>,
    pub focus_symbols: Vec<String>,
    pub max_files: usize,
    pub max_symbols: usize,
    pub include_tests: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedFile {
    pub file_idx: usize,
    pub score: f64,
    pub symbol_count: usize,
    pub import_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedSymbol {
    pub symbol_idx: usize,
    pub file_idx: usize,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoEdge {
    pub from_file_idx: usize,
    pub to_file_idx: usize,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FocusHit {
    pub kind: String,
    pub ref_idx: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TruncationSummary {
    pub files_dropped: usize,
    pub symbols_dropped: usize,
    pub edges_dropped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoMapPayload {
    pub files_ranked: Vec<RankedFile>,
    pub symbols_ranked: Vec<RankedSymbol>,
    pub edges: Vec<RepoEdge>,
    pub focus_hits: Vec<FocusHit>,
    pub truncation: TruncationSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedFileRich {
    pub path: String,
    pub score: f64,
    pub symbol_count: usize,
    pub import_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedSymbolRich {
    pub name: String,
    pub file: String,
    pub kind: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoEdgeRich {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FocusHitRich {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoMapPayloadRich {
    pub files_ranked: Vec<RankedFileRich>,
    pub symbols_ranked: Vec<RankedSymbolRich>,
    pub edges: Vec<RepoEdgeRich>,
    pub focus_hits: Vec<FocusHitRich>,
    pub truncation: TruncationSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RepoQueryRequest {
    pub repo_root: String,
    pub symbol_query: String,
    pub pattern_query: String,
    pub language: String,
    pub selector: String,
    pub max_results: usize,
    pub include_tests: bool,
    pub exact: bool,
    pub files_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoQueryMatch {
    pub file_idx: usize,
    pub symbol_idx: usize,
    pub line: usize,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoQueryPayload {
    pub query: String,
    pub matches: Vec<RepoQueryMatch>,
    pub truncation: TruncationSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoQueryMatchRich {
    pub file: String,
    pub symbol: String,
    pub kind: String,
    pub line: usize,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoQueryPayloadRich {
    pub query: String,
    pub matches: Vec<RepoQueryMatchRich>,
    pub truncation: TruncationSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord)]
#[serde(default)]
pub struct IndexedSymbolDef {
    pub kind: String,
    pub name: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RepoIndexFileEntry {
    pub path: String,
    pub size: u64,
    pub mtime_secs: u64,
    pub is_test: bool,
    pub symbols: Vec<IndexedSymbolDef>,
    pub imports: Vec<String>,
    pub token_lines: BTreeMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RepoIndexSnapshot {
    pub version: u32,
    pub include_tests: bool,
    pub files: BTreeMap<String, RepoIndexFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoIndexUpdateSummary {
    pub indexed_files: usize,
    pub removed_files: usize,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct CacheEntry {
    pub size: u64,
    pub mtime_secs: u64,
    pub symbols: Vec<(String, String)>,
    pub symbol_defs: Vec<IndexedSymbolDef>,
    pub imports: Vec<String>,
    pub token_lines: BTreeMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct RepoScanCache {
    pub version: u32,
    pub files: BTreeMap<String, CacheEntry>,
}
