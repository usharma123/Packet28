use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

pub(crate) fn scan_repo(root: &Path, include_tests: bool) -> Result<Vec<FileScan>, CovyError> {
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
                    size,
                    symbols: entry.symbols.clone(),
                    symbol_defs: entry.symbol_defs.clone(),
                    imports: entry.imports.clone(),
                    token_lines: entry.token_lines.clone(),
                    mtime_secs,
                });
                continue;
            }
        }

        let content = match std::fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let (symbol_defs, imports, token_lines) = extract_index_metadata(&rel, &content);
        let symbols = symbol_defs
            .iter()
            .map(|symbol| (symbol.kind.clone(), symbol.name.clone()))
            .collect::<Vec<_>>();
        cache.files.insert(
            rel.clone(),
            CacheEntry {
                size,
                mtime_secs,
                symbols: symbols.clone(),
                symbol_defs: symbol_defs.clone(),
                imports: imports.clone(),
                token_lines: token_lines.clone(),
            },
        );
        cache_dirty = true;

        out.push(FileScan {
            path: rel,
            size,
            symbols,
            symbol_defs,
            imports,
            token_lines,
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

pub(crate) fn scan_cache_path(root: &Path) -> PathBuf {
    root.join(MAP_CACHE_DIR).join(MAP_CACHE_FILE)
}

pub(crate) fn load_scan_cache(root: &Path) -> RepoScanCache {
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
        cache
    } else {
        return empty_cache();
    };

    if cache.version != MAP_CACHE_VERSION {
        return empty_cache();
    }

    cache
}

pub(crate) fn write_scan_cache(root: &Path, cache: &RepoScanCache) {
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

pub(crate) fn empty_cache() -> RepoScanCache {
    RepoScanCache {
        version: MAP_CACHE_VERSION,
        files: BTreeMap::new(),
    }
}

pub(crate) fn metadata_mtime_secs(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn is_source_file(path: &Path) -> bool {
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

pub(crate) fn is_generated_or_vendor_path(path: &str) -> bool {
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

pub(crate) fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.py")
        || lower.ends_with("test.rs")
}

pub(crate) fn extract_imports(content: &str) -> Vec<String> {
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

pub(crate) fn extract_index_metadata(
    path: &str,
    content: &str,
) -> (
    Vec<IndexedSymbolDef>,
    Vec<String>,
    BTreeMap<String, Vec<usize>>,
) {
    let (symbol_defs, imports) = if let Some(language) = detect_source_language(path) {
        extract_metadata_ast_with_lines(language, content)
            .unwrap_or_else(|| extract_metadata_regex_with_lines(path, content))
    } else {
        extract_metadata_regex_with_lines(path, content)
    };
    let token_lines = extract_token_lines(content, &symbol_defs);
    (symbol_defs, imports, token_lines)
}

pub(crate) fn extract_metadata_regex_with_lines(
    _path: &str,
    content: &str,
) -> (Vec<IndexedSymbolDef>, Vec<String>) {
    let mut out = BTreeSet::<IndexedSymbolDef>::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        for cap in symbol_re().captures_iter(line) {
            let kind = cap.name("kind").map(|m| m.as_str()).unwrap_or("");
            let name = cap.name("name").map(|m| m.as_str()).unwrap_or("");
            if !name.is_empty() {
                out.insert(IndexedSymbolDef {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    line: line_no,
                });
            }
        }
        for cap in java_type_re().captures_iter(line) {
            let kind = cap
                .name("kind")
                .map(|m| m.as_str())
                .unwrap_or("class")
                .to_ascii_lowercase();
            let name = cap.name("name").map(|m| m.as_str()).unwrap_or("");
            if !name.is_empty() {
                out.insert(IndexedSymbolDef {
                    kind,
                    name: name.to_string(),
                    line: line_no,
                });
            }
        }
        for cap in java_method_re().captures_iter(line) {
            let name = cap.name("name").map(|m| m.as_str()).unwrap_or("").trim();
            if !name.is_empty() && !is_reserved_word(name) {
                out.insert(IndexedSymbolDef {
                    kind: "method".to_string(),
                    name: name.to_string(),
                    line: line_no,
                });
            }
        }
    }
    (out.into_iter().collect(), extract_imports(content))
}

pub(crate) fn extract_token_lines(
    content: &str,
    symbol_defs: &[IndexedSymbolDef],
) -> BTreeMap<String, Vec<usize>> {
    let mut lines_by_token = BTreeMap::<String, Vec<usize>>::new();
    let symbol_tokens = symbol_defs
        .iter()
        .map(|symbol| symbol.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        for cap in identifier_re().captures_iter(line) {
            let token = cap
                .name("token")
                .map(|m| m.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if token.len() < 3 || is_reserved_word(&token) {
                continue;
            }
            if !symbol_tokens.contains(&token) && token.len() < 4 {
                continue;
            }
            let entry = lines_by_token.entry(token).or_default();
            if entry.last().copied() == Some(line_no) || entry.len() >= 8 {
                continue;
            }
            entry.push(line_no);
        }
    }
    for symbol in symbol_defs {
        let key = symbol.name.to_ascii_lowercase();
        let entry = lines_by_token.entry(key).or_default();
        if !entry.contains(&symbol.line) {
            entry.insert(0, symbol.line);
            entry.truncate(8);
        }
    }
    lines_by_token
}
