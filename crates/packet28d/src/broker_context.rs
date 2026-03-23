use super::*;
use crate::broker_handoff::{
    compute_handoff_state, next_action_summary, slim_broker_response, write_broker_artifacts,
};

pub(crate) fn compute_broker_response(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
) -> Result<BrokerGetContextResponse> {
    let started_at = Instant::now();
    let mut diagnostics_ms = BTreeMap::new();
    let snapshot_started = Instant::now();
    let snapshot = load_agent_snapshot_for_task(state, &request.task_id)?;
    diagnostics_ms.insert(
        "snapshot_load".to_string(),
        snapshot_started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    );
    let task = load_task_record(state, &request.task_id);
    let root = state.lock().map_err(lock_err)?.root.clone();
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let objective = broker_objective(state, request);
    let focus_symbols = derive_broker_focus_symbols(&snapshot, request);
    let focus_paths =
        derive_broker_focus_paths(state, &root, objective.as_deref(), &snapshot, request, 8)?;
    let version = current_context_version(state, &request.task_id)?;
    let action = request.action.unwrap_or(BrokerAction::Plan);
    let allowed_sections =
        filter_requested_section_ids(action, &request.include_sections, &request.exclude_sections);
    let needs_manage = allowed_sections.contains("relevant_context")
        || allowed_sections.contains("recommended_actions");
    let manage = if needs_manage {
        let manage_started = Instant::now();
        let payload = load_context_manage_for_task(&kernel, request, &focus_paths, &focus_symbols)?;
        diagnostics_ms.insert(
            "context_manage".to_string(),
            manage_started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        Some(payload)
    } else {
        diagnostics_ms.insert("context_manage".to_string(), 0);
        None
    };
    diagnostics_ms.insert("search_evidence".to_string(), 0);
    let effective_limits = resolve_effective_limits(
        action,
        request.verbosity,
        request.max_sections,
        request.default_max_items_per_section,
        &request.section_item_limits,
    );
    let section_build_started = Instant::now();
    let full_sections =
        build_broker_sections(&root, state, request, &snapshot, manage.as_ref(), None);
    diagnostics_ms.insert(
        "section_build".to_string(),
        section_build_started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    );
    let budget_tokens = request
        .budget_tokens
        .unwrap_or_else(broker_default_budget_tokens);
    let budget_bytes = request
        .budget_bytes
        .unwrap_or_else(broker_default_budget_bytes);
    let (selected_sections, budget_pruned_evictions) = prune_sections_for_budget(
        action,
        full_sections.clone(),
        budget_tokens,
        budget_bytes as u64,
        effective_limits.max_sections,
    );
    let selected_sections = postprocess_selected_sections(
        selected_sections,
        &budget_pruned_evictions,
        &snapshot,
        &effective_limits,
    );
    let previous_response = match request.since_version.as_deref() {
        Some(since_version) if since_version != version => {
            load_versioned_broker_response(&root, &request.task_id, since_version)?
        }
        _ => None,
    };
    let delta = build_delta(&selected_sections, previous_response.as_ref());
    let changed_ids = delta
        .changed_sections
        .iter()
        .map(|section| section.id.clone())
        .collect::<HashSet<_>>();
    let use_delta_view = should_use_delta_view(request, &delta, selected_sections.len());
    let sections = if use_delta_view {
        delta.changed_sections.clone()
    } else {
        selected_sections.clone()
    };
    let brief = render_brief(&request.task_id, &version, &sections);
    let (est_tokens, est_bytes) = estimate_text_cost(&brief);
    let resolved_questions = build_resolved_questions(task.as_ref(), &snapshot);
    let discovered_paths = merged_unique(
        &snapshot.focus_paths,
        &snapshot
            .read_paths_by_tool
            .iter()
            .flat_map(|summary| summary.paths.iter().cloned())
            .chain(
                snapshot
                    .edited_paths_by_tool
                    .iter()
                    .flat_map(|summary| summary.paths.iter().cloned()),
            )
            .collect::<Vec<_>>(),
    );
    let discovered_symbols = merged_unique(&snapshot.focus_symbols, &[]);
    let latest_intention = snapshot.latest_intention.clone();
    let next_action_summary = next_action_summary(manage.as_ref(), &snapshot);
    let (handoff_ready, _) = compute_handoff_state(task.as_ref(), &snapshot);
    let mut eviction_candidates = build_eviction_candidates(&selected_sections);
    eviction_candidates.extend(budget_pruned_evictions);
    eviction_candidates.sort_by(|a, b| {
        a.section_id
            .cmp(&b.section_id)
            .then_with(|| a.reason.cmp(&b.reason))
    });
    eviction_candidates.dedup_by(|a, b| a.section_id == b.section_id && a.reason == b.reason);
    Ok(BrokerGetContextResponse {
        stale: request
            .since_version
            .as_deref()
            .is_some_and(|since| since != version),
        invalidates_since_version: request
            .since_version
            .as_deref()
            .is_some_and(|since| since != version),
        context_version: version.clone(),
        response_mode: if use_delta_view {
            BrokerResponseMode::Delta
        } else {
            BrokerResponseMode::Full
        },
        artifact_id: None,
        latest_intention,
        next_action_summary,
        handoff_ready,
        brief,
        supersedes_prior_context: true,
        supersession_mode: BrokerSupersessionMode::Replace,
        superseded_before_version: version.clone(),
        sections: sections.clone(),
        est_tokens,
        est_bytes,
        budget_remaining_tokens: budget_tokens.saturating_sub(est_tokens),
        budget_remaining_bytes: (budget_bytes as u64).saturating_sub(est_bytes),
        section_estimates: build_section_estimates(&sections, &changed_ids),
        eviction_candidates,
        delta,
        working_set: manage
            .as_ref()
            .map(|manage| {
                manage
                    .working_set
                    .iter()
                    .map(|packet| BrokerPacketRef {
                        cache_key: packet.cache_key.clone(),
                        target: packet.target.clone(),
                        score: packet.score,
                        summary: packet.summary.clone(),
                        source_tier: packet.source_tier,
                        memory_kind: packet.memory_kind,
                        packet_types: packet.packet_types.clone(),
                        est_tokens: packet.est_tokens,
                        est_bytes: packet.est_bytes,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        recommended_actions: manage
            .as_ref()
            .map(|manage| {
                manage
                    .recommended_actions
                    .iter()
                    .map(|action| BrokerRecommendedAction {
                        kind: action.kind.clone(),
                        summary: action.summary.clone(),
                        related_paths: action.related_paths.clone(),
                        related_symbols: action.related_symbols.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        active_decisions: snapshot
            .active_decisions
            .iter()
            .map(|decision| BrokerDecision {
                id: decision.id.clone(),
                text: decision.text.clone(),
                resolves_question_id: task
                    .as_ref()
                    .and_then(|task| task.linked_decisions.get(&decision.id))
                    .cloned(),
            })
            .collect(),
        open_questions: snapshot
            .open_questions
            .iter()
            .map(|question| BrokerQuestion {
                id: question.id.clone(),
                text: question.text.clone(),
            })
            .collect(),
        resolved_questions,
        changed_paths_since_checkpoint: snapshot.changed_paths_since_checkpoint.clone(),
        changed_symbols_since_checkpoint: snapshot.changed_symbols_since_checkpoint.clone(),
        recent_tool_invocations: snapshot.recent_tool_invocations.clone(),
        tool_failures: snapshot.tool_failures.clone(),
        discovered_paths,
        discovered_symbols,
        evidence_artifact_ids: snapshot.evidence_artifact_ids.clone(),
        effective_max_sections: effective_limits.max_sections,
        effective_default_max_items_per_section: effective_limits.default_max_items_per_section,
        effective_section_item_limits: effective_limits.section_item_limits,
        diagnostics_ms: {
            diagnostics_ms.insert(
                "total".to_string(),
                started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            );
            diagnostics_ms
        },
    })
}

fn estimate_request_to_get_request(
    request: &BrokerEstimateContextRequest,
) -> BrokerGetContextRequest {
    BrokerGetContextRequest {
        task_id: request.task_id.clone(),
        action: request.action,
        budget_tokens: request.budget_tokens,
        budget_bytes: request.budget_bytes,
        since_version: request.since_version.clone(),
        focus_paths: request.focus_paths.clone(),
        focus_symbols: request.focus_symbols.clone(),
        tool_name: request.tool_name.clone(),
        tool_result_kind: request.tool_result_kind,
        query: request.query.clone(),
        include_sections: request.include_sections.clone(),
        exclude_sections: request.exclude_sections.clone(),
        verbosity: request.verbosity,
        response_mode: request.response_mode,
        include_self_context: request.include_self_context,
        max_sections: request.max_sections,
        default_max_items_per_section: request.default_max_items_per_section,
        section_item_limits: request.section_item_limits.clone(),
        persist_artifacts: request.persist_artifacts,
        recall_mode: request.recall_mode,
        include_debug_memory: request.include_debug_memory,
    }
}

pub(crate) fn refresh_broker_context_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    since_version: Option<String>,
) -> Result<Option<BrokerGetContextResponse>> {
    let request = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(task_id)
        .and_then(|task| task.latest_broker_request.clone());
    let Some(mut request) = request else {
        return Ok(None);
    };
    request.since_version = since_version.clone();
    request.response_mode = Some(BrokerResponseMode::Full);
    let mut response = compute_broker_response(state, &request)?;
    response.artifact_id = Some(response.context_version.clone());
    write_broker_artifacts(state, task_id, since_version.as_deref(), &response)?;
    Ok(Some(response))
}

pub(crate) fn broker_get_context(
    state: Arc<Mutex<DaemonState>>,
    mut request: BrokerGetContextRequest,
) -> Result<BrokerGetContextResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker get_context requires task_id");
    }
    let previous_request = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&request.task_id)
        .and_then(|task| task.latest_broker_request.clone());
    inherit_broker_request_defaults(&mut request, previous_request.as_ref());
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(broker_default_budget_tokens());
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(broker_default_budget_bytes());
    }
    if request.verbosity.is_none() {
        request.verbosity = Some(BrokerVerbosity::Standard);
    }
    if request.response_mode.is_none() {
        request.response_mode = Some(BrokerResponseMode::Full);
    }
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
        ensure_context_version(task);
        let mut session_request = request.clone();
        session_request.since_version = None;
        session_request.persist_artifacts = Some(true);
        task.latest_broker_request = Some(session_request);
        persist_state(&guard)?;
    }
    let _ = set_context_reason(&state, &request.task_id, "get_context");
    let mut response = compute_broker_response(&state, &request)?;
    daemon_log(&format!(
        "broker get_context task={} diagnostics_ms={:?}",
        request.task_id, response.diagnostics_ms
    ));
    let persist_artifacts = should_persist_broker_artifacts(&request);
    if persist_artifacts {
        response.artifact_id = Some(response.context_version.clone());
        write_broker_artifacts(
            &state,
            &request.task_id,
            request.since_version.as_deref(),
            &response,
        )?;
    }
    if matches!(
        broker_request_response_mode(&request),
        BrokerResponseMode::Slim
    ) {
        Ok(slim_broker_response(
            &response,
            response.artifact_id.clone(),
        ))
    } else {
        Ok(response)
    }
}

pub(crate) fn broker_estimate_context(
    state: Arc<Mutex<DaemonState>>,
    mut request: BrokerEstimateContextRequest,
) -> Result<BrokerEstimateContextResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker estimate_context requires task_id");
    }
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(broker_default_budget_tokens());
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(broker_default_budget_bytes());
    }
    let get_request = estimate_request_to_get_request(&request);
    let response = compute_broker_response(&state, &get_request)?;
    Ok(BrokerEstimateContextResponse {
        context_version: response.context_version.clone(),
        selected_section_ids: response
            .sections
            .iter()
            .map(|section| section.id.clone())
            .collect(),
        est_tokens: response.est_tokens,
        est_bytes: response.est_bytes,
        budget_remaining_tokens: response.budget_remaining_tokens,
        budget_remaining_bytes: response.budget_remaining_bytes,
        section_estimates: response.section_estimates,
        eviction_candidates: response.eviction_candidates,
        would_use_delta: should_use_delta_view(
            &get_request,
            &response.delta,
            response.delta.changed_sections.len() + response.delta.unchanged_section_ids.len(),
        ),
        would_include_brief: !response.sections.is_empty(),
        effective_max_sections: response.effective_max_sections,
        effective_default_max_items_per_section: response.effective_default_max_items_per_section,
        effective_section_item_limits: response.effective_section_item_limits,
        diagnostics_ms: response.diagnostics_ms,
    })
}

