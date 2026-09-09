use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use context_kernel_core::KernelRequest;
use packet28_daemon_protocol::broker::{
    BrokerAction, BrokerDeltaResponse, BrokerGetContextRequest, BrokerGetContextResponse,
    BrokerHandoffDescriptor, BrokerHandoffReadiness, BrokerHandoffStatus,
    BrokerPrepareHandoffRequest, BrokerPrepareHandoffResponse, BrokerResponseMode, BrokerSection,
};
use packet28_daemon_protocol::paths::{
    task_brief_json_path, task_brief_markdown_path, task_event_log_path, task_state_json_path,
    task_version_json_path,
};
use packet28_daemon_protocol::task::TaskRecord;
use serde_json::json;

use super::context::compute_broker_response;
use super::limits::estimate_text_cost;
use super::render::{load_task_record, load_versioned_broker_response, render_brief};
use super::support::{
    ensure_task_record_mut, load_agent_snapshot_for_task, now_unix_millis, set_context_reason,
};
use crate::state::DaemonState;
use crate::{
    context_version_storage_id, fence_task_namespace_admission, lock_err, persist_task,
    task_storage_id,
};

pub(crate) fn next_action_summary(
    manage: Option<&suite_packet_core::ContextManagePayload>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Option<String> {
    manage
        .and_then(|payload| payload.recommended_actions.first())
        .map(|action| action.summary.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            snapshot
                .latest_intention
                .as_ref()
                .map(|intention| intention.text.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            snapshot
                .open_questions
                .first()
                .map(|question| format!("Resolve open question: {}", question.text))
        })
}

fn normalize_timestamp_millis(value: u64) -> u64 {
    // Older hook fields were persisted in unix seconds while handoff timestamps
    // are stored in unix milliseconds. Normalize both so repeated handoffs and
    // resumed tasks can compare boundaries safely.
    if value < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn handoff_contradiction_warnings(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<String> {
    let contradiction_terms = [
        "contradict",
        "contradicted",
        "contradiction",
        "reject",
        "rejected",
        "refute",
        "refuted",
        "falsify",
        "falsified",
        "not true",
    ];
    let mut warnings = Vec::new();
    for decision in snapshot
        .active_decisions
        .iter()
        .filter(|decision| decision.id.starts_with("hypothesis:"))
    {
        let hypothesis_key = decision
            .id
            .strip_prefix("hypothesis:")
            .unwrap_or(decision.id.as_str())
            .to_ascii_lowercase();
        let hypothesis_text = decision
            .text
            .strip_prefix("hypothesis active: ")
            .unwrap_or(decision.text.as_str())
            .to_ascii_lowercase();
        for invocation in &snapshot.recent_tool_invocations {
            let evidence = [
                invocation.result_summary.as_deref().unwrap_or_default(),
                invocation.compact_preview.as_deref().unwrap_or_default(),
                invocation.request_summary.as_deref().unwrap_or_default(),
                invocation.command.as_deref().unwrap_or_default(),
            ]
            .join(" ")
            .to_ascii_lowercase();
            if evidence.trim().is_empty()
                || !contradiction_terms
                    .iter()
                    .any(|term| evidence.contains(term))
            {
                continue;
            }
            let mentions_hypothesis = evidence.contains(&hypothesis_key)
                || decision.related_paths.iter().any(|path| {
                    !path.trim().is_empty() && evidence.contains(&path.to_ascii_lowercase())
                })
                || decision.related_symbols.iter().any(|symbol| {
                    !symbol.trim().is_empty() && evidence.contains(&symbol.to_ascii_lowercase())
                })
                || hypothesis_text
                    .split_whitespace()
                    .filter(|term| term.len() >= 5)
                    .take(4)
                    .any(|term| evidence.contains(term));
            if mentions_hypothesis {
                warnings.push(format!(
                    "handoff_contradiction: active {} may be contradicted by tool #{} {}",
                    decision.id, invocation.sequence, invocation.tool_name
                ));
                break;
            }
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

fn handoff_readiness_score(
    handoff_ready: bool,
    warnings: &[String],
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    next_action_summary: Option<&str>,
) -> BrokerHandoffReadiness {
    let mut score = 100_u64;
    let mut reasons = Vec::new();
    if !handoff_ready {
        score = score.saturating_sub(35);
        reasons.push("handoff_not_ready".to_string());
    }
    if snapshot.latest_checkpoint_id.is_none() {
        score = score.saturating_sub(20);
        reasons.push("missing_checkpoint".to_string());
    }
    if !snapshot.open_questions.is_empty() {
        score = score.saturating_sub(
            (snapshot.open_questions.len() as u64)
                .saturating_mul(10)
                .min(25),
        );
        reasons.push(format!("open_questions={}", snapshot.open_questions.len()));
    }
    if !warnings.is_empty() {
        score = score.saturating_sub((warnings.len() as u64).saturating_mul(25).min(50));
        reasons.push(format!("contradictions={}", warnings.len()));
    }
    if next_action_summary
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        score = score.saturating_sub(10);
        reasons.push("missing_next_action".to_string());
    }
    let has_dirty_state = !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
        || !snapshot.files_edited.is_empty();
    let has_recent_verification = snapshot.recent_tool_invocations.iter().any(|tool| {
        matches!(
            tool.operation_kind,
            suite_packet_core::ToolOperationKind::Build
                | suite_packet_core::ToolOperationKind::Test
                | suite_packet_core::ToolOperationKind::Diff
        )
    });
    if has_dirty_state && !has_recent_verification {
        score = score.saturating_sub(15);
        reasons.push("missing_recent_verification".to_string());
    }
    let status = if score >= 85 {
        "ready"
    } else if score >= 60 {
        "caution"
    } else {
        "blocked"
    }
    .to_string();
    BrokerHandoffReadiness {
        score,
        status,
        reasons,
    }
}

pub(crate) fn latest_handoff_descriptor(
    task: Option<&TaskRecord>,
) -> Option<BrokerHandoffDescriptor> {
    task.and_then(|task| {
        task.latest_handoff_id
            .as_ref()
            .and_then(|handoff_id| {
                task.handoffs
                    .iter()
                    .find(|handoff| &handoff.handoff_id == handoff_id)
                    .cloned()
            })
            .or_else(|| {
                task.handoffs
                    .iter()
                    .max_by(|a, b| {
                        a.generated_at_unix_ms
                            .cmp(&b.generated_at_unix_ms)
                            .then_with(|| a.handoff_id.cmp(&b.handoff_id))
                    })
                    .cloned()
            })
    })
}

pub(crate) fn latest_ready_handoff_descriptor(
    task: Option<&TaskRecord>,
) -> Option<BrokerHandoffDescriptor> {
    task.and_then(|task| {
        task.handoffs
            .iter()
            .filter(|handoff| handoff.status == BrokerHandoffStatus::Ready)
            .max_by(|a, b| {
                a.generated_at_unix_ms
                    .cmp(&b.generated_at_unix_ms)
                    .then_with(|| a.handoff_id.cmp(&b.handoff_id))
            })
            .cloned()
    })
}

fn derive_handoff_id(task_id: &str, generated_at_unix_ms: u64) -> String {
    format!("{task_id}:handoff:{generated_at_unix_ms}")
}

/// Promotes a handoff to the task's latest ready handoff.
///
/// Existing ready or consumed handoffs are marked as superseded, and handoff
/// history is retained in newest-first order up to the configured limit.
///
/// # Examples
///
/// ```rust,ignore
/// promote_new_ready_handoff(&mut task, handoff);
/// assert_eq!(task.latest_handoff_id.as_deref(), Some("handoff-123"));
/// ```
fn promote_new_ready_handoff(task: &mut TaskRecord, mut handoff: BrokerHandoffDescriptor) {
    for existing in &mut task.handoffs {
        if matches!(
            existing.status,
            BrokerHandoffStatus::Ready | BrokerHandoffStatus::Consumed
        ) {
            existing.status = BrokerHandoffStatus::Superseded;
            existing.superseded_by_handoff_id = Some(handoff.handoff_id.clone());
        }
    }
    handoff.status = BrokerHandoffStatus::Ready;
    task.latest_handoff_id = Some(handoff.handoff_id.clone());
    task.latest_handoff_artifact_id = Some(handoff.artifact_id.clone());
    task.latest_handoff_generated_at_unix = Some(handoff.generated_at_unix_ms);
    task.latest_handoff_checkpoint_id = handoff.checkpoint_id.clone();
    task.handoffs.push(handoff);
    task.handoffs.sort_by(|a, b| {
        b.generated_at_unix_ms
            .cmp(&a.generated_at_unix_ms)
            .then_with(|| a.handoff_id.cmp(&b.handoff_id))
    });
    cap_handoff_history(&mut task.handoffs);
}

/// Upper bound on retained handoff descriptors per task.
///
/// Superseded handoffs accumulate over a long-lived session. Without a bound the
/// task record can grow past the paginated record-size limit and poison registry
/// listing. Keeping the most recent descriptors preserves the active/ready
/// handoff (always the newest) plus recent history.
const TASK_HANDOFF_HISTORY_MAX: usize = 64;

/// Limits handoff history to the most recent descriptors.
///
/// The input must already be ordered from newest to oldest.
///
/// # Examples
///
/// ```
/// let mut handoffs = Vec::new();
/// cap_handoff_history(&mut handoffs);
/// assert!(handoffs.len() <= TASK_HANDOFF_HISTORY_MAX);
/// ```
fn cap_handoff_history(handoffs: &mut Vec<BrokerHandoffDescriptor>) {
    if handoffs.len() > TASK_HANDOFF_HISTORY_MAX {
        handoffs.truncate(TASK_HANDOFF_HISTORY_MAX);
    }
}

/// Marks a handoff as consumed and records its resume time and count.
///
/// Returns `None` when the task or handoff does not exist.
///
/// # Examples
///
/// ```no_run
/// # let state: Arc<Mutex<DaemonState>> = unimplemented!();
/// let consumed = mark_handoff_consumed(&state, "task-1", "handoff-1")?;
/// assert!(consumed.is_some());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub(crate) fn mark_handoff_consumed(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    handoff_id: &str,
) -> Result<Option<BrokerHandoffDescriptor>> {
    let mut guard = state.lock().map_err(lock_err)?;
    let Some(task) = guard.tasks.tasks.get_mut(task_id) else {
        return Ok(None);
    };
    let Some(handoff) = task
        .handoffs
        .iter_mut()
        .find(|handoff| handoff.handoff_id == handoff_id)
    else {
        return Ok(None);
    };
    handoff.status = BrokerHandoffStatus::Consumed;
    handoff.resume_count = handoff.resume_count.saturating_add(1);
    handoff.consumed_at_unix_ms = Some(now_unix_millis());
    let updated = handoff.clone();
    persist_task(&guard, task_id)?;
    Ok(Some(updated))
}

pub(crate) fn compute_handoff_state(
    task: Option<&TaskRecord>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> (bool, String) {
    let latest_ready_handoff = latest_ready_handoff_descriptor(task);
    let latest_handoff_at = latest_ready_handoff
        .as_ref()
        .map(|handoff| handoff.generated_at_unix_ms)
        .or_else(|| task.and_then(|task| task.latest_handoff_generated_at_unix))
        .map(normalize_timestamp_millis);
    let latest_hook_boundary_at = task
        .and_then(|task| task.latest_hook_boundary_at_unix)
        .map(normalize_timestamp_millis);
    let latest_hook_boundary_kind = task.and_then(|task| task.latest_hook_boundary_kind.as_deref());
    let threshold_exceeded = task.is_some_and(|task| task.hook_threshold_exceeded);
    let Some(checkpoint_id) = snapshot.latest_checkpoint_id.as_ref() else {
        if snapshot.latest_intention.is_none() {
            return (
                false,
                "Intent required before preparing a handoff.".to_string(),
            );
        }
        if latest_hook_boundary_at.is_some_and(|boundary_at| {
            latest_handoff_at.is_none_or(|handoff_at| boundary_at > handoff_at)
        }) {
            let reason = latest_hook_boundary_kind.unwrap_or("boundary");
            return (
                true,
                format!("Hook boundary '{reason}' is available for handoff."),
            );
        }
        if threshold_exceeded {
            return (
                true,
                "Soft context threshold reached and intent is available.".to_string(),
            );
        }
        return (
            false,
            "Hook boundary or threshold required before preparing a handoff.".to_string(),
        );
    };
    let latest_handoff_checkpoint_id = latest_ready_handoff
        .as_ref()
        .and_then(|handoff| handoff.checkpoint_id.as_ref())
        .or_else(|| task.and_then(|task| task.latest_handoff_checkpoint_id.as_ref()));
    let has_newer_intention = snapshot.latest_intention.as_ref().is_some_and(|intention| {
        let intention_at = normalize_timestamp_millis(intention.occurred_at_unix);
        latest_handoff_at.is_none_or(|handoff_at| intention_at > handoff_at)
    });
    let has_state_delta = !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
        || (latest_handoff_checkpoint_id != Some(checkpoint_id));
    if latest_handoff_at.is_none() {
        return (
            true,
            "Checkpoint is available and no handoff has been prepared yet.".to_string(),
        );
    }
    if has_newer_intention {
        return (
            true,
            "Checkpoint is available and a newer worker intention was recorded.".to_string(),
        );
    }
    if has_state_delta {
        return (
            true,
            "Checkpoint is available and task state changed since the last handoff.".to_string(),
        );
    }
    (
        false,
        "Checkpoint is already handed off and no newer intention or delta is available."
            .to_string(),
    )
}

pub(crate) fn slim_broker_response(
    response: &BrokerGetContextResponse,
    artifact_id: Option<String>,
) -> BrokerGetContextResponse {
    BrokerGetContextResponse {
        context_version: response.context_version.clone(),
        response_mode: BrokerResponseMode::Slim,
        artifact_id,
        latest_intention: response.latest_intention.clone(),
        next_action_summary: response.next_action_summary.clone(),
        handoff_ready: response.handoff_ready,
        stale: response.stale,
        brief: response.brief.clone(),
        supersedes_prior_context: response.supersedes_prior_context,
        supersession_mode: response.supersession_mode,
        superseded_before_version: response.superseded_before_version.clone(),
        sections: Vec::new(),
        est_tokens: response.est_tokens,
        est_bytes: response.est_bytes,
        budget_remaining_tokens: response.budget_remaining_tokens,
        budget_remaining_bytes: response.budget_remaining_bytes,
        section_estimates: Vec::new(),
        eviction_candidates: Vec::new(),
        delta: BrokerDeltaResponse::default(),
        working_set: Vec::new(),
        recommended_actions: Vec::new(),
        active_decisions: Vec::new(),
        open_questions: Vec::new(),
        resolved_questions: Vec::new(),
        changed_paths_since_checkpoint: Vec::new(),
        changed_symbols_since_checkpoint: Vec::new(),
        recent_tool_invocations: Vec::new(),
        tool_failures: Vec::new(),
        discovered_paths: Vec::new(),
        discovered_symbols: Vec::new(),
        evidence_artifact_ids: Vec::new(),
        invalidates_since_version: response.invalidates_since_version,
        effective_max_sections: response.effective_max_sections,
        effective_default_max_items_per_section: response.effective_default_max_items_per_section,
        effective_section_item_limits: response.effective_section_item_limits.clone(),
        diagnostics_ms: response.diagnostics_ms.clone(),
    }
}

fn broker_memory_kind_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> suite_packet_core::MemoryKind {
    state
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .tasks
                .tasks
                .get(task_id)
                .and_then(|task| task.latest_context_reason.clone())
        })
        .filter(|reason| reason == "prepare_handoff")
        .map(|_| suite_packet_core::MemoryKind::Handoff)
        .unwrap_or(suite_packet_core::MemoryKind::Brief)
}

fn broker_memory_summary(
    task_id: &str,
    memory_kind: suite_packet_core::MemoryKind,
    response: &BrokerGetContextResponse,
) -> String {
    let headline = response
        .next_action_summary
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            response
                .latest_intention
                .as_ref()
                .map(|intention| intention.text.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "resume the current task".to_string());
    match memory_kind {
        suite_packet_core::MemoryKind::Handoff => {
            format!("Checkpoint handoff for {task_id}: {headline}")
        }
        _ => format!("Current task context for {task_id}: {headline}"),
    }
}

fn persist_broker_memory_entry(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    response: &BrokerGetContextResponse,
) -> Result<()> {
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let memory_kind = broker_memory_kind_for_task(state, task_id);
    let summary = broker_memory_summary(task_id, memory_kind, response);
    let checkpoint_id = load_agent_snapshot_for_task(state, task_id)?.latest_checkpoint_id;
    let summary_hash = blake3::hash(summary.trim().as_bytes()).to_hex().to_string();
    let lineage = checkpoint_id
        .clone()
        .unwrap_or_else(|| "no-checkpoint".to_string());
    let dedupe_key = format!(
        "{task_id}:{}:{lineage}:{}",
        memory_kind.as_str(),
        &summary_hash[..12]
    );
    let latest_intention_text = response
        .latest_intention
        .as_ref()
        .map(|intention| intention.text.clone());
    let recommended_actions = response
        .recommended_actions
        .iter()
        .map(|action| action.summary.clone())
        .collect::<Vec<_>>();
    kernel.execute(KernelRequest {
        target: "packet28.broker_memory.write".to_string(),
        reducer_input: json!({
            "task_id": task_id,
            "memory_kind": memory_kind,
            "summary": summary,
            "brief": response.brief,
            "context_version": response.context_version,
            "checkpoint_id": checkpoint_id,
            "dedupe_key": dedupe_key,
            "artifact_id": response.artifact_id,
            "next_action_summary": response.next_action_summary,
            "latest_intention_text": latest_intention_text,
            "recommended_actions": recommended_actions,
            "evidence_artifact_ids": response.evidence_artifact_ids,
            "paths": response.discovered_paths,
            "symbols": response.discovered_symbols,
        }),
        policy_context: json!({
            "task_id": task_id,
        }),
        ..KernelRequest::default()
    })?;
    Ok(())
}

pub(crate) fn write_broker_artifacts(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    since_version: Option<&str>,
    response: &BrokerGetContextResponse,
) -> Result<String> {
    fence_task_namespace_admission(state, task_id)?;
    let root = state.lock().map_err(lock_err)?.root.clone();
    let storage_id = task_storage_id(task_id)?;
    let context_version = context_version_storage_id(&response.context_version)?;
    let brief_md_path = task_brief_markdown_path(&root, &storage_id);
    let brief_json_path = task_brief_json_path(&root, &storage_id);
    let state_json_path = task_state_json_path(&root, &storage_id);
    let version_json_path = task_version_json_path(&root, &storage_id, &context_version);
    let version_snapshot =
        build_version_snapshot_response(&root, task_id, since_version, response)?;
    if let Some(parent) = brief_md_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create task artifact dir '{}'", parent.display())
        })?;
    }
    if let Some(parent) = version_json_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create versioned broker artifact dir '{}'",
                parent.display()
            )
        })?;
    }
    fs::write(&brief_md_path, &response.brief)
        .with_context(|| format!("failed to write '{}'", brief_md_path.display()))?;
    fs::write(&brief_json_path, serde_json::to_vec_pretty(response)?)
        .with_context(|| format!("failed to write '{}'", brief_json_path.display()))?;
    fs::write(
        &version_json_path,
        serde_json::to_vec_pretty(&version_snapshot)?,
    )
    .with_context(|| format!("failed to write '{}'", version_json_path.display()))?;

    let hash = blake3::hash(serde_json::to_string(response)?.as_bytes())
        .to_hex()
        .to_string();
    let generated_at = now_unix_millis();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_brief_path = Some(brief_md_path.to_string_lossy().to_string());
        task.latest_brief_hash = Some(hash.clone());
        task.latest_brief_generated_at_unix = Some(generated_at);
        task.last_context_refresh_at_unix = Some(generated_at);
        let state_value = json!({
            "task_id": task_id,
            "context_version": task.latest_context_version,
            "latest_brief_path": task.latest_brief_path,
            "latest_brief_hash": task.latest_brief_hash,
            "latest_brief_generated_at_unix": task.latest_brief_generated_at_unix,
            "latest_context_reason": task.latest_context_reason,
            "brief_json_path": brief_json_path.to_string_lossy().to_string(),
            "event_path": task_event_log_path(&root, &storage_id).to_string_lossy().to_string(),
            "supports_push": true,
        });
        persist_task(&guard, task_id)?;
        fs::write(&state_json_path, serde_json::to_vec_pretty(&state_value)?)
            .with_context(|| format!("failed to write '{}'", state_json_path.display()))?;
    }
    persist_broker_memory_entry(state, task_id, response)?;
    Ok(hash)
}

