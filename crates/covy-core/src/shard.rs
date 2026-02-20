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
