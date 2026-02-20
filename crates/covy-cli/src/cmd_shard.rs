use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use covy_core::CovyConfig;

#[derive(Args)]
pub struct ShardArgs {
    #[command(subcommand)]
    pub command: ShardCommands,
}

#[derive(Subcommand)]
pub enum ShardCommands {
    /// Plan test shards for CI runners
    Plan(ShardPlanArgs),
}

#[derive(Args)]
pub struct ShardPlanArgs {
    /// Number of shards
    #[arg(long)]
    pub shards: usize,

    /// Input tests file
    #[arg(long)]
    pub tests_file: Option<String>,

    /// Impact JSON output file (selected_tests field)
    #[arg(long)]
    pub impact_json: Option<String>,

    /// Timing history path
    #[arg(long)]
    pub timings: Option<String>,

    /// Fallback duration (seconds) for unknown tests
    #[arg(long)]
    pub unknown_test_seconds: Option<f64>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Directory for shard output files
    #[arg(long)]
    pub write_files: Option<String>,
}

pub fn run(args: ShardArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    match args.command {
        ShardCommands::Plan(plan) => {
            let tests = load_tests(&plan)?;
            if tests.is_empty() {
                anyhow::bail!("No tests provided for shard planning");
            }
            let timings_path = plan
                .timings
                .as_deref()
                .unwrap_or(&config.shard.timings_path)
                .to_string();
            let timings = load_timings(Path::new(&timings_path))?;
            let unknown_seconds = plan
                .unknown_test_seconds
                .unwrap_or(config.shard.unknown_test_seconds);
            let unknown_ms = (unknown_seconds * 1000.0) as u64;
            let jobs = covy_core::shard::build_timed_jobs(&tests, &timings, unknown_ms);
            let shard_plan = covy_core::shard::plan_shards_lpt(&jobs, plan.shards);

            if let Some(dir) = plan.write_files.as_deref() {
                write_shard_files(dir, &shard_plan)?;
            }

            if plan.json {
                println!("{}", serde_json::to_string_pretty(&shard_plan)?);
            } else {
                render_text(&shard_plan);
            }

            Ok(0)
        }
    }
}

fn load_tests(args: &ShardPlanArgs) -> Result<Vec<String>> {
    match (&args.tests_file, &args.impact_json) {
        (Some(path), None) => load_tests_from_file(Path::new(path)),
        (None, Some(path)) => load_tests_from_impact_json(Path::new(path)),
        (Some(_), Some(_)) => anyhow::bail!("Provide either --tests-file or --impact-json, not both"),
        (None, None) => anyhow::bail!("One of --tests-file or --impact-json is required"),
    }
}

fn load_tests_from_file(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read tests file {}", path.display()))?;
    let tests = content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    Ok(tests)
}

fn load_tests_from_impact_json(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read impact JSON {}", path.display()))?;
    let impact: covy_core::impact::ImpactResult =
        serde_json::from_str(&content).context("Failed to parse impact JSON")?;
    Ok(impact.selected_tests)
}

fn load_timings(path: &Path) -> Result<covy_core::testmap::TestTimingHistory> {
    if !path.exists() {
        return Ok(covy_core::testmap::TestTimingHistory::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read timings file {}", path.display()))?;
    covy_core::cache::deserialize_test_timings(&bytes).map_err(Into::into)
}

fn write_shard_files(dir: &str, plan: &covy_core::shard::ShardPlan) -> Result<()> {
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    for shard in &plan.shards {
        let path = dir.join(format!("shard-{}.txt", shard.id + 1));
        let content = if shard.tests.is_empty() {
            String::new()
        } else {
            format!("{}\n", shard.tests.join("\n"))
        };
        std::fs::write(path, content)?;
    }
    Ok(())
}

fn render_text(plan: &covy_core::shard::ShardPlan) {
    println!(
        "shards={} total_ms={} makespan_ms={}",
        plan.shards.len(),
        plan.total_predicted_duration_ms,
        plan.makespan_ms
    );
    for shard in &plan.shards {
        println!(
            "shard={} tests={} predicted_ms={}",
            shard.id + 1,
            shard.tests.len(),
            shard.predicted_duration_ms
        );
        for test in &shard.tests {
            println!("{test}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tests_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tests.txt");
        std::fs::write(&path, "a\n\nb\n").unwrap();
        let tests = load_tests_from_file(&path).unwrap();
        assert_eq!(tests, vec!["a".to_string(), "b".to_string()]);
    }
}
