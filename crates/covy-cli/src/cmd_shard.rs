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

    /// Input tasks.json file
    #[arg(long)]
    pub tasks_json: Option<String>,

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
    let provided = [
        args.tasks_json.is_some(),
        args.tests_file.is_some(),
        args.impact_json.is_some(),
    ]
    .into_iter()
    .filter(|v| *v)
    .count();

    if provided != 1 {
        anyhow::bail!(
            "Provide exactly one of --tasks-json, --tests-file, or --impact-json"
        );
    }

    if let Some(path) = &args.tasks_json {
        return load_tests_from_tasks_json(Path::new(path));
    }
    if let Some(path) = &args.tests_file {
        return load_tests_from_file(Path::new(path));
    }
    load_tests_from_impact_json(Path::new(args.impact_json.as_deref().unwrap_or_default()))
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

fn load_tests_from_tasks_json(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read tasks JSON {}", path.display()))?;
    let tasks: covy_core::shard::TaskSet =
        serde_json::from_str(&content).context("Failed to parse tasks JSON")?;
    let ids = tasks
        .tasks
        .into_iter()
        .map(|task| task.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    Ok(ids)
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

    #[test]
    fn test_load_tests_from_impact_json_with_python_nodeids() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("impact.json");
        let payload = serde_json::json!({
            "selected_tests": ["tests/test_x.py::test_a", "tests/test_y.py::test_b"],
            "smoke_tests": [],
            "missing_mappings": [],
            "stale": false,
            "confidence": 1.0,
            "escalate_full_suite": false
        });
        std::fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();
        let tests = load_tests_from_impact_json(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                "tests/test_x.py::test_a".to_string(),
                "tests/test_y.py::test_b".to_string()
            ]
        );
    }

    #[test]
    fn test_load_tests_from_tasks_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tasks.json");
        let payload = serde_json::json!({
            "schema_version": 1,
            "tasks": [
                {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1200},
                {"id": "tests/test_mod.py::test_one", "selector": "tests/test_mod.py::test_one", "est_ms": 900}
            ]
        });
        std::fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();
        let tests = load_tests_from_tasks_json(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                "com.foo.BarTest".to_string(),
                "tests/test_mod.py::test_one".to_string()
            ]
        );
    }
}
