use super::*;

#[derive(Debug, Clone)]
pub(crate) enum KernelPlanMutation {
    Cancel {
        step_id: String,
        reason: String,
    },
    Replace {
        step: KernelStepRequest,
        reason: String,
    },
    Append {
        step: KernelStepRequest,
        reason: String,
    },
}

pub(crate) fn policy_context_with_task_id(
    mut policy_context: Value,
    task_id: Option<&str>,
) -> Value {
    let Some(task_id) = task_id.filter(|task_id| !task_id.trim().is_empty()) else {
        return policy_context;
    };
    match &mut policy_context {
        Value::Object(map) => {
            map.entry("task_id".to_string())
                .or_insert_with(|| Value::String(task_id.to_string()));
            policy_context
        }
        Value::Null => json!({ "task_id": task_id }),
        other => json!({
            "task_id": task_id,
            "sequence_policy_context": other.clone(),
        }),
    }
}

pub(crate) fn kernel_step_estimate(
    step: &KernelStepRequest,
) -> context_scheduler_core::StepEstimate {
    let usage = usage_for_packets(&step.input_packets);
    context_scheduler_core::StepEstimate {
        tokens: usage.tokens,
        bytes: usage.bytes,
        runtime_ms: usage.runtime_ms,
    }
}

pub(crate) fn schedule_step_from_kernel(
    step: &KernelStepRequest,
) -> context_scheduler_core::ScheduleStep {
    context_scheduler_core::ScheduleStep {
        id: step.id.clone(),
        target: step.target.clone(),
        depends_on: step.depends_on.clone(),
        estimate: kernel_step_estimate(step),
    }
}

pub(crate) fn schedule_budget_remaining(
    budget: ExecutionBudget,
    consumed: context_scheduler_core::StepEstimate,
) -> context_scheduler_core::ScheduleBudget {
    context_scheduler_core::ScheduleBudget {
        token_cap: budget
            .token_cap
            .map(|cap| cap.saturating_sub(consumed.tokens)),
        byte_cap: budget
            .byte_cap
            .map(|cap| cap.saturating_sub(consumed.bytes)),
        runtime_ms_cap: budget
            .runtime_ms_cap
            .map(|cap| cap.saturating_sub(consumed.runtime_ms)),
    }
}

