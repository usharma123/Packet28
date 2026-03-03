use std::collections::{HashMap, VecDeque};

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

pub fn schedule(request: ScheduleRequest) -> Result<ScheduleResult, ScheduleError> {
    validate_request(&request)?;
    let topo = topological_order(&request.steps)?;

    let mut ordered_steps = Vec::new();
    let mut skipped_steps = Vec::new();
    let mut accepted = HashMap::<String, bool>::new();
    let mut usage = StepEstimate::default();
    let mut budget_exhausted = false;

    for (index, step) in topo.iter().enumerate() {
        if step
            .depends_on
            .iter()
            .any(|dep| !accepted.get(dep).copied().unwrap_or(false))
        {
            skipped_steps.push(SkippedStep {
                id: step.id.clone(),
                reason: "dependency_not_satisfied".to_string(),
            });
            accepted.insert(step.id.clone(), false);
            continue;
        }

        if exceeds_budget(usage, step.estimate, request.budget) {
            budget_exhausted = true;
            skipped_steps.push(SkippedStep {
                id: step.id.clone(),
                reason: "budget_exceeded".to_string(),
            });
            accepted.insert(step.id.clone(), false);

            for next in topo.iter().skip(index + 1) {
                skipped_steps.push(SkippedStep {
                    id: next.id.clone(),
                    reason: "budget_exceeded".to_string(),
                });
                accepted.insert(next.id.clone(), false);
            }
            break;
        }

        usage = StepEstimate {
            tokens: usage.tokens.saturating_add(step.estimate.tokens),
            bytes: usage.bytes.saturating_add(step.estimate.bytes),
            runtime_ms: usage.runtime_ms.saturating_add(step.estimate.runtime_ms),
        };
        ordered_steps.push(step.clone());
        accepted.insert(step.id.clone(), true);
    }

    Ok(ScheduleResult {
        ordered_steps,
        skipped_steps,
        estimated_usage: usage,
        budget_exhausted,
    })
}

fn validate_request(request: &ScheduleRequest) -> Result<(), ScheduleError> {
    let mut known = HashMap::<String, ()>::new();
    for step in &request.steps {
        let id = step.id.trim();
        if id.is_empty() {
            return Err(ScheduleError::EmptyStepId);
        }
        if known.insert(id.to_string(), ()).is_some() {
            return Err(ScheduleError::DuplicateStepId { id: id.to_string() });
        }
    }

    for step in &request.steps {
        for dep in &step.depends_on {
            if !known.contains_key(dep) {
                return Err(ScheduleError::UnknownDependency {
                    step_id: step.id.clone(),
                    depends_on: dep.clone(),
                });
            }
        }
    }
    Ok(())
}

fn topological_order(steps: &[ScheduleStep]) -> Result<Vec<ScheduleStep>, ScheduleError> {
    let mut indegree = HashMap::<String, usize>::new();
    let mut outgoing = HashMap::<String, Vec<String>>::new();
    let mut steps_by_id = HashMap::<String, ScheduleStep>::new();
    let mut first_seen_index = HashMap::<String, usize>::new();

    for (idx, step) in steps.iter().enumerate() {
        indegree.entry(step.id.clone()).or_insert(0);
        outgoing.entry(step.id.clone()).or_default();
        steps_by_id.insert(step.id.clone(), step.clone());
        first_seen_index.insert(step.id.clone(), idx);
    }

    for step in steps {
        for dep in &step.depends_on {
            *indegree.entry(step.id.clone()).or_insert(0) += 1;
            outgoing
                .entry(dep.clone())
                .or_default()
                .push(step.id.clone());
        }
    }

    let mut ready: Vec<_> = indegree
        .iter()
        .filter_map(|(id, d)| if *d == 0 { Some(id.clone()) } else { None })
        .collect();
    ready.sort_by_key(|id| first_seen_index.get(id).copied().unwrap_or(usize::MAX));

    let mut queue: VecDeque<_> = ready.into_iter().collect();
    let mut ordered = Vec::with_capacity(steps.len());

    while let Some(id) = queue.pop_front() {
        let step = steps_by_id
            .get(&id)
            .cloned()
            .expect("known step must exist");
        ordered.push(step);

        if let Some(children) = outgoing.get(&id) {
            for child in children {
                if let Some(indeg) = indegree.get_mut(child) {
                    *indeg -= 1;
                    if *indeg == 0 {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
    }

    if ordered.len() != steps.len() {
        return Err(ScheduleError::DependencyCycle);
    }

    Ok(ordered)
}

fn exceeds_budget(current: StepEstimate, add: StepEstimate, budget: ScheduleBudget) -> bool {
    if let Some(cap) = budget.token_cap {
        if current.tokens.saturating_add(add.tokens) > cap {
            return true;
        }
    }
    if let Some(cap) = budget.byte_cap {
        if current.bytes.saturating_add(add.bytes) > cap {
            return true;
        }
    }
    if let Some(cap) = budget.runtime_ms_cap {
        if current.runtime_ms.saturating_add(add.runtime_ms) > cap {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, depends_on: &[&str], tokens: u64) -> ScheduleStep {
        ScheduleStep {
            id: id.to_string(),
            target: format!("{id}.target"),
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
            estimate: StepEstimate {
                tokens,
                bytes: 0,
                runtime_ms: 0,
            },
        }
    }

    #[test]
    fn schedules_in_dependency_order() {
        let result = schedule(ScheduleRequest {
            steps: vec![
                step("b", &["a"], 1),
                step("a", &[], 1),
                step("c", &["b"], 1),
            ],
            budget: ScheduleBudget::default(),
        })
        .unwrap();

        let ids: Vec<_> = result.ordered_steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(result.skipped_steps.is_empty());
    }

    #[test]
    fn rejects_unknown_dependency() {
        let err = schedule(ScheduleRequest {
            steps: vec![step("a", &["missing"], 1)],
            budget: ScheduleBudget::default(),
        })
        .unwrap_err();
        assert!(matches!(
            err,
            ScheduleError::UnknownDependency {
                step_id,
                depends_on
            } if step_id == "a" && depends_on == "missing"
        ));
    }

    #[test]
    fn exits_early_when_budget_exhausted() {
        let result = schedule(ScheduleRequest {
            steps: vec![step("a", &[], 2), step("b", &["a"], 2), step("c", &[], 2)],
            budget: ScheduleBudget {
                token_cap: Some(3),
                byte_cap: None,
                runtime_ms_cap: None,
            },
        })
        .unwrap();

        assert_eq!(result.ordered_steps.len(), 1);
        assert!(result.budget_exhausted);
        assert_eq!(result.skipped_steps.len(), 2);
    }

    #[test]
    fn rejects_dependency_cycles() {
        let err = schedule(ScheduleRequest {
            steps: vec![step("a", &["b"], 1), step("b", &["a"], 1)],
            budget: ScheduleBudget::default(),
        })
        .unwrap_err();

        assert_eq!(err, ScheduleError::DependencyCycle);
    }
}
