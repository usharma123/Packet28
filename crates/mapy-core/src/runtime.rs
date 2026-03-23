use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use regex::Regex;
use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance, SymbolRef};

use crate::scan::{
    extract_index_metadata, is_generated_or_vendor_path, is_source_file, is_test_path,
    load_scan_cache, metadata_mtime_secs, scan_repo,
};
use crate::types::{
    FocusHit, FocusHitRich, IndexedSymbolDef, RankedFile, RankedFileRich, RankedSymbol,
    RankedSymbolRich, RepoEdge, RepoEdgeRich, RepoIndexFileEntry, RepoIndexSnapshot,
    RepoIndexUpdateSummary, RepoMapPayload, RepoMapPayloadRich, RepoMapRequest, RepoQueryMatch,
    RepoQueryMatchRich, RepoQueryPayload, RepoQueryPayloadRich, RepoQueryRequest,
    TruncationSummary,
};
use crate::{
    collect_syntax_candidates, detect_source_language, parse_source_language_name,
    resolve_import_leaf, SourceLanguage,
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

#[derive(Debug, Clone)]
struct QueryMatchTmp {
    path: String,
    name: String,
    kind: String,
    line: usize,
    score: f64,
}

pub(crate) const MAP_CACHE_VERSION: u32 = 4;
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

pub fn build_repo_query(req: RepoQueryRequest) -> Result<EnvelopeV1<RepoQueryPayload>, CovyError> {
    let root = PathBuf::from(&req.repo_root);
    if !root.exists() {
        return Err(CovyError::Other(format!(
            "repo_root does not exist: {}",
            req.repo_root
        )));
    }

    let has_symbol_query = !req.symbol_query.trim().is_empty();
    let has_pattern_query = !req.pattern_query.trim().is_empty();
    if has_symbol_query && has_pattern_query {
        return Err(CovyError::Other(
            "symbol_query and pattern_query are mutually exclusive".to_string(),
        ));
    }
    if has_pattern_query {
        return build_repo_pattern_query(root, req);
    }

    let started = Instant::now();
    let query = req.symbol_query.trim().to_string();
    if query.is_empty() {
        return Err(CovyError::Other("symbol_query cannot be empty".to_string()));
    }

    let snapshot = load_query_index(&root, req.include_tests)?;
    let max_results = req.max_results.max(1);
    let files_only = req.files_only;
    let normalized_query = query.to_ascii_lowercase();

    let mut matches = snapshot
        .files
        .values()
        .filter(|entry| req.include_tests || !entry.is_test)
        .flat_map(|entry| {
            entry.symbols.iter().filter_map(|symbol| {
                let score = query_match_score(
                    &symbol.name,
                    &normalized_query,
                    &query,
                    req.exact,
                    files_only,
                )?;
                Some(QueryMatchTmp {
                    path: entry.path.clone(),
                    name: symbol.name.clone(),
                    kind: symbol.kind.clone(),
                    line: symbol.line,
                    score,
                })
            })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.name.cmp(&right.name))
    });

    if files_only {
        let mut seen = BTreeSet::new();
        matches.retain(|candidate| seen.insert(candidate.path.clone()));
    }

    let matches_dropped = matches.len().saturating_sub(max_results);
    matches.truncate(max_results);

    let files = matches
        .iter()
        .map(|candidate| FileRef {
            path: candidate.path.clone(),
            relevance: Some(candidate.score),
            source: None,
        })
        .collect::<Vec<_>>();
    let symbols = matches
        .iter()
        .map(|candidate| SymbolRef {
            name: candidate.name.clone(),
            file: Some(candidate.path.clone()),
            kind: Some(candidate.kind.clone()),
            relevance: Some(candidate.score),
            source: None,
        })
        .collect::<Vec<_>>();

    let payload = RepoQueryPayload {
        query: query.clone(),
        matches: matches
            .iter()
            .enumerate()
            .map(|(idx, candidate)| RepoQueryMatch {
                file_idx: idx,
                symbol_idx: idx,
                line: candidate.line,
                score: candidate.score,
            })
            .collect(),
        truncation: TruncationSummary {
            files_dropped: matches_dropped,
            symbols_dropped: 0,
            edges_dropped: 0,
        },
    };

    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();

    Ok(EnvelopeV1 {
        version: "1".to_string(),
        tool: "mapy".to_string(),
        kind: "repo_query".to_string(),
        hash: String::new(),
        summary: format!(
            "repo_query matches={} query={}",
            payload.matches.len(),
            query
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
    .with_canonical_hash_and_real_budget())
}

fn build_repo_pattern_query(
    root: PathBuf,
    req: RepoQueryRequest,
) -> Result<EnvelopeV1<RepoQueryPayload>, CovyError> {
    let started = Instant::now();
    let pattern = req.pattern_query.trim().to_string();
    if pattern.is_empty() {
        return Err(CovyError::Other(
            "pattern_query cannot be empty".to_string(),
        ));
    }
    let language = parse_source_language_name(&req.language)
        .ok_or_else(|| CovyError::Other(format!("unsupported query language: {}", req.language)))?;
    let selectors = infer_pattern_selectors(language, &pattern, &req.selector);
    let regex = compile_pattern_regex(&pattern)?;
    let snapshot = load_query_index(&root, req.include_tests)?;
    let candidate_paths = shortlist_pattern_files(&snapshot, language, &pattern, req.include_tests);
    let max_results = req.max_results.max(1);

    let mut matches = Vec::new();
    for path in candidate_paths {
        let full_path = root.join(&path);
        let Ok(content) = std::fs::read_to_string(&full_path) else {
            continue;
        };
        for candidate in collect_syntax_candidates(language, &content, &selectors) {
            let normalized = normalize_syntax_text(&candidate.text);
            if !regex.is_match(&normalized) {
                continue;
            }
            matches.push(QueryMatchTmp {
                path: path.clone(),
                name: display_name_for_candidate(&candidate),
                kind: candidate.kind,
                line: candidate.line,
                score: pattern_match_score(&normalized, &pattern),
            });
        }
    }

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.name.cmp(&right.name))
    });

    if req.files_only {
        let mut seen = BTreeSet::new();
        matches.retain(|candidate| seen.insert(candidate.path.clone()));
    }

    let matches_dropped = matches.len().saturating_sub(max_results);
    matches.truncate(max_results);

    let files = matches
        .iter()
        .map(|candidate| FileRef {
            path: candidate.path.clone(),
            relevance: Some(candidate.score),
            source: None,
        })
        .collect::<Vec<_>>();
    let symbols = matches
        .iter()
        .map(|candidate| SymbolRef {
            name: candidate.name.clone(),
            file: Some(candidate.path.clone()),
            kind: Some(candidate.kind.clone()),
            relevance: Some(candidate.score),
            source: None,
        })
        .collect::<Vec<_>>();

    let payload = RepoQueryPayload {
        query: pattern.clone(),
        matches: matches
            .iter()
            .enumerate()
            .map(|(idx, candidate)| RepoQueryMatch {
                file_idx: idx,
                symbol_idx: idx,
                line: candidate.line,
                score: candidate.score,
            })
            .collect(),
        truncation: TruncationSummary {
            files_dropped: matches_dropped,
            symbols_dropped: 0,
            edges_dropped: 0,
        },
    };

    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default().len();
    Ok(EnvelopeV1 {
        version: "1".to_string(),
        tool: "mapy".to_string(),
        kind: "repo_query".to_string(),
        hash: String::new(),
        summary: format!(
            "repo_query matches={} query={}",
            payload.matches.len(),
            pattern
        ),
        files,
        symbols,
        risk: None,
        confidence: Some(0.95),
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
    .with_canonical_hash_and_real_budget())
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

    let module_index = build_module_index(by_file.keys());

    for (path, scan) in &by_file {
        for imp in &scan.imports {
            if let Some(target) = resolve_import_target(path, imp, &module_index) {
                if target != *path {
                    edges_tmp.push(RepoEdgeTmp {
                        from: path.clone(),
                        to: target.clone(),
                        kind: "import".to_string(),
                    });
                    *outdegree.entry(path.clone()).or_insert(0) += 1;
                    *indegree.entry(target).or_insert(0) += 1;
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

pub fn expand_repo_query_payload(envelope: &EnvelopeV1<RepoQueryPayload>) -> RepoQueryPayloadRich {
    RepoQueryPayloadRich {
        query: envelope.payload.query.clone(),
        matches: envelope
            .payload
            .matches
            .iter()
            .filter_map(|matched| {
                let file = envelope.files.get(matched.file_idx)?;
                let symbol = envelope.symbols.get(matched.symbol_idx)?;
                Some(RepoQueryMatchRich {
                    file: file.path.clone(),
                    symbol: symbol.name.clone(),
                    kind: symbol.kind.clone().unwrap_or_else(|| "symbol".to_string()),
                    line: matched.line,
                    score: matched.score,
                })
            })
            .collect(),
        truncation: envelope.payload.truncation.clone(),
    }
}

fn load_query_index(root: &Path, include_tests: bool) -> Result<RepoIndexSnapshot, CovyError> {
    if include_tests {
        return build_repo_index(root, true);
    }

    let cache = load_scan_cache(root);
    if cache.files.is_empty() {
        return build_repo_index(root, false);
    }

    Ok(RepoIndexSnapshot {
        version: cache.version,
        include_tests: false,
        files: cache
            .files
            .into_iter()
            .map(|(path, entry)| {
                let is_test = is_test_path(&path);
                (
                    path.clone(),
                    RepoIndexFileEntry {
                        path,
                        size: entry.size,
                        mtime_secs: entry.mtime_secs,
                        is_test,
                        symbols: entry.symbol_defs,
                        imports: entry.imports,
                        token_lines: entry.token_lines,
                    },
                )
            })
            .collect(),
    })
}

fn query_match_score(
    candidate: &str,
    normalized_query: &str,
    raw_query: &str,
    exact: bool,
    files_only: bool,
) -> Option<f64> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate == raw_query {
        return Some(1.0);
    }
    let lower = candidate.to_ascii_lowercase();
    if lower == normalized_query {
        return Some(0.98);
    }
    if exact {
        return None;
    }

    let base = focus_term_match_score(candidate, normalized_query);
    if base == 0.0 {
        return None;
    }
    Some(if files_only {
        base
    } else {
        (base - 0.05).max(0.1)
    })
}

fn infer_pattern_selectors(
    language: SourceLanguage,
    pattern: &str,
    explicit_selector: &str,
) -> Vec<String> {
    let explicit = explicit_selector
        .split(',')
        .map(str::trim)
        .filter(|selector| !selector.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !explicit.is_empty() {
        return explicit;
    }

    let tokens = extract_pattern_literals(pattern)
        .into_iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();

    let selectors = match language {
        SourceLanguage::Rust => {
            let mut selectors = Vec::new();
            if tokens.contains("fn") {
                selectors.push("function_item");
            }
            if tokens.contains("struct") {
                selectors.push("struct_item");
            }
            if tokens.contains("enum") {
                selectors.push("enum_item");
            }
            if tokens.contains("trait") {
                selectors.push("trait_item");
            }
            if tokens.contains("type") {
                selectors.push("type_item");
            }
            if tokens.contains("const") {
                selectors.push("const_item");
            }
            if tokens.contains("static") {
                selectors.push("static_item");
            }
            if tokens.contains("mod") {
                selectors.push("mod_item");
            }
            selectors
        }
        SourceLanguage::Python => {
            let mut selectors = Vec::new();
            if tokens.contains("def") || tokens.contains("async") {
                selectors.push("function_definition");
                selectors.push("async_function_definition");
            }
            if tokens.contains("class") {
                selectors.push("class_definition");
            }
            selectors
        }
        SourceLanguage::TypeScript | SourceLanguage::TypeScriptJsx => {
            let mut selectors = Vec::new();
            if tokens.contains("function") {
                selectors.push("function_declaration");
            }
            if tokens.contains("class") {
                selectors.push("class_declaration");
            }
            if tokens.contains("interface") {
                selectors.push("interface_declaration");
            }
            if tokens.contains("type") {
                selectors.push("type_alias_declaration");
            }
            if tokens.contains("enum") {
                selectors.push("enum_declaration");
            }
            if tokens.contains("const") || tokens.contains("let") || tokens.contains("var") {
                selectors.push("lexical_declaration");
                selectors.push("variable_declarator");
            }
            selectors
        }
        SourceLanguage::JavaScript => {
            let mut selectors = Vec::new();
            if tokens.contains("function") {
                selectors.push("function_declaration");
            }
            if tokens.contains("class") {
                selectors.push("class_declaration");
            }
            if tokens.contains("const") || tokens.contains("let") || tokens.contains("var") {
                selectors.push("lexical_declaration");
                selectors.push("variable_declarator");
            }
            selectors
        }
        SourceLanguage::Java => {
            let mut selectors = Vec::new();
            if tokens.contains("class") {
                selectors.push("class_declaration");
            }
            if tokens.contains("interface") {
                selectors.push("interface_declaration");
            }
            if tokens.contains("enum") {
                selectors.push("enum_declaration");
            }
            if tokens.contains("@interface") || tokens.contains("annotation") {
                selectors.push("annotation_type_declaration");
            }
            if tokens.contains("void")
                || tokens.contains("public")
                || tokens.contains("private")
                || tokens.contains("protected")
                || pattern.contains('(')
            {
                selectors.push("method_declaration");
            }
            selectors
        }
        SourceLanguage::Go => {
            let mut selectors = Vec::new();
            if tokens.contains("func") {
                selectors.push("function_declaration");
                selectors.push("method_declaration");
            }
            if tokens.contains("type") {
                selectors.push("type_declaration");
            }
            if tokens.contains("var") {
                selectors.push("var_declaration");
            }
            if tokens.contains("const") {
                selectors.push("const_declaration");
            }
            selectors
        }
        SourceLanguage::Cpp => {
            let mut selectors = Vec::new();
            if pattern.contains('(') {
                selectors.push("function_definition");
                selectors.push("declaration");
            }
            if tokens.contains("class") {
                selectors.push("class_specifier");
            }
            if tokens.contains("struct") {
                selectors.push("struct_specifier");
            }
            if tokens.contains("enum") {
                selectors.push("enum_specifier");
            }
            selectors
        }
    };

    selectors.into_iter().map(str::to_string).collect()
}

fn compile_pattern_regex(pattern: &str) -> Result<Regex, CovyError> {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut regex = String::new();
    let mut idx = 0usize;
    let mut pending_whitespace = false;

    while idx < chars.len() {
        let ch = chars[idx];
        if ch.is_whitespace() {
            pending_whitespace = true;
            idx += 1;
            continue;
        }
        if pending_whitespace {
            regex.push_str(r"\s+");
            pending_whitespace = false;
        }
        if ch == '$' {
            if chars.get(idx + 1) == Some(&'$') && chars.get(idx + 2) == Some(&'$') {
                regex.push_str(".*?");
                idx += 3;
                while let Some(next) = chars.get(idx) {
                    if next.is_ascii_alphanumeric() || *next == '_' {
                        idx += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            if chars.get(idx + 1) == Some(&'_') {
                regex.push_str(".+?");
                idx += 2;
                continue;
            }
            let mut end = idx + 1;
            while let Some(next) = chars.get(end) {
                if next.is_ascii_alphanumeric() || *next == '_' {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > idx + 1 {
                regex.push_str(r"[A-Za-z_][A-Za-z0-9_]*");
                idx = end;
                continue;
            }
        }
        regex.push_str(&regex::escape(&ch.to_string()));
        idx += 1;
    }
    if pending_whitespace {
        regex.push_str(r"\s+");
    }

    Regex::new(&regex)
        .map_err(|source| CovyError::Other(format!("invalid pattern query `{pattern}`: {source}")))
}

fn shortlist_pattern_files(
    snapshot: &RepoIndexSnapshot,
    language: SourceLanguage,
    pattern: &str,
    include_tests: bool,
) -> Vec<String> {
    let literals = extract_pattern_literals(pattern)
        .into_iter()
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| !is_pattern_keyword(token))
        .collect::<Vec<_>>();

    let mut scored = snapshot
        .files
        .values()
        .filter(|entry| include_tests || !entry.is_test)
        .filter(|entry| detect_source_language(&entry.path) == Some(language))
        .map(|entry| {
            let symbol_hits = entry
                .symbols
                .iter()
                .map(|symbol| symbol.name.to_ascii_lowercase())
                .collect::<BTreeSet<_>>();
            let score = literals.iter().fold(0usize, |acc, token| {
                let symbol_boost = usize::from(symbol_hits.contains(token)) * 4;
                let token_boost = entry
                    .token_lines
                    .contains_key(token)
                    .then_some(2usize)
                    .unwrap_or(0);
                acc + symbol_boost + token_boost
            });
            (entry.path.clone(), score)
        })
        .collect::<Vec<_>>();

    let any_positive = scored.iter().any(|(_, score)| *score > 0);
    if any_positive {
        scored.retain(|(_, score)| *score > 0);
    }

    scored.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    if any_positive && !literals.is_empty() {
        if let Some(max_score) = scored.first().map(|(_, score)| *score) {
            scored.retain(|(_, score)| *score + 2 >= max_score);
        }
        if scored.len() > 64 {
            scored.truncate(64);
        }
    }
    scored.into_iter().map(|(path, _)| path).collect()
}

fn normalize_syntax_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn display_name_for_candidate(candidate: &crate::SyntaxCandidate) -> String {
    static IDENTIFIER_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = IDENTIFIER_RE
        .get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("identifier regex is valid"));

    re.find_iter(&candidate.text)
        .map(|matched| matched.as_str())
        .find(|token| !is_pattern_keyword(token))
        .unwrap_or(&candidate.kind)
        .to_string()
}

fn pattern_match_score(normalized_candidate: &str, pattern: &str) -> f64 {
    let normalized_pattern = normalize_syntax_text(pattern);
    if normalized_candidate == normalized_pattern {
        return 1.0;
    }

    let literals = extract_pattern_literals(pattern)
        .into_iter()
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| !is_pattern_keyword(token))
        .collect::<Vec<_>>();
    if literals.is_empty() {
        return 0.6;
    }

    let normalized_lower = normalized_candidate.to_ascii_lowercase();
    let literal_hits = literals
        .iter()
        .filter(|token| normalized_lower.contains(token.as_str()))
        .count();
    let coverage = literal_hits as f64 / literals.len() as f64;
    let prefix_bonus = if normalized_candidate.starts_with(&normalized_pattern) {
        0.15
    } else {
        0.0
    };
    (0.55 + (coverage * 0.3) + prefix_bonus).min(0.99)
}

fn extract_pattern_literals(pattern: &str) -> Vec<String> {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut idx = 0usize;

    while idx < chars.len() {
        let ch = chars[idx];
        if ch == '$' {
            idx += 1;
            while chars.get(idx) == Some(&'$') {
                idx += 1;
            }
            while let Some(next) = chars.get(idx) {
                if next.is_ascii_alphanumeric() || *next == '_' {
                    idx += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = idx;
            idx += 1;
            while let Some(next) = chars.get(idx) {
                if next.is_ascii_alphanumeric() || *next == '_' {
                    idx += 1;
                } else {
                    break;
                }
            }
            out.push(chars[start..idx].iter().collect());
            continue;
        }
        idx += 1;
    }

    out
}

fn is_pattern_keyword(token: &str) -> bool {
    matches!(
        token,
        "_" | "fn"
            | "pub"
            | "async"
            | "unsafe"
            | "extern"
            | "impl"
            | "struct"
            | "enum"
            | "trait"
            | "type"
            | "const"
            | "static"
            | "mod"
            | "class"
            | "interface"
            | "function"
            | "def"
            | "void"
            | "public"
            | "private"
            | "protected"
            | "let"
            | "var"
            | "func"
            | "annotation"
    )
}

fn build_module_index<'a>(
    paths: impl Iterator<Item = &'a String>,
) -> BTreeMap<String, Vec<String>> {
    let mut index = BTreeMap::<String, Vec<String>>::new();
    for path in paths {
        for key in module_keys_for_file(path) {
            index.entry(key).or_default().push(path.clone());
        }
    }
    for candidates in index.values_mut() {
        candidates.sort();
        candidates.dedup();
    }
    index
}

fn resolve_import_target(
    importer_path: &str,
    import_ref: &str,
    module_index: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let mut matches = BTreeSet::new();
    for key in import_reference_keys(importer_path, import_ref) {
        if let Some(candidates) = module_index.get(&key) {
            for candidate in candidates {
                matches.insert(candidate.clone());
            }
        }
    }

    choose_best_import_target(importer_path, matches.into_iter().collect())
}

fn choose_best_import_target(importer_path: &str, candidates: Vec<String>) -> Option<String> {
    let importer_lang = detect_source_language(importer_path);
    candidates.into_iter().max_by(|left, right| {
        resolution_score(importer_path, importer_lang, left).cmp(&resolution_score(
            importer_path,
            importer_lang,
            right,
        ))
    })
}

fn resolution_score(
    importer_path: &str,
    importer_lang: Option<SourceLanguage>,
    candidate: &str,
) -> (usize, usize, usize, std::cmp::Reverse<String>) {
    let importer_dir = Path::new(importer_path)
        .parent()
        .map(|path| normalize_path(&path.to_string_lossy()))
        .unwrap_or_default();
    let candidate_dir = Path::new(candidate)
        .parent()
        .map(|path| normalize_path(&path.to_string_lossy()))
        .unwrap_or_default();
    let same_language = usize::from(detect_source_language(candidate) == importer_lang);
    let shared_dir = common_prefix_segments(&importer_dir, &candidate_dir);
    let shared_path = common_prefix_segments(importer_path, candidate);
    (
        same_language,
        shared_dir,
        shared_path,
        std::cmp::Reverse(candidate.to_string()),
    )
}

fn module_keys_for_file(path: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let normalized = normalize_path(path);
    if let Some(extless) = strip_source_extension(&normalized) {
        insert_module_key(&mut keys, &extless);

        if let Some(base) = Path::new(&extless)
            .file_name()
            .and_then(|name| name.to_str())
        {
            insert_module_key(&mut keys, base);
        }

        if extless.ends_with("/index") {
            if let Some(parent) = Path::new(&extless).parent() {
                let parent = normalize_path(&parent.to_string_lossy());
                insert_module_key(&mut keys, &parent);
            }
        }

        if extless.ends_with("/mod") {
            if let Some(parent) = Path::new(&extless).parent() {
                let parent = normalize_path(&parent.to_string_lossy());
                insert_module_key(&mut keys, &parent);
            }
        }
    }

    if let Some((crate_name, rust_module)) = rust_module_identity(&normalized) {
        if rust_module.is_empty() {
            insert_module_key(&mut keys, "crate");
            insert_module_key(&mut keys, &crate_name);
        } else {
            insert_module_key(&mut keys, &rust_module);
            insert_module_key(&mut keys, &format!("crate::{rust_module}"));
            insert_module_key(&mut keys, &format!("{crate_name}::{rust_module}"));
        }
    }

    if let Some(java_key) = java_module_key(&normalized) {
        insert_module_key(&mut keys, &java_key);
    }

    if let Some(python_key) = python_module_key(&normalized) {
        insert_module_key(&mut keys, &python_key);
    }

    keys
}

fn import_reference_keys(importer_path: &str, import_ref: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let normalized = normalize_path(import_ref);
    if normalized.is_empty() {
        return keys;
    }

    insert_module_key(&mut keys, &normalized);
    if let Some(leaf) = resolve_import_leaf(&normalized) {
        insert_module_key(&mut keys, &leaf);
    }

    if normalized.contains('.') && !normalized.contains('/') && !normalized.contains("::") {
        insert_module_key(&mut keys, &normalized.replace('.', "/"));
    }
    if normalized.contains("::") {
        insert_module_key(&mut keys, &normalized.replace("::", "/"));
    }

    if let Some(resolved) = resolve_relative_import_path(importer_path, &normalized) {
        insert_module_key(&mut keys, &resolved);
    }

    if let Some((crate_name, importer_module)) = rust_module_identity(importer_path) {
        for key in rust_import_reference_keys(&crate_name, &importer_module, &normalized) {
            insert_module_key(&mut keys, &key);
        }
    }

    keys
}

fn rust_import_reference_keys(
    crate_name: &str,
    importer_module: &str,
    raw: &str,
) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return keys;
    }

    if let Some(stripped) = trimmed.strip_prefix("crate::") {
        insert_module_key(&mut keys, stripped);
        insert_module_key(&mut keys, trimmed);
        insert_module_key(&mut keys, &format!("{crate_name}::{stripped}"));
        if let Some(parent) = stripped.rsplit_once("::").map(|(prefix, _)| prefix) {
            insert_module_key(&mut keys, parent);
            insert_module_key(&mut keys, &format!("crate::{parent}"));
            insert_module_key(&mut keys, &format!("{crate_name}::{parent}"));
        }
        return keys;
    }

    if let Some(stripped) = trimmed.strip_prefix("self::") {
        if let Some(resolved) = join_rust_modules(importer_module, stripped) {
            insert_module_key(&mut keys, &resolved);
            insert_module_key(&mut keys, &format!("crate::{resolved}"));
            insert_module_key(&mut keys, &format!("{crate_name}::{resolved}"));
        }
        return keys;
    }

    if trimmed.starts_with("super::") {
        let mut remainder = trimmed;
        let mut parent = importer_module.to_string();
        while let Some(stripped) = remainder.strip_prefix("super::") {
            remainder = stripped;
            parent = parent
                .rsplit_once("::")
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or_default();
        }
        if let Some(resolved) = join_rust_modules(&parent, remainder) {
            insert_module_key(&mut keys, &resolved);
            insert_module_key(&mut keys, &format!("crate::{resolved}"));
            insert_module_key(&mut keys, &format!("{crate_name}::{resolved}"));
            if let Some(parent) = resolved.rsplit_once("::").map(|(prefix, _)| prefix) {
                insert_module_key(&mut keys, parent);
                insert_module_key(&mut keys, &format!("crate::{parent}"));
                insert_module_key(&mut keys, &format!("{crate_name}::{parent}"));
            }
        }
        return keys;
    }

    insert_module_key(&mut keys, trimmed);
    insert_module_key(&mut keys, &format!("crate::{trimmed}"));
    insert_module_key(&mut keys, &format!("{crate_name}::{trimmed}"));
    if let Some(parent) = trimmed.rsplit_once("::").map(|(prefix, _)| prefix) {
        insert_module_key(&mut keys, parent);
        insert_module_key(&mut keys, &format!("crate::{parent}"));
        insert_module_key(&mut keys, &format!("{crate_name}::{parent}"));
    }
    keys
}

fn join_rust_modules(prefix: &str, suffix: &str) -> Option<String> {
    let suffix = suffix.trim_matches(':').trim();
    if suffix.is_empty() {
        if prefix.is_empty() {
            None
        } else {
            Some(prefix.to_string())
        }
    } else if prefix.is_empty() {
        Some(suffix.to_string())
    } else {
        Some(format!("{prefix}::{suffix}"))
    }
}

fn rust_module_identity(path: &str) -> Option<(String, String)> {
    let normalized = normalize_path(path);
    if detect_source_language(&normalized) != Some(SourceLanguage::Rust) {
        return None;
    }

    let (crate_root, relative) = split_source_root(&normalized)?;
    let crate_name = Path::new(crate_root)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("crate")
        .replace('-', "_");
    let extless = strip_source_extension(relative)?;
    let module = if extless == "lib" || extless == "main" {
        String::new()
    } else if let Some(prefix) = extless.strip_suffix("/mod") {
        prefix.replace('/', "::")
    } else {
        extless.replace('/', "::")
    };
    Some((crate_name, module))
}

fn java_module_key(path: &str) -> Option<String> {
    let normalized = normalize_path(path);
    if detect_source_language(&normalized) != Some(SourceLanguage::Java) {
        return None;
    }

    let suffix = split_java_source_root(&normalized)?;
    let extless = strip_source_extension(suffix)?;
    Some(extless.replace('/', "."))
}

fn python_module_key(path: &str) -> Option<String> {
    let normalized = normalize_path(path);
    if detect_source_language(&normalized) != Some(SourceLanguage::Python) {
        return None;
    }
    let extless = strip_source_extension(&normalized)?;
    if let Some(prefix) = extless.strip_suffix("/__init__") {
        if prefix.is_empty() {
            return None;
        }
        return Some(prefix.replace('/', "."));
    }
    Some(extless.replace('/', "."))
}

fn resolve_relative_import_path(importer_path: &str, raw: &str) -> Option<String> {
    if !raw.starts_with("./") && !raw.starts_with("../") {
        return None;
    }
    let base = Path::new(importer_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let resolved = normalize_path(&base.join(raw).to_string_lossy());
    if let Some(extless) = strip_source_extension(&resolved) {
        return Some(extless);
    }
    Some(resolved)
}

fn strip_source_extension(path: &str) -> Option<String> {
    let path = normalize_path(path);
    let source_language = detect_source_language(&path)?;
    let extension = match source_language {
        SourceLanguage::Java => "java",
        SourceLanguage::Rust => "rs",
        SourceLanguage::Python => "py",
        SourceLanguage::TypeScript => "ts",
        SourceLanguage::TypeScriptJsx => "tsx",
        SourceLanguage::JavaScript => {
            if path.ends_with(".jsx") {
                "jsx"
            } else {
                "js"
            }
        }
        SourceLanguage::Go => "go",
        SourceLanguage::Cpp => Path::new(&path).extension().and_then(|ext| ext.to_str())?,
    };
    path.strip_suffix(&format!(".{extension}"))
        .map(|value| value.to_string())
}

fn insert_module_key(out: &mut BTreeSet<String>, key: &str) {
    let normalized = normalize_path(key).trim_matches('/').trim().to_string();
    if normalized.is_empty() {
        return;
    }
    out.insert(normalized.clone());
    if normalized.contains('/') {
        out.insert(normalized.replace('/', "."));
        out.insert(normalized.replace('/', "::"));
    }
}

fn split_source_root(path: &str) -> Option<(&str, &str)> {
    if let Some(relative) = path.strip_prefix("src/") {
        return Some(("", relative));
    }
    let marker = "/src/";
    let split = path.find(marker)?;
    Some((&path[..split], path.get(split + marker.len()..)?))
}

fn split_java_source_root(path: &str) -> Option<&str> {
    for marker in ["src/main/java/", "src/test/java/"] {
        if let Some(relative) = path.strip_prefix(marker) {
            return Some(relative);
        }
    }
    for marker in ["/src/main/java/", "/src/test/java/", "/java/"] {
        if let Some(idx) = path.find(marker) {
            return path.get(idx + marker.len()..);
        }
    }
    if let Some(relative) = path.strip_prefix("java/") {
        return Some(relative);
    }
    None
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
        path_focus_match_score(&normalized_path, focus_symbols)
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

fn path_focus_match_score(normalized_path: &str, focus_symbols: &BTreeSet<String>) -> f64 {
    let path_tokens = normalized_path
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let mut term_scores = focus_symbols
        .iter()
        .filter_map(|candidate| {
            let token_score = path_tokens
                .iter()
                .map(|token| focus_term_match_score(token, candidate))
                .fold(0.0, f64::max);
            let match_score = if normalized_path.contains(candidate) {
                1.0
            } else {
                token_score
            };
            (match_score > 0.0).then_some(match_score)
        })
        .collect::<Vec<_>>();
    term_scores.sort_by(|left, right| right.total_cmp(left));

    match term_scores.as_slice() {
        [] => 0.0,
        [first] => 0.3 * *first,
        [first, rest @ ..] => {
            let bonus = rest.iter().take(2).sum::<f64>() * 0.15;
            (0.3 * *first + bonus).min(0.6)
        }
    }
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
    if candidate.len() >= 6 && focus_term.len() >= 6 && shared_prefix_len(&candidate, &focus_term) >= 5
    {
        return 0.45;
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

fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(left, right)| left == right).count()
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
