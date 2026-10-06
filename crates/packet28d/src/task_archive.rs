//! Online, evidence-preserving archival of oversized dormant task records.
//!
//! For each selected record the daemon:
//!
//! 1. under the state mutex, checks eligibility and installs a per-task
//!    maintenance fence, which requires that no admitted request names the
//!    task and blocks every later mutation of it;
//! 2. without the state mutex, encodes the complete record, publishes it as an
//!    immutable owner-only archive named by its blake3 digest, and reads it
//!    back;
//! 3. under the state mutex, rechecks the fence and that the record's exact
//!    bytes are unchanged, replaces it with a compact tombstone, and stages the
//!    tombstone in the registry WAL;
//! 4. waits for the WAL barrier, then releases the fence.
//!
//! A crash before step 3 is durable leaves the original record authoritative
//! and, at most, an unreferenced archive. Once the tombstone is durable its
//! pointer is the only authority for the original. Event logs and artifacts
//! are never touched. Other tasks, status, and pagination are served
//! throughout; the mutex is never held across archive I/O.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use packet28_daemon_core::storage::load_active_task_record;
use packet28_daemon_core::storage::record_archive::{
    encode_task_record_compact, prepare_task_record_archive, publish_task_record_archive,
    task_record_archive_refusal, task_record_encoded_len, PreparedTaskRecordArchive,
    TASK_RECORD_ARCHIVE_REASON_OVERSIZED,
};
use packet28_daemon_protocol::paths::TaskStorageId;
use packet28_daemon_protocol::registry::{
    TaskRecordArchiveCandidateV1, TaskRecordArchiveOutcome, TaskRecordArchiveReportV1,
    TaskRecordArchiveRequestV1, MAX_TASK_RECORD_ARCHIVE_CANDIDATES, MIN_TASK_RECORD_ARCHIVE_BYTES,
};
use packet28_daemon_protocol::task::TaskRecord;

use crate::persistence::RegistryDelta;
use crate::state::DaemonState;
use crate::task_maintenance::MaintenanceLeaseId;
use crate::{daemon_log, lock_err};

/// Plans or performs archival for the records selected by `request`.
///
/// # Errors
///
/// Fails for an ambiguous or unsafe selector. Per-record refusals and failures
/// are reported in the returned candidates.
pub(crate) fn archive_task_records(
    state: Arc<Mutex<DaemonState>>,
    request: TaskRecordArchiveRequestV1,
) -> Result<TaskRecordArchiveReportV1> {
    let selector = Selector::from_request(&request)?;
    let root = state.lock().map_err(lock_err)?.root.clone();
    let active_task_id = active_task_id(&root);
    let (daemon_pid, selected, remaining_candidates) = {
        let guard = state.lock().map_err(lock_err)?;
        let (selected, remaining) = selector.select(&guard)?;
        (guard.runtime.pid, selected, remaining)
    };
    let mut report = TaskRecordArchiveReportV1 {
        apply: request.apply,
        daemon_pid,
        candidates: Vec::with_capacity(selected.len()),
        remaining_candidates,
    };
    for (task_id, encoded_bytes) in selected {
        let candidate = if request.apply {
            archive_one(
                &state,
                &root,
                &task_id,
                encoded_bytes,
                active_task_id.as_deref(),
            )
        } else {
            plan_one(&state, &task_id, encoded_bytes, active_task_id.as_deref())
        };
        report.candidates.push(candidate);
    }
    Ok(report)
}

enum Selector {
    Exact(String),
    MinBytes(u64),
}

impl Selector {
    fn from_request(request: &TaskRecordArchiveRequestV1) -> Result<Self> {
        match (&request.task_id, request.min_record_bytes) {
            (Some(task_id), None) => {
                TaskStorageId::try_from(task_id.as_str())
                    .map_err(|error| anyhow!("invalid task identifier {task_id:?}: {error}"))?;
                Ok(Self::Exact(task_id.clone()))
            }
            (None, Some(min)) if min >= MIN_TASK_RECORD_ARCHIVE_BYTES as u64 => {
                Ok(Self::MinBytes(min))
            }
            (None, Some(min)) => Err(anyhow!(
                "--min-record-bytes {min} is below the {MIN_TASK_RECORD_ARCHIVE_BYTES}-byte \
                 archive floor; ordinary records are never archived"
            )),
            _ => Err(anyhow!(
                "record archival requires exactly one selector: a task id or a minimum record size"
            )),
        }
    }

