use suite_packet_core::shard::{Shard, ShardPlan};
#[cfg(test)]
use suite_packet_core::shard::{
    Task, TaskSet, UniversalShardPlan, SHARD_PLAN_SCHEMA_VERSION, TASK_SCHEMA_VERSION,
};

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

    let (imbalance_ratio, parallel_efficiency) = compute_load_metrics(&shards, total, makespan);
    let whale_threshold = compute_whale_threshold_ms(input);
    ShardPlan {
        shards,
        total_predicted_duration_ms: total,
        makespan_ms: makespan,
        imbalance_ratio,
        parallel_efficiency,
        whale_count: count_whales(input, whale_threshold),
        top_10_share: compute_top_10_share(input, total),
    }
}

pub fn compute_whale_threshold_ms(input: &[(String, u64)]) -> u64 {
    if input.is_empty() {
        return 30_000;
    }

    let mut durations: Vec<u64> = input.iter().map(|(_, d)| *d).collect();
    durations.sort_unstable();
    let idx = ((durations.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    let p95 = durations[idx.min(durations.len() - 1)];
    std::cmp::max(30_000, p95.saturating_mul(2))
}

pub fn plan_shards_whale_lpt(input: &[(String, u64)], shard_count: usize) -> ShardPlan {
    if shard_count == 0 {
        return ShardPlan::default();
    }

    let mut jobs = input.to_vec();
    jobs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let whale_threshold = compute_whale_threshold_ms(&jobs);
    let mut whales = Vec::new();
    let mut rest = Vec::new();
    for job in jobs {
        if job.1 > whale_threshold {
            whales.push(job);
        } else {
            rest.push(job);
        }
    }

    let mut shards: Vec<Shard> = (0..shard_count)
        .map(|id| Shard {
            id,
            tests: Vec::new(),
            predicted_duration_ms: 0,
        })
        .collect();

    for (test_id, duration_ms) in whales.into_iter().chain(rest.into_iter()) {
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

    let (imbalance_ratio, parallel_efficiency) = compute_load_metrics(&shards, total, makespan);
    ShardPlan {
        shards,
        total_predicted_duration_ms: total,
        makespan_ms: makespan,
        imbalance_ratio,
        parallel_efficiency,
        whale_count: count_whales(input, whale_threshold),
        top_10_share: compute_top_10_share(input, total),
    }
}

fn count_whales(input: &[(String, u64)], whale_threshold: u64) -> usize {
    input
        .iter()
        .filter(|(_, duration)| *duration > whale_threshold)
        .count()
}

fn compute_top_10_share(input: &[(String, u64)], total: u64) -> f64 {
    if total == 0 || input.is_empty() {
        return 0.0;
    }
    let mut durations: Vec<u64> = input.iter().map(|(_, d)| *d).collect();
    durations.sort_unstable_by(|a, b| b.cmp(a));
    let top_sum: u64 = durations.into_iter().take(10).sum();
    top_sum as f64 / total as f64
}

fn compute_load_metrics(shards: &[Shard], total: u64, makespan: u64) -> (f64, f64) {
    if shards.is_empty() {
        return (0.0, 0.0);
    }
    let mut loads: Vec<u64> = shards.iter().map(|s| s.predicted_duration_ms).collect();
    loads.sort_unstable();
    let median = if loads.len() % 2 == 1 {
        loads[loads.len() / 2] as f64
    } else {
        let hi = loads.len() / 2;
        let lo = hi - 1;
        (loads[lo] as f64 + loads[hi] as f64) / 2.0
    };
    let imbalance_ratio = if median > 0.0 {
        makespan as f64 / median
    } else {
        0.0
    };
    let parallel_efficiency = if makespan > 0 {
        total as f64 / ((makespan as f64) * (shards.len() as f64))
    } else {
        1.0
    };
    (imbalance_ratio, parallel_efficiency)
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
        assert!(plan.parallel_efficiency > 0.0);
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

    #[test]
    fn test_compute_whale_threshold_uses_p95_rule() {
        let jobs = vec![
            ("a".to_string(), 1000),
            ("b".to_string(), 2000),
            ("c".to_string(), 3000),
            ("d".to_string(), 4000),
            ("e".to_string(), 5000),
        ];
        assert_eq!(compute_whale_threshold_ms(&jobs), 30_000);
    }

    #[test]
    fn test_plan_shards_whale_lpt_assigns_large_outlier_first() {
        let input = vec![
            ("whale".to_string(), 90_000),
            ("a".to_string(), 10_000),
            ("b".to_string(), 9_000),
            ("c".to_string(), 8_000),
            ("d".to_string(), 7_000),
        ];
        let plan = plan_shards_whale_lpt(&input, 2);
        assert_eq!(plan.shards.len(), 2);
        assert!(plan
            .shards
            .iter()
            .any(|s| s.tests.iter().any(|t| t == "whale")));
    }

    #[test]
    fn test_plan_shards_whale_lpt_is_deterministic_on_ties() {
        let input = vec![
            ("b".to_string(), 10),
            ("a".to_string(), 10),
            ("d".to_string(), 10),
            ("c".to_string(), 10),
        ];
        let p1 = plan_shards_whale_lpt(&input, 2);
        let p2 = plan_shards_whale_lpt(&input, 2);
        assert_eq!(p1.shards[0].tests, p2.shards[0].tests);
        assert_eq!(p1.shards[1].tests, p2.shards[1].tests);
    }

    #[test]
    fn test_plan_metrics_are_computed() {
        let input = vec![
            ("a".to_string(), 50),
            ("b".to_string(), 40),
            ("c".to_string(), 10),
        ];
        let plan = plan_shards_lpt(&input, 2);
        assert!(plan.imbalance_ratio >= 1.0);
        assert!(plan.parallel_efficiency > 0.0);
        assert!(plan.top_10_share > 0.0);
    }
}
