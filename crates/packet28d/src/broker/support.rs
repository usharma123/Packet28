use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use context_kernel_core::{Kernel, KernelRequest};
use packet28_daemon_core::storage::now_unix;
use packet28_daemon_protocol::broker::{
    BrokerAction, BrokerGetContextRequest, BrokerResponseMode, BrokerWriteOp,
    BrokerWriteStateRequest,
};
use packet28_daemon_protocol::message::{DaemonEvent, DaemonResponse, DaemonStatus};
use packet28_daemon_protocol::registry::{
    DaemonRegistryResponseV1, DaemonStatusV1, MAX_DAEMON_STATUS_V1_RESPONSE_BYTES,
};
use packet28_daemon_protocol::task::TaskRecord;
use serde_json::{json, Value};

use crate::index::build_index_status;
use crate::state::{DaemonState, TaskGenerationId};
use crate::{daemon_log, lock_err, mark_task_dirty, persist_task, resolve_root};

const DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS: u64 = 5_000;
const DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES: usize = 32_000;
const MAX_LEGACY_STATUS_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) fn kernel_for_request(
    state: &Arc<Mutex<DaemonState>>,
    request: &KernelRequest,
) -> Result<Arc<Kernel>> {
    let root = match persist_root_override(&request.target, &request.policy_context) {
        Some(root) => resolve_root(Path::new(&root)),
        None => state.lock().map_err(lock_err)?.root.clone(),
    };
    kernel_for_root(state, &root)
}

pub(crate) fn kernel_for_context_root(
    state: &Arc<Mutex<DaemonState>>,
    root: &str,
) -> Result<Arc<Kernel>> {
    if root.is_empty() {
        let root = state.lock().map_err(lock_err)?.root.clone();
        kernel_for_root(state, &root)
    } else {
        kernel_for_root(state, Path::new(root))
    }
}

fn kernel_for_root(state: &Arc<Mutex<DaemonState>>, root: &Path) -> Result<Arc<Kernel>> {
    let registry = state.lock().map_err(lock_err)?.kernel_registry.clone();
    Ok(registry.get(root)?)
}

