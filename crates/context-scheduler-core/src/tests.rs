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

#[test]
fn uses_raw_step_ids_consistently_during_validation_and_sorting() {
    let result = schedule(ScheduleRequest {
        steps: vec![step(" a ", &[], 1), step("b", &[" a "], 1)],
        budget: ScheduleBudget::default(),
    })
    .unwrap();

    let ids = result
        .ordered_steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![" a ", "b"]);
}

#[test]
fn apply_mutations_supports_cancel_replace_and_append() {
    let steps = vec![step("a", &[], 1), step("b", &["a"], 1)];
    let result = apply_mutations(
        &steps,
        &[
            ScheduleMutation::Cancel {
                step_id: "a".to_string(),
                reason: "done".to_string(),
            },
            ScheduleMutation::Replace {
                step: ScheduleStep {
                    id: "b".to_string(),
                    target: "b.focused".to_string(),
                    depends_on: Vec::new(),
                    estimate: StepEstimate {
                        tokens: 2,
                        bytes: 0,
                        runtime_ms: 0,
                    },
                },
                reason: "focus".to_string(),
            },
            ScheduleMutation::Append {
                step: step("c", &["b"], 1),
                reason: "followup".to_string(),
            },
        ],
    )
    .unwrap();

    let ids = result
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["b", "c"]);
    assert_eq!(result.steps[0].target, "b.focused");
    assert_eq!(result.applied.len(), 3);
}
