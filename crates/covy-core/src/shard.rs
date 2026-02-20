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
}
