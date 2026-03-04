use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance, SymbolRef};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
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

#[derive(Debug, Clone, Default)]
struct FileScan {
    path: String,
    symbols: Vec<(String, String)>,
    imports: Vec<String>,
    mtime_secs: u64,
}

#[derive(Debug, Clone)]
struct RankedFileTmp {
    path: String,
    score: f64,
    symbol_count: usize,
    import_count: usize,
}

#[derive(Debug, Clone)]
struct RankedSymbolTmp {
    name: String,
    file: String,
    kind: String,
    score: f64,
}

#[derive(Debug, Clone)]
struct RepoEdgeTmp {
    from: String,
    to: String,
    kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct CacheEntry {
    size: u64,
    mtime_secs: u64,
    symbols: Vec<(String, String)>,
    imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct RepoScanCache {
    version: u32,
    files: BTreeMap<String, CacheEntry>,
}

const MAP_CACHE_VERSION: u32 = 2;
const MAP_CACHE_DIR: &str = ".packet28";
const MAP_CACHE_FILE: &str = "mapy-cache-v1.bin";
const MAP_CACHE_FILE_LEGACY: &str = "mapy-cache-v1.json";

pub fn build_repo_map(req: RepoMapRequest) -> Result<EnvelopeV1<RepoMapPayload>, CovyError> {
    let started = Instant::now();
    let root = PathBuf::from(&req.repo_root);
    if !root.exists() {
        return Err(CovyError::Other(format!(
            "repo_root does not exist: {}",
            req.repo_root
        )));
    }

    let max_files = if req.max_files == 0 {
        80
    } else {
        req.max_files
    };
    let max_symbols = if req.max_symbols == 0 {
        300
    } else {
        req.max_symbols
    };

    let files = scan_repo(&root, req.include_tests)?;
    let mut by_file = BTreeMap::<String, FileScan>::new();
    for file in files {
        by_file.insert(file.path.clone(), file);
    }

    let mut focus_hits = BTreeSet::<(String, String)>::new();
    for path in &req.focus_paths {
        focus_hits.insert(("file".to_string(), normalize_path(path)));
    }
    for sym in &req.focus_symbols {
        focus_hits.insert(("symbol".to_string(), sym.trim().to_string()));
    }

    let mut indegree = BTreeMap::<String, usize>::new();
    let mut outdegree = BTreeMap::<String, usize>::new();
    let mut edges_tmp = Vec::<RepoEdgeTmp>::new();

    let mut basename_to_file = BTreeMap::<String, String>::new();
    for path in by_file.keys() {
        if let Some(base) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
            basename_to_file
                .entry(base.to_string())
                .or_insert_with(|| path.clone());
        }
    }

    for (path, scan) in &by_file {
        for imp in &scan.imports {
            if let Some(target) = basename_to_file.get(imp) {
                if target != path {
                    edges_tmp.push(RepoEdgeTmp {
                        from: path.clone(),
                        to: target.clone(),
                        kind: "import".to_string(),
                    });
                    *outdegree.entry(path.clone()).or_insert(0) += 1;
                    *indegree.entry(target.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    edges_tmp.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    edges_tmp.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);

    let max_degree = by_file
        .keys()
        .map(|path| {
            indegree.get(path).copied().unwrap_or(0) + outdegree.get(path).copied().unwrap_or(0)
        })
        .max()
        .unwrap_or(1)
        .max(1);

    let newest = by_file.values().map(|f| f.mtime_secs).max().unwrap_or(0);
    let oldest = by_file
        .values()
        .map(|f| f.mtime_secs)
        .min()
        .unwrap_or(newest);
    let recency_span = newest.saturating_sub(oldest).max(1);

    let normalized_focus_paths = req
        .focus_paths
        .iter()
        .map(|v| normalize_path(v))
        .collect::<Vec<_>>();

    let focus_symbols = req
        .focus_symbols
        .iter()
        .map(|v| v.trim().to_string())
        .collect::<BTreeSet<_>>();

    let mut ranked_files_tmp = Vec::<RankedFileTmp>::new();
    let mut ranked_symbols_tmp = Vec::<RankedSymbolTmp>::new();

    for (path, scan) in &by_file {
        let focus_match =
            file_focus_match(path, &scan.symbols, &normalized_focus_paths, &focus_symbols);
        let change_proximity = proximity_score(path, &normalized_focus_paths);
        let degree =
            indegree.get(path).copied().unwrap_or(0) + outdegree.get(path).copied().unwrap_or(0);
        let dependency_centrality = degree as f64 / max_degree as f64;
        let recency_hint = (scan.mtime_secs.saturating_sub(oldest)) as f64 / recency_span as f64;

        let score = 0.45 * focus_match
            + 0.25 * change_proximity
            + 0.20 * dependency_centrality
            + 0.10 * recency_hint;

        ranked_files_tmp.push(RankedFileTmp {
            path: path.clone(),
            score,
            symbol_count: scan.symbols.len(),
            import_count: scan.imports.len(),
        });

        for (kind, name) in &scan.symbols {
            let symbol_focus = if focus_symbols.contains(name) {
                1.0
            } else {
                0.0
            };
            let symbol_score = (0.7 * score + 0.3 * symbol_focus).min(1.0);
            ranked_symbols_tmp.push(RankedSymbolTmp {
                name: name.clone(),
                file: path.clone(),
                kind: kind.clone(),
                score: symbol_score,
            });
            if symbol_focus > 0.0 {
                focus_hits.insert(("symbol".to_string(), name.clone()));
            }
        }

        if focus_match > 0.0 {
            focus_hits.insert(("file".to_string(), path.clone()));
        }
    }

    ranked_files_tmp.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });

    ranked_symbols_tmp.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });

    let files_dropped = ranked_files_tmp.len().saturating_sub(max_files);
    let symbols_dropped = ranked_symbols_tmp.len().saturating_sub(max_symbols);

    ranked_files_tmp.truncate(max_files);
    ranked_symbols_tmp.truncate(max_symbols);

    let kept_files = ranked_files_tmp
        .iter()
        .map(|f| f.path.as_str())
        .collect::<BTreeSet<_>>();
    let edge_cap = max_files.saturating_mul(3).max(32);
    edges_tmp.retain(|edge| {
        kept_files.contains(edge.from.as_str()) && kept_files.contains(edge.to.as_str())
    });
    let edges_dropped = edges_tmp.len().saturating_sub(edge_cap);
    edges_tmp.truncate(edge_cap);

    let files = ranked_files_tmp
        .iter()
        .map(|f| FileRef {
            path: f.path.clone(),
            relevance: Some(f.score),
            source: None,
        })
        .collect::<Vec<_>>();

    let symbols = ranked_symbols_tmp
        .iter()
        .map(|s| SymbolRef {
            name: s.name.clone(),
            file: None,
            kind: Some(s.kind.clone()),
            relevance: Some(s.score),
            source: None,
        })
        .collect::<Vec<_>>();

    let file_index = files
        .iter()
        .enumerate()
        .map(|(idx, file)| (file.path.clone(), idx))
        .collect::<BTreeMap<_, _>>();

    let symbol_index = ranked_symbols_tmp
        .iter()
        .enumerate()
        .map(|(idx, symbol)| {
            (
                (
                    symbol.name.clone(),
                    symbol.file.clone(),
                    symbol.kind.clone(),
                ),
                idx,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let ranked_files = ranked_files_tmp
        .iter()
        .filter_map(|f| {
            file_index.get(&f.path).copied().map(|file_idx| RankedFile {
                file_idx,
                score: f.score,
                symbol_count: f.symbol_count,
                import_count: f.import_count,
            })
        })
        .collect::<Vec<_>>();

    let ranked_symbols = ranked_symbols_tmp
        .iter()
        .filter_map(|s| {
            let symbol_idx = symbol_index
                .get(&(s.name.clone(), s.file.clone(), s.kind.clone()))
                .copied()?;
            let file_idx = file_index.get(&s.file).copied()?;
            Some(RankedSymbol {
                symbol_idx,
                file_idx,
                score: s.score,
            })
        })
        .collect::<Vec<_>>();

    let edges = edges_tmp
        .into_iter()
        .filter_map(|edge| {
            let from_file_idx = file_index.get(&edge.from).copied()?;
            let to_file_idx = file_index.get(&edge.to).copied()?;
            Some(RepoEdge {
                from_file_idx,
                to_file_idx,
                kind: edge.kind,
            })
        })
        .collect::<Vec<_>>();

    let focus_hits = focus_hits
        .into_iter()
        .filter_map(|(kind, value)| {
            if kind == "file" {
                file_index.get(&value).copied().map(|ref_idx| FocusHit {
                    kind: "file".to_string(),
                    ref_idx,
                })
            } else if kind == "symbol" {
                let ref_idx = ranked_symbols_tmp.iter().find_map(|s| {
                    if s.name == value {
                        symbol_index
                            .get(&(s.name.clone(), s.file.clone(), s.kind.clone()))
                            .copied()
                    } else {
                        None
                    }
                })?;
                Some(FocusHit {
                    kind: "symbol".to_string(),
                    ref_idx,
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let payload = RepoMapPayload {
        files_ranked: ranked_files,
        symbols_ranked: ranked_symbols,
        edges,
        focus_hits,
        truncation: TruncationSummary {
            files_dropped,
            symbols_dropped,
            edges_dropped,
        },
    };

    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();

    let envelope = EnvelopeV1 {
        version: "1".to_string(),
        tool: "mapy".to_string(),
        kind: "repo_map".to_string(),
        hash: String::new(),
        summary: format!(
            "repo_map files={} symbols={} edges={}",
            payload.files_ranked.len(),
            payload.symbols_ranked.len(),
            payload.edges.len()
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(1.0),
        budget_cost: BudgetCost {
            est_tokens: 0,
            est_bytes: payload_bytes,
            runtime_ms: started.elapsed().as_millis() as u64,
            tool_calls: 1,
            payload_est_tokens: Some((payload_bytes / 4) as u64),
            payload_est_bytes: Some(payload_bytes),
        },
        provenance: Provenance {
            inputs: vec![normalize_path(&req.repo_root)],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash_and_real_budget();

    Ok(envelope)
}

pub fn expand_repo_map_payload(envelope: &EnvelopeV1<RepoMapPayload>) -> RepoMapPayloadRich {
    let files_ranked = envelope
        .payload
        .files_ranked
        .iter()
        .filter_map(|ranked| {
            let file = envelope.files.get(ranked.file_idx)?;
            Some(RankedFileRich {
                path: file.path.clone(),
                score: ranked.score,
                symbol_count: ranked.symbol_count,
                import_count: ranked.import_count,
            })
        })
        .collect::<Vec<_>>();

    let symbols_ranked = envelope
        .payload
        .symbols_ranked
        .iter()
        .filter_map(|ranked| {
            let symbol = envelope.symbols.get(ranked.symbol_idx)?;
            let file = envelope.files.get(ranked.file_idx)?;
            Some(RankedSymbolRich {
                name: symbol.name.clone(),
                file: file.path.clone(),
                kind: symbol.kind.clone().unwrap_or_else(|| "symbol".to_string()),
                score: ranked.score,
            })
        })
        .collect::<Vec<_>>();

    let edges = envelope
        .payload
        .edges
        .iter()
        .filter_map(|edge| {
            let from = envelope.files.get(edge.from_file_idx)?;
            let to = envelope.files.get(edge.to_file_idx)?;
            Some(RepoEdgeRich {
                from: from.path.clone(),
                to: to.path.clone(),
                kind: edge.kind.clone(),
            })
        })
        .collect::<Vec<_>>();

    let focus_hits = envelope
        .payload
        .focus_hits
        .iter()
        .filter_map(|hit| match hit.kind.as_str() {
            "file" => envelope.files.get(hit.ref_idx).map(|file| FocusHitRich {
                kind: "file".to_string(),
                value: file.path.clone(),
            }),
            "symbol" => envelope
                .symbols
                .get(hit.ref_idx)
                .map(|symbol| FocusHitRich {
                    kind: "symbol".to_string(),
                    value: symbol.name.clone(),
                }),
            _ => None,
        })
        .collect::<Vec<_>>();

    RepoMapPayloadRich {
        files_ranked,
        symbols_ranked,
        edges,
        focus_hits,
        truncation: envelope.payload.truncation.clone(),
    }
}

fn scan_repo(root: &Path, include_tests: bool) -> Result<Vec<FileScan>, CovyError> {
    let mut out = Vec::new();
    let mut cache = load_scan_cache(root);
    let mut cache_dirty = false;
    let mut seen = BTreeSet::<String>::new();

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true);
    let root_owned = root.to_path_buf();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let rel = entry
            .path()
            .strip_prefix(&root_owned)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        !is_generated_or_vendor_path(&rel)
    });

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if !is_source_file(path) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        seen.insert(rel.clone());

        if !include_tests && is_test_path(&rel) {
            continue;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let size = metadata.len();
        let mtime_secs = metadata_mtime_secs(&metadata);
        if let Some(entry) = cache.files.get(&rel) {
            if entry.size == size && entry.mtime_secs == mtime_secs {
                out.push(FileScan {
                    path: rel,
                    symbols: entry.symbols.clone(),
                    imports: entry.imports.clone(),
                    mtime_secs,
                });
                continue;
            }
        }

        let content = match std::fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let (symbols, imports) = extract_metadata(&rel, &content);
        cache.files.insert(
            rel.clone(),
            CacheEntry {
                size,
                mtime_secs,
                symbols: symbols.clone(),
                imports: imports.clone(),
            },
        );
        cache_dirty = true;

        out.push(FileScan {
            path: rel,
            symbols,
            imports,
            mtime_secs,
        });
    }

    let original_cache_len = cache.files.len();
    cache.files.retain(|path, _| seen.contains(path));
    cache_dirty |= cache.files.len() != original_cache_len;
    if cache_dirty {
        write_scan_cache(root, &cache);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scan_cache_path(root: &Path) -> PathBuf {
    root.join(MAP_CACHE_DIR).join(MAP_CACHE_FILE)
}

fn load_scan_cache(root: &Path) -> RepoScanCache {
    let path = scan_cache_path(root);
    let raw = if let Ok(raw) = std::fs::read(&path) {
        raw
    } else {
        let legacy_path = root.join(MAP_CACHE_DIR).join(MAP_CACHE_FILE_LEGACY);
        let Ok(raw) = std::fs::read(legacy_path) else {
            return empty_cache();
        };
        raw
    };

    let cache = if let Ok(cache) = bincode::deserialize::<RepoScanCache>(&raw) {
        cache
    } else if let Ok(cache) = serde_json::from_slice::<RepoScanCache>(&raw) {
        // Backward-compatible read path for older JSON cache versions.
        cache
    } else {
        return empty_cache();
    };

    if cache.version != MAP_CACHE_VERSION {
        return empty_cache();
    }

    cache
}

fn write_scan_cache(root: &Path, cache: &RepoScanCache) {
    let path = scan_cache_path(root);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let Ok(encoded) = bincode::serialize(cache) else {
        return;
    };

    let _ = std::fs::write(path, encoded);
}

fn empty_cache() -> RepoScanCache {
    RepoScanCache {
        version: MAP_CACHE_VERSION,
        files: BTreeMap::new(),
    }
}

fn metadata_mtime_secs(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|v| v.to_str()),
        Some("rs")
            | Some("py")
            | Some("js")
            | Some("jsx")
            | Some("ts")
            | Some("tsx")
            | Some("java")
            | Some("go")
            | Some("c")
            | Some("cc")
            | Some("cpp")
            | Some("h")
            | Some("hpp")
    )
}

fn is_generated_or_vendor_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with(".git/")
        || lower.contains("/.git/")
        || lower.starts_with("target/")
        || lower.contains("/target/")
        || lower.starts_with("build/")
        || lower.contains("/build/")
        || lower.starts_with("dist/")
        || lower.contains("/dist/")
        || lower.starts_with("out/")
        || lower.contains("/out/")
        || lower.starts_with("coverage/")
        || lower.contains("/coverage/")
        || lower.starts_with("node_modules/")
        || lower.contains("/node_modules/")
        || lower.contains("/jacoco-resources/")
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.py")
        || lower.ends_with("test.rs")
}

fn extract_symbols(_path: &str, content: &str) -> Vec<(String, String)> {
    let mut out = BTreeSet::<(String, String)>::new();

    for cap in symbol_re().captures_iter(content) {
        let kind = cap.name("kind").map(|m| m.as_str()).unwrap_or("");
        let name = cap.name("name").map(|m| m.as_str()).unwrap_or("");
        if !name.is_empty() {
            out.insert((kind.to_string(), name.to_string()));
        }
    }

    for cap in java_type_re().captures_iter(content) {
        let kind = cap
            .name("kind")
            .map(|m| m.as_str())
            .unwrap_or("class")
            .to_ascii_lowercase();
        let name = cap.name("name").map(|m| m.as_str()).unwrap_or("");
        if !name.is_empty() {
            out.insert((kind, name.to_string()));
        }
    }

    for cap in java_method_re().captures_iter(content) {
        let name = cap.name("name").map(|m| m.as_str()).unwrap_or("").trim();
        if !name.is_empty() && !is_reserved_word(name) {
            out.insert(("method".to_string(), name.to_string()));
        }
    }

    out.into_iter().collect()
}

fn extract_imports(content: &str) -> Vec<String> {
    let mut out = BTreeSet::<String>::new();

    for cap in import_re().captures_iter(content) {
        let target = cap.name("target").map(|m| m.as_str()).unwrap_or("").trim();
        if target.is_empty() {
            continue;
        }
        let normalized = target
            .rsplit(['/', '.', ':'])
            .next()
            .unwrap_or(target)
            .trim()
            .to_string();
        if !normalized.is_empty() {
            out.insert(normalized);
        }
    }

    out.into_iter().collect()
}

fn extract_metadata(path: &str, content: &str) -> (Vec<(String, String)>, Vec<String>) {
    if let Some(language) = detect_source_language(path) {
        if let Some((symbols, imports)) = extract_metadata_ast(language, content) {
            return (symbols, imports);
        }
    }

    (extract_symbols(path, content), extract_imports(content))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceLanguage {
    Java,
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    Cpp,
}

fn detect_source_language(path: &str) -> Option<SourceLanguage> {
    if path.ends_with(".java") {
        return Some(SourceLanguage::Java);
    }
    if path.ends_with(".rs") {
        return Some(SourceLanguage::Rust);
    }
    if path.ends_with(".py") {
        return Some(SourceLanguage::Python);
    }
    if path.ends_with(".ts") || path.ends_with(".tsx") {
        return Some(SourceLanguage::TypeScript);
    }
    if path.ends_with(".js") || path.ends_with(".jsx") {
        return Some(SourceLanguage::JavaScript);
    }
    if path.ends_with(".go") {
        return Some(SourceLanguage::Go);
    }
    if path.ends_with(".cpp")
        || path.ends_with(".cc")
        || path.ends_with(".cxx")
        || path.ends_with(".hpp")
        || path.ends_with(".hh")
        || path.ends_with(".h")
        || path.ends_with(".c")
    {
        return Some(SourceLanguage::Cpp);
    }
    None
}

fn file_focus_match(
    path: &str,
    symbols: &[(String, String)],
    focus_paths: &[String],
    focus_symbols: &BTreeSet<String>,
) -> f64 {
    let path_match: f64 = if focus_paths.iter().any(|p| path == p || path.starts_with(p)) {
        1.0
    } else {
        0.0
    };

    let symbol_match: f64 = if symbols.iter().any(|(_, name)| focus_symbols.contains(name)) {
        1.0
    } else {
        0.0
    };

    path_match.max(symbol_match)
}

fn proximity_score(path: &str, focus_paths: &[String]) -> f64 {
    if focus_paths.is_empty() {
        return 0.0;
    }

    let mut best = 0.0f64;
    for focus in focus_paths {
        let prefix = common_prefix_segments(path, focus);
        let denom = focus.split('/').filter(|v| !v.is_empty()).count().max(1) as f64;
        let score = (prefix as f64 / denom).clamp(0.0, 1.0);
        if score > best {
            best = score;
        }
    }
    best
}

fn common_prefix_segments(a: &str, b: &str) -> usize {
    let aa = a.split('/').filter(|v| !v.is_empty()).collect::<Vec<_>>();
    let bb = b.split('/').filter(|v| !v.is_empty()).collect::<Vec<_>>();
    let mut count = 0usize;
    for (x, y) in aa.iter().zip(bb.iter()) {
        if x == y {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn symbol_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:(?P<kind>fn|struct|enum|trait|impl|class|interface|def|function)\s+)(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid symbol regex")
    })
}

fn java_type_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public|protected|private|abstract|static|final|sealed|non-sealed|\s)*\b(?P<kind>class|interface|enum|record)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid java type regex")
    })
}

fn java_method_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public|protected|private|static|final|abstract|synchronized|native|strictfp|\s)+(?:<[^>]+>\s*)?(?:[A-Za-z_][A-Za-z0-9_<>\[\],.?]*\s+)+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\([^;\n{}]*\)\s*(?:\{|throws\b)",
        )
        .expect("valid java method regex")
    })
}

thread_local! {
    static JAVA_PARSER: RefCell<Option<Parser>> = RefCell::new(init_java_parser());
    static RUST_PARSER: RefCell<Option<Parser>> = RefCell::new(init_rust_parser());
    static PYTHON_PARSER: RefCell<Option<Parser>> = RefCell::new(init_python_parser());
    static TYPESCRIPT_PARSER: RefCell<Option<Parser>> = RefCell::new(init_typescript_parser());
    static JAVASCRIPT_PARSER: RefCell<Option<Parser>> = RefCell::new(init_javascript_parser());
    static GO_PARSER: RefCell<Option<Parser>> = RefCell::new(init_go_parser());
    static CPP_PARSER: RefCell<Option<Parser>> = RefCell::new(init_cpp_parser());
}

fn init_java_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_java::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_rust_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_python_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_python::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_typescript_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_javascript_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_javascript::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_go_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_go::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn init_cpp_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    let language = tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE);
    parser.set_language(&language).ok()?;
    Some(parser)
}

