//! Per-task request admission, record-maintenance fences, and record-size
//! diagnostics.
//!
//! Every client request that names a task is admitted under the state mutex:
//! admission rejects superseded or archived identities and tasks fenced for
//! record maintenance, then counts the request as in flight until its response
//! is produced. A maintenance fence is installed only while that count is
//! zero, in the same critical section that snapshots the record, so a fenced
//! task has no admitted request in flight and admits no new one. Background
//! paths that are not client requests are excluded at their mutation points by
//! [`DaemonState::require_task_mutable`], which the fence and the archived
//! marker both fail.
//!
//! Only the fenced task is affected. Other tasks, status, and pagination keep
//! running; no global lock is held across archive file I/O.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use packet28_daemon_core::storage::record_archive::task_record_encoded_len;
use packet28_daemon_protocol::registry::{
    TaskRecordSizeLevel, TaskRecordSizeWarningV1, MAX_STATUS_RECORD_SIZE_WARNINGS,
};
use packet28_daemon_protocol::task::TaskRecord;

use crate::state::DaemonState;
use crate::{daemon_log, lock_err};

/// Opaque identity of one installed maintenance fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaintenanceLeaseId(u64);

#[derive(Debug, Default)]
pub(crate) struct TaskMaintenance {
    in_flight: BTreeMap<String, usize>,
    fenced: BTreeMap<String, MaintenanceLeaseId>,
    next_lease: u64,
}

impl TaskMaintenance {
    pub(crate) fn is_fenced(&self, task_id: &str) -> bool {
        self.fenced.contains_key(task_id)
    }

    pub(crate) fn in_flight(&self, task_id: &str) -> usize {
        self.in_flight.get(task_id).copied().unwrap_or(0)
    }

    fn admit(&mut self, task_ids: &[String]) {
        for task_id in task_ids {
            *self.in_flight.entry(task_id.clone()).or_default() += 1;
        }
    }

    fn release(&mut self, task_ids: &[String]) {
        for task_id in task_ids {
            if let Some(count) = self.in_flight.get_mut(task_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.in_flight.remove(task_id);
                }
            }
        }
    }

    /// Installs a fence when no admitted request names `task_id`.
    pub(crate) fn try_fence(&mut self, task_id: &str) -> Result<MaintenanceLeaseId> {
        if self.is_fenced(task_id) {
            anyhow::bail!("task '{task_id}' is already under record maintenance");
        }
        let in_flight = self.in_flight(task_id);
        if in_flight > 0 {
            anyhow::bail!(
                "task '{task_id}' has {in_flight} request(s) in flight; record maintenance \
                 cannot be admitted safely, retry when the task is idle"
            );
        }
        self.next_lease = self
            .next_lease
            .checked_add(1)
            .ok_or_else(|| anyhow!("record maintenance lease identifiers exhausted"))?;
        let lease = MaintenanceLeaseId(self.next_lease);
        self.fenced.insert(task_id.to_string(), lease);
        Ok(lease)
    }

    pub(crate) fn holds(&self, task_id: &str, lease: MaintenanceLeaseId) -> bool {
        self.fenced.get(task_id) == Some(&lease)
    }

    pub(crate) fn unfence(&mut self, task_id: &str, lease: MaintenanceLeaseId) {
        if self.holds(task_id, lease) {
            self.fenced.remove(task_id);
        }
    }
}

impl DaemonState {
    /// Fails when `task_id` may not be mutated: it is fenced for record
    /// maintenance or is an archived tombstone.
    ///
    /// Every task mutation must pass this check in the same critical section
    /// that changes the record, so neither a request nor a background path can
    /// rewrite (and thereby resurrect) an archived identity.
    pub(crate) fn require_task_mutable(&self, task_id: &str) -> Result<()> {
        if self.task_maintenance.is_fenced(task_id) {
            anyhow::bail!(
                "task '{task_id}' is fenced for record maintenance; retry after it completes"
            );
        }
        if let Some(task) = self.tasks.tasks.get(task_id) {
            if task.archived.is_some() {
                packet28_daemon_core::storage::require_continuable_task(task)?;
            }
        }
        Ok(())
    }
}

/// Admission held for the lifetime of one client request.
pub(crate) struct TaskRequestAdmission {
    state: Arc<Mutex<DaemonState>>,
    task_ids: Vec<String>,
}