fn persist_root_override(target: &str, policy_context: &Value) -> Option<String> {
    if !matches!(target, "agenty.state.write" | "agenty.state.snapshot") {
        return None;
    }

    policy_context
        .get("persist_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn build_status(state: &DaemonState) -> Result<DaemonStatus> {
    let mut status = DaemonStatus {
        pid: state.runtime.pid,
        version: state.runtime.version.clone(),
        socket_path: state.runtime.socket_path.clone(),
        workspace_root: state.runtime.workspace_root.clone(),
        started_at_unix: state.runtime.started_at_unix,
        ready_at_unix: state.runtime.ready_at_unix,
        log_path: state.runtime.log_path.clone(),
        uptime_secs: now_unix().saturating_sub(state.runtime.started_at_unix),
        tasks: Vec::new(),
        watches: Vec::new(),
        index: Some(build_index_status(&state.interactive_index)),
    };
    let mut encoded_bytes = encoded_legacy_status_response_bytes(&status)?;
    ensure_legacy_status_size(encoded_bytes)?;
    for task in state.tasks.tasks.values() {
        encoded_bytes =
            add_legacy_status_item_size(encoded_bytes, status.tasks.len(), task, "task")?;
        status.tasks.push(task.clone());
    }
    for watch in &state.watches.watches {
        encoded_bytes =
            add_legacy_status_item_size(encoded_bytes, status.watches.len(), watch, "watch")?;
        status.watches.push(watch.clone());
    }
    debug_assert_eq!(
        encoded_legacy_status_response_bytes(&status)?,
        encoded_bytes
    );
    Ok(status)
}

pub(crate) fn build_registry_status_v1(state: &DaemonState) -> Result<DaemonStatusV1> {
    let mut status = DaemonStatusV1 {
        pid: state.runtime.pid,
        version: state.runtime.version.clone(),
        socket_path: state.runtime.socket_path.clone(),
        workspace_root: state.runtime.workspace_root.clone(),
        started_at_unix: state.runtime.started_at_unix,
        ready_at_unix: state.runtime.ready_at_unix,
        log_path: state.runtime.log_path.clone(),
        uptime_secs: now_unix().saturating_sub(state.runtime.started_at_unix),
        task_count: state.tasks.tasks.len(),
        watch_count: state.watches.watches.len(),
        registry_revision: Some(state.registry_revision()),
        index_truncated: false,
        index: Some(build_index_status(&state.interactive_index)),
        record_size_warning_count: 0,
        record_size_warnings: Vec::new(),
    };
    (
        status.record_size_warning_count,
        status.record_size_warnings,
    ) = state.record_sizes.status_warnings();
    bound_registry_status_index_details(&mut status)?;
    Ok(status)
}

fn bound_registry_status_index_details(status: &mut DaemonStatusV1) -> Result<()> {
    if encoded_registry_status_response_bytes(status)? <= MAX_DAEMON_STATUS_V1_RESPONSE_BYTES {
        return Ok(());
    }
    if let Some(index) = status.index.as_mut() {
        index.manifest.dirty_paths.clear();
        index.manifest.queued_paths.clear();
        status.index_truncated = true;
    }
    if encoded_registry_status_response_bytes(status)? <= MAX_DAEMON_STATUS_V1_RESPONSE_BYTES {
        return Ok(());
    }
    status.index = None;
    status.index_truncated = true;
    if encoded_registry_status_response_bytes(status)? <= MAX_DAEMON_STATUS_V1_RESPONSE_BYTES {
        return Ok(());
    }
    Err(anyhow!(
        "daemon registry status metadata exceeds its liveness response bound even without index details"
    ))
}

fn encoded_legacy_status_response_bytes(status: &DaemonStatus) -> Result<usize> {
    serde_json::to_vec(&DaemonResponse::Status {
        status: status.clone(),
    })
    .map(|bytes| bytes.len())
    .context("failed to size legacy daemon status response")
}

fn encoded_registry_status_response_bytes(status: &DaemonStatusV1) -> Result<usize> {
    serde_json::to_vec(&DaemonRegistryResponseV1::Status {
        status: Box::new(status.clone()),
    })
    .map(|bytes| bytes.len())
    .context("failed to size daemon registry status response")
}

fn add_legacy_status_item_size(
    encoded_bytes: usize,
    preceding_items: usize,
    item: &impl serde::Serialize,
    kind: &str,
) -> Result<usize> {
    let item_bytes = serde_json::to_vec(item)
        .with_context(|| format!("failed to size legacy daemon status {kind} record"))?
        .len();
    let encoded_bytes = encoded_bytes
        .checked_add(item_bytes)
        .and_then(|bytes| bytes.checked_add(usize::from(preceding_items != 0)))
        .ok_or_else(|| anyhow!("legacy daemon status size overflow"))?;
    ensure_legacy_status_size(encoded_bytes)?;
    Ok(encoded_bytes)
}

fn ensure_legacy_status_size(encoded_bytes: usize) -> Result<()> {
    if encoded_bytes > MAX_LEGACY_STATUS_RESPONSE_BYTES {
        anyhow::bail!(
            "legacy status requires more than {MAX_LEGACY_STATUS_RESPONSE_BYTES} bytes; \
             use registry_status_v1 and the versioned registry page requests"
        );
    }
    Ok(())
}

pub(crate) fn emit_task_event(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    kind: &str,
    data: Value,
) -> Result<()> {
    let _ = emit_task_event_ordered(state, task_id, None, kind, data, true)?;
    Ok(())
}

fn emit_task_event_ordered(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: Option<TaskGenerationId>,
    kind: &str,
    data: Value,
    create_task: bool,
) -> Result<bool> {
    let persistence = state.lock().map_err(lock_err)?.persistence.clone();
    let _event_guard = persistence.event_guard();
    let (_activity_lease, required_revision, require_checkpoint, prepare_lock_hold) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let lock_acquired = Instant::now();
        guard.require_task_mutable(task_id)?;
        if !guard.tasks.tasks.contains_key(task_id) {
            if !create_task {
                return Ok(false);
            }
            guard.tasks.tasks.insert(
                task_id.to_string(),
                TaskRecord {
                    task_id: task_id.to_string(),
                    ..TaskRecord::default()
                },
            );
        }

        let generation = match expected_generation {
            Some(expected) => {
                let Some(current) = guard.task_generations.current(task_id) else {
                    return Ok(false);
                };
                if current.id() != expected || current.is_cancelled() {
                    return Ok(false);
                }
                current
            }
            None => guard.task_generations.ensure(task_id)?,
        };
        let Some(activity_lease) = generation.acquire_operation() else {
            return Ok(false);
        };
        let require_checkpoint = !persistence.task_is_durably_admitted(task_id);
        let required_revision = if require_checkpoint {
            mark_task_dirty(&guard, task_id)?
        } else {
            persistence.latest_revision()
        };
        (
            activity_lease,
            required_revision,
            require_checkpoint,
            lock_acquired.elapsed(),
        )
    };
    persistence.record_event_state_lock_hold(prepare_lock_hold);

    let frame = persistence.append_event(
        task_id,
        DaemonEvent {
            kind: kind.to_string(),
            occurred_at_unix: now_unix(),
            data,
        },
        required_revision,
        require_checkpoint,
    )?;

    let mut guard = state.lock().map_err(lock_err)?;
    let lock_acquired = Instant::now();
    guard.require_task_mutable(task_id)?;
    let task = guard
        .tasks
        .tasks
        .get_mut(task_id)
        .ok_or_else(|| anyhow!("task '{task_id}' disappeared after durable event append"))?;
    if task.last_event_seq > frame.seq {
        anyhow::bail!(
            "task '{}' in-memory high-water {} is ahead of appended event sequence {}",
            task_id,
            task.last_event_seq,
            frame.seq
        );
    }
    task.last_event_seq = frame.seq;
    persist_task(&guard, task_id)?;
    publish_task_event_to_subscribers(&mut guard, task_id, &frame);
    guard.changes.notify();
    let publication_lock_hold = lock_acquired.elapsed();
    drop(guard);
    persistence.record_event_state_lock_hold(publication_lock_hold);
    Ok(true)
}