fn extract_metadata_ast(
    language: SourceLanguage,
    content: &str,
) -> Option<(Vec<(String, String)>, Vec<String>)> {
    match language {
        SourceLanguage::Java => extract_java_metadata_ast(content),
        SourceLanguage::Rust => extract_rust_metadata_ast(content),
        SourceLanguage::Python => extract_python_metadata_ast(content),
        SourceLanguage::TypeScript => extract_typescript_metadata_ast(content),
        SourceLanguage::JavaScript => extract_javascript_metadata_ast(content),
        SourceLanguage::Go => extract_go_metadata_ast(content),
        SourceLanguage::Cpp => extract_cpp_metadata_ast(content),
    }
}

fn extract_java_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    JAVA_PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        let parser = parser.as_mut()?;
        let tree = parser.parse(content, None)?;

        let mut symbols = BTreeSet::<(String, String)>::new();
        let mut imports = BTreeSet::<String>::new();
        walk_java_ast(
            tree.root_node(),
            content.as_bytes(),
            &mut symbols,
            &mut imports,
        );
        Some((symbols.into_iter().collect(), imports.into_iter().collect()))
    })
}

fn extract_rust_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    RUST_PARSER.with(|cell| extract_with_walker(cell, content, walk_rust_ast))
}

fn extract_python_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    PYTHON_PARSER.with(|cell| extract_with_walker(cell, content, walk_python_ast))
}