    /// Returns selected `(task_id, encoded_bytes)` in identifier order and
    /// the number of matches beyond the per-request bound.
    fn select(&self, state: &DaemonState) -> Result<(Vec<(String, u64)>, usize)> {
        match self {
            Self::Exact(task_id) => {
                let encoded = match state.tasks.tasks.get(task_id) {
                    Some(task) => task_record_encoded_len(task)?,
                    None => 0,
                };
                Ok((vec![(task_id.clone(), encoded)], 0))
            }
            Self::MinBytes(min) => {
                let mut selected = Vec::new();
                let mut remaining = 0_usize;
                for (task_id, task) in &state.tasks.tasks {
                    if task.archived.is_some() {
                        continue;
                    }
                    let encoded = task_record_encoded_len(task)?;
                    if encoded < *min {
                        continue;
                    }
                    if selected.len() < MAX_TASK_RECORD_ARCHIVE_CANDIDATES {
                        selected.push((task_id.clone(), encoded));
                    } else {
                        remaining = remaining.saturating_add(1);
                    }
                }
                Ok((selected, remaining))
            }
        }
    }
}

fn active_task_id(root: &std::path::Path) -> Option<String> {
    match load_active_task_record(root) {
        Ok(record) => record.map(|record| record.task_id),
        Err(error) => {
            // An unreadable pointer cannot prove which task is active; the
            // caller treats every candidate as possibly active.
            daemon_log(&format!(
                "record archival could not read the active task pointer: {error}"
            ));
            Some(String::new())
        }
    }
}

/// Returns why `task_id` cannot be archived now, including daemon-runtime
/// state that the registry record alone does not show.
fn refusal(state: &DaemonState, task_id: &str, active_task_id: Option<&str>) -> Option<String> {
    if active_task_id == Some("") {
        return Some("the agent active-task pointer is unreadable".to_string());
    }
    if let Some(reason) = task_record_archive_refusal(&state.tasks, task_id, active_task_id) {
        return Some(reason);
    }
    let encoded = state
        .tasks
        .tasks
        .get(task_id)
        .map(task_record_encoded_len)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(0);
    if encoded < MIN_TASK_RECORD_ARCHIVE_BYTES as u64 {
        return Some(format!(
            "record is {encoded} bytes, below the {MIN_TASK_RECORD_ARCHIVE_BYTES}-byte archive floor"
        ));
    }
    if state
        .watches
        .watches
        .iter()
        .any(|watch| watch.active && watch.spec.task_id == task_id)
    {
        return Some("task has active watches".to_string());
    }
    if state
        .task_generations
        .current(task_id)
        .is_some_and(|generation| !generation.is_idle())
    {
        return Some("task has in-progress daemon work".to_string());
    }
    if state.task_maintenance.in_flight(task_id) > 0 {
        return Some("task has requests in flight".to_string());
    }
    if state.task_maintenance.is_fenced(task_id) {
        return Some("task is already under record maintenance".to_string());
    }
    None
}

fn candidate(task_id: &str, encoded_bytes: u64) -> TaskRecordArchiveCandidateV1 {
    TaskRecordArchiveCandidateV1 {
        task_id: task_id.to_string(),
        encoded_bytes,
        ..TaskRecordArchiveCandidateV1::default()
    }
}

fn already_archived(state: &DaemonState, task_id: &str) -> Option<TaskRecordArchiveCandidateV1> {
    let task = state.tasks.tasks.get(task_id)?;
    let archive = task.archived.clone()?;
    Some(TaskRecordArchiveCandidateV1 {
        outcome: TaskRecordArchiveOutcome::AlreadyArchived,
        archive: Some(archive),
        ..candidate(task_id, task_record_encoded_len(task).unwrap_or(0))
    })
}

fn refused(task_id: &str, encoded_bytes: u64, reason: String) -> TaskRecordArchiveCandidateV1 {
    TaskRecordArchiveCandidateV1 {
        outcome: TaskRecordArchiveOutcome::Refused,
        reason: Some(reason),
        ..candidate(task_id, encoded_bytes)
    }
}

fn failed(task_id: &str, encoded_bytes: u64, reason: String) -> TaskRecordArchiveCandidateV1 {
    TaskRecordArchiveCandidateV1 {
        outcome: TaskRecordArchiveOutcome::Failed,
        reason: Some(reason),
        ..candidate(task_id, encoded_bytes)
    }
}

