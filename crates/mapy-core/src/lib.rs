use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use suite_packet_core::{BudgetCost, CovyError, EnvelopeV1, FileRef, Provenance, SymbolRef};

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
    pub path: String,
    pub score: f64,
    pub symbol_count: usize,
    pub import_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedSymbol {
    pub name: String,
    pub file: String,
    pub kind: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FocusHit {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TruncationSummary {
    pub files_dropped: usize,
    pub symbols_dropped: usize,
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

#[derive(Debug, Clone, Default)]
struct FileScan {
    path: String,
    symbols: Vec<(String, String)>, // (kind, name)
    imports: Vec<String>,
    mtime_secs: u64,
}

pub fn build_repo_map(req: RepoMapRequest) -> Result<EnvelopeV1<RepoMapPayload>, CovyError> {
    let root = PathBuf::from(&req.repo_root);
    if !root.exists() {
        return Err(CovyError::Other(format!(
            "repo_root does not exist: {}",
            req.repo_root
        )));
    }

    let max_files = if req.max_files == 0 { 80 } else { req.max_files };
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
    let mut edges = Vec::<RepoEdge>::new();

    // Build file basename index to resolve import -> file.
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
                    edges.push(RepoEdge {
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

    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    edges.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);

    let max_degree = by_file
        .keys()
        .map(|path| indegree.get(path).copied().unwrap_or(0) + outdegree.get(path).copied().unwrap_or(0))
        .max()
        .unwrap_or(1)
        .max(1);

    let newest = by_file.values().map(|f| f.mtime_secs).max().unwrap_or(0);
    let oldest = by_file.values().map(|f| f.mtime_secs).min().unwrap_or(newest);
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

    let mut ranked_files = Vec::<RankedFile>::new();
    let mut ranked_symbols = Vec::<RankedSymbol>::new();

    for (path, scan) in &by_file {
        let focus_match = file_focus_match(path, &scan.symbols, &normalized_focus_paths, &focus_symbols);
        let change_proximity = proximity_score(path, &normalized_focus_paths);
        let degree = indegree.get(path).copied().unwrap_or(0) + outdegree.get(path).copied().unwrap_or(0);
        let dependency_centrality = degree as f64 / max_degree as f64;
        let recency_hint = (scan.mtime_secs.saturating_sub(oldest)) as f64 / recency_span as f64;

        let score = 0.45 * focus_match + 0.25 * change_proximity + 0.20 * dependency_centrality + 0.10 * recency_hint;

        ranked_files.push(RankedFile {
            path: path.clone(),
            score,
            symbol_count: scan.symbols.len(),
            import_count: scan.imports.len(),
        });

        for (kind, name) in &scan.symbols {
            let symbol_focus = if focus_symbols.contains(name) { 1.0 } else { 0.0 };
            let symbol_score = (0.7 * score + 0.3 * symbol_focus).min(1.0);
            ranked_symbols.push(RankedSymbol {
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

    ranked_files.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });

    ranked_symbols.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });

    let files_dropped = ranked_files.len().saturating_sub(max_files);
    let symbols_dropped = ranked_symbols.len().saturating_sub(max_symbols);

    ranked_files.truncate(max_files);
    ranked_symbols.truncate(max_symbols);

    let payload = RepoMapPayload {
        files_ranked: ranked_files.clone(),
        symbols_ranked: ranked_symbols.clone(),
        edges,
        focus_hits: focus_hits
            .into_iter()
            .map(|(kind, value)| FocusHit { kind, value })
            .collect(),
        truncation: TruncationSummary {
            files_dropped,
            symbols_dropped,
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
        files: payload
            .files_ranked
            .iter()
            .map(|f| FileRef {
                path: f.path.clone(),
                relevance: Some(f.score),
                source: Some("mapy.repo".to_string()),
            })
            .collect(),
        symbols: payload
            .symbols_ranked
            .iter()
            .map(|s| SymbolRef {
                name: s.name.clone(),
                file: Some(s.file.clone()),
                relevance: Some(s.score),
                source: Some("mapy.repo".to_string()),
            })
            .collect(),
        risk: None,
        confidence: Some(1.0),
        budget_cost: BudgetCost {
            est_tokens: (payload_bytes / 4) as u64,
            est_bytes: payload_bytes,
            runtime_ms: 0,
            tool_calls: 1,
        },
        provenance: Provenance {
            inputs: vec![normalize_path(&req.repo_root)],
            git_base: None,
            git_head: None,
            generated_at_unix: now_unix(),
        },
        payload,
    }
    .with_canonical_hash();

    Ok(envelope)
}

fn scan_repo(root: &Path, include_tests: bool) -> Result<Vec<FileScan>, CovyError> {
    let mut out = Vec::new();

    let mut builder = WalkBuilder::new(root);
    builder.hidden(false).parents(true).ignore(true).git_ignore(true);

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

        if !include_tests && is_test_path(&rel) {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let symbols = extract_symbols(&content);
        let imports = extract_imports(&content);
        let mtime_secs = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        out.push(FileScan {
            path: rel,
            symbols,
            imports,
            mtime_secs,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
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

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.py")
        || lower.ends_with("test.rs")
}

fn extract_symbols(content: &str) -> Vec<(String, String)> {
    let mut out = BTreeSet::<(String, String)>::new();

    for cap in symbol_re().captures_iter(content) {
        let kind = cap.name("kind").map(|m| m.as_str()).unwrap_or("");
        let name = cap.name("name").map(|m| m.as_str()).unwrap_or("");
        if !name.is_empty() {
            out.insert((kind.to_string(), name.to_string()));
        }
    }

    out.into_iter().collect()
}

fn extract_imports(content: &str) -> Vec<String> {
    let mut out = BTreeSet::<String>::new();

    for cap in import_re().captures_iter(content) {
        let target = cap
            .name("target")
            .map(|m| m.as_str())
            .unwrap_or("")
            .trim();
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

    let symbol_match: f64 = if symbols
        .iter()
        .any(|(_, name)| focus_symbols.contains(name))
    {
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

fn import_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:use|import|from|#include)\s+(?:<)?(?P<target>[A-Za-z0-9_./:-]+)",
        )
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
        assert!(env.payload.files_ranked[0].path <= env.payload.files_ranked[1].path);
    }
}