fn publish_task_event_to_subscribers(
    state: &mut DaemonState,
    task_id: &str,
    frame: &packet28_daemon_protocol::message::DaemonEventFrame,
) {
    if let Some(subscribers) = state.subscribers.get_mut(task_id) {
        subscribers.retain(
            |subscriber| match subscriber.sender.try_send(frame.clone()) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    daemon_log(&format!(
                        "subscriber lagged task_id={task_id} subscriber_id={}; \
                         closing stream for replay from sequence {}",
                        subscriber.id,
                        frame.seq.saturating_sub(1)
                    ));
                    false
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            },
        );
        if subscribers.is_empty() {
            state.subscribers.remove(task_id);
        }
    }
}

pub(crate) fn complete_task_cancellation_for_generation(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    generation: TaskGenerationId,
    removed_watch_ids: &[String],
) -> Result<Option<TaskRecord>> {
    let persistence = state.lock().map_err(lock_err)?.persistence.clone();
    let _event_guard = persistence.event_guard();
    let (required_revision, require_checkpoint, prepare_lock_hold) = {
        let guard = state.lock().map_err(lock_err)?;
        let lock_acquired = Instant::now();
        let Some(task) = guard.tasks.tasks.get(task_id) else {
            return Ok(None);
        };
        if task.lifecycle.is_cancelled() {
            return Ok(Some(task.clone()));
        }
        let Some(current) = guard.task_generations.current(task_id) else {
            anyhow::bail!("task '{task_id}' lost its generation before cancellation completed");
        };
        if current.id() != generation || !current.is_cancelled() {
            anyhow::bail!("task '{task_id}' generation changed before cancellation completed");
        }
        if !task.lifecycle.is_cancelling() {
            anyhow::bail!(
                "task '{task_id}' lifecycle is {:?}, not cancelling",
                task.lifecycle
            );
        }
        let require_checkpoint = !persistence.task_is_durably_admitted(task_id);
        let required_revision = if require_checkpoint {
            mark_task_dirty(&guard, task_id)?
        } else {
            persistence.latest_revision()
        };
        (
            required_revision,
            require_checkpoint,
            lock_acquired.elapsed(),
        )
    };
    persistence.record_event_state_lock_hold(prepare_lock_hold);

    let frame = persistence.append_event(
        task_id,
        DaemonEvent {
            kind: "task_cancelled".to_string(),
            occurred_at_unix: now_unix(),
            data: json!({
                "task_id": task_id,
                "removed_watch_ids": removed_watch_ids,
            }),
        },
        required_revision,
        require_checkpoint,
    )?;

    let mut guard = state.lock().map_err(lock_err)?;
    let lock_acquired = Instant::now();
    let current = guard.task_generations.current(task_id).ok_or_else(|| {
        anyhow!("task '{task_id}' lost its generation after cancellation was recorded")
    })?;
    if current.id() != generation || !current.is_cancelled() {
        anyhow::bail!("task '{task_id}' generation changed after cancellation was recorded");
    }
    guard.require_task_mutable(task_id)?;
    let task =
        guard.tasks.tasks.get_mut(task_id).ok_or_else(|| {
            anyhow!("task '{task_id}' disappeared after cancellation was recorded")
        })?;
    if task.last_event_seq > frame.seq {
        anyhow::bail!(
            "task '{}' in-memory high-water {} is ahead of cancellation event sequence {}",
            task_id,
            task.last_event_seq,
            frame.seq
        );
    }
    task.last_event_seq = frame.seq;
    task.last_completed_at_unix = Some(frame.event.occurred_at_unix);
    task.lifecycle.complete_cancel()?;
    let terminal_task = task.clone();
    let checkpoint_result = persist_task(&guard, task_id);
    publish_task_event_to_subscribers(&mut guard, task_id, &frame);
    guard.subscribers.remove(task_id);
    guard
        .task_generations
        .remove_if_current(task_id, generation);
    guard.changes.notify();
    let publication_lock_hold = lock_acquired.elapsed();
    drop(guard);
    persistence.record_event_state_lock_hold(publication_lock_hold);
    checkpoint_result?;
    Ok(Some(terminal_task))
}

