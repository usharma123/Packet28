//! Packet28 daemon application and composition root.
//!
//! [`serve`] owns one workspace daemon from recovery and state construction
//! through transport orchestration, cancellation, persistence flush, and
//! runtime-file cleanup. The `packet28d` executable remains a shallow CLI and
//! process-exit adapter.
//!
//! Wire DTOs, framing, and endpoint paths belong to
//! `packet28-daemon-protocol`; storage, integrity, leases, and recovery belong
//! to `packet28-daemon-core`. Broker implementation modules are private behind
//! one explicit crate-internal facade.
//!
//! `serve` is intentionally not a doctest target: it changes the process
//! working directory, acquires workspace leases, binds a listener, and blocks
//! until shutdown. Hermetic public happy-path examples live with the protocol
//! framing and daemon-core storage APIs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
#[cfg(test)]
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
#[cfg(test)]
use context_kernel_core::PersistConfig;
use context_kernel_core::{
    normalize_sequence_request, Kernel, KernelFailure, KernelRequest, KernelResponse,
    KernelStepRequest, SequenceObserver,
};
use context_memory_core::{
    ContextStoreListFilter, ContextStorePaging, ContextStorePruneRequest, RecallOptions,
};
use diffy_core::model::CoverageFormat;
use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};
#[cfg(test)]
use packet28_daemon_core::storage::ensure_daemon_dir;
use packet28_daemon_core::storage::{load_task_events, now_unix};
#[cfg(test)]
use packet28_daemon_protocol::broker::{
    BrokerAction, BrokerEvictionCandidate, BrokerGetContextRequest, BrokerSection,
    BrokerSourceKind, BrokerValidatePlanRequest, BrokerVerbosity,
};
use packet28_daemon_protocol::broker::{
    BrokerGetContextResponse, BrokerPlanStep, BrokerPrepareHandoffRequest, BrokerResponseMode,
    BrokerTaskStatusRequest, BrokerTaskStatusResponse, BrokerWriteOp, BrokerWriteStateBatchRequest,
    BrokerWriteStateRequest,
};
use packet28_daemon_protocol::commands::{
    CoverCheckRequest, CoverCheckResponse, PacketFetchResponse, TaskSubmitSpec, TestMapRequest,
    TestMapResponse, TestMapSummary, TestShardRequest, TestShardResponse, WatchKind, WatchSpec,
};
use packet28_daemon_protocol::context_store::{
    ContextRecallRequest, ContextRecallResponse, ContextStoreGetRequest, ContextStoreGetResponse,
    ContextStoreListRequest, ContextStoreListResponse, ContextStorePruneDaemonRequest,
    ContextStorePruneResponse, ContextStoreStatsRequest, ContextStoreStatsResponse,
};
use packet28_daemon_protocol::frame::write_frame;
use packet28_daemon_protocol::index::{
    DaemonIndexClearResponse, DaemonIndexManifest, DaemonIndexRebuildRequest,
    DaemonIndexRebuildResponse, DaemonIndexState, DaemonIndexStatusResponse,
};
#[cfg(test)]
use packet28_daemon_protocol::message::DaemonEvent;
use packet28_daemon_protocol::message::{
    DaemonEventFrame, DaemonRequest, DaemonResponse, DaemonRuntimeInfo,
};
use packet28_daemon_protocol::paths::{
    index_dir, index_manifest_path, index_snapshot_path, task_artifact_dir, task_brief_json_path,
    task_brief_markdown_path, task_state_json_path, ContextVersionStorageId, TaskStorageId,
};
#[cfg(test)]
use packet28_daemon_protocol::paths::{ready_path, task_event_log_path, task_version_json_path};
use packet28_daemon_protocol::task::{
    TaskAwaitHandoffRequest, TaskAwaitHandoffResponse, TaskLaunchAgentRequest,
    TaskLaunchAgentResponse, TaskLifecycle, TaskRecord, TaskRegistry, WatchRegistration,
    WatchRegistry,
};
use serde_json::{json, Value};

mod application;
mod broker;
mod commands;
mod hooks;
mod index;
mod instruction_files;
mod kernel_registry;
mod launch;
mod persistence;
mod planning;
mod runtime;
mod runtime_files;
#[cfg(unix)]
mod runtime_files_unix;
mod server;
mod state;
mod watch;