pub(crate) fn broker_validate_plan(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerValidatePlanRequest,
) -> Result<BrokerValidatePlanResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker validate_plan requires task_id");
    }
    let root = state.lock().map_err(lock_err)?.root.clone();
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let normalized_steps = normalize_plan_steps(&request.steps);
    let coverage = load_cached_coverage(&root)?;
    // TODO: Use testmap coverage to validate test-oriented plan steps.
    let _ = load_cached_testmap(&root)?;
    let focus_paths = normalized_steps
        .iter()
        .flat_map(|step| step.paths.iter().cloned())
        .collect::<Vec<_>>();
    let focus_symbols = normalized_steps
        .iter()
        .flat_map(|step| step.symbols.iter().cloned())
        .collect::<Vec<_>>();
    let repo_map = mapy_core::expand_repo_map_payload(&build_repo_map_envelope(
        &root,
        &focus_paths,
        &focus_symbols,
        48,
        96,
    )?);
    let deleted_files = current_deleted_paths(&root);
    let completed_steps = snapshot
        .completed_steps
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut files_read = snapshot.files_read.iter().cloned().collect::<HashSet<_>>();
    let step_index = normalized_steps
        .iter()
        .enumerate()
        .map(|(idx, step)| (step.id.clone(), idx))
        .collect::<HashMap<_, _>>();
    let mut touched_paths = HashMap::<String, usize>::new();
    for (idx, step) in normalized_steps.iter().enumerate() {
        for path in &step.paths {
            touched_paths.entry(path.clone()).or_insert(idx);
        }
    }

    let mut violations = Vec::new();
    let mut warnings = Vec::new();

    for step in &normalized_steps {
        for path in &step.paths {
            if !root.join(path).exists() {
                let rule = if deleted_files.contains(path) {
                    "deleted_path"
                } else {
                    "unknown_path"
                };
                let message = if deleted_files.contains(path) {
                    format!("step targets '{path}', which is deleted in the current diff")
                } else {
                    format!("step targets '{path}', which does not exist in the current workspace")
                };
                violations.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: rule.to_string(),
                    severity: "error".to_string(),
                    message,
                    related_paths: vec![path.clone()],
                    related_symbols: Vec::new(),
                });
            }
        }

        for dependency in &step.depends_on {
            if completed_steps.contains(dependency) {
                warnings.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: "redundant_dependency".to_string(),
                    severity: "warning".to_string(),
                    message: format!(
                        "step depends on '{dependency}', but that step is already completed"
                    ),
                    related_paths: step.paths.clone(),
                    related_symbols: step.symbols.clone(),
                });
            } else if !step_index.contains_key(dependency) {
                violations.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: "missing_dependency".to_string(),
                    severity: "error".to_string(),
                    message: format!("step depends on unknown step '{dependency}'"),
                    related_paths: step.paths.clone(),
                    related_symbols: step.symbols.clone(),
                });
            }
        }

        if is_read_like_action(&step.action) {
            files_read.extend(step.paths.iter().cloned());
        }

        if request.require_read_before_edit.unwrap_or(true) && is_edit_like_action(&step.action) {
            for path in &step.paths {
                if !files_read.contains(path) {
                    violations.push(BrokerPlanViolation {
                        step_id: step.id.clone(),
                        rule: "read_before_edit".to_string(),
                        severity: "error".to_string(),
                        message: format!(
                            "step edits '{path}' before the agent has recorded a file_read for it"
                        ),
                        related_paths: vec![path.clone()],
                        related_symbols: step.symbols.clone(),
                    });
                }
            }
        }
    }

    for edge in &repo_map.edges {
        let Some(importer_idx) = touched_paths.get(&edge.from).copied() else {
            continue;
        };
        let Some(imported_idx) = touched_paths.get(&edge.to).copied() else {
            continue;
        };
        if importer_idx < imported_idx {
            let importer_step = &normalized_steps[importer_idx];
            let imported_step = &normalized_steps[imported_idx];
            let importer_depends = importer_step
                .depends_on
                .iter()
                .any(|id| id == &imported_step.id);
            let imported_depends = imported_step
                .depends_on
                .iter()
                .any(|id| id == &importer_step.id);
            if !importer_depends && !imported_depends {
                violations.push(BrokerPlanViolation {
                    step_id: importer_step.id.clone(),
                    rule: "dependency_order".to_string(),
                    severity: "error".to_string(),
                    message: format!(
                        "step touches '{}' before its dependency '{}'; add a dependency or reorder the plan",
                        edge.from, edge.to
                    ),
                    related_paths: vec![edge.from.clone(), edge.to.clone()],
                    related_symbols: Vec::new(),
                });
            }
        }
    }

    if request.require_test_gate.unwrap_or(true) {
        for (idx, step) in normalized_steps.iter().enumerate() {
            if !is_edit_like_action(&step.action) {
                continue;
            }
            for path in &step.paths {
                if !coverage_gap_for_path(coverage.as_ref(), path) {
                    continue;
                }
                let has_following_test_gate =
                    normalized_steps.iter().skip(idx + 1).any(is_test_like_step);
                if !has_following_test_gate {
                    violations.push(BrokerPlanViolation {
                        step_id: step.id.clone(),
                        rule: "missing_test_gate".to_string(),
                        severity: "error".to_string(),
                        message: format!(
                            "step edits uncovered path '{path}' without a later test-focused step"
                        ),
                        related_paths: vec![path.clone()],
                        related_symbols: step.symbols.clone(),
                    });
                }
            }
        }
    }

    let est_plan_tokens = normalized_steps
        .iter()
        .map(estimate_plan_step_tokens)
        .sum::<u64>();
    if let Some(budget_tokens) = request.budget_tokens {
        if est_plan_tokens > budget_tokens {
            violations.push(BrokerPlanViolation {
                step_id: "plan".to_string(),
                rule: "budget_exceeded".to_string(),
                severity: "error".to_string(),
                message: format!(
                    "normalized plan is estimated at ~{est_plan_tokens} tokens, over the requested budget of {budget_tokens}"
                ),
                related_paths: normalized_steps
                    .iter()
                    .flat_map(|step| step.paths.iter().cloned())
                    .collect(),
                related_symbols: Vec::new(),
            });
        }
    }

    Ok(BrokerValidatePlanResponse {
        valid: violations.is_empty(),
        violations,
        warnings,
        normalized_steps,
        est_plan_tokens: Some(est_plan_tokens),
    })
}