pub(crate) fn build_version_snapshot_response(
    root: &Path,
    task_id: &str,
    since_version: Option<&str>,
    response: &BrokerGetContextResponse,
) -> Result<BrokerGetContextResponse> {
    if !matches!(response.response_mode, BrokerResponseMode::Delta) {
        return Ok(response.clone());
    }

    let previous = match since_version {
        Some(version) if version != response.context_version => {
            load_versioned_broker_response(root, task_id, version)?
        }
        _ => None,
    };

    let mut snapshot = response.clone();
    snapshot.response_mode = BrokerResponseMode::Full;
    snapshot.sections = merge_version_snapshot_sections(previous.as_ref(), response);
    snapshot.brief = render_brief(task_id, &snapshot.context_version, &snapshot.sections);
    let (est_tokens, est_bytes) = estimate_text_cost(&snapshot.brief);
    snapshot.est_tokens = est_tokens;
    snapshot.est_bytes = est_bytes;
    Ok(snapshot)
}

pub(crate) fn merge_version_snapshot_sections(
    previous: Option<&BrokerGetContextResponse>,
    response: &BrokerGetContextResponse,
) -> Vec<BrokerSection> {
    let Some(previous) = previous else {
        return response.delta.changed_sections.clone();
    };

    let changed_by_id = response
        .delta
        .changed_sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();
    let removed_ids = response
        .delta
        .removed_section_ids
        .iter()
        .map(|id| id.as_str())
        .collect::<HashSet<_>>();
    let mut merged = previous
        .sections
        .iter()
        .filter(|section| !removed_ids.contains(section.id.as_str()))
        .map(|section| {
            changed_by_id
                .get(section.id.as_str())
                .cloned()
                .cloned()
                .unwrap_or_else(|| section.clone())
        })
        .collect::<Vec<_>>();

    for section in &response.delta.changed_sections {
        if !previous
            .sections
            .iter()
            .any(|existing| existing.id == section.id)
        {
            merged.push(section.clone());
        }
    }

    merged
}

