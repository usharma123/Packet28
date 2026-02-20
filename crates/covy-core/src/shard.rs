pub const TASK_SCHEMA_VERSION: u16 = 1;
pub const SHARD_PLAN_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub selector: String,
    pub est_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub splittable: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskSet {
    #[serde(default = "default_task_schema_version")]
    pub schema_version: u16,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Default for TaskSet {
    fn default() -> Self {
        Self {
            schema_version: TASK_SCHEMA_VERSION,
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PlannedTask {
    pub id: String,
    pub selector: String,
    pub est_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PlannedShard {
    pub id: usize,
    #[serde(default)]
    pub tasks: Vec<PlannedTask>,
    pub predicted_duration_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UniversalShardPlan {
    #[serde(default = "default_shard_plan_schema_version")]
    pub schema_version: u16,
    pub algorithm: String,
    #[serde(default)]
    pub shards: Vec<PlannedShard>,
}

impl Default for UniversalShardPlan {
    fn default() -> Self {
        Self {
            schema_version: SHARD_PLAN_SCHEMA_VERSION,
            algorithm: "lpt".to_string(),
            shards: Vec::new(),
        }
    }
}

fn default_task_schema_version() -> u16 {
    TASK_SCHEMA_VERSION
}

fn default_shard_plan_schema_version() -> u16 {
    SHARD_PLAN_SCHEMA_VERSION
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Shard {
    pub id: usize,
    pub tests: Vec<String>,
    pub predicted_duration_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ShardPlan {
    pub shards: Vec<Shard>,
    pub total_predicted_duration_ms: u64,
    pub makespan_ms: u64,
}

pub fn build_timed_jobs(
    test_ids: &[String],
    timings: &crate::testmap::TestTimingHistory,
    unknown_test_duration_ms: u64,
) -> Vec<(String, u64)> {
    test_ids
        .iter()
        .map(|test_id| {
            let duration = timings
                .duration_ms
                .get(test_id)
                .copied()
                .unwrap_or(unknown_test_duration_ms);
            (test_id.clone(), duration)
        })
        .collect()
}

/// Longest-processing-time-first bin-packing planner.
pub fn plan_shards_lpt(input: &[(String, u64)], shard_count: usize) -> ShardPlan {
    if shard_count == 0 {
        return ShardPlan::default();
    }

    let mut jobs = input.to_vec();
    jobs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut shards: Vec<Shard> = (0..shard_count)
        .map(|id| Shard {
            id,
            tests: Vec::new(),
            predicted_duration_ms: 0,
        })
        .collect();

    for (test_id, duration_ms) in jobs {
        let target = shards
            .iter()
            .min_by(|a, b| {
                a.predicted_duration_ms
                    .cmp(&b.predicted_duration_ms)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .map(|s| s.id)
            .unwrap_or(0);

        if let Some(shard) = shards.get_mut(target) {
            shard.tests.push(test_id);
            shard.predicted_duration_ms = shard.predicted_duration_ms.saturating_add(duration_ms);
        }
    }

    let total = shards.iter().map(|s| s.predicted_duration_ms).sum();
    let makespan = shards
        .iter()
        .map(|s| s.predicted_duration_ms)
        .max()
        .unwrap_or(0);

    ShardPlan {
        shards,
        total_predicted_duration_ms: total,
        makespan_ms: makespan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_shards_lpt_balances_work() {
        let input = vec![
            ("t1".to_string(), 100),
            ("t2".to_string(), 90),
            ("t3".to_string(), 80),
            ("t4".to_string(), 70),
        ];
        let plan = plan_shards_lpt(&input, 2);
        assert_eq!(plan.shards.len(), 2);
        assert_eq!(plan.total_predicted_duration_ms, 340);
        assert!(plan.makespan_ms <= 170);
    }

    #[test]
    fn test_plan_shards_lpt_is_deterministic_on_ties() {
        let input = vec![
            ("b".to_string(), 10),
            ("a".to_string(), 10),
            ("d".to_string(), 10),
            ("c".to_string(), 10),
        ];
        let p1 = plan_shards_lpt(&input, 2);
        let p2 = plan_shards_lpt(&input, 2);
        assert_eq!(p1.shards[0].tests, p2.shards[0].tests);
        assert_eq!(p1.shards[1].tests, p2.shards[1].tests);
    }

    #[test]
    fn test_build_timed_jobs_uses_fallback_for_unknown_tests() {
        let mut timings = crate::testmap::TestTimingHistory::default();
        timings.duration_ms.insert("known".to_string(), 50);
        let jobs = build_timed_jobs(
            &["known".to_string(), "unknown".to_string()],
            &timings,
            8000,
        );
        assert_eq!(jobs[0], ("known".to_string(), 50));
        assert_eq!(jobs[1], ("unknown".to_string(), 8000));
    }

    #[test]
    fn test_taskset_defaults_schema_version() {
        let taskset: TaskSet = serde_json::from_str(r#"{"tasks":[]}"#).unwrap();
        assert_eq!(taskset.schema_version, TASK_SCHEMA_VERSION);
    }

    #[test]
    fn test_task_defaults_optional_fields() {
        let task: Task = serde_json::from_str(
            r#"{
                "id":"tests/test_mod.py::test_one",
                "selector":"tests/test_mod.py::test_one",
                "est_ms":1200
            }"#,
        )
        .unwrap();
        assert!(task.tags.is_empty());
        assert!(task.module.is_none());
        assert!(!task.splittable);
    }

    #[test]
    fn test_universal_shard_plan_defaults_schema_version() {
        let plan: UniversalShardPlan =
            serde_json::from_str(r#"{"algorithm":"lpt","shards":[]}"#).unwrap();
        assert_eq!(plan.schema_version, SHARD_PLAN_SCHEMA_VERSION);
    }
}
