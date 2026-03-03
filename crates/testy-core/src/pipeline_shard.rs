use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub type ShardError = anyhow::Error;

#[derive(Debug, Clone)]
pub struct ShardRequest {
    pub mode: ShardMode,
}

#[derive(Debug, Clone)]
pub enum ShardMode {
    Plan(ShardPlanRequest),
    Update(ShardUpdateRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardPlannerAlgorithm {
    Lpt,
    WhaleLpt,
}

impl ShardPlannerAlgorithm {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lpt => "lpt",
            Self::WhaleLpt => "whale-lpt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShardPlanRequest {
    pub shard_count: usize,
    pub tasks_json: Option<String>,
    pub tests_file: Option<String>,
    pub impact_json: Option<String>,
    pub tier: String,
    pub include_tag: Vec<String>,
    pub exclude_tag: Vec<String>,
    pub tier_exclude_tags_pr: Vec<String>,
    pub tier_exclude_tags_nightly: Vec<String>,
    pub timings_path: String,
    pub unknown_test_seconds: f64,
    pub algorithm: ShardPlannerAlgorithm,
    pub write_files: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShardUpdateRequest {
    pub junit_xml: Vec<String>,
    pub timings_jsonl: Vec<String>,
    pub timings_path: String,
    pub export_json: Option<String>,
    pub junit_id_granularity: crate::shard_timing::JunitIdGranularity,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShardPlanSummary {
    pub total_tasks: usize,
    pub selected_tasks: usize,
    pub filtered_tasks: usize,
    pub tier: String,
    pub algorithm: String,
    pub unknown_test_duration_ms: u64,
    pub timings_path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShardTimingSummary {
    pub observations_ingested: usize,
    pub tests_updated: usize,
    pub timings_path: String,
    pub exported_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShardResponse {
    pub shard_plan: Option<crate::shard::ShardPlan>,
    pub plan_summary: Option<ShardPlanSummary>,
    pub timing_summary: Option<ShardTimingSummary>,
    pub filtered_out: Vec<String>,
    pub selected_tests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanningTask {
    id: String,
    tags: Vec<String>,
}

pub fn run_shard(req: ShardRequest) -> Result<ShardResponse, ShardError> {
    match req.mode {
        ShardMode::Plan(plan) => run_plan(plan),
        ShardMode::Update(update) => run_update(update),
    }
}

fn run_plan(args: ShardPlanRequest) -> Result<ShardResponse> {
    let tasks = load_tasks(&args)?;
    if tasks.is_empty() {
        anyhow::bail!("No tasks provided for shard planning");
    }

    let total_tasks = tasks.len();
    let (tasks, filtered_out) = apply_tag_filters(tasks, &args)?;
    if tasks.is_empty() {
        anyhow::bail!("No tasks remained after applying tier/tag filters");
    }

    let tests: Vec<String> = tasks.into_iter().map(|task| task.id).collect();
    let timings = load_timings(Path::new(&args.timings_path))?;
    let unknown_ms = (args.unknown_test_seconds * 1000.0) as u64;
    let jobs = crate::shard::build_timed_jobs(&tests, &timings, unknown_ms);
    let shard_plan = match args.algorithm {
        ShardPlannerAlgorithm::Lpt => crate::shard::plan_shards_lpt(&jobs, args.shard_count),
        ShardPlannerAlgorithm::WhaleLpt => {
            crate::shard::plan_shards_whale_lpt(&jobs, args.shard_count)
        }
    };

    if let Some(dir) = args.write_files.as_deref() {
        write_shard_files(dir, &shard_plan)?;
    }

    Ok(ShardResponse {
        shard_plan: Some(shard_plan),
        plan_summary: Some(ShardPlanSummary {
            total_tasks,
            selected_tasks: tests.len(),
            filtered_tasks: filtered_out.len(),
            tier: args.tier,
            algorithm: args.algorithm.as_str().to_string(),
            unknown_test_duration_ms: unknown_ms,
            timings_path: args.timings_path,
        }),
        timing_summary: None,
        filtered_out,
        selected_tests: tests,
    })
}

fn run_update(args: ShardUpdateRequest) -> Result<ShardResponse> {
    let mut timings = load_timings(Path::new(&args.timings_path))?;

    let junit_files = resolve_globs(&args.junit_xml)?;
    let jsonl_files = resolve_globs(&args.timings_jsonl)?;
    if junit_files.is_empty() && jsonl_files.is_empty() {
        anyhow::bail!("No timing inputs found. Provide --junit-xml and/or --timings-jsonl.");
    }

    let observations =
        load_timing_observations(&junit_files, &jsonl_files, args.junit_id_granularity)?;
    if observations.is_empty() {
        anyhow::bail!("No timing observations found in provided inputs.");
    }

    let updated = apply_timing_observations(&mut timings, &observations);
    write_timings(Path::new(&args.timings_path), &timings)?;

    let exported_json = if let Some(path) = args.export_json.as_deref() {
        write_timings_json(Path::new(path), &timings)?;
        Some(path.to_string())
    } else {
        None
    };

    Ok(ShardResponse {
        shard_plan: None,
        plan_summary: None,
        timing_summary: Some(ShardTimingSummary {
            observations_ingested: observations.len(),
            tests_updated: updated,
            timings_path: args.timings_path,
            exported_json,
        }),
        filtered_out: Vec::new(),
        selected_tests: Vec::new(),
    })
}

fn load_tasks(args: &ShardPlanRequest) -> Result<Vec<PlanningTask>> {
    let provided = [
        args.tasks_json.is_some(),
        args.tests_file.is_some(),
        args.impact_json.is_some(),
    ]
    .into_iter()
    .filter(|v| *v)
    .count();

    if provided != 1 {
        anyhow::bail!("Provide exactly one of --tasks-json, --tests-file, or --impact-json");
    }

    if let Some(path) = &args.tasks_json {
        return load_tasks_from_tasks_json(Path::new(path));
    }
    if let Some(path) = &args.tests_file {
        return load_tasks_from_file(Path::new(path));
    }
    load_tasks_from_impact_json(Path::new(args.impact_json.as_deref().unwrap_or_default()))
}

fn load_tasks_from_file(path: &Path) -> Result<Vec<PlanningTask>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read tests file {}", path.display()))?;
    let tests = content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|id| PlanningTask {
            id: id.to_string(),
            tags: Vec::new(),
        })
        .collect();
    Ok(tests)
}

const IMPACT_RESULT_EXAMPLE: &str = r#"{
  "selected_tests": ["com.foo.BarTest", "tests/test_x.py::test_a"],
  "smoke_tests": [],
  "missing_mappings": [],
  "stale": false,
  "confidence": 1.0,
  "escalate_full_suite": false
}"#;

fn load_tasks_from_impact_json(path: &Path) -> Result<Vec<PlanningTask>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read impact JSON {}", path.display()))?;
    let impact: crate::impact::ImpactResult =
        deserialize_json_with_example(&content, "ImpactResult", IMPACT_RESULT_EXAMPLE)?;
    Ok(impact
        .selected_tests
        .into_iter()
        .map(|id| PlanningTask {
            id,
            tags: Vec::new(),
        })
        .collect())
}

const TASKSET_EXAMPLE: &str = r#"{
  "schema_version": 1,
  "tasks": [
    {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1200, "tags": ["unit"]},
    {"id": "tests/test_mod.py::test_one", "selector": "tests/test_mod.py::test_one", "est_ms": 900}
  ]
}"#;

fn load_tasks_from_tasks_json(path: &Path) -> Result<Vec<PlanningTask>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read tasks JSON {}", path.display()))?;
    let tasks: crate::shard::TaskSet =
        deserialize_json_with_example(&content, "TaskSet", TASKSET_EXAMPLE)?;
    let ids = tasks
        .tasks
        .into_iter()
        .map(|task| PlanningTask {
            id: task.id.trim().to_string(),
            tags: task.tags,
        })
        .filter(|task| !task.id.is_empty())
        .collect();
    Ok(ids)
}

fn apply_tag_filters(
    tasks: Vec<PlanningTask>,
    args: &ShardPlanRequest,
) -> Result<(Vec<PlanningTask>, Vec<String>)> {
    let tier = args.tier.trim().to_ascii_lowercase();
    let mut exclude: BTreeSet<String> = match tier.as_str() {
        "pr" => args
            .tier_exclude_tags_pr
            .iter()
            .map(normalize_tag)
            .collect(),
        "nightly" => args
            .tier_exclude_tags_nightly
            .iter()
            .map(normalize_tag)
            .collect(),
        _ => anyhow::bail!(
            "Unsupported tier '{}'. Expected 'pr' or 'nightly'",
            args.tier
        ),
    };
    exclude.extend(args.exclude_tag.iter().map(normalize_tag));
    let include: BTreeSet<String> = args.include_tag.iter().map(normalize_tag).collect();

    let mut kept = Vec::new();
    let mut filtered_out = Vec::new();

    for task in tasks {
        let task_tags: Vec<String> = task.tags.iter().map(normalize_tag).collect();
        let include_ok = include.is_empty() || task_tags.iter().any(|tag| include.contains(tag));
        let excluded = task_tags.iter().any(|tag| exclude.contains(tag));
        if include_ok && !excluded {
            kept.push(task);
        } else {
            filtered_out.push(task.id);
        }
    }

    Ok((kept, filtered_out))
}

fn normalize_tag(tag: impl AsRef<str>) -> String {
    tag.as_ref().trim().to_ascii_lowercase()
}

fn load_timings(path: &Path) -> Result<crate::testmap::TestTimingHistory> {
    if !path.exists() {
        return Ok(crate::testmap::TestTimingHistory::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read timings file {}", path.display()))?;
    crate::cache::deserialize_test_timings(&bytes).map_err(Into::into)
}

fn write_timings(path: &Path, timings: &crate::testmap::TestTimingHistory) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = crate::cache::serialize_test_timings(timings)?;
    std::fs::write(path, bytes)
        .with_context(|| format!("Failed to write timings file {}", path.display()))?;
    Ok(())
}

fn write_timings_json(path: &Path, timings: &crate::testmap::TestTimingHistory) -> Result<()> {
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
    junit_id_granularity: crate::shard_timing::JunitIdGranularity,
) -> Result<Vec<crate::shard_timing::TimingObservation>> {
    let mut observations = Vec::new();
    for path in junit_files {
        observations.extend(crate::shard_timing::parse_junit_xml_file(
            path,
            junit_id_granularity,
        )?);
    }
    for path in jsonl_files {
        observations.extend(crate::shard_timing::parse_timing_jsonl_file(path)?);
    }
    Ok(observations)
}

fn apply_timing_observations(
    timings: &mut crate::testmap::TestTimingHistory,
    observations: &[crate::shard_timing::TimingObservation],
) -> usize {
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
        let prev_duration = timings
            .duration_ms
            .get(*test_id)
            .copied()
            .unwrap_or(new_avg);
        let merged_count = prev_count.saturating_add(new_count);
        let merged_duration = if merged_count == 0 {
            new_avg
        } else {
            (((prev_duration as u128 * prev_count as u128) + (new_avg as u128 * new_count as u128))
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

fn write_shard_files(dir: &str, plan: &crate::shard::ShardPlan) -> Result<()> {
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

fn deserialize_json_with_example<T: serde::de::DeserializeOwned>(
    input: &str,
    type_name: &str,
    example: &str,
) -> anyhow::Result<T> {
    serde_json::from_str(input).map_err(|e| {
        anyhow::anyhow!("Failed to parse {type_name}: {e}\n\nExpected JSON shape:\n{example}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tests_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tests.txt");
        std::fs::write(&path, "a\n\nb\n").unwrap();
        let tests = load_tasks_from_file(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                PlanningTask {
                    id: "a".to_string(),
                    tags: Vec::new()
                },
                PlanningTask {
                    id: "b".to_string(),
                    tags: Vec::new()
                }
            ]
        );
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
        let tests = load_tasks_from_impact_json(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                PlanningTask {
                    id: "tests/test_x.py::test_a".to_string(),
                    tags: Vec::new()
                },
                PlanningTask {
                    id: "tests/test_y.py::test_b".to_string(),
                    tags: Vec::new()
                }
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
        let tests = load_tasks_from_tasks_json(&path).unwrap();
        assert_eq!(
            tests,
            vec![
                PlanningTask {
                    id: "com.foo.BarTest".to_string(),
                    tags: Vec::new()
                },
                PlanningTask {
                    id: "tests/test_mod.py::test_one".to_string(),
                    tags: Vec::new()
                }
            ]
        );
    }

    #[test]
    fn test_apply_tag_filters_pr_excludes_slow() {
        let tasks = vec![
            PlanningTask {
                id: "fast-test".to_string(),
                tags: vec!["unit".to_string()],
            },
            PlanningTask {
                id: "slow-test".to_string(),
                tags: vec!["slow".to_string()],
            },
        ];
        let req = ShardPlanRequest {
            shard_count: 2,
            tasks_json: None,
            tier: "pr".to_string(),
            include_tag: Vec::new(),
            exclude_tag: Vec::new(),
            tests_file: None,
            impact_json: None,
            tier_exclude_tags_pr: vec!["slow".to_string()],
            tier_exclude_tags_nightly: Vec::new(),
            timings_path: ".covy/state/testtimings.bin".to_string(),
            unknown_test_seconds: 8.0,
            algorithm: ShardPlannerAlgorithm::Lpt,
            write_files: None,
        };
        let (filtered, filtered_out) = apply_tag_filters(tasks, &req).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "fast-test");
        assert_eq!(filtered_out, vec!["slow-test".to_string()]);
    }

    #[test]
    fn test_apply_timing_observations_updates_history() {
        let mut timings = crate::testmap::TestTimingHistory::default();
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
        let mut timings = crate::testmap::TestTimingHistory::default();
        timings.duration_ms.insert("t".to_string(), 123);
        timings.sample_count.insert("t".to_string(), 1);
        write_timings(&path, &timings).unwrap();
        let loaded = load_timings(&path).unwrap();
        assert_eq!(loaded.duration_ms.get("t"), Some(&123));
        assert_eq!(loaded.sample_count.get("t"), Some(&1));
    }

    #[test]
    fn test_run_shard_plan_produces_plan_and_summary() {
        let dir = tempfile::TempDir::new().unwrap();
        let tests_file = dir.path().join("tests.txt");
        std::fs::write(&tests_file, "t1\nt2\n").unwrap();

        let response = run_shard(ShardRequest {
            mode: ShardMode::Plan(ShardPlanRequest {
                shard_count: 2,
                tasks_json: None,
                tests_file: Some(tests_file.to_string_lossy().to_string()),
                impact_json: None,
                tier: "nightly".to_string(),
                include_tag: Vec::new(),
                exclude_tag: Vec::new(),
                tier_exclude_tags_pr: vec!["slow".to_string()],
                tier_exclude_tags_nightly: Vec::new(),
                timings_path: dir.path().join("timings.bin").to_string_lossy().to_string(),
                unknown_test_seconds: 1.0,
                algorithm: ShardPlannerAlgorithm::Lpt,
                write_files: None,
            }),
        })
        .unwrap();

        assert!(response.shard_plan.is_some());
        assert!(response.plan_summary.is_some());
        assert_eq!(
            response.selected_tests,
            vec!["t1".to_string(), "t2".to_string()]
        );
    }

    #[test]
    fn test_run_shard_update_produces_timing_summary() {
        let dir = tempfile::TempDir::new().unwrap();
        let jsonl = dir.path().join("timings.jsonl");
        std::fs::write(
            &jsonl,
            "{\"test_id\":\"com.foo.BarTest\",\"duration_ms\":1200}\n",
        )
        .unwrap();

        let response = run_shard(ShardRequest {
            mode: ShardMode::Update(ShardUpdateRequest {
                junit_xml: Vec::new(),
                timings_jsonl: vec![jsonl.to_string_lossy().to_string()],
                timings_path: dir.path().join("timings.bin").to_string_lossy().to_string(),
                export_json: None,
                junit_id_granularity: crate::shard_timing::JunitIdGranularity::Method,
            }),
        })
        .unwrap();

        let summary = response.timing_summary.unwrap();
        assert_eq!(summary.observations_ingested, 1);
        assert_eq!(summary.tests_updated, 1);
        assert!(Path::new(&summary.timings_path).exists());
    }
}
