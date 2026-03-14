use super::*;

pub(crate) fn estimate_text_cost(text: &str) -> (u64, u64) {
    let est_bytes = text.len() as u64;
    let est_tokens = est_bytes.saturating_add(3) / 4;
    (est_tokens.max(1), est_bytes)
}

pub(crate) fn estimate_brief_banner_cost() -> (u64, u64) {
    estimate_text_cost(
        "[Packet28 Context v{context_version} — current Packet28 context for task {task_id}; supersedes all prior Packet28 context for this task]",
    )
}

pub(crate) fn estimate_rendered_section_cost(section: &BrokerSection) -> (u64, u64) {
    estimate_text_cost(&format!("## {}\n{}", section.title, section.body))
}

fn section_ids_for_action(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "active_decisions",
            "open_questions",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "recommended_actions",
        ],
        BrokerAction::Inspect => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "checkpoint_deltas",
            "active_decisions",
            "open_questions",
        ],
        BrokerAction::ChooseTool => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "discovered_scope",
            "search_evidence",
            "code_evidence",
            "recommended_actions",
            "relevant_context",
            "open_questions",
            "active_decisions",
        ],
        BrokerAction::Interpret => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "code_evidence",
            "recommended_actions",
            "relevant_context",
            "active_decisions",
            "open_questions",
            "resolved_questions",
        ],
        BrokerAction::Edit => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "evidence_cache",
            "checkpoint_deltas",
            "active_decisions",
            "search_evidence",
            "code_evidence",
            "relevant_context",
            "resolved_questions",
        ],
        BrokerAction::Summarize => &[
            "task_objective",
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "progress",
            "recent_tool_activity",
            "tool_failures",
            "active_decisions",
            "resolved_questions",
            "open_questions",
            "checkpoint_deltas",
        ],
    }
}

fn default_limits_for_action(action: BrokerAction) -> BrokerEffectiveLimits {
    let mut section_item_limits = BTreeMap::new();
    match action {
        BrokerAction::Plan => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("active_decisions".to_string(), 8);
            section_item_limits.insert("open_questions".to_string(), 8);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 6);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Inspect => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::ChooseTool => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 4);
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("discovered_scope".to_string(), 6);
            section_item_limits.insert("search_evidence".to_string(), 6);
            section_item_limits.insert("code_evidence".to_string(), 4);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("open_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 6,
                default_max_items_per_section: 5,
                section_item_limits,
            }
        }
        BrokerAction::Interpret => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 5);
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 4,
                section_item_limits,
            }
        }
        BrokerAction::Edit => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 8);
            section_item_limits.insert("agent_intention".to_string(), 6);
            section_item_limits.insert("checkpoint_context".to_string(), 6);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("evidence_cache".to_string(), 4);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            section_item_limits.insert("search_evidence".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 5);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Summarize => {
            section_item_limits.insert("budget_notes".to_string(), 4);
            section_item_limits.insert("task_memory".to_string(), 6);
            section_item_limits.insert("agent_intention".to_string(), 5);
            section_item_limits.insert("checkpoint_context".to_string(), 5);
            section_item_limits.insert("progress".to_string(), 3);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
    }
}

fn legacy_verbosity_limits(
    action: BrokerAction,
    verbosity: BrokerVerbosity,
) -> BrokerEffectiveLimits {
    let mut limits = default_limits_for_action(action);
    match verbosity {
        BrokerVerbosity::Compact => {
            limits.max_sections = limits.max_sections.min(4);
            limits.default_max_items_per_section = 3;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).min(3);
            }
        }
        BrokerVerbosity::Standard => {}
        BrokerVerbosity::Rich => {
            limits.max_sections = limits.max_sections.max(8);
            limits.default_max_items_per_section = 12;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).max(10);
            }
        }
    }
    limits
}

pub(crate) fn resolve_effective_limits(
    action: BrokerAction,
    verbosity: Option<BrokerVerbosity>,
    max_sections: Option<usize>,
    default_max_items_per_section: Option<usize>,
    section_item_limits: &BTreeMap<String, usize>,
) -> BrokerEffectiveLimits {
    let has_explicit_limits = max_sections.is_some()
        || default_max_items_per_section.is_some()
        || !section_item_limits.is_empty();
    let mut limits = if has_explicit_limits {
        default_limits_for_action(action)
    } else {
        legacy_verbosity_limits(action, verbosity.unwrap_or(BrokerVerbosity::Standard))
    };
    if let Some(value) = max_sections.filter(|value| *value > 0) {
        limits.max_sections = value;
    }
    if let Some(value) = default_max_items_per_section.filter(|value| *value > 0) {
        limits.default_max_items_per_section = value;
    }
    for (section_id, limit) in section_item_limits {
        if *limit > 0 {
            limits
                .section_item_limits
                .insert(section_id.clone(), *limit);
        }
    }
    limits
}

pub(crate) fn section_item_limit(limits: &BrokerEffectiveLimits, section_id: &str) -> usize {
    limits
        .section_item_limits
        .get(section_id)
        .copied()
        .unwrap_or(limits.default_max_items_per_section)
}

pub(crate) fn filter_requested_section_ids(
    action: BrokerAction,
    include_sections: &[String],
    exclude_sections: &[String],
) -> HashSet<String> {
    let mut allowed = section_ids_for_action(action)
        .iter()
        .map(|value| (*value).to_string())
        .collect::<HashSet<_>>();
    if !include_sections.is_empty() {
        allowed = include_sections
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .collect();
    }
    for excluded in exclude_sections {
        allowed.remove(excluded.trim());
    }
    allowed
}

pub(crate) fn should_run_reducer_search(allowed_sections: &HashSet<String>) -> bool {
    allowed_sections.contains("search_evidence") || allowed_sections.contains("code_evidence")
}

pub(crate) fn action_critical_section_ids(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "search_evidence",
            "relevant_context",
            "recommended_actions",
        ],
        BrokerAction::Inspect => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "code_evidence",
            "search_evidence",
            "relevant_context",
        ],
        BrokerAction::ChooseTool => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "recommended_actions",
        ],
        BrokerAction::Interpret => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "recent_tool_activity",
            "tool_failures",
            "code_evidence",
        ],
        BrokerAction::Edit => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "code_evidence",
            "current_focus",
            "checkpoint_deltas",
            "evidence_cache",
        ],
        BrokerAction::Summarize => &[
            "budget_notes",
            "task_memory",
            "agent_intention",
            "checkpoint_context",
            "progress",
            "recent_tool_activity",
            "tool_failures",
        ],
    }
}