fn handoff_context_request(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerPrepareHandoffRequest,
) -> BrokerGetContextRequest {
    let fallback_query = state.lock().ok().and_then(|guard| {
        guard
            .tasks
            .tasks
            .get(&request.task_id)
            .and_then(|task| task.latest_broker_request.as_ref())
            .and_then(|previous| previous.query.clone())
    });
    BrokerGetContextRequest {
        task_id: request.task_id.clone(),
        action: Some(BrokerAction::Summarize),
        budget_tokens: Some(2_000),
        budget_bytes: Some(12_000),
        query: request.query.clone().or(fallback_query),
        include_sections: vec![
            "task_objective".to_string(),
            "agent_intention".to_string(),
            "task_memory".to_string(),
            "checkpoint_context".to_string(),
            "checkpoint_deltas".to_string(),
            "active_decisions".to_string(),
            "open_questions".to_string(),
            "resolved_questions".to_string(),
            "recent_tool_activity".to_string(),
            "evidence_cache".to_string(),
            "recommended_actions".to_string(),
        ],
        response_mode: Some(request.response_mode.unwrap_or(BrokerResponseMode::Slim)),
        max_sections: Some(7),
        default_max_items_per_section: Some(4),
        section_item_limits: BTreeMap::from([
            ("agent_intention".to_string(), 5),
            ("task_memory".to_string(), 5),
            ("checkpoint_context".to_string(), 5),
            ("checkpoint_deltas".to_string(), 6),
            ("active_decisions".to_string(), 4),
            ("open_questions".to_string(), 4),
            ("resolved_questions".to_string(), 4),
            ("recent_tool_activity".to_string(), 4),
            ("evidence_cache".to_string(), 4),
            ("recommended_actions".to_string(), 4),
        ]),
        persist_artifacts: Some(true),
        recall_mode: Some(context_memory_core::RecallMode::Conceptual),
        include_debug_memory: request.include_debug_memory,
        ..BrokerGetContextRequest::default()
    }
}