impl Drop for TaskRequestAdmission {
    fn drop(&mut self) {
        if self.task_ids.is_empty() {
            return;
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.task_maintenance.release(&self.task_ids);
    }
}

/// Admits a request naming `named` tasks, of which `continued` would continue
/// or mutate them.
///
/// Continued tasks must not be superseded or archived. Every named task must
/// not be fenced. All are counted in flight until the returned admission drops.
pub(crate) fn admit_task_request(
    state: &Arc<Mutex<DaemonState>>,
    named: &[&str],
    continued: &[&str],
) -> Result<TaskRequestAdmission> {
    let mut guard = state.lock().map_err(lock_err)?;
    for task_id in continued {
        if let Some(task) = guard.tasks.tasks.get(*task_id) {
            packet28_daemon_core::storage::require_continuable_task(task)?;
        }
    }
    let mut task_ids = named
        .iter()
        .chain(continued)
        .map(|task_id| (*task_id).to_string())
        .collect::<Vec<_>>();
    task_ids.sort();
    task_ids.dedup();
    for task_id in &task_ids {
        if guard.task_maintenance.is_fenced(task_id) {
            anyhow::bail!(
                "task '{task_id}' is fenced for record maintenance; retry after it completes"
            );
        }
    }
    guard.task_maintenance.admit(&task_ids);
    drop(guard);
    Ok(TaskRequestAdmission {
        state: state.clone(),
        task_ids,
    })
}

/// Tracks records at or above the size warning threshold.
///
/// Sizes are observed on every staged task write, so a record that grows
/// toward the pagination bound is reported before it becomes unlistable.
#[derive(Debug, Default)]
pub(crate) struct RecordSizeIndex {
    warnings: Mutex<BTreeMap<String, u64>>,
}

impl RecordSizeIndex {
    /// Records `task`'s compact size and logs a threshold crossing once.
    pub(crate) fn observe(&self, task: &TaskRecord) {
        let encoded_bytes = match task_record_encoded_len(task) {
            Ok(bytes) => bytes,
            Err(error) => {
                daemon_log(&format!(
                    "failed to size task record task_id={} error={error}",
                    task.task_id
                ));
                return;
            }
        };
        self.observe_size(&task.task_id, encoded_bytes);
    }

    pub(crate) fn observe_size(&self, task_id: &str, encoded_bytes: u64) {
        let level = TaskRecordSizeLevel::classify(encoded_bytes);
        let mut warnings = self
            .warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = warnings
            .get(task_id)
            .and_then(|bytes| TaskRecordSizeLevel::classify(*bytes));
        match level {
            Some(level) => {
                warnings.insert(task_id.to_string(), encoded_bytes);
                if previous != Some(level) {
                    daemon_log(&format!(
                        "task record size {} task_id={task_id} encoded_bytes={encoded_bytes}; \
                         run `Packet28 daemon storage archive-record --task-id {task_id}` to \
                         plan evidence-preserving archival",
                        match level {
                            TaskRecordSizeLevel::Warning => "warning",
                            TaskRecordSizeLevel::OverPageLimit => "over page limit",
                        }
                    ));
                }
            }
            None => {
                warnings.remove(task_id);
            }
        }
    }

    pub(crate) fn forget(&self, task_id: &str) {
        self.warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id);
    }

    /// Returns the warning count and the largest bounded set of warnings.
    pub(crate) fn status_warnings(&self) -> (usize, Vec<TaskRecordSizeWarningV1>) {
        let warnings = self
            .warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut ordered = warnings
            .iter()
            .filter_map(|(task_id, bytes)| {
                TaskRecordSizeLevel::classify(*bytes).map(|level| TaskRecordSizeWarningV1 {
                    task_id: task_id.clone(),
                    encoded_bytes: *bytes,
                    level,
                })
            })
            .collect::<Vec<_>>();
        let count = ordered.len();
        ordered.sort_by(|left, right| {
            right
                .encoded_bytes
                .cmp(&left.encoded_bytes)
                .then_with(|| left.task_id.cmp(&right.task_id))
        });
        ordered.truncate(MAX_STATUS_RECORD_SIZE_WARNINGS);
        (count, ordered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use packet28_daemon_protocol::registry::{
        MAX_REGISTRY_PAGE_ITEM_BYTES, TASK_RECORD_SIZE_WARNING_BYTES,
    };

    #[test]
    fn fence_requires_zero_in_flight_and_blocks_new_admission() {
        let mut maintenance = TaskMaintenance::default();
        maintenance.admit(&["task-a".to_string()]);
        assert!(maintenance.try_fence("task-a").is_err());
        let lease_b = maintenance.try_fence("task-b").unwrap();
        maintenance.release(&["task-a".to_string()]);
        let lease_a = maintenance.try_fence("task-a").unwrap();
        assert!(maintenance.try_fence("task-a").is_err());
        assert!(maintenance.holds("task-a", lease_a));
        maintenance.unfence("task-a", lease_b);
        assert!(
            maintenance.is_fenced("task-a"),
            "a foreign lease cannot unfence"
        );
        maintenance.unfence("task-a", lease_a);
        assert!(!maintenance.is_fenced("task-a"));
        assert!(maintenance.is_fenced("task-b"));
    }

    #[test]
    fn size_index_reports_near_limit_and_over_limit_records_largest_first() {
        let index = RecordSizeIndex::default();
        index.observe_size("small", 4096);
        index.observe_size("near", TASK_RECORD_SIZE_WARNING_BYTES as u64);
        index.observe_size("over", MAX_REGISTRY_PAGE_ITEM_BYTES as u64 + 1);
        let (count, warnings) = index.status_warnings();
        assert_eq!(count, 2);
        assert_eq!(warnings[0].task_id, "over");
        assert_eq!(warnings[0].level, TaskRecordSizeLevel::OverPageLimit);
        assert_eq!(warnings[1].task_id, "near");
        assert_eq!(warnings[1].level, TaskRecordSizeLevel::Warning);
        index.observe_size("near", 1024);
        index.forget("over");
        assert_eq!(index.status_warnings().0, 0);
    }
}