fn extract_typescript_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    TYPESCRIPT_PARSER.with(|cell| extract_with_walker(cell, content, walk_typescript_ast))
}

fn extract_javascript_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    JAVASCRIPT_PARSER.with(|cell| extract_with_walker(cell, content, walk_javascript_ast))
}

fn extract_go_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    GO_PARSER.with(|cell| extract_with_walker(cell, content, walk_go_ast))
}

fn extract_cpp_metadata_ast(content: &str) -> Option<(Vec<(String, String)>, Vec<String>)> {
    CPP_PARSER.with(|cell| extract_with_walker(cell, content, walk_cpp_ast))
}

fn extract_with_walker(
    cell: &RefCell<Option<Parser>>,
    content: &str,
    walker: fn(Node<'_>, &[u8], &mut BTreeSet<(String, String)>, &mut BTreeSet<String>),
) -> Option<(Vec<(String, String)>, Vec<String>)> {
    let mut parser = cell.borrow_mut();
    let parser = parser.as_mut()?;
    let tree = parser.parse(content, None)?;
    let mut symbols = BTreeSet::<(String, String)>::new();
    let mut imports = BTreeSet::<String>::new();
    walker(
        tree.root_node(),
        content.as_bytes(),
        &mut symbols,
        &mut imports,
    );
    Some((symbols.into_iter().collect(), imports.into_iter().collect()))
}

fn walk_java_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "class_declaration" => insert_named_child(node, src, "class", symbols),
        "interface_declaration" => insert_named_child(node, src, "interface", symbols),
        "enum_declaration" => insert_named_child(node, src, "enum", symbols),
        "record_declaration" => insert_named_child(node, src, "record", symbols),
        "method_declaration" => insert_named_child(node, src, "method", symbols),
        "constructor_declaration" => insert_named_child(node, src, "constructor", symbols),
        "import_declaration" => insert_java_import(node, src, imports),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_java_ast(child, src, symbols, imports);
    }
}