/// Prepares a broker handoff when the task has sufficient state for transfer.
///
/// When a handoff is not ready, returns readiness information and any existing handoff
/// metadata. If a matching ready handoff artifact is available, returns it for resumption.
/// When ready, generates and persists a new handoff artifact and returns its descriptor and
/// broker context.
///
/// # Examples
///
/// ```ignore
/// let response = broker_prepare_handoff(state, request)?;
/// if let Some(context) = response.context {
///     println!("Prepared context: {}", context.context_version);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub(crate) fn broker_prepare_handoff(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerPrepareHandoffRequest,
) -> Result<BrokerPrepareHandoffResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker prepare_handoff requires task_id");
    }
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let task = load_task_record(&state, &request.task_id);
    let latest_handoff = latest_handoff_descriptor(task.as_ref());
    let latest_ready_handoff = latest_ready_handoff_descriptor(task.as_ref());
    let (handoff_ready, handoff_reason) = compute_handoff_state(task.as_ref(), &snapshot);
    let warnings = handoff_contradiction_warnings(&snapshot);
    let latest_intention = snapshot.latest_intention.clone();
    let next_action_summary = next_action_summary(None, &snapshot);
    let readiness = handoff_readiness_score(
        handoff_ready,
        &warnings,
        &snapshot,
        next_action_summary.as_deref(),
    );
    if !handoff_ready {
        if let Some(existing_handoff) = latest_ready_handoff.as_ref() {
            if let Some(existing_context_version) = task
                .as_ref()
                .and_then(|task| task.latest_context_version.as_deref())
                .or(Some(existing_handoff.context_version.as_str()))
            {
                let root = state.lock().map_err(lock_err)?.root.clone();
                if let Some(existing_context) = load_versioned_broker_response(
                    &root,
                    &request.task_id,
                    existing_context_version,
                )? {
                    if existing_context.artifact_id.as_deref()
                        == Some(existing_handoff.artifact_id.as_str())
                    {
                        let context = if matches!(
                            request.response_mode.unwrap_or(BrokerResponseMode::Slim),
                            BrokerResponseMode::Slim
                        ) {
                            slim_broker_response(
                                &existing_context,
                                Some(existing_handoff.artifact_id.clone()),
                            )
                        } else {
                            existing_context
                        };
                        return Ok(BrokerPrepareHandoffResponse {
                            task_id: request.task_id,
                            handoff_ready: true,
                            handoff_reason: "Latest handoff artifact is available for resume."
                                .to_string(),
                            warnings,
                            readiness,
                            latest_checkpoint_id: snapshot.latest_checkpoint_id,
                            handoff: Some(existing_handoff.clone()),
                            latest_handoff_artifact_id: Some(existing_handoff.artifact_id.clone()),
                            latest_handoff_generated_at_unix: Some(
                                existing_handoff.generated_at_unix_ms,
                            ),
                            latest_handoff_checkpoint_id: existing_handoff.checkpoint_id.clone(),
                            latest_intention,
                            next_action_summary: context.next_action_summary.clone(),
                            context: Some(context),
                        });
                    }
                }
            }
        }
        return Ok(BrokerPrepareHandoffResponse {
            task_id: request.task_id,
            handoff_ready,
            handoff_reason,
            warnings,
            readiness,
            latest_checkpoint_id: snapshot.latest_checkpoint_id,
            handoff: latest_handoff.clone(),
            latest_handoff_artifact_id: latest_handoff
                .as_ref()
                .map(|handoff| handoff.artifact_id.clone())
                .or_else(|| {
                    task.as_ref()
                        .and_then(|task| task.latest_handoff_artifact_id.clone())
                }),
            latest_handoff_generated_at_unix: latest_handoff
                .as_ref()
                .map(|handoff| handoff.generated_at_unix_ms)
                .or_else(|| {
                    task.as_ref()
                        .and_then(|task| task.latest_handoff_generated_at_unix)
                }),
            latest_handoff_checkpoint_id: latest_handoff
                .as_ref()
                .and_then(|handoff| handoff.checkpoint_id.clone())
                .or_else(|| {
                    task.as_ref()
                        .and_then(|task| task.latest_handoff_checkpoint_id.clone())
                }),
            latest_intention,
            next_action_summary,
            context: None,
        });
    }

    let get_request = handoff_context_request(&state, &request);
    let _ = set_context_reason(&state, &request.task_id, "prepare_handoff");
    let mut context = compute_broker_response(&state, &get_request)?;
    context.artifact_id = Some(context.context_version.clone());
    write_broker_artifacts(
        &state,
        &request.task_id,
        get_request.since_version.as_deref(),
        &context,
    )?;
    let generated_at = now_unix_millis();
    let artifact_id = context
        .artifact_id
        .clone()
        .unwrap_or_else(|| context.context_version.clone());
    let handoff = BrokerHandoffDescriptor {
        handoff_id: derive_handoff_id(&request.task_id, generated_at),
        task_id: request.task_id.clone(),
        artifact_id: artifact_id.clone(),
        context_version: context.context_version.clone(),
        checkpoint_id: snapshot.latest_checkpoint_id.clone(),
        status: BrokerHandoffStatus::Ready,
        generated_at_unix_ms: generated_at,
        consumed_at_unix_ms: None,
        superseded_by_handoff_id: None,
        resume_count: 0,
    };
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
        promote_new_ready_handoff(task, handoff.clone());
        persist_task(&guard, &request.task_id)?;
    }
    let context = if matches!(
        get_request
            .response_mode
            .unwrap_or(BrokerResponseMode::Slim),
        BrokerResponseMode::Slim
    ) {
        slim_broker_response(&context, context.artifact_id.clone())
    } else {
        context
    };
    Ok(BrokerPrepareHandoffResponse {
        task_id: request.task_id,
        handoff_ready: true,
        handoff_reason,
        warnings,
        readiness,
        latest_checkpoint_id: snapshot.latest_checkpoint_id.clone(),
        handoff: Some(handoff),
        latest_handoff_artifact_id: Some(artifact_id),
        latest_handoff_generated_at_unix: Some(generated_at),
        latest_handoff_checkpoint_id: snapshot.latest_checkpoint_id,
        latest_intention,
        next_action_summary: context.next_action_summary.clone(),
        context: Some(context),
    })
}

