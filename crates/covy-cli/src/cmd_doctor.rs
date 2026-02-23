use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct DoctorArgs {
    /// Base ref for validation (default from config)
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for validation (default from config)
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
struct MappingStats {
    mapped: usize,
    total: usize,
    unmapped_prefixes: Vec<(String, usize)>,
    suggested_strip_prefixes: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct DoctorSummary {
    config_path: String,
    config_base_dir: String,
    repo_root: String,
    report_files: usize,
    parsed_report_paths: usize,
    mapped: usize,
    total: usize,
    mapped_pct: f64,
    unmapped_prefixes: Vec<(String, usize)>,
    suggested_strip_prefixes: Vec<String>,
    next_step: String,
}

pub fn run(args: DoctorArgs, config_path: &str) -> Result<i32> {
    let config = load_config_checked(config_path)?;
    let base = args.base_ref.as_deref().unwrap_or(&config.diff.base);
    let head = args.head_ref.as_deref().unwrap_or(&config.diff.head);

    ensure_git_available()?;
    validate_git_refs(base, head)?;

    let repo_root = crate::cmd_common::detect_repo_root()?;
    let config_path_abs = std::fs::canonicalize(Path::new(config_path))
        .unwrap_or_else(|_| Path::new(config_path).to_path_buf());
    let config_base_dir = config_path_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let report_files = crate::cmd_common::resolve_report_globs_for_config(
        config_path,
        &config.ingest.report_paths,
    )?;
    if report_files.is_empty() {
        if args.json {
            let summary = DoctorSummary {
                config_path: config_path_abs.display().to_string(),
                config_base_dir: config_base_dir.display().to_string(),
                repo_root: repo_root.display().to_string(),
                report_files: 0,
                parsed_report_paths: 0,
                mapped: 0,
                total: 0,
                mapped_pct: 0.0,
                unmapped_prefixes: Vec::new(),
                suggested_strip_prefixes: Vec::new(),
                next_step: "configure [ingest].report_paths and run covy map-paths --learn --write"
                    .to_string(),
            };
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            println!("Repo root: {}", repo_root.display());
            println!("No report files matched [ingest].report_paths");
            println!(
                "Next: configure [ingest].report_paths and run covy map-paths --learn --write"
            );
        }
        return Ok(0);
    }

    let report_paths = parse_report_paths_quick(&report_files)?;

    let stats = evaluate_mapping(&report_paths, &config, &repo_root)?;
    let pct = if stats.total == 0 {
        0.0
    } else {
        (stats.mapped as f64 / stats.total as f64) * 100.0
    };

    if args.json {
        let summary = DoctorSummary {
            config_path: config_path_abs.display().to_string(),
            config_base_dir: config_base_dir.display().to_string(),
            repo_root: repo_root.display().to_string(),
            report_files: report_files.len(),
            parsed_report_paths: report_paths.len(),
            mapped: stats.mapped,
            total: stats.total,
            mapped_pct: pct,
            unmapped_prefixes: stats.unmapped_prefixes.clone(),
            suggested_strip_prefixes: stats.suggested_strip_prefixes.clone(),
            next_step: "run covy map-paths --learn --write".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(0);
    }

    println!("Repo root: {}", repo_root.display());
    println!("Parsed reports: {} files", report_paths.len());
    if stats.total == 0 {
        println!("Mapped paths: 0/0 (0.0%)");
        println!("No file paths were extracted from reports.");
        return Ok(0);
    }

    println!("Mapped paths: {}/{} ({pct:.1}%)", stats.mapped, stats.total);

    if !stats.unmapped_prefixes.is_empty() {
        println!("Unmapped prefixes (top):");
        for (prefix, count) in stats.unmapped_prefixes.iter().take(5) {
            println!("  - {prefix} ({count})");
        }
    }

    if !stats.suggested_strip_prefixes.is_empty() {
        let joined = stats
            .suggested_strip_prefixes
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Suggested rule: strip_prefix += [{joined}]");
    }

    println!("Next: run covy map-paths --learn --write");
    Ok(0)
}

fn load_config_checked(config_path: &str) -> Result<CovyConfig> {
    CovyConfig::load(Path::new(config_path))
        .with_context(|| format!("Invalid config at {config_path}"))
        .map_err(Into::into)
}

fn ensure_git_available() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .context("git is not available in PATH")?;
    if !output.status.success() {
        anyhow::bail!("git command is unavailable");
    }
    Ok(())
}

fn validate_git_refs(base: &str, head: &str) -> Result<()> {
    for r in [base, head] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", r])
            .output()
            .with_context(|| format!("Failed to resolve git ref '{r}'"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to resolve git ref '{r}': {stderr}");
        }
    }
    Ok(())
}

fn parse_report_paths_quick(report_files: &[PathBuf]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for report in report_files {
        let coverage = covy_ingest::ingest_path(report)
            .with_context(|| format!("Failed to parse report {}", report.display()))?;
        paths.extend(coverage.files.keys().cloned());
    }
    Ok(paths)
}

fn evaluate_mapping(
    report_paths: &[String],
    config: &CovyConfig,
    repo_root: &Path,
) -> Result<MappingStats> {
    let snapshot = covy_core::snapshot::build_snapshot(repo_root)?;
    let repo_paths: Vec<String> = snapshot.file_hashes.keys().cloned().collect();
    let known_refs: Vec<&str> = repo_paths.iter().map(|s| s.as_str()).collect();

    let mut replace_rules: BTreeMap<String, String> = config.path_mapping.rules.clone();
    for rule in &config.paths.replace_prefix {
        replace_rules.insert(rule.from.clone(), rule.to.clone());
    }

    let mut strip_prefixes = config.paths.strip_prefix.clone();
    strip_prefixes.extend(config.ingest.strip_prefixes.clone());

    let mut mapper = covy_core::pathmap::PathMapper::with_options(
        strip_prefixes,
        replace_rules,
        config.paths.ignore_globs.clone(),
        config.paths.case_sensitive,
        Some(&snapshot),
    );

    let mut mapped = 0usize;
    let mut unmapped: Vec<String> = Vec::new();

    for path in report_paths {
        let normalized = normalize_path(path);
        if mapper.resolve(&normalized, &known_refs).is_some() {
            mapped += 1;
        } else {
            unmapped.push(normalized);
        }
    }

    let unmapped_prefixes = top_prefixes(&unmapped);
    let suggested_strip_prefixes =
        infer_strip_prefixes(report_paths, &repo_paths, config.paths.case_sensitive);

    Ok(MappingStats {
        mapped,
        total: report_paths.len(),
        unmapped_prefixes,
        suggested_strip_prefixes,
    })
}

fn infer_strip_prefixes(
    report_paths: &[String],
    repo_paths: &[String],
    case_sensitive: bool,
) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for report in report_paths {
        let report = normalize_path(report);
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
    ranked.into_iter().take(3).map(|(p, _)| p).collect()
}

fn best_suffix_match<'a>(
    path: &str,
    repo_paths: &'a [String],
    case_sensitive: bool,
) -> Option<(&'a str, usize)> {
    let mut best: Option<(&str, usize)> = None;
    for repo in repo_paths {
        let repo_norm = normalize_path(repo);
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

fn first_two_segments(path: &str) -> String {
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let first = parts.next().unwrap_or(path);
    let second = parts.next();
    if let Some(second) = second {
        format!("{first}/{second}")
    } else {
        first.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_top_prefixes_ranks_deterministically() {
        let paths = vec![
            "/__w/repo/repo/src/main.rs".to_string(),
            "/__w/repo/repo/src/lib.rs".to_string(),
            "/workspace/app/src/a.rs".to_string(),
        ];

        let ranked = top_prefixes(&paths);
        assert_eq!(ranked[0], ("__w/repo".to_string(), 2));
    }

    #[test]
    fn test_infer_strip_prefixes_from_suffix_matches() {
        let report_paths = vec![
            "/__w/repo/repo/src/main.rs".to_string(),
            "/__w/repo/repo/src/lib.rs".to_string(),
        ];
        let repo_paths = vec!["src/main.rs".to_string(), "src/lib.rs".to_string()];

        let suggestions = infer_strip_prefixes(&report_paths, &repo_paths, true);
        assert_eq!(suggestions, vec!["/__w/repo/repo".to_string()]);
    }

    #[test]
    fn test_load_config_checked_reports_precise_path() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("broken.toml");
        std::fs::write(&config_path, "[impact\nmax_tests = 10").unwrap();

        let err = load_config_checked(config_path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Invalid config at"));
        assert!(err.to_string().contains("broken.toml"));
    }
}
