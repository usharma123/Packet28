use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::path_diagnose::{
    explain_path_mapping, learn_path_mapping, load_repo_paths, PathExplainRequest,
    PathExplainResponse, PathLearnRequest, PathLearnResponse,
};
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

pub fn run(args: MapPathsArgs, config_path: &str) -> Result<i32> {
    if !args.learn && args.explain.is_none() {
        anyhow::bail!("Specify at least one of --learn or --explain");
    }

    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let repo_root = std::env::current_dir()?;
    let repo_files = load_repo_paths(&repo_root)?;

    let mut learned_result: Option<PathLearnResponse> = None;
    let mut explain_result: Option<PathExplainResponse> = None;
    let mut wrote_config = false;

    if args.learn {
        let report_files = if args.paths.is_empty() {
            crate::cmd_common::resolve_report_globs_for_config(
                config_path,
                &config.ingest.report_paths,
            )?
        } else {
            crate::cmd_common::resolve_report_globs(&args.paths)?
        };
        if report_files.is_empty() {
            anyhow::bail!(
                "No report files found. Provide --paths <globs> or configure [ingest].report_paths in covy.toml."
            );
        }

        let observed_paths = load_report_paths(&report_files)?;
        let learned = learn_path_mapping(PathLearnRequest::new(
            observed_paths,
            repo_files.clone(),
            config.paths.case_sensitive,
        ))?;

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
        let explanation =
            explain_path_mapping(PathExplainRequest::from_config(path, repo_files, &config))?;
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
            learn: Option<PathLearnResponse>,
            explain: Option<PathExplainResponse>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