fn plan_one(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    encoded_bytes: u64,
    active_task_id: Option<&str>,
) -> TaskRecordArchiveCandidateV1 {
    let snapshot = {
        let guard = match state.lock().map_err(lock_err) {
            Ok(guard) => guard,
            Err(error) => return failed(task_id, encoded_bytes, format!("{error:#}")),
        };
        if let Some(archived) = already_archived(&guard, task_id) {
            return archived;
        }
        if let Some(reason) = refusal(&guard, task_id, active_task_id) {
            return refused(task_id, encoded_bytes, reason);
        }
        guard.tasks.tasks.get(task_id).cloned()
    };
    let Some(original) = snapshot else {
        return refused(
            task_id,
            encoded_bytes,
            "task is not present in the registry".to_string(),
        );
    };
    match prepare_task_record_archive(
        &original,
        TASK_RECORD_ARCHIVE_REASON_OVERSIZED,
        packet28_daemon_core::storage::now_unix(),
    ) {
        Ok(prepared) => TaskRecordArchiveCandidateV1 {
            outcome: TaskRecordArchiveOutcome::WouldArchive,
            archive: Some(prepared.pointer().clone()),
            ..candidate(task_id, encoded_bytes)
        },
        Err(error) => refused(task_id, encoded_bytes, error.to_string()),
    }
}

/// Releases a maintenance fence on every exit path.
struct FenceRelease<'a> {
    state: &'a Arc<Mutex<DaemonState>>,
    task_id: &'a str,
    lease: MaintenanceLeaseId,
}

impl Drop for FenceRelease<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.task_maintenance.unfence(self.task_id, self.lease);
        guard.changes.notify();
    }
}

fn archive_one(
    state: &Arc<Mutex<DaemonState>>,
    root: &std::path::Path,
    task_id: &str,
    encoded_bytes: u64,
    active_task_id: Option<&str>,
) -> TaskRecordArchiveCandidateV1 {
    // Phase 1: eligibility and fence, atomically with the record snapshot.
    let (lease, original) = {
        let mut guard = match state.lock().map_err(lock_err) {
            Ok(guard) => guard,
            Err(error) => return failed(task_id, encoded_bytes, format!("{error:#}")),
        };
        if let Some(archived) = already_archived(&guard, task_id) {
            return archived;
        }
        if let Some(reason) = refusal(&guard, task_id, active_task_id) {
            return refused(task_id, encoded_bytes, reason);
        }
        let Some(original) = guard.tasks.tasks.get(task_id).cloned() else {
            return refused(
                task_id,
                encoded_bytes,
                "task is not present in the registry".to_string(),
            );
        };
        match guard.task_maintenance.try_fence(task_id) {
            Ok(lease) => (lease, original),
            Err(error) => return refused(task_id, encoded_bytes, format!("{error:#}")),
        }
    };
    let _release = FenceRelease {
        state,
        task_id,
        lease,
    };

    // Phase 2: archive publication without the state mutex.
    let prepared = match prepare_and_publish(root, &original) {
        Ok(prepared) => prepared,
        Err(error) => {
            return failed(
                task_id,
                encoded_bytes,
                format!("{error:#}; the original record is unchanged"),
            );
        }
    };
    maybe_exit_after_archive_phase("archive_published");

    // Phase 3: exact-bytes recheck and tombstone staging under the mutex.
    let staged = (|| -> Result<(u64, crate::persistence::PersistenceHandle)> {
        let mut guard = state.lock().map_err(lock_err)?;
        if !guard.task_maintenance.holds(task_id, lease) {
            anyhow::bail!("the record maintenance fence was lost before commit");
        }
        let current = guard
            .tasks
            .tasks
            .get(task_id)
            .ok_or_else(|| anyhow!("task disappeared before archive commit"))?;
        if encode_task_record_compact(current)? != prepared.bytes {
            anyhow::bail!("task record changed after archive preparation");
        }
        if guard
            .watches
            .watches
            .iter()
            .any(|watch| watch.active && watch.spec.task_id == task_id)
            || guard
                .task_generations
                .current(task_id)
                .is_some_and(|generation| !generation.is_idle())
        {
            anyhow::bail!("task became active before archive commit");
        }
        let tombstone = prepared.tombstone.clone();
        let revision = guard
            .persistence
            .stage(RegistryDelta::default().upsert_task(tombstone.clone()))?;
        guard.tasks.tasks.insert(task_id.to_string(), tombstone);
        if let Some(generation) = guard.task_generations.current(task_id) {
            guard
                .task_generations
                .remove_if_current(task_id, generation.id());
        }
        guard.subscribers.remove(task_id);
        guard.record_sizes.forget(task_id);
        guard.changes.notify();
        Ok((revision, guard.persistence.clone()))
    })();
    let (revision, persistence) = match staged {
        Ok(staged) => staged,
        Err(error) => {
            return failed(
                task_id,
                encoded_bytes,
                format!(
                    "{error:#}; the original record is unchanged and the unreferenced archive \
                     {} is retained",
                    prepared.pointer().archive_file
                ),
            );
        }
    };

    // Phase 4: durability before success is reported.
    if let Err(error) = persistence.flush_through(revision) {
        return failed(
            task_id,
            encoded_bytes,
            format!(
                "{error:#}; the tombstone is fenced in memory and the archive {} is durable, \
                 but the tombstone is not yet durable",
                prepared.pointer().archive_file
            ),
        );
    }
    maybe_exit_after_archive_phase("tombstone_durable");
    let pointer = prepared.pointer().clone();
    daemon_log(&format!(
        "archived task record task_id={task_id} digest={} original_bytes={} tombstone_bytes={} \
         archive={}",
        pointer.digest,
        pointer.original_encoded_bytes,
        pointer.tombstone_encoded_bytes,
        pointer.archive_file
    ));
    TaskRecordArchiveCandidateV1 {
        outcome: TaskRecordArchiveOutcome::Archived,
        archive: Some(pointer),
        ..candidate(task_id, encoded_bytes)
    }
}

