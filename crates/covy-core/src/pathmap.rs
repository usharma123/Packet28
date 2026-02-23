use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use crate::diagnostics::DiagnosticsData;
use crate::model::{CoverageData, RepoSnapshot};

/// Strategies for mapping coverage file paths to repository file paths.
pub struct PathMapper {
    strip_prefixes: Vec<String>,
    rules: Vec<(String, String)>,
    ignore_globs: Vec<String>,
    ignore_globs_lower: Vec<String>,
    case_sensitive: bool,
    /// Reverse index: filename → list of full paths in the repo.
    suffix_index: HashMap<String, Vec<String>>,
    /// Content hash index for fallback matching.
    hash_index: HashMap<String, Vec<String>>,
    /// LRU cache of resolved mappings.
    cache: HashMap<String, Option<String>>,
    /// Cached known-path index keyed by hash of known_paths.
    cached_known_index: Option<(u64, Arc<HashMap<String, String>>)>,
}

impl PathMapper {
    pub fn new(
        strip_prefixes: Vec<String>,
        rules: BTreeMap<String, String>,
        snapshot: Option<&RepoSnapshot>,
    ) -> Self {
        Self::with_options(strip_prefixes, rules, Vec::new(), !cfg!(windows), snapshot)
    }

    pub fn with_options(
        strip_prefixes: Vec<String>,
        rules: BTreeMap<String, String>,
        ignore_globs: Vec<String>,
        case_sensitive: bool,
        snapshot: Option<&RepoSnapshot>,
    ) -> Self {
        let mut suffix_index = HashMap::new();
        let mut hash_index = HashMap::new();
        let normalized_strip_prefixes = normalize_prefixes(strip_prefixes);
        let normalized_rules = normalize_rules(rules);
        let normalized_ignore_globs = ignore_globs
            .into_iter()
            .map(|g| normalize_path(g.trim()))
            .filter(|g| !g.is_empty())
            .collect::<Vec<_>>();
        let normalized_ignore_globs_lower = if case_sensitive {
            Vec::new()
        } else {
            normalized_ignore_globs
                .iter()
                .map(|g| g.to_ascii_lowercase())
                .collect::<Vec<_>>()
        };

        if let Some(snap) = snapshot {
            for (path, hash) in &snap.file_hashes {
                let normalized_path = normalize_path(path);
                // Build suffix index by filename
                if let Some(filename) = normalized_path.rsplit('/').next() {
                    suffix_index
                        .entry(normalize_case(filename, case_sensitive))
                        .or_insert_with(Vec::new)
                        .push(normalized_path.clone());
                }
                // Build hash index
                hash_index
                    .entry(hash.clone())
                    .or_insert_with(Vec::new)
                    .push(normalized_path.clone());
            }
        }

        Self {
            strip_prefixes: normalized_strip_prefixes,
            rules: normalized_rules,
            ignore_globs: normalized_ignore_globs,
            ignore_globs_lower: normalized_ignore_globs_lower,
            case_sensitive,
            suffix_index,
            hash_index,
            cache: HashMap::new(),
            cached_known_index: None,
        }
    }

    /// Resolve a coverage file path to a repository file path.
    /// Strategy chain: exact match → rule substitution → strip prefix → suffix match.
    pub fn resolve(&mut self, coverage_path: &str, known_paths: &[&str]) -> Option<String> {
        let cache_key = normalize_case(&normalize_path(coverage_path), self.case_sensitive);
        if let Some(cached) = self.cache.get(&cache_key) {
            return cached.clone();
        }

        let result = self.resolve_inner(coverage_path, known_paths);
        self.cache.insert(cache_key, result.clone());
        result
    }

    fn resolve_inner(&mut self, coverage_path: &str, known_paths: &[&str]) -> Option<String> {
        let normalized = normalize_path(coverage_path);
        if self.is_ignored(&normalized) {
            return None;
        }

        let known_index = self.get_known_index(known_paths);

        // 1. Exact match
        if let Some(exact) = self.find_known(&normalized, known_index.as_ref()) {
            return Some(exact.to_string());
        }

        // 2. Rule substitution
        for (from, to) in &self.rules {
            if let Some(rest) = strip_path_prefix_with_case(&normalized, from, self.case_sensitive)
            {
                let candidate = normalize_path(&format!("{to}{rest}"));
                if let Some(found) = self.find_known(&candidate, known_index.as_ref()) {
                    return Some(found.to_string());
                }
            }
        }

        // 3. Strip prefix
        for prefix in &self.strip_prefixes {
            if let Some(stripped) =
                strip_path_prefix_with_case(&normalized, prefix, self.case_sensitive)
            {
                let candidate = stripped.trim_start_matches('/');
                if let Some(found) = self.find_known(candidate, known_index.as_ref()) {
                    return Some(found.to_string());
                }
            }
        }

        // 4. Suffix match (by filename)
        let filename = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
        let filename_key = normalize_case(filename, self.case_sensitive);
        let mut best: Option<(&str, usize)> = None;

        if let Some(snapshot_candidates) = self.suffix_index.get(&filename_key) {
            for candidate in snapshot_candidates {
                if let Some(found) = self.find_known(candidate, known_index.as_ref()) {
                    let score = common_suffix_len(
                        &normalize_case(found, self.case_sensitive),
                        &normalize_case(&normalized, self.case_sensitive),
                    );
                    best = pick_better_match(best, (found, score), self.case_sensitive);
                }
            }
        }

        if best.is_none() {
            for known in known_paths {
                let known_normalized = normalize_path(known);
                let known_filename = known_normalized
                    .rsplit('/')
                    .next()
                    .unwrap_or(known_normalized.as_str());
                if normalize_case(known_filename, self.case_sensitive) != filename_key {
                    continue;
                }
                let score = common_suffix_len(
                    &normalize_case(&known_normalized, self.case_sensitive),
                    &normalize_case(&normalized, self.case_sensitive),
                );
                best = pick_better_match(best, (known, score), self.case_sensitive);
            }
        }

        best.map(|(path, _)| path.to_string())
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

    fn is_ignored(&self, path: &str) -> bool {
        if self.case_sensitive {
            for pattern in &self.ignore_globs {
                if glob_matches(pattern, path) {
                    return true;
                }
            }
            return false;
        }

        for pattern in &self.ignore_globs {
            if glob_matches(pattern, path) {
                return true;
            }
        }

        let lower_path = path.to_ascii_lowercase();
        for pattern in &self.ignore_globs_lower {
            if glob_matches(pattern, &lower_path) {
                return true;
            }
        }
        false
    }

    fn build_known_index(&self, known_paths: &[&str]) -> HashMap<String, String> {
        let mut index = HashMap::with_capacity(known_paths.len());
        for &path in known_paths {
            let normalized = normalize_path(path);
            let key = normalize_case(&normalized, self.case_sensitive);
            index.entry(key).or_insert_with(|| path.to_string());
        }
        index
    }

    fn get_known_index(&mut self, known_paths: &[&str]) -> Arc<HashMap<String, String>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        known_paths.len().hash(&mut hasher);
        for path in known_paths {
            path.hash(&mut hasher);
        }
        let known_paths_key = hasher.finish();

        let needs_rebuild = self
            .cached_known_index
            .as_ref()
            .map(|(cached_key, _)| *cached_key != known_paths_key)
            .unwrap_or(true);
        if needs_rebuild {
            self.cached_known_index = Some((
                known_paths_key,
                Arc::new(self.build_known_index(known_paths)),
            ));
        }

        Arc::clone(
            &self
            .cached_known_index
            .as_ref()
            .expect("known index cache must be initialized")
            .1,
        )
    }