use crate::commands::{
    run_context_recall, run_context_store_get, run_context_store_list, run_context_store_prune,
    run_context_store_stats, run_cover_check, run_test_map, run_test_shard,
};
use crate::hooks::hook_ingest;
#[cfg(test)]
use crate::index::IndexIngress;
use crate::index::{
    daemon_index_clear, daemon_index_rebuild, daemon_index_status, daemon_packet28_search,
};
use crate::instruction_files::resolve_instruction_file;
use crate::launch::task_launch_agent;
#[cfg(test)]
use crate::persistence::PersistenceOwner;
use crate::persistence::RegistryDelta;
#[cfg(test)]
use crate::runtime::{BlockingPool, DaemonRuntimeConfig, ShutdownSignal, StateChangeSignal};
use crate::runtime_files::{default_index_manifest, save_index_manifest_file};
#[cfg(test)]
use crate::runtime_files::{load_index_manifest_file, load_index_runtime_files};
#[cfg(test)]
use crate::state::TaskGenerationRegistry;
use crate::state::{
    BackgroundCommand, DaemonState, IndexCommand, InteractiveIndexRuntime, OwnedChildProcess,
    PendingWatchEvent, TaskGenerationId, TaskGenerationToken, TaskSequenceObserver, WatchEventMsg,
};
use crate::watch::{
    cancel_task, register_task_and_watches, remove_watch, rollback_failed_initial_task_admission,
    run_initial_sequence_for_task, TaskAdmission,
};
#[cfg(test)]
use crate::watch::{rollback_failed_task_admission, run_sequence_for_task, WatchIngress};

pub use application::serve;

#[cfg(feature = "shared-repository-scan")]
pub mod shared_repository_scan;

const INTERACTIVE_INDEX_SCHEMA_VERSION: u32 = 2;
const INDEX_BATCH_DEBOUNCE_MS: u64 = 150;
const TASK_PERSISTENCE_DEBOUNCE_MS: u64 = 20;

fn persist_task(state: &DaemonState, task_id: &str) -> Result<()> {
    mark_task_dirty(state, task_id).map(|_| ())
}

fn mark_task_dirty(state: &DaemonState, task_id: &str) -> Result<u64> {
    let task = state
        .tasks
        .tasks
        .get(task_id)
        .cloned()
        .ok_or_else(|| anyhow!("cannot persist missing task '{task_id}'"))?;
    state
        .persistence
        .stage(RegistryDelta::default().upsert_task(task))
}

fn persist_watch(state: &DaemonState, watch_id: &str) -> Result<()> {
    let watch = state
        .watches
        .watches
        .iter()
        .find(|watch| watch.watch_id == watch_id)
        .cloned()
        .ok_or_else(|| anyhow!("cannot persist missing watch '{watch_id}'"))?;
    state
        .persistence
        .stage(RegistryDelta::default().upsert_watch(watch))
        .map(|_| ())
}

#[cfg(test)]
fn persist_state_for_test(state: &DaemonState) -> Result<()> {
    let mut delta = RegistryDelta::default();
    for task in state.tasks.tasks.values().cloned() {
        delta = delta.upsert_task(task);
    }
    for watch in state.watches.watches.iter().cloned() {
        delta = delta.upsert_watch(watch);
    }
    state.persistence.stage(delta).map(|_| ())
}

fn flush_persistence(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let persistence = state.lock().map_err(lock_err)?.persistence.clone();
    persistence.flush().map(|_| ())
}

fn fence_task_namespace_admission(state: &Arc<Mutex<DaemonState>>, task_id: &str) -> Result<()> {
    let admission = {
        let guard = state.lock().map_err(lock_err)?;
        if !guard.tasks.tasks.contains_key(task_id) {
            anyhow::bail!("task '{task_id}' must exist before writing its managed namespace");
        }
        if guard.persistence.task_is_durably_admitted(task_id) {
            None
        } else {
            let revision = mark_task_dirty(&guard, task_id)?;
            Some((guard.persistence.clone(), revision))
        }
    };
    if let Some((persistence, revision)) = admission {
        persistence.ensure_task_admitted(task_id, revision)?;
    }
    Ok(())
}