#[cfg(test)]
mod cap_history_tests {
    use super::*;

    /// Creates a handoff descriptor with deterministic handoff and artifact identifiers.
    ///
    /// # Examples
    ///
    /// ```
    /// let descriptor = handoff(7, 1_700_000_000_000);
    /// assert_eq!(descriptor.handoff_id, "handoff-00007");
    /// assert_eq!(descriptor.artifact_id, "artifact-00007");
    /// assert_eq!(descriptor.generated_at_unix_ms, 1_700_000_000_000);
    /// ```
    fn handoff(id: usize, generated_at_unix_ms: u64) -> BrokerHandoffDescriptor {
        BrokerHandoffDescriptor {
            handoff_id: format!("handoff-{id:05}"),
            artifact_id: format!("artifact-{id:05}"),
            generated_at_unix_ms,
            ..BrokerHandoffDescriptor::default()
        }
    }

    #[test]
    fn promote_bounds_handoff_history_to_the_newest() {
        let mut task = TaskRecord::default();
        let total = TASK_HANDOFF_HISTORY_MAX + 40;
        for index in 0..total {
            promote_new_ready_handoff(&mut task, handoff(index, index as u64 + 1));
        }

        assert_eq!(task.handoffs.len(), TASK_HANDOFF_HISTORY_MAX);
        let expected_ids: Vec<_> = ((total - TASK_HANDOFF_HISTORY_MAX)..total)
            .rev()
            .map(|index| format!("handoff-{index:05}"))
            .collect();
        let retained_ids: Vec<_> = task
            .handoffs
            .iter()
            .map(|descriptor| descriptor.handoff_id.clone())
            .collect();
        assert_eq!(retained_ids, expected_ids);
        assert_eq!(
            task.latest_handoff_id.as_deref(),
            Some(format!("handoff-{:05}", total - 1).as_str())
        );
        assert_eq!(
            task.handoffs.first().map(|descriptor| descriptor.status),
            Some(BrokerHandoffStatus::Ready)
        );
        assert!(task
            .handoffs
            .iter()
            .skip(1)
            .all(|descriptor| descriptor.status == BrokerHandoffStatus::Superseded));
    }

