use super::*;

fn packet_source_kind(packet: &suite_packet_core::ContextManagePacketRef) -> BrokerSourceKind {
    if packet.target.starts_with("agenty.state.") {
        BrokerSourceKind::SelfAuthored
    } else if packet.target.starts_with("contextq.")
        || packet.target.starts_with("packet28.broker_memory.")
        || packet.target.starts_with("mapy.")
        || packet.target.starts_with("context.")
    {
        BrokerSourceKind::Derived
    } else {
        BrokerSourceKind::External
    }
}

fn render_relevant_context_line(packet: &suite_packet_core::ContextManagePacketRef) -> String {
    let summary = packet
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let rendered = match (packet.source_tier.as_deref(), summary) {
        (_, Some(summary)) => summary.to_string(),
        (Some("curated_memory"), None) => "curated task memory".to_string(),
        (Some("telemetry"), None) => "task telemetry".to_string(),
        _ => "relevant context".to_string(),
    };
    format!("- {rendered}")
}

pub(crate) fn load_task_record(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Option<TaskRecord> {
    state.lock().ok()?.tasks.tasks.get(task_id).cloned()
}

fn shrink_section_to_budget(
    section: &BrokerSection,
    remaining_tokens: u64,
    remaining_bytes: u64,
) -> Option<BrokerSection> {
    if remaining_tokens == 0 || remaining_bytes == 0 {
        return None;
    }
    let lines = section.body.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    for line_count in (1..=lines.len()).rev() {
        let candidate_body = lines[..line_count].join("\n");
        let (est_tokens, est_bytes) = estimate_text_cost(&candidate_body);
        if est_tokens <= remaining_tokens && est_bytes <= remaining_bytes {
            let mut candidate = section.clone();
            candidate.body = candidate_body;
            return Some(candidate);
        }
    }
    let mut candidate = section.clone();
    let max_chars = remaining_bytes.min((remaining_tokens.saturating_mul(4)).max(1)) as usize;
    let truncated = section
        .body
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    if truncated.is_empty() {
        return None;
    }
    candidate.body = format!("{truncated}...");
    Some(candidate)
}

pub(crate) fn prune_sections_for_budget(
    action: BrokerAction,
    sections: Vec<BrokerSection>,
    budget_tokens: u64,
    budget_bytes: u64,
    max_sections: usize,
) -> (Vec<BrokerSection>, Vec<BrokerEvictionCandidate>) {
    if sections.is_empty() {
        return (sections, Vec::new());
    }

    let critical_ids = action_critical_section_ids(action)
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut selected = Vec::new();
    let mut pruned = Vec::new();
    let (mut used_tokens, mut used_bytes) = estimate_brief_banner_cost();
    let min_remaining_tokens_for_optional = ((budget_tokens as f64) * 0.2).ceil() as u64;
    let min_remaining_bytes_for_optional = ((budget_bytes as f64) * 0.2).ceil() as u64;

    let consider = |section: BrokerSection,
                    must_keep: bool,
                    selected: &mut Vec<BrokerSection>,
                    pruned: &mut Vec<BrokerEvictionCandidate>,
                    used_tokens: &mut u64,
                    used_bytes: &mut u64| {
        let (est_tokens, est_bytes) = estimate_rendered_section_cost(&section);
        if est_tokens + *used_tokens <= budget_tokens && est_bytes + *used_bytes <= budget_bytes {
            *used_tokens = (*used_tokens).saturating_add(est_tokens);
            *used_bytes = (*used_bytes).saturating_add(est_bytes);
            selected.push(section);
            return;
        }
        let remaining_tokens = budget_tokens.saturating_sub(*used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(*used_bytes);
        if must_keep {
            if let Some(shrunk) =
                shrink_section_to_budget(&section, remaining_tokens, remaining_bytes)
            {
                let (shrunk_tokens, shrunk_bytes) = estimate_rendered_section_cost(&shrunk);
                *used_tokens = (*used_tokens).saturating_add(shrunk_tokens);
                *used_bytes = (*used_bytes).saturating_add(shrunk_bytes);
                selected.push(shrunk);
                return;
            }
        }
        pruned.push(BrokerEvictionCandidate {
            section_id: section.id.clone(),
            reason: "budget_pruned".to_string(),
            est_tokens,
        });
    };

    let mut objective = sections
        .iter()
        .find(|section| section.id == "task_objective")
        .cloned();
    if let Some(objective) = objective.take() {
        consider(
            objective,
            true,
            &mut selected,
            &mut pruned,
            &mut used_tokens,
            &mut used_bytes,
        );
    }

    for section_id in action_critical_section_ids(action) {
        if let Some(section) = sections
            .iter()
            .find(|section| section.id == *section_id)
            .cloned()
        {
            consider(
                section,
                true,
                &mut selected,
                &mut pruned,
                &mut used_tokens,
                &mut used_bytes,
            );
        }
    }

    for section in sections {
        if section.id == "task_objective" || critical_ids.contains(section.id.as_str()) {
            continue;
        }
        let remaining_tokens = budget_tokens.saturating_sub(used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(used_bytes);
        if remaining_tokens < min_remaining_tokens_for_optional
            || remaining_bytes < min_remaining_bytes_for_optional
        {
            let (est_tokens, _) = estimate_rendered_section_cost(&section);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
            continue;
        }
        consider(
            section,
            false,
            &mut selected,
            &mut pruned,
            &mut used_tokens,
            &mut used_bytes,
        );
    }

    if selected.len() > max_sections {
        for section in selected.drain(max_sections..) {
            let (est_tokens, _) = estimate_rendered_section_cost(&section);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id,
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
        }
    }

    (selected, pruned)
}

pub(crate) fn build_broker_sections(
    root: &Path,
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    manage: Option<&suite_packet_core::ContextManagePayload>,
    _repo_map: Option<&suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>>,
) -> Vec<BrokerSection> {
    let action = request.action.unwrap_or(BrokerAction::Plan);
    let effective_limits = resolve_effective_limits(
        action,
        request.verbosity,
        request.max_sections,
        request.default_max_items_per_section,
        &request.section_item_limits,
    );
    let allowed_sections =
        filter_requested_section_ids(action, &request.include_sections, &request.exclude_sections);
    let task = load_task_record(state, &request.task_id);
    let resolved_questions = build_resolved_questions(task.as_ref(), snapshot);
    let focus_symbols = if request.focus_symbols.is_empty() {
        merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols)
    } else {
        request.focus_symbols.clone()
    };
    let mut query_focus = derive_query_focus(broker_objective(state, request).as_deref());
    if !focus_symbols.is_empty() {
        query_focus.full_symbol_terms.clear();
        query_focus.symbol_terms.clear();
    }
    let query_focus = merge_query_focus_with_symbols(query_focus, &focus_symbols);
    let mut sections = Vec::new();

    if let Some(objective) = query_focus.raw_query.clone() {
        sections.push(BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: objective,
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let task_memory_lines = render_task_memory_lines(snapshot);
    if !task_memory_lines.is_empty() {
        sections.push(BrokerSection {
            id: "task_memory".to_string(),
            title: "Task Memory".to_string(),
            body: truncate_lines(
                task_memory_lines,
                section_item_limit(&effective_limits, "task_memory"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Plan
                    | BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Interpret
                    | BrokerAction::Edit
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let intention_lines = latest_intention_lines(snapshot);
    if !intention_lines.is_empty() {
        sections.push(BrokerSection {
            id: "agent_intention".to_string(),
            title: "Latest Intention".to_string(),
            body: truncate_lines(
                intention_lines,
                section_item_limit(&effective_limits, "agent_intention"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let checkpoint_context_lines = render_checkpoint_context_lines(snapshot);
    if !checkpoint_context_lines.is_empty() {
        sections.push(BrokerSection {
            id: "checkpoint_context".to_string(),
            title: "Checkpoint Context".to_string(),
            body: truncate_lines(
                checkpoint_context_lines,
                section_item_limit(&effective_limits, "checkpoint_context"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    if !snapshot.active_decisions.is_empty() {
        sections.push(BrokerSection {
            id: "active_decisions".to_string(),
            title: "Active Decisions".to_string(),
            body: truncate_lines(
                snapshot
                    .active_decisions
                    .iter()
                    .map(|decision| {
                        let suffix = task
                            .as_ref()
                            .and_then(|task| task.linked_decisions.get(&decision.id))
                            .map(|question_id| format!(" (answers {question_id})"))
                            .unwrap_or_default();
                        format!("- {}: {}{}", decision.id, decision.text, suffix)
                    })
                    .collect(),
                section_item_limit(&effective_limits, "active_decisions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.open_questions.is_empty() {
        sections.push(BrokerSection {
            id: "open_questions".to_string(),
            title: "Open Questions".to_string(),
            body: truncate_lines(
                snapshot
                    .open_questions
                    .iter()
                    .map(|question| format!("- {}: {}", question.id, question.text))
                    .collect(),
                section_item_limit(&effective_limits, "open_questions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !resolved_questions.is_empty() {
        sections.push(BrokerSection {
            id: "resolved_questions".to_string(),
            title: "Resolved Questions".to_string(),
            body: truncate_lines(
                resolved_questions
                    .iter()
                    .map(|question| {
                        match (&question.resolved_by_decision_id, &question.resolution_text) {
                            (Some(decision_id), Some(text)) => {
                                format!(
                                    "- {}: {} -> {} ({})",
                                    question.id, question.text, decision_id, text
                                )
                            }
                            (Some(decision_id), None) => {
                                format!("- {}: {} -> {}", question.id, question.text, decision_id)
                            }
                            _ => format!("- {}: {}", question.id, question.text),
                        }
                    })
                    .collect(),
                section_item_limit(&effective_limits, "resolved_questions"),
            ),
            priority: if matches!(action, BrokerAction::Interpret | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let focus_lines = merged_unique(
        &merged_unique(&snapshot.focus_paths, &snapshot.checkpoint_focus_paths),
        &request.focus_paths,
    )
    .into_iter()
    .map(|path| format!("- path: {path}"))
    .chain(
        merged_unique(
            &merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols),
            &request.focus_symbols,
        )
        .into_iter()
        .map(|symbol| format!("- symbol: {symbol}")),
    )
    .collect::<Vec<_>>();
    if !focus_lines.is_empty() {
        sections.push(BrokerSection {
            id: "current_focus".to_string(),
            title: "Current Focus".to_string(),
            body: truncate_lines(
                focus_lines,
                section_item_limit(&effective_limits, "current_focus"),
            ),
            priority: if matches!(action, BrokerAction::Inspect | BrokerAction::Edit) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let discovered_scope_lines = snapshot
        .read_paths_by_tool
        .iter()
        .flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- read via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        })
        .chain(snapshot.edited_paths_by_tool.iter().flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- edited via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        }))
        .chain(
            merged_unique(&snapshot.focus_symbols, &snapshot.checkpoint_focus_symbols)
                .into_iter()
                .map(|symbol| format!("- symbol: {symbol}")),
        )
        .collect::<Vec<_>>();
    if !discovered_scope_lines.is_empty() {
        sections.push(BrokerSection {
            id: "discovered_scope".to_string(),
            title: "Discovered Scope".to_string(),
            body: truncate_lines(
                discovered_scope_lines,
                section_item_limit(&effective_limits, "discovered_scope"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Plan
                    | BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Edit
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.recent_tool_invocations.is_empty() {
        let lines = render_recent_tool_activity_lines(snapshot, false);
        sections.push(BrokerSection {
            id: "recent_tool_activity".to_string(),
            title: "Recent Tool Activity".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "recent_tool_activity"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Interpret
                    | BrokerAction::Edit
                    | BrokerAction::Summarize
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let missed_savings = render_missed_savings_lines(snapshot);
    if !missed_savings.is_empty() {
        sections.push(BrokerSection {
            id: "savings_opportunities".to_string(),
            title: "Savings Opportunities".to_string(),
            body: truncate_lines(
                missed_savings,
                section_item_limit(&effective_limits, "savings_opportunities"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Edit
                    | BrokerAction::Summarize
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.tool_failures.is_empty() {
        let lines = snapshot
            .tool_failures
            .iter()
            .rev()
            .map(|failure| {
                format!(
                    "- #{} {} [{}] {}",
                    failure.sequence,
                    failure.tool_name,
                    serde_json::to_string(&failure.operation_kind)
                        .unwrap_or_else(|_| "\"generic\"".to_string())
                        .trim_matches('"'),
                    failure
                        .error_message
                        .as_deref()
                        .or(failure.error_class.as_deref())
                        .unwrap_or("tool failed")
                )
            })
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "tool_failures".to_string(),
            title: "Tool Failures".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "tool_failures"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.evidence_artifact_ids.is_empty() {
        sections.push(BrokerSection {
            id: "evidence_cache".to_string(),
            title: "Evidence Cache".to_string(),
            body: truncate_lines(
                snapshot
                    .evidence_artifact_ids
                    .iter()
                    .map(|artifact_id| format!("- artifact: {artifact_id}"))
                    .collect(),
                section_item_limit(&effective_limits, "evidence_cache"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
    {
        let body = snapshot
            .changed_paths_since_checkpoint
            .iter()
            .map(|path| format!("- changed path: {path}"))
            .chain(
                snapshot
                    .changed_symbols_since_checkpoint
                    .iter()
                    .map(|symbol| format!("- changed symbol: {symbol}")),
            )
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "checkpoint_deltas".to_string(),
            title: "Checkpoint Deltas".to_string(),
            body: truncate_lines(
                body,
                section_item_limit(&effective_limits, "checkpoint_deltas"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if should_run_reducer_search(&allowed_sections) {
        let search_execution = build_reducer_search_execution(
            Some(state),
            root,
            snapshot,
            request,
            &query_focus,
            action,
            section_item_limit(&effective_limits, "search_evidence").max(8),
            section_item_limit(&effective_limits, "code_evidence").min(15),
        );
        let reducer_files = search_execution.files;
        if !reducer_files.is_empty() {
            let evidence_by_file = search_execution.evidence_by_file;
            let lines = reducer_files
                .iter()
                .map(|file| {
                    let line_hint = file
                        .preview_matches
                        .first()
                        .map(|(line, _)| format!(":{line}"))
                        .unwrap_or_default();
                    let terms = file
                        .matched_terms
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "- {}{} [matches={}] — direct reducer hit for {}",
                        file.path, line_hint, file.match_count, terms
                    )
                })
                .collect::<Vec<_>>();
            if !lines.is_empty() {
                sections.push(BrokerSection {
                    id: "search_evidence".to_string(),
                    title: "Relevant Files".to_string(),
                    body: truncate_lines(
                        lines,
                        section_item_limit(&effective_limits, "search_evidence"),
                    ),
                    priority: if matches!(
                        action,
                        BrokerAction::Plan | BrokerAction::Inspect | BrokerAction::ChooseTool
                    ) {
                        1
                    } else {
                        2
                    },
                    source_kind: BrokerSourceKind::Derived,
                });
            }

            let max_items = section_item_limit(&effective_limits, "code_evidence");
            let evidence_lines = reducer_files
                .iter()
                .flat_map(|file| {
                    evidence_by_file
                        .get(&file.path)
                        .map(|summary| summary.rendered_lines.clone())
                        .unwrap_or_default()
                })
                .take(max_items)
                .collect::<Vec<_>>();
            if !evidence_lines.is_empty() {
                sections.push(BrokerSection {
                    id: "code_evidence".to_string(),
                    title: "Code Evidence".to_string(),
                    body: truncate_lines(evidence_lines, max_items),
                    priority: if matches!(
                        action,
                        BrokerAction::Inspect
                            | BrokerAction::Interpret
                            | BrokerAction::Edit
                            | BrokerAction::ChooseTool
                    ) {
                        1
                    } else {
                        2
                    },
                    source_kind: BrokerSourceKind::Derived,
                });
            }
        }
    }

    if manage.is_some_and(|manage| {
        !manage.working_set.is_empty() || !manage.recommended_packets.is_empty()
    }) {
        let manage = manage.expect("manage checked above");
        let packets = if !manage.working_set.is_empty() {
            &manage.working_set
        } else {
            &manage.recommended_packets
        };
        let visible_packets = packets
            .iter()
            .filter(|packet| {
                request.include_self_context
                    || packet_source_kind(packet) != BrokerSourceKind::SelfAuthored
            })
            .map(render_relevant_context_line)
            .collect::<Vec<_>>();
        if !visible_packets.is_empty() {
            sections.push(BrokerSection {
                id: "relevant_context".to_string(),
                title: "Relevant Context".to_string(),
                body: truncate_lines(
                    visible_packets,
                    section_item_limit(&effective_limits, "relevant_context"),
                ),
                priority: if matches!(
                    action,
                    BrokerAction::Plan | BrokerAction::Interpret | BrokerAction::ChooseTool
                ) {
                    1
                } else {
                    2
                },
                source_kind: BrokerSourceKind::External,
            });
        }
    }

    if manage.is_some_and(|manage| !manage.recommended_actions.is_empty()) {
        let manage = manage.expect("manage checked above");
        let title = match request
            .tool_result_kind
            .unwrap_or(BrokerToolResultKind::Generic)
        {
            BrokerToolResultKind::Build => "Build Guidance",
            BrokerToolResultKind::Stack => "Stack Guidance",
            BrokerToolResultKind::Test => "Test Guidance",
            BrokerToolResultKind::Diff => "Diff Guidance",
            BrokerToolResultKind::Generic => "Recommended Actions",
        };
        sections.push(BrokerSection {
            id: "recommended_actions".to_string(),
            title: title.to_string(),
            body: truncate_lines(
                manage
                    .recommended_actions
                    .iter()
                    .map(|action| format!("- {}: {}", action.kind, action.summary))
                    .collect(),
                section_item_limit(&effective_limits, "recommended_actions"),
            ),
            priority: if matches!(action, BrokerAction::ChooseTool | BrokerAction::Interpret) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if matches!(action, BrokerAction::Summarize) {
        let progress = vec![
            format!("- completed steps: {}", snapshot.completed_steps.len()),
            format!("- files read: {}", snapshot.files_read.len()),
            format!("- files edited: {}", snapshot.files_edited.len()),
        ];
        sections.push(BrokerSection {
            id: "progress".to_string(),
            title: "Progress".to_string(),
            body: progress.join("\n"),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    sections.retain(|section| allowed_sections.contains(&section.id));
    sections
}

pub(crate) fn render_brief(
    task_id: &str,
    context_version: &str,
    sections: &[BrokerSection],
) -> String {
    let mut blocks = vec![format!(
        "[Packet28 Context v{context_version} — current Packet28 context for task {task_id}; supersedes all prior Packet28 context for this task]"
    )];
    blocks.extend(
        sections
            .iter()
            .map(|section| format!("## {}\n{}", section.title, section.body)),
    );
    blocks.join("\n\n")
}

pub(crate) fn load_versioned_broker_response(
    root: &Path,
    task_id: &str,
    context_version: &str,
) -> Result<Option<BrokerGetContextResponse>> {
    let path = task_version_json_path(root, task_id, context_version);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read versioned broker response '{}'",
            path.display()
        )
    })?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(crate) fn build_delta(
    current: &[BrokerSection],
    previous: Option<&BrokerGetContextResponse>,
) -> BrokerDeltaResponse {
    let Some(previous) = previous else {
        return BrokerDeltaResponse {
            changed_sections: current.to_vec(),
            removed_section_ids: Vec::new(),
            unchanged_section_ids: Vec::new(),
            full_refresh_required: true,
        };
    };
    let current_by_id = current
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();
    let previous_by_id = previous
        .sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();

    let mut changed_sections = Vec::new();
    let mut unchanged_section_ids = Vec::new();
    for section in current {
        match previous_by_id.get(section.id.as_str()) {
            Some(old) if *old == section => unchanged_section_ids.push(section.id.clone()),
            _ => changed_sections.push(section.clone()),
        }
    }
    let removed_section_ids = previous
        .sections
        .iter()
        .filter(|section| !current_by_id.contains_key(section.id.as_str()))
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    BrokerDeltaResponse {
        changed_sections,
        removed_section_ids,
        unchanged_section_ids,
        full_refresh_required: false,
    }
}

pub(crate) fn build_section_estimates(
    sections: &[BrokerSection],
    changed_ids: &HashSet<String>,
) -> Vec<BrokerSectionEstimate> {
    sections
        .iter()
        .map(|section| {
            let (est_tokens, est_bytes) = estimate_rendered_section_cost(section);
            BrokerSectionEstimate {
                id: section.id.clone(),
                est_tokens,
                est_bytes,
                source_kind: section.source_kind,
                changed: changed_ids.contains(&section.id),
            }
        })
        .collect()
}

pub(crate) fn build_eviction_candidates(
    sections: &[BrokerSection],
) -> Vec<BrokerEvictionCandidate> {
    sections
        .iter()
        .filter(|section| {
            matches!(
                section.id.as_str(),
                "relevant_context"
                    | "search_evidence"
                    | "checkpoint_deltas"
                    | "recommended_actions"
            )
        })
        .map(|section| {
            let (est_tokens, _) = estimate_rendered_section_cost(section);
            let reason = match section.id.as_str() {
                "relevant_context" => "refreshable evidence".to_string(),
                "search_evidence" => "search evidence can be regenerated".to_string(),
                "checkpoint_deltas" => "checkpoint state can be recomputed".to_string(),
                "recommended_actions" => "guidance can be regenerated".to_string(),
                _ => "refreshable section".to_string(),
            };
            BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason,
                est_tokens,
            }
        })
        .collect()
}

pub(crate) fn should_use_delta_view(
    request: &BrokerGetContextRequest,
    delta: &BrokerDeltaResponse,
    full_sections_len: usize,
) -> bool {
    match broker_request_response_mode(request) {
        BrokerResponseMode::Full => false,
        BrokerResponseMode::Delta => request.since_version.is_some(),
        BrokerResponseMode::Slim | BrokerResponseMode::Auto => {
            request.since_version.is_some()
                && !delta.full_refresh_required
                && !delta.changed_sections.is_empty()
                && delta.changed_sections.len() < full_sections_len
        }
    }
}
