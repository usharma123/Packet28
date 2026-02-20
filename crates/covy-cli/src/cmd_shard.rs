use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Update timing history from runner timing artifacts
    Update(ShardUpdateArgs),
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

#[derive(Args)]
pub struct ShardUpdateArgs {
    /// JUnit XML timing inputs (supports globs)
    #[arg(long)]
    pub junit_xml: Vec<String>,

    /// Generic timing JSONL inputs (supports globs)
    #[arg(long)]
    pub timings_jsonl: Vec<String>,

    /// Timing history path
    #[arg(long)]
    pub timings: Option<String>,

    /// Optional JSON export path for the merged timings snapshot
    #[arg(long)]
    pub export_json: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, serde::Serialize)]
struct ShardUpdateSummary {
    observations_ingested: usize,
    tests_updated: usize,
    timings_path: String,
    exported_json: Option<String>,
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
        ShardCommands::Update(update) => {
            let timings_path = update
                .timings
                .as_deref()
                .unwrap_or(&config.shard.timings_path)
                .to_string();
            let mut timings = load_timings(Path::new(&timings_path))?;

            let junit_files = resolve_globs(&update.junit_xml)?;
            let jsonl_files = resolve_globs(&update.timings_jsonl)?;
            if junit_files.is_empty() && jsonl_files.is_empty() {
                anyhow::bail!("No timing inputs found. Provide --junit-xml and/or --timings-jsonl.");
            }

            let observations = load_timing_observations(&junit_files, &jsonl_files)?;
            if observations.is_empty() {
                anyhow::bail!("No timing observations found in provided inputs.");
            }

            let updated = apply_timing_observations(&mut timings, &observations);
            write_timings(Path::new(&timings_path), &timings)?;

            let exported_json = if let Some(path) = update.export_json.as_deref() {
                write_timings_json(Path::new(path), &timings)?;
                Some(path.to_string())
            } else {
                None
            };

            let summary = ShardUpdateSummary {
                observations_ingested: observations.len(),
                tests_updated: updated,
                timings_path,
                exported_json,
            };
            if update.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "timings updated: observations={} tests_updated={} timings_path={}",
                    summary.observations_ingested, summary.tests_updated, summary.timings_path
                );
                if let Some(path) = &summary.exported_json {
                    println!("timings exported: {path}");
                }
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

fn write_timings(path: &Path, timings: &covy_core::testmap::TestTimingHistory) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = covy_core::cache::serialize_test_timings(timings)?;
    std::fs::write(path, bytes)
        .with_context(|| format!("Failed to write timings file {}", path.display()))?;
    Ok(())
}

fn write_timings_json(path: &Path, timings: &covy_core::testmap::TestTimingHistory) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(timings)?;
    std::fs::write(path, json)
        .with_context(|| format!("Failed to write timings JSON {}", path.display()))?;
    Ok(())
}

fn resolve_globs(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .with_context(|| format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        if matches.is_empty() {
            tracing::warn!("No files matched pattern: {pattern}");
        }
        files.extend(matches);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn load_timing_observations(
    junit_files: &[PathBuf],
    jsonl_files: &[PathBuf],
) -> Result<Vec<crate::shard_timing::TimingObservation>> {
    let mut observations = Vec::new();
    for path in junit_files {
        observations.extend(crate::shard_timing::parse_junit_xml_file(path)?);
    }
    for path in jsonl_files {
        observations.extend(crate::shard_timing::parse_timing_jsonl_file(path)?);
    }
    Ok(observations)
}

fn apply_timing_observations(
    timings: &mut covy_core::testmap::TestTimingHistory,
    observations: &[crate::shard_timing::TimingObservation],
) -> usize {
    use std::collections::BTreeMap;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut grouped: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for observation in observations {
        grouped
            .entry(observation.test_id.as_str())
            .or_default()
            .push(observation.duration_ms);
    }

    for (test_id, durations) in &grouped {
        if durations.is_empty() {
            continue;
        }
        let new_count = durations.len() as u32;
        let new_total: u64 = durations.iter().sum();
        let new_avg = new_total / durations.len() as u64;

        let prev_count = timings.sample_count.get(*test_id).copied().unwrap_or(0);
        let prev_duration = timings.duration_ms.get(*test_id).copied().unwrap_or(new_avg);
        let merged_count = prev_count.saturating_add(new_count);
        let merged_duration = if merged_count == 0 {
            new_avg
        } else {
            (((prev_duration as u128 * prev_count as u128)
                + (new_avg as u128 * new_count as u128))
                / (merged_count as u128)) as u64
        };

        timings
            .duration_ms
            .insert((*test_id).to_string(), merged_duration);
        timings
            .sample_count
            .insert((*test_id).to_string(), merged_count);
        timings.last_seen.insert((*test_id).to_string(), now);
    }

    timings.generated_at = now;
    grouped.len()
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
        "shards={} total_ms={} makespan_ms={} imbalance_ratio={:.3} parallel_efficiency={:.3} whale_count={} top_10_share={:.3}",
        plan.shards.len(),
        plan.total_predicted_duration_ms,
        plan.makespan_ms,
        plan.imbalance_ratio,
        plan.parallel_efficiency,
        plan.whale_count,
        plan.top_10_share
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

    #[test]
    fn test_apply_timing_observations_updates_history() {
        let mut timings = covy_core::testmap::TestTimingHistory::default();
        timings.duration_ms.insert("t1".to_string(), 1000);
        timings.sample_count.insert("t1".to_string(), 2);
        let obs = vec![
            crate::shard_timing::TimingObservation {
                test_id: "t1".to_string(),
                duration_ms: 2000,
            },
            crate::shard_timing::TimingObservation {
                test_id: "t2".to_string(),
                duration_ms: 800,
            },
        ];
        let updated = apply_timing_observations(&mut timings, &obs);
        assert_eq!(updated, 2);
        assert!(timings.duration_ms.contains_key("t1"));
        assert!(timings.duration_ms.contains_key("t2"));
        assert_eq!(timings.sample_count["t1"], 3);
        assert_eq!(timings.sample_count["t2"], 1);
    }

    #[test]
    fn test_write_and_load_timings_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("timings.bin");
        let mut timings = covy_core::testmap::TestTimingHistory::default();
        timings.duration_ms.insert("t".to_string(), 123);
        timings.sample_count.insert("t".to_string(), 1);
        write_timings(&path, &timings).unwrap();
        let loaded = load_timings(&path).unwrap();
        assert_eq!(loaded.duration_ms.get("t"), Some(&123));
        assert_eq!(loaded.sample_count.get("t"), Some(&1));
    }
}