pub(crate) fn emit_task_event_for_generation(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    generation: TaskGenerationId,
    kind: &str,
    data: Value,
) -> Result<bool> {
    emit_task_event_ordered(state, task_id, Some(generation), kind, data, false)
}

pub(crate) fn refresh_task_context_summary_for_generation(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    generation: TaskGenerationId,
) -> Result<Option<Value>> {
    let kernel = {
        let guard = state.lock().map_err(lock_err)?;
        let Some(current) = guard.task_generations.current(task_id) else {
            return Ok(None);
        };
        if current.id() != generation || current.is_cancelled() {
            return Ok(None);
        }
        guard.kernel.clone()
    };
    let response = match kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": task_id,
            "budget_tokens": DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS,
            "budget_bytes": DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES,
            "scope": "task_first",
        }),
        ..KernelRequest::default()
    }) {
        Ok(response) => response,
        Err(err) => {
            daemon_log(&format!(
                "context manage refresh failed task_id={task_id}: {err}"
            ));
            return Ok(None);
        }
    };
    let Some(packet) = response.output_packets.first() else {
        return Ok(None);
    };
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!(source.to_string()))?;
    let summary = json!({
        "working_set_tokens": envelope.payload.budget.working_set_tokens,
        "evictable_tokens": envelope.payload.budget.evictable_tokens,
        "changed_paths_since_checkpoint": envelope.payload.changed_paths_since_checkpoint.len(),
        "changed_symbols_since_checkpoint": envelope.payload.changed_symbols_since_checkpoint.len(),
    });
    let mut guard = state.lock().map_err(lock_err)?;
    let Some(current) = guard.task_generations.current(task_id) else {
        return Ok(None);
    };
    if current.id() != generation || current.is_cancelled() {
        return Ok(None);
    }
    guard.require_task_mutable(task_id)?;
    if let Some(task) = guard.tasks.tasks.get_mut(task_id) {
        task.last_context_refresh_at_unix = Some(now_unix());
        task.working_set_est_tokens = envelope.payload.budget.working_set_tokens;
        task.evictable_est_tokens = envelope.payload.budget.evictable_tokens;
        task.changed_since_checkpoint_paths = envelope.payload.changed_paths_since_checkpoint.len();
        task.changed_since_checkpoint_symbols =
            envelope.payload.changed_symbols_since_checkpoint.len();
    }
    persist_task(&guard, task_id)?;
    Ok(Some(summary))
}

pub(crate) fn broker_default_budget_tokens() -> u64 {
    DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS
}