fn reconcile_task_event_high_waters(
    tasks: &mut TaskRegistry,
    event_tails: &BTreeMap<String, Option<u64>>,
) -> Result<BTreeSet<String>> {
    if tasks.tasks.len() != event_tails.len() {
        anyhow::bail!(
            "task registry/event-tail snapshot cardinality mismatch: {} tasks, {} tails",
            tasks.tasks.len(),
            event_tails.len()
        );
    }
    let mut changed_task_ids = BTreeSet::new();
    for (task_id, task) in &mut tasks.tasks {
        let durable_sequence = *event_tails
            .get(task_id)
            .ok_or_else(|| anyhow!("task '{task_id}' is missing from the event-tail snapshot"))?;
        // The durable event log is the sequence owner: `append_next_task_event`
        // fsyncs a frame before the in-memory high-water is bumped, so the log
        // always leads the registry. A registry high-water that is *ahead* of
        // the log therefore means the log lost committed bytes (truncation or
        // loss), not that the registry is authoritative. Rather than fail the
        // whole daemon closed on one such task, self-heal by trusting the
        // durable log tail and persist the correction (the caller stages every
        // changed task). A high-water that trails the log is the ordinary
        // crash-after-append case and is bumped up as before.
        match durable_sequence {
            None if task.last_event_seq == 0 => {}
            None => {
                daemon_log(&format!(
                    "healing task '{task_id}': registry high-water {} is ahead of a missing event log; resetting it to 0",
                    task.last_event_seq
                ));
                task.last_event_seq = 0;
                changed_task_ids.insert(task_id.clone());
            }
            Some(durable_sequence) if task.last_event_seq > durable_sequence => {
                daemon_log(&format!(
                    "healing task '{task_id}': registry high-water {} is ahead of durable event sequence {durable_sequence}; resetting it to {durable_sequence}",
                    task.last_event_seq
                ));
                task.last_event_seq = durable_sequence;
                changed_task_ids.insert(task_id.clone());
            }
            Some(durable_sequence) if task.last_event_seq < durable_sequence => {
                task.last_event_seq = durable_sequence;
                changed_task_ids.insert(task_id.clone());
            }
            Some(_) => {}
        }
    }
    Ok(changed_task_ids)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TaskRestartReconciliation {
    changed_tasks: usize,
    changed_task_ids: BTreeSet<String>,
    removed_watch_ids: BTreeSet<String>,
    replan_task_ids: Vec<String>,
}

/// Validates restart work before any lifecycle or watch state is mutated, and
/// heals per-task inconsistencies that must not brick the whole daemon.
///
/// A live process group cannot be authenticated from its persisted numeric PID
/// alone because operating systems reuse process identifiers. That case stays
/// fatal: startup fails closed until an operator quiesces the group, instead of
/// risking signalling an unrelated process.
///
/// A replan task that lost its stored kernel sequence, by contrast, cannot be
/// re-executed but must not take the whole daemon down. It is downgraded to
/// `Idle` (keeping its record and recording why in `last_error`) so the rest of
/// the workspace recovers; the task can be resubmitted. Returns the ids of tasks
/// healed this way so the caller persists the downgrade.
fn preflight_restart_recovery(tasks: &mut TaskRegistry) -> Result<BTreeSet<String>> {
    let mut healed = BTreeSet::new();
    for (task_id, task) in &mut tasks.tasks {
        // A live recovered agent must never be orphaned; keep this fatal, and
        // check it first so a task that is both live and missing its sequence
        // still fails closed rather than being silently downgraded.
        if task.latest_agent_completed_at_unix.is_none() {
            if let Some(pid) = task.latest_agent_pid {
                if crate::launch::recovered_agent_process_group_exists(pid)? {
                    anyhow::bail!(
                        "task '{task_id}' has a live recovered agent process group for pid {pid}; \
                         refusing recovery from an unauthenticated persisted pid"
                    );
                }
            }
        }
        if matches!(
            task.lifecycle,
            TaskLifecycle::ReplanPending
                | TaskLifecycle::RunningRecoveredReplan
                | TaskLifecycle::RunningReplanPending
        ) && (!task.sequence_present || task.sequence.is_none())
        {
            daemon_log(&format!(
                "healing task '{task_id}': {:?} lifecycle has no stored sequence to replan; downgrading to Idle",
                task.lifecycle
            ));
            task.lifecycle = TaskLifecycle::Idle;
            let evidence =
                "durable replan sequence was missing after restart; downgraded to idle".to_string();
            task.last_error = Some(match task.last_error.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}; {evidence}"),
                _ => evidence,
            });
            healed.insert(task_id.clone());
        }
    }
    Ok(healed)
}