fn merge_focus_into_map_step(
    step: &KernelStepRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Option<KernelStepRequest> {
    if step.target != "mapy.repo"
        || (snapshot.focus_paths.is_empty() && snapshot.focus_symbols.is_empty())
    {
        return None;
    }

    let mut request: mapy_core::RepoMapRequest =
        serde_json::from_value(step.reducer_input.clone()).ok()?;
    let mut changed = false;
    for path in &snapshot.focus_paths {
        if !request.focus_paths.iter().any(|existing| existing == path) {
            request.focus_paths.push(path.clone());
            changed = true;
        }
    }
    for symbol in &snapshot.focus_symbols {
        if !request
            .focus_symbols
            .iter()
            .any(|existing| existing == symbol)
        {
            request.focus_symbols.push(symbol.clone());
            changed = true;
        }
    }
    if !changed {
        return None;
    }

    let mut replaced = step.clone();
    replaced.reducer_input = serde_json::to_value(request).ok()?;
    Some(replaced)
}

pub(crate) fn build_reactive_kernel_mutations(
    remaining: &[KernelStepRequest],
    original_steps: &[KernelStepRequest],
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    completed_success: &BTreeSet<String>,
    mode: ReactiveReplanMode,
    append_focused_map: bool,
    anchor_step_id: Option<&str>,
) -> Vec<KernelPlanMutation> {
    let mut mutations = Vec::new();

    for step in remaining {
        if snapshot
            .completed_steps
            .iter()
            .any(|completed| completed == &step.id)
        {
            mutations.push(KernelPlanMutation::Cancel {
                step_id: step.id.clone(),
                reason: "completed_step".to_string(),
            });
            continue;
        }
        if mode == ReactiveReplanMode::TaskAware
            && (!snapshot.changed_paths_since_checkpoint.is_empty()
                || !snapshot.changed_symbols_since_checkpoint.is_empty())
            && !step_affected_by_snapshot(step, snapshot)
        {
            mutations.push(KernelPlanMutation::Cancel {
                step_id: step.id.clone(),
                reason: "inputs_unchanged".to_string(),
            });
            continue;
        }
        if let Some(replaced) = merge_focus_into_map_step(step, snapshot) {
            mutations.push(KernelPlanMutation::Replace {
                step: replaced,
                reason: "focus_narrowed".to_string(),
            });
        }
    }

    if append_focused_map
        && (!snapshot.focus_paths.is_empty() || !snapshot.focus_symbols.is_empty())
        && !remaining.iter().any(|step| step.target == "mapy.repo")
    {
        if let Some(template) = original_steps
            .iter()
            .find(|step| step.target == "mapy.repo")
        {
            let appended_id = format!("{}__reactive_focus", template.id);
            if !remaining.iter().any(|step| step.id == appended_id)
                && !completed_success.contains(&appended_id)
                && !snapshot
                    .completed_steps
                    .iter()
                    .any(|step_id| step_id == &appended_id)
            {
                let mut appended = template.clone();
                appended.id = appended_id;
                appended.depends_on.retain(|dep| {
                    !completed_success.contains(dep)
                        && !snapshot.completed_steps.iter().any(|done| done == dep)
                });
                if let Some(anchor) = anchor_step_id {
                    if !appended.depends_on.iter().any(|dep| dep == anchor) {
                        appended.depends_on.push(anchor.to_string());
                    }
                }
                if let Some(replaced) = merge_focus_into_map_step(&appended, snapshot) {
                    mutations.push(KernelPlanMutation::Append {
                        step: replaced,
                        reason: "focus_followup".to_string(),
                    });
                }
            }
        }
    }

    mutations
}

fn step_affected_by_snapshot(
    step: &KernelStepRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> bool {
    let changed_paths = &snapshot.changed_paths_since_checkpoint;
    let changed_symbols = &snapshot.changed_symbols_since_checkpoint;
    let focus_changed = !snapshot.focus_paths.is_empty() || !snapshot.focus_symbols.is_empty();

    if let Some(reactive) = step.reactive.as_ref() {
        if reactive.rerun_on_focus_change && focus_changed {
            return true;
        }
        if !reactive.path_globs.is_empty() {
            let matched = changed_paths.iter().any(|path| {
                reactive.path_globs.iter().any(|glob| {
                    glob::Pattern::new(glob)
                        .map(|pattern| pattern.matches(path))
                        .unwrap_or(false)
                })
            });
            if reactive.skip_if_inputs_unchanged {
                return matched;
            }
            if matched {
                return true;
            }
        }
    }

    match step.target.as_str() {
        "mapy.repo" => {
            focus_changed
                || changed_paths.iter().any(|path| {
                    !(path.ends_with(".info")
                        || path.ends_with(".lcov")
                        || path.ends_with(".xml")
                        || path.contains("coverage")
                        || path.contains("report"))
                })
                || !changed_symbols.is_empty()
        }
        "diffy.analyze" | "testy.impact" => changed_paths.iter().any(|path| {
            !(path.ends_with(".info")
                || path.ends_with(".lcov")
                || path.ends_with(".xml")
                || path.contains("coverage")
                || path.contains("report")
                || path.ends_with(".log"))
        }),
        "contextq.correlate" | "contextq.assemble" | "contextq.manage" => {
            !changed_paths.is_empty() || !changed_symbols.is_empty() || focus_changed
        }
        "stacky.slice" | "buildy.reduce" => changed_paths.iter().any(|path| {
            path.ends_with(".log")
                || path.ends_with(".txt")
                || path.contains("report")
                || path.contains("diagnostic")
        }),
        target if target.contains("cover") || target.contains("guard") => !changed_paths.is_empty(),
        _ => true,
    }
}

pub(crate) fn to_schedule_mutations(
    mutations: &[KernelPlanMutation],
) -> Vec<context_scheduler_core::ScheduleMutation> {
    mutations
        .iter()
        .map(|mutation| match mutation {
            KernelPlanMutation::Cancel { step_id, reason } => {
                context_scheduler_core::ScheduleMutation::Cancel {
                    step_id: step_id.clone(),
                    reason: reason.clone(),
                }
            }
            KernelPlanMutation::Replace { step, reason } => {
                context_scheduler_core::ScheduleMutation::Replace {
                    step: schedule_step_from_kernel(step),
                    reason: reason.clone(),
                }
            }
            KernelPlanMutation::Append { step, reason } => {
                context_scheduler_core::ScheduleMutation::Append {
                    step: schedule_step_from_kernel(step),
                    reason: reason.clone(),
                }
            }
        })
        .collect()
}

pub(crate) fn apply_kernel_mutations(
    steps: &[KernelStepRequest],
    mutations: &[KernelPlanMutation],
) -> Vec<KernelStepRequest> {
    let mut by_id = steps
        .iter()
        .cloned()
        .map(|step| (step.id.clone(), step))
        .collect::<HashMap<_, _>>();
    let mut order = steps.iter().map(|step| step.id.clone()).collect::<Vec<_>>();

    for mutation in mutations {
        match mutation {
            KernelPlanMutation::Cancel { step_id, .. } => {
                if by_id.remove(step_id).is_some() {
                    order.retain(|id| id != step_id);
                    for step in by_id.values_mut() {
                        step.depends_on.retain(|dep| dep != step_id);
                    }
                }
            }
            KernelPlanMutation::Replace { step, .. } => {
                if by_id.contains_key(&step.id) {
                    by_id.insert(step.id.clone(), step.clone());
                }
            }
            KernelPlanMutation::Append { step, .. } => {
                if !by_id.contains_key(&step.id) {
                    order.push(step.id.clone());
                    by_id.insert(step.id.clone(), step.clone());
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

pub(crate) fn remove_satisfied_dependency(remaining: &mut [KernelStepRequest], completed_id: &str) {
    for step in remaining {
        step.depends_on.retain(|dep| dep != completed_id);
    }
}

pub(crate) fn remove_failed_dependents(
    remaining: &mut Vec<KernelStepRequest>,
    failed_id: &str,
) -> Vec<KernelStepRequest> {
    let mut removed = Vec::new();
    let mut failed = vec![failed_id.to_string()];
    while let Some(dep_id) = failed.pop() {
        let (mut newly_removed, kept): (Vec<_>, Vec<_>) = remaining
            .drain(..)
            .partition(|step| step.depends_on.iter().any(|dep| dep == &dep_id));
        for step in &newly_removed {
            failed.push(step.id.clone());
        }
        removed.append(&mut newly_removed);
        *remaining = kept;
    }
    removed
}

pub(crate) fn resolve_sequence_task_id(req: &KernelSequenceRequest) -> Option<String> {
    if let Some(task_id) = req
        .reactive
        .task_id
        .as_ref()
        .filter(|task_id| !task_id.trim().is_empty())
    {
        return Some(task_id.clone());
    }

    req.steps.iter().find_map(|step| {
        step.policy_context
            .get("task_id")
            .and_then(Value::as_str)
            .filter(|task_id| !task_id.trim().is_empty())
            .map(ToOwned::to_owned)
    })
}

pub(crate) fn record_replan_cancellations(
    steps: &[KernelStepRequest],
    applied: &[context_scheduler_core::AppliedMutation],
    skipped: &mut Vec<String>,
    step_results: &mut Vec<KernelStepResponse>,
) {
    for mutation in applied.iter().filter(|mutation| mutation.kind == "cancel") {
        if let Some(step) = steps.iter().find(|step| step.id == mutation.step_id) {
            skipped.push(step.id.clone());
            step_results.push(KernelStepResponse {
                id: step.id.clone(),
                target: step.target.clone(),
                status: "skipped".to_string(),
                response: None,
                failure: Some(KernelFailure {
                    code: mutation.reason.clone(),
                    message: format!("step skipped by replanning: {}", mutation.reason),
                    target: Some(step.target.clone()),
                }),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appended_focus_map_prunes_satisfied_dependencies() {
        let template = KernelStepRequest {
            id: "map".to_string(),
            target: "mapy.repo".to_string(),
            depends_on: vec!["done".to_string(), "pending".to_string()],
            reducer_input: serde_json::to_value(mapy_core::RepoMapRequest::default()).unwrap(),
            ..KernelStepRequest::default()
        };
        let snapshot = suite_packet_core::AgentSnapshotPayload {
            focus_paths: vec!["src/main.rs".to_string()],
            completed_steps: vec!["done".to_string()],
            ..suite_packet_core::AgentSnapshotPayload::default()
        };
        let completed_success = BTreeSet::from(["done".to_string()]);

        let mutations = build_reactive_kernel_mutations(
            &[],
            &[template],
            &snapshot,
            &completed_success,
            ReactiveReplanMode::TaskAware,
            true,
            Some("anchor"),
        );

        let KernelPlanMutation::Append { step, .. } = &mutations[0] else {
            panic!("expected appended map mutation");
        };
        assert_eq!(
            step.depends_on,
            vec!["pending".to_string(), "anchor".to_string()]
        );
    }
}
