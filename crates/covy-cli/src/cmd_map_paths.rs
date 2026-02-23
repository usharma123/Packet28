use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct MapPathsArgs {
    /// Learn path mapping rules from configured reports
    #[arg(long)]
    pub learn: bool,

    /// Persist learned rules to covy.toml
    #[arg(long)]
    pub write: bool,

    /// Explain how a path maps into repository-relative path
    #[arg(long)]
    pub explain: Option<String>,

    /// Coverage report file paths/globs to use for --learn (overrides [ingest].report_paths)
    #[arg(long = "paths")]
    pub paths: Vec<String>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LearnResult {
    mapped: usize,
    total: usize,
    suggested_strip_prefixes: Vec<String>,
    unmapped_prefixes: Vec<(String, usize)>,
}

pub fn run(args: MapPathsArgs, config_path: &str) -> Result<i32> {
    if !args.learn && args.explain.is_none() {
        anyhow::bail!("Specify at least one of --learn or --explain");
    }

    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let repo_files = load_repo_files()?;

    let mut learned_result: Option<LearnResult> = None;
    let mut explain_result: Option<ExplainResult> = None;
    let mut wrote_config = false;

    if args.learn {
        let report_files = if args.paths.is_empty() {
            resolve_report_files_from_config(config_path, &config.ingest.report_paths)?
        } else {
            crate::cmd_common::resolve_report_globs(&args.paths)?
        };
        if report_files.is_empty() {
            anyhow::bail!(
                "No report files found. Provide --paths <globs> or configure [ingest].report_paths in covy.toml."
            );
        }

        let observed_paths = load_report_paths(&report_files)?;
        let learned =
            learn_strip_prefixes(&observed_paths, &repo_files, config.paths.case_sensitive);

        if !args.json {
            if learned.total == 0 {
                println!("No report paths were detected from configured reports.");
            } else {
                let pct = (learned.mapped as f64 / learned.total as f64) * 100.0;
                println!(
                    "Mapped paths: {}/{} ({pct:.1}%)",
                    learned.mapped, learned.total
                );
                if learned.suggested_strip_prefixes.is_empty() {
                    println!("No strip_prefix suggestions generated.");
                } else {
                    println!("Suggested strip_prefix rules:");
                    for prefix in &learned.suggested_strip_prefixes {
                        println!("  - {prefix}");
                    }
                }
                if !learned.unmapped_prefixes.is_empty() {
                    println!("Top unmapped prefixes:");
                    for (prefix, count) in learned.unmapped_prefixes.iter().take(5) {
                        println!("  - {prefix} ({count})");
                    }
                }
            }
        }

        if args.write {
            if learned.suggested_strip_prefixes.is_empty() {
                if !args.json {
                    println!("Skipping --write: no suggested strip_prefix rules to persist.");
                }
            } else {
                write_strip_prefixes(config_path, &learned.suggested_strip_prefixes)?;
                wrote_config = true;
                if !args.json {
                    println!("Updated {} with [paths].strip_prefix", config_path);
                }
            }
        }
        learned_result = Some(learned);
    }

    if let Some(path) = args.explain.as_deref() {
        let explanation = explain_path(path, &repo_files, &config);
        if args.json {
            explain_result = Some(explanation);
        } else {
            println!("input: {}", explanation.input);
            println!("rule: {}", explanation.rule);
            match explanation.mapped {
                Some(mapped) => println!("mapped: {mapped}"),
                None => println!("mapped: (no match)"),
            }
        }
    }

    if args.json {
        #[derive(serde::Serialize)]
        struct MapPathsJsonOutput {
            learn: Option<LearnResult>,
            explain: Option<ExplainResult>,
            wrote_config: bool,
        }
        let out = MapPathsJsonOutput {
            learn: learned_result,
            explain: explain_result,
            wrote_config,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    }

    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct ExplainResult {
    input: String,
    rule: String,
    mapped: Option<String>,
}

fn explain_path(path: &str, repo_files: &[String], config: &CovyConfig) -> ExplainResult {
    let input = normalize_path(path);
    let normalized_repo_files = repo_files
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Vec<_>>();
    let known: BTreeSet<String> = normalized_repo_files
        .iter()
        .map(|p| normalize_case(p, config.paths.case_sensitive))
        .collect();

    if config
        .paths
        .ignore_globs
        .iter()
        .any(|g| glob_matches(g, &input, config.paths.case_sensitive))
    {
        return ExplainResult {
            input,
            rule: "ignore_globs".to_string(),
            mapped: None,
        };
    }

    if contains_path(&known, &input, config.paths.case_sensitive) {
        return ExplainResult {
            input: input.clone(),
            rule: "exact".to_string(),
            mapped: Some(input),
        };
    }

    for rule in &config.paths.replace_prefix {
        let from = normalize_path(&rule.from);
        let to = normalize_path(&rule.to);
        if let Some(rest) = strip_prefix_case(&input, &from, config.paths.case_sensitive) {
            let candidate = normalize_path(&format!("{to}{rest}"));
            if contains_path(&known, &candidate, config.paths.case_sensitive) {
                return ExplainResult {
                    input,
                    rule: format!("replace_prefix:{}=>{}", rule.from, rule.to),
                    mapped: Some(candidate),
                };
            }
        }
    }

    for (from, to) in &config.path_mapping.rules {
        let from = normalize_path(from);
        let to = normalize_path(to);
        if let Some(rest) = strip_prefix_case(&input, &from, config.paths.case_sensitive) {
            let candidate = normalize_path(&format!("{to}{rest}"));
            if contains_path(&known, &candidate, config.paths.case_sensitive) {
                return ExplainResult {
                    input,
                    rule: format!("legacy_path_mapping:{}=>{}", from, to),
                    mapped: Some(candidate),
                };
            }
        }
    }

    for prefix in &config.paths.strip_prefix {
        let prefix = normalize_path(prefix);
        if let Some(stripped) = strip_prefix_case(&input, &prefix, config.paths.case_sensitive) {
            let candidate = stripped.trim_start_matches('/').to_string();
            if contains_path(&known, &candidate, config.paths.case_sensitive) {
                return ExplainResult {
                    input,
                    rule: format!("strip_prefix:{prefix}"),
                    mapped: Some(candidate),
                };
            }
        }
    }

    let file_name = input.rsplit('/').next().unwrap_or(input.as_str());
    let mut best: Option<(&str, usize)> = None;
    for repo in &normalized_repo_files {
        let repo_name = repo.rsplit('/').next().unwrap_or(repo.as_str());
        if normalize_case(repo_name, config.paths.case_sensitive)
            != normalize_case(file_name, config.paths.case_sensitive)
        {
            continue;
        }
        let score = common_suffix_len(
            &normalize_case(repo.as_str(), config.paths.case_sensitive),
            &normalize_case(&input, config.paths.case_sensitive),
        );
        best = choose_best(best, (repo.as_str(), score), config.paths.case_sensitive);
    }

    ExplainResult {
        input,
        rule: "suffix_fallback".to_string(),
        mapped: best.map(|(p, _)| p.to_string()),
    }
}

fn resolve_report_files_from_config(
    config_path: &str,
    patterns: &[String],
) -> Result<Vec<PathBuf>> {
    crate::cmd_common::resolve_report_globs_for_config(config_path, patterns)
}

fn load_report_paths(report_files: &[PathBuf]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for report in report_files {
        let mut coverage = covy_ingest::ingest_path(report)
            .with_context(|| format!("Failed to parse coverage report {}", report.display()))?;
        covy_core::pathmap::auto_normalize_paths(&mut coverage, None);
        paths.extend(coverage.files.keys().cloned());
    }
    Ok(paths)
}

fn load_repo_files() -> Result<Vec<String>> {
    let root = std::env::current_dir()?;
    let snapshot = covy_core::snapshot::build_snapshot(&root)?;
    Ok(snapshot.file_hashes.keys().cloned().collect())
}

fn learn_strip_prefixes(
    report_paths: &[String],
    repo_files: &[String],
    case_sensitive: bool,
) -> LearnResult {
    let mut mapped = 0usize;
    let mut prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut unmapped_prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
    let normalized_repo_files = repo_files
        .iter()
        .map(|repo| normalize_path(repo))
        .collect::<Vec<_>>();
    let known_repo_refs: Vec<&str> = normalized_repo_files.iter().map(|s| s.as_str()).collect();
    let mut mapper = covy_core::pathmap::PathMapper::with_options(
        Vec::new(),
        BTreeMap::new(),
        Vec::new(),
        case_sensitive,
        None,
    );

    for report_path in report_paths {
        let normalized = normalize_path(report_path);
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

    LearnResult {
        mapped,
        total: report_paths.len(),
        suggested_strip_prefixes: suggested_strip_prefixes
            .into_iter()
            .take(5)
            .map(|(prefix, _)| prefix)
            .collect(),
        unmapped_prefixes,
    }
}

fn write_strip_prefixes(config_path: &str, strip_prefixes: &[String]) -> Result<()> {
    let path = Path::new(config_path);
    let mut doc = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        raw.parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("Failed to parse {} as TOML", path.display()))?
    } else {
        toml_edit::DocumentMut::new()
    };

    if !doc.as_table().contains_key("paths") {
        doc["paths"] = toml_edit::table();
    }
    if !doc["paths"].is_table() {
        anyhow::bail!("[paths] must be a TOML table");
    }

    let mut array = toml_edit::Array::default();
    for prefix in strip_prefixes {
        array.push(prefix.as_str());
    }
    doc["paths"]["strip_prefix"] = toml_edit::value(array);

    std::fs::write(path, doc.to_string())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
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

fn normalize_path(path: &str) -> String {
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
    use tempfile::TempDir;

    #[test]
    fn test_learn_strip_prefixes_from_absolute_paths() {
        let report_paths = vec![
            "/__w/repo/repo/src/main.rs".to_string(),
            "/__w/repo/repo/src/lib.rs".to_string(),
        ];
        let repo_files = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];

        let learned = learn_strip_prefixes(&report_paths, &repo_files, true);
        assert_eq!(learned.total, 2);
        assert_eq!(learned.mapped, 2);
        assert_eq!(
            learned.suggested_strip_prefixes,
            vec!["/__w/repo/repo".to_string()]
        );
    }

    #[test]
    fn test_resolve_report_files_deduplicates_and_sorts() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.info");
        let b = dir.path().join("b.info");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        let patterns = vec![
            format!("{}/*.info", dir.path().display()),
            format!("{}/a.info", dir.path().display()),
        ];
        let files = crate::cmd_common::resolve_report_globs(&patterns).unwrap();

        assert_eq!(files, vec![a, b]);
    }

    #[test]
    fn test_explain_path_prefers_replace_prefix() {
        let mut cfg = CovyConfig::default();
        cfg.paths.case_sensitive = true;
        cfg.paths.strip_prefix = vec!["/workspace".to_string()];
        cfg.paths.replace_prefix = vec![covy_core::config::ReplacePrefixRule {
            from: "/build/classes".to_string(),
            to: "src/main/java".to_string(),
        }];
        let repo_files = vec!["src/main/java/com/App.java".to_string()];

        let result = explain_path("/build/classes/com/App.java", &repo_files, &cfg);
        assert_eq!(
            result.mapped,
            Some("src/main/java/com/App.java".to_string())
        );
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
        let learned = learn_strip_prefixes(&report_paths, &repo_files, true);
        assert_eq!(learned.total, 2);
        assert_eq!(learned.mapped, 2);
    }

    #[test]
    fn test_write_strip_prefixes_updates_paths_table() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("covy.toml");
        std::fs::write(&config_path, "[project]\nname='demo'\n").unwrap();

        write_strip_prefixes(
            config_path.to_str().unwrap(),
            &[
                "/__w/repo/repo".to_string(),
                "/home/runner/work/repo/repo".to_string(),
            ],
        )
        .unwrap();

        let updated = std::fs::read_to_string(&config_path).unwrap();
        assert!(updated.contains("[paths]"));
        assert!(updated.contains("strip_prefix"));
        assert!(updated.contains("/__w/repo/repo"));
    }
}