/// Reconciles lifecycle state that cannot survive process ownership changes.
///
/// Restart-completed cancellation is intentionally evidenced by the terminal
/// registry record rather than a synthetic `task_cancelled` event. The paired
/// registry checkpoint can atomically persist `Cancelled`, its completion
/// timestamp, and `last_error` before readiness. An event written before that
/// checkpoint could be duplicated after a crash, while one written afterward
/// could be lost after the terminal state commits. Keeping the registry record
/// as the recovery marker makes repeated startup reconciliation idempotent;
/// live cancellation continues to publish `task_cancelled`.
fn reconcile_interrupted_task_lifecycles(
    tasks: &mut TaskRegistry,
    watches: &mut WatchRegistry,
    recovered_at_unix: u64,
) -> Result<TaskRestartReconciliation> {
    let mut reconciliation = TaskRestartReconciliation::default();
    for (task_id, task) in &mut tasks.tasks {
        let interrupted_agent_pid = task
            .latest_agent_pid
            .filter(|_| task.latest_agent_completed_at_unix.is_none());
        if let Some(pid) = interrupted_agent_pid {
            task.latest_agent_completed_at_unix = Some(recovered_at_unix);
            let evidence = format!(
                "delegated agent pid {pid} was interrupted before packet28d restart recovery"
            );
            task.last_error = Some(match task.last_error.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}; {evidence}"),
                _ => evidence,
            });
        }
        let persisted_lifecycle = task.lifecycle;
        let evidence = match persisted_lifecycle {
            TaskLifecycle::Idle => {
                if interrupted_agent_pid.is_some() {
                    reconciliation.changed_tasks += 1;
                    reconciliation.changed_task_ids.insert(task_id.clone());
                }
                continue;
            }
            TaskLifecycle::Cancelled => {
                if task.watch_ids.is_empty() && interrupted_agent_pid.is_none() {
                    continue;
                }
                let removed_watch_ids = std::mem::take(&mut task.watch_ids);
                watches.watches.retain(|watch| {
                    let removed = removed_watch_ids
                        .iter()
                        .any(|watch_id| watch_id == &watch.watch_id);
                    if removed {
                        reconciliation
                            .removed_watch_ids
                            .insert(watch.watch_id.clone());
                    }
                    !removed
                });
                reconciliation.changed_tasks += 1;
                reconciliation.changed_task_ids.insert(task_id.clone());
                continue;
            }
            TaskLifecycle::ReplanPending => {
                if interrupted_agent_pid.is_some() {
                    reconciliation.changed_tasks += 1;
                    reconciliation.changed_task_ids.insert(task_id.clone());
                }
                reconciliation.replan_task_ids.push(task_id.clone());
                continue;
            }
            TaskLifecycle::Cancelling { .. } => {
                let removed_watch_ids = std::mem::take(&mut task.watch_ids);
                watches.watches.retain(|watch| {
                    let removed = removed_watch_ids
                        .iter()
                        .any(|watch_id| watch_id == &watch.watch_id);
                    if removed {
                        reconciliation
                            .removed_watch_ids
                            .insert(watch.watch_id.clone());
                    }
                    !removed
                });
                task.lifecycle.complete_cancel()?;
                task.last_completed_at_unix = Some(recovered_at_unix);
                format!(
                    "task cancellation completed by packet28d restart from persisted lifecycle \
                     {persisted_lifecycle:?}"
                )
            }
            TaskLifecycle::Running => {
                task.lifecycle = TaskLifecycle::Idle;
                format!(
                    "task interrupted by packet28d restart before persisted lifecycle \
                     {persisted_lifecycle:?} completed"
                )
            }
            TaskLifecycle::RunningRecoveredReplan => {
                task.lifecycle = TaskLifecycle::ReplanPending;
                reconciliation.replan_task_ids.push(task_id.clone());
                "task interrupted after its durable claim before completion; \
                 durable replan remains queued"
                    .to_string()
            }
            TaskLifecycle::RunningReplanPending => {
                task.lifecycle = TaskLifecycle::ReplanPending;
                reconciliation.replan_task_ids.push(task_id.clone());
                format!(
                    "task interrupted by packet28d restart before persisted lifecycle \
                     {persisted_lifecycle:?} completed; durable replan remains queued"
                )
            }
        };
        task.last_error = Some(match task.last_error.take() {
            Some(existing) if !existing.is_empty() => format!("{existing}; {evidence}"),
            _ => evidence,
        });
        reconciliation.changed_tasks += 1;
        reconciliation.changed_task_ids.insert(task_id.clone());
    }
    Ok(reconciliation)
}

fn daemon_log(message: &str) {
    eprintln!(
        "[packet28d {}] {message}",
        packet28_daemon_core::storage::now_unix()
    );
}

fn resolve_root(path: &Path) -> PathBuf {
    let mut current = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    loop {
        if current.join(".git").exists() {
            return current;
        }
        if !current.pop() {
            return path.to_path_buf();
        }
    }
}

fn task_storage_id(value: &str) -> Result<TaskStorageId> {
    TaskStorageId::try_from(value)
        .map_err(|error| anyhow!("invalid task storage identifier {value:?}: {error}"))
}

fn context_version_storage_id(value: &str) -> Result<ContextVersionStorageId> {
    ContextVersionStorageId::try_from(value)
        .map_err(|error| anyhow!("invalid context-version storage identifier {value:?}: {error}"))
}

fn lock_err<T>(err: std::sync::PoisonError<T>) -> anyhow::Error {
    anyhow!("daemon state lock poisoned: {err}")
}

#[cfg(test)]
mod tests;