pub(crate) fn broker_default_budget_bytes() -> usize {
    DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES
}

/// Returns the mutable record for `task_id`, creating an empty record when
/// absent.
///
/// # Errors
///
/// Fails when the task is fenced for record maintenance or is an archived
/// tombstone; see [`DaemonState::require_task_mutable`].
pub(crate) fn ensure_task_record_mut<'a>(
    state: &'a mut DaemonState,
    task_id: &str,
) -> Result<&'a mut TaskRecord> {
    state.require_task_mutable(task_id)?;
    Ok(state
        .tasks
        .tasks
        .entry(task_id.to_string())
        .or_insert_with(|| TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        }))
}

fn next_context_version(current: Option<&str>) -> String {
    current
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1)
        .to_string()
}

pub(crate) fn ensure_context_version(task: &mut TaskRecord) -> String {
    let version = task
        .latest_context_version
        .clone()
        .unwrap_or_else(|| next_context_version(None));
    task.latest_context_version = Some(version.clone());
    version
}

pub(crate) fn bump_context_version(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard, task_id)?;
    let version = next_context_version(task.latest_context_version.as_deref());
    task.latest_context_version = Some(version.clone());
    persist_task(&guard, task_id)?;
    Ok(version)
}

pub(crate) fn set_context_reason(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard, task_id)?;
    task.latest_context_reason = Some(reason.into());
    persist_task(&guard, task_id)?;
    Ok(())
}

pub(crate) fn set_context_reason_for_generation(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    generation: TaskGenerationId,
    reason: impl Into<String>,
) -> Result<bool> {
    let mut guard = state.lock().map_err(lock_err)?;
    let Some(current) = guard.task_generations.current(task_id) else {
        return Ok(false);
    };
    if current.id() != generation || current.is_cancelled() {
        return Ok(false);
    }
    guard.require_task_mutable(task_id)?;
    let Some(task) = guard.tasks.tasks.get_mut(task_id) else {
        return Ok(false);
    };
    task.latest_context_reason = Some(reason.into());
    persist_task(&guard, task_id)?;
    Ok(true)
}

pub(crate) fn current_context_version(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let version = ensure_context_version(ensure_task_record_mut(&mut guard, task_id)?);
    persist_task(&guard, task_id)?;
    Ok(version)
}

pub(crate) fn update_broker_link_state(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerWriteStateRequest,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard, &request.task_id)?;
    let mut changed = false;
    match request.op.unwrap_or(BrokerWriteOp::FileRead) {
        BrokerWriteOp::QuestionOpen => {
            if let (Some(question_id), Some(text)) = (&request.question_id, &request.text) {
                task.question_texts
                    .insert(question_id.clone(), text.clone());
                task.resolved_questions.remove(question_id);
                changed = true;
            }
        }
        BrokerWriteOp::QuestionResolve => {
            if let Some(question_id) = &request.question_id {
                task.question_texts
                    .entry(question_id.clone())
                    .or_insert_with(|| "resolved question".to_string());
                if let Some(decision_id) = &request.resolution_decision_id {
                    task.resolved_questions
                        .insert(question_id.clone(), decision_id.clone());
                    task.linked_decisions
                        .insert(decision_id.clone(), question_id.clone());
                } else {
                    task.resolved_questions
                        .entry(question_id.clone())
                        .or_default();
                }
                changed = true;
            }
        }
        BrokerWriteOp::DecisionAdd => {
            if let (Some(decision_id), Some(question_id)) =
                (&request.decision_id, &request.resolves_question_id)
            {
                task.linked_decisions
                    .insert(decision_id.clone(), question_id.clone());
                task.resolved_questions
                    .insert(question_id.clone(), decision_id.clone());
                changed = true;
            }
        }
        BrokerWriteOp::DecisionSupersede => {
            if let Some(decision_id) = &request.decision_id {
                task.linked_decisions.remove(decision_id);
                task.resolved_questions
                    .retain(|_, linked_decision_id| linked_decision_id != decision_id);
                changed = true;
            }
        }
        _ => {}
    }
    if changed {
        persist_task(&guard, &request.task_id)?;
    }
    Ok(())
}

