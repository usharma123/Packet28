use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance, SymbolRef};

use crate::scan::{
    extract_index_metadata, is_generated_or_vendor_path, is_source_file, is_test_path,
    metadata_mtime_secs, scan_repo,
};
use crate::types::{
    FocusHit, FocusHitRich, IndexedSymbolDef, RankedFile, RankedFileRich, RankedSymbol,
    RankedSymbolRich, RepoEdge, RepoEdgeRich, RepoIndexFileEntry, RepoIndexSnapshot,
    RepoIndexUpdateSummary, RepoMapPayload, RepoMapPayloadRich, RepoMapRequest,
    TruncationSummary,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct FileScan {
    pub path: String,
    pub size: u64,
    pub symbols: Vec<(String, String)>,
    pub symbol_defs: Vec<IndexedSymbolDef>,
    pub imports: Vec<String>,
    pub token_lines: BTreeMap<String, Vec<usize>>,
    pub mtime_secs: u64,
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

pub(crate) const MAP_CACHE_VERSION: u32 = 3;
pub(crate) const MAP_CACHE_DIR: &str = ".packet28";
pub(crate) const MAP_CACHE_FILE: &str = "mapy-cache-v1.bin";
pub(crate) const MAP_CACHE_FILE_LEGACY: &str = "mapy-cache-v1.json";

pub fn build_repo_map(req: RepoMapRequest) -> Result<EnvelopeV1<RepoMapPayload>, CovyError> {
    let root = PathBuf::from(&req.repo_root);
    if !root.exists() {
        return Err(CovyError::Other(format!(
            "repo_root does not exist: {}",
            req.repo_root
        )));
    }
    let scans = scan_repo(&root, req.include_tests)?;
    build_repo_map_from_scans(req, scans)
}

pub fn build_repo_map_from_index(
    req: RepoMapRequest,
    snapshot: &RepoIndexSnapshot,
) -> Result<EnvelopeV1<RepoMapPayload>, CovyError> {
    let scans = snapshot
        .files
        .values()
        .filter(|entry| snapshot.include_tests || !entry.is_test)
        .map(|entry| FileScan {
            path: entry.path.clone(),
            size: entry.size,
            symbols: entry
                .symbols
                .iter()
                .map(|symbol| (symbol.kind.clone(), symbol.name.clone()))
                .collect(),
            symbol_defs: entry.symbols.clone(),
            imports: entry.imports.clone(),
            token_lines: entry.token_lines.clone(),
            mtime_secs: entry.mtime_secs,
        })
        .collect::<Vec<_>>();
    build_repo_map_from_scans(req, scans)
}

fn build_repo_map_from_scans(
    req: RepoMapRequest,
    files: Vec<FileScan>,
) -> Result<EnvelopeV1<RepoMapPayload>, CovyError> {
    let started = Instant::now();
    let max_files = if req.max_files == 0 { 80 } else { req.max_files };
    let max_symbols = if req.max_symbols == 0 {
        300
    } else {
        req.max_symbols
    };
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
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
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

        let (focus_weight, dependency_weight) = if focus_symbols.is_empty() {
            (0.45, 0.20)
        } else {
            (0.55, 0.10)
        };
        let score = focus_weight * focus_match
            + 0.25 * change_proximity
            + dependency_weight * dependency_centrality
            + 0.10 * recency_hint;

        ranked_files_tmp.push(RankedFileTmp {
            path: path.clone(),
            score,
            symbol_count: scan.symbols.len(),
            import_count: scan.imports.len(),
        });

        for (kind, name) in &scan.symbols {
            let symbol_focus = focus_symbols
                .iter()
                .map(|candidate| focus_term_match_score(name, candidate))
                .fold(0.0, f64::max);
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
            file: Some(s.file.clone()),
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

pub fn build_repo_index(root: &Path, include_tests: bool) -> Result<RepoIndexSnapshot, CovyError> {
    if !root.exists() {
        return Err(CovyError::Other(format!(
            "repo_root does not exist: {}",
            root.display()
        )));
    }
    let files = scan_repo(root, include_tests)?;
    Ok(repo_index_from_scans(files, include_tests))
}

pub fn update_repo_index(
    root: &Path,
    snapshot: &mut RepoIndexSnapshot,
    changed_paths: &[String],
    include_tests: bool,
) -> Result<RepoIndexUpdateSummary, CovyError> {
    let mut indexed_files = 0usize;
    let mut removed_files = 0usize;
    let mut changed = BTreeSet::new();
    snapshot.include_tests = include_tests;
    for raw_path in changed_paths {
        let relative_path = normalize_path(raw_path);
        if relative_path.is_empty() {
            continue;
        }
        changed.insert(relative_path.clone());
        let full_path = root.join(&relative_path);
        let should_remove = !full_path.exists()
            || !is_source_file(&full_path)
            || is_generated_or_vendor_path(&relative_path)
            || (!include_tests && is_test_path(&relative_path));
        if should_remove {
            if snapshot.files.remove(&relative_path).is_some() {
                removed_files += 1;
            }
            continue;
        }
        let metadata = std::fs::metadata(&full_path).map_err(|source| {
            CovyError::Other(format!(
                "failed to read metadata for '{}': {source}",
                full_path.display()
            ))
        })?;
        let size = metadata.len();
        let mtime_secs = metadata_mtime_secs(&metadata);
        let content = std::fs::read_to_string(&full_path).map_err(|source| {
            CovyError::Other(format!(
                "failed to read '{}': {source}",
                full_path.display()
            ))
        })?;
        let (symbols, imports, token_lines) = extract_index_metadata(&relative_path, &content);
        snapshot.files.insert(
            relative_path.clone(),
            RepoIndexFileEntry {
                path: relative_path,
                size,
                mtime_secs,
                is_test: is_test_path(raw_path),
                symbols,
                imports,
                token_lines,
            },
        );
        indexed_files += 1;
    }
    Ok(RepoIndexUpdateSummary {
        indexed_files,
        removed_files,
        changed_paths: changed.into_iter().collect(),
    })
}

fn repo_index_from_scans(files: Vec<FileScan>, include_tests: bool) -> RepoIndexSnapshot {
    let mut entries = BTreeMap::new();
    for file in files {
        let path = file.path.clone();
        entries.insert(
            path.clone(),
            RepoIndexFileEntry {
                path: path.clone(),
                size: file.size,
                mtime_secs: file.mtime_secs,
                is_test: is_test_path(&path),
                symbols: file.symbol_defs,
                imports: file.imports,
                token_lines: file.token_lines,
            },
        );
    }
    RepoIndexSnapshot {
        version: MAP_CACHE_VERSION,
        include_tests,
        files: entries,
    }
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
            "symbol" => envelope.symbols.get(hit.ref_idx).map(|symbol| FocusHitRich {
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

pub(crate) fn file_focus_match(
    path: &str,
    symbols: &[(String, String)],
    focus_paths: &[String],
    focus_symbols: &BTreeSet<String>,
) -> f64 {
    let normalized_path = normalize_path(path).to_ascii_lowercase();
    let explicit_path_match = focus_paths.iter().any(|p| {
        let candidate = normalize_path(p).to_ascii_lowercase();
        normalized_path == candidate
            || normalized_path.starts_with(&candidate)
            || candidate.starts_with(&normalized_path)
    });
    let path_match = if explicit_path_match {
        1.0
    } else {
        focus_symbols
            .iter()
            .map(|candidate| {
                if normalized_path.contains(candidate) {
                    0.3
                } else {
                    0.0
                }
            })
            .fold(0.0, f64::max)
    };

    let symbol_match = symbols
        .iter()
        .flat_map(|(_, name)| {
            focus_symbols
                .iter()
                .map(move |candidate| focus_term_match_score(name, candidate))
        })
        .fold(0.0, f64::max);

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

pub(crate) fn focus_term_match_score(candidate: &str, focus_term: &str) -> f64 {
    let candidate = candidate.trim().to_ascii_lowercase();
    let focus_term = focus_term.trim().to_ascii_lowercase();
    if candidate.is_empty() || focus_term.is_empty() {
        return 0.0;
    }
    if candidate == focus_term {
        return 1.0;
    }
    if candidate.contains(&focus_term) || focus_term.contains(&candidate) {
        return 0.6;
    }
    0.0
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

pub(crate) fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