fn walk_rust_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_item" => insert_name_or_identifier(node, src, "function", symbols),
        "struct_item" => insert_name_or_identifier(node, src, "struct", symbols),
        "enum_item" => insert_name_or_identifier(node, src, "enum", symbols),
        "trait_item" => insert_name_or_identifier(node, src, "trait", symbols),
        "type_item" => insert_name_or_identifier(node, src, "type", symbols),
        "use_declaration" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_rust_ast);
}

fn walk_python_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_definition" => insert_name_or_identifier(node, src, "function", symbols),
        "class_definition" => insert_name_or_identifier(node, src, "class", symbols),
        "import_statement" | "import_from_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_python_ast);
}

fn walk_typescript_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "class_declaration" => insert_name_or_identifier(node, src, "class", symbols),
        "interface_declaration" => insert_name_or_identifier(node, src, "interface", symbols),
        "type_alias_declaration" => insert_name_or_identifier(node, src, "type", symbols),
        "enum_declaration" => insert_name_or_identifier(node, src, "enum", symbols),
        "method_definition" => insert_name_or_identifier(node, src, "method", symbols),
        "import_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_typescript_ast);
}

fn walk_javascript_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "class_declaration" => insert_name_or_identifier(node, src, "class", symbols),
        "method_definition" => insert_name_or_identifier(node, src, "method", symbols),
        "import_statement" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_javascript_ast);
}

