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
    pub imbalance_ratio: f64,
    pub parallel_efficiency: f64,
    pub whale_count: usize,
    pub top_10_share: f64,
}