fn prepare_and_publish(
    root: &std::path::Path,
    original: &TaskRecord,
) -> Result<PreparedTaskRecordArchive> {
    let prepared = prepare_task_record_archive(
        original,
        TASK_RECORD_ARCHIVE_REASON_OVERSIZED,
        packet28_daemon_core::storage::now_unix(),
    )?;
    let storage_id = TaskStorageId::try_from(original.task_id.as_str())
        .map_err(|error| anyhow!("invalid task identifier {:?}: {error}", original.task_id))?;
    publish_task_record_archive(root, &storage_id, &prepared.bytes)?;
    let readback = packet28_daemon_core::storage::record_archive::read_task_record_archive(
        root,
        &prepared.tombstone,
    )?;
    if readback != prepared.bytes {
        anyhow::bail!("archive readback differs from the prepared record");
    }
    Ok(prepared)
}

#[cfg(debug_assertions)]
fn maybe_exit_after_archive_phase(phase: &str) {
    if std::env::var("PACKET28_TASK_RECORD_ARCHIVE_EXIT_AFTER").as_deref() == Ok(phase) {
        std::process::exit(87);
    }
}

#[cfg(not(debug_assertions))]
fn maybe_exit_after_archive_phase(_phase: &str) {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::broker::testing::{emit_task_event, set_context_reason};
    use crate::task_maintenance::admit_task_request;
    use crate::tests::support::{
        daemon_test_root, daemon_test_state, insert_admitted_task_record, shutdown_test_persistence,
    };
    use packet28_daemon_core::storage::record_archive::read_task_record_archive;
    use packet28_daemon_core::storage::{load_task_events, load_task_registry};

    const BIG: &str = "task-big";
    const GOOD: &str = "task-good";

    fn seed(state: &Arc<Mutex<DaemonState>>) {
        insert_admitted_task_record(
            state,
            TaskRecord {
                task_id: BIG.to_string(),
                last_error: Some("x".repeat(200 * 1024)),
                ..TaskRecord::default()
            },
        );
        insert_admitted_task_record(
            state,
            TaskRecord {
                task_id: GOOD.to_string(),
                ..TaskRecord::default()
            },
        );
    }

    fn apply(state: &Arc<Mutex<DaemonState>>, task_id: &str) -> TaskRecordArchiveCandidateV1 {
        let mut report = archive_task_records(
            state.clone(),
            TaskRecordArchiveRequestV1 {
                task_id: Some(task_id.to_string()),
                min_record_bytes: None,
                apply: true,
            },
        )
        .unwrap();
        report.candidates.remove(0)
    }

    #[test]
    fn admitted_requests_block_the_fence_and_the_fence_blocks_only_its_task() {
        let state = daemon_test_state();
        seed(&state);

        let admission = admit_task_request(&state, &[], &[BIG]).unwrap();
        let busy = apply(&state, BIG);
        assert_eq!(busy.outcome, TaskRecordArchiveOutcome::Refused);
        assert!(busy.reason.unwrap().contains("in flight"));
        drop(admission);

        let lease = state
            .lock()
            .unwrap()
            .task_maintenance
            .try_fence(BIG)
            .unwrap();
        assert!(admit_task_request(&state, &[BIG], &[]).is_err());
        assert!(set_context_reason(&state, BIG, "blocked").is_err());
        assert!(emit_task_event(state.clone(), BIG, "probe", serde_json::json!({})).is_err());
        let _healthy = admit_task_request(&state, &[], &[GOOD]).unwrap();
        set_context_reason(&state, GOOD, "unaffected").unwrap();
        emit_task_event(state.clone(), GOOD, "probe", serde_json::json!({})).unwrap();
        assert_eq!(
            state.lock().unwrap().tasks.tasks[BIG].latest_context_reason,
            None,
            "a fenced record is never mutated"
        );
        state.lock().unwrap().task_maintenance.unfence(BIG, lease);

        assert_eq!(
            apply(&state, BIG).outcome,
            TaskRecordArchiveOutcome::Archived
        );
        let error = admit_task_request(&state, &[], &[BIG]).err().unwrap();
        assert!(format!("{error:#}").contains("archived"), "{error:#}");
        let (cancelled, removed) = crate::watch::cancel_task(state.clone(), BIG).unwrap();
        assert!(cancelled.unwrap().archived.is_some());
        assert!(removed.is_empty());
        assert!(crate::mark_task_dirty(&state.lock().unwrap(), BIG).is_err());
    }

    #[test]
    fn concurrent_writers_cannot_resurrect_an_archived_record() {
        let state = daemon_test_state();
        seed(&state);
        let root = daemon_test_root(&state);
        let stop = Arc::new(AtomicBool::new(false));
        let archived = Arc::new(AtomicBool::new(false));
        let writers = (0..3)
            .map(|index| {
                let state = (*state).clone();
                let stop = stop.clone();
                let archived = archived.clone();
                std::thread::spawn(move || {
                    let mut accepted_after_archive = 0_u64;
                    let mut ordinal = 0_u64;
                    while !stop.load(Ordering::Acquire) {
                        ordinal += 1;
                        let was_archived = archived.load(Ordering::Acquire);
                        let reason = set_context_reason(&state, BIG, format!("w{index}-{ordinal}"));
                        let event = emit_task_event(
                            state.clone(),
                            BIG,
                            "probe",
                            serde_json::json!({ "writer": index, "ordinal": ordinal }),
                        );
                        if was_archived {
                            accepted_after_archive += u64::from(reason.is_ok());
                            accepted_after_archive += u64::from(event.is_ok());
                        }
                        // Healthy tasks are never blocked by the target's maintenance.
                        set_context_reason(&state, GOOD, format!("g{index}-{ordinal}")).unwrap();
                    }
                    accepted_after_archive
                })
            })
            .collect::<Vec<_>>();

        let mut outcome = None;
        for _ in 0..200 {
            let candidate = apply(&state, BIG);
            match candidate.outcome {
                TaskRecordArchiveOutcome::Archived => {
                    outcome = Some(candidate);
                    break;
                }
                TaskRecordArchiveOutcome::Refused | TaskRecordArchiveOutcome::Failed => {
                    std::thread::yield_now();
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        let outcome = outcome.expect("archive must eventually win against live writers");
        archived.store(true, Ordering::Release);
        std::thread::sleep(std::time::Duration::from_millis(100));
        stop.store(true, Ordering::Release);
        for writer in writers {
            assert_eq!(
                writer.join().unwrap(),
                0,
                "no write may land after archival"
            );
        }

        let pointer = outcome.archive.unwrap();
        let tombstone = state.lock().unwrap().tasks.tasks[BIG].clone();
        assert_eq!(tombstone.archived.as_ref(), Some(&pointer));
        let original: TaskRecord =
            serde_json::from_slice(&read_task_record_archive(&root, &tombstone).unwrap()).unwrap();
        // The archive is the exact record at commit, including the last
        // accepted concurrent write, and the event high-water matches the log.
        assert_eq!(
            original.latest_context_reason,
            tombstone.latest_context_reason
        );
        assert_eq!(original.last_event_seq, tombstone.last_event_seq);
        let events = load_task_events(&root, BIG).unwrap();
        assert_eq!(
            events.last().map_or(0, |frame| frame.seq),
            tombstone.last_event_seq
        );

        shutdown_test_persistence(&state);
        let durable = load_task_registry(&root).unwrap();
        assert_eq!(durable.tasks[BIG].archived.as_ref(), Some(&pointer));
        assert_eq!(durable.tasks[BIG].last_error, None);
        assert!(durable.tasks[GOOD].latest_context_reason.is_some());
    }
}