fn walk_go_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_declaration" => insert_name_or_identifier(node, src, "function", symbols),
        "method_declaration" => insert_name_or_identifier(node, src, "method", symbols),
        "type_spec" => insert_name_or_identifier(node, src, "type", symbols),
        "import_declaration" | "import_spec" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_go_ast);
}

fn walk_cpp_ast(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
) {
    match node.kind() {
        "function_definition" => insert_name_or_identifier(node, src, "function", symbols),
        "class_specifier" => insert_name_or_identifier(node, src, "class", symbols),
        "struct_specifier" => insert_name_or_identifier(node, src, "struct", symbols),
        "enum_specifier" => insert_name_or_identifier(node, src, "enum", symbols),
        "preproc_include" => insert_import_leaf(node, src, imports),
        _ => {}
    }

    walk_children(node, src, symbols, imports, walk_cpp_ast);
}

fn walk_children(
    node: Node<'_>,
    src: &[u8],
    symbols: &mut BTreeSet<(String, String)>,
    imports: &mut BTreeSet<String>,
    walker: fn(Node<'_>, &[u8], &mut BTreeSet<(String, String)>, &mut BTreeSet<String>),
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walker(child, src, symbols, imports);
    }
}

fn insert_named_child(
    node: Node<'_>,
    src: &[u8],
    kind: &str,
    out: &mut BTreeSet<(String, String)>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(src) else {
        return;
    };
    let name = name.trim();
    if !name.is_empty() && !is_reserved_word(name) {
        out.insert((kind.to_string(), name.to_string()));
    }
}