/// Publishes constant-size recovery links from the trusted registry.
/// Historical packets retain their original owner and are read at snapshot time.
/// Repeating the exact write is idempotent, including after interrupted startup.
pub(crate) fn inherit_recovered_agent_state(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let (kernel, links) = {
        let guard = state.lock().map_err(lock_err)?;
        let links = guard
            .tasks
            .tasks
            .values()
            .filter_map(|task| {
                task.recovered_from
                    .as_ref()
                    .map(|link| (task.task_id.clone(), link.predecessor_task_id.clone()))
            })
            .collect::<Vec<_>>();
        (guard.kernel.clone(), links)
    };
    for (task_id, predecessor_task_id) in links {
        kernel
            .execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: json!({
                    "task_id": task_id,
                    "event_id": format!("history-recovery:{predecessor_task_id}"),
                    "occurred_at_unix": 0,
                    "actor": "packet28d",
                    "kind": "recovered_from",
                    "data": {"type": "recovered_from", "predecessor_task_id": predecessor_task_id},
                }),
                ..KernelRequest::default()
            })
            .with_context(|| {
                format!(
                    "failed to inherit agent state from '{predecessor_task_id}' for '{task_id}'"
                )
            })?;
        state
            .lock()
            .map_err(lock_err)?
            .agent_snapshots
            .remove(&task_id);
    }
    Ok(())
}