    #[test]
    fn cap_handoff_history_is_noop_below_bound() {
        let mut handoffs: Vec<BrokerHandoffDescriptor> =
            (0..8).map(|index| handoff(index, index as u64)).collect();
        cap_handoff_history(&mut handoffs);
        assert_eq!(handoffs.len(), 8);
    }

    #[test]
    fn cap_handoff_history_is_noop_at_bound() {
        let mut handoffs: Vec<_> = (0..TASK_HANDOFF_HISTORY_MAX)
            .rev()
            .map(|index| handoff(index, index as u64))
            .collect();
        let ids_before: Vec<_> = handoffs
            .iter()
            .map(|descriptor| descriptor.handoff_id.clone())
            .collect();

        cap_handoff_history(&mut handoffs);

        assert_eq!(handoffs.len(), TASK_HANDOFF_HISTORY_MAX);
        assert_eq!(
            handoffs
                .iter()
                .map(|descriptor| descriptor.handoff_id.clone())
                .collect::<Vec<_>>(),
            ids_before
        );
    }

    #[test]
    fn promotion_past_bound_evicts_only_oldest_and_updates_latest_metadata() {
        let mut task = TaskRecord::default();
        for index in 0..TASK_HANDOFF_HISTORY_MAX {
            promote_new_ready_handoff(&mut task, handoff(index, index as u64));
        }
        let mut newest = handoff(TASK_HANDOFF_HISTORY_MAX, TASK_HANDOFF_HISTORY_MAX as u64);
        newest.checkpoint_id = Some("checkpoint-new".to_string());

        promote_new_ready_handoff(&mut task, newest.clone());

        assert_eq!(task.handoffs.len(), TASK_HANDOFF_HISTORY_MAX);
        assert_eq!(task.handoffs.first(), Some(&newest));
        assert!(!task
            .handoffs
            .iter()
            .any(|descriptor| descriptor.handoff_id == "handoff-00000"));
        let previous = task
            .handoffs
            .iter()
            .find(|descriptor| descriptor.handoff_id == "handoff-00063")
            .unwrap();
        assert_eq!(previous.status, BrokerHandoffStatus::Superseded);
        assert_eq!(
            previous.superseded_by_handoff_id.as_deref(),
            Some(newest.handoff_id.as_str())
        );
        assert_eq!(
            task.latest_handoff_id.as_deref(),
            Some(newest.handoff_id.as_str())
        );
        assert_eq!(
            task.latest_handoff_artifact_id.as_deref(),
            Some(newest.artifact_id.as_str())
        );
        assert_eq!(
            task.latest_handoff_generated_at_unix,
            Some(newest.generated_at_unix_ms)
        );
        assert_eq!(
            task.latest_handoff_checkpoint_id.as_deref(),
            Some("checkpoint-new")
        );
    }

    #[test]
    fn promotion_orders_equal_timestamps_by_handoff_id() {
        let mut task = TaskRecord::default();
        promote_new_ready_handoff(&mut task, handoff(2, 99));
        promote_new_ready_handoff(&mut task, handoff(0, 99));
        promote_new_ready_handoff(&mut task, handoff(1, 99));

        let ordered_ids: Vec<_> = task
            .handoffs
            .iter()
            .map(|descriptor| descriptor.handoff_id.as_str())
            .collect();
        assert_eq!(
            ordered_ids,
            vec!["handoff-00000", "handoff-00001", "handoff-00002"]
        );
        assert_eq!(task.latest_handoff_id.as_deref(), Some("handoff-00001"));
        assert_eq!(
            task.handoffs
                .iter()
                .find(|descriptor| descriptor.status == BrokerHandoffStatus::Ready)
                .map(|descriptor| descriptor.handoff_id.as_str()),
            Some("handoff-00001")
        );
    }
}