fn insert_name_or_identifier(
    node: Node<'_>,
    src: &[u8],
    kind: &str,
    out: &mut BTreeSet<(String, String)>,
) {
    if let Some(name_node) = node.child_by_field_name("name") {
        if let Ok(name) = name_node.utf8_text(src) {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !is_reserved_word(trimmed) {
                out.insert((kind.to_string(), trimmed.to_string()));
                return;
            }
        }
    }

    if let Some(identifier) = find_identifier(node, src, 0) {
        if !identifier.is_empty() && !is_reserved_word(&identifier) {
            out.insert((kind.to_string(), identifier));
        }
    }
}

fn insert_java_import(node: Node<'_>, src: &[u8], out: &mut BTreeSet<String>) {
    let Ok(import_text) = node.utf8_text(src) else {
        return;
    };

    let mut normalized = import_text.trim().trim_end_matches(';').trim().to_string();
    if let Some(stripped) = normalized.strip_prefix("import") {
        normalized = stripped.trim().to_string();
    }
    if let Some(stripped) = normalized.strip_prefix("static") {
        normalized = stripped.trim().to_string();
    }

    let leaf = normalized
        .trim_end_matches(".*")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim();
    if !leaf.is_empty() && !is_reserved_word(leaf) {
        out.insert(leaf.to_string());
    }
}