    fn find_known<'a>(
        &self,
        candidate: &str,
        known_index: &'a HashMap<String, String>,
    ) -> Option<&'a str> {
        let key = normalize_case(&normalize_path(candidate), self.case_sensitive);
        known_index.get(&key).map(|s| s.as_str())
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
    let normalized = path.replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("./") {
        stripped.to_string()
    } else {
        normalized
    }
}

fn normalize_case(path: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        path.to_string()
    } else {
        path.to_ascii_lowercase()
    }
}

fn normalize_prefixes(prefixes: Vec<String>) -> Vec<String> {
    let mut out = prefixes
        .into_iter()
        .map(|p| normalize_path(p.trim()))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

fn normalize_rules(rules: BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = rules
        .into_iter()
        .map(|(from, to)| (normalize_path(from.trim()), normalize_path(to.trim())))
        .filter(|(from, _)| !from.is_empty())
        .collect::<Vec<_>>();
    out.sort_by(|(a_from, _), (b_from, _)| {
        b_from
            .len()
            .cmp(&a_from.len())
            .then_with(|| a_from.cmp(b_from))
    });
    out
}

fn strip_path_prefix_with_case<'a>(
    path: &'a str,
    prefix: &str,
    case_sensitive: bool,
) -> Option<&'a str> {
    if case_sensitive {
        return path.strip_prefix(prefix);
    }

    let lower_path = path.to_ascii_lowercase();
    let lower_prefix = prefix.to_ascii_lowercase();
    if !lower_path.starts_with(&lower_prefix) {
        return None;
    }
    Some(&path[prefix.len()..])
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(path))
        .unwrap_or(false)
}

fn pick_better_match<'a>(
    current: Option<(&'a str, usize)>,
    candidate: (&'a str, usize),
    case_sensitive: bool,
) -> Option<(&'a str, usize)> {
    match current {
        None => Some(candidate),
        Some((best_path, best_score)) => {
            if candidate.1 > best_score {
                return Some(candidate);
            }
            if candidate.1 < best_score {
                return Some((best_path, best_score));
            }

            let candidate_key = normalize_case(candidate.0, case_sensitive);
            let best_key = normalize_case(best_path, case_sensitive);
            if candidate_key < best_key {
                Some(candidate)
            } else {
                Some((best_path, best_score))
            }
        }
    }
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
    fn test_ignore_glob_never_resolves() {
        let mut mapper = PathMapper::with_options(
            vec![],
            BTreeMap::new(),
            vec!["**/bazel-out/**".to_string()],
            true,
            None,
        );
        let known = vec!["src/main.rs"];
        assert_eq!(
            mapper.resolve("bazel-out/k8-fastbuild/bin/main.rs", &known),
            None
        );
    }

    #[test]
    fn test_case_insensitive_exact_match() {
        let mut mapper = PathMapper::with_options(vec![], BTreeMap::new(), vec![], false, None);
        let known = vec!["Src/Main.rs"];
        assert_eq!(
            mapper.resolve("src/main.rs", &known),
            Some("Src/Main.rs".to_string())
        );
    }

    #[test]
    fn test_strip_prefix_removes_leading_separator() {
        let mut mapper = PathMapper::new(vec!["/workspace".to_string()], BTreeMap::new(), None);
        let known = vec!["src/main.rs"];
        assert_eq!(
            mapper.resolve("/workspace/src/main.rs", &known),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_suffix_match_is_deterministic_on_ties() {
        let mut mapper = PathMapper::new(vec![], BTreeMap::new(), None);
        let known = vec!["a/foo/main.rs", "b/foo/main.rs"];
        assert_eq!(
            mapper.resolve("/tmp/work/foo/main.rs", &known),
            Some("a/foo/main.rs".to_string())
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