pub(crate) fn load_agent_snapshot_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<suite_packet_core::AgentSnapshotPayload> {
    if let Some(snapshot) = state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .get(task_id)
        .cloned()
    {
        return Ok(snapshot);
    }
    let (kernel, lineage) = {
        let guard = state.lock().map_err(lock_err)?;
        let mut lineage = vec![task_id.to_string()];
        let mut current = task_id;
        while let Some(link) = guard
            .tasks
            .tasks
            .get(current)
            .and_then(|task| task.recovered_from.as_ref())
        {
            if link.successor_task_id != current
                || guard
                    .tasks
                    .tasks
                    .get(&link.predecessor_task_id)
                    .and_then(|task| task.superseded_by.as_ref())
                    != Some(link)
                || lineage.contains(&link.predecessor_task_id)
            {
                anyhow::bail!("invalid recovery lineage for task {task_id:?}");
            }
            lineage.push(link.predecessor_task_id.clone());
            current = &link.predecessor_task_id;
        }
        lineage.reverse();
        (guard.kernel.clone(), lineage)
    };
    let response = kernel.execute(KernelRequest {
        target: "agenty.state.snapshot".to_string(),
        reducer_input: json!({ "task_id": task_id, "recovery_lineage": lineage }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no agent snapshot packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid agent snapshot packet: {source}"))?;
    let snapshot = envelope.payload;
    state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .insert(task_id.to_string(), snapshot.clone());
    Ok(snapshot)
}

pub(crate) fn load_context_manage_for_task(
    kernel: &Arc<context_kernel_core::Kernel>,
    request: &BrokerGetContextRequest,
    focus_paths: &[String],
    focus_symbols: &[String],
) -> Result<suite_packet_core::ContextManagePayload> {
    let response = kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": request.task_id,
            "query": request.query,
            "budget_tokens": request.budget_tokens.unwrap_or_else(broker_default_budget_tokens),
            "budget_bytes": request.budget_bytes.unwrap_or_else(broker_default_budget_bytes),
            "scope": "task_first",
            "mode": request.recall_mode.unwrap_or_else(|| {
                if matches!(request.action.unwrap_or(BrokerAction::Plan), BrokerAction::Inspect) {
                    context_memory_core::RecallMode::Telemetry
                } else {
                    context_memory_core::RecallMode::Conceptual
                }
            }),
            "include_debug": request.include_debug_memory,
            "focus_paths": focus_paths,
            "focus_symbols": focus_symbols,
        }),
        policy_context: json!({
            "task_id": request.task_id,
        }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no context manage packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid context manage packet: {source}"))?;
    Ok(envelope.payload)
}

#[cfg(test)]
pub(crate) fn metadata_mtime_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn build_repo_map_envelope(
    state: &Arc<Mutex<DaemonState>>,
    root: &Path,
    focus_paths: &[String],
    focus_symbols: &[String],
    max_files: usize,
    max_symbols: usize,
) -> Result<suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>> {
    let runtime = {
        let guard = state.lock().map_err(lock_err)?;
        if guard.root != root {
            anyhow::bail!(
                "broker repository map root '{}' does not match daemon root '{}'",
                root.display(),
                guard.root.display()
            );
        }
        if !guard.interactive_index.repo_is_query_ready() {
            anyhow::bail!("authenticated daemon repository map runtime is not current");
        }
        guard
            .interactive_index
            .repo_runtime
            .clone()
            .ok_or_else(|| anyhow!("authenticated daemon repository map runtime is unavailable"))?
    };
    mapy_core::build_repo_map_from_runtime(
        mapy_core::RepoMapRequest {
            repo_root: root.to_string_lossy().to_string(),
            focus_paths: focus_paths.to_vec(),
            focus_symbols: focus_symbols.to_vec(),
            max_files,
            max_symbols,
            include_tests: true,
        },
        &runtime,
    )
    .map_err(|source| anyhow!(source.to_string()))
}

pub(crate) fn load_cached_coverage(root: &Path) -> Result<Option<suite_packet_core::CoverageData>> {
    let path = root.join(".covy").join("state").join("latest.bin");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cached coverage state '{}'", path.display()))?;
    let coverage = suite_foundation_core::cache::deserialize_coverage(&bytes)
        .map_err(|source| anyhow!(source.to_string()))?;
    Ok(Some(coverage))
}

pub(crate) fn load_cached_testmap(root: &Path) -> Result<Option<suite_packet_core::TestMapIndex>> {
    let path = root.join(".covy").join("state").join("testmap.bin");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(testy_core::pipeline_testmap::load_testmap(&path)?))
}

pub(crate) fn broker_objective(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
) -> Option<String> {
    if let Some(query) = request
        .query
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return Some(query.to_string());
    }
    let guard = state.lock().ok()?;
    guard
        .tasks
        .tasks
        .get(&request.task_id)
        .and_then(|task| task.latest_broker_request.as_ref())
        .and_then(|previous| previous.query.as_ref())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn request_query_missing(request: &BrokerGetContextRequest) -> bool {
    request
        .query
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
}

pub(crate) fn inherit_broker_request_defaults(
    request: &mut BrokerGetContextRequest,
    previous: Option<&BrokerGetContextRequest>,
) {
    let Some(previous) = previous else {
        return;
    };
    let action_was_explicit = request.action.is_some();

    if request.action.is_none() {
        request.action = previous.action;
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = previous.budget_tokens;
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = previous.budget_bytes;
    }
    if request.focus_paths.is_empty() {
        request.focus_paths = previous.focus_paths.clone();
    }
    if request.focus_symbols.is_empty() {
        request.focus_symbols = previous.focus_symbols.clone();
    }
    if request.tool_name.is_none() {
        request.tool_name = previous.tool_name.clone();
    }
    if request.tool_result_kind.is_none() {
        request.tool_result_kind = previous.tool_result_kind;
    }
    if request_query_missing(request) {
        request.query = previous
            .query
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }
    if !action_was_explicit && request.include_sections.is_empty() {
        request.include_sections = previous.include_sections.clone();
    }
    if !action_was_explicit && request.exclude_sections.is_empty() {
        request.exclude_sections = previous.exclude_sections.clone();
    }
    if request.verbosity.is_none() {
        request.verbosity = previous.verbosity;
    }
    if request.response_mode.is_none() {
        request.response_mode = previous.response_mode;
    }
    if !action_was_explicit && request.max_sections.is_none() {
        request.max_sections = previous.max_sections;
    }
    if !action_was_explicit && request.default_max_items_per_section.is_none() {
        request.default_max_items_per_section = previous.default_max_items_per_section;
    }
    if !action_was_explicit && request.section_item_limits.is_empty() {
        request.section_item_limits = previous.section_item_limits.clone();
    }
    if request.persist_artifacts.is_none() {
        request.persist_artifacts = previous.persist_artifacts;
    }
}

pub(crate) fn broker_request_response_mode(
    request: &BrokerGetContextRequest,
) -> BrokerResponseMode {
    request.response_mode.unwrap_or(BrokerResponseMode::Full)
}

pub(crate) fn should_persist_broker_artifacts(request: &BrokerGetContextRequest) -> bool {
    matches!(
        broker_request_response_mode(request),
        BrokerResponseMode::Slim
    ) || request.persist_artifacts.unwrap_or(true)
}

#[derive(Debug, Clone)]
pub(crate) struct BrokerEffectiveLimits {
    pub(crate) max_sections: usize,
    pub(crate) default_max_items_per_section: usize,
    pub(crate) section_item_limits: BTreeMap<String, usize>,
}

pub(crate) fn event_id_for_write(request: &BrokerWriteStateRequest) -> String {
    let payload = serde_json::to_string(request).unwrap_or_else(|_| request.task_id.clone());
    let hash = blake3::hash(payload.as_bytes()).to_hex().to_string();
    format!("broker-{}", &hash[..16])
}

pub(crate) fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn derived_tool_invocation_id(request: &BrokerWriteStateRequest) -> String {
    request
        .invocation_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| event_id_for_write(request))
}

