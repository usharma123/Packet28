mod context;
mod handoff;
mod limits;
mod ops;
mod render;
mod search;
mod search_plan;
mod snapshot;
mod support;

pub(crate) use context::{
    broker_decompose, broker_estimate_context, broker_get_context, broker_validate_plan,
    refresh_broker_context_for_task,
};
pub(crate) use handoff::{broker_prepare_handoff, mark_handoff_consumed};
pub(crate) use limits::estimate_text_cost;
pub(crate) use ops::{broker_task_status, broker_write_state, broker_write_state_batch};
pub(crate) use render::{load_task_record, load_versioned_broker_response};
pub(crate) use snapshot::insert_sorted_unique;
pub(crate) use support::{
    build_registry_status_v1, build_status, complete_task_cancellation_for_generation,
    emit_task_event_for_generation, ensure_task_record_mut, inherit_recovered_agent_state,
    kernel_for_context_root, kernel_for_request, load_agent_snapshot_for_task, now_unix_millis,
    refresh_task_context_summary_for_generation, set_context_reason_for_generation,
};

#[cfg(test)]
pub(crate) mod testing {
    pub(crate) use super::context::broker_validate_plan;
    pub(crate) use super::handoff::{broker_prepare_handoff, compute_handoff_state};
    pub(crate) use super::limits::{
        estimate_text_cost, filter_requested_section_ids, resolve_effective_limits,
        should_run_reducer_search,
    };
    pub(crate) use super::render::{
        build_action_critic_lines, build_broker_sections, build_budget_preflight_section,
        confidence_payoff, confidence_risk, prune_sections_for_budget, render_brief,
    };
    pub(crate) use super::search::{
        build_authenticated_search_evidence, extract_code_evidence, EvidenceMatchKind,
    };
    pub(crate) use super::search_plan::{
        build_reducer_search_execution, derive_query_focus, expand_scope_paths, infer_scope_paths,
        merge_query_focus_with_symbols, ReducerSearchFile, SearchExecution, SearchExecutionArgs,
        ToolResultProvenance,
    };
    pub(crate) use super::snapshot::{
        build_budget_notes_section, postprocess_selected_sections, render_checkpoint_context_lines,
        render_task_memory_lines,
    };
    pub(crate) use super::support::{
        broker_default_budget_tokens, build_repo_map_envelope, emit_task_event,
        emit_task_event_for_generation, inherit_broker_request_defaults, set_context_reason,
    };
}