fn insert_import_leaf(node: Node<'_>, src: &[u8], out: &mut BTreeSet<String>) {
    let Ok(raw) = node.utf8_text(src) else {
        return;
    };

    let mut normalized = raw
        .replace("import", " ")
        .replace("from", " ")
        .replace("use", " ")
        .replace("#include", " ")
        .replace("static", " ")
        .replace(';', " ")
        .replace('{', " ")
        .replace('}', " ")
        .replace('\"', " ")
        .replace('\'', " ")
        .replace('<', " ")
        .replace('>', " ");
    normalized = normalized.trim().to_string();
    if normalized.is_empty() {
        return;
    }

    let leaf = normalized
        .trim_end_matches(".*")
        .rsplit(['/', '.', ':'])
        .next()
        .unwrap_or("")
        .trim();
    if !leaf.is_empty() && !is_reserved_word(leaf) {
        out.insert(leaf.to_string());
    }
}

fn find_identifier(node: Node<'_>, src: &[u8], depth: usize) -> Option<String> {
    if depth > 5 {
        return None;
    }
    if node.kind() == "identifier" || node.kind() == "type_identifier" {
        let text = node.utf8_text(src).ok()?.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_identifier(child, src, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "return" | "new" | "do" | "case"
    )
}

fn import_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:use|import|from|#include)\s+(?:<)?(?P<target>[A-Za-z0-9_./:-]+)")
            .expect("valid import regex")
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn deterministic_tie_breaks_are_lexical() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

        let env = build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            max_files: 10,
            max_symbols: 10,
            ..RepoMapRequest::default()
        })
        .unwrap();

        assert!(!env.payload.files_ranked.is_empty());
        let left = env
            .files
            .get(env.payload.files_ranked[0].file_idx)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let right = env
            .files
            .get(env.payload.files_ranked[1].file_idx)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        assert!(left <= right);
    }

    #[test]
    fn excludes_generated_paths_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
        std::fs::create_dir_all(root.join("target/site/jacoco/jacoco-resources")).unwrap();

        std::fs::write(
            root.join("src/main/java/com/example/Calculator.java"),
            "public class Calculator { public int add(int a, int b) { return a + b; } }",
        )
        .unwrap();
        std::fs::write(
            root.join("target/site/jacoco/jacoco-resources/prettify.js"),
            "function prettyPrint() {}",
        )
        .unwrap();

        let env = build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            ..RepoMapRequest::default()
        })
        .unwrap();

        assert!(env.files.iter().all(|f| !f.path.contains("target/")));
    }

    #[test]
    fn extracts_java_symbols_with_modifiers() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
        std::fs::write(
            root.join("src/main/java/com/example/Calculator.java"),
            r#"
package com.example;

public class Calculator {
  public int add(int a, int b) { return a + b; }
  private static String label() { return "x"; }
}
"#,
        )
        .unwrap();

        let env = build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            ..RepoMapRequest::default()
        })
        .unwrap();

        let names = env
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("Calculator"));
        assert!(names.contains("add"));
        assert!(names.contains("label"));
    }

    #[test]
    fn extracts_java_import_edges_from_ast() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
        std::fs::write(
            root.join("src/main/java/com/example/Util.java"),
            "package com.example; public class Util {}",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main/java/com/example/Calculator.java"),
            r#"
package com.example;
import com.example.Util;
public class Calculator {
  public int add(int a, int b) { return a + b; }
}
"#,
        )
        .unwrap();

        let env = build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            ..RepoMapRequest::default()
        })
        .unwrap();

        assert!(
            env.payload.edges.iter().any(|edge| {
                let from = env
                    .files
                    .get(edge.from_file_idx)
                    .map(|f| f.path.as_str())
                    .unwrap_or("");
                let to = env
                    .files
                    .get(edge.to_file_idx)
                    .map(|f| f.path.as_str())
                    .unwrap_or("");
                from.ends_with("Calculator.java") && to.ends_with("Util.java")
            }),
            "expected import edge from Calculator.java to Util.java"
        );
    }

    #[test]
    fn writes_incremental_cache_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            ..RepoMapRequest::default()
        })
        .unwrap();

        assert!(root.join(".packet28/mapy-cache-v1.bin").exists());
    }

    #[test]
    fn extracts_symbols_for_non_java_languages() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();

        std::fs::write(
            root.join("src/lib.rs"),
            "fn parse_input() {}\nstruct Engine;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.py"),
            "class Parser:\n  pass\n\ndef parse_input():\n  return 1\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/app.ts"),
            "interface Runner {}\nfunction parseInput() { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/app.js"),
            "class Handler {}\nfunction handleInput() { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.go"),
            "package main\nimport \"fmt\"\nfunc ParseInput() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.cpp"),
            "#include <vector>\nclass Parser{};\nint parse_input(){ return 0; }\n",
        )
        .unwrap();

        let env = build_repo_map(RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            ..RepoMapRequest::default()
        })
        .unwrap();

        let names = env
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("parse_input") || names.contains("ParseInput"));
        assert!(names.contains("Engine"));
        assert!(names.contains("Parser") || names.contains("Handler"));
    }
}
