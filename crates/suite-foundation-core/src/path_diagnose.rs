use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::config::CovyConfig;
use crate::pathmap::PathMapper;

#[derive(Debug, thiserror::Error)]
pub enum PathDiagnosisError {
    #[error(transparent)]
    Core(#[from] crate::error::CovyError),
}

#[derive(Debug, Clone)]
pub struct PathDiagnosisRequest {
    pub report_paths: Vec<String>,
    pub repo_paths: Vec<String>,
    pub strip_prefixes: Vec<String>,
    pub replace_rules: BTreeMap<String, String>,
    pub ignore_globs: Vec<String>,
    pub case_sensitive: bool,
}

impl PathDiagnosisRequest {
    pub fn from_config(
        report_paths: Vec<String>,
        repo_paths: Vec<String>,
        config: &CovyConfig,
    ) -> Self {
        let mut replace_rules = config.path_mapping.rules.clone();
        for rule in &config.paths.replace_prefix {
            replace_rules.insert(rule.from.clone(), rule.to.clone());
        }

        let mut strip_prefixes = config.paths.strip_prefix.clone();
        strip_prefixes.extend(config.ingest.strip_prefixes.clone());

        Self {
            report_paths,
            repo_paths,
            strip_prefixes,
            replace_rules,
            ignore_globs: config.paths.ignore_globs.clone(),
            case_sensitive: config.paths.case_sensitive,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathDiagnosisResponse {
    pub mapped: usize,
    pub total: usize,
    pub unmapped_prefixes: Vec<(String, usize)>,
    pub suggested_strip_prefixes: Vec<String>,
    pub explanation_chain: Vec<String>,
    pub warnings: Vec<String>,
    pub suggested_fixes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PathLearnRequest {
    pub report_paths: Vec<String>,
    pub repo_paths: Vec<String>,
    pub case_sensitive: bool,
    pub max_suggested_strip_prefixes: usize,
}

impl PathLearnRequest {
    pub fn new(report_paths: Vec<String>, repo_paths: Vec<String>, case_sensitive: bool) -> Self {
        Self {
            report_paths,
            repo_paths,
            case_sensitive,
            max_suggested_strip_prefixes: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathLearnResponse {
    pub mapped: usize,
    pub total: usize,
    pub suggested_strip_prefixes: Vec<String>,
    pub unmapped_prefixes: Vec<(String, usize)>,
    pub warnings: Vec<String>,
    pub suggested_fixes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PathExplainRequest {
    pub input_path: String,
    pub repo_paths: Vec<String>,
    pub case_sensitive: bool,
    pub ignore_globs: Vec<String>,
    pub replace_prefix_rules: Vec<(String, String)>,
    pub legacy_rules: BTreeMap<String, String>,
    pub strip_prefixes: Vec<String>,
}

impl PathExplainRequest {
    pub fn from_config(
        input_path: impl Into<String>,
        repo_paths: Vec<String>,
        config: &CovyConfig,
    ) -> Self {
        Self {
            input_path: input_path.into(),
            repo_paths,
            case_sensitive: config.paths.case_sensitive,
            ignore_globs: config.paths.ignore_globs.clone(),
            replace_prefix_rules: config
                .paths
                .replace_prefix
                .iter()
                .map(|r| (r.from.clone(), r.to.clone()))
                .collect(),
            legacy_rules: config.path_mapping.rules.clone(),
            strip_prefixes: config.paths.strip_prefix.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingStrategy {
    IgnoreGlobs,
    Exact,
    ReplacePrefix,
    LegacyPathMapping,
    StripPrefix,
    SuffixFallback,
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathExplainResponse {
    pub input: String,
    pub rule: String,
    pub mapped: Option<String>,
    pub strategy: MappingStrategy,
    pub explanation_chain: Vec<String>,
    pub warnings: Vec<String>,
    pub suggested_fixes: Vec<String>,
}

pub fn load_repo_paths(repo_root: &Path) -> Result<Vec<String>, PathDiagnosisError> {
    let snapshot = crate::snapshot::build_snapshot(repo_root)?;
    Ok(snapshot.file_hashes.keys().cloned().collect())
}

pub fn diagnose_paths(
    req: PathDiagnosisRequest,
) -> Result<PathDiagnosisResponse, PathDiagnosisError> {
    let normalized_repo_paths = req
        .repo_paths
        .iter()
        .map(|repo| normalize_repo_relative_path(repo))
        .collect::<Vec<_>>();
    let known_refs: Vec<&str> = normalized_repo_paths.iter().map(|s| s.as_str()).collect();

    let mut mapper = PathMapper::with_options(
        req.strip_prefixes,
        req.replace_rules,
        req.ignore_globs,
        req.case_sensitive,
        None,
    );

    let mut mapped = 0usize;
    let mut unmapped: Vec<String> = Vec::new();
    for report_path in &req.report_paths {
        let normalized = normalize_repo_relative_path(report_path);
        if mapper.resolve(&normalized, &known_refs).is_some() {
            mapped += 1;
        } else {
            unmapped.push(normalized);
        }
    }

    let suggested_strip_prefixes = infer_strip_prefixes(
        &req.report_paths,
        &normalized_repo_paths,
        req.case_sensitive,
        3,
    );
    let mut warnings = Vec::new();
    if req.report_paths.is_empty() {
        warnings.push("No report paths were provided for diagnosis.".to_string());
    } else if mapped < req.report_paths.len() {
        warnings.push("Some report paths could not be mapped to repository paths.".to_string());
    }
    let suggested_fixes = suggested_strip_prefixes
        .iter()
        .map(|p| format!("Add '{p}' to [paths].strip_prefix"))
        .collect();

    Ok(PathDiagnosisResponse {
        mapped,
        total: req.report_paths.len(),
        unmapped_prefixes: top_prefixes(&unmapped),
        suggested_strip_prefixes,
        explanation_chain: vec![
            "exact match".to_string(),
            "replace-prefix mapping".to_string(),
            "strip-prefix mapping".to_string(),
            "suffix fallback".to_string(),
        ],
        warnings,
        suggested_fixes,
    })
}

pub fn learn_path_mapping(req: PathLearnRequest) -> Result<PathLearnResponse, PathDiagnosisError> {
    let normalized_repo_files = req
        .repo_paths
        .iter()
        .map(|repo| normalize_repo_relative_path(repo))
        .collect::<Vec<_>>();
    let known_repo_refs: Vec<&str> = normalized_repo_files.iter().map(|s| s.as_str()).collect();
    let mut mapper = PathMapper::with_options(
        Vec::new(),
        BTreeMap::new(),
        Vec::new(),
        req.case_sensitive,
        None,
    );

    let mut mapped = 0usize;
    let mut prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut unmapped_prefix_counts: BTreeMap<String, usize> = BTreeMap::new();

    for report_path in &req.report_paths {
        let normalized = normalize_repo_relative_path(report_path);
        if let Some(repo) = mapper.resolve(&normalized, &known_repo_refs) {
            mapped += 1;
            if normalized.ends_with(&repo) {
                let prefix = normalized[..normalized.len() - repo.len()]
                    .trim_end_matches('/')
                    .to_string();
                if !prefix.is_empty() {
                    *prefix_counts.entry(prefix).or_insert(0) += 1;
                }
            }
        } else {
            let prefix = first_two_segments(&normalized);
            *unmapped_prefix_counts.entry(prefix).or_insert(0) += 1;
        }
    }

    let mut suggested_strip_prefixes: Vec<(String, usize)> = prefix_counts.into_iter().collect();
    suggested_strip_prefixes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut unmapped_prefixes: Vec<(String, usize)> = unmapped_prefix_counts.into_iter().collect();
    unmapped_prefixes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let suggested_strip_prefixes = suggested_strip_prefixes
        .into_iter()
        .take(req.max_suggested_strip_prefixes)
        .map(|(prefix, _)| prefix)
        .collect::<Vec<_>>();

    let mut warnings = Vec::new();
    if req.report_paths.is_empty() {
        warnings.push("No report paths were provided for learning.".to_string());
    } else if mapped < req.report_paths.len() {
        warnings.push("Learning completed with unmapped path prefixes.".to_string());
    }
    let suggested_fixes = suggested_strip_prefixes
        .iter()
        .map(|p| format!("Add '{p}' to [paths].strip_prefix"))
        .collect();

    Ok(PathLearnResponse {
        mapped,
        total: req.report_paths.len(),
        suggested_strip_prefixes,
        unmapped_prefixes,
        warnings,
        suggested_fixes,
    })
}

pub fn explain_path_mapping(
    req: PathExplainRequest,
) -> Result<PathExplainResponse, PathDiagnosisError> {
    let input = normalize_repo_relative_path(&req.input_path);
    let normalized_repo_paths = req
        .repo_paths
        .iter()
        .map(|p| normalize_repo_relative_path(p))
        .collect::<Vec<_>>();
    let known: BTreeSet<String> = normalized_repo_paths
        .iter()
        .map(|p| normalize_case(p, req.case_sensitive))
        .collect();

    for glob in &req.ignore_globs {
        if glob_matches(glob, &input, req.case_sensitive) {
            return Ok(PathExplainResponse {
                input,
                rule: "ignore_globs".to_string(),
                mapped: None,
                strategy: MappingStrategy::IgnoreGlobs,
                explanation_chain: vec!["ignore_globs".to_string()],
                warnings: vec!["Path matched [paths].ignore_globs and was skipped.".to_string()],
                suggested_fixes: Vec::new(),
            });
        }
    }

    if contains_path(&known, &input, req.case_sensitive) {
        return Ok(PathExplainResponse {
            input: input.clone(),
            rule: "exact".to_string(),
            mapped: Some(input),
            strategy: MappingStrategy::Exact,
            explanation_chain: vec!["exact".to_string()],
            warnings: Vec::new(),
            suggested_fixes: Vec::new(),
        });
    }

    for (from_raw, to_raw) in &req.replace_prefix_rules {
        let from = normalize_repo_relative_path(from_raw);
        let to = normalize_repo_relative_path(to_raw);
        if let Some(rest) = strip_prefix_case(&input, &from, req.case_sensitive) {
            let candidate = normalize_repo_relative_path(&format!("{to}{rest}"));
            if contains_path(&known, &candidate, req.case_sensitive) {
                return Ok(PathExplainResponse {
                    input,
                    rule: format!("replace_prefix:{from_raw}=>{to_raw}"),
                    mapped: Some(candidate),
                    strategy: MappingStrategy::ReplacePrefix,
                    explanation_chain: vec!["replace_prefix".to_string()],
                    warnings: Vec::new(),
                    suggested_fixes: Vec::new(),
                });
            }
        }
    }

    for (from_raw, to_raw) in &req.legacy_rules {
        let from = normalize_repo_relative_path(from_raw);
        let to = normalize_repo_relative_path(to_raw);
        if let Some(rest) = strip_prefix_case(&input, &from, req.case_sensitive) {
            let candidate = normalize_repo_relative_path(&format!("{to}{rest}"));
            if contains_path(&known, &candidate, req.case_sensitive) {
                return Ok(PathExplainResponse {
                    input,
                    rule: format!("legacy_path_mapping:{from}=>{to}"),
                    mapped: Some(candidate),
                    strategy: MappingStrategy::LegacyPathMapping,
                    explanation_chain: vec!["legacy_path_mapping".to_string()],
                    warnings: vec![
                        "Matched legacy [path_mapping].rules; consider migrating to [paths].replace_prefix."
                            .to_string(),
                    ],
                    suggested_fixes: Vec::new(),
                });
            }
        }
    }

    for prefix_raw in &req.strip_prefixes {
        let prefix = normalize_repo_relative_path(prefix_raw);
        if let Some(stripped) = strip_prefix_case(&input, &prefix, req.case_sensitive) {
            let candidate = stripped.trim_start_matches('/').to_string();
            if contains_path(&known, &candidate, req.case_sensitive) {
                return Ok(PathExplainResponse {
                    input,
                    rule: format!("strip_prefix:{prefix}"),
                    mapped: Some(candidate),
                    strategy: MappingStrategy::StripPrefix,
                    explanation_chain: vec!["strip_prefix".to_string()],
                    warnings: Vec::new(),
                    suggested_fixes: Vec::new(),
                });
            }
        }
    }

    let file_name = input.rsplit('/').next().unwrap_or(input.as_str());
    let mut best: Option<(&str, usize)> = None;
    for repo in &normalized_repo_paths {
        let repo_name = repo.rsplit('/').next().unwrap_or(repo.as_str());
        if normalize_case(repo_name, req.case_sensitive)
            != normalize_case(file_name, req.case_sensitive)
        {
            continue;
        }
        let score = common_suffix_len(
            &normalize_case(repo.as_str(), req.case_sensitive),
            &normalize_case(&input, req.case_sensitive),
        );
        best = choose_best(best, (repo.as_str(), score), req.case_sensitive);
    }

    if let Some((mapped, _)) = best {
        return Ok(PathExplainResponse {
            input,
            rule: "suffix_fallback".to_string(),
            mapped: Some(mapped.to_string()),
            strategy: MappingStrategy::SuffixFallback,
            explanation_chain: vec!["suffix_fallback".to_string()],
            warnings: vec![
                "Matched by suffix fallback only; this may be ambiguous in large repositories."
                    .to_string(),
            ],
            suggested_fixes: vec!["Add [paths].replace_prefix or [paths].strip_prefix rules for deterministic mapping.".to_string()],
        });
    }

    Ok(PathExplainResponse {
        input,
        rule: "no_match".to_string(),
        mapped: None,
        strategy: MappingStrategy::NoMatch,
        explanation_chain: vec!["no_match".to_string()],
        warnings: vec!["No mapping strategy produced a repository path.".to_string()],
        suggested_fixes: vec![
            "Run 'covy map-paths --learn' to infer strip_prefix candidates.".to_string(),
            "Add [paths].replace_prefix rules for build output directories.".to_string(),
        ],
    })
}

fn infer_strip_prefixes(
    report_paths: &[String],
    repo_paths: &[String],
    case_sensitive: bool,
    limit: usize,
) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for report in report_paths {
        let report = normalize_repo_relative_path(report);
        if let Some((repo, _)) = best_suffix_match(&report, repo_paths, case_sensitive) {
            if report.ends_with(repo) {
                let prefix = report[..report.len() - repo.len()]
                    .trim_end_matches('/')
                    .to_string();
                if !prefix.is_empty() {
                    *counts.entry(prefix).or_insert(0) += 1;
                }
            }
        }
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(limit).map(|(p, _)| p).collect()
}

fn best_suffix_match<'a>(
    path: &str,
    repo_paths: &'a [String],
    case_sensitive: bool,
) -> Option<(&'a str, usize)> {
    let mut best: Option<(&str, usize)> = None;
    for repo in repo_paths {
        let repo_norm = normalize_repo_relative_path(repo);
        if normalize_case(path, case_sensitive)
            .ends_with(&normalize_case(&repo_norm, case_sensitive))
        {
            let score = repo_norm.len();
            best = match best {
                None => Some((repo.as_str(), score)),
                Some((current_repo, current_score)) => {
                    if score > current_score {
                        Some((repo.as_str(), score))
                    } else if score < current_score {
                        Some((current_repo, current_score))
                    } else if normalize_case(repo, case_sensitive)
                        < normalize_case(current_repo, case_sensitive)
                    {
                        Some((repo.as_str(), score))
                    } else {
                        Some((current_repo, current_score))
                    }
                }
            };
        }
    }
    best
}

fn top_prefixes(paths: &[String]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in paths {
        let prefix = first_two_segments(path);
        *counts.entry(prefix).or_insert(0) += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

fn contains_path(known: &BTreeSet<String>, candidate: &str, case_sensitive: bool) -> bool {
    known.contains(&normalize_case(candidate, case_sensitive))
}

fn choose_best<'a>(
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

fn normalize_case(path: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        path.to_string()
    } else {
        path.to_ascii_lowercase()
    }
}

pub fn normalize_repo_relative_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("./") {
        stripped.to_string()
    } else {
        normalized
    }
}

fn strip_prefix_case<'a>(path: &'a str, prefix: &str, case_sensitive: bool) -> Option<&'a str> {
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

fn glob_matches(pattern: &str, path: &str, case_sensitive: bool) -> bool {
    if let Ok(p) = glob::Pattern::new(pattern) {
        if p.matches(path) {
            return true;
        }
    }
    if !case_sensitive {
        let lower_pattern = pattern.to_ascii_lowercase();
        let lower_path = path.to_ascii_lowercase();
        if let Ok(p) = glob::Pattern::new(&lower_pattern) {
            return p.matches(&lower_path);
        }
    }
    false
}

fn common_suffix_len(a: &str, b: &str) -> usize {
    a.bytes()
        .rev()
        .zip(b.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

fn first_two_segments(path: &str) -> String {
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let a = parts.next().unwrap_or(path);
    let b = parts.next();
    if let Some(b) = b {
        format!("{a}/{b}")
    } else {
        a.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReplacePrefixRule;

    #[test]
    fn test_learn_strip_prefixes_from_absolute_paths() {
        let report_paths = vec![
            "/__w/repo/repo/src/main.rs".to_string(),
            "/__w/repo/repo/src/lib.rs".to_string(),
        ];
        let repo_files = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];

        let learned =
            learn_path_mapping(PathLearnRequest::new(report_paths, repo_files, true)).unwrap();
        assert_eq!(learned.total, 2);
        assert_eq!(learned.mapped, 2);
        assert_eq!(
            learned.suggested_strip_prefixes,
            vec!["/__w/repo/repo".to_string()]
        );
    }

    #[test]
    fn test_explain_path_prefers_replace_prefix() {
        let mut cfg = CovyConfig::default();
        cfg.paths.case_sensitive = true;
        cfg.paths.strip_prefix = vec!["/workspace".to_string()];
        cfg.paths.replace_prefix = vec![ReplacePrefixRule {
            from: "/build/classes".to_string(),
            to: "src/main/java".to_string(),
        }];
        let repo_files = vec!["src/main/java/com/App.java".to_string()];

        let req = PathExplainRequest::from_config("/build/classes/com/App.java", repo_files, &cfg);
        let result = explain_path_mapping(req).unwrap();
        assert_eq!(
            result.mapped,
            Some("src/main/java/com/App.java".to_string())
        );
        assert_eq!(result.strategy, MappingStrategy::ReplacePrefix);
        assert!(result.rule.starts_with("replace_prefix:"));
    }

    #[test]
    fn test_learn_counts_package_style_paths_as_mapped() {
        let report_paths = vec![
            "com/example/Calculator.java".to_string(),
            "com/example/StringUtils.java".to_string(),
        ];
        let repo_files = vec![
            "JavaTest/src/main/java/com/example/Calculator.java".to_string(),
            "JavaTest/src/main/java/com/example/StringUtils.java".to_string(),
        ];
        let learned =
            learn_path_mapping(PathLearnRequest::new(report_paths, repo_files, true)).unwrap();
        assert_eq!(learned.total, 2);
        assert_eq!(learned.mapped, 2);
    }

    #[test]
    fn test_diagnose_top_prefixes_deterministic() {
        let mut cfg = CovyConfig::default();
        cfg.paths.case_sensitive = true;
        let req = PathDiagnosisRequest::from_config(
            vec![
                "/__w/repo/repo/src/main.rs".to_string(),
                "/__w/repo/repo/src/lib.rs".to_string(),
                "/workspace/app/src/a.rs".to_string(),
            ],
            vec!["src/other.rs".to_string()],
            &cfg,
        );
        let diag = diagnose_paths(req).unwrap();
        assert_eq!(diag.unmapped_prefixes[0], ("__w/repo".to_string(), 2));
    }

    #[test]
    fn test_diagnose_infers_strip_prefixes_from_suffix_match() {
        let mut cfg = CovyConfig::default();
        cfg.paths.case_sensitive = true;
        let req = PathDiagnosisRequest::from_config(
            vec![
                "/__w/repo/repo/src/main.rs".to_string(),
                "/__w/repo/repo/src/lib.rs".to_string(),
            ],
            vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            &cfg,
        );

        let diag = diagnose_paths(req).unwrap();
        assert_eq!(
            diag.suggested_strip_prefixes,
            vec!["/__w/repo/repo".to_string()]
        );
    }
}
