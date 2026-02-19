use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::diagnostics::DiagnosticsData;
use crate::model::{CoverageData, RepoSnapshot};

/// Strategies for mapping coverage file paths to repository file paths.
pub struct PathMapper {
    strip_prefixes: Vec<String>,
    rules: BTreeMap<String, String>,
    /// Reverse index: filename → list of full paths in the repo.
    suffix_index: HashMap<String, Vec<String>>,
    /// Content hash index for fallback matching.
    hash_index: HashMap<String, Vec<String>>,
    /// LRU cache of resolved mappings.
    cache: HashMap<String, Option<String>>,
}

impl PathMapper {
    pub fn new(
        strip_prefixes: Vec<String>,
        rules: BTreeMap<String, String>,
        snapshot: Option<&RepoSnapshot>,
    ) -> Self {
        let mut suffix_index = HashMap::new();
        let mut hash_index = HashMap::new();

        if let Some(snap) = snapshot {
            for (path, hash) in &snap.file_hashes {
                // Build suffix index by filename
                if let Some(filename) = path.rsplit('/').next() {
                    suffix_index
                        .entry(filename.to_string())
                        .or_insert_with(Vec::new)
                        .push(path.clone());
                }
                // Build hash index
                hash_index
                    .entry(hash.clone())
                    .or_insert_with(Vec::new)
                    .push(path.clone());
            }
        }

        Self {
            strip_prefixes,
            rules,
            suffix_index,
            hash_index,
            cache: HashMap::new(),
        }
    }

    /// Resolve a coverage file path to a repository file path.
    /// Strategy chain: exact match → rule substitution → strip prefix → suffix match.
    pub fn resolve(&mut self, coverage_path: &str, known_paths: &[&str]) -> Option<String> {
        if let Some(cached) = self.cache.get(coverage_path) {
            return cached.clone();
        }

        let result = self.resolve_inner(coverage_path, known_paths);
        self.cache.insert(coverage_path.to_string(), result.clone());
        result
    }

    fn resolve_inner(&self, coverage_path: &str, known_paths: &[&str]) -> Option<String> {
        // 1. Exact match
        if known_paths.contains(&coverage_path) {
            return Some(coverage_path.to_string());
        }

        // 2. Rule substitution
        for (from, to) in &self.rules {
            if coverage_path.starts_with(from.as_str()) {
                let candidate = format!("{}{}", to, &coverage_path[from.len()..]);
                if known_paths.contains(&candidate.as_str()) {
                    return Some(candidate);
                }
            }
        }

        // 3. Strip prefix
        for prefix in &self.strip_prefixes {
            let stripped = coverage_path
                .strip_prefix(prefix.as_str())
                .unwrap_or(coverage_path);
            if stripped != coverage_path && known_paths.contains(&stripped) {
                return Some(stripped.to_string());
            }
        }

        // 4. Suffix match (by filename)
        let filename = coverage_path.rsplit('/').next().unwrap_or(coverage_path);
        if let Some(candidates) = self.suffix_index.get(filename) {
            if candidates.len() == 1 {
                return Some(candidates[0].clone());
            }
            // If multiple, try finding the best suffix match
            let normalized = normalize_path(coverage_path);
            let mut best: Option<&str> = None;
            let mut best_score = 0;
            for candidate in candidates {
                let score = common_suffix_len(&normalized, candidate);
                if score > best_score {
                    best_score = score;
                    best = Some(candidate);
                }
            }
            return best.map(|s| s.to_string());
        }

        None
    }

    /// Resolve using content hash as fallback.
    pub fn resolve_by_hash(&self, content_hash: &str) -> Option<String> {
        self.hash_index.get(content_hash).and_then(|paths| {
            if paths.len() == 1 {
                Some(paths[0].clone())
            } else {
                None
            }
        })
    }
}

/// Automatically normalize paths in coverage data to be relative.
///
/// Strategy:
/// 1. If `source_root` is provided, strip it from all paths.
/// 2. Otherwise, detect common absolute prefix and strip it.
/// 3. As fallback, try `git rev-parse --show-toplevel`.
/// 4. Normalize backslashes and strip leading `./`.
pub fn auto_normalize_paths(data: &mut CoverageData, source_root: Option<&Path>) {
    let root = source_root
        .map(|p| p.to_string_lossy().to_string())
        .or_else(|| detect_common_prefix(data.files.keys().map(|k| k.as_str())))
        .or_else(git_toplevel);

    let old_files = std::mem::take(&mut data.files);
    let mut new_files = BTreeMap::new();
    for (path, fc) in old_files {
        let mut p: String = path.replace('\\', "/");

        if let Some(ref root) = root {
            let root_normalized = root.replace('\\', "/");
            let root_with_slash = if root_normalized.ends_with('/') {
                root_normalized.clone()
            } else {
                format!("{root_normalized}/")
            };
            if p.starts_with(&root_with_slash) {
                p = p[root_with_slash.len()..].to_string();
            } else if p == root_normalized {
                // Edge case: path equals root exactly
                p = String::new();
            }
        }

        // Strip leading ./
        if let Some(stripped) = p.strip_prefix("./") {
            p = stripped.to_string();
        }

        if !p.is_empty() {
            new_files.insert(p, fc);
        }
    }
    data.files = new_files;
}