pub(crate) fn derived_tool_sequence(request: &BrokerWriteStateRequest) -> u64 {
    request.sequence.unwrap_or_else(now_unix_millis)
}

pub(crate) fn material_write_is_noop(
    request: &BrokerWriteStateRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> bool {
    let op = request.op.unwrap_or(BrokerWriteOp::FileRead);
    match op {
        BrokerWriteOp::FocusSet => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::FocusClear => {
            if request.paths.is_empty() && request.symbols.is_empty() {
                snapshot.focus_paths.is_empty() && snapshot.focus_symbols.is_empty()
            } else {
                request
                    .paths
                    .iter()
                    .all(|path| !snapshot.focus_paths.iter().any(|existing| existing == path))
                    && request.symbols.iter().all(|symbol| {
                        !snapshot
                            .focus_symbols
                            .iter()
                            .any(|existing| existing == symbol)
                    })
            }
        }
        BrokerWriteOp::FileRead => request
            .paths
            .iter()
            .all(|path| snapshot.files_read.iter().any(|existing| existing == path)),
        BrokerWriteOp::FileEdit => request.paths.iter().all(|path| {
            snapshot
                .files_edited
                .iter()
                .any(|existing| existing == path)
        }),
        BrokerWriteOp::Intention => snapshot.latest_intention.as_ref().is_some_and(|intention| {
            intention.text == request.text.clone().unwrap_or_default()
                && intention.note == request.note
                && intention.step_id == request.step_id
                && intention.question_id == request.question_id
                && intention.paths == request.paths
                && intention.symbols == request.symbols
        }),
        BrokerWriteOp::CheckpointSave => {
            snapshot
                .latest_checkpoint_id
                .as_ref()
                .zip(request.checkpoint_id.as_ref())
                .is_some_and(|(current, requested)| current == requested)
                && snapshot.checkpoint_note == request.note
                && snapshot.checkpoint_focus_paths == request.paths
                && snapshot.checkpoint_focus_symbols == request.symbols
        }
        BrokerWriteOp::QuestionOpen => request.question_id.as_ref().is_some_and(|question_id| {
            snapshot
                .open_questions
                .iter()
                .any(|question| question.id == *question_id)
        }),
        BrokerWriteOp::QuestionResolve => request.question_id.as_ref().is_some_and(|question_id| {
            !snapshot
                .open_questions
                .iter()
                .any(|question| question.id == *question_id)
        }),
        BrokerWriteOp::DecisionAdd => request.decision_id.as_ref().is_some_and(|decision_id| {
            snapshot
                .active_decisions
                .iter()
                .any(|decision| decision.id == *decision_id)
        }),
        BrokerWriteOp::DecisionSupersede => {
            request.decision_id.as_ref().is_some_and(|decision_id| {
                !snapshot
                    .active_decisions
                    .iter()
                    .any(|decision| decision.id == *decision_id)
            })
        }
        BrokerWriteOp::StepComplete => request.step_id.as_ref().is_some_and(|step_id| {
            snapshot
                .completed_steps
                .iter()
                .any(|existing| existing == step_id)
        }),
        BrokerWriteOp::FocusInferred => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::ToolInvocationStarted
        | BrokerWriteOp::ToolInvocationCompleted
        | BrokerWriteOp::ToolInvocationFailed
        | BrokerWriteOp::ToolResult
        | BrokerWriteOp::EvidenceCaptured => false,
    }
}