pub(crate) fn broker_decompose(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerDecomposeRequest,
) -> Result<BrokerDecomposeResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker decompose requires task_id");
    }
    if request.task_text.trim().is_empty() {
        anyhow::bail!("broker decompose requires task_text");
    }
    let Some(intent) = request.intent else {
        return Ok(BrokerDecomposeResponse {
            steps: Vec::new(),
            assumptions: Vec::new(),
            unresolved: vec!["intent is required for deterministic decomposition".to_string()],
            selected_scope_paths: Vec::new(),
        });
    };

    let root = state.lock().map_err(lock_err)?.root.clone();
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let repo_map = build_repo_map_envelope(
        &root,
        &merged_unique(&snapshot.focus_paths, &request.scope_paths),
        &merged_unique(&snapshot.focus_symbols, &request.scope_symbols),
        64,
        128,
    )?;
    let rich_map = mapy_core::expand_repo_map_payload(&repo_map);
    let coverage = load_cached_coverage(&root)?;
    let testmap = load_cached_testmap(&root)?;
    let primary_scope_paths = infer_scope_paths(
        &request.task_text,
        &rich_map,
        &request.scope_paths,
        &request.scope_symbols,
    );
    let selected_scope_paths = expand_scope_paths(
        &request.task_text,
        &rich_map,
        &primary_scope_paths,
        &request.scope_symbols,
        8,
    );
    if selected_scope_paths.is_empty() {
        return Ok(BrokerDecomposeResponse {
            steps: Vec::new(),
            assumptions: vec![format!(
                "intent locked to {:?} for deterministic decomposition",
                intent
            )],
            unresolved: vec![
                "unable to resolve scope paths from task text; supply scope_paths or scope_symbols"
                    .to_string(),
            ],
            selected_scope_paths,
        });
    }

    let max_steps = request.max_steps.unwrap_or(8).max(1);
    let edge_map = rich_map
        .edges
        .iter()
        .filter(|edge| {
            selected_scope_paths.contains(&edge.from) && selected_scope_paths.contains(&edge.to)
        })
        .fold(BTreeMap::<String, Vec<String>>::new(), |mut acc, edge| {
            acc.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
            acc
        });
    let mut ordered_paths = selected_scope_paths.clone();
    ordered_paths.sort_by_key(|path| edge_map.get(path).map(|deps| deps.len()).unwrap_or(0));

    let mut steps = Vec::new();
    let mut path_to_step = BTreeMap::<String, String>::new();
    let action = match intent {
        BrokerDecomposeIntent::Rename => "rename",
        BrokerDecomposeIntent::Extract => "extract",
        BrokerDecomposeIntent::SplitFile => "split_file",
        BrokerDecomposeIntent::MergeFiles => "merge_files",
        BrokerDecomposeIntent::RestructureModule => "restructure_module",
    };

    for (idx, path) in ordered_paths.iter().enumerate() {
        if steps.len() >= max_steps {
            break;
        }
        let step_id = format!("step-{}", idx + 1);
        let depends_on = edge_map
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|dependency| path_to_step.get(dependency).cloned())
            .collect::<Vec<_>>();
        let related_symbols = rich_map
            .symbols_ranked
            .iter()
            .filter(|symbol| symbol.file == *path)
            .take(3)
            .map(|symbol| symbol.name.clone())
            .collect::<Vec<_>>();
        let description = match intent {
            BrokerDecomposeIntent::Rename => format!("Rename identifiers and references in {path}"),
            BrokerDecomposeIntent::Extract => {
                format!("Extract focused logic from {path} into a smaller unit")
            }
            BrokerDecomposeIntent::SplitFile => {
                format!("Split {path} into smaller responsibility-focused files")
            }
            BrokerDecomposeIntent::MergeFiles => {
                format!("Merge related logic centered on {path}")
            }
            BrokerDecomposeIntent::RestructureModule => {
                format!("Restructure module boundaries around {path}")
            }
        };
        let coverage_gap = coverage_gap_for_path(coverage.as_ref(), path);
        let step = BrokerDecomposedStep {
            id: step_id.clone(),
            action: action.to_string(),
            description,
            paths: vec![path.clone()],
            symbols: related_symbols.clone(),
            depends_on,
            coverage_gap,
            est_tokens: 120 + (related_symbols.len() as u64 * 24),
        };
        path_to_step.insert(path.clone(), step_id);
        steps.push(step);
    }

    let mut test_targets = Vec::new();
    for step in &steps {
        if !step.coverage_gap {
            continue;
        }
        for path in &step.paths {
            test_targets.extend(find_candidate_test_paths(path, &rich_map, testmap.as_ref()));
        }
    }
    let test_targets = merged_unique(&[], &test_targets);
    if !test_targets.is_empty() && steps.len() < max_steps {
        let depends_on = steps.iter().map(|step| step.id.clone()).collect::<Vec<_>>();
        steps.push(BrokerDecomposedStep {
            id: format!("step-{}", steps.len() + 1),
            action: "add_tests".to_string(),
            description: "Add or update tests to cover the decomposed scope".to_string(),
            paths: test_targets,
            symbols: Vec::new(),
            depends_on,
            coverage_gap: false,
            est_tokens: 160,
        });
    }

    Ok(BrokerDecomposeResponse {
        steps,
        assumptions: vec![format!(
            "intent constrained to {}",
            action.replace('_', " ")
        )],
        unresolved: Vec::new(),
        selected_scope_paths,
    })
}
