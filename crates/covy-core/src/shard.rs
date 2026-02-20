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

/// Build a list of (test_id, duration_ms) pairs from test IDs using a timing history,
/// falling back to `unknown_test_duration_ms` when a test has no recorded duration.
///
/// # Parameters
///
/// - `test_ids`: Ordered slice of test identifier strings to convert.
/// - `timings`: Timing history mapping test IDs to their duration in milliseconds.
/// - `unknown_test_duration_ms`: Duration to use for tests not present in `timings`.
///
/// # Returns
///
/// A `Vec<(String, u64)>` where each element is the test ID and its resolved duration in
/// milliseconds, preserving the order of `test_ids`.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
///
/// // Construct a timing history with one known test.
/// let timings = crate::testmap::TestTimingHistory {
///     duration_ms: HashMap::from([("known".to_string(), 50_u64)]),
/// };
///
/// let test_ids = vec!["known".to_string(), "unknown".to_string()];
/// let jobs = crate::shard::build_timed_jobs(&test_ids, &timings, 8000);
///
/// assert_eq!(jobs, vec![("known".to_string(), 50), ("unknown".to_string(), 8000)]);
/// ```
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

/// Assigns tests to shards using the Longest-Processing-Time-first (LPT) bin-packing heuristic.
///
/// Jobs are placed in descending order of duration; each job is assigned to the shard with the
/// smallest current predicted duration, with ties broken by the smaller shard id. If `shard_count`
/// is zero, returns an empty default `ShardPlan`.
///
/// # Returns
///
/// A `ShardPlan` containing per-shard assignments and aggregate timing metrics:
/// - `shards`: vector of `Shard` with `id`, assigned `tests`, and `predicted_duration_ms`
/// - `total_predicted_duration_ms`: sum of all shard predicted durations
/// - `makespan_ms`: maximum shard predicted duration
///
/// # Examples
///
/// ```
/// let jobs = vec![("a".to_string(), 100), ("b".to_string(), 50), ("c".to_string(), 75)];
/// let plan = plan_shards_lpt(&jobs, 2);
/// assert_eq!(plan.shards.len(), 2);
/// assert_eq!(plan.total_predicted_duration_ms, 225);
/// assert!(plan.makespan_ms <= 125);
/// ```
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
}