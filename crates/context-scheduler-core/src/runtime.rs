use std::collections::{HashMap, VecDeque};

use crate::types::{
    AppliedMutation, MutationResult, ScheduleBudget, ScheduleError, ScheduleMutation,
    ScheduleRequest, ScheduleResult, ScheduleStep, SkippedStep, StepEstimate,
};

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

pub fn apply_mutations(
    steps: &[ScheduleStep],
    mutations: &[ScheduleMutation],
) -> Result<MutationResult, ScheduleError> {
    validate_request(&ScheduleRequest {
        steps: steps.to_vec(),
        budget: ScheduleBudget::default(),
    })?;

    let mut by_id = steps
        .iter()
        .cloned()
        .map(|step| (step.id.clone(), step))
        .collect::<HashMap<_, _>>();
    let mut order = steps.iter().map(|step| step.id.clone()).collect::<Vec<_>>();
    let mut applied = Vec::new();

    for mutation in mutations {
        match mutation {
            ScheduleMutation::Cancel { step_id, reason } => {
                if by_id.remove(step_id).is_some() {
                    order.retain(|id| id != step_id);
                    for step in by_id.values_mut() {
                        step.depends_on.retain(|dep| dep != step_id);
                    }
                    applied.push(AppliedMutation {
                        kind: "cancel".to_string(),
                        step_id: step_id.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            ScheduleMutation::Replace { step, reason } => {
                if by_id.contains_key(&step.id) {
                    by_id.insert(step.id.clone(), step.clone());
                    applied.push(AppliedMutation {
                        kind: "replace".to_string(),
                        step_id: step.id.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            ScheduleMutation::Append { step, reason } => {
                if !by_id.contains_key(&step.id) {
                    order.push(step.id.clone());
                    by_id.insert(step.id.clone(), step.clone());
                    applied.push(AppliedMutation {
                        kind: "append".to_string(),
                        step_id: step.id.clone(),
                        reason: reason.clone(),
                    });
                }
            }
        }
    }

    let result_steps = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect::<Vec<_>>();
    validate_request(&ScheduleRequest {
        steps: result_steps.clone(),
        budget: ScheduleBudget::default(),
    })?;
    topological_order(&result_steps)?;

    Ok(MutationResult {
        steps: result_steps,
        applied,
    })
}

fn validate_request(request: &ScheduleRequest) -> Result<(), ScheduleError> {
    let mut known = HashMap::<String, ()>::new();
    for step in &request.steps {
        if step.id.trim().is_empty() {
            return Err(ScheduleError::EmptyStepId);
        }
        if known.insert(step.id.clone(), ()).is_some() {
            return Err(ScheduleError::DuplicateStepId {
                id: step.id.clone(),
            });
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
