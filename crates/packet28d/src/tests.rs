use super::*;
use crate::application::{
    bind_preferred_daemon_listener, bind_tcp_listener, bind_unix_listener, effective_uid,
    run_transport, shutdown_persistent_kernels, supervise_daemon_tasks, DaemonListener,
    DaemonRuntimeTasks, UnixSocketPathTooLong, SUN_PATH_CAPACITY,
};
use crate::broker::testing::*;
use crate::planning::{merged_unique_many, normalize_plan_steps};

mod action_critic;
mod budget;
mod cancellation;
mod code_evidence;
mod context_debt;
mod context_render;
mod evidence_confidence;
mod handoff;
mod instruction_resolution;
mod persistence;
mod plan_validation;
mod replan;
mod search;
pub(crate) mod support;
mod transport_runtime;

#[test]
fn explicit_limits_override_verbosity_alias() {
    let mut section_limits = BTreeMap::new();
    section_limits.insert("relevant_context".to_string(), 2);
    let limits = resolve_effective_limits(
        BrokerAction::Plan,
        Some(BrokerVerbosity::Rich),
        Some(3),
        Some(5),
        &section_limits,
    );
    assert_eq!(limits.max_sections, 3);
    assert_eq!(limits.default_max_items_per_section, 5);
    assert_eq!(limits.section_item_limits["relevant_context"], 2);
}

#[test]
fn omitted_explicit_limits_use_deterministic_action_defaults() {
    let plan_limits =
        resolve_effective_limits(BrokerAction::Plan, None, None, None, &BTreeMap::new());
    let choose_tool_limits =
        resolve_effective_limits(BrokerAction::ChooseTool, None, None, None, &BTreeMap::new());
    assert_eq!(plan_limits.max_sections, 8);
    assert_eq!(plan_limits.default_max_items_per_section, 8);
    assert_eq!(plan_limits.section_item_limits["code_evidence"], 6);
    assert_eq!(choose_tool_limits.max_sections, 6);
    assert_eq!(choose_tool_limits.default_max_items_per_section, 5);
}

#[test]
fn brief_always_starts_with_supersession_header() {
    let brief = render_brief(
        "task-123",
        "7",
        &[BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: "Investigate auth flow".to_string(),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        }],
    );
    assert!(brief.starts_with("[Packet28 Context v7"));
    assert!(brief.contains("supersedes all prior Packet28 context"));
}
