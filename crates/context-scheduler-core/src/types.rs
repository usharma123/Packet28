use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScheduleBudget {
    pub token_cap: Option<u64>,
    pub byte_cap: Option<usize>,
    pub runtime_ms_cap: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StepEstimate {
    pub tokens: u64,
    pub bytes: usize,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleStep {
    pub id: String,
    pub target: String,
    pub depends_on: Vec<String>,
    pub estimate: StepEstimate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleRequest {
    pub steps: Vec<ScheduleStep>,
    pub budget: ScheduleBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedStep {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleResult {
    pub ordered_steps: Vec<ScheduleStep>,
    pub skipped_steps: Vec<SkippedStep>,
    pub estimated_usage: StepEstimate,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScheduleMutation {
    Cancel { step_id: String, reason: String },
    Replace { step: ScheduleStep, reason: String },
    Append { step: ScheduleStep, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedMutation {
    pub kind: String,
    pub step_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationResult {
    pub steps: Vec<ScheduleStep>,
    pub applied: Vec<AppliedMutation>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScheduleError {
    #[error("step id cannot be empty")]
    EmptyStepId,

    #[error("duplicate step id '{id}'")]
    DuplicateStepId { id: String },

    #[error("step '{step_id}' depends on unknown step '{depends_on}'")]
    UnknownDependency { step_id: String, depends_on: String },

    #[error("dependency cycle detected in scheduler request")]
    DependencyCycle,
}