/// Automatically normalize paths in diagnostics data to be relative.
pub fn auto_normalize_issue_paths(data: &mut DiagnosticsData, source_root: Option<&Path>) {
    let root = source_root
        .map(|p| p.to_string_lossy().to_string())
        .or_else(|| detect_common_prefix(data.issues_by_file.keys().map(|k| k.as_str())))
        .or_else(git_toplevel);

    let old_issues = std::mem::take(&mut data.issues_by_file);
    let mut new_issues = BTreeMap::new();

    for (path, mut issues) in old_issues {
        let mut p: String = path.replace('\\', "/");

        if let Some(ref root) = root {
            let root_normalized = root.replace('\\', "/");
            let root_with_slash = if root_normalized.ends_with('/') {
                root_normalized.clone()
            } else {
                format!("{root_normalized}/")
            };
            if p.starts_with(&root_with_slash) {
                p = p[root_with_slash.len()..].to_string();
            } else if p == root_normalized {
                p = String::new();
            }
        }

        if let Some(stripped) = p.strip_prefix("./") {
            p = stripped.to_string();
        }

        if !p.is_empty() {
            for issue in &mut issues {
                issue.path = p.clone();
            }
            new_issues.insert(p, issues);
        }
    }

    data.issues_by_file = new_issues;
}

/// Detect common absolute prefix across all file paths.
fn detect_common_prefix<'a, I>(paths: I) -> Option<String>
where
    I: Iterator<Item = &'a str>,
{
    let paths: Vec<&str> = paths.collect();
    if paths.is_empty() {
        return None;
    }

    // Only detect prefix if paths are absolute
    if !paths
        .iter()
        .all(|p| p.starts_with('/') || (p.len() >= 2 && p.as_bytes()[1] == b':'))
    {
        return None;
    }

    let first = paths[0].replace('\\', "/");
    let mut prefix_end = 0;

    // Find the longest common directory prefix
    for (i, ch) in first.char_indices() {
        if ch == '/' {
            let candidate = &first[..=i];
            if paths
                .iter()
                .all(|p| p.replace('\\', "/").starts_with(candidate))
            {
                prefix_end = i + 1;
            } else {
                break;
            }
        }
    }

    if prefix_end > 1 {
        Some(first[..prefix_end].to_string())
    } else {
        None
    }
}

/// Try to get git repo root via `git rev-parse --show-toplevel`.
fn git_toplevel() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn common_suffix_len(a: &str, b: &str) -> usize {
    a.bytes()
        .rev()
        .zip(b.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{Issue, Severity};

    #[test]
    fn test_exact_match() {
        let mut mapper = PathMapper::new(vec![], BTreeMap::new(), None);
        let known = vec!["src/main.rs", "src/lib.rs"];
        assert_eq!(
            mapper.resolve("src/main.rs", &known),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_strip_prefix() {
        let mut mapper = PathMapper::new(vec!["/app/".to_string()], BTreeMap::new(), None);
        let known = vec!["src/main.rs"];
        assert_eq!(
            mapper.resolve("/app/src/main.rs", &known),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_rule_substitution() {
        let mut rules = BTreeMap::new();
        rules.insert("/build/classes/".to_string(), "src/main/java/".to_string());
        let mut mapper = PathMapper::new(vec![], rules, None);
        let known = vec!["src/main/java/com/App.java"];
        assert_eq!(
            mapper.resolve("/build/classes/com/App.java", &known),
            Some("src/main/java/com/App.java".to_string())
        );
    }

    #[test]
    fn test_auto_normalize_absolute_paths() {
        let mut data = CoverageData::new();
        data.files.insert(
            "/home/user/project/src/main.rs".to_string(),
            crate::model::FileCoverage::new(),
        );
        data.files.insert(
            "/home/user/project/tests/test.rs".to_string(),
            crate::model::FileCoverage::new(),
        );

        auto_normalize_paths(&mut data, None);
        assert!(data.files.contains_key("src/main.rs"));
        assert!(data.files.contains_key("tests/test.rs"));
    }

    #[test]
    fn test_auto_normalize_with_source_root() {
        let mut data = CoverageData::new();
        data.files.insert(
            "/app/src/main.rs".to_string(),
            crate::model::FileCoverage::new(),
        );

        auto_normalize_paths(&mut data, Some(Path::new("/app")));
        assert!(data.files.contains_key("src/main.rs"));
    }

    #[test]
    fn test_auto_normalize_strips_dot_slash() {
        let mut data = CoverageData::new();
        data.files.insert(
            "./src/main.rs".to_string(),
            crate::model::FileCoverage::new(),
        );

        auto_normalize_paths(&mut data, None);
        assert!(data.files.contains_key("src/main.rs"));
    }

    #[test]
    fn test_auto_normalize_backslashes() {
        let mut data = CoverageData::new();
        data.files.insert(
            "C:\\Users\\dev\\project\\src\\main.rs".to_string(),
            crate::model::FileCoverage::new(),
        );

        auto_normalize_paths(&mut data, Some(Path::new("C:\\Users\\dev\\project")));
        assert!(data.files.contains_key("src/main.rs"));
    }

    #[test]
    fn test_auto_normalize_issue_paths() {
        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "/repo/src/main.rs".to_string(),
            vec![Issue {
                path: "/repo/src/main.rs".to_string(),
                line: 10,
                column: None,
                end_line: None,
                severity: Severity::Warning,
                rule_id: "x".to_string(),
                message: "m".to_string(),
                source: "tool".to_string(),
                fingerprint: "fp".to_string(),
            }],
        );

        auto_normalize_issue_paths(&mut data, Some(Path::new("/repo")));
        assert!(data.issues_by_file.contains_key("src/main.rs"));
        assert_eq!(data.issues_by_file["src/main.rs"][0].path, "src/main.rs");
    }

    #[test]
    fn test_caching() {
        let mut mapper = PathMapper::new(vec![], BTreeMap::new(), None);
        let known = vec!["src/main.rs"];
        mapper.resolve("src/main.rs", &known);
        // Second call uses cache
        assert_eq!(
            mapper.resolve("src/main.rs", &known),
            Some("src/main.rs".to_string())
        );
    }
}
