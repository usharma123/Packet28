//! Safe inspection and bounded retention for workspace-local task state.
//!
//! Retention is dry-run unless [`RetentionOptions::apply`] is explicitly set.
//! Only task artifacts, event logs, and inactive task-registry records beneath
//! a real workspace-local `.packet28` directory are eligible. Symlinks,
//! unreadable entries, ambiguous task identifiers, active tasks, and state
//! owned by a running daemon are never removed.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{hash_map::RandomState, BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::BuildHasher as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(all(test, unix))]
use fs2::FileExt;
use packet28_daemon_protocol::hooks::ActiveTaskRecord;
use packet28_daemon_protocol::paths::{
    active_task_path, agent_runtime_dir, daemon_dir, ready_path, task_artifacts_dir,
    task_events_dir, task_registry_path, TaskStorageId, READY_FILE_NAME, TASK_EVENT_LOG_SUFFIX,
    TASK_REGISTRY_FILE_NAME,
};
use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::capability::{
    generated_deletion_prefix, generated_name_matches, AtomicWriteError, CapabilityDir,
    CapabilityEntryKind, RemovalProgress, ACTIVE_TASK_WRITE_TEMP_PREFIX, DELETION_TEMP_PREFIX,
    NOREPLACE_PROBE_DESTINATION_PREFIX, NOREPLACE_PROBE_SOURCE_PREFIX,
    RETENTION_JOURNAL_WRITE_DELETION_TEMP_PREFIX, RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
    TASK_REGISTRY_WRITE_TEMP_PREFIX,
};
#[cfg(unix)]
use crate::storage::{
    AnchoredFileLock, AnchoredFileLockFinishError, AnchoredFileLockMode,
    TASK_REGISTRY_LOCK_FILE_NAME,
};
use crate::storage::{
    AuthorityJsonProfile, ACTIVE_TASK_LOCK_FILE_NAME, MAX_ACTIVE_TASK_RECORD_BYTES,
    MAX_TASK_REGISTRY_BYTES,
};
#[cfg(not(unix))]
use crate::task_store_lease::acquire_daemon_task_store_lease;
#[cfg(unix)]
use crate::task_store_lease::{
    acquire_daemon_task_store_lease_from, acquire_task_store_recovery_lease_from,
    try_acquire_task_retention_instance_gate_from,
};
use crate::task_store_lease::{
    acquire_task_store_recovery_lease, daemon_instance_lock_path, task_store_lifecycle_lock_path,
    try_acquire_task_store_retention_lease, LeaseRole, TaskRetentionAdmission, TaskStoreLease,
};
use crate::{DaemonCoreError, Result};

mod scan;

#[cfg(not(unix))]
use scan::filesystem_allocated_bytes;

#[cfg(unix)]
use scan::{
    ensure_same_filesystem, scan_capability_directory_with_limits,
    scan_capability_entry_with_limits,
};
use scan::{
    file_identity, same_device, scan_path_with_limits, scan_path_with_limits_from_parent,
    ScanLimits, ScanSummary,
};

/// Schema version for serialized [`TaskStoreReport`] values.
pub const TASK_STORE_REPORT_SCHEMA_VERSION: u32 = 2;

const STATE_DIR_NAME: &str = ".packet28";
const QUARANTINE_DIR_NAME: &str = ".retention-trash";
const QUARANTINE_JOURNAL_FILE_NAME: &str = "journal-v1.json";
const QUARANTINE_JOURNAL_DELETION_FILE_NAME: &str = ".journal-v1.final-delete.json";
const LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION: u32 = 1;
const QUARANTINE_JOURNAL_SCHEMA_VERSION: u32 = 2;
// A journal carries at most one complete registry record plus its storage key.
// The registry already contains two copies of a task id (map key + record
// field); a 2x bound covers the journal's additional storage-key copy while
// keeping corrupt reads finite. Compact encoding avoids pretty-print expansion.
const MAX_QUARANTINE_JOURNAL_ENVELOPE_BYTES: usize = 64 * 1024;
/// Maximum encoded size accepted for one crash-recovery quarantine journal.
///
/// Every public task-registry write mechanically serializes the maximum
/// journal for each record against this same bound before changing state.
pub const MAX_TASK_RETENTION_JOURNAL_BYTES: usize =
    MAX_TASK_REGISTRY_BYTES * 2 + MAX_QUARANTINE_JOURNAL_ENVELOPE_BYTES;
const MAX_QUARANTINE_COMPONENTS: usize = 2;
const MAX_QUARANTINE_RECORDS: usize = 1;
// `StoreSnapshot` first applies these traversal bounds to the complete state
// tree. Any later aggregate or per-candidate traversal gets a fresh budget for
// revalidation, but cannot expand the authoritative state tree that already
// passed the global snapshot bound.
const MAX_RETENTION_SCAN_DEPTH: usize = 64;
const MAX_RETENTION_SCAN_ENTRIES_PER_TRAVERSAL: usize = 65_536;
// Managed-root enumeration has an explicit independent immediate-entry bound.
// Those entries are also covered by the complete state-tree traversal above.
const MAX_RETENTION_MANAGED_ROOT_ENTRIES: usize = 65_536;
const MAX_TASK_STORE_ISSUES: usize = 1_024;
const ISSUE_BUDGET_EXHAUSTED_KIND: &str = "issue_budget_exhausted";
#[cfg(unix)]
const MAX_QUARANTINE_GROUPS: usize = 4_096;
#[cfg(unix)]
const MAX_QUARANTINE_GROUP_ENTRIES: usize = 32;
#[cfg(unix)]
const MAX_STARTUP_RECOVERY_PASSES: usize = 8;
#[cfg(unix)]
const MAX_QUARANTINE_GROUP_CREATE_ATTEMPTS: usize = 16;
static QUARANTINE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Retention execution mode represented in a report.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    /// Metrics-only inspection without a retention bound.
    Inspect,
    /// A bounded plan that did not mutate the store.
    DryRun,
    /// An explicitly requested cleanup attempt.
    Apply,
}

/// Bounds and execution mode for task-store retention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionOptions {
    /// Maximum permitted age in seconds.
    ///
    /// Candidates exactly on the boundary are retained; only candidates older
    /// than this value are selected by age.
    pub max_age_seconds: Option<u64>,
    /// Maximum permitted logical bytes across task records, artifacts, and events.
    ///
    /// Candidates are selected oldest-first only while the store is strictly
    /// larger than this value.
    pub max_bytes: Option<u64>,
    /// Whether to apply the plan. `false` is always a non-mutating dry run.
    pub apply: bool,
}

impl RetentionOptions {
    /// Creates an unbounded metrics-only inspection.
    pub const fn inspect() -> Self {
        Self {
            max_age_seconds: None,
            max_bytes: None,
            apply: false,
        }
    }

    /// Creates a non-mutating retention plan.
    pub const fn dry_run(max_age_seconds: Option<u64>, max_bytes: Option<u64>) -> Self {
        Self {
            max_age_seconds,
            max_bytes,
            apply: false,
        }
    }

    /// Returns the same bounds with explicit deletion enabled.
    pub const fn apply(mut self) -> Self {
        self.apply = true;
        self
    }
}

#[cfg(unix)]
enum RetentionApplyState {
    Armed {
        lease: TaskStoreLease,
        admission: TaskRetentionAdmission,
    },
    ReadOnly,
}

/// Current logical-size and entry-count observations for a Packet28 state tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStoreMetrics {
    /// Logical bytes beneath the entire workspace-local `.packet28` directory.
    pub state_logical_bytes: u64,
    /// Allocated filesystem bytes beneath `.packet28`.
    pub state_allocated_bytes: u64,
    /// Whether allocated-byte metrics use native filesystem block counts.
    ///
    /// When false, allocated-byte fields fall back to logical byte counts.
    pub allocated_bytes_supported: bool,
    /// Regular files beneath `.packet28`.
    pub state_files: u64,
    /// Directories beneath `.packet28`, including the state root.
    pub state_directories: u64,
    /// Symlinks encountered without following them.
    pub state_symlinks: u64,
    /// Actual bytes in the task-registry checkpoint plus authenticated WAL.
    pub task_registry_file_bytes: u64,
    /// Allocated bytes for the task-registry checkpoint plus authenticated WAL.
    pub task_registry_allocated_bytes: u64,
    /// Successfully decoded records in the task registry.
    pub task_registry_records: u64,
    /// Whether the task registry was absent or decoded without error.
    pub task_registry_reliable: bool,
    /// Logical bytes under the task-artifact root.
    pub task_artifact_logical_bytes: u64,
    /// Allocated filesystem bytes under the task-artifact root.
    pub task_artifact_allocated_bytes: u64,
    /// Regular files under the task-artifact root.
    pub task_artifact_files: u64,
    /// Task-artifact directories, including the artifact root when present.
    pub task_artifact_directories: u64,
    /// Logical bytes under the daemon task-event root.
    pub task_event_logical_bytes: u64,
    /// Allocated filesystem bytes under the daemon task-event root.
    pub task_event_allocated_bytes: u64,
    /// Regular files under the task-event root.
    pub task_event_files: u64,
    /// Logical bytes retained in incomplete or committed quarantine groups.
    #[serde(default)]
    pub retention_quarantine_logical_bytes: u64,
    /// Allocated bytes retained in incomplete or committed quarantine groups.
    #[serde(default)]
    pub retention_quarantine_allocated_bytes: u64,
    /// Quarantine groups awaiting rollback, committed deletion, or repair.
    #[serde(default)]
    pub retention_quarantine_groups: u64,
    /// Logical bytes governed by retention: compact task records, artifacts,
    /// events, and durable quarantine.
    pub managed_task_logical_bytes: u64,
    /// Allocated bytes occupied by the registry, artifacts, events, and
    /// durable quarantine.
    pub managed_task_allocated_bytes: u64,
    /// Task records or active-task pointers currently protected as active.
    pub active_tasks: u64,
    /// Oldest known task record, artifact, or event timestamp.
    pub oldest_task_timestamp_unix: Option<u64>,
    /// Newest known task record, artifact, or event timestamp.
    pub newest_task_timestamp_unix: Option<u64>,
}

/// Why a task-store candidate was selected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RetentionReason {
    /// The candidate is strictly older than the configured maximum age.
    AgeLimit,
    /// Removing the candidate is required to approach the configured byte limit.
    SizeLimit,
}

/// Result of one planned retention action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOutcome {
    /// The default dry run left the candidate unchanged.
    WouldRemove,
    /// Explicit apply removed the candidate.
    Removed,
    /// Revalidation protected a candidate that changed or became active.
    Skipped,
    /// Cleanup failed and the candidate was restored or durably quarantined.
    Failed,
}

/// One task selected by the retention planner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionAction {
    /// Storage-safe task key used by artifact and event paths.
    pub storage_key: String,
    /// Original registry task identifiers associated with the storage key.
    pub task_ids: Vec<String>,
    /// Logical bytes expected to be reclaimed.
    pub logical_bytes: u64,
    /// Planned logical bytes confirmed removed by this action.
    #[serde(default)]
    pub removed_logical_bytes: u64,
    /// Planned logical bytes confirmed retained after this action.
    #[serde(default)]
    pub remaining_logical_bytes: u64,
    /// Whether removed/remaining byte progress was measured reliably.
    #[serde(default)]
    pub byte_accounting_reliable: bool,
    /// Latest known modification timestamp.
    pub latest_timestamp_unix: Option<u64>,
    /// Bounds that selected this candidate.
    pub reasons: Vec<RetentionReason>,
    /// Dry-run or apply outcome.
    pub outcome: RetentionOutcome,
}

/// Non-fatal corruption, unreadable-entry, or safety observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStoreIssue {
    /// Stable machine-readable issue category.
    pub kind: String,
    /// Affected path.
    pub path: String,
    /// Human-readable diagnostic.
    pub message: String,
}

/// Retention planning and cleanup accounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionAccounting {
    /// Configured maximum age.
    pub max_age_seconds: Option<u64>,
    /// Configured maximum logical bytes.
    pub max_bytes: Option<u64>,
    /// Candidates protected by active, ambiguous, corrupt, or unsafe state.
    pub protected_tasks: u64,
    /// Logical bytes associated with protected candidates.
    pub protected_logical_bytes: u64,
    /// Candidates selected by the planner.
    pub planned_tasks: u64,
    /// Logical bytes selected by the planner.
    pub planned_logical_bytes: u64,
    /// Candidates actually removed by explicit apply.
    pub removed_tasks: u64,
    /// Logical bytes removed by explicit apply.
    pub removed_logical_bytes: u64,
    /// Planned candidates skipped after revalidation.
    pub skipped_tasks: u64,
    /// Planned candidates whose cleanup encountered a reported failure.
    #[serde(default)]
    pub failed_tasks: u64,
    /// Logical bytes confirmed remaining after failed cleanup attempts.
    #[serde(default)]
    pub failed_logical_bytes: u64,
    /// Precommit quarantine groups restored during this apply.
    #[serde(default)]
    pub recovered_precommit_groups: u64,
    /// Committed quarantine groups whose deletion completed during this apply.
    #[serde(default)]
    pub recovered_committed_groups: u64,
    /// Quarantine groups left protected because recovery found a conflict.
    #[serde(default)]
    pub recovery_conflicted_groups: u64,
    /// Whether `metrics_after` came from a successful post-apply rescan.
    #[serde(default)]
    pub final_rescan_reliable: bool,
    /// Whether every action's removed/remaining byte split is reliable.
    #[serde(default)]
    pub action_byte_accounting_reliable: bool,
    /// Planned logical bytes after a dry run, or observed bytes after apply.
    pub remaining_managed_logical_bytes: u64,
    /// Bytes still above the configured size bound because they are protected.
    pub remaining_over_limit_bytes: u64,
}

/// Timestamped inspection and optional retention result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStoreReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Timestamp supplied by the caller for this observation.
    pub observed_at_unix: u64,
    /// Canonical workspace root.
    pub workspace_root: String,
    /// Validated workspace-local Packet28 state root.
    pub state_root: String,
    /// Inspection, dry-run, or explicit-apply mode.
    pub mode: RetentionMode,
    /// Metrics before retention planning or cleanup.
    pub metrics_before: TaskStoreMetrics,
    /// Metrics after cleanup, equal to `metrics_before` for inspect and dry-run.
    pub metrics_after: TaskStoreMetrics,
    /// Retention accounting.
    pub retention: RetentionAccounting,
    /// Deterministically ordered planned actions.
    pub actions: Vec<RetentionAction>,
    /// Non-fatal safety and corruption observations.
    pub issues: Vec<TaskStoreIssue>,
}

/// Result of startup recovery for durable retention quarantine groups.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStoreRecoveryReport {
    /// Precommit groups restored to their original managed paths.
    pub restored_precommit_groups: u64,
    /// Committed groups whose registry update and deletion completed.
    pub completed_committed_groups: u64,
    /// Groups left protected because an original path or registry record changed.
    pub conflicted_groups: u64,
    /// Non-fatal corruption, conflict, or filesystem observations.
    pub issues: Vec<TaskStoreIssue>,
}

/// Inspects task storage without applying a retention bound.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the workspace or state root cannot be
/// inspected. Returns [`DaemonCoreError::UnsafeStateRoot`] if `.packet28` is a
/// symlink, is not a directory, or resolves outside the workspace.
pub fn inspect_task_store(root: &Path, observed_at_unix: u64) -> Result<TaskStoreReport> {
    retain_task_store(root, observed_at_unix, RetentionOptions::inspect())
}

/// Recovers durable retention quarantine groups before daemon state is loaded.
///
/// Precommit groups are restored without replacing any recreated source.
/// Committed groups finish their conditional registry removal and capability-
/// relative deletion. Corrupt or conflicting groups remain protected and are
/// reported rather than guessed.
///
/// Call this before acquiring the daemon's long-lived shared task-store lease.
///
/// # Errors
///
/// Waits for an in-flight supported writer or maintenance operation. Returns
/// [`DaemonCoreError::Io`] or [`DaemonCoreError::UnsafeStateRoot`] when the
/// state or quarantine capability cannot be safely opened.
#[cfg(unix)]
pub fn recover_task_store_quarantine(root: &Path) -> Result<TaskStoreRecoveryReport> {
    recover_task_store_quarantine_with_observer(root, || {})
}

#[cfg(unix)]
fn recover_task_store_quarantine_with_observer(
    root: &Path,
    after_initial_snapshot: impl FnOnce(),
) -> Result<TaskStoreRecoveryReport> {
    let lease = acquire_task_store_recovery_lease(root)?;
    after_initial_snapshot();
    let state = lease.state_capability()?;
    let daemon = lease.daemon_capability()?;
    recover_quarantine_groups(lease.workspace_root(), &state, &daemon)
}

/// Recovers retention state and returns the daemon's long-lived shared lease.
///
/// `daemon_instance_lease` must be the exclusive daemon-instance lease for
/// `root`, acquired before recovery begins and retained by the caller through
/// daemon shutdown. The handoff is gap-safe: after exclusive recovery, the
/// function acquires a shared daemon lease and rechecks the quarantine while
/// both leases prevent a new daemon or cleanup from entering the conversion
/// gap. Conflicted groups refuse startup.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] for unsafe or conflicted recovery state and
/// for lifecycle lock failures. Supported writers and maintenance operations
/// may delay this blocking startup handoff.
pub fn recover_task_store_quarantine_and_acquire_daemon_lease(
    root: &Path,
    daemon_instance_lease: &TaskStoreLease,
) -> Result<(TaskStoreRecoveryReport, TaskStoreLease)> {
    recover_task_store_quarantine_and_acquire_daemon_lease_with_observer(
        root,
        daemon_instance_lease,
        || {},
    )
}

#[cfg(unix)]
fn recover_task_store_quarantine_and_acquire_daemon_lease_with_observer(
    root: &Path,
    daemon_instance_lease: &TaskStoreLease,
    after_first_recovery: impl FnOnce(),
) -> Result<(TaskStoreRecoveryReport, TaskStoreLease)> {
    validate_daemon_instance_lease(root, daemon_instance_lease)?;
    let mut aggregate = TaskStoreRecoveryReport::default();
    let mut observer = Some(after_first_recovery);
    for _ in 0..MAX_STARTUP_RECOVERY_PASSES {
        let recovery_lease = acquire_task_store_recovery_lease_from(daemon_instance_lease)?;
        let state = recovery_lease.state_capability()?;
        let daemon = recovery_lease.daemon_capability()?;
        let recovery = recover_quarantine_groups(recovery_lease.workspace_root(), &state, &daemon)?;
        drop(recovery_lease);
        aggregate.restored_precommit_groups = aggregate
            .restored_precommit_groups
            .saturating_add(recovery.restored_precommit_groups);
        aggregate.completed_committed_groups = aggregate
            .completed_committed_groups
            .saturating_add(recovery.completed_committed_groups);
        aggregate.conflicted_groups = aggregate
            .conflicted_groups
            .saturating_add(recovery.conflicted_groups);
        extend_issues(&mut aggregate.issues, recovery.issues);
        if aggregate.conflicted_groups > 0 {
            aggregate.issues.sort_by(|left, right| {
                (&left.kind, &left.path, &left.message).cmp(&(
                    &right.kind,
                    &right.path,
                    &right.message,
                ))
            });
            aggregate.issues.dedup();
            let first_issue = aggregate
                .issues
                .first()
                .map(|issue| issue.message.as_str())
                .unwrap_or("no issue detail was recorded");
            return Err(DaemonCoreError::io(
                "daemon startup recovery left conflicted quarantine state",
                Path::new(root)
                    .join(STATE_DIR_NAME)
                    .join(QUARANTINE_DIR_NAME),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} conflicted group(s); first issue: {first_issue}",
                        aggregate.conflicted_groups
                    ),
                ),
            ));
        }
        if let Some(observer) = observer.take() {
            observer();
        }
        let daemon_lease = acquire_daemon_task_store_lease_from(daemon_instance_lease)?;
        if !task_store_quarantine_has_groups(&daemon_lease)? {
            aggregate.issues.sort_by(|left, right| {
                (&left.kind, &left.path, &left.message).cmp(&(
                    &right.kind,
                    &right.path,
                    &right.message,
                ))
            });
            aggregate.issues.dedup();
            return Ok((aggregate, daemon_lease));
        }
        drop(daemon_lease);
    }
    Err(retention_resource_limit_error(
        "daemon startup recovery exceeded the supported handoff-pass bound",
        &Path::new(root)
            .join(STATE_DIR_NAME)
            .join(QUARANTINE_DIR_NAME),
        format!(
            "cleanup repeatedly won the recovery handoff for all {MAX_STARTUP_RECOVERY_PASSES} supported passes"
        ),
    ))
}

#[cfg(not(unix))]
fn recover_task_store_quarantine_and_acquire_daemon_lease_with_observer(
    root: &Path,
    daemon_instance_lease: &TaskStoreLease,
    after_first_recovery: impl FnOnce(),
) -> Result<(TaskStoreRecoveryReport, TaskStoreLease)> {
    validate_daemon_instance_lease(root, daemon_instance_lease)?;
    after_first_recovery();
    acquire_daemon_task_store_lease(root).map(|lease| (TaskStoreRecoveryReport::default(), lease))
}

fn validate_daemon_instance_lease(root: &Path, lease: &TaskStoreLease) -> Result<()> {
    let expected_path = daemon_instance_lock_path(lease.workspace_root());
    if lease.role() == LeaseRole::DaemonInstance
        && lease.matches_root_argument(root)
        && lease.path() == expected_path
    {
        return Ok(());
    }
    Err(DaemonCoreError::io(
        "daemon startup recovery requires the matching daemon-instance lease",
        &expected_path,
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "expected lease {}, received {}",
                expected_path.display(),
                lease.path().display()
            ),
        ),
    ))
}

/// Recovery is unavailable where retention cannot use Unix file identities.
///
/// # Errors
///
/// Always returns [`DaemonCoreError::RetentionApplyUnsupported`].
#[cfg(not(unix))]
pub fn recover_task_store_quarantine(_root: &Path) -> Result<TaskStoreRecoveryReport> {
    Err(DaemonCoreError::RetentionApplyUnsupported)
}

/// Plans or explicitly applies bounded task-store retention.
///
/// Dry-run is the default. Actual deletion requires
/// [`RetentionOptions::apply`] to be `true`, a configured age or byte bound, a
/// stopped daemon, reliable active-task state, and a race-safe candidate
/// revalidation.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidRetentionPolicy`] when apply is requested
/// without a bound. Returns [`DaemonCoreError::UnsafeStateRoot`] for an unsafe
/// state root, [`DaemonCoreError::RetentionBlockedByDaemon`] while the daemon
/// readiness marker exists, [`DaemonCoreError::RetentionApplyUnsupported`] on
/// platforms without Unix file identity, or [`DaemonCoreError::Io`] for fatal
/// filesystem operations.
pub fn retain_task_store(
    root: &Path,
    observed_at_unix: u64,
    options: RetentionOptions,
) -> Result<TaskStoreReport> {
    retain_task_store_with_lease_observer(root, observed_at_unix, options, || {})
}

fn retain_task_store_with_lease_observer(
    root: &Path,
    observed_at_unix: u64,
    options: RetentionOptions,
    after_lease_acquired: impl FnOnce(),
) -> Result<TaskStoreReport> {
    retain_task_store_with_lease_observers(
        root,
        observed_at_unix,
        options,
        || {},
        after_lease_acquired,
    )
}

fn retain_task_store_with_lease_observers(
    root: &Path,
    observed_at_unix: u64,
    options: RetentionOptions,
    before_lease_acquire: impl FnOnce(),
    after_lease_acquired: impl FnOnce(),
) -> Result<TaskStoreReport> {
    if options.apply && options.max_age_seconds.is_none() && options.max_bytes.is_none() {
        return Err(DaemonCoreError::InvalidRetentionPolicy {
            message: "explicit apply requires max_age_seconds, max_bytes, or both",
        });
    }

    #[cfg(not(unix))]
    if options.apply {
        return Err(DaemonCoreError::RetentionApplyUnsupported);
    }

    let mode = if options.max_age_seconds.is_none() && options.max_bytes.is_none() {
        RetentionMode::Inspect
    } else if options.apply {
        RetentionMode::Apply
    } else {
        RetentionMode::DryRun
    };
    let mut recovery = TaskStoreRecoveryReport::default();
    let (mut snapshot, mut plan, metrics_before, retention_apply_state) = if options.apply {
        // Applying retention performs no recursive or registry scan until the
        // exclusive lifecycle lease is held. This makes the one authoritative
        // snapshot, its metrics, and its plan describe the same store state.
        before_lease_acquire();
        let lease = try_acquire_task_store_retention_lease(root)?.ok_or_else(|| {
            DaemonCoreError::RetentionBlockedByDaemon {
                path: task_store_lifecycle_lock_path(root),
            }
        })?;
        let instance_gate =
            try_acquire_task_retention_instance_gate_from(&lease)?.ok_or_else(|| {
                DaemonCoreError::RetentionBlockedByDaemon {
                    path: daemon_instance_lock_path(lease.workspace_root()),
                }
            })?;
        let retained_state = lease.state_capability()?;
        let retained_daemon = lease.daemon_capability()?;
        after_lease_acquired();
        let mut snapshot =
            StoreSnapshot::load_with_lease(&lease, observed_at_unix, ScanLimits::DEFAULT)?;
        validate_apply_capability_filesystems(&snapshot, &retained_state, &retained_daemon)?;
        let readiness = ready_path(&snapshot.workspace_root);
        if retained_daemon
            .entry_identity(OsStr::new(READY_FILE_NAME))
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect retained daemon readiness marker",
                    &readiness,
                    source,
                )
            })?
            .is_some()
        {
            return Err(DaemonCoreError::RetentionBlockedByDaemon { path: readiness });
        }
        let recovered_registry_temps =
            remove_stale_task_registry_write_temps(&snapshot, &retained_daemon)?;
        let recovered_active_temps =
            remove_stale_active_task_write_temps(&snapshot, &retained_state)?;
        if recovered_registry_temps > 0 || recovered_active_temps > 0 {
            snapshot =
                StoreSnapshot::load_with_lease(&lease, observed_at_unix, ScanLimits::DEFAULT)?;
            if recovered_registry_temps > 0 {
                push_owned_issue(
                    &mut snapshot.issues,
                    TaskStoreIssue {
                        kind: "task_registry_write_temp_recovered".to_string(),
                        path: daemon_dir(&snapshot.workspace_root).display().to_string(),
                        message: format!(
                            "removed {recovered_registry_temps} stale task-registry atomic-write file(s)"
                        ),
                    },
                );
            }
            if recovered_active_temps > 0 {
                push_owned_issue(
                    &mut snapshot.issues,
                    TaskStoreIssue {
                        kind: "active_task_write_temp_recovered".to_string(),
                        path: agent_runtime_dir(&snapshot.workspace_root)
                            .display()
                            .to_string(),
                        message: format!(
                            "removed {recovered_active_temps} stale active-task atomic-write file(s)"
                        ),
                    },
                );
            }
        }
        let quarantine_present = retained_state
            .entry_identity(OsStr::new(QUARANTINE_DIR_NAME))
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect retained retention quarantine",
                    retained_state.display_path().join(QUARANTINE_DIR_NAME),
                    source,
                )
            })?
            .is_some();
        if quarantine_present {
            recovery = recover_quarantine_groups(
                &snapshot.workspace_root,
                &retained_state,
                &retained_daemon,
            )?;
            let mut prior_issues = std::mem::take(&mut snapshot.issues);
            extend_issues(&mut prior_issues, std::mem::take(&mut recovery.issues));
            snapshot =
                StoreSnapshot::load_with_lease(&lease, observed_at_unix, ScanLimits::DEFAULT)?;
            extend_issues(&mut snapshot.issues, prior_issues);
        }
        let metrics_before = snapshot.metrics.clone();
        let plan = build_plan(&snapshot, options);
        (
            snapshot,
            plan,
            metrics_before,
            RetentionApplyState::Armed {
                lease,
                admission: instance_gate,
            },
        )
    } else {
        let snapshot = StoreSnapshot::load(root, observed_at_unix)?;
        let metrics_before = snapshot.metrics.clone();
        let plan = build_plan(&snapshot, options);
        (
            snapshot,
            plan,
            metrics_before,
            RetentionApplyState::ReadOnly,
        )
    };

    #[cfg(unix)]
    if let RetentionApplyState::Armed { lease, admission } = &retention_apply_state {
        if !plan.items.is_empty() {
            apply_plan(&mut snapshot, &mut plan, lease, admission)?;
        }
    }
    #[cfg(not(unix))]
    if options.apply && !plan.items.is_empty() {
        apply_plan(&mut snapshot, &mut plan)?;
    }

    #[cfg(unix)]
    let final_snapshot = match &retention_apply_state {
        RetentionApplyState::Armed { lease, .. } => {
            Some(load_post_apply_snapshot_with_lease(lease, observed_at_unix))
        }
        RetentionApplyState::ReadOnly => None,
    };
    #[cfg(not(unix))]
    let final_snapshot = options
        .apply
        .then(|| load_post_apply_snapshot(&snapshot.workspace_root, observed_at_unix));
    let (metrics_after, final_rescan_reliable) = match final_snapshot {
        Some(Ok(final_snapshot)) => {
            extend_issues(&mut snapshot.issues, final_snapshot.issues);
            (final_snapshot.metrics, true)
        }
        Some(Err(error)) => {
            push_owned_issue(
                &mut snapshot.issues,
                TaskStoreIssue {
                    kind: "post_apply_rescan_failed".to_string(),
                    path: snapshot.state_root.display().to_string(),
                    message: error.to_string(),
                },
            );
            // Preserve the last reliable metrics rather than discarding
            // the already-completed per-action result. Consumers can gate
            // these fields on `final_rescan_reliable`.
            (snapshot.metrics.clone(), false)
        }
        None => (metrics_before.clone(), true),
    };
    let remaining_managed_logical_bytes = if options.apply {
        metrics_after.managed_task_logical_bytes
    } else {
        plan.projected_managed_logical_bytes
    };
    let remaining_over_limit_bytes = options
        .max_bytes
        .map(|limit| remaining_managed_logical_bytes.saturating_sub(limit))
        .unwrap_or(0);
    let removed_tasks = plan
        .actions
        .iter()
        .filter(|action| action.outcome == RetentionOutcome::Removed)
        .count() as u64;
    let removed_logical_bytes = plan
        .actions
        .iter()
        .map(|action| action.removed_logical_bytes);
    let removed_logical_bytes = saturating_sum_u64(removed_logical_bytes);
    let skipped_tasks = plan
        .actions
        .iter()
        .filter(|action| action.outcome == RetentionOutcome::Skipped)
        .count() as u64;
    let failed_tasks = plan
        .actions
        .iter()
        .filter(|action| action.outcome == RetentionOutcome::Failed)
        .count() as u64;
    let failed_logical_bytes = plan
        .actions
        .iter()
        .filter(|action| action.outcome == RetentionOutcome::Failed)
        .map(|action| action.remaining_logical_bytes);
    let failed_logical_bytes = saturating_sum_u64(failed_logical_bytes);
    let action_byte_accounting_reliable = plan
        .actions
        .iter()
        .all(|action| action.byte_accounting_reliable);
    snapshot.issues.sort_by(|left, right| {
        (&left.kind, &left.path, &left.message).cmp(&(&right.kind, &right.path, &right.message))
    });
    snapshot.issues.dedup();

    Ok(TaskStoreReport {
        schema_version: TASK_STORE_REPORT_SCHEMA_VERSION,
        observed_at_unix,
        workspace_root: snapshot.workspace_root.display().to_string(),
        state_root: snapshot.state_root.display().to_string(),
        mode,
        metrics_before,
        metrics_after,
        retention: RetentionAccounting {
            max_age_seconds: options.max_age_seconds,
            max_bytes: options.max_bytes,
            protected_tasks: plan.protected_tasks,
            protected_logical_bytes: plan.protected_logical_bytes,
            planned_tasks: plan.actions.len() as u64,
            planned_logical_bytes: saturating_sum_u64(
                plan.actions.iter().map(|action| action.logical_bytes),
            ),
            removed_tasks,
            removed_logical_bytes,
            skipped_tasks,
            failed_tasks,
            failed_logical_bytes,
            recovered_precommit_groups: recovery.restored_precommit_groups,
            recovered_committed_groups: recovery.completed_committed_groups,
            recovery_conflicted_groups: recovery.conflicted_groups,
            final_rescan_reliable,
            action_byte_accounting_reliable,
            remaining_managed_logical_bytes,
            remaining_over_limit_bytes,
        },
        actions: plan.actions,
        issues: snapshot.issues,
    })
}

fn saturating_sum_u64(values: impl IntoIterator<Item = u64>) -> u64 {
    values
        .into_iter()
        .fold(0_u64, |total, value| total.saturating_add(value))
}

#[cfg(unix)]
fn validate_apply_capability_filesystems(
    snapshot: &StoreSnapshot,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
) -> Result<()> {
    let expected_state_identity =
        snapshot
            .state_root_identity
            .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                path: snapshot.state_root.clone(),
            })?;
    if state.identity() != expected_state_identity {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: snapshot.state_root.clone(),
        });
    }
    ensure_same_filesystem(
        state.identity(),
        daemon.identity(),
        daemon.display_path(),
        "daemon state for retention is on another filesystem",
    )?;
    match state.open_private_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700) {
        Ok(quarantine) => ensure_same_filesystem(
            state.identity(),
            quarantine.identity(),
            quarantine.display_path(),
            "retention quarantine is on another filesystem",
        ),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonCoreError::io(
            "failed to open retention quarantine while validating filesystems",
            snapshot.state_root.join(QUARANTINE_DIR_NAME),
            source,
        )),
    }
}

#[cfg(unix)]
fn remove_stale_task_registry_write_temps(
    snapshot: &StoreSnapshot,
    daemon: &CapabilityDir,
) -> Result<usize> {
    let Some(expected_state_identity) = snapshot.state_root_identity else {
        return Ok(0);
    };
    ensure_same_filesystem(
        expected_state_identity,
        daemon.identity(),
        daemon.display_path(),
        "daemon state for registry recovery is on another filesystem",
    )?;
    with_anchored_registry_lock(daemon, &snapshot.workspace_root, || {
        daemon
            .remove_generated_regular_files(TASK_REGISTRY_WRITE_TEMP_PREFIX)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to recover stale task-registry atomic write",
                    daemon.display_path(),
                    source,
                )
            })
    })
}

#[cfg(unix)]
fn remove_stale_active_task_write_temps(
    snapshot: &StoreSnapshot,
    state: &CapabilityDir,
) -> Result<usize> {
    let Some(expected_state_identity) = snapshot.state_root_identity else {
        return Ok(0);
    };
    if state.identity() != expected_state_identity {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: snapshot.state_root.clone(),
        });
    }
    let agent_path = agent_runtime_dir(&snapshot.workspace_root);
    let agent = match state.open_dir(OsStr::new("agent")) {
        Ok(agent) => agent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open active-task state while recovering writes",
                &agent_path,
                source,
            ));
        }
    };
    ensure_same_filesystem(
        expected_state_identity,
        agent.identity(),
        agent.display_path(),
        "active-task state for write recovery is on another filesystem",
    )?;
    let lock_path = agent.display_path().join(ACTIVE_TASK_LOCK_FILE_NAME);
    let lock = AnchoredFileLock::acquire(
        &agent,
        OsStr::new(ACTIVE_TASK_LOCK_FILE_NAME),
        lock_path.clone(),
        AnchoredFileLockMode::Exclusive,
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open, acquire, or authenticate active-task write lock for recovery",
            &lock_path,
            source,
        )
    })?;
    let cleanup = agent
        .remove_generated_regular_files(ACTIVE_TASK_WRITE_TEMP_PREFIX)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to recover stale active-task atomic write",
                agent.display_path(),
                source,
            )
        });
    let finish = lock.finish();
    match (cleanup, finish) {
        (_, Err(AnchoredFileLockFinishError::Attachment(source))) => {
            Err(DaemonCoreError::StorageMutationAuthorityLost {
                operation: "active-task temporary recovery",
                path: lock_path,
                source,
            })
        }
        (Ok(removed), Ok(())) => Ok(removed),
        (Err(error), _) => Err(error),
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock active-task write recovery",
            &lock_path,
            source,
        )),
    }
}

#[cfg(test)]
std::thread_local! {
    static INJECT_POST_APPLY_RESCAN_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static INJECT_HANDOFF_QUARANTINE_PRESENT_PASSES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(all(test, unix))]
std::thread_local! {
    static INJECT_COMMITTED_DELETION_BATCH_ENTRIES: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(not(unix))]
fn load_post_apply_snapshot(root: &Path, observed_at_unix: u64) -> Result<StoreSnapshot> {
    #[cfg(test)]
    if INJECT_POST_APPLY_RESCAN_FAILURE.with(|configured| configured.replace(false)) {
        return Err(DaemonCoreError::io(
            "injected post-apply task-store rescan failure",
            root,
            std::io::Error::other("injected post-apply rescan failure"),
        ));
    }
    StoreSnapshot::load(root, observed_at_unix)
}

#[cfg(unix)]
fn load_post_apply_snapshot_with_lease(
    lease: &TaskStoreLease,
    observed_at_unix: u64,
) -> Result<StoreSnapshot> {
    #[cfg(test)]
    if INJECT_POST_APPLY_RESCAN_FAILURE.with(|configured| configured.replace(false)) {
        return Err(DaemonCoreError::io(
            "injected post-apply task-store rescan failure",
            lease.workspace_root(),
            std::io::Error::other("injected post-apply rescan failure"),
        ));
    }
    StoreSnapshot::load_with_lease(lease, observed_at_unix, ScanLimits::DEFAULT)
}

#[derive(Debug, Clone)]
struct StoreSnapshot {
    workspace_root: PathBuf,
    state_root: PathBuf,
    observed_at_unix: u64,
    metrics: TaskStoreMetrics,
    state_root_identity: Option<FileIdentity>,
    candidates: BTreeMap<CandidateKey, Candidate>,
    unattributed_protected_logical_bytes: u64,
    issues: Vec<TaskStoreIssue>,
}

impl StoreSnapshot {
    fn load(root: &Path, observed_at_unix: u64) -> Result<Self> {
        Self::load_with_limits(root, observed_at_unix, ScanLimits::DEFAULT)
    }

    fn load_with_limits(
        root: &Path,
        observed_at_unix: u64,
        scan_limits: ScanLimits,
    ) -> Result<Self> {
        let workspace_root = fs::canonicalize(root).map_err(|source| {
            DaemonCoreError::io("failed to resolve workspace root", root, source)
        })?;
        let state_root = workspace_root.join(STATE_DIR_NAME);
        match fs::symlink_metadata(&state_root) {
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    workspace_root,
                    state_root,
                    observed_at_unix,
                    metrics: TaskStoreMetrics {
                        task_registry_reliable: true,
                        allocated_bytes_supported: cfg!(unix),
                        ..TaskStoreMetrics::default()
                    },
                    state_root_identity: None,
                    candidates: BTreeMap::new(),
                    unattributed_protected_logical_bytes: 0,
                    issues: Vec::new(),
                });
            }
            Err(source) => {
                return Err(DaemonCoreError::io(
                    "failed to inspect Packet28 state root",
                    &state_root,
                    source,
                ));
            }
        }

        let state_root = validate_state_root(&workspace_root, &state_root)?;
        #[cfg(unix)]
        let state_capability = CapabilityDir::open(&state_root).map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state capability for inspection",
                &state_root,
                source,
            )
        })?;
        let mut issues = Vec::new();
        let state_scan =
            scan_path_with_limits(&state_root, &mut issues, "state_entry", scan_limits)?;
        #[cfg(unix)]
        if state_scan.identity != Some(state_capability.identity()) {
            return Err(DaemonCoreError::RetentionCandidateChanged { path: state_root });
        }
        let state_identity = state_scan.identity;
        #[cfg(unix)]
        let (registry_snapshot, active_snapshot) =
            read_authority_snapshots(&workspace_root, &state_capability, &mut issues)?;
        #[cfg(not(unix))]
        let registry_snapshot = read_registry(&workspace_root, &mut issues)?;
        #[cfg(not(unix))]
        let active_snapshot = read_active_task(&workspace_root, &mut issues)?;
        let managed_layout_reliable =
            managed_layout_is_reliable(&workspace_root, &state_root, state_identity, &mut issues);
        let mut candidates = BTreeMap::<CandidateKey, Candidate>::new();

        add_registry_candidates(&workspace_root, &registry_snapshot, &mut candidates)?;
        add_artifact_candidates(
            &workspace_root,
            &mut candidates,
            &mut issues,
            scan_limits,
            state_identity,
        )?;
        add_event_candidates(
            &workspace_root,
            &mut candidates,
            &mut issues,
            scan_limits,
            state_identity,
        )?;
        protect_aliased_candidates(&mut candidates);

        let active_storage_keys = active_storage_keys(
            &workspace_root,
            &registry_snapshot.registry,
            active_snapshot.task_id.as_deref(),
        );
        let reliable_protection =
            registry_snapshot.reliable && active_snapshot.reliable && managed_layout_reliable;
        for candidate in candidates.values_mut() {
            if !reliable_protection {
                candidate.protected_reasons.insert(
                    "active-task, registry, or managed layout state is corrupt, unreadable, or unsafe"
                        .to_string(),
                );
            }
            if storage_key_is_active(&active_storage_keys, &candidate.storage_key) {
                candidate
                    .protected_reasons
                    .insert("task is active".to_string());
            }
            if candidate.task_ids.len() > 1 {
                candidate
                    .protected_reasons
                    .insert("multiple task identifiers map to the same storage key".to_string());
            }
            if !candidate.safe {
                candidate
                    .protected_reasons
                    .insert("candidate contains an unsafe or unreadable entry".to_string());
            }
        }

        let artifact_scan = scan_path_with_limits_from_parent(
            &task_artifacts_dir(&workspace_root),
            &mut Vec::new(),
            "artifact_entry",
            scan_limits,
            state_identity,
        )?;
        let event_scan = scan_path_with_limits_from_parent(
            &task_events_dir(&workspace_root),
            &mut Vec::new(),
            "event_entry",
            scan_limits,
            state_identity,
        )?;
        let (quarantine_scan, quarantine_groups) =
            inspect_quarantine_layout(&state_root, state_identity, &mut issues, scan_limits)?;
        // A registry that cannot be decoded has no trustworthy record-level
        // attribution. Count its complete raw size once as protected managed
        // state instead of reporting an empty store or guessing records.
        let unreliable_registry_logical_bytes = if registry_snapshot.reliable {
            0
        } else {
            registry_snapshot.file_bytes
        };
        let unattributed_protected_logical_bytes =
            unreliable_registry_logical_bytes.saturating_add(quarantine_scan.logical_bytes);
        let managed_task_logical_bytes = candidates
            .values()
            .map(Candidate::logical_bytes)
            .fold(0_u64, u64::saturating_add)
            .saturating_add(unattributed_protected_logical_bytes);
        let managed_task_allocated_bytes = registry_snapshot
            .allocated_bytes
            .saturating_add(registry_snapshot.wal_allocated_bytes)
            .saturating_add(artifact_scan.allocated_bytes)
            .saturating_add(event_scan.allocated_bytes)
            .saturating_add(quarantine_scan.allocated_bytes);
        let active_tasks = active_storage_keys.len() as u64;
        let oldest_task_timestamp_unix = candidates
            .values()
            .filter_map(|candidate| candidate.latest_timestamp_unix)
            .min();
        let newest_task_timestamp_unix = candidates
            .values()
            .filter_map(|candidate| candidate.latest_timestamp_unix)
            .max();
        let metrics = TaskStoreMetrics {
            state_logical_bytes: state_scan.logical_bytes,
            state_allocated_bytes: state_scan.allocated_bytes,
            allocated_bytes_supported: cfg!(unix),
            state_files: state_scan.files,
            state_directories: state_scan.directories,
            state_symlinks: state_scan.symlinks,
            task_registry_file_bytes: registry_snapshot
                .file_bytes
                .saturating_add(registry_snapshot.wal_file_bytes),
            task_registry_allocated_bytes: registry_snapshot
                .allocated_bytes
                .saturating_add(registry_snapshot.wal_allocated_bytes),
            task_registry_records: registry_snapshot.registry.tasks.len() as u64,
            task_registry_reliable: registry_snapshot.reliable,
            task_artifact_logical_bytes: artifact_scan.logical_bytes,
            task_artifact_allocated_bytes: artifact_scan.allocated_bytes,
            task_artifact_files: artifact_scan.files,
            task_artifact_directories: artifact_scan.directories,
            task_event_logical_bytes: event_scan.logical_bytes,
            task_event_allocated_bytes: event_scan.allocated_bytes,
            task_event_files: event_scan.files,
            retention_quarantine_logical_bytes: quarantine_scan.logical_bytes,
            retention_quarantine_allocated_bytes: quarantine_scan.allocated_bytes,
            retention_quarantine_groups: quarantine_groups,
            managed_task_logical_bytes,
            managed_task_allocated_bytes,
            active_tasks,
            oldest_task_timestamp_unix,
            newest_task_timestamp_unix,
        };
        issues.sort_by(|left, right| {
            (&left.kind, &left.path, &left.message).cmp(&(&right.kind, &right.path, &right.message))
        });
        issues.dedup();

        Ok(Self {
            workspace_root,
            state_root,
            observed_at_unix,
            metrics,
            state_root_identity: state_scan.identity,
            candidates,
            unattributed_protected_logical_bytes,
            issues,
        })
    }

    #[cfg(unix)]
    fn load_with_lease(
        lease: &TaskStoreLease,
        observed_at_unix: u64,
        scan_limits: ScanLimits,
    ) -> Result<Self> {
        lease.validate_namespace_attachment()?;
        let workspace_root = lease.workspace_root().to_path_buf();
        let state = lease.state_capability()?;
        let daemon = lease.daemon_capability()?;
        let state_root = state.display_path().to_path_buf();
        let mut issues = Vec::new();
        let state_scan =
            scan_capability_directory_with_limits(&state, &mut issues, "state_entry", scan_limits)?;
        let state_identity = Some(state.identity());
        let (registry_snapshot, active_snapshot) =
            read_authority_snapshots_from_daemon(&workspace_root, &state, &daemon, &mut issues)?;

        let (artifact_root, artifact_reliable) = open_optional_managed_directory(
            &state,
            OsStr::new("task"),
            &mut issues,
            "artifact_root",
        );
        let (event_root, event_reliable) = open_optional_managed_directory(
            &daemon,
            OsStr::new("tasks"),
            &mut issues,
            "event_root",
        );
        let (_agent_root, agent_reliable) =
            open_optional_managed_directory(&state, OsStr::new("agent"), &mut issues, "agent_root");
        let managed_layout_reliable = artifact_reliable && event_reliable && agent_reliable;

        let mut candidates = BTreeMap::<CandidateKey, Candidate>::new();
        add_registry_candidates(&workspace_root, &registry_snapshot, &mut candidates)?;
        add_artifact_candidates_anchored(
            artifact_root.as_ref(),
            &mut candidates,
            &mut issues,
            scan_limits,
        )?;
        add_event_candidates_anchored(
            event_root.as_ref(),
            &mut candidates,
            &mut issues,
            scan_limits,
        )?;
        protect_aliased_candidates(&mut candidates);

        let active_storage_keys = active_storage_keys(
            &workspace_root,
            &registry_snapshot.registry,
            active_snapshot.task_id.as_deref(),
        );
        let reliable_protection =
            registry_snapshot.reliable && active_snapshot.reliable && managed_layout_reliable;
        for candidate in candidates.values_mut() {
            if !reliable_protection {
                candidate.protected_reasons.insert(
                    "active-task, registry, or managed layout state is corrupt, unreadable, or unsafe"
                        .to_string(),
                );
            }
            if storage_key_is_active(&active_storage_keys, &candidate.storage_key) {
                candidate
                    .protected_reasons
                    .insert("task is active".to_string());
            }
            if candidate.task_ids.len() > 1 {
                candidate
                    .protected_reasons
                    .insert("multiple task identifiers map to the same storage key".to_string());
            }
            if !candidate.safe {
                candidate
                    .protected_reasons
                    .insert("candidate contains an unsafe or unreadable entry".to_string());
            }
        }

        let artifact_scan = match artifact_root.as_ref() {
            Some(directory) => scan_capability_directory_with_limits(
                directory,
                &mut Vec::new(),
                "artifact_entry",
                scan_limits,
            )?,
            None => ScanSummary {
                safe: artifact_reliable,
                ..ScanSummary::default()
            },
        };
        let event_scan = match event_root.as_ref() {
            Some(directory) => scan_capability_directory_with_limits(
                directory,
                &mut Vec::new(),
                "event_entry",
                scan_limits,
            )?,
            None => ScanSummary {
                safe: event_reliable,
                ..ScanSummary::default()
            },
        };
        let (quarantine_scan, quarantine_groups) =
            inspect_quarantine_layout_anchored(&state, &mut issues, scan_limits)?;
        lease.validate_namespace_attachment()?;

        let unreliable_registry_logical_bytes = if registry_snapshot.reliable {
            0
        } else {
            registry_snapshot.file_bytes
        };
        let unattributed_protected_logical_bytes =
            unreliable_registry_logical_bytes.saturating_add(quarantine_scan.logical_bytes);
        let managed_task_logical_bytes = candidates
            .values()
            .map(Candidate::logical_bytes)
            .fold(0_u64, u64::saturating_add)
            .saturating_add(unattributed_protected_logical_bytes);
        let managed_task_allocated_bytes = registry_snapshot
            .allocated_bytes
            .saturating_add(registry_snapshot.wal_allocated_bytes)
            .saturating_add(artifact_scan.allocated_bytes)
            .saturating_add(event_scan.allocated_bytes)
            .saturating_add(quarantine_scan.allocated_bytes);
        let metrics = TaskStoreMetrics {
            state_logical_bytes: state_scan.logical_bytes,
            state_allocated_bytes: state_scan.allocated_bytes,
            allocated_bytes_supported: true,
            state_files: state_scan.files,
            state_directories: state_scan.directories,
            state_symlinks: state_scan.symlinks,
            task_registry_file_bytes: registry_snapshot
                .file_bytes
                .saturating_add(registry_snapshot.wal_file_bytes),
            task_registry_allocated_bytes: registry_snapshot
                .allocated_bytes
                .saturating_add(registry_snapshot.wal_allocated_bytes),
            task_registry_records: registry_snapshot.registry.tasks.len() as u64,
            task_registry_reliable: registry_snapshot.reliable,
            task_artifact_logical_bytes: artifact_scan.logical_bytes,
            task_artifact_allocated_bytes: artifact_scan.allocated_bytes,
            task_artifact_files: artifact_scan.files,
            task_artifact_directories: artifact_scan.directories,
            task_event_logical_bytes: event_scan.logical_bytes,
            task_event_allocated_bytes: event_scan.allocated_bytes,
            task_event_files: event_scan.files,
            retention_quarantine_logical_bytes: quarantine_scan.logical_bytes,
            retention_quarantine_allocated_bytes: quarantine_scan.allocated_bytes,
            retention_quarantine_groups: quarantine_groups,
            managed_task_logical_bytes,
            managed_task_allocated_bytes,
            active_tasks: active_storage_keys.len() as u64,
            oldest_task_timestamp_unix: candidates
                .values()
                .filter_map(|candidate| candidate.latest_timestamp_unix)
                .min(),
            newest_task_timestamp_unix: candidates
                .values()
                .filter_map(|candidate| candidate.latest_timestamp_unix)
                .max(),
        };
        issues.sort_by(|left, right| {
            (&left.kind, &left.path, &left.message).cmp(&(&right.kind, &right.path, &right.message))
        });
        issues.dedup();
        Ok(Self {
            workspace_root,
            state_root,
            observed_at_unix,
            metrics,
            state_root_identity: state_identity,
            candidates,
            unattributed_protected_logical_bytes,
            issues,
        })
    }

    #[cfg(test)]
    fn candidate(&self, storage_key: &str) -> Option<&Candidate> {
        self.candidates
            .get(&CandidateKey::Managed(storage_key.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateKey {
    Managed(String),
    Opaque {
        namespace: OpaqueNamespace,
        raw_name_digest: [u8; 32],
    },
}

impl CandidateKey {
    fn managed(storage_key: String) -> Self {
        Self::Managed(storage_key)
    }

    fn opaque(namespace: OpaqueNamespace, name: &OsStr) -> Self {
        Self::Opaque {
            namespace,
            raw_name_digest: *blake3::hash(name.as_encoded_bytes()).as_bytes(),
        }
    }

    fn report_storage_key(&self) -> String {
        match self {
            Self::Managed(storage_key) => storage_key.clone(),
            Self::Opaque {
                namespace,
                raw_name_digest,
            } => format!(
                "__opaque/{}/{}",
                namespace.as_str(),
                blake3::Hash::from_bytes(*raw_name_digest)
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OpaqueNamespace {
    Artifact,
    Event,
}

impl OpaqueNamespace {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Event => "event",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Candidate {
    storage_key: String,
    task_ids: Vec<String>,
    record_values: BTreeMap<String, serde_json::Value>,
    registry_revision: Option<crate::storage::RegistryRevision>,
    registry_checkpoint_generation: Option<u64>,
    record_logical_bytes: u64,
    artifact: Option<ManagedComponent>,
    event: Option<ManagedComponent>,
    latest_timestamp_unix: Option<u64>,
    safe: bool,
    protected_reasons: BTreeSet<String>,
}

impl Candidate {
    fn new(storage_key: String) -> Self {
        Self {
            storage_key,
            safe: true,
            ..Self::default()
        }
    }

    fn logical_bytes(&self) -> u64 {
        self.record_logical_bytes
            .saturating_add(
                self.artifact
                    .as_ref()
                    .map_or(0, |component| component.scan.logical_bytes),
            )
            .saturating_add(
                self.event
                    .as_ref()
                    .map_or(0, |component| component.scan.logical_bytes),
            )
    }

    fn update_timestamp(&mut self, timestamp: Option<u64>) {
        self.latest_timestamp_unix = match (self.latest_timestamp_unix, timestamp) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
    }
}

#[derive(Debug, Clone)]
struct ManagedComponent {
    path: PathBuf,
    scan: ScanSummary,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileIdentity {
    #[cfg(unix)]
    pub(crate) device: u64,
    #[cfg(unix)]
    pub(crate) inode: u64,
    #[cfg(not(unix))]
    pub(crate) length: u64,
    #[cfg(not(unix))]
    pub(crate) modified_unix_nanos: u128,
}

fn protect_aliased_candidates(candidates: &mut BTreeMap<CandidateKey, Candidate>) {
    let mut filesystem_alias_owners = BTreeMap::<String, BTreeSet<CandidateKey>>::new();
    #[cfg(unix)]
    let mut physical_identity_owners = BTreeMap::<FileIdentity, BTreeSet<CandidateKey>>::new();

    for (candidate_key, candidate) in candidates.iter() {
        if matches!(candidate_key, CandidateKey::Managed(_)) {
            filesystem_alias_owners
                .entry(crate::storage::task_storage_key_alias_class(
                    &candidate.storage_key,
                ))
                .or_default()
                .insert(candidate_key.clone());
        }
        for component in [candidate.artifact.as_ref(), candidate.event.as_ref()]
            .into_iter()
            .flatten()
        {
            #[cfg(unix)]
            for identity in &component.scan.physical_identities {
                physical_identity_owners
                    .entry(*identity)
                    .or_default()
                    .insert(candidate_key.clone());
            }
        }
    }

    for (candidate_key, candidate) in candidates.iter_mut() {
        if matches!(candidate_key, CandidateKey::Managed(_)) {
            if !crate::storage::task_storage_key_is_portable(&candidate.storage_key) {
                candidate.protected_reasons.insert(
                    "storage-key spelling is not portable across supported filesystems".to_string(),
                );
            }
            if filesystem_alias_owners
                .get(&crate::storage::task_storage_key_alias_class(
                    &candidate.storage_key,
                ))
                .is_some_and(|owners| owners.len() > 1)
            {
                candidate.protected_reasons.insert(
                    "multiple storage-key spellings alias on supported filesystems".to_string(),
                );
            }
        }
        #[cfg(unix)]
        let has_physical_alias = [candidate.artifact.as_ref(), candidate.event.as_ref()]
            .into_iter()
            .flatten()
            .flat_map(|component| component.scan.physical_identities.iter())
            .any(|identity| {
                physical_identity_owners
                    .get(identity)
                    .is_some_and(|owners| owners.len() > 1)
            });
        #[cfg(not(unix))]
        let has_physical_alias = false;
        if has_physical_alias {
            candidate.protected_reasons.insert(
                "multiple logical candidates resolve to the same physical entry".to_string(),
            );
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct QuarantineJournal {
    schema_version: u32,
    phase: QuarantinePhase,
    storage_key: String,
    record_values: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    registry_revision: Option<crate::storage::RegistryRevision>,
    #[serde(default)]
    registry_checkpoint_generation: Option<u64>,
    components: Vec<JournalComponent>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QuarantinePhase {
    Precommit,
    Committed,
}

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct JournalComponent {
    kind: JournalComponentKind,
    identity: FileIdentity,
}

#[cfg(all(unix, test))]
std::thread_local! {
    static INJECT_TASK_REGISTRY_JOURNAL_LIMIT:
        std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(unix, test))]
pub(crate) fn inject_task_registry_journal_limit_once(max_bytes: usize) {
    INJECT_TASK_REGISTRY_JOURNAL_LIMIT.with(|configured| configured.set(Some(max_bytes)));
}

#[cfg(unix)]
pub(crate) fn validate_task_registry_retention_envelopes(
    path: &Path,
    registry: &TaskRegistry,
    encoded_registry_bytes: usize,
) -> Result<()> {
    if encoded_registry_bytes > MAX_TASK_REGISTRY_BYTES {
        return Err(DaemonCoreError::TaskRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: encoded_registry_bytes as u64,
            max_bytes: MAX_TASK_REGISTRY_BYTES as u64,
        });
    }
    #[cfg(test)]
    let max_bytes = INJECT_TASK_REGISTRY_JOURNAL_LIMIT
        .with(|configured| configured.take())
        .unwrap_or(MAX_TASK_RETENTION_JOURNAL_BYTES);
    #[cfg(not(test))]
    let max_bytes = MAX_TASK_RETENTION_JOURNAL_BYTES;

    let maximum_identity = FileIdentity {
        device: u64::MAX,
        inode: u64::MAX,
    };
    for (task_id, record) in &registry.tasks {
        let record_value = serde_json::to_value(record).map_err(|source| {
            DaemonCoreError::json(
                "failed to encode task record for retention journal validation from",
                path,
                source,
            )
        })?;
        let journal = QuarantineJournal {
            schema_version: QUARANTINE_JOURNAL_SCHEMA_VERSION,
            phase: QuarantinePhase::Committed,
            storage_key: storage_key_for_task(Path::new(""), task_id),
            record_values: BTreeMap::from([(task_id.clone(), record_value)]),
            registry_revision: Some(crate::storage::RegistryRevision::ZERO),
            registry_checkpoint_generation: None,
            components: vec![
                JournalComponent {
                    kind: JournalComponentKind::Artifacts,
                    identity: maximum_identity,
                },
                JournalComponent {
                    kind: JournalComponentKind::Events,
                    identity: maximum_identity,
                },
            ],
        };
        let journal_bytes = serde_json::to_vec(&journal).map_err(|source| {
            DaemonCoreError::json(
                "failed to encode maximum retention journal for",
                path,
                source,
            )
        })?;
        if journal_bytes.len() > max_bytes {
            return Err(DaemonCoreError::TaskRegistryRetentionEnvelopeTooLarge {
                path: path.to_path_buf(),
                journal_bytes: journal_bytes.len() as u64,
                max_bytes: max_bytes as u64,
            });
        }
        crate::storage::validate_authority_json(
            &journal_bytes,
            AuthorityJsonProfile::RetentionJournal { max_bytes },
        )
        .map_err(|error| {
            crate::storage::map_authority_json_error(
                path,
                AuthorityJsonProfile::RetentionJournal { max_bytes },
                "failed to validate maximum retention journal for",
                error,
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum JournalComponentKind {
    Artifacts,
    Events,
}

#[cfg(unix)]
impl JournalComponentKind {
    const fn staged_name(self) -> &'static str {
        match self {
            Self::Artifacts => "artifacts",
            Self::Events => "events.jsonl",
        }
    }

    const fn deletion_name(self) -> &'static str {
        match self {
            Self::Artifacts => ".deleting-artifacts",
            Self::Events => ".deleting-events",
        }
    }

    const fn restoration_name(self) -> &'static str {
        match self {
            Self::Artifacts => ".restoring-artifacts",
            Self::Events => ".restoring-events",
        }
    }

    fn original_relative_path(self, storage_key: &str) -> PathBuf {
        match self {
            Self::Artifacts => PathBuf::from("task").join(storage_key),
            Self::Events => PathBuf::from("daemon")
                .join("tasks")
                .join(format!("{storage_key}{TASK_EVENT_LOG_SUFFIX}")),
        }
    }
}

#[derive(Debug)]
struct RegistrySnapshot {
    registry: TaskRegistry,
    record_values: BTreeMap<String, serde_json::Value>,
    revision: Option<crate::storage::RegistryRevision>,
    checkpoint_generation: Option<u64>,
    reliable: bool,
    file_bytes: u64,
    allocated_bytes: u64,
    wal_file_bytes: u64,
    wal_allocated_bytes: u64,
}

#[derive(Debug)]
struct ActiveTaskSnapshot {
    task_id: Option<String>,
    reliable: bool,
}

#[derive(Debug)]
struct TargetedCandidateSnapshot {
    candidate: Option<Candidate>,
    active_storage_keys: BTreeSet<String>,
    reliable: bool,
    issues: Vec<TaskStoreIssue>,
}

#[derive(Debug)]
struct RetentionPlan {
    items: Vec<PlanItem>,
    actions: Vec<RetentionAction>,
    protected_tasks: u64,
    protected_logical_bytes: u64,
    projected_managed_logical_bytes: u64,
}

#[derive(Debug, Clone)]
struct PlanItem {
    candidate: Candidate,
    reasons: BTreeSet<RetentionReason>,
}

fn validate_state_root(workspace_root: &Path, state_root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(state_root).map_err(|source| {
        DaemonCoreError::io("failed to inspect Packet28 state root", state_root, source)
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DaemonCoreError::UnsafeStateRoot {
            workspace_root: workspace_root.to_path_buf(),
            state_root: state_root.to_path_buf(),
            reason: "state root is a symlink",
        });
    }
    if !metadata.is_dir() {
        return Err(DaemonCoreError::UnsafeStateRoot {
            workspace_root: workspace_root.to_path_buf(),
            state_root: state_root.to_path_buf(),
            reason: "state root is not a directory",
        });
    }
    let canonical = fs::canonicalize(state_root).map_err(|source| {
        DaemonCoreError::io("failed to resolve Packet28 state root", state_root, source)
    })?;
    if !canonical.starts_with(workspace_root) {
        return Err(DaemonCoreError::UnsafeStateRoot {
            workspace_root: workspace_root.to_path_buf(),
            state_root: canonical,
            reason: "state root resolves outside the workspace",
        });
    }
    Ok(canonical)
}

#[cfg(test)]
std::thread_local! {
    static INJECT_REGISTRY_BEFORE_CAPABILITY_READ:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static INJECT_ACTIVE_TASK_BEFORE_CAPABILITY_READ:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn inject_registry_before_capability_read_once(observer: impl FnOnce() + 'static) {
    INJECT_REGISTRY_BEFORE_CAPABILITY_READ.with(|configured| {
        *configured.borrow_mut() = Some(Box::new(observer));
    });
}

#[cfg(test)]
fn inject_active_task_before_capability_read_once(observer: impl FnOnce() + 'static) {
    INJECT_ACTIVE_TASK_BEFORE_CAPABILITY_READ.with(|configured| {
        *configured.borrow_mut() = Some(Box::new(observer));
    });
}

#[cfg(unix)]
fn read_authority_snapshots(
    root: &Path,
    state: &CapabilityDir,
    issues: &mut Vec<TaskStoreIssue>,
) -> Result<(RegistrySnapshot, ActiveTaskSnapshot)> {
    let path = task_registry_path(root);
    let daemon = match state.open_dir(OsStr::new("daemon")) {
        Ok(daemon) => daemon,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let registry = empty_registry_snapshot(true);
            let mut active = read_active_task(root, state, issues)?;
            reconcile_active_task_authority(root, &registry, &mut active, issues);
            return Ok((registry, active));
        }
        Err(source) => {
            push_issue(
                issues,
                "registry_unreadable",
                &path,
                format!("failed to open task-registry capability: {source}"),
            );
            return Ok(unreliable_authority_snapshots());
        }
    };
    read_authority_snapshots_from_daemon(root, state, &daemon, issues)
}

#[cfg(unix)]
fn read_authority_snapshots_from_daemon(
    root: &Path,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
    issues: &mut Vec<TaskStoreIssue>,
) -> Result<(RegistrySnapshot, ActiveTaskSnapshot)> {
    let path = task_registry_path(root);
    let lock_path = daemon.display_path().join(TASK_REGISTRY_LOCK_FILE_NAME);
    let lock = match daemon.open_existing_lock_file(OsStr::new(TASK_REGISTRY_LOCK_FILE_NAME)) {
        Ok(lock) => lock,
        Err(source) => {
            push_issue(
                issues,
                "registry_unreadable",
                &lock_path,
                format!("failed to open existing task-registry lock: {source}"),
            );
            return Ok(unreliable_authority_snapshots());
        }
    };
    let lock = match lock {
        Some(file) => match AnchoredFileLock::lock_existing(
            daemon,
            OsStr::new(TASK_REGISTRY_LOCK_FILE_NAME),
            lock_path.clone(),
            file,
            AnchoredFileLockMode::Shared,
        ) {
            Ok(lock) => Some(lock),
            Err(source) => {
                push_issue(
                    issues,
                    "registry_unreadable",
                    &lock_path,
                    format!(
                        "failed to acquire or authenticate shared task-registry lock: {source}"
                    ),
                );
                return Ok(unreliable_authority_snapshots());
            }
        },
        None => None,
    };

    #[cfg(test)]
    INJECT_REGISTRY_BEFORE_CAPABILITY_READ.with(|configured| {
        if let Some(observer) = configured.borrow_mut().take() {
            observer();
        }
    });

    let read = daemon.read_file_limited_with_metadata(
        OsStr::new(TASK_REGISTRY_FILE_NAME),
        MAX_TASK_REGISTRY_BYTES,
    );
    let (fallback_wal_file_bytes, fallback_wal_allocated_bytes) = daemon
        .entry_storage_bytes(OsStr::new(crate::storage::REGISTRY_DELTA_WAL_FILE_NAME))
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut snapshot = match read {
        Ok(read) => match decode_registry_with_raw_values(&read.bytes) {
            Ok((fallback_registry, fallback_values)) => {
                match crate::storage::load_retained_registry_snapshot_under_task_lock(root, daemon)
                {
                    Ok(loaded) => RegistrySnapshot {
                        registry: loaded.registry,
                        record_values: loaded.record_values,
                        revision: Some(loaded.revision),
                        checkpoint_generation: loaded.checkpoint_generation,
                        reliable: true,
                        file_bytes: read.logical_bytes,
                        allocated_bytes: read.allocated_bytes,
                        wal_file_bytes: loaded.wal_file_bytes,
                        wal_allocated_bytes: loaded.wal_allocated_bytes,
                    },
                    Err(source) => {
                        push_issue(
                            issues,
                            "registry_unreadable",
                            &path,
                            format!(
                                "failed to load checkpoint-plus-WAL registry authority: {source}"
                            ),
                        );
                        RegistrySnapshot {
                            registry: fallback_registry,
                            record_values: fallback_values,
                            revision: None,
                            checkpoint_generation: None,
                            reliable: false,
                            file_bytes: read.logical_bytes,
                            allocated_bytes: read.allocated_bytes,
                            wal_file_bytes: fallback_wal_file_bytes,
                            wal_allocated_bytes: fallback_wal_allocated_bytes,
                        }
                    }
                }
            }
            Err(source) => {
                push_issue(
                    issues,
                    "registry_corrupt",
                    &path,
                    format!("failed to decode task registry: {source}"),
                );
                RegistrySnapshot {
                    registry: TaskRegistry::default(),
                    record_values: BTreeMap::new(),
                    revision: None,
                    checkpoint_generation: None,
                    reliable: false,
                    file_bytes: read.logical_bytes,
                    allocated_bytes: read.allocated_bytes,
                    wal_file_bytes: fallback_wal_file_bytes,
                    wal_allocated_bytes: fallback_wal_allocated_bytes,
                }
            }
        },
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            match crate::storage::load_retained_registry_snapshot_under_task_lock(root, daemon) {
                Ok(loaded) => RegistrySnapshot {
                    registry: loaded.registry,
                    record_values: loaded.record_values,
                    revision: Some(loaded.revision),
                    checkpoint_generation: loaded.checkpoint_generation,
                    reliable: true,
                    file_bytes: 0,
                    allocated_bytes: 0,
                    wal_file_bytes: loaded.wal_file_bytes,
                    wal_allocated_bytes: loaded.wal_allocated_bytes,
                },
                Err(source) => {
                    push_issue(
                        issues,
                        "registry_unreadable",
                        &path,
                        format!("failed to load checkpoint-plus-WAL registry authority: {source}"),
                    );
                    empty_registry_snapshot(false)
                }
            }
        }
        Err(source) => {
            let (file_bytes, allocated_bytes) = daemon
                .entry_storage_bytes(OsStr::new(TASK_REGISTRY_FILE_NAME))
                .ok()
                .flatten()
                .unwrap_or_default();
            let issue_kind = if matches!(
                daemon.entry_is_regular_file(OsStr::new(TASK_REGISTRY_FILE_NAME)),
                Ok(Some(false))
            ) {
                "registry_unsafe"
            } else {
                "registry_unreadable"
            };
            push_issue(
                issues,
                issue_kind,
                &path,
                format!("failed to read bounded task registry: {source}"),
            );
            RegistrySnapshot {
                registry: TaskRegistry::default(),
                record_values: BTreeMap::new(),
                revision: None,
                checkpoint_generation: None,
                reliable: false,
                file_bytes,
                allocated_bytes,
                wal_file_bytes: fallback_wal_file_bytes,
                wal_allocated_bytes: fallback_wal_allocated_bytes,
            }
        }
    };

    let mut active = match read_active_task(root, state, issues) {
        Ok(active) => active,
        Err(error) => {
            if let Some(lock) = lock {
                let _ = lock.finish();
            }
            return Err(error);
        }
    };
    reconcile_active_task_authority(root, &snapshot, &mut active, issues);

    if let Some(lock) = lock {
        if let Err(source) = lock.finish() {
            let detail = match source {
                AnchoredFileLockFinishError::Attachment(source) => {
                    format!(
                        "task-registry lock authority changed during the combined registry/active read: {source}"
                    )
                }
                AnchoredFileLockFinishError::Unlock(source) => {
                    format!("failed to release shared task-registry lock: {source}")
                }
            };
            snapshot.reliable = false;
            active.task_id = None;
            active.reliable = false;
            push_issue(issues, "registry_unreadable", &lock_path, detail);
        }
    }
    Ok((snapshot, active))
}

#[cfg(unix)]
fn empty_registry_snapshot(reliable: bool) -> RegistrySnapshot {
    RegistrySnapshot {
        registry: TaskRegistry::default(),
        record_values: BTreeMap::new(),
        revision: None,
        checkpoint_generation: None,
        reliable,
        file_bytes: 0,
        allocated_bytes: 0,
        wal_file_bytes: 0,
        wal_allocated_bytes: 0,
    }
}

#[cfg(unix)]
fn unreliable_authority_snapshots() -> (RegistrySnapshot, ActiveTaskSnapshot) {
    (
        empty_registry_snapshot(false),
        ActiveTaskSnapshot {
            task_id: None,
            reliable: false,
        },
    )
}

#[cfg(unix)]
fn reconcile_active_task_authority(
    root: &Path,
    registry: &RegistrySnapshot,
    active: &mut ActiveTaskSnapshot,
    issues: &mut Vec<TaskStoreIssue>,
) {
    let Some(task_id) = active.task_id.as_deref() else {
        return;
    };
    let admitted = registry.reliable
        && active.reliable
        && registry
            .registry
            .tasks
            .get(task_id)
            .is_some_and(|record| record.task_id == task_id);
    if admitted {
        return;
    }
    if registry.reliable && active.reliable {
        let path = active_task_path(root);
        push_issue(
            issues,
            "active_task_registry_inconsistent",
            &path,
            format!(
                "active-task identifier {task_id:?} is absent from the same locked task registry"
            ),
        );
    }
    active.task_id = None;
    active.reliable = false;
}

#[cfg(not(unix))]
fn read_registry(root: &Path, issues: &mut Vec<TaskStoreIssue>) -> Result<RegistrySnapshot> {
    let path = task_registry_path(root);
    let wal_metadata = fs::symlink_metadata(crate::storage::registry_delta_wal_path(root)).ok();
    let wal_file_bytes = wal_metadata.as_ref().map_or(0, fs::Metadata::len);
    let wal_allocated_bytes = wal_metadata.as_ref().map_or(0, filesystem_allocated_bytes);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RegistrySnapshot {
                registry: TaskRegistry::default(),
                record_values: BTreeMap::new(),
                revision: None,
                checkpoint_generation: None,
                reliable: true,
                file_bytes: 0,
                allocated_bytes: 0,
                wal_file_bytes,
                wal_allocated_bytes,
            });
        }
        Err(source) => {
            push_issue(
                issues,
                "registry_unreadable",
                &path,
                format!("failed to inspect task registry: {source}"),
            );
            return Ok(RegistrySnapshot {
                registry: TaskRegistry::default(),
                record_values: BTreeMap::new(),
                revision: None,
                checkpoint_generation: None,
                reliable: false,
                file_bytes: 0,
                allocated_bytes: 0,
                wal_file_bytes,
                wal_allocated_bytes,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        push_issue(
            issues,
            "registry_unsafe",
            &path,
            "task registry is not a regular file".to_string(),
        );
        return Ok(RegistrySnapshot {
            registry: TaskRegistry::default(),
            record_values: BTreeMap::new(),
            revision: None,
            checkpoint_generation: None,
            reliable: false,
            file_bytes: metadata.len(),
            allocated_bytes: filesystem_allocated_bytes(&metadata),
            wal_file_bytes,
            wal_allocated_bytes,
        });
    }
    if metadata.len() > MAX_TASK_REGISTRY_BYTES as u64 {
        push_issue(
            issues,
            "registry_unreadable",
            &path,
            format!(
                "task registry exceeds the supported {MAX_TASK_REGISTRY_BYTES}-byte read bound"
            ),
        );
        return Ok(RegistrySnapshot {
            registry: TaskRegistry::default(),
            record_values: BTreeMap::new(),
            revision: None,
            checkpoint_generation: None,
            reliable: false,
            file_bytes: metadata.len(),
            allocated_bytes: filesystem_allocated_bytes(&metadata),
            wal_file_bytes,
            wal_allocated_bytes,
        });
    }
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(source) => {
            push_issue(
                issues,
                "registry_unreadable",
                &path,
                format!("failed to read task registry: {source}"),
            );
            return Ok(RegistrySnapshot {
                registry: TaskRegistry::default(),
                record_values: BTreeMap::new(),
                revision: None,
                checkpoint_generation: None,
                reliable: false,
                file_bytes: metadata.len(),
                allocated_bytes: filesystem_allocated_bytes(&metadata),
                wal_file_bytes,
                wal_allocated_bytes,
            });
        }
    };
    match decode_registry_with_raw_values(&raw) {
        Ok((registry, record_values)) => Ok(RegistrySnapshot {
            registry,
            record_values,
            revision: None,
            checkpoint_generation: None,
            reliable: true,
            file_bytes: raw.len() as u64,
            allocated_bytes: filesystem_allocated_bytes(&metadata),
            wal_file_bytes,
            wal_allocated_bytes,
        }),
        Err(source) => {
            push_issue(
                issues,
                "registry_corrupt",
                &path,
                format!("failed to decode task registry: {source}"),
            );
            Ok(RegistrySnapshot {
                registry: TaskRegistry::default(),
                record_values: BTreeMap::new(),
                revision: None,
                checkpoint_generation: None,
                reliable: false,
                file_bytes: raw.len() as u64,
                allocated_bytes: filesystem_allocated_bytes(&metadata),
                wal_file_bytes,
                wal_allocated_bytes,
            })
        }
    }
}

fn decode_registry_with_raw_values(
    raw: &[u8],
) -> std::result::Result<(TaskRegistry, BTreeMap<String, serde_json::Value>), String> {
    let mut value = crate::storage::decode_json_value_without_duplicate_keys(
        raw,
        AuthorityJsonProfile::TaskRegistry,
    )
    .map_err(|source| source.to_string())?;
    if let Some(message) = crate::storage::task_registry_value_shape_error(&value) {
        return Err(message.to_string());
    }
    let record_values = value
        .get_mut("tasks")
        .and_then(serde_json::Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    drop(value);
    let registry: TaskRegistry =
        serde_json::from_slice(raw).map_err(|source| source.to_string())?;
    if let Some(message) = crate::storage::task_registry_shape_error(&registry) {
        return Err(message);
    }
    Ok((registry, record_values))
}

#[cfg(unix)]
fn read_active_task(
    root: &Path,
    state: &CapabilityDir,
    issues: &mut Vec<TaskStoreIssue>,
) -> Result<ActiveTaskSnapshot> {
    let path = active_task_path(root);
    let agent = match state.open_dir(OsStr::new("agent")) {
        Ok(agent) => agent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: true,
            });
        }
        Err(source) => {
            push_issue(
                issues,
                "active_task_unreadable",
                &path,
                format!("failed to open active-task capability: {source}"),
            );
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            });
        }
    };
    let name = path.file_name().ok_or_else(|| {
        DaemonCoreError::io(
            "failed to resolve active-task file name",
            &path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "active-task path has no file name",
            ),
        )
    })?;

    #[cfg(test)]
    INJECT_ACTIVE_TASK_BEFORE_CAPABILITY_READ.with(|configured| {
        if let Some(observer) = configured.borrow_mut().take() {
            observer();
        }
    });

    let raw = match agent.read_file_limited(name, MAX_ACTIVE_TASK_RECORD_BYTES) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: true,
            });
        }
        Err(source) => {
            push_issue(
                issues,
                "active_task_unreadable",
                &path,
                format!("failed to read bounded active-task record: {source}"),
            );
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            });
        }
    };
    match crate::storage::decode_active_task_record(&path, &raw) {
        Ok(record) => Ok(active_task_snapshot_from_record(record, &path, issues)),
        Err(source) => {
            push_issue(
                issues,
                "active_task_corrupt",
                &path,
                format!("failed to decode active-task record: {source}"),
            );
            Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            })
        }
    }
}

#[cfg(not(unix))]
fn read_active_task(root: &Path, issues: &mut Vec<TaskStoreIssue>) -> Result<ActiveTaskSnapshot> {
    let path = active_task_path(root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: true,
            });
        }
        Err(source) => {
            push_issue(
                issues,
                "active_task_unreadable",
                &path,
                format!("failed to inspect active-task record: {source}"),
            );
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        push_issue(
            issues,
            "active_task_unsafe",
            &path,
            "active-task record is not a regular file".to_string(),
        );
        return Ok(ActiveTaskSnapshot {
            task_id: None,
            reliable: false,
        });
    }
    if metadata.len() > MAX_ACTIVE_TASK_RECORD_BYTES as u64 {
        push_issue(
            issues,
            "active_task_unreadable",
            &path,
            format!(
                "active-task record exceeds the supported {MAX_ACTIVE_TASK_RECORD_BYTES}-byte read bound"
            ),
        );
        return Ok(ActiveTaskSnapshot {
            task_id: None,
            reliable: false,
        });
    }
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(source) => {
            push_issue(
                issues,
                "active_task_unreadable",
                &path,
                format!("failed to read active-task record: {source}"),
            );
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            });
        }
    };
    let record = match crate::storage::decode_active_task_record(&path, &raw) {
        Ok(record) => record,
        Err(source) => {
            push_issue(
                issues,
                "active_task_corrupt",
                &path,
                format!("failed to decode active-task record: {source}"),
            );
            return Ok(ActiveTaskSnapshot {
                task_id: None,
                reliable: false,
            });
        }
    };
    Ok(active_task_snapshot_from_record(record, &path, issues))
}

fn active_task_snapshot_from_record(
    record: ActiveTaskRecord,
    path: &Path,
    issues: &mut Vec<TaskStoreIssue>,
) -> ActiveTaskSnapshot {
    if let Some(message) = crate::storage::task_identifier_shape_error(&record.task_id) {
        push_issue(
            issues,
            "active_task_corrupt",
            path,
            format!("active-task record has an unsupported task identifier: {message}"),
        );
        return ActiveTaskSnapshot {
            task_id: None,
            reliable: false,
        };
    }
    ActiveTaskSnapshot {
        // Whitespace is significant in supported task identifiers and in the
        // storage-key derivation used by writers. Retain the exact validated
        // persisted value for protection.
        task_id: Some(record.task_id),
        reliable: true,
    }
}

fn add_registry_candidates(
    root: &Path,
    registry: &RegistrySnapshot,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
) -> Result<()> {
    let registry_path = task_registry_path(root);
    for (task_id, record) in &registry.registry.tasks {
        let storage_key = storage_key_for_task(root, task_id);
        let record_value = registry.record_values.get(task_id).ok_or_else(|| {
            DaemonCoreError::io(
                "failed to locate raw task registry record in",
                &registry_path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("raw registry value is missing task {task_id}"),
                ),
            )
        })?;
        let record_bytes = serde_json::to_vec(record_value).map_err(|source| {
            DaemonCoreError::json(
                "failed to measure task registry record for",
                &registry_path,
                source,
            )
        })?;
        let candidate_key = CandidateKey::managed(storage_key.clone());
        let candidate = candidates
            .entry(candidate_key)
            .or_insert_with(|| Candidate::new(storage_key));
        candidate.task_ids.push(task_id.clone());
        candidate
            .record_values
            .insert(task_id.clone(), record_value.clone());
        candidate.registry_revision = registry.revision;
        candidate.registry_checkpoint_generation = registry.checkpoint_generation;
        candidate.record_logical_bytes = candidate
            .record_logical_bytes
            .saturating_add(record_bytes.len() as u64);
        candidate.update_timestamp(latest_record_timestamp(record));
    }
    Ok(())
}

fn add_artifact_candidates(
    root: &Path,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
    state_identity: Option<FileIdentity>,
) -> Result<()> {
    let artifact_root = task_artifacts_dir(root);
    let entries = match read_managed_root(&artifact_root, issues, "artifact_root") {
        Some(entries) => entries,
        None => return Ok(()),
    };
    let mut entries_seen = 0_usize;
    for entry in entries {
        consume_managed_root_entry(
            &mut entries_seen,
            scan_limits.max_entries_per_managed_root,
            &artifact_root,
        )?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                push_issue(
                    issues,
                    "artifact_entry_unreadable",
                    &artifact_root,
                    format!("failed to enumerate task artifact: {source}"),
                );
                continue;
            }
        };
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(storage_key) = file_name.to_str().map(str::to_string) else {
            push_issue(
                issues,
                "artifact_name_invalid",
                &path,
                "non-UTF-8 task artifact names are protected".to_string(),
            );
            let candidate_key = CandidateKey::opaque(OpaqueNamespace::Artifact, &file_name);
            let storage_key = candidate_key.report_storage_key();
            let scan = scan_path_with_limits_from_parent(
                &path,
                issues,
                "artifact_entry_unreadable",
                scan_limits,
                state_identity,
            )?;
            let candidate = candidates
                .entry(candidate_key)
                .or_insert_with(|| Candidate::new(storage_key));
            candidate.update_timestamp(scan.latest_timestamp_unix);
            candidate.safe = false;
            candidate.artifact = Some(ManagedComponent { path, scan });
            continue;
        };
        if !crate::storage::task_storage_key_is_portable(&storage_key) {
            push_issue(
                issues,
                "artifact_name_invalid",
                &path,
                "nonportable task artifact names are opaque and protected".to_string(),
            );
        }
        let scan = scan_path_with_limits_from_parent(
            &path,
            issues,
            "artifact_entry_unreadable",
            scan_limits,
            state_identity,
        )?;
        let expected_directory = fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        let candidate_key = CandidateKey::managed(storage_key.clone());
        let candidate = candidates
            .entry(candidate_key)
            .or_insert_with(|| Candidate::new(storage_key));
        candidate.update_timestamp(scan.latest_timestamp_unix);
        candidate.safe &= scan.safe && expected_directory;
        candidate.artifact = Some(ManagedComponent { path, scan });
    }
    Ok(())
}

fn add_event_candidates(
    root: &Path,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
    state_identity: Option<FileIdentity>,
) -> Result<()> {
    let event_root = task_events_dir(root);
    let entries = match read_managed_root(&event_root, issues, "event_root") {
        Some(entries) => entries,
        None => return Ok(()),
    };
    let mut entries_seen = 0_usize;
    for entry in entries {
        consume_managed_root_entry(
            &mut entries_seen,
            scan_limits.max_entries_per_managed_root,
            &event_root,
        )?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                push_issue(
                    issues,
                    "event_entry_unreadable",
                    &event_root,
                    format!("failed to enumerate task event: {source}"),
                );
                continue;
            }
        };
        let path = entry.path();
        let file_name = entry.file_name();
        let recognized_storage_key = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(TASK_EVENT_LOG_SUFFIX))
            .filter(|key| storage_key_is_safe(key));
        let recognized = recognized_storage_key.is_some();
        let candidate_key = recognized_storage_key.map_or_else(
            || CandidateKey::opaque(OpaqueNamespace::Event, &file_name),
            |storage_key| CandidateKey::managed(storage_key.to_string()),
        );
        let storage_key = candidate_key.report_storage_key();
        if !recognized {
            push_issue(
                issues,
                "event_name_invalid",
                &path,
                "unrecognized task-event entry is protected".to_string(),
            );
        }
        let scan = scan_path_with_limits_from_parent(
            &path,
            issues,
            "event_entry_unreadable",
            scan_limits,
            state_identity,
        )?;
        let expected_file = fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        let candidate = candidates
            .entry(candidate_key)
            .or_insert_with(|| Candidate::new(storage_key));
        candidate.update_timestamp(scan.latest_timestamp_unix);
        candidate.safe &= scan.safe && expected_file && recognized;
        candidate.event = Some(ManagedComponent { path, scan });
    }
    Ok(())
}

#[cfg(unix)]
fn add_artifact_candidates_anchored(
    artifact_root: Option<&CapabilityDir>,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
) -> Result<()> {
    let Some(artifact_root) = artifact_root else {
        return Ok(());
    };
    let entries = artifact_root
        .entries_bounded(scan_limits.max_entries_per_managed_root)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate retained task artifact root",
                artifact_root.display_path(),
                source,
            )
        })?;
    for file_name in entries {
        let path = artifact_root.display_path().join(&file_name);
        let scan = scan_capability_entry_with_limits(
            artifact_root,
            &file_name,
            issues,
            "artifact_entry_unreadable",
            scan_limits,
        )?;
        let Some(storage_key) = file_name.to_str().map(str::to_string) else {
            push_issue(
                issues,
                "artifact_name_invalid",
                &path,
                "non-UTF-8 task artifact names are protected".to_string(),
            );
            let candidate_key = CandidateKey::opaque(OpaqueNamespace::Artifact, &file_name);
            let storage_key = candidate_key.report_storage_key();
            let candidate = candidates
                .entry(candidate_key)
                .or_insert_with(|| Candidate::new(storage_key));
            candidate.update_timestamp(scan.latest_timestamp_unix);
            candidate.safe = false;
            candidate.artifact = Some(ManagedComponent { path, scan });
            continue;
        };
        if !crate::storage::task_storage_key_is_portable(&storage_key) {
            push_issue(
                issues,
                "artifact_name_invalid",
                &path,
                "nonportable task artifact names are opaque and protected".to_string(),
            );
        }
        let expected_directory = artifact_root
            .entry_metadata(&file_name)
            .map_err(|source| {
                DaemonCoreError::io("failed to revalidate task artifact entry", &path, source)
            })?
            .is_some_and(|metadata| metadata.kind == CapabilityEntryKind::Directory);
        let candidate_key = CandidateKey::managed(storage_key.clone());
        let candidate = candidates
            .entry(candidate_key)
            .or_insert_with(|| Candidate::new(storage_key));
        candidate.update_timestamp(scan.latest_timestamp_unix);
        candidate.safe &= scan.safe && expected_directory;
        candidate.artifact = Some(ManagedComponent { path, scan });
    }
    Ok(())
}

#[cfg(unix)]
fn add_event_candidates_anchored(
    event_root: Option<&CapabilityDir>,
    candidates: &mut BTreeMap<CandidateKey, Candidate>,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
) -> Result<()> {
    let Some(event_root) = event_root else {
        return Ok(());
    };
    let entries = event_root
        .entries_bounded(scan_limits.max_entries_per_managed_root)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate retained task event root",
                event_root.display_path(),
                source,
            )
        })?;
    for file_name in entries {
        let path = event_root.display_path().join(&file_name);
        let recognized_storage_key = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(TASK_EVENT_LOG_SUFFIX))
            .filter(|key| storage_key_is_safe(key));
        let recognized = recognized_storage_key.is_some();
        let candidate_key = recognized_storage_key.map_or_else(
            || CandidateKey::opaque(OpaqueNamespace::Event, &file_name),
            |storage_key| CandidateKey::managed(storage_key.to_string()),
        );
        let storage_key = candidate_key.report_storage_key();
        if !recognized {
            push_issue(
                issues,
                "event_name_invalid",
                &path,
                "unrecognized task-event entry is protected".to_string(),
            );
        }
        let scan = scan_capability_entry_with_limits(
            event_root,
            &file_name,
            issues,
            "event_entry_unreadable",
            scan_limits,
        )?;
        let expected_file = event_root
            .entry_metadata(&file_name)
            .map_err(|source| {
                DaemonCoreError::io("failed to revalidate task event entry", &path, source)
            })?
            .is_some_and(|metadata| metadata.kind == CapabilityEntryKind::RegularFile);
        let candidate = candidates
            .entry(candidate_key)
            .or_insert_with(|| Candidate::new(storage_key));
        candidate.update_timestamp(scan.latest_timestamp_unix);
        candidate.safe &= scan.safe && expected_file && recognized;
        candidate.event = Some(ManagedComponent { path, scan });
    }
    Ok(())
}

#[cfg(unix)]
fn load_targeted_candidate_with_lease(
    snapshot: &StoreSnapshot,
    storage_key: &str,
    lease: &TaskStoreLease,
) -> Result<TargetedCandidateSnapshot> {
    if lease.role() != LeaseRole::Retention
        || !lease.matches_root_argument(&snapshot.workspace_root)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: snapshot.workspace_root.clone(),
        });
    }
    let state = lease.state_capability()?;
    let daemon = lease.daemon_capability()?;
    load_targeted_candidate_anchored(snapshot, storage_key, &state, &daemon)
}

#[cfg(unix)]
fn load_targeted_candidate_anchored(
    snapshot: &StoreSnapshot,
    storage_key: &str,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
) -> Result<TargetedCandidateSnapshot> {
    if snapshot.state_root_identity != Some(state.identity()) {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: snapshot.state_root.clone(),
        });
    }
    ensure_same_filesystem(
        state.identity(),
        daemon.identity(),
        daemon.display_path(),
        "daemon state for retention revalidation is on another filesystem",
    )?;

    let mut issues = Vec::new();
    if !storage_key_is_safe(storage_key) {
        push_issue(
            &mut issues,
            "storage_key_invalid",
            &snapshot.state_root,
            "retention candidate storage key is not one path component".to_string(),
        );
        return Ok(TargetedCandidateSnapshot {
            candidate: None,
            active_storage_keys: BTreeSet::new(),
            reliable: false,
            issues,
        });
    }

    let (registry_snapshot, active_snapshot) =
        read_authority_snapshots_from_daemon(&snapshot.workspace_root, state, daemon, &mut issues)?;
    let (artifact_root, artifact_reliable) =
        open_optional_managed_directory(state, OsStr::new("task"), &mut issues, "artifact_root");
    let (event_root, event_reliable) =
        open_optional_managed_directory(daemon, OsStr::new("tasks"), &mut issues, "event_root");
    let (_agent_root, agent_reliable) =
        open_optional_managed_directory(state, OsStr::new("agent"), &mut issues, "agent_root");
    let managed_layout_reliable = artifact_reliable && event_reliable && agent_reliable;

    let mut candidate = Candidate::new(storage_key.to_string());
    let mut present = false;
    for (task_id, record) in &registry_snapshot.registry.tasks {
        if storage_key_for_task(&snapshot.workspace_root, task_id) != storage_key {
            continue;
        }
        let record_value = registry_snapshot
            .record_values
            .get(task_id)
            .ok_or_else(|| {
                DaemonCoreError::io(
                    "failed to locate raw task registry record in",
                    task_registry_path(&snapshot.workspace_root),
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("raw registry value is missing task {task_id}"),
                    ),
                )
            })?;
        let record_bytes = serde_json::to_vec(record_value).map_err(|source| {
            DaemonCoreError::json(
                "failed to verify task registry record in",
                task_registry_path(&snapshot.workspace_root),
                source,
            )
        })?;
        candidate.task_ids.push(task_id.clone());
        candidate
            .record_values
            .insert(task_id.clone(), record_value.clone());
        candidate.registry_revision = registry_snapshot.revision;
        candidate.registry_checkpoint_generation = registry_snapshot.checkpoint_generation;
        candidate.record_logical_bytes = candidate
            .record_logical_bytes
            .saturating_add(record_bytes.len() as u64);
        candidate.update_timestamp(latest_record_timestamp(record));
        present = true;
    }

    if let Some(artifact_root) = artifact_root.as_ref() {
        let artifact_name = OsStr::new(storage_key);
        let artifact_path = artifact_root.display_path().join(artifact_name);
        if let Some(metadata) = artifact_root
            .entry_metadata(artifact_name)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect retained task artifact candidate",
                    &artifact_path,
                    source,
                )
            })?
        {
            let scan = scan_capability_entry_with_limits(
                artifact_root,
                artifact_name,
                &mut issues,
                "artifact_entry_unreadable",
                ScanLimits::DEFAULT,
            )?;
            candidate.update_timestamp(scan.latest_timestamp_unix);
            candidate.safe &= scan.safe && metadata.kind == CapabilityEntryKind::Directory;
            candidate.artifact = Some(ManagedComponent {
                path: artifact_path,
                scan,
            });
            present = true;
        }
    }

    let event_name = OsString::from(format!("{storage_key}{TASK_EVENT_LOG_SUFFIX}"));
    if let Some(event_root) = event_root.as_ref() {
        let event_path = event_root.display_path().join(&event_name);
        if let Some(metadata) = event_root.entry_metadata(&event_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect retained task event candidate",
                &event_path,
                source,
            )
        })? {
            let scan = scan_capability_entry_with_limits(
                event_root,
                &event_name,
                &mut issues,
                "event_entry_unreadable",
                ScanLimits::DEFAULT,
            )?;
            candidate.update_timestamp(scan.latest_timestamp_unix);
            candidate.safe &= scan.safe && metadata.kind == CapabilityEntryKind::RegularFile;
            candidate.event = Some(ManagedComponent {
                path: event_path,
                scan,
            });
            present = true;
        }
    }

    let targeted_alias_conflict = targeted_candidate_has_namespace_alias_anchored(
        artifact_root.as_ref(),
        event_root.as_ref(),
        storage_key,
        &candidate,
        &mut issues,
    )?;
    if targeted_alias_conflict {
        candidate
            .protected_reasons
            .insert("candidate has a filesystem-spelling or physical-identity alias".to_string());
    }

    let active_storage_keys = active_storage_keys(
        &snapshot.workspace_root,
        &registry_snapshot.registry,
        active_snapshot.task_id.as_deref(),
    );
    let reliable = registry_snapshot.reliable
        && active_snapshot.reliable
        && managed_layout_reliable
        && !targeted_alias_conflict;
    if !reliable {
        candidate.protected_reasons.insert(
            "active-task, registry, or managed layout state is corrupt, unreadable, or unsafe"
                .to_string(),
        );
    }
    if storage_key_is_active(&active_storage_keys, storage_key) {
        candidate
            .protected_reasons
            .insert("task is active".to_string());
    }
    if candidate.task_ids.len() > 1 {
        candidate
            .protected_reasons
            .insert("multiple task identifiers map to the same storage key".to_string());
    }
    if !candidate.safe {
        candidate
            .protected_reasons
            .insert("candidate contains an unsafe or unreadable entry".to_string());
    }
    issues.sort_by(|left, right| {
        (&left.kind, &left.path, &left.message).cmp(&(&right.kind, &right.path, &right.message))
    });
    issues.dedup();

    Ok(TargetedCandidateSnapshot {
        candidate: present.then_some(candidate),
        active_storage_keys,
        reliable,
        issues,
    })
}

#[cfg(unix)]
fn targeted_candidate_has_namespace_alias_anchored(
    artifact_root: Option<&CapabilityDir>,
    event_root: Option<&CapabilityDir>,
    storage_key: &str,
    candidate: &Candidate,
    issues: &mut Vec<TaskStoreIssue>,
) -> Result<bool> {
    let expected_event_name = OsString::from(format!("{storage_key}{TASK_EVENT_LOG_SUFFIX}"));
    let target_identities = [candidate.artifact.as_ref(), candidate.event.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(|component| component.scan.physical_identities.iter().copied())
        .collect::<BTreeSet<_>>();
    let expected_artifact_name = OsStr::new(storage_key);
    let mut conflict = false;

    for (managed_root, expected_name, issue_kind) in [
        (
            artifact_root,
            expected_artifact_name,
            "artifact_alias_unreadable",
        ),
        (
            event_root,
            expected_event_name.as_os_str(),
            "event_alias_unreadable",
        ),
    ] {
        let Some(managed_root) = managed_root else {
            continue;
        };
        let entries =
            match managed_root.entries_bounded(ScanLimits::DEFAULT.max_entries_per_managed_root) {
                Ok(entries) => entries,
                Err(source) => {
                    push_issue(
                        issues,
                        issue_kind,
                        managed_root.display_path(),
                        format!("failed to enumerate candidate aliases: {source}"),
                    );
                    conflict = true;
                    continue;
                }
            };
        let expected_alias =
            crate::storage::task_storage_key_alias_class(&expected_name.to_string_lossy());
        for actual_name in entries {
            if actual_name == expected_name {
                continue;
            }
            let path = managed_root.display_path().join(&actual_name);
            if actual_name.to_str().is_some_and(|actual| {
                crate::storage::task_storage_key_alias_class(actual) == expected_alias
            }) {
                push_issue(
                    issues,
                    "storage_key_alias",
                    &path,
                    format!("managed entry spelling aliases retention candidate {expected_name:?}"),
                );
                conflict = true;
            }
            match managed_root.entry_metadata(&actual_name) {
                Ok(Some(metadata)) if target_identities.contains(&metadata.identity) => {
                    push_issue(
                        issues,
                        "physical_entry_alias",
                        &path,
                        "managed entry shares a physical identity with another candidate path"
                            .to_string(),
                    );
                    conflict = true;
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(source) => {
                    push_issue(
                        issues,
                        issue_kind,
                        &path,
                        format!("failed to inspect candidate alias: {source}"),
                    );
                    conflict = true;
                }
            }
        }
    }

    Ok(conflict)
}

fn managed_layout_is_reliable(
    workspace_root: &Path,
    state_root: &Path,
    state_identity: Option<FileIdentity>,
    issues: &mut Vec<TaskStoreIssue>,
) -> bool {
    let daemon_root = daemon_dir(workspace_root);
    let agent_root = agent_runtime_dir(workspace_root);
    let mut reliable = true;
    for (path, issue_kind) in [
        (task_artifacts_dir(workspace_root), "artifact_root"),
        (daemon_root, "daemon_root"),
        (task_events_dir(workspace_root), "event_root"),
        (agent_root, "agent_root"),
    ] {
        reliable &= optional_state_directory_is_reliable(
            &path,
            state_root,
            state_identity,
            issues,
            issue_kind,
        );
    }
    reliable
}

#[cfg(unix)]
fn open_optional_managed_directory(
    parent: &CapabilityDir,
    name: &OsStr,
    issues: &mut Vec<TaskStoreIssue>,
    issue_kind: &str,
) -> (Option<CapabilityDir>, bool) {
    match parent.open_dir(name) {
        Ok(directory) => (Some(directory), true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => (None, true),
        Err(source) => {
            push_issue(
                issues,
                issue_kind,
                &parent.display_path().join(name),
                format!("managed directory failed descriptor authentication: {source}"),
            );
            (None, false)
        }
    }
}

#[cfg(unix)]
fn inspect_quarantine_layout_anchored(
    state: &CapabilityDir,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
) -> Result<(ScanSummary, u64)> {
    let (quarantine, reliable) = open_optional_managed_directory(
        state,
        OsStr::new(QUARANTINE_DIR_NAME),
        issues,
        "retention_quarantine_unreadable",
    );
    let Some(quarantine) = quarantine else {
        return Ok((
            ScanSummary {
                safe: reliable,
                ..ScanSummary::default()
            },
            0,
        ));
    };
    let scan = scan_capability_directory_with_limits(
        &quarantine,
        issues,
        "retention_quarantine_unreadable",
        scan_limits,
    )?;
    let groups = quarantine
        .entries_bounded(scan_limits.max_entries_per_managed_root)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate retained retention quarantine",
                quarantine.display_path(),
                source,
            )
        })?
        .len() as u64;
    Ok((scan, groups))
}

fn optional_state_directory_is_reliable(
    path: &Path,
    state_root: &Path,
    state_identity: Option<FileIdentity>,
    issues: &mut Vec<TaskStoreIssue>,
    issue_kind: &str,
) -> bool {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return true,
        Err(source) => {
            push_issue(
                issues,
                issue_kind,
                path,
                format!("failed to inspect managed directory: {source}"),
            );
            return false;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        push_issue(
            issues,
            issue_kind,
            path,
            "managed directory is not a real directory".to_string(),
        );
        return false;
    }
    if !same_device(state_identity, Some(file_identity(&metadata))) {
        push_issue(
            issues,
            issue_kind,
            path,
            "managed directory is on another filesystem and is not eligible for retention"
                .to_string(),
        );
        return false;
    }
    match fs::canonicalize(path) {
        Ok(canonical) if canonical.starts_with(state_root) => true,
        Ok(canonical) => {
            push_issue(
                issues,
                issue_kind,
                &canonical,
                "managed directory resolves outside Packet28 state".to_string(),
            );
            false
        }
        Err(source) => {
            push_issue(
                issues,
                issue_kind,
                path,
                format!("failed to resolve managed directory: {source}"),
            );
            false
        }
    }
}

fn storage_key_is_safe(storage_key: &str) -> bool {
    let mut components = Path::new(storage_key).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn read_managed_root(
    root: &Path,
    issues: &mut Vec<TaskStoreIssue>,
    issue_kind: &str,
) -> Option<fs::ReadDir> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return None,
        Err(source) => {
            push_issue(
                issues,
                issue_kind,
                root,
                format!("failed to inspect managed root: {source}"),
            );
            return None;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        push_issue(
            issues,
            issue_kind,
            root,
            "managed root is not a real directory".to_string(),
        );
        return None;
    }
    match fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(source) => {
            push_issue(
                issues,
                issue_kind,
                root,
                format!("failed to enumerate managed root: {source}"),
            );
            None
        }
    }
}

fn consume_managed_root_entry(
    entries_seen: &mut usize,
    max_entries: usize,
    root: &Path,
) -> Result<()> {
    if *entries_seen < max_entries {
        *entries_seen += 1;
        return Ok(());
    }
    Err(retention_resource_limit_error(
        "task-store managed-root enumeration exceeded the supported entry bound",
        root,
        format!("maximum supported entries per managed root is {max_entries}"),
    ))
}

fn inspect_quarantine_layout(
    state_root: &Path,
    state_identity: Option<FileIdentity>,
    issues: &mut Vec<TaskStoreIssue>,
    scan_limits: ScanLimits,
) -> Result<(ScanSummary, u64)> {
    let quarantine_root = state_root.join(QUARANTINE_DIR_NAME);
    let scan = scan_path_with_limits_from_parent(
        &quarantine_root,
        issues,
        "retention_quarantine_unreadable",
        scan_limits,
        state_identity,
    )?;
    let Some(entries) =
        read_managed_root(&quarantine_root, issues, "retention_quarantine_unreadable")
    else {
        return Ok((scan, 0));
    };
    let mut groups = 0_u64;
    let mut entries_seen = 0_usize;
    for entry in entries {
        consume_managed_root_entry(
            &mut entries_seen,
            scan_limits.max_entries_per_managed_root,
            &quarantine_root,
        )?;
        match entry {
            Ok(entry) => {
                groups = groups.saturating_add(1);
                push_issue(
                    issues,
                    "retention_quarantine_pending",
                    &entry.path(),
                    "retention quarantine group is protected until recovery completes".to_string(),
                );
            }
            Err(source) => push_issue(
                issues,
                "retention_quarantine_unreadable",
                &quarantine_root,
                format!("failed to enumerate retention quarantine group: {source}"),
            ),
        }
    }
    Ok((scan, groups))
}

fn active_storage_keys(
    root: &Path,
    registry: &TaskRegistry,
    active_task_id: Option<&str>,
) -> BTreeSet<String> {
    let mut active = registry
        .tasks
        .iter()
        .filter(|(_, record)| task_record_is_active(record))
        .map(|(task_id, _)| storage_key_for_task(root, task_id))
        .collect::<BTreeSet<_>>();
    if let Some(task_id) = active_task_id {
        active.insert(storage_key_for_task(root, task_id));
    }
    // A retained successor can still read its predecessors' immutable artifacts.
    // Protect every referenced namespace until its referring record is removed.
    for record in registry.tasks.values() {
        if let Some(link) = &record.recovered_from {
            active.insert(storage_key_for_task(root, &link.predecessor_task_id));
        }
        for handoff in &record.handoffs {
            if handoff.task_id != record.task_id {
                active.insert(storage_key_for_task(root, &handoff.task_id));
            }
        }
    }
    active
}

fn storage_key_is_active(active_storage_keys: &BTreeSet<String>, storage_key: &str) -> bool {
    let alias_class = crate::storage::task_storage_key_alias_class(storage_key);
    active_storage_keys
        .iter()
        .any(|active| crate::storage::task_storage_key_alias_class(active) == alias_class)
}

fn task_record_is_active(record: &TaskRecord) -> bool {
    if record.lifecycle.is_running()
        || record.lifecycle.is_cancelling()
        || record.lifecycle.has_pending_replan()
    {
        return true;
    }
    match (
        record.latest_agent_started_at_unix,
        record.latest_agent_completed_at_unix,
    ) {
        (Some(started), Some(completed)) => {
            normalize_timestamp_seconds(completed) < normalize_timestamp_seconds(started)
        }
        (Some(_), None) => true,
        _ => false,
    }
}

fn latest_record_timestamp(record: &TaskRecord) -> Option<u64> {
    [
        record.last_started_at_unix,
        record.last_completed_at_unix,
        record.last_replan_at_unix,
        record.last_context_refresh_at_unix,
        record.latest_brief_generated_at_unix,
        record.latest_handoff_generated_at_unix,
        record.latest_agent_started_at_unix,
        record.latest_agent_completed_at_unix,
        record.latest_hook_event_at_unix,
        record.latest_hook_boundary_at_unix,
        record.latest_hook_bootstrap_at_unix,
        record.latest_hook_progress_at_unix,
    ]
    .into_iter()
    .flatten()
    .map(normalize_timestamp_seconds)
    .max()
}

fn normalize_timestamp_seconds(value: u64) -> u64 {
    // Persisted task fields historically mix Unix seconds and milliseconds.
    // This threshold matches the daemon's compatibility normalization: values
    // this large cannot be contemporary Unix seconds.
    if value < 100_000_000_000 {
        value
    } else {
        value / 1_000
    }
}

fn storage_key_for_task(_root: &Path, task_id: &str) -> String {
    if TaskStorageId::try_from(task_id).is_ok() {
        return task_id.to_string();
    }
    // Retention still recognizes legacy on-disk spellings produced by the
    // former lossy public helper. New producers cannot reach this encoder.
    let safe = task_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "task".to_string()
    } else {
        safe
    }
}

fn build_plan(snapshot: &StoreSnapshot, options: RetentionOptions) -> RetentionPlan {
    let mut selected = BTreeMap::<String, PlanItem>::new();
    let mut removable = snapshot
        .candidates
        .values()
        .filter(|candidate| candidate.protected_reasons.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    removable.sort_by(compare_candidate_age);

    if let Some(max_age_seconds) = options.max_age_seconds {
        for candidate in &removable {
            let is_older = candidate.latest_timestamp_unix.is_some_and(|timestamp| {
                snapshot.observed_at_unix.saturating_sub(timestamp) > max_age_seconds
            });
            if is_older {
                selected.insert(
                    candidate.storage_key.clone(),
                    PlanItem {
                        candidate: candidate.clone(),
                        reasons: BTreeSet::from([RetentionReason::AgeLimit]),
                    },
                );
            }
        }
    }

    let mut projected_managed_logical_bytes = snapshot.metrics.managed_task_logical_bytes;
    for item in selected.values() {
        projected_managed_logical_bytes =
            projected_managed_logical_bytes.saturating_sub(item.candidate.logical_bytes());
    }
    if let Some(max_bytes) = options.max_bytes {
        for candidate in &removable {
            if projected_managed_logical_bytes <= max_bytes {
                break;
            }
            if let Some(item) = selected.get_mut(&candidate.storage_key) {
                item.reasons.insert(RetentionReason::SizeLimit);
                continue;
            }
            let logical_bytes = candidate.logical_bytes();
            if logical_bytes == 0 {
                continue;
            }
            selected.insert(
                candidate.storage_key.clone(),
                PlanItem {
                    candidate: candidate.clone(),
                    reasons: BTreeSet::from([RetentionReason::SizeLimit]),
                },
            );
            projected_managed_logical_bytes =
                projected_managed_logical_bytes.saturating_sub(logical_bytes);
        }
    }

    let mut items = selected.into_values().collect::<Vec<_>>();
    items.sort_by(|left, right| compare_candidate_age(&left.candidate, &right.candidate));
    let actions = items
        .iter()
        .map(|item| RetentionAction {
            storage_key: item.candidate.storage_key.clone(),
            task_ids: item.candidate.task_ids.clone(),
            logical_bytes: item.candidate.logical_bytes(),
            removed_logical_bytes: 0,
            remaining_logical_bytes: item.candidate.logical_bytes(),
            byte_accounting_reliable: true,
            latest_timestamp_unix: item.candidate.latest_timestamp_unix,
            reasons: item.reasons.iter().copied().collect(),
            outcome: RetentionOutcome::WouldRemove,
        })
        .collect();
    let protected_tasks = snapshot
        .candidates
        .values()
        .filter(|candidate| !candidate.protected_reasons.is_empty())
        .count() as u64;
    let protected_logical_bytes = snapshot
        .candidates
        .values()
        .filter(|candidate| !candidate.protected_reasons.is_empty())
        .map(Candidate::logical_bytes)
        .fold(0_u64, u64::saturating_add)
        .saturating_add(snapshot.unattributed_protected_logical_bytes);

    RetentionPlan {
        items,
        actions,
        protected_tasks,
        protected_logical_bytes,
        projected_managed_logical_bytes,
    }
}

fn compare_candidate_age(left: &Candidate, right: &Candidate) -> CmpOrdering {
    match (left.latest_timestamp_unix, right.latest_timestamp_unix) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
    .then_with(|| left.storage_key.cmp(&right.storage_key))
}

#[cfg(unix)]
fn apply_plan(
    snapshot: &mut StoreSnapshot,
    plan: &mut RetentionPlan,
    lease: &TaskStoreLease,
    admission: &TaskRetentionAdmission,
) -> Result<()> {
    if lease.role() != LeaseRole::Retention
        || !lease.matches_root_argument(&snapshot.workspace_root)
        || !admission.authorizes(lease)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: snapshot.workspace_root.clone(),
        });
    }
    let daemon = lease.daemon_capability()?;
    for (index, item) in plan.items.iter().enumerate() {
        let readiness = ready_path(&snapshot.workspace_root);
        match daemon.entry_identity(OsStr::new(READY_FILE_NAME)) {
            Ok(None) => {}
            Ok(Some(_)) => {
                push_owned_issue(
                    &mut snapshot.issues,
                    TaskStoreIssue {
                        kind: "candidate_cleanup_failed".to_string(),
                        path: readiness.display().to_string(),
                        message: "daemon readiness appeared during retention cleanup".to_string(),
                    },
                );
                if let Some(action) = plan.actions.get_mut(index) {
                    action.outcome = RetentionOutcome::Failed;
                }
                continue;
            }
            Err(source) => {
                push_owned_issue(
                    &mut snapshot.issues,
                    issue_from_cleanup_error(
                        &item.candidate,
                        &DaemonCoreError::io(
                            "failed to inspect retained daemon readiness marker",
                            &readiness,
                            source,
                        ),
                    ),
                );
                if let Some(action) = plan.actions.get_mut(index) {
                    action.outcome = RetentionOutcome::Failed;
                }
                continue;
            }
        }
        let current = match load_targeted_candidate_with_lease(
            snapshot,
            &item.candidate.storage_key,
            lease,
        ) {
            Ok(current) => current,
            Err(DaemonCoreError::RetentionCandidateChanged { path }) => {
                push_owned_issue(
                    &mut snapshot.issues,
                    TaskStoreIssue {
                        kind: "candidate_changed".to_string(),
                        path: path.display().to_string(),
                        message: "Packet28 state root changed after inspection".to_string(),
                    },
                );
                if let Some(action) = plan.actions.get_mut(index) {
                    action.outcome = RetentionOutcome::Skipped;
                }
                continue;
            }
            Err(error) => {
                push_owned_issue(
                    &mut snapshot.issues,
                    issue_from_cleanup_error(&item.candidate, &error),
                );
                if let Some(action) = plan.actions.get_mut(index) {
                    action.outcome = RetentionOutcome::Failed;
                }
                continue;
            }
        };
        extend_issues(&mut snapshot.issues, current.issues);
        let issue_count_before_outcome = snapshot.issues.len();
        let outcome = match current.candidate.as_ref() {
            Some(candidate)
                if candidate.protected_reasons.is_empty()
                    && candidate_matches(&item.candidate, candidate) =>
            {
                match apply_candidate_with_lease(snapshot, candidate, lease) {
                    Ok(outcome) => outcome,
                    Err(CandidateApplyError {
                        error: DaemonCoreError::RetentionCandidateChanged { path },
                        committed: false,
                        rollback_confirmed: true,
                        ..
                    }) => {
                        push_owned_issue(
                            &mut snapshot.issues,
                            TaskStoreIssue {
                                kind: "candidate_changed".to_string(),
                                path: path.display().to_string(),
                                message: "candidate identity changed during cleanup".to_string(),
                            },
                        );
                        RetentionOutcome::Skipped
                    }
                    Err(error) => {
                        push_owned_issue(
                            &mut snapshot.issues,
                            issue_from_cleanup_error(candidate, &error.error),
                        );
                        if let Some(measurement_error) = &error.measurement_error {
                            push_owned_issue(
                                &mut snapshot.issues,
                                TaskStoreIssue {
                                    kind: "candidate_accounting_failed".to_string(),
                                    path: candidate
                                        .artifact
                                        .as_ref()
                                        .or(candidate.event.as_ref())
                                        .map(|component| component.path.display().to_string())
                                        .unwrap_or_else(|| candidate.storage_key.clone()),
                                    message: measurement_error.clone(),
                                },
                            );
                        }
                        if let Some(action) = plan.actions.get_mut(index) {
                            action.removed_logical_bytes = error.removed_logical_bytes;
                            action.remaining_logical_bytes = error.remaining_logical_bytes;
                            action.byte_accounting_reliable = error.byte_accounting_reliable;
                        }
                        RetentionOutcome::Failed
                    }
                }
            }
            _ => {
                push_owned_issue(
                    &mut snapshot.issues,
                    TaskStoreIssue {
                        kind: "candidate_changed".to_string(),
                        path: item
                            .candidate
                            .artifact
                            .as_ref()
                            .or(item.candidate.event.as_ref())
                            .map(|component| component.path.display().to_string())
                            .unwrap_or_else(|| {
                                task_registry_path(&snapshot.workspace_root)
                                    .display()
                                    .to_string()
                            }),
                        message: "candidate changed or became protected after inspection"
                            .to_string(),
                    },
                );
                RetentionOutcome::Skipped
            }
        };
        if outcome == RetentionOutcome::Skipped
            && snapshot.issues.len() == issue_count_before_outcome
        {
            push_owned_issue(
                &mut snapshot.issues,
                TaskStoreIssue {
                    kind: "candidate_changed".to_string(),
                    path: item
                        .candidate
                        .artifact
                        .as_ref()
                        .or(item.candidate.event.as_ref())
                        .map(|component| component.path.display().to_string())
                        .unwrap_or_else(|| {
                            task_registry_path(&snapshot.workspace_root)
                                .display()
                                .to_string()
                        }),
                    message: "candidate changed or became active during cleanup".to_string(),
                },
            );
        }
        if let Some(action) = plan.actions.get_mut(index) {
            action.outcome = outcome;
            match outcome {
                RetentionOutcome::Removed => {
                    action.removed_logical_bytes = action.logical_bytes;
                    action.remaining_logical_bytes = 0;
                }
                RetentionOutcome::WouldRemove
                | RetentionOutcome::Skipped
                | RetentionOutcome::Failed => {}
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
struct CandidateApplyError {
    error: DaemonCoreError,
    committed: bool,
    rollback_confirmed: bool,
    removed_logical_bytes: u64,
    remaining_logical_bytes: u64,
    byte_accounting_reliable: bool,
    measurement_error: Option<String>,
}

#[cfg(unix)]
impl CandidateApplyError {
    fn before_transaction(candidate: &Candidate, error: DaemonCoreError) -> Self {
        Self {
            error,
            committed: false,
            rollback_confirmed: true,
            removed_logical_bytes: 0,
            remaining_logical_bytes: candidate.logical_bytes(),
            byte_accounting_reliable: true,
            measurement_error: None,
        }
    }

    fn rolled_back(candidate: &Candidate, error: DaemonCoreError) -> Self {
        Self::before_transaction(candidate, error)
    }

    fn rollback_failed(
        candidate: &Candidate,
        group_path: &Path,
        trigger_error: DaemonCoreError,
        rollback_error: DaemonCoreError,
    ) -> Self {
        Self {
            error: DaemonCoreError::io(
                "failed to roll back precommit task retention",
                group_path,
                std::io::Error::other(format!(
                    "{rollback_error}; rollback was required after: {trigger_error}"
                )),
            ),
            committed: false,
            rollback_confirmed: false,
            removed_logical_bytes: 0,
            remaining_logical_bytes: candidate.logical_bytes(),
            byte_accounting_reliable: false,
            measurement_error: Some(
                "precommit rollback failed; remaining bytes conservatively retain the full planned candidate"
                    .to_string(),
            ),
        }
    }

    fn capture_committed(
        transaction: &StagingTransaction,
        candidate: &Candidate,
        error: DaemonCoreError,
    ) -> Self {
        let (remaining_logical_bytes, byte_accounting_reliable, measurement_error) =
            match transaction.remaining_candidate_logical_bytes(candidate) {
                Ok(remaining) => (remaining, true, None),
                Err(measurement_error) => (
                    candidate.logical_bytes(),
                    false,
                    Some(measurement_error.to_string()),
                ),
            };
        Self {
            error,
            committed: true,
            rollback_confirmed: false,
            removed_logical_bytes: candidate
                .logical_bytes()
                .saturating_sub(remaining_logical_bytes),
            remaining_logical_bytes,
            byte_accounting_reliable,
            measurement_error,
        }
    }
}

#[cfg(unix)]
fn issue_from_cleanup_error(candidate: &Candidate, error: &DaemonCoreError) -> TaskStoreIssue {
    let path = candidate
        .artifact
        .as_ref()
        .or(candidate.event.as_ref())
        .map(|component| component.path.display().to_string())
        .unwrap_or_else(|| candidate.storage_key.clone());
    TaskStoreIssue {
        kind: "candidate_cleanup_failed".to_string(),
        path,
        message: error.to_string(),
    }
}

#[cfg(unix)]
fn remove_anchored_registry_records_if_unchanged_with_commit(
    daemon: &CapabilityDir,
    workspace_root: &Path,
    expected_records: &BTreeMap<String, serde_json::Value>,
    expected_revision: Option<crate::storage::RegistryRevision>,
    expected_checkpoint_generation: Option<u64>,
    before_remove: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    if expected_records.is_empty() {
        before_remove()?;
        return Ok(true);
    }
    with_anchored_registry_lock(daemon, workspace_root, || {
        crate::storage::remove_retained_registry_records_under_task_lock(
            workspace_root,
            daemon,
            expected_records,
            expected_revision,
            expected_checkpoint_generation,
            false,
            before_remove,
        )
    })
}

#[cfg(unix)]
fn finish_anchored_committed_registry_removal(
    daemon: &CapabilityDir,
    workspace_root: &Path,
    expected_records: &BTreeMap<String, serde_json::Value>,
    expected_revision: Option<crate::storage::RegistryRevision>,
    expected_checkpoint_generation: Option<u64>,
) -> Result<bool> {
    if expected_records.is_empty() {
        return Ok(true);
    }
    with_anchored_registry_lock(daemon, workspace_root, || {
        crate::storage::remove_retained_registry_records_under_task_lock(
            workspace_root,
            daemon,
            expected_records,
            expected_revision,
            expected_checkpoint_generation,
            true,
            || Ok(()),
        )
    })
}

#[cfg(unix)]
fn with_anchored_registry_lock<T>(
    daemon: &CapabilityDir,
    workspace_root: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock_path = daemon.display_path().join(TASK_REGISTRY_LOCK_FILE_NAME);
    let lock = AnchoredFileLock::acquire(
        daemon,
        OsStr::new(TASK_REGISTRY_LOCK_FILE_NAME),
        lock_path.clone(),
        AnchoredFileLockMode::Exclusive,
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open, acquire, or authenticate anchored task registry lock",
            &lock_path,
            source,
        )
    })?;
    let result = operation();
    let finish = lock.finish();
    match (result, finish) {
        (Ok(value), Ok(())) => Ok(value),
        (_, Err(AnchoredFileLockFinishError::Attachment(source))) => {
            Err(DaemonCoreError::StorageMutationAuthorityLost {
                operation: "retention task-registry mutation",
                path: task_registry_path(workspace_root),
                source,
            })
        }
        (Err(error), _) => Err(error),
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock anchored task registry",
            task_registry_path(workspace_root),
            source,
        )),
    }
}

#[cfg(not(unix))]
fn apply_plan(_snapshot: &mut StoreSnapshot, _plan: &mut RetentionPlan) -> Result<()> {
    Err(DaemonCoreError::RetentionApplyUnsupported)
}

#[cfg(unix)]
fn apply_candidate_with_lease(
    snapshot: &StoreSnapshot,
    candidate: &Candidate,
    lease: &TaskStoreLease,
) -> std::result::Result<RetentionOutcome, CandidateApplyError> {
    if lease.role() != LeaseRole::Retention
        || !lease.matches_root_argument(&snapshot.workspace_root)
    {
        return Err(CandidateApplyError::before_transaction(
            candidate,
            DaemonCoreError::RetentionCandidateChanged {
                path: snapshot.workspace_root.clone(),
            },
        ));
    }
    let state = lease
        .state_capability()
        .map_err(|error| CandidateApplyError::before_transaction(candidate, error))?;
    let daemon = lease
        .daemon_capability()
        .map_err(|error| CandidateApplyError::before_transaction(candidate, error))?;
    apply_candidate_with_authority(
        snapshot,
        candidate,
        &state,
        &daemon,
        || Ok(()),
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(all(unix, test))]
fn apply_candidate(
    snapshot: &StoreSnapshot,
    candidate: &Candidate,
) -> std::result::Result<RetentionOutcome, CandidateApplyError> {
    apply_candidate_with_observers(snapshot, candidate, || Ok(()), || Ok(()), || Ok(()))
}

#[cfg(all(unix, test))]
fn apply_candidate_with_observers(
    snapshot: &StoreSnapshot,
    candidate: &Candidate,
    after_stage: impl FnOnce() -> Result<()>,
    before_readiness_check: impl FnOnce() -> Result<()>,
    before_delete: impl FnOnce() -> Result<()>,
) -> std::result::Result<RetentionOutcome, CandidateApplyError> {
    let state = CapabilityDir::open(&snapshot.state_root).map_err(|source| {
        CandidateApplyError::before_transaction(
            candidate,
            DaemonCoreError::io(
                "failed to open test Packet28 state authority",
                &snapshot.state_root,
                source,
            ),
        )
    })?;
    let daemon = state.open_dir(OsStr::new("daemon")).map_err(|source| {
        CandidateApplyError::before_transaction(
            candidate,
            DaemonCoreError::io(
                "failed to open test daemon authority",
                daemon_dir(&snapshot.workspace_root),
                source,
            ),
        )
    })?;
    apply_candidate_with_authority(
        snapshot,
        candidate,
        &state,
        &daemon,
        after_stage,
        before_readiness_check,
        before_delete,
    )
}

#[cfg(unix)]
fn apply_candidate_with_authority(
    snapshot: &StoreSnapshot,
    candidate: &Candidate,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
    after_stage: impl FnOnce() -> Result<()>,
    before_readiness_check: impl FnOnce() -> Result<()>,
    before_delete: impl FnOnce() -> Result<()>,
) -> std::result::Result<RetentionOutcome, CandidateApplyError> {
    let mut transaction =
        StagingTransaction::new_with_authority(snapshot, candidate, state, daemon)
            .map_err(|error| CandidateApplyError::before_transaction(candidate, error))?;
    if let Err(error) = transaction.stage_all(candidate) {
        return Err(transaction.rollback_after_precommit_error(candidate, error));
    }
    #[cfg(test)]
    if let Err(error) = inject_configured_failure_after_stage(candidate) {
        return Err(transaction.rollback_after_precommit_error(candidate, error));
    }
    if let Err(error) = after_stage() {
        return Err(transaction.rollback_after_precommit_error(candidate, error));
    }

    let candidate_remains_safe = match candidate_remains_safe_after_staging_anchored(
        snapshot,
        candidate,
        &transaction.state,
        &transaction.daemon,
    ) {
        Ok(safe) => safe,
        Err(error) => {
            return Err(transaction.rollback_after_precommit_error(candidate, error));
        }
    };
    if !candidate_remains_safe {
        transaction.rollback_before_skip(candidate)?;
        return Ok(RetentionOutcome::Skipped);
    }

    if let Err(error) = before_readiness_check() {
        return Err(transaction.rollback_after_precommit_error(candidate, error));
    }
    let readiness = ready_path(&snapshot.workspace_root);
    let readiness_exists = match transaction
        .daemon
        .entry_identity(OsStr::new(READY_FILE_NAME))
    {
        Ok(identity) => identity.is_some(),
        Err(source) => {
            return Err(transaction.rollback_after_precommit_error(
                candidate,
                DaemonCoreError::io(
                    "failed to inspect retained daemon readiness marker",
                    &readiness,
                    source,
                ),
            ));
        }
    };
    if readiness_exists {
        return Err(transaction.rollback_after_precommit_error(
            candidate,
            DaemonCoreError::RetentionBlockedByDaemon { path: readiness },
        ));
    }

    let expected_records = candidate.record_values.clone();
    let daemon = match transaction.daemon.duplicate() {
        Ok(daemon) => daemon,
        Err(source) => {
            let error = DaemonCoreError::io(
                "failed to retain daemon registry capability",
                transaction.daemon.display_path(),
                source,
            );
            return Err(transaction.rollback_after_precommit_error(candidate, error));
        }
    };
    let removed = match remove_anchored_registry_records_if_unchanged_with_commit(
        &daemon,
        &snapshot.workspace_root,
        &expected_records,
        candidate.registry_revision,
        candidate.registry_checkpoint_generation,
        || transaction.mark_committed(),
    ) {
        Ok(removed) => removed,
        Err(error) if transaction.rollback_enabled => {
            return Err(transaction.rollback_after_precommit_error(candidate, error));
        }
        Err(error) => {
            return Err(CandidateApplyError::capture_committed(
                &transaction,
                candidate,
                error,
            ));
        }
    };
    if !removed {
        transaction.rollback_before_skip(candidate)?;
        return Ok(RetentionOutcome::Skipped);
    }

    #[cfg(test)]
    inject_configured_committed_partial_delete(&transaction, candidate)
        .map_err(|error| CandidateApplyError::capture_committed(&transaction, candidate, error))?;
    #[cfg(test)]
    inject_configured_committed_nested_partial_failure(&transaction, candidate)
        .map_err(|error| CandidateApplyError::capture_committed(&transaction, candidate, error))?;
    #[cfg(test)]
    inject_configured_committed_measurement_failure(candidate)
        .map_err(|error| CandidateApplyError::capture_committed(&transaction, candidate, error))?;
    before_delete()
        .map_err(|error| CandidateApplyError::capture_committed(&transaction, candidate, error))?;
    transaction
        .delete_committed()
        .map_err(|error| CandidateApplyError::capture_committed(&transaction, candidate, error))?;
    Ok(RetentionOutcome::Removed)
}

#[cfg(test)]
std::thread_local! {
    static INJECT_FAILURE_AFTER_STAGE_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_ROLLBACK_CONFLICT_AFTER_STAGE_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_CONSTRUCTION_ROLLBACK_CONFLICT_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_COMMITTED_PARTIAL_DELETE_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_COMMITTED_NESTED_PARTIAL_FAILURE_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_COMMITTED_MEASUREMENT_FAILURE_FOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_QUARANTINE_GROUP_NAMES:
        std::cell::RefCell<std::collections::VecDeque<OsString>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
fn inject_configured_failure_after_stage(candidate: &Candidate) -> Result<()> {
    let inject_rollback_conflict = INJECT_ROLLBACK_CONFLICT_AFTER_STAGE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() == Some(candidate.storage_key.as_str()) {
            configured.take();
            true
        } else {
            false
        }
    });
    if inject_rollback_conflict {
        let artifact = candidate.artifact.as_ref().ok_or_else(|| {
            DaemonCoreError::RetentionCandidateChanged {
                path: candidate.storage_key.clone().into(),
            }
        })?;
        fs::create_dir(&artifact.path).map_err(|source| {
            DaemonCoreError::io(
                "failed to recreate retention source for rollback-conflict injection",
                &artifact.path,
                source,
            )
        })?;
        fs::write(artifact.path.join("payload.bin"), b"replacement").map_err(|source| {
            DaemonCoreError::io(
                "failed to populate recreated retention source for rollback-conflict injection",
                &artifact.path,
                source,
            )
        })?;
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: artifact.path.clone(),
        });
    }
    INJECT_FAILURE_AFTER_STAGE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() != Some(candidate.storage_key.as_str()) {
            return Ok(());
        }
        configured.take();
        Err(DaemonCoreError::io(
            "injected post-stage retention failure",
            candidate.storage_key.as_str(),
            std::io::Error::other("injected post-stage retention failure"),
        ))
    })
}

#[cfg(test)]
fn inject_configured_committed_partial_delete(
    transaction: &StagingTransaction,
    candidate: &Candidate,
) -> Result<()> {
    INJECT_COMMITTED_PARTIAL_DELETE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() != Some(candidate.storage_key.as_str()) {
            return Ok(());
        }
        configured.take();
        let component = transaction.journal.components.first().ok_or_else(|| {
            DaemonCoreError::RetentionCandidateChanged {
                path: transaction.group.display_path().to_path_buf(),
            }
        })?;
        let staged_name = OsStr::new(component.kind.staged_name());
        transaction
            .group
            .remove_tree_entry_verified(staged_name, component.identity)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inject partial committed retention deletion",
                    transaction.group.display_path().join(staged_name),
                    source,
                )
            })
    })
}

#[cfg(test)]
fn inject_configured_committed_nested_partial_failure(
    transaction: &StagingTransaction,
    candidate: &Candidate,
) -> Result<()> {
    INJECT_COMMITTED_NESTED_PARTIAL_FAILURE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() != Some(candidate.storage_key.as_str()) {
            return Ok(());
        }
        configured.take();
        let component = transaction.journal.components.first().ok_or_else(|| {
            DaemonCoreError::RetentionCandidateChanged {
                path: transaction.group.display_path().to_path_buf(),
            }
        })?;
        let staged_name = OsStr::new(component.kind.staged_name());
        let staged = transaction.group.open_dir(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to open staged artifact for partial deletion injection",
                transaction.group.display_path().join(staged_name),
                source,
            )
        })?;
        let child_name = staged
            .entries()
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect staged artifact for partial deletion injection",
                    staged.display_path(),
                    source,
                )
            })?
            .into_iter()
            .next()
            .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                path: staged.display_path().to_path_buf(),
            })?;
        let child_identity = staged
            .entry_identity(&child_name)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to identify staged artifact child for partial deletion injection",
                    staged.display_path().join(&child_name),
                    source,
                )
            })?
            .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                path: staged.display_path().join(&child_name),
            })?;
        staged
            .remove_tree_entry_verified(&child_name, child_identity)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inject nested partial committed retention deletion",
                    staged.display_path().join(&child_name),
                    source,
                )
            })?;
        Err(DaemonCoreError::io(
            "injected failure after nested committed retention deletion",
            staged.display_path(),
            std::io::Error::other("injected nested partial deletion failure"),
        ))
    })
}

#[cfg(test)]
fn inject_configured_committed_measurement_failure(candidate: &Candidate) -> Result<()> {
    INJECT_COMMITTED_MEASUREMENT_FAILURE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() != Some(candidate.storage_key.as_str()) {
            return Ok(());
        }
        configured.take();
        let artifact = candidate.artifact.as_ref().ok_or_else(|| {
            DaemonCoreError::RetentionCandidateChanged {
                path: candidate.storage_key.clone().into(),
            }
        })?;
        fs::create_dir(&artifact.path).map_err(|source| {
            DaemonCoreError::io(
                "failed to recreate retention source for measurement injection",
                &artifact.path,
                source,
            )
        })?;
        let replacement = artifact.path.join("replacement.bin");
        fs::write(&replacement, b"replacement").map_err(|source| {
            DaemonCoreError::io(
                "failed to populate recreated retention source for measurement injection",
                &replacement,
                source,
            )
        })?;
        Err(DaemonCoreError::io(
            "injected committed retention measurement failure",
            &artifact.path,
            std::io::Error::other("injected committed measurement failure"),
        ))
    })
}

#[cfg(unix)]
fn candidate_remains_safe_after_staging_anchored(
    snapshot: &StoreSnapshot,
    candidate: &Candidate,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
) -> Result<bool> {
    let latest = load_targeted_candidate_anchored(snapshot, &candidate.storage_key, state, daemon)?;
    Ok(match latest.candidate.as_ref() {
        Some(current) => {
            latest.reliable
                && current.protected_reasons.is_empty()
                && current.task_ids == candidate.task_ids
                && current.record_values == candidate.record_values
                && current.registry_revision == candidate.registry_revision
                && current.registry_checkpoint_generation
                    == candidate.registry_checkpoint_generation
                && current.artifact.is_none()
                && current.event.is_none()
        }
        None => {
            latest.reliable
                && candidate.task_ids.is_empty()
                && !storage_key_is_active(&latest.active_storage_keys, &candidate.storage_key)
        }
    })
}

#[cfg(unix)]
#[derive(Debug)]
struct StagingTransaction {
    state: CapabilityDir,
    daemon: CapabilityDir,
    quarantine: CapabilityDir,
    group: CapabilityDir,
    group_name: OsString,
    journal: QuarantineJournal,
    journal_identity: Option<FileIdentity>,
    staged_components: Vec<usize>,
    rollback_enabled: bool,
}

#[cfg(unix)]
fn next_quarantine_group_name() -> OsString {
    #[cfg(test)]
    if let Some(name) =
        INJECT_QUARANTINE_GROUP_NAMES.with(|configured| configured.borrow_mut().pop_front())
    {
        return name;
    }

    let sequence = QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let observed_at_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let process_id = std::process::id();
    let entropy_high = RandomState::new().hash_one((process_id, sequence, observed_at_nanos, 0_u8));
    let entropy_low = RandomState::new().hash_one((process_id, sequence, observed_at_nanos, 1_u8));
    OsString::from(format!(
        "task-{process_id}-{sequence}-{entropy_high:016x}{entropy_low:016x}"
    ))
}

#[cfg(unix)]
fn create_quarantine_group_with_retry(
    quarantine: &CapabilityDir,
) -> Result<(OsString, CapabilityDir)> {
    let mut last_collision = None;
    for _ in 0..MAX_QUARANTINE_GROUP_CREATE_ATTEMPTS {
        let group_name = next_quarantine_group_name();
        match quarantine.create_dir(&group_name, 0o700) {
            Ok(group) => return Ok((group_name, group)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(group_name);
            }
            Err(source) => {
                return Err(DaemonCoreError::io(
                    "failed to create retention quarantine group",
                    quarantine.display_path().join(&group_name),
                    source,
                ));
            }
        }
    }
    let collision = last_collision.unwrap_or_else(|| OsString::from("unknown-collision"));
    Err(DaemonCoreError::io(
        "failed to create a unique retention quarantine group",
        quarantine.display_path().join(collision),
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("all {MAX_QUARANTINE_GROUP_CREATE_ATTEMPTS} high-entropy group names collided"),
        ),
    ))
}

#[cfg(unix)]
impl StagingTransaction {
    #[cfg(test)]
    fn new(snapshot: &StoreSnapshot, candidate: &Candidate) -> Result<Self> {
        let state = CapabilityDir::open(&snapshot.state_root).map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state capability",
                &snapshot.state_root,
                source,
            )
        })?;
        let daemon = state.open_dir(OsStr::new("daemon")).map_err(|source| {
            DaemonCoreError::io(
                "failed to open daemon state capability",
                snapshot.state_root.join("daemon"),
                source,
            )
        })?;
        Self::new_with_authority(snapshot, candidate, &state, &daemon)
    }

    fn new_with_authority(
        snapshot: &StoreSnapshot,
        candidate: &Candidate,
        state: &CapabilityDir,
        daemon: &CapabilityDir,
    ) -> Result<Self> {
        if snapshot.state_root_identity != Some(state.identity())
            || state
                .entry_identity(OsStr::new("daemon"))
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to authenticate retained daemon state",
                        snapshot.state_root.join("daemon"),
                        source,
                    )
                })?
                != Some(daemon.identity())
        {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: snapshot.state_root.clone(),
            });
        }
        ensure_same_filesystem(
            state.identity(),
            daemon.identity(),
            daemon.display_path(),
            "daemon state for retention is on another filesystem",
        )?;
        let state = state.duplicate().map_err(|source| {
            DaemonCoreError::io(
                "failed to retain Packet28 state capability",
                &snapshot.state_root,
                source,
            )
        })?;
        let daemon = daemon.duplicate().map_err(|source| {
            DaemonCoreError::io(
                "failed to retain daemon state capability",
                daemon.display_path(),
                source,
            )
        })?;
        let quarantine = state
            .ensure_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to create retention quarantine capability",
                    snapshot.state_root.join(QUARANTINE_DIR_NAME),
                    source,
                )
            })?;
        ensure_same_filesystem(
            state.identity(),
            quarantine.identity(),
            quarantine.display_path(),
            "retention quarantine is on another filesystem",
        )?;
        let journal =
            journal_for_candidate(&snapshot.workspace_root, &snapshot.state_root, candidate)?;
        let (group_name, group) = create_quarantine_group_with_retry(&quarantine)?;
        ensure_same_filesystem(
            state.identity(),
            group.identity(),
            group.display_path(),
            "new retention quarantine group is on another filesystem",
        )?;
        let mut transaction = Self {
            state,
            daemon,
            quarantine,
            group,
            group_name,
            journal,
            journal_identity: None,
            staged_components: Vec::new(),
            rollback_enabled: true,
        };
        if let Err(error) = transaction.persist_journal() {
            return Err(transaction.fail_construction(error));
        }
        if let Err(source) = transaction.group.probe_noreplace_rename() {
            let error = DaemonCoreError::io(
                "atomic no-replace rename is unavailable for retention quarantine",
                transaction.group.display_path(),
                source,
            );
            return Err(transaction.fail_construction(error));
        }
        Ok(transaction)
    }

    fn fail_construction(&mut self, trigger_error: DaemonCoreError) -> DaemonCoreError {
        let group_path = self.group.display_path().to_path_buf();
        #[cfg(test)]
        if INJECT_CONSTRUCTION_ROLLBACK_CONFLICT_FOR.with(|configured| {
            let mut configured = configured.borrow_mut();
            if configured.as_deref() == Some(self.journal.storage_key.as_str()) {
                configured.take();
                true
            } else {
                false
            }
        }) {
            let held_name = OsString::from(format!(
                ".held-construction-{}",
                QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            if let Err(source) =
                self.quarantine
                    .rename_to_noreplace(&self.group_name, &self.quarantine, &held_name)
            {
                self.rollback_enabled = false;
                return DaemonCoreError::io(
                    "failed to inject constructor rollback conflict",
                    &group_path,
                    source,
                );
            }
            if let Err(source) = self.quarantine.create_dir(&self.group_name, 0o700) {
                self.rollback_enabled = false;
                return DaemonCoreError::io(
                    "failed to inject constructor rollback replacement",
                    &group_path,
                    source,
                );
            }
        }
        match self.rollback() {
            Ok(()) => trigger_error,
            Err(rollback_error) => {
                self.rollback_enabled = false;
                DaemonCoreError::io(
                    "failed to clean up an unstarted retention transaction",
                    group_path,
                    std::io::Error::other(format!(
                        "{rollback_error}; cleanup was required after: {trigger_error}"
                    )),
                )
            }
        }
    }

    fn rollback_after_precommit_error(
        &mut self,
        candidate: &Candidate,
        trigger_error: DaemonCoreError,
    ) -> CandidateApplyError {
        let group_path = self.group.display_path().to_path_buf();
        match self.rollback() {
            Ok(()) => CandidateApplyError::rolled_back(candidate, trigger_error),
            Err(rollback_error) => {
                // Recovery now exclusively owns the durable precommit group.
                // Do not let Drop silently retry and make the reported outcome
                // depend on an unobservable second attempt.
                self.rollback_enabled = false;
                CandidateApplyError::rollback_failed(
                    candidate,
                    &group_path,
                    trigger_error,
                    rollback_error,
                )
            }
        }
    }

    fn rollback_before_skip(
        &mut self,
        candidate: &Candidate,
    ) -> std::result::Result<(), CandidateApplyError> {
        let path = candidate
            .artifact
            .as_ref()
            .or(candidate.event.as_ref())
            .map(|component| component.path.clone())
            .unwrap_or_else(|| self.daemon.display_path().join(TASK_REGISTRY_FILE_NAME));
        let trigger_error = DaemonCoreError::RetentionCandidateChanged { path };
        match self.rollback() {
            Ok(()) => Ok(()),
            Err(rollback_error) => {
                let group_path = self.group.display_path().to_path_buf();
                self.rollback_enabled = false;
                Err(CandidateApplyError::rollback_failed(
                    candidate,
                    &group_path,
                    trigger_error,
                    rollback_error,
                ))
            }
        }
    }

    fn stage_all(&mut self, candidate: &Candidate) -> Result<()> {
        let mut components = Vec::new();
        if let Some(component) = &candidate.artifact {
            components.push(component);
        }
        if let Some(component) = &candidate.event {
            components.push(component);
        }
        for (index, component) in components.into_iter().enumerate() {
            self.stage_component(index, component)?;
        }
        Ok(())
    }

    fn stage_component(&mut self, index: usize, component: &ManagedComponent) -> Result<()> {
        self.stage_component_with_observer(index, component, || Ok(()))
    }

    fn stage_component_with_observer(
        &mut self,
        index: usize,
        component: &ManagedComponent,
        after_rename: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.stage_component_with_observers(index, component, || Ok(()), after_rename)
    }

    fn stage_component_with_observers(
        &mut self,
        index: usize,
        component: &ManagedComponent,
        before_rename: impl FnOnce() -> Result<()>,
        after_rename: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let journal_component = self.journal.components[index].clone();
        let staged_name = OsStr::new(journal_component.kind.staged_name());
        let (parent, original_name) = self.open_original_location(&journal_component)?;
        let current_scan = scan_capability_entry_with_limits(
            &parent,
            &original_name,
            &mut Vec::new(),
            "candidate_revalidation_failed",
            ScanLimits::DEFAULT,
        )?;
        if !current_scan.safe || !scan_matches(&component.scan, &current_scan) {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: component.path.clone(),
            });
        }
        if parent.entry_identity(&original_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect retention source capability",
                &component.path,
                source,
            )
        })? != Some(journal_component.identity)
        {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: component.path.clone(),
            });
        }
        before_rename()?;
        parent
            .rename_to_noreplace_uncommitted(&original_name, &self.group, staged_name)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to stage retention candidate",
                    &component.path,
                    source,
                )
            })?;
        // From this point onward, every error path must attempt rollback. The
        // rename is already visible even if the following durability sync
        // fails.
        self.staged_components.push(index);
        after_rename()?;
        parent.sync_rename(&self.group).map_err(|source| {
            DaemonCoreError::io(
                "failed to synchronize staged retention candidate",
                &component.path,
                source,
            )
        })?;
        if self.group.entry_identity(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to verify staged retention candidate",
                self.group.display_path().join(staged_name),
                source,
            )
        })? != Some(journal_component.identity)
        {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: component.path.clone(),
            });
        }
        Ok(())
    }

    fn mark_committed(&mut self) -> Result<()> {
        self.validate_precommit_group_for_commit()?;
        self.mark_committed_with_observer(|| Ok(()))
    }

    fn mark_committed_with_observer(
        &mut self,
        after_rename: impl FnOnce() -> std::io::Result<()>,
    ) -> Result<()> {
        self.journal.phase = QuarantinePhase::Committed;
        let bytes = self.encode_journal()?;
        match self.group.write_json_atomically_with_observer(
            OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
            &bytes,
            RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            after_rename,
        ) {
            Ok(()) => {
                self.rollback_enabled = false;
                let identity = authenticate_quarantine_journal(
                    &self.group,
                    OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                    &self.journal,
                    None,
                )?;
                validate_committed_group(&self.group, &self.journal, false, Some(identity))?;
                self.journal_identity = Some(identity);
                Ok(())
            }
            Err(error) => {
                // A visible committed marker is the point of no return. Even
                // when its directory fsync reports an error, recovery must own
                // the transaction instead of racing an in-process rollback.
                if error.renamed {
                    self.rollback_enabled = false;
                } else {
                    self.journal.phase = QuarantinePhase::Precommit;
                }
                Err(self.journal_write_error(error))
            }
        }
    }

    fn rollback(&mut self) -> Result<()> {
        for index in self.staged_components.iter().rev().copied() {
            let component = self.journal.components[index].clone();
            let (parent, original_name) = self.open_original_location(&component)?;
            restore_component_from_group(&self.group, &parent, &original_name, &component)?;
        }
        self.staged_components.clear();
        remove_precommit_transient_files(&self.group)?;
        let remaining = bounded_quarantine_group_entries(
            &self.group,
            "failed to validate rolled-back retention quarantine",
        )?;
        match remaining.as_slice() {
            [] => {}
            [name] if name == OsStr::new(QUARANTINE_JOURNAL_FILE_NAME) => {
                let identity = authenticate_quarantine_journal(
                    &self.group,
                    name,
                    &self.journal,
                    self.journal_identity,
                )?;
                self.journal_identity = Some(identity);
            }
            _ => {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: self.group.display_path().to_path_buf(),
                });
            }
        }
        self.quarantine
            .remove_tree_entry_verified(&self.group_name, self.group.identity())
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to remove rolled-back retention quarantine",
                    self.quarantine.display_path().join(&self.group_name),
                    source,
                )
            })?;
        self.rollback_enabled = false;
        Ok(())
    }

    fn delete_committed(&mut self) -> Result<()> {
        let progress = delete_committed_group(
            &self.quarantine,
            &self.group_name,
            self.group.identity(),
            &self.journal,
            false,
        )?;
        self.rollback_enabled = false;
        match progress {
            RemovalProgress::Complete => Ok(()),
            RemovalProgress::More => Err(retention_resource_limit_error(
                "committed retention deletion made bounded progress and requires recovery",
                self.group.display_path(),
                "the committed quarantine group remains durable for the next recovery pass"
                    .to_string(),
            )),
        }
    }

    fn persist_journal(&mut self) -> Result<()> {
        let bytes = self.encode_journal()?;
        self.group
            .write_json_atomically(
                OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                &bytes,
                RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            )
            .map_err(|error| self.journal_write_error(error))?;
        let identity = authenticate_quarantine_journal(
            &self.group,
            OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
            &self.journal,
            None,
        )?;
        self.journal_identity = Some(identity);
        Ok(())
    }

    fn validate_precommit_group_for_commit(&self) -> Result<()> {
        if self.journal.phase != QuarantinePhase::Precommit
            || self.staged_components.len() != self.journal.components.len()
            || self
                .staged_components
                .iter()
                .copied()
                .ne(0..self.journal.components.len())
        {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: self.group.display_path().to_path_buf(),
            });
        }
        let entries = bounded_quarantine_group_entries(
            &self.group,
            "failed to enumerate precommit retention quarantine before commit",
        )?;
        let mut expected = BTreeSet::from([OsString::from(QUARANTINE_JOURNAL_FILE_NAME)]);
        for component in &self.journal.components {
            expected.insert(OsString::from(component.kind.staged_name()));
        }
        if entries.into_iter().collect::<BTreeSet<_>>() != expected {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: self.group.display_path().to_path_buf(),
            });
        }
        let journal_identity =
            self.journal_identity
                .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                    path: self.group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
                })?;
        authenticate_quarantine_journal(
            &self.group,
            OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
            &self.journal,
            Some(journal_identity),
        )?;
        for component in &self.journal.components {
            authenticate_quarantine_component(
                &self.group,
                OsStr::new(component.kind.staged_name()),
                component.identity,
            )?;
            let (parent, original_name) = self.open_original_location(component)?;
            if parent
                .entry_identity(&original_name)
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to verify staged retention source absence before commit",
                        parent.display_path().join(&original_name),
                        source,
                    )
                })?
                .is_some()
            {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: parent.display_path().join(original_name),
                });
            }
        }
        Ok(())
    }

    fn encode_journal(&self) -> Result<Vec<u8>> {
        self.encode_journal_with_limit(MAX_TASK_RETENTION_JOURNAL_BYTES)
    }

    fn encode_journal_with_limit(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(&self.journal).map_err(|source| {
            DaemonCoreError::json(
                "failed to encode retention quarantine journal for",
                self.group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
                source,
            )
        })?;
        if bytes.len() > max_bytes {
            return Err(DaemonCoreError::io(
                "retention quarantine journal exceeds the supported write bound",
                self.group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("journal is {} bytes; maximum is {max_bytes}", bytes.len()),
                ),
            ));
        }
        crate::storage::validate_authority_json(
            &bytes,
            AuthorityJsonProfile::RetentionJournal { max_bytes },
        )
        .map_err(|error| {
            crate::storage::map_authority_json_error(
                &self.group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
                AuthorityJsonProfile::RetentionJournal { max_bytes },
                "failed to validate encoded retention quarantine journal for",
                error,
            )
        })?;
        Ok(bytes)
    }

    fn journal_write_error(&self, error: AtomicWriteError) -> DaemonCoreError {
        DaemonCoreError::io(
            if error.renamed {
                "failed to synchronize renamed retention quarantine journal"
            } else {
                "failed to persist retention quarantine journal"
            },
            self.group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
            error.source,
        )
    }

    fn remaining_candidate_logical_bytes(&self, candidate: &Candidate) -> Result<u64> {
        let mut remaining = 0_u64;
        if !candidate.record_values.is_empty() {
            let workspace_root = self.journal_workspace_root();
            let tasks = with_anchored_registry_lock(&self.daemon, &workspace_root, || {
                crate::storage::load_retained_registry_snapshot_under_task_lock(
                    &workspace_root,
                    &self.daemon,
                )
                .map(|snapshot| snapshot.record_values)
            })?;
            for (task_id, expected) in &candidate.record_values {
                match tasks.get(task_id) {
                    Some(current) if current == expected => {
                        remaining = remaining.saturating_add(
                            serde_json::to_vec(expected)
                                .map_err(|source| {
                                    DaemonCoreError::json(
                                        "failed to measure retained task registry record in",
                                        self.daemon.display_path().join(TASK_REGISTRY_FILE_NAME),
                                        source,
                                    )
                                })?
                                .len() as u64,
                        );
                    }
                    None => {}
                    Some(_) => {
                        return Err(DaemonCoreError::RetentionCandidateChanged {
                            path: self.daemon.display_path().join(TASK_REGISTRY_FILE_NAME),
                        });
                    }
                }
            }
        }

        for component in &self.journal.components {
            remaining =
                remaining.saturating_add(self.component_remaining_logical_bytes(component)?);
        }
        if remaining > candidate.logical_bytes() {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: self.group.display_path().to_path_buf(),
            });
        }
        Ok(remaining)
    }

    fn component_remaining_logical_bytes(&self, component: &JournalComponent) -> Result<u64> {
        let (parent, original_name) = self.open_original_location(component)?;
        let original_identity = parent.entry_identity(&original_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to measure retention source after committed cleanup",
                parent.display_path().join(&original_name),
                source,
            )
        })?;
        match original_identity {
            Some(identity) if identity == component.identity => {
                return parent
                    .entry_logical_bytes_verified(&original_name, component.identity)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to measure retained cleanup source",
                            parent.display_path().join(&original_name),
                            source,
                        )
                    });
            }
            Some(_) => {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: parent.display_path().join(&original_name),
                });
            }
            None => {}
        }
        for entry in bounded_quarantine_group_entries(
            &self.group,
            "failed to measure committed retention quarantine",
        )? {
            if self.group.entry_identity(&entry).map_err(|source| {
                DaemonCoreError::io(
                    "failed to measure committed retention quarantine entry",
                    self.group.display_path().join(&entry),
                    source,
                )
            })? == Some(component.identity)
            {
                return self
                    .group
                    .entry_logical_bytes_verified(&entry, component.identity)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to measure retained committed component",
                            self.group.display_path().join(&entry),
                            source,
                        )
                    });
            }
        }
        for entry in bounded_quarantine_group_names(
            &self.quarantine,
            "failed to measure retention quarantine root",
        )? {
            if self.quarantine.entry_identity(&entry).map_err(|source| {
                DaemonCoreError::io(
                    "failed to measure retention quarantine root entry",
                    self.quarantine.display_path().join(&entry),
                    source,
                )
            })? == Some(component.identity)
            {
                return self
                    .quarantine
                    .entry_logical_bytes_verified(&entry, component.identity)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to measure retained quarantine component",
                            self.quarantine.display_path().join(&entry),
                            source,
                        )
                    });
            }
        }
        Ok(0)
    }

    fn journal_workspace_root(&self) -> PathBuf {
        self.state
            .display_path()
            .parent()
            .unwrap_or_else(|| self.state.display_path())
            .to_path_buf()
    }

    fn open_original_location(
        &self,
        component: &JournalComponent,
    ) -> Result<(CapabilityDir, OsString)> {
        open_journal_location(&self.state, &self.journal, component)
    }
}

#[cfg(unix)]
fn delete_committed_group(
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    expected: FileIdentity,
    journal: &QuarantineJournal,
    allow_missing_components: bool,
) -> Result<RemovalProgress> {
    delete_committed_group_with_observer(
        quarantine,
        group_name,
        expected,
        journal,
        allow_missing_components,
        |_| Ok(()),
    )
}

#[cfg(unix)]
fn delete_committed_group_with_observer(
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    expected: FileIdentity,
    journal: &QuarantineJournal,
    allow_missing_components: bool,
    before_component_delete: impl FnOnce(&CapabilityDir) -> Result<()>,
) -> Result<RemovalProgress> {
    let group_path = quarantine.display_path().join(group_name);
    let (tombstone, group) = quarantine
        .tombstone_dir_entry_verified(group_name, expected)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to isolate committed retention quarantine",
                &group_path,
                source,
            )
        })?;
    validate_committed_group(&group, journal, allow_missing_components, None)?;
    before_component_delete(&group)?;
    // A same-user process can still rename entries inside the private group.
    // Revalidate the complete journal-derived set and all identities before
    // deleting any declared component.
    validate_committed_group(&group, journal, allow_missing_components, None)?;

    for component in &journal.components {
        let staged_name = OsStr::new(component.kind.staged_name());
        let deletion_name = OsStr::new(component.kind.deletion_name());
        if group.entry_identity(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to revalidate duplicated committed quarantine component",
                group.display_path().join(staged_name),
                source,
            )
        })? == Some(component.identity)
            && group.entry_identity(deletion_name).map_err(|source| {
                DaemonCoreError::io(
                    "failed to revalidate duplicated isolated committed component",
                    group.display_path().join(deletion_name),
                    source,
                )
            })? == Some(component.identity)
        {
            // A file tombstone is published before its source link is removed.
            // A crash between those operations leaves two names for the same
            // regular inode. Make the isolated name durable, then remove only
            // the duplicate staged link so the normal bounded deletion path
            // can resume.
            group.sync().map_err(|source| {
                DaemonCoreError::io(
                    "failed to synchronize duplicated committed quarantine component",
                    group.display_path(),
                    source,
                )
            })?;
            group
                .remove_tree_entry_verified(staged_name, component.identity)
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to remove duplicated committed quarantine link",
                        group.display_path().join(staged_name),
                        source,
                    )
                })?;
        }
        if group.entry_identity(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to revalidate committed quarantine component",
                group.display_path().join(staged_name),
                source,
            )
        })? == Some(component.identity)
        {
            group
                .tombstone_entry_to_verified(staged_name, component.identity, deletion_name)
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to isolate committed quarantine component",
                        group.display_path().join(staged_name),
                        source,
                    )
                })?;
        }
        if group.entry_identity(deletion_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to revalidate isolated committed quarantine component",
                group.display_path().join(deletion_name),
                source,
            )
        })? == Some(component.identity)
        {
            let progress =
                remove_committed_component_batch(&group, deletion_name, component.identity)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to delete isolated committed quarantine component",
                            group.display_path().join(deletion_name),
                            source,
                        )
                    })?;
            if progress == RemovalProgress::More {
                return Ok(RemovalProgress::More);
            }
        }
    }
    let remaining = bounded_quarantine_group_entries(
        &group,
        "failed to revalidate emptied committed retention quarantine",
    )?;
    if remaining != vec![OsString::from(QUARANTINE_JOURNAL_FILE_NAME)] {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().to_path_buf(),
        });
    }
    let journal_name = OsStr::new(QUARANTINE_JOURNAL_FILE_NAME);
    let journal_identity = authenticate_quarantine_journal(&group, journal_name, journal, None)?;
    group
        .tombstone_entry_to_verified(
            journal_name,
            journal_identity,
            OsStr::new(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
        )
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to isolate final committed quarantine journal",
                group.display_path().join(journal_name),
                source,
            )
        })?;
    authenticate_quarantine_journal(
        &group,
        OsStr::new(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
        journal,
        Some(journal_identity),
    )?;
    group
        .remove_tombstone_verified(
            OsStr::new(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
            journal_identity,
        )
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to delete final committed quarantine journal",
                group
                    .display_path()
                    .join(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
                source,
            )
        })?;
    quarantine
        .remove_empty_dir_verified(&tombstone, expected)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove empty committed quarantine group",
                quarantine.display_path().join(&tombstone),
                source,
            )
        })?;
    Ok(RemovalProgress::Complete)
}

#[cfg(unix)]
fn remove_committed_component_batch(
    group: &CapabilityDir,
    name: &OsStr,
    expected: FileIdentity,
) -> std::io::Result<RemovalProgress> {
    #[cfg(test)]
    if let Some(max_entries) = INJECT_COMMITTED_DELETION_BATCH_ENTRIES.with(std::cell::Cell::get) {
        return group.remove_tombstone_verified_batch_with_limit(name, expected, max_entries);
    }
    group.remove_tombstone_verified_batch(name, expected)
}

#[cfg(unix)]
fn validate_committed_group(
    group: &CapabilityDir,
    journal: &QuarantineJournal,
    allow_missing_components: bool,
    expected_journal_identity: Option<FileIdentity>,
) -> Result<FileIdentity> {
    if journal.phase != QuarantinePhase::Committed {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME),
        });
    }
    let entries = bounded_quarantine_group_entries(
        group,
        "failed to enumerate committed retention quarantine",
    )?;
    let mut allowed_names = BTreeSet::from([OsString::from(QUARANTINE_JOURNAL_FILE_NAME)]);
    for component in &journal.components {
        allowed_names.insert(OsString::from(component.kind.staged_name()));
        allowed_names.insert(OsString::from(component.kind.deletion_name()));
    }
    if entries.iter().any(|entry| !allowed_names.contains(entry)) {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().to_path_buf(),
        });
    }
    let journal_identity = authenticate_quarantine_journal(
        group,
        OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
        journal,
        expected_journal_identity,
    )?;

    for component in &journal.components {
        let staged_name = OsStr::new(component.kind.staged_name());
        let deletion_name = OsStr::new(component.kind.deletion_name());
        let staged = group.entry_identity(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect committed quarantine component",
                group.display_path().join(staged_name),
                source,
            )
        })?;
        let deleting = group.entry_identity(deletion_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect isolated committed quarantine component",
                group.display_path().join(deletion_name),
                source,
            )
        })?;
        match (staged, deleting) {
            (Some(identity), None) if identity == component.identity => {
                authenticate_quarantine_component(group, staged_name, component.identity)?;
            }
            (None, Some(identity)) if identity == component.identity => {
                authenticate_quarantine_component(group, deletion_name, component.identity)?;
            }
            (Some(staged_identity), Some(deleting_identity))
                if staged_identity == component.identity
                    && deleting_identity == component.identity
                    && group.entry_is_regular_file(staged_name).map_err(|source| {
                        DaemonCoreError::io(
                            "failed to inspect duplicated committed component type",
                            group.display_path().join(staged_name),
                            source,
                        )
                    })? == Some(true)
                    && group
                        .entry_is_regular_file(deletion_name)
                        .map_err(|source| {
                            DaemonCoreError::io(
                                "failed to inspect duplicated isolated component type",
                                group.display_path().join(deletion_name),
                                source,
                            )
                        })?
                        == Some(true) =>
            {
                group
                    .authenticate_regular_file_with_link_count(staged_name, component.identity, 2)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to authenticate duplicated committed component",
                            group.display_path().join(staged_name),
                            source,
                        )
                    })?;
                group
                    .authenticate_regular_file_with_link_count(deletion_name, component.identity, 2)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to authenticate duplicated isolated component",
                            group.display_path().join(deletion_name),
                            source,
                        )
                    })?;
            }
            (None, None) if allow_missing_components => {}
            _ => {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: group.display_path().join(staged_name),
                });
            }
        }
    }
    Ok(journal_identity)
}

#[cfg(unix)]
fn authenticate_quarantine_component(
    group: &CapabilityDir,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<()> {
    match group.entry_is_regular_file(name).map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect retention quarantine component type",
            group.display_path().join(name),
            source,
        )
    })? {
        Some(true) => group
            .authenticate_regular_file(name, expected)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to authenticate retention quarantine file",
                    group.display_path().join(name),
                    source,
                )
            }),
        Some(false) => {
            let directory = group.open_private_dir(name, 0o700).map_err(|source| {
                DaemonCoreError::io(
                    "failed to authenticate retention quarantine directory",
                    group.display_path().join(name),
                    source,
                )
            })?;
            if directory.identity() != expected {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: group.display_path().join(name),
                });
            }
            Ok(())
        }
        None => Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().join(name),
        }),
    }
}

#[cfg(unix)]
fn authenticate_quarantine_journal(
    group: &CapabilityDir,
    name: &OsStr,
    expected_journal: &QuarantineJournal,
    expected_identity: Option<FileIdentity>,
) -> Result<FileIdentity> {
    let (actual, identity) = read_quarantine_journal(group, name)?;
    if expected_identity.is_some_and(|expected| expected != identity) || &actual != expected_journal
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().join(name),
        });
    }
    Ok(identity)
}

#[cfg(unix)]
fn read_quarantine_journal(
    group: &CapabilityDir,
    name: &OsStr,
) -> Result<(QuarantineJournal, FileIdentity)> {
    let read = group
        .read_file_limited_with_metadata(name, MAX_TASK_RETENTION_JOURNAL_BYTES)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to authenticate retention quarantine journal",
                group.display_path().join(name),
                source,
            )
        })?;
    if group.entry_identity(name).map_err(|source| {
        DaemonCoreError::io(
            "failed to revalidate retention quarantine journal name",
            group.display_path().join(name),
            source,
        )
    })? != Some(read.identity)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().join(name),
        });
    }
    let value = crate::storage::decode_json_value_without_duplicate_keys(
        &read.bytes,
        AuthorityJsonProfile::RetentionJournal {
            max_bytes: MAX_TASK_RETENTION_JOURNAL_BYTES,
        },
    )
    .map_err(|error| {
        crate::storage::map_authority_json_error(
            &group.display_path().join(name),
            AuthorityJsonProfile::RetentionJournal {
                max_bytes: MAX_TASK_RETENTION_JOURNAL_BYTES,
            },
            "failed to decode retention quarantine journal from",
            error,
        )
    })?;
    let actual: QuarantineJournal = serde_json::from_value(value).map_err(|source| {
        DaemonCoreError::json(
            "failed to validate retention quarantine journal from",
            group.display_path().join(name),
            source,
        )
    })?;
    Ok((actual, read.identity))
}

#[cfg(unix)]
impl Drop for StagingTransaction {
    fn drop(&mut self) {
        if self.rollback_enabled {
            let _ = self.rollback();
        }
    }
}

#[cfg(unix)]
fn journal_for_candidate(
    workspace_root: &Path,
    state_root: &Path,
    candidate: &Candidate,
) -> Result<QuarantineJournal> {
    let mut components = Vec::new();
    if let Some(component) = &candidate.artifact {
        components.push(journal_component(
            state_root,
            &candidate.storage_key,
            component,
            JournalComponentKind::Artifacts,
        )?);
    }
    if let Some(component) = &candidate.event {
        components.push(journal_component(
            state_root,
            &candidate.storage_key,
            component,
            JournalComponentKind::Events,
        )?);
    }
    let journal = QuarantineJournal {
        schema_version: QUARANTINE_JOURNAL_SCHEMA_VERSION,
        phase: QuarantinePhase::Precommit,
        storage_key: candidate.storage_key.clone(),
        record_values: candidate.record_values.clone(),
        registry_revision: candidate.registry_revision,
        registry_checkpoint_generation: candidate.registry_checkpoint_generation,
        components,
    };
    validate_quarantine_journal(workspace_root, &journal)?;
    Ok(journal)
}

#[cfg(unix)]
fn journal_component(
    state_root: &Path,
    storage_key: &str,
    component: &ManagedComponent,
    kind: JournalComponentKind,
) -> Result<JournalComponent> {
    let relative = component.path.strip_prefix(state_root).map_err(|_| {
        DaemonCoreError::RetentionCandidateChanged {
            path: component.path.clone(),
        }
    })?;
    if relative != kind.original_relative_path(storage_key) {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: component.path.clone(),
        });
    }
    let Some(identity) = component.scan.identity else {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: component.path.clone(),
        });
    };
    Ok(JournalComponent { kind, identity })
}

#[cfg(unix)]
fn validate_quarantine_journal(workspace_root: &Path, journal: &QuarantineJournal) -> Result<()> {
    let component_kinds = journal
        .components
        .iter()
        .map(|component| component.kind)
        .collect::<BTreeSet<_>>();
    let records_are_bound = journal.record_values.iter().all(|(task_id, value)| {
        crate::storage::task_identifier_shape_error(task_id).is_none()
            && storage_key_for_task(workspace_root, task_id) == journal.storage_key
            && value.get("task_id").and_then(serde_json::Value::as_str) == Some(task_id.as_str())
    });
    let schema_is_supported = matches!(
        journal.schema_version,
        LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION | QUARANTINE_JOURNAL_SCHEMA_VERSION
    );
    let revision_matches_schema = match journal.schema_version {
        QUARANTINE_JOURNAL_SCHEMA_VERSION => {
            journal.record_values.is_empty() || journal.registry_revision.is_some()
        }
        LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION => {
            journal.registry_revision.is_none() && journal.registry_checkpoint_generation.is_none()
        }
        _ => false,
    };
    let valid = schema_is_supported
        && revision_matches_schema
        && storage_key_is_safe(&journal.storage_key)
        && crate::storage::task_storage_key_is_portable(&journal.storage_key)
        && journal.components.len() <= MAX_QUARANTINE_COMPONENTS
        && component_kinds.len() == journal.components.len()
        && journal.record_values.len() <= MAX_QUARANTINE_RECORDS
        && records_are_bound;
    if valid {
        return Ok(());
    }
    Err(DaemonCoreError::io(
        "invalid retention quarantine journal",
        QUARANTINE_JOURNAL_FILE_NAME,
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal contains an unsupported schema, duplicate component, or unbound record",
        ),
    ))
}

#[cfg(unix)]
fn bounded_quarantine_group_names(
    quarantine: &CapabilityDir,
    operation: &'static str,
) -> Result<Vec<OsString>> {
    bounded_quarantine_group_names_with_limit(quarantine, operation, MAX_QUARANTINE_GROUPS)
}

#[cfg(unix)]
fn bounded_quarantine_group_names_with_limit(
    quarantine: &CapabilityDir,
    operation: &'static str,
    max_groups: usize,
) -> Result<Vec<OsString>> {
    match quarantine.entries_bounded(max_groups) {
        Ok(entries) => Ok(entries),
        Err(source) if source.kind() == std::io::ErrorKind::InvalidData => {
            Err(retention_resource_limit_error(
                "retention quarantine exceeded the supported group bound",
                quarantine.display_path(),
                format!("maximum supported groups is {max_groups}: {source}"),
            ))
        }
        Err(source) => Err(DaemonCoreError::io(
            operation,
            quarantine.display_path(),
            source,
        )),
    }
}

#[cfg(unix)]
fn bounded_quarantine_group_entries(
    group: &CapabilityDir,
    operation: &'static str,
) -> Result<Vec<OsString>> {
    bounded_quarantine_group_entries_with_limit(group, operation, MAX_QUARANTINE_GROUP_ENTRIES)
}

#[cfg(unix)]
fn bounded_quarantine_group_entries_with_limit(
    group: &CapabilityDir,
    operation: &'static str,
    max_entries: usize,
) -> Result<Vec<OsString>> {
    match group.entries_bounded(max_entries) {
        Ok(entries) => Ok(entries),
        Err(source) if source.kind() == std::io::ErrorKind::InvalidData => {
            Err(retention_resource_limit_error(
                "retention quarantine group exceeded the supported immediate-entry bound",
                group.display_path(),
                format!("maximum supported entries is {max_entries}: {source}"),
            ))
        }
        Err(source) => Err(DaemonCoreError::io(operation, group.display_path(), source)),
    }
}

#[cfg(unix)]
fn task_store_quarantine_has_groups(lease: &TaskStoreLease) -> Result<bool> {
    #[cfg(test)]
    if INJECT_HANDOFF_QUARANTINE_PRESENT_PASSES.with(|remaining| {
        let current = remaining.get();
        if current == 0 {
            false
        } else {
            remaining.set(current - 1);
            true
        }
    }) {
        return Ok(true);
    }
    let state = lease.state_capability()?;
    let state_path = state.display_path();
    let quarantine = match state.open_private_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700) {
        Ok(quarantine) => quarantine,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open retention quarantine during daemon recovery handoff",
                state_path.join(QUARANTINE_DIR_NAME),
                source,
            ));
        }
    };
    ensure_same_filesystem(
        state.identity(),
        quarantine.identity(),
        quarantine.display_path(),
        "retention quarantine during daemon recovery handoff is on another filesystem",
    )?;
    quarantine.has_entries().map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect retention quarantine during daemon recovery handoff",
            quarantine.display_path(),
            source,
        )
    })
}

#[cfg(unix)]
fn recover_quarantine_groups(
    workspace_root: &Path,
    state: &CapabilityDir,
    daemon: &CapabilityDir,
) -> Result<TaskStoreRecoveryReport> {
    ensure_same_filesystem(
        state.identity(),
        daemon.identity(),
        daemon.display_path(),
        "daemon state for retention recovery is on another filesystem",
    )?;
    let quarantine = match state.open_private_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700) {
        Ok(quarantine) => quarantine,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TaskStoreRecoveryReport::default());
        }
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open retention quarantine capability for recovery",
                state.display_path().join(QUARANTINE_DIR_NAME),
                source,
            ));
        }
    };
    ensure_same_filesystem(
        state.identity(),
        quarantine.identity(),
        quarantine.display_path(),
        "retention quarantine for recovery is on another filesystem",
    )?;
    let mut report = TaskStoreRecoveryReport::default();
    let group_names = bounded_quarantine_group_names(
        &quarantine,
        "failed to enumerate retention quarantine for recovery",
    )?;
    for group_name in group_names {
        let group_path = quarantine.display_path().join(&group_name);
        let group = match quarantine.open_private_dir(&group_name, 0o700) {
            Ok(group) => group,
            Err(source) => {
                report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                push_issue(
                    &mut report.issues,
                    "retention_recovery_invalid_group",
                    &group_path,
                    format!("quarantine group is not a real directory: {source}"),
                );
                continue;
            }
        };
        if let Err(error) = ensure_same_filesystem(
            state.identity(),
            group.identity(),
            group.display_path(),
            "retention quarantine group for recovery is on another filesystem",
        ) {
            report.conflicted_groups = report.conflicted_groups.saturating_add(1);
            push_issue(
                &mut report.issues,
                "retention_recovery_invalid_group",
                &group_path,
                error.to_string(),
            );
            continue;
        }
        let entries = match bounded_quarantine_group_entries(
            &group,
            "failed to enumerate quarantine group",
        ) {
            Ok(entries) if entries.is_empty() => {
                match quarantine.remove_empty_dir_verified(&group_name, group.identity()) {
                    Ok(()) => push_issue(
                        &mut report.issues,
                        "retention_recovery_empty_group_removed",
                        &group_path,
                        "removed an empty quarantine group left before or after journal cleanup"
                            .to_string(),
                    ),
                    Err(source) => {
                        report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                        push_issue(
                            &mut report.issues,
                            "retention_recovery_invalid_group",
                            &group_path,
                            format!("failed to remove empty quarantine group: {source}"),
                        );
                    }
                }
                continue;
            }
            Ok(entries) => entries,
            Err(source) => {
                report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                push_issue(
                    &mut report.issues,
                    "retention_recovery_invalid_group",
                    &group_path,
                    format!("failed to enumerate quarantine group: {source}"),
                );
                continue;
            }
        };
        let normal_journal = OsString::from(QUARANTINE_JOURNAL_FILE_NAME);
        let final_journal = OsString::from(QUARANTINE_JOURNAL_DELETION_FILE_NAME);
        let finalized_journal = entries == [final_journal.clone()];
        if !entries.contains(&normal_journal) && !finalized_journal {
            match remove_journal_less_transient_group(&quarantine, &group_name, &group, &entries) {
                Ok(true) => {
                    push_issue(
                        &mut report.issues,
                        "retention_recovery_unstarted_group_removed",
                        &group_path,
                        "removed a quarantine group left while its journal was being published or deleted"
                            .to_string(),
                    );
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                    push_issue(
                        &mut report.issues,
                        "retention_recovery_invalid_group",
                        &group_path,
                        error.to_string(),
                    );
                    continue;
                }
            }
        }
        let journal_name = if finalized_journal {
            final_journal.as_os_str()
        } else {
            normal_journal.as_os_str()
        };
        let (journal, journal_identity) = match read_quarantine_journal(&group, journal_name) {
            Ok(journal) => journal,
            Err(source) => {
                report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                push_issue(
                    &mut report.issues,
                    "retention_recovery_invalid_journal",
                    &group_path,
                    source.to_string(),
                );
                continue;
            }
        };
        if let Err(error) = validate_quarantine_journal(workspace_root, &journal) {
            report.conflicted_groups = report.conflicted_groups.saturating_add(1);
            push_issue(
                &mut report.issues,
                "retention_recovery_invalid_journal",
                &group_path,
                error.to_string(),
            );
            continue;
        }
        if finalized_journal {
            let recovery = if journal.phase == QuarantinePhase::Committed {
                finish_finalized_committed_group(
                    workspace_root,
                    daemon,
                    &quarantine,
                    &group_name,
                    &group,
                    &journal,
                    journal_identity,
                )
            } else {
                Err(DaemonCoreError::RetentionCandidateChanged {
                    path: group_path.clone(),
                })
            };
            match recovery {
                Ok(()) => {
                    report.completed_committed_groups =
                        report.completed_committed_groups.saturating_add(1);
                }
                Err(error) => {
                    report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                    push_issue(
                        &mut report.issues,
                        "retention_recovery_commit_failed",
                        &group_path,
                        error.to_string(),
                    );
                }
            }
            continue;
        }

        if journal.phase == QuarantinePhase::Precommit {
            match remove_precommit_transient_files(&group) {
                Ok(0) => {}
                Ok(removed) => push_issue(
                    &mut report.issues,
                    "retention_recovery_transient_files_removed",
                    &group_path,
                    format!(
                        "removed {removed} internal transient file(s) left before the precommit transaction could finish"
                    ),
                ),
                Err(error) => {
                    report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                    push_issue(
                        &mut report.issues,
                        "retention_recovery_restore_conflict",
                        &group_path,
                        error.to_string(),
                    );
                    continue;
                }
            }
        }

        let recovery = match journal.phase {
            QuarantinePhase::Precommit => {
                restore_precommit_group(state, &quarantine, &group_name, &group, &journal)
                    .map(|()| RemovalProgress::Complete)
            }
            QuarantinePhase::Committed => finish_committed_group(
                workspace_root,
                daemon,
                &quarantine,
                &group_name,
                &group,
                &journal,
                journal_identity,
            ),
        };
        match recovery {
            Ok(RemovalProgress::Complete) => match journal.phase {
                QuarantinePhase::Precommit => {
                    report.restored_precommit_groups =
                        report.restored_precommit_groups.saturating_add(1);
                }
                QuarantinePhase::Committed => {
                    report.completed_committed_groups =
                        report.completed_committed_groups.saturating_add(1);
                }
            },
            Ok(RemovalProgress::More) => push_issue(
                &mut report.issues,
                "retention_recovery_deletion_pending",
                &group_path,
                "committed deletion made a bounded durable batch of progress and remains protected for the next recovery pass"
                    .to_string(),
            ),
            Err(error) => {
                report.conflicted_groups = report.conflicted_groups.saturating_add(1);
                push_issue(
                    &mut report.issues,
                    match journal.phase {
                        QuarantinePhase::Precommit => "retention_recovery_restore_conflict",
                        QuarantinePhase::Committed => "retention_recovery_commit_failed",
                    },
                    &group_path,
                    error.to_string(),
                );
            }
        }
    }
    report.issues.sort_by(|left, right| {
        (&left.kind, &left.path, &left.message).cmp(&(&right.kind, &right.path, &right.message))
    });
    report.issues.dedup();
    Ok(report)
}

#[cfg(unix)]
fn remove_journal_less_transient_group(
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    group: &CapabilityDir,
    entries: &[OsString],
) -> Result<bool> {
    let [entry] = entries else {
        return Ok(false);
    };
    if quarantine_transient_kind(entry)
        .is_none_or(|kind| matches!(kind, QuarantineTransientKind::AmbiguousGeneratedDeletion))
    {
        return Ok(false);
    }
    remove_verified_transient_file(group, entry)?;
    quarantine
        .remove_empty_dir_verified(group_name, group.identity())
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove unstarted retention quarantine group",
                quarantine.display_path().join(group_name),
                source,
            )
        })?;
    Ok(true)
}

#[cfg(unix)]
fn remove_precommit_transient_files(group: &CapabilityDir) -> Result<usize> {
    let entries = bounded_quarantine_group_entries(
        group,
        "failed to enumerate precommit retention quarantine",
    )?;
    let transient_entries = entries
        .into_iter()
        .filter(|entry| quarantine_transient_kind(entry).is_some())
        .collect::<Vec<_>>();
    for entry in &transient_entries {
        remove_verified_transient_file(group, entry)?;
    }
    Ok(transient_entries.len())
}

#[cfg(unix)]
fn remove_verified_transient_file(group: &CapabilityDir, entry: &OsStr) -> Result<()> {
    let _retained = group.open_read_file(entry).map_err(|source| {
        DaemonCoreError::io(
            "failed to authenticate retention quarantine transient",
            group.display_path().join(entry),
            source,
        )
    })?;
    let identity = group
        .entry_identity(entry)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to identify retention quarantine transient",
                group.display_path().join(entry),
                source,
            )
        })?
        .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().join(entry),
        })?;
    group
        .remove_tree_entry_verified(entry, identity)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove retention quarantine transient",
                group.display_path().join(entry),
                source,
            )
        })
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuarantineTransientKind {
    AtomicWrite,
    GeneratedDeletion,
    AmbiguousGeneratedDeletion,
    NoreplaceProbeSource,
    NoreplaceProbeDestination,
}

#[cfg(unix)]
fn quarantine_transient_kind(name: &OsStr) -> Option<QuarantineTransientKind> {
    let probe_source_deletion = generated_deletion_prefix(NOREPLACE_PROBE_SOURCE_PREFIX);
    let probe_destination_deletion = generated_deletion_prefix(NOREPLACE_PROBE_DESTINATION_PREFIX);
    let kind = [
        (
            RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            QuarantineTransientKind::AtomicWrite,
        ),
        (
            DELETION_TEMP_PREFIX,
            QuarantineTransientKind::AmbiguousGeneratedDeletion,
        ),
        (
            RETENTION_JOURNAL_WRITE_DELETION_TEMP_PREFIX,
            QuarantineTransientKind::GeneratedDeletion,
        ),
        (
            NOREPLACE_PROBE_SOURCE_PREFIX,
            QuarantineTransientKind::NoreplaceProbeSource,
        ),
        (
            NOREPLACE_PROBE_DESTINATION_PREFIX,
            QuarantineTransientKind::NoreplaceProbeDestination,
        ),
        (
            probe_source_deletion.as_ref(),
            QuarantineTransientKind::GeneratedDeletion,
        ),
        (
            probe_destination_deletion.as_ref(),
            QuarantineTransientKind::GeneratedDeletion,
        ),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| generated_name_matches(name, prefix).then_some(kind));
    kind
}

#[cfg(unix)]
fn restore_precommit_group(
    state: &CapabilityDir,
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    group: &CapabilityDir,
    journal: &QuarantineJournal,
) -> Result<()> {
    for component in journal.components.iter().rev() {
        let (parent, original_name) = open_journal_location(state, journal, component)?;
        restore_component_from_group(group, &parent, &original_name, component)?;
    }

    let remaining =
        bounded_quarantine_group_entries(group, "failed to inspect restored quarantine group")?;
    if remaining != vec![OsString::from(QUARANTINE_JOURNAL_FILE_NAME)] {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().to_path_buf(),
        });
    }
    quarantine
        .remove_tree_entry_verified(group_name, group.identity())
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove restored quarantine group",
                quarantine.display_path().join(group_name),
                source,
            )
        })
}

#[cfg(unix)]
fn restore_component_from_group(
    group: &CapabilityDir,
    parent: &CapabilityDir,
    original_name: &OsStr,
    component: &JournalComponent,
) -> Result<()> {
    restore_component_from_group_with_observer(group, parent, original_name, component, || Ok(()))
}

#[cfg(unix)]
fn restore_component_from_group_with_observer(
    group: &CapabilityDir,
    parent: &CapabilityDir,
    original_name: &OsStr,
    component: &JournalComponent,
    before_isolate: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let staged_name = OsStr::new(component.kind.staged_name());
    let restoration_name = OsStr::new(component.kind.restoration_name());
    let original = parent.entry_identity(original_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect recovery destination",
            parent.display_path().join(original_name),
            source,
        )
    })?;
    let staged = group.entry_identity(staged_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect quarantined recovery source",
            group.display_path().join(staged_name),
            source,
        )
    })?;
    let restoring = group.entry_identity(restoration_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect isolated recovery source",
            group.display_path().join(restoration_name),
            source,
        )
    })?;
    let duplicate_regular_links = original == Some(component.identity)
        && staged.is_none_or(|identity| identity == component.identity)
        && restoring.is_none_or(|identity| identity == component.identity)
        && (staged.is_some() || restoring.is_some());
    if duplicate_regular_links {
        before_isolate()?;
        converge_duplicate_regular_recovery_links(
            group,
            parent,
            original_name,
            component,
            staged_name,
            restoration_name,
        )?;
        return Ok(());
    }
    let duplicate_regular_links_without_destination = original.is_none()
        && staged == Some(component.identity)
        && restoring == Some(component.identity)
        && group.entry_is_regular_file(staged_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect duplicated quarantined recovery source type",
                group.display_path().join(staged_name),
                source,
            )
        })? == Some(true)
        && group
            .entry_is_regular_file(restoration_name)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect duplicated isolated recovery source type",
                    group.display_path().join(restoration_name),
                    source,
                )
            })?
            == Some(true);
    match (original, staged, restoring) {
        (Some(identity), None, None) if identity == component.identity => {
            parent.sync().map_err(|source| {
                DaemonCoreError::io(
                    "failed to synchronize recovered retention destination",
                    parent.display_path(),
                    source,
                )
            })?;
            return Ok(());
        }
        (None, Some(identity), None) if identity == component.identity => {
            before_isolate()?;
            group
                .tombstone_entry_to_verified(staged_name, component.identity, restoration_name)
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to isolate quarantined recovery source",
                        group.display_path().join(staged_name),
                        source,
                    )
                })?;
        }
        (None, None, Some(identity)) if identity == component.identity => {
            before_isolate()?;
        }
        (None, Some(_), Some(_)) if duplicate_regular_links_without_destination => {
            before_isolate()?;
        }
        _ => {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: parent.display_path().join(original_name),
            });
        }
    }

    group
        .rename_to_noreplace(restoration_name, parent, original_name)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to restore quarantined component without replacement",
                parent.display_path().join(original_name),
                source,
            )
        })?;
    if parent.entry_identity(original_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to verify restored quarantined component",
            parent.display_path().join(original_name),
            source,
        )
    })? != Some(component.identity)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: parent.display_path().join(original_name),
        });
    }
    if duplicate_regular_links_without_destination {
        converge_duplicate_regular_recovery_links(
            group,
            parent,
            original_name,
            component,
            staged_name,
            restoration_name,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn converge_duplicate_regular_recovery_links(
    group: &CapabilityDir,
    parent: &CapabilityDir,
    original_name: &OsStr,
    component: &JournalComponent,
    staged_name: &OsStr,
    restoration_name: &OsStr,
) -> Result<()> {
    let original_path = parent.display_path().join(original_name);
    if parent.entry_identity(original_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to revalidate duplicated recovery destination",
            &original_path,
            source,
        )
    })? != Some(component.identity)
        || parent
            .entry_is_regular_file(original_name)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to inspect duplicated recovery destination type",
                    &original_path,
                    source,
                )
            })?
            != Some(true)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: original_path,
        });
    }

    let mut extras = Vec::with_capacity(2);
    for name in [staged_name, restoration_name] {
        match group.entry_identity(name).map_err(|source| {
            DaemonCoreError::io(
                "failed to revalidate duplicated quarantine link",
                group.display_path().join(name),
                source,
            )
        })? {
            None => {}
            Some(identity)
                if identity == component.identity
                    && group.entry_is_regular_file(name).map_err(|source| {
                        DaemonCoreError::io(
                            "failed to inspect duplicated quarantine link type",
                            group.display_path().join(name),
                            source,
                        )
                    })? == Some(true) =>
            {
                extras.push(name);
            }
            Some(_) => {
                return Err(DaemonCoreError::RetentionCandidateChanged {
                    path: group.display_path().join(name),
                });
            }
        }
    }
    if extras.is_empty() {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().to_path_buf(),
        });
    }

    // The destination link must be durable before duplicate quarantine links
    // are removed. A retry can safely repeat this sequence after any failure.
    parent.sync().map_err(|source| {
        DaemonCoreError::io(
            "failed to synchronize duplicated recovery destination",
            parent.display_path(),
            source,
        )
    })?;
    for name in extras {
        if parent.entry_identity(original_name).map_err(|source| {
            DaemonCoreError::io(
                "failed to verify duplicated recovery destination before cleanup",
                &original_path,
                source,
            )
        })? != Some(component.identity)
        {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: original_path,
            });
        }
        group
            .remove_tree_entry_verified(name, component.identity)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to remove duplicated quarantine link",
                    group.display_path().join(name),
                    source,
                )
            })?;
    }
    group.sync().map_err(|source| {
        DaemonCoreError::io(
            "failed to synchronize duplicate quarantine-link cleanup",
            group.display_path(),
            source,
        )
    })?;
    for name in [staged_name, restoration_name] {
        let duplicate_identity = group.entry_identity(name).map_err(|source| {
            DaemonCoreError::io(
                "failed to verify duplicate quarantine-link cleanup",
                group.display_path().join(name),
                source,
            )
        })?;
        if duplicate_identity.is_some() {
            return Err(DaemonCoreError::RetentionCandidateChanged {
                path: group.display_path().join(name),
            });
        }
    }
    if parent.entry_identity(original_name).map_err(|source| {
        DaemonCoreError::io(
            "failed to verify recovered destination after duplicate cleanup",
            &original_path,
            source,
        )
    })? != Some(component.identity)
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: original_path,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn finish_committed_group(
    workspace_root: &Path,
    daemon: &CapabilityDir,
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    group: &CapabilityDir,
    journal: &QuarantineJournal,
    journal_identity: FileIdentity,
) -> Result<RemovalProgress> {
    // Validate the complete quarantine set before the irreversible registry
    // mutation. Partial committed deletion may legitimately have removed a
    // declared component, but unknown entries or replaced identities must
    // fail closed while the registry record is still intact.
    validate_committed_group(group, journal, true, Some(journal_identity))?;
    if !finish_anchored_committed_registry_removal(
        daemon,
        workspace_root,
        &journal.record_values,
        journal.registry_revision,
        journal.registry_checkpoint_generation,
    )? {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: task_registry_path(workspace_root),
        });
    }
    delete_committed_group(quarantine, group_name, group.identity(), journal, true)
}

#[cfg(unix)]
fn finish_finalized_committed_group(
    workspace_root: &Path,
    daemon: &CapabilityDir,
    quarantine: &CapabilityDir,
    group_name: &OsStr,
    group: &CapabilityDir,
    journal: &QuarantineJournal,
    journal_identity: FileIdentity,
) -> Result<()> {
    let final_name = OsStr::new(QUARANTINE_JOURNAL_DELETION_FILE_NAME);
    let entries = bounded_quarantine_group_entries(
        group,
        "failed to enumerate finalized committed retention quarantine",
    )?;
    if entries != [OsString::from(QUARANTINE_JOURNAL_DELETION_FILE_NAME)]
        || journal.phase != QuarantinePhase::Committed
    {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: group.display_path().to_path_buf(),
        });
    }
    authenticate_quarantine_journal(group, final_name, journal, Some(journal_identity))?;
    if !finish_anchored_committed_registry_removal(
        daemon,
        workspace_root,
        &journal.record_values,
        journal.registry_revision,
        journal.registry_checkpoint_generation,
    )? {
        return Err(DaemonCoreError::RetentionCandidateChanged {
            path: task_registry_path(workspace_root),
        });
    }
    authenticate_quarantine_journal(group, final_name, journal, Some(journal_identity))?;
    group
        .remove_tombstone_verified(final_name, journal_identity)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to delete finalized committed quarantine journal",
                group.display_path().join(final_name),
                source,
            )
        })?;
    quarantine
        .remove_empty_dir_verified(group_name, group.identity())
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove finalized committed quarantine group",
                quarantine.display_path().join(group_name),
                source,
            )
        })
}

#[cfg(unix)]
fn open_journal_location(
    state: &CapabilityDir,
    journal: &QuarantineJournal,
    component: &JournalComponent,
) -> Result<(CapabilityDir, OsString)> {
    let relative = component.kind.original_relative_path(&journal.storage_key);
    let original_name = relative
        .file_name()
        .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
            path: state.display_path().join(&relative),
        })?
        .to_os_string();
    let parent_relative =
        relative
            .parent()
            .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                path: state.display_path().join(&relative),
            })?;
    let parent = state.open_relative_dir(parent_relative).map_err(|source| {
        DaemonCoreError::io(
            "failed to open recovery destination capability",
            state.display_path().join(parent_relative),
            source,
        )
    })?;
    Ok((parent, original_name))
}

fn candidate_matches(planned: &Candidate, current: &Candidate) -> bool {
    planned.task_ids == current.task_ids
        && planned.record_values == current.record_values
        && planned.record_logical_bytes == current.record_logical_bytes
        && component_matches(planned.artifact.as_ref(), current.artifact.as_ref())
        && component_matches(planned.event.as_ref(), current.event.as_ref())
}

fn component_matches(
    planned: Option<&ManagedComponent>,
    current: Option<&ManagedComponent>,
) -> bool {
    match (planned, current) {
        (Some(planned), Some(current)) => {
            planned.path == current.path && scan_matches(&planned.scan, &current.scan)
        }
        (None, None) => true,
        _ => false,
    }
}

fn scan_matches(planned: &ScanSummary, current: &ScanSummary) -> bool {
    planned.identity == current.identity
        && planned.logical_bytes == current.logical_bytes
        && planned.allocated_bytes == current.allocated_bytes
        && planned.files == current.files
        && planned.directories == current.directories
        && planned.symlinks == current.symlinks
        && planned.metadata_fingerprint == current.metadata_fingerprint
        && scan_physical_identities_match(planned, current)
}

#[cfg(unix)]
fn scan_physical_identities_match(planned: &ScanSummary, current: &ScanSummary) -> bool {
    planned.physical_identities == current.physical_identities
}

#[cfg(not(unix))]
fn scan_physical_identities_match(_planned: &ScanSummary, _current: &ScanSummary) -> bool {
    true
}

fn push_issue(issues: &mut Vec<TaskStoreIssue>, kind: &str, path: &Path, message: String) {
    push_owned_issue(
        issues,
        TaskStoreIssue {
            kind: kind.to_string(),
            path: diagnostic_path(path),
            message,
        },
    );
}

fn diagnostic_path(path: &Path) -> String {
    let display = path.display().to_string();
    #[cfg(unix)]
    if path.to_str().is_none() {
        use std::os::unix::ffi::OsStrExt as _;

        return format!(
            "{display} [raw-path-digest:{}]",
            blake3::hash(path.as_os_str().as_bytes())
        );
    }
    display
}

fn push_owned_issue(issues: &mut Vec<TaskStoreIssue>, issue: TaskStoreIssue) {
    push_owned_issue_with_limit(issues, issue, MAX_TASK_STORE_ISSUES);
}

fn push_owned_issue_with_limit(
    issues: &mut Vec<TaskStoreIssue>,
    issue: TaskStoreIssue,
    max_issues: usize,
) {
    if max_issues == 0 || issues.len() >= max_issues {
        return;
    }
    if issues.len() + 1 == max_issues
        && !issues
            .iter()
            .any(|existing| existing.kind == ISSUE_BUDGET_EXHAUSTED_KIND)
    {
        issues.push(TaskStoreIssue {
            kind: ISSUE_BUDGET_EXHAUSTED_KIND.to_string(),
            path: issue.path,
            message: format!(
                "additional task-store issues were omitted after the {max_issues}-entry diagnostic bound"
            ),
        });
        return;
    }
    issues.push(issue);
}

fn extend_issues(
    issues: &mut Vec<TaskStoreIssue>,
    additional: impl IntoIterator<Item = TaskStoreIssue>,
) {
    for issue in additional {
        push_owned_issue(issues, issue);
    }
}

fn retention_resource_limit_error(
    operation: &'static str,
    path: &Path,
    message: String,
) -> DaemonCoreError {
    DaemonCoreError::io(
        operation,
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    )
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, FileTimes};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use packet28_daemon_protocol::commands::WatchSpec;
    use packet28_daemon_protocol::paths::{
        active_task_path, ready_path, task_artifact_dir as typed_task_artifact_dir,
        task_event_log_path as typed_task_event_log_path, task_registry_path, watch_registry_path,
        TaskStorageId,
    };
    use packet28_daemon_protocol::task::{
        TaskLifecycle, TaskRecord, TaskRegistry, WatchRegistration, WatchRegistry,
    };
    use tempfile::tempdir;

    #[cfg(unix)]
    use crate::capability;
    use crate::task_store_lease::{
        acquire_daemon_instance_lease, acquire_daemon_task_store_lease,
        acquire_task_store_writer_lease,
    };

    use super::*;

    #[test]
    fn retention_preserves_recovery_artifact_owners_across_two_generations() {
        use packet28_daemon_protocol::task::TaskHistoryRecovery;
        let root = tempdir().unwrap();
        let mut records = ["old", "mid", "new"].map(|task_id| TaskRecord {
            task_id: task_id.to_string(),
            last_completed_at_unix: Some(1),
            ..TaskRecord::default()
        });
        for index in 0..2 {
            let link = TaskHistoryRecovery {
                predecessor_task_id: records[index].task_id.clone(),
                successor_task_id: records[index + 1].task_id.clone(),
                ..TaskHistoryRecovery::default()
            };
            records[index].superseded_by = Some(link.clone());
            records[index + 1].recovered_from = Some(link);
        }
        records[2].lifecycle = TaskLifecycle::Running;
        write_paired_registry(root.path(), records);
        let old = write_artifact(root.path(), "old", b"old evidence", 1);
        let mid = write_artifact(root.path(), "mid", b"mid evidence", 1);
        retain_task_store(
            root.path(),
            100,
            RetentionOptions {
                max_age_seconds: Some(1),
                max_bytes: Some(0),
                apply: true,
            },
        )
        .unwrap();
        assert_eq!(fs::read(old).unwrap(), b"old evidence");
        assert_eq!(fs::read(mid).unwrap(), b"mid evidence");
        assert_eq!(
            crate::storage::load_task_registry(root.path())
                .unwrap()
                .tasks
                .len(),
            3
        );
    }

    fn task_artifact_dir(root: &Path, task_id: &str) -> PathBuf {
        match TaskStorageId::try_from(task_id) {
            Ok(task_id) => typed_task_artifact_dir(root, &task_id),
            Err(_) => task_artifacts_dir(root).join(storage_key_for_task(root, task_id)),
        }
    }

    fn task_event_log_path(root: &Path, task_id: &str) -> PathBuf {
        match TaskStorageId::try_from(task_id) {
            Ok(task_id) => typed_task_event_log_path(root, &task_id),
            Err(_) => task_events_dir(root).join(format!(
                "{}{TASK_EVENT_LOG_SUFFIX}",
                storage_key_for_task(root, task_id)
            )),
        }
    }

    fn write_artifact(root: &Path, task_id: &str, bytes: &[u8], modified_at_unix: u64) -> PathBuf {
        let directory = task_artifact_dir(root, task_id);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("payload.bin");
        fs::write(&path, bytes).unwrap();
        set_modified(&path, modified_at_unix);
        set_modified(&directory, modified_at_unix);
        path
    }

    fn set_modified(path: &Path, timestamp: u64) {
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(timestamp)))
            .unwrap();
    }

    fn write_registry(root: &Path, records: impl IntoIterator<Item = TaskRecord>) {
        let mut registry = TaskRegistry::default();
        for record in records {
            registry.tasks.insert(record.task_id.clone(), record);
        }
        crate::storage::ensure_daemon_dir(root).unwrap();
        #[cfg(unix)]
        {
            let lock_path = daemon_dir(root).join(TASK_REGISTRY_LOCK_FILE_NAME);
            if !lock_path.exists() {
                File::options()
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
                    .unwrap();
                fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        fs::write(
            task_registry_path(root),
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();
    }

    fn write_paired_registry(root: &Path, records: impl IntoIterator<Item = TaskRecord>) {
        let mut registry = TaskRegistry::default();
        for record in records {
            registry.tasks.insert(record.task_id.clone(), record);
        }
        crate::storage::save_task_watch_registry_checkpoint(
            root,
            &registry,
            &WatchRegistry::default(),
        )
        .unwrap();
    }

    fn checkpoint_generation(path: &Path) -> Option<u64> {
        serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap())
            .unwrap()
            .get("task_watch_checkpoint_generation")
            .and_then(serde_json::Value::as_u64)
    }

    fn paired_registry_with_watch(
        mut record: TaskRecord,
        watch_id: &str,
    ) -> (TaskRegistry, WatchRegistry) {
        record.watch_ids = vec![watch_id.to_string()];
        let task_id = record.task_id.clone();
        (
            TaskRegistry {
                tasks: BTreeMap::from([(task_id.clone(), record)]),
            },
            WatchRegistry {
                watches: vec![WatchRegistration {
                    watch_id: watch_id.to_string(),
                    spec: WatchSpec {
                        task_id,
                        ..WatchSpec::default()
                    },
                    ..WatchRegistration::default()
                }],
            },
        )
    }

    #[cfg(unix)]
    fn create_quarantine_group(root: &Path, name: &str) -> (CapabilityDir, CapabilityDir) {
        crate::storage::ensure_daemon_dir(root).unwrap();
        let state = CapabilityDir::open(&root.join(STATE_DIR_NAME)).unwrap();
        let quarantine = state
            .ensure_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700)
            .unwrap();
        let group = quarantine.create_dir(OsStr::new(name), 0o700).unwrap();
        (quarantine, group)
    }

    #[cfg(unix)]
    fn acquire_retention_guards(root: &Path) -> (TaskStoreLease, TaskRetentionAdmission) {
        let lease = try_acquire_task_store_retention_lease(root)
            .unwrap()
            .expect("test workspace has no competing lifecycle owner");
        let admission = try_acquire_task_retention_instance_gate_from(&lease)
            .unwrap()
            .expect("test workspace has no daemon instance owner");
        (lease, admission)
    }

    #[cfg(unix)]
    fn stage_committed_artifact_group(root: &Path, task_id: &str) -> (StagingTransaction, PathBuf) {
        write_artifact(root, task_id, b"payload", 10);
        crate::storage::ensure_daemon_dir(root).unwrap();
        let snapshot = StoreSnapshot::load(root, 100).unwrap();
        let candidate = snapshot.candidate(task_id).unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let staged = transaction
            .group
            .display_path()
            .join(JournalComponentKind::Artifacts.staged_name());
        (transaction, staged)
    }

    #[cfg(unix)]
    fn finish_committed_recovery_in_tiny_batches(root: &Path) -> (usize, bool) {
        INJECT_COMMITTED_DELETION_BATCH_ENTRIES.with(|configured| configured.set(Some(2)));
        let mut saw_pending = false;
        for pass in 1..=256 {
            let report = recover_task_store_quarantine(root).unwrap();
            assert_eq!(report.conflicted_groups, 0);
            if report.completed_committed_groups == 1 {
                INJECT_COMMITTED_DELETION_BATCH_ENTRIES.with(|configured| configured.set(None));
                return (pass, saw_pending);
            }
            assert_eq!(report.completed_committed_groups, 0);
            saw_pending |= report
                .issues
                .iter()
                .any(|issue| issue.kind == "retention_recovery_deletion_pending");
        }
        INJECT_COMMITTED_DELETION_BATCH_ENTRIES.with(|configured| configured.set(None));
        panic!("committed recovery did not finish within the supported test passes");
    }

    fn inactive_record(task_id: &str, timestamp: u64) -> TaskRecord {
        TaskRecord {
            task_id: task_id.to_string(),
            lifecycle: TaskLifecycle::Idle,
            last_completed_at_unix: Some(timestamp),
            ..TaskRecord::default()
        }
    }

    #[test]
    fn snapshot_entry_bound_fails_closed_before_returning_partial_metrics() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "bounded", b"payload", 10);
        let limits = ScanLimits {
            max_depth: 64,
            max_entries_per_traversal: 1,
            max_entries_per_managed_root: 64,
        };

        let error = StoreSnapshot::load_with_limits(root.path(), 100, limits).unwrap_err();

        assert!(error.to_string().contains("entry bound"));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
    }

    #[test]
    fn managed_root_entry_bound_accepts_exact_limit_and_rejects_one_over() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "one", b"1", 10);
        write_artifact(root.path(), "two", b"2", 20);
        let limits = ScanLimits {
            max_depth: 64,
            max_entries_per_traversal: 64,
            max_entries_per_managed_root: 2,
        };

        assert!(StoreSnapshot::load_with_limits(root.path(), 100, limits).is_ok());

        let excess = write_artifact(root.path(), "three", b"3", 30);
        let error = StoreSnapshot::load_with_limits(root.path(), 100, limits).unwrap_err();

        assert!(error.to_string().contains("managed-root enumeration"));
        assert_eq!(fs::read(excess).unwrap(), b"3");
    }

    #[test]
    fn issue_budget_reserves_a_machine_readable_truncation_sentinel() {
        let mut issues = Vec::new();
        for index in 0..5 {
            push_owned_issue_with_limit(
                &mut issues,
                TaskStoreIssue {
                    kind: format!("issue-{index}"),
                    path: format!("/tmp/{index}"),
                    message: format!("message {index}"),
                },
                3,
            );
        }

        assert_eq!(issues.len(), 3);
        assert_eq!(
            issues
                .iter()
                .filter(|issue| issue.kind == ISSUE_BUDGET_EXHAUSTED_KIND)
                .count(),
            1
        );
    }

    #[test]
    fn logical_byte_aggregation_saturates_instead_of_wrapping() {
        assert_eq!(saturating_sum_u64([u64::MAX, 1]), u64::MAX);
    }

    #[cfg(unix)]
    fn allocated_bytes(path: &Path) -> u64 {
        let metadata = fs::symlink_metadata(path).unwrap();
        let own_bytes = metadata.blocks().saturating_mul(512);
        if !metadata.is_dir() {
            return own_bytes;
        }
        fs::read_dir(path)
            .unwrap()
            .map(|entry| allocated_bytes(&entry.unwrap().path()))
            .fold(own_bytes, u64::saturating_add)
    }

    #[cfg(unix)]
    #[test]
    fn inspection_reports_native_allocated_bytes_for_each_managed_scope() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"payload", 10);
        let state_root = root.path().join(STATE_DIR_NAME);
        let artifact_root = task_artifacts_dir(root.path());

        let report = inspect_task_store(root.path(), 100).unwrap();

        assert!(report.metrics_before.allocated_bytes_supported);
        assert_eq!(
            report.metrics_before.state_allocated_bytes,
            allocated_bytes(&state_root)
        );
        assert_eq!(
            report.metrics_before.task_artifact_allocated_bytes,
            allocated_bytes(&artifact_root)
        );
        assert_eq!(
            report.metrics_before.managed_task_allocated_bytes,
            report.metrics_before.task_artifact_allocated_bytes
        );
    }

    #[test]
    fn schema_v1_reports_deserialize_with_safe_defaults_for_v2_fields() {
        let root = tempdir().unwrap();
        let report = inspect_task_store(root.path(), 100).unwrap();
        let mut value = serde_json::to_value(report).unwrap();
        value["schema_version"] = serde_json::json!(1);
        for metrics_name in ["metrics_before", "metrics_after"] {
            let metrics = value[metrics_name].as_object_mut().unwrap();
            metrics.remove("retention_quarantine_logical_bytes");
            metrics.remove("retention_quarantine_allocated_bytes");
            metrics.remove("retention_quarantine_groups");
        }
        let accounting = value["retention"].as_object_mut().unwrap();
        for field in [
            "failed_tasks",
            "failed_logical_bytes",
            "recovered_precommit_groups",
            "recovered_committed_groups",
            "recovery_conflicted_groups",
            "final_rescan_reliable",
            "action_byte_accounting_reliable",
        ] {
            accounting.remove(field);
        }

        let decoded: TaskStoreReport = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.schema_version, 1);
        assert_eq!(
            (
                decoded.metrics_before.retention_quarantine_logical_bytes,
                decoded.metrics_before.retention_quarantine_groups,
                decoded.retention.failed_tasks,
                decoded.retention.recovered_precommit_groups,
                decoded.retention.final_rescan_reliable,
                decoded.retention.action_byte_accounting_reliable,
            ),
            (0, 0, 0, 0, false, false)
        );
    }

    #[test]
    fn inspection_of_fresh_workspace_does_not_initialize_task_store_state() {
        let root = tempdir().unwrap();
        let state = root.path().join(STATE_DIR_NAME);

        let report = inspect_task_store(root.path(), 100).unwrap();

        assert_eq!(report.mode, RetentionMode::Inspect);
        assert!(report.metrics_before.task_registry_reliable);
        assert_eq!(report.metrics_before.task_registry_records, 0);
        assert_eq!(report.metrics_before.active_tasks, 0);
        assert!(report.issues.is_empty());
        assert!(!state.exists());
    }

    #[cfg(unix)]
    #[test]
    fn inspection_does_not_create_a_missing_registry_authority_lock() {
        let root = tempdir().unwrap();
        let state = root.path().join(STATE_DIR_NAME);
        let daemon = state.join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&daemon, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = daemon.join(TASK_REGISTRY_LOCK_FILE_NAME);

        let report = inspect_task_store(root.path(), 100).unwrap();

        assert!(report.metrics_before.task_registry_reliable);
        assert_eq!(report.metrics_before.task_registry_records, 0);
        assert_eq!(report.metrics_before.active_tasks, 0);
        assert!(report.issues.is_empty());
        assert!(!lock.exists());
        assert!(!task_registry_path(root.path()).exists());
        assert!(!state.join("agent").exists());
    }

    #[cfg(not(unix))]
    #[test]
    fn inspection_marks_allocated_byte_fallback_as_logical() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"payload", 10);

        let report = inspect_task_store(root.path(), 100).unwrap();

        assert!(!report.metrics_before.allocated_bytes_supported);
        assert_eq!(
            report.metrics_before.state_allocated_bytes,
            report.metrics_before.state_logical_bytes
        );
        assert_eq!(
            report.metrics_before.managed_task_allocated_bytes,
            report.metrics_before.managed_task_logical_bytes
        );
    }

    #[test]
    fn dry_run_is_non_mutating_by_default() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "old-task", b"payload", 10);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(20), None)).unwrap();

        assert_eq!(
            (
                report.mode,
                report.retention.planned_tasks,
                report.retention.removed_tasks,
                artifact.exists(),
                report.metrics_after == report.metrics_before,
            ),
            (RetentionMode::DryRun, 1, 0, true, true)
        );
    }

    #[test]
    fn age_limit_retains_exact_boundary_and_selects_strictly_older_task() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "at-boundary", b"a", 90);
        write_artifact(root.path(), "older", b"b", 89);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(10), None)).unwrap();

        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| action.storage_key.as_str())
                .collect::<Vec<_>>(),
            vec!["older"]
        );
    }

    #[test]
    fn age_limit_normalizes_historical_millisecond_record_timestamps() {
        let root = tempdir().unwrap();
        write_registry(
            root.path(),
            [
                inactive_record("at-boundary", 1_799_999_990_000),
                inactive_record("older", 1_799_999_989_000),
            ],
        );

        let report = retain_task_store(
            root.path(),
            1_800_000_000,
            RetentionOptions::dry_run(Some(10), None),
        )
        .unwrap();

        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| action.storage_key.as_str())
                .collect::<Vec<_>>(),
            vec!["older"]
        );
        assert_eq!(
            report.metrics_before.newest_task_timestamp_unix,
            Some(1_799_999_990)
        );
    }

    #[cfg(unix)]
    #[test]
    fn wal_only_task_participates_in_retention_planning() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry::default(),
            &WatchRegistry::default(),
        )
        .unwrap();
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default()
                .upsert_task(inactive_record("wal-only", 10)),
        )
        .unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(20), None)).unwrap();

        assert_eq!(report.retention.planned_tasks, 1);
        assert_eq!(report.actions[0].storage_key, "wal-only");
        assert_eq!(report.metrics_before.task_registry_records, 1);
        assert!(report.metrics_before.task_registry_reliable);
    }

    #[cfg(unix)]
    #[test]
    fn wal_updated_recency_prevents_stale_checkpoint_eviction() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry {
                tasks: BTreeMap::from([("updated".to_string(), inactive_record("updated", 10))]),
            },
            &WatchRegistry::default(),
        )
        .unwrap();
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default()
                .upsert_task(inactive_record("updated", 95)),
        )
        .unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(20), None)).unwrap();

        assert_eq!(report.retention.planned_tasks, 0);
        assert_eq!(report.metrics_before.newest_task_timestamp_unix, Some(95));
        assert!(report.metrics_before.task_registry_reliable);
    }

    #[cfg(unix)]
    #[test]
    fn retention_inspection_does_not_repair_torn_wal_under_shared_lock() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry {
                tasks: BTreeMap::from([(
                    "protected".to_string(),
                    inactive_record("protected", 10),
                )]),
            },
            &WatchRegistry::default(),
        )
        .unwrap();
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default()
                .upsert_task(inactive_record("protected", 95)),
        )
        .unwrap();
        let wal_path = crate::storage::registry_delta_wal_path(root.path());
        let complete_len = fs::metadata(&wal_path).unwrap().len();
        File::options()
            .write(true)
            .open(&wal_path)
            .unwrap()
            .set_len(complete_len - 1)
            .unwrap();
        let torn_bytes = fs::read(&wal_path).unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(20), None)).unwrap();

        assert_eq!(fs::read(&wal_path).unwrap(), torn_bytes);
        assert!(!report.metrics_before.task_registry_reliable);
        assert_eq!(report.retention.planned_tasks, 0);
        assert_eq!(report.retention.protected_tasks, 1);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_unreadable"));
    }

    #[cfg(unix)]
    #[test]
    fn wal_allocation_participates_in_managed_retention_accounting() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry::default(),
            &WatchRegistry::default(),
        )
        .unwrap();
        let mut record = inactive_record("large-wal", 10);
        record.last_error = Some("x".repeat(128 * 1024));
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default().upsert_task(record),
        )
        .unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(20), None).apply(),
        )
        .unwrap();

        assert_eq!(report.retention.removed_tasks, 1);
        assert!(report.metrics_before.task_registry_file_bytes > 128 * 1024);
        assert!(
            report.metrics_before.managed_task_allocated_bytes
                >= report.metrics_before.task_registry_allocated_bytes
        );
        assert!(
            report.metrics_before.task_registry_allocated_bytes
                > report.metrics_after.task_registry_allocated_bytes
        );
        assert_eq!(
            report.metrics_after.task_registry_file_bytes,
            fs::metadata(task_registry_path(root.path())).unwrap().len()
                + crate::storage::REGISTRY_DELTA_WAL_HEADER_BYTES as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn wal_append_during_revalidation_is_serialized_after_cleanup() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry {
                tasks: BTreeMap::from([("wal-race".to_string(), inactive_record("wal-race", 10))]),
            },
            &WatchRegistry::default(),
        )
        .unwrap();
        write_artifact(root.path(), "wal-race", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("wal-race").unwrap().clone();
        let cleanup_root = root.path().to_path_buf();
        let (staged_tx, staged_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            let lease = try_acquire_task_store_retention_lease(&cleanup_root)
                .unwrap()
                .unwrap();
            let outcome = apply_candidate_with_observers(
                &snapshot,
                &candidate,
                || {
                    staged_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                    Ok(())
                },
                || Ok(()),
                || Ok(()),
            );
            drop(lease);
            outcome
        });
        staged_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let writer_root = root.path().to_path_buf();
        let (written_tx, written_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            crate::storage::append_task_watch_registry_delta(
                &writer_root,
                crate::storage::RegistryRevisionRange::single(
                    crate::storage::RegistryRevision::new(1),
                )
                .unwrap(),
                &crate::storage::RegistryDeltaBatch::default()
                    .upsert_task(inactive_record("wal-race", 101)),
            )
            .unwrap();
            write_artifact(&writer_root, "wal-race", b"new", 101);
            written_tx.send(()).unwrap();
        });
        assert!(matches!(
            written_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        continue_tx.send(()).unwrap();
        assert_eq!(cleanup.join().unwrap().unwrap(), RetentionOutcome::Removed);
        written_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.join().unwrap();

        let registry = crate::storage::load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert_eq!(
            registry.tasks.tasks["wal-race"].last_completed_at_unix,
            Some(101)
        );
        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "wal-race").join("payload.bin")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn mixed_timestamp_units_do_not_falsely_mark_completed_agent_active() {
        let root = tempdir().unwrap();
        write_registry(
            root.path(),
            [TaskRecord {
                task_id: "completed".to_string(),
                lifecycle: TaskLifecycle::Idle,
                latest_agent_started_at_unix: Some(1_800_000_000_000),
                latest_agent_completed_at_unix: Some(1_800_000_001),
                ..TaskRecord::default()
            }],
        );

        let report = retain_task_store(
            root.path(),
            1_800_000_100,
            RetentionOptions::dry_run(Some(1), None),
        )
        .unwrap();

        assert_eq!(
            (
                report.metrics_before.active_tasks,
                report.retention.protected_tasks,
                report.retention.planned_tasks,
            ),
            (0, 0, 1)
        );
    }

    #[test]
    fn byte_limit_retains_exact_boundary() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"four", 10);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(4))).unwrap();

        assert_eq!(report.retention.planned_tasks, 0);
    }

    #[test]
    fn byte_limit_selects_oldest_candidate_when_one_byte_over() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"four", 10);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(3))).unwrap();

        assert_eq!(
            (
                report.retention.planned_tasks,
                report.retention.planned_logical_bytes,
                report.retention.remaining_managed_logical_bytes,
            ),
            (1, 4, 0)
        );
    }

    #[test]
    fn active_registry_task_is_protected() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "active", b"payload", 10);
        write_registry(
            root.path(),
            [TaskRecord {
                task_id: "active".to_string(),
                lifecycle: TaskLifecycle::Running,
                last_started_at_unix: Some(10),
                ..TaskRecord::default()
            }],
        );

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)),
        )
        .unwrap();

        assert_eq!(
            (
                report.retention.planned_tasks,
                report.retention.protected_tasks,
                report.metrics_before.active_tasks,
                report.retention.remaining_over_limit_bytes,
            ),
            (0, 1, 1, report.metrics_before.managed_task_logical_bytes,)
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_mismatched_registry_identifier_is_protected_as_corruption() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        let invalid = TaskRegistry {
            tasks: BTreeMap::from([(
                "registry-key".to_string(),
                TaskRecord {
                    task_id: "embedded-id".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        let bytes = serde_json::to_vec_pretty(&invalid).unwrap();
        fs::write(&path, &bytes).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert!(report.actions.is_empty());
        assert_eq!(
            report.retention.remaining_managed_logical_bytes,
            bytes.len() as u64
        );
        assert_eq!(
            report.retention.remaining_over_limit_bytes,
            bytes.len() as u64
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_corrupt"));
    }

    #[cfg(unix)]
    #[test]
    fn one_legacy_invalid_registry_id_protects_the_entire_store() {
        let root = tempdir().unwrap();
        let aliased = write_artifact(root.path(), "live", b"aliased", 10);
        let unrelated = write_artifact(root.path(), "unrelated", b"unrelated", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let registry_path = task_registry_path(root.path());
        let registry = br#"{"tasks":{"LIVE":{"task_id":"LIVE","last_completed_at_unix":10}}}"#;
        fs::write(&registry_path, registry).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)).apply(),
        )
        .unwrap();

        assert!(!report.metrics_before.task_registry_reliable);
        assert!(report.actions.is_empty());
        assert_eq!(report.retention.planned_tasks, 0);
        assert_eq!(report.retention.protected_tasks, 2);
        assert_eq!(fs::read(aliased).unwrap(), b"aliased");
        assert_eq!(fs::read(unrelated).unwrap(), b"unrelated");
        assert_eq!(fs::read(registry_path).unwrap(), registry);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_corrupt"));
    }

    #[test]
    fn unregistered_active_task_is_inconsistent_and_protects_all_candidates() {
        let root = tempdir().unwrap();
        let pointer = write_artifact(root.path(), "pointer-task", b"payload", 10);
        let unrelated = write_artifact(root.path(), "unrelated-task", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "pointer-task".to_string(),
                session_id: None,
                updated_at_unix: 10,
            })
            .unwrap(),
        )
        .unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(report.metrics_before.active_tasks, 0);
        assert_eq!(report.retention.planned_tasks, 0);
        assert_eq!(report.retention.protected_tasks, 2);
        assert!(pointer.exists());
        assert!(unrelated.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "active_task_registry_inconsistent"));
    }

    #[test]
    fn legacy_nonportable_active_pointer_fails_closed_and_protects_all_candidates() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), " live ", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: " live ".to_string(),
                session_id: None,
                updated_at_unix: 10,
            })
            .unwrap(),
        )
        .unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(report.metrics_before.active_tasks, 0);
        assert_eq!(report.retention.planned_tasks, 0);
        assert_eq!(report.retention.protected_tasks, 1);
        assert!(artifact.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "active_task_corrupt"));
    }

    #[cfg(unix)]
    #[test]
    fn multiply_linked_files_protect_every_candidate_that_reaches_the_inode() {
        let root = tempdir().unwrap();
        let first = write_artifact(root.path(), "hardlink-a", b"payload", 10);
        let second = write_artifact(root.path(), "hardlink-b", b"replace", 10);
        fs::remove_file(&second).unwrap();
        fs::hard_link(&first, &second).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)).apply(),
        )
        .unwrap();

        assert!(report.actions.is_empty());
        assert_eq!(report.retention.protected_tasks, 2);
        assert!(first.exists());
        assert!(second.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "hardlink_entry"));
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_created_after_snapshot_fails_revalidation_without_staging() {
        let root = tempdir().unwrap();
        let payload = write_artifact(root.path(), "hardlink-race", b"payload", 10);
        write_registry(root.path(), [inactive_record("hardlink-race", 10)]);
        let registry_path = task_registry_path(root.path());
        let registry_before = fs::read(&registry_path).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("hardlink-race").unwrap();
        let alias = root.path().join("outside-link");
        fs::hard_link(&payload, &alias).unwrap();

        let error = apply_candidate(&snapshot, candidate).unwrap_err();

        assert!(matches!(
            error.error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(fs::read(&payload).unwrap(), b"payload");
        assert_eq!(fs::read(&alias).unwrap(), b"payload");
        assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[test]
    fn oversized_active_task_record_fails_closed_without_being_read() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "protected", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(&path)
            .unwrap()
            .set_len(MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1)
            .unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(artifact.exists());
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "active_task_unreadable" && issue.message.contains("read bound")
        }));
    }

    #[test]
    fn historical_overlong_active_task_identifier_fails_closed() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "protected", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "a".repeat(crate::storage::MAX_TASK_STORAGE_KEY_BYTES + 1),
                session_id: None,
                updated_at_unix: 10,
            })
            .unwrap(),
        )
        .unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(artifact.exists());
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "active_task_corrupt" && issue.message.contains("maximum supported size")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn active_task_capability_rejects_a_symlink_swap_without_following() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "protected", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "protected".to_string(),
                session_id: None,
                updated_at_unix: 10,
            })
            .unwrap(),
        )
        .unwrap();
        let held = path.with_extension("held");
        let outside = tempdir().unwrap();
        let outside_path = outside.path().join("active-task.json");
        let outside_bytes = serde_json::to_vec(&ActiveTaskRecord {
            task_id: "outside".to_string(),
            session_id: None,
            updated_at_unix: 10,
        })
        .unwrap();
        fs::write(&outside_path, &outside_bytes).unwrap();
        let path_for_swap = path;
        inject_active_task_before_capability_read_once(move || {
            fs::rename(&path_for_swap, &held).unwrap();
            symlink(&outside_path, &path_for_swap).unwrap();
        });

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(report.metrics_before.active_tasks, 0);
        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(artifact.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "active_task_unreadable"));
    }

    #[cfg(unix)]
    #[test]
    fn active_task_growth_before_open_is_bounded_and_fails_closed() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "protected", b"payload", 10);
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}").unwrap();
        let path_for_growth = path;
        inject_active_task_before_capability_read_once(move || {
            File::options()
                .write(true)
                .open(path_for_growth)
                .unwrap()
                .set_len(MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1)
                .unwrap();
        });

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(artifact.exists());
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "active_task_unreadable" && issue.message.contains("exceeds")
        }));
    }

    #[test]
    fn dry_run_accounting_sums_selected_candidate_bytes() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "first", b"one", 10);
        write_artifact(root.path(), "second", b"12345", 20);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            (
                report.retention.planned_tasks,
                report.retention.planned_logical_bytes,
                report.metrics_before.managed_task_logical_bytes,
            ),
            (2, 8, 8)
        );
    }

    #[test]
    fn planned_actions_are_reported_oldest_first() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "z-oldest", b"one", 10);
        write_artifact(root.path(), "a-newest", b"two", 20);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| action.storage_key.as_str())
                .collect::<Vec<_>>(),
            vec!["z-oldest", "a-newest"]
        );
    }

    #[test]
    fn size_plans_order_unknown_timestamps_after_known_ages() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "z-known", b"one", 10);
        write_registry(
            root.path(),
            [TaskRecord {
                task_id: "a-unknown".to_string(),
                lifecycle: TaskLifecycle::Idle,
                ..TaskRecord::default()
            }],
        );

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| action.storage_key.as_str())
                .collect::<Vec<_>>(),
            vec!["z-known", "a-unknown"]
        );
    }

    #[test]
    fn corrupt_registry_protects_all_candidates() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"payload", 10);
        let path = task_registry_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "{not-json").unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)),
        )
        .unwrap();

        assert_eq!(
            (
                report.metrics_before.task_registry_reliable,
                report.retention.planned_tasks,
                report.retention.protected_tasks,
            ),
            (false, 0, 1)
        );
    }

    #[test]
    fn duplicate_registry_keys_fail_closed_during_inspection() {
        let record =
            serde_json::to_string(&inactive_record("duplicate", 10)).expect("record encodes");
        let duplicate_top_level =
            format!(r#"{{"tasks":{{"duplicate":{record}}},"tasks":{{"duplicate":{record}}}}}"#);
        let duplicate_task =
            format!(r#"{{"tasks":{{"duplicate":{record},"duplicate":{record}}}}}"#);
        let duplicate_nested_task_id = format!(
            r#"{{"tasks":{{"duplicate":{{"task_id":"duplicate",{}}}}}}}"#,
            &record[1..]
        );
        let duplicate_nested_unknown = format!(
            r#"{{"tasks":{{"duplicate":{{"future":1,"future":2,{}}}}}}}"#,
            &record[1..]
        );

        for raw in [
            duplicate_top_level,
            duplicate_task,
            duplicate_nested_task_id,
            duplicate_nested_unknown,
        ] {
            let root = tempdir().unwrap();
            let artifact = write_artifact(root.path(), "duplicate", b"payload", 10);
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            fs::write(task_registry_path(root.path()), raw.as_bytes()).unwrap();

            let report =
                retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0)))
                    .unwrap();

            assert_eq!(report.retention.planned_tasks, 0);
            assert_eq!(report.retention.protected_tasks, 1);
            assert_eq!(
                fs::read(task_registry_path(root.path())).unwrap(),
                raw.as_bytes()
            );
            assert!(artifact.exists());
            assert!(report.issues.iter().any(|issue| {
                issue.kind == "registry_corrupt" && issue.message.contains("duplicate")
            }));
        }
    }

    #[test]
    fn corrupt_registry_raw_bytes_remain_in_protected_size_accounting() {
        let root = tempdir().unwrap();
        let path = task_registry_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = b"{not-json";
        fs::write(&path, raw).unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            (
                report.metrics_before.task_registry_reliable,
                report.metrics_before.managed_task_logical_bytes,
                report.retention.protected_logical_bytes,
                report.retention.planned_tasks,
                report.retention.remaining_over_limit_bytes,
            ),
            (
                false,
                raw.len() as u64,
                raw.len() as u64,
                0,
                raw.len() as u64
            )
        );
    }

    #[test]
    fn oversized_registry_is_bounded_and_counted_as_protected_raw_state() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        File::create(&path)
            .unwrap()
            .set_len(MAX_TASK_REGISTRY_BYTES as u64 + 1)
            .unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert!(!report.metrics_before.task_registry_reliable);
        assert_eq!(
            report.metrics_before.task_registry_file_bytes,
            MAX_TASK_REGISTRY_BYTES as u64 + 1
        );
        assert_eq!(
            report.retention.protected_logical_bytes,
            MAX_TASK_REGISTRY_BYTES as u64 + 1
        );
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "registry_unreadable" && issue.message.contains("read bound")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn registry_capability_rejects_a_symlink_swap_without_following() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("inside", 10)]);
        let path = task_registry_path(root.path());
        let held = path.with_extension("held");
        let outside = tempdir().unwrap();
        let outside_path = outside.path().join(TASK_REGISTRY_FILE_NAME);
        let outside_registry = TaskRegistry {
            tasks: BTreeMap::from([("outside".to_string(), inactive_record("outside", 10))]),
        };
        let outside_bytes = serde_json::to_vec_pretty(&outside_registry).unwrap();
        fs::write(&outside_path, &outside_bytes).unwrap();
        let path_for_swap = path;
        inject_registry_before_capability_read_once(move || {
            fs::rename(&path_for_swap, &held).unwrap();
            symlink(&outside_path, &path_for_swap).unwrap();
        });

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert!(!report.metrics_before.task_registry_reliable);
        assert_eq!(report.metrics_before.task_registry_records, 0);
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_unsafe"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_growth_before_open_is_bounded_and_counted_as_protected() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("inside", 10)]);
        let path = task_registry_path(root.path());
        let path_for_growth = path;
        inject_registry_before_capability_read_once(move || {
            File::options()
                .write(true)
                .open(path_for_growth)
                .unwrap()
                .set_len(MAX_TASK_REGISTRY_BYTES as u64 + 1)
                .unwrap();
        });

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert!(!report.metrics_before.task_registry_reliable);
        assert_eq!(
            report.metrics_before.task_registry_file_bytes,
            MAX_TASK_REGISTRY_BYTES as u64 + 1
        );
        assert_eq!(
            report.retention.protected_logical_bytes,
            MAX_TASK_REGISTRY_BYTES as u64 + 1
        );
        assert_eq!(report.retention.planned_tasks, 0);
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "registry_unreadable" && issue.message.contains("exceeds")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn retention_registry_snapshot_waits_for_an_existing_writer_lock() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("inside", 10)]);
        let lock_path = daemon_dir(root.path()).join(TASK_REGISTRY_LOCK_FILE_NAME);
        let lock = File::options()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let root_path = root.path().to_path_buf();
        let (completed_tx, completed_rx) = mpsc::channel();
        let inspection = thread::spawn(move || {
            completed_tx
                .send(inspect_task_store(&root_path, 100))
                .unwrap();
        });

        assert!(matches!(
            completed_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        FileExt::unlock(&lock).unwrap();
        completed_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        inspection.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn registry_shared_lock_covers_the_active_task_authority_read() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("inside", 10)]);
        crate::storage::save_active_task_record(
            root.path(),
            &ActiveTaskRecord {
                task_id: "inside".to_string(),
                session_id: None,
                updated_at_unix: 10,
            },
        )
        .unwrap();

        let daemon_path = daemon_dir(root.path());
        let (start_tx, start_rx) = mpsc::channel();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let daemon = CapabilityDir::open(&daemon_path).unwrap();
            let lock = daemon
                .open_existing_lock_file(OsStr::new(TASK_REGISTRY_LOCK_FILE_NAME))
                .unwrap()
                .unwrap();
            start_rx.recv().unwrap();
            attempted_tx.send(()).unwrap();
            FileExt::lock_exclusive(&lock).unwrap();
            let _ = acquired_tx.send(());
            FileExt::unlock(&lock).unwrap();
        });

        inject_active_task_before_capability_read_once(move || {
            start_tx.send(()).unwrap();
            attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
        });

        let report = inspect_task_store(root.path(), 100).unwrap();
        assert_eq!(report.metrics_before.active_tasks, 1);
        assert!(!report
            .issues
            .iter()
            .any(|issue| issue.kind == "active_task_registry_inconsistent"));
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unauthentic_writable_registry_raw_bytes_remain_protected() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let path = task_registry_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = b"{\"schema_version\":1,\"tasks\":{}}";
        fs::write(&path, raw).unwrap();
        let original_mode = fs::metadata(&path).unwrap().permissions().mode();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let result = retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0)));
        fs::set_permissions(&path, fs::Permissions::from_mode(original_mode)).unwrap();
        let report = result.unwrap();

        assert_eq!(
            (
                report.metrics_before.task_registry_reliable,
                report.metrics_before.managed_task_logical_bytes,
                report.retention.protected_logical_bytes,
                report.retention.planned_tasks,
                report.retention.remaining_over_limit_bytes,
            ),
            (
                false,
                raw.len() as u64,
                raw.len() as u64,
                0,
                raw.len() as u64
            )
        );
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_unreadable"));
    }

    #[test]
    fn unreadable_registry_shape_is_reported_and_protected() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "task", b"payload", 10);
        fs::create_dir_all(task_registry_path(root.path())).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), Some(0)),
        )
        .unwrap();

        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "registry_unsafe"));
        assert_eq!(report.retention.protected_tasks, 1);
    }

    // APFS rejects ill-formed UTF-8 names with EILSEQ; Linux filesystems
    // permit creating the opaque entry needed for this regression.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf_artifact_is_an_opaque_protected_candidate_in_both_modes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = tempdir().unwrap();
        let artifact_root = task_artifacts_dir(root.path());
        fs::create_dir_all(&artifact_root).unwrap();
        let opaque_dir = artifact_root.join(OsString::from_vec(b"task-\xff".to_vec()));
        fs::create_dir(&opaque_dir).unwrap();
        let payload = opaque_dir.join("payload.bin");
        fs::write(&payload, b"opaque").unwrap();

        let dry =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();
        let apply = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(
            (
                dry.metrics_before.managed_task_logical_bytes,
                dry.retention.protected_tasks,
                dry.retention.protected_logical_bytes,
                dry.retention.planned_tasks,
                dry.retention.remaining_over_limit_bytes,
            ),
            (6, 1, 6, 0, 6)
        );
        assert_eq!(
            (
                apply.metrics_before.managed_task_logical_bytes,
                apply.metrics_before.managed_task_allocated_bytes,
                apply.metrics_before.task_event_files,
                apply.metrics_before.retention_quarantine_groups,
            ),
            (
                dry.metrics_before.managed_task_logical_bytes,
                dry.metrics_before.managed_task_allocated_bytes,
                dry.metrics_before.task_event_files,
                dry.metrics_before.retention_quarantine_groups,
            )
        );
        assert_eq!(
            (
                apply.retention.protected_tasks,
                apply.retention.protected_logical_bytes,
                apply.retention.planned_tasks,
                apply.retention.removed_tasks,
                apply.retention.remaining_over_limit_bytes,
            ),
            (1, 6, 0, 0, 6)
        );
        assert!(payload.exists());
        assert!(apply
            .issues
            .iter()
            .any(|issue| issue.kind == "artifact_name_invalid"));
    }

    #[test]
    fn noncanonical_on_disk_artifact_spellings_are_opaque_and_protected() {
        for storage_key in [
            "LIVE".to_string(),
            "con".to_string(),
            "a".repeat(crate::storage::MAX_TASK_STORAGE_KEY_BYTES + 1),
        ] {
            let root = tempdir().unwrap();
            let artifact_dir = task_artifacts_dir(root.path()).join(&storage_key);
            fs::create_dir_all(&artifact_dir).unwrap();
            let payload = artifact_dir.join("payload.bin");
            fs::write(&payload, b"historical").unwrap();
            set_modified(&payload, 10);
            set_modified(&artifact_dir, 10);
            write_registry(root.path(), []);

            let report = retain_task_store(
                root.path(),
                100,
                RetentionOptions::dry_run(Some(1), Some(0)).apply(),
            )
            .unwrap();

            assert!(
                payload.exists(),
                "noncanonical spelling {storage_key:?} was removed"
            );
            assert_eq!(report.retention.protected_tasks, 1);
            assert_eq!(report.retention.planned_tasks, 0);
            assert!(report
                .actions
                .iter()
                .all(|action| action.outcome != RetentionOutcome::Removed));
            assert!(report
                .issues
                .iter()
                .any(|issue| issue.kind == "artifact_name_invalid"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_event_names_are_protected_with_dry_run_apply_parity() {
        let root = tempdir().unwrap();
        let event_root = task_events_dir(root.path());
        fs::create_dir_all(&event_root).unwrap();
        let empty_key = event_root.join(".events.jsonl");
        let current_dir_key = event_root.join("..events.jsonl");
        fs::write(&empty_key, b"empty").unwrap();
        fs::write(&current_dir_key, b"dot").unwrap();

        let dry =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();
        let apply = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(
            (
                dry.metrics_before.managed_task_logical_bytes,
                dry.retention.protected_tasks,
                dry.retention.protected_logical_bytes,
                dry.retention.planned_tasks,
                dry.retention.remaining_over_limit_bytes,
            ),
            (8, 2, 8, 0, 8)
        );
        assert_eq!(
            (
                apply.metrics_before.managed_task_logical_bytes,
                apply.metrics_before.managed_task_allocated_bytes,
                apply.metrics_before.task_event_files,
                apply.metrics_before.retention_quarantine_groups,
            ),
            (
                dry.metrics_before.managed_task_logical_bytes,
                dry.metrics_before.managed_task_allocated_bytes,
                dry.metrics_before.task_event_files,
                dry.metrics_before.retention_quarantine_groups,
            )
        );
        assert_eq!(
            (
                apply.retention.protected_tasks,
                apply.retention.protected_logical_bytes,
                apply.retention.planned_tasks,
                apply.retention.removed_tasks,
                apply.retention.remaining_over_limit_bytes,
            ),
            (2, 8, 0, 0, 8)
        );
        assert!(empty_key.exists());
        assert!(current_dir_key.exists());
        assert_eq!(
            apply
                .issues
                .iter()
                .filter(|issue| issue.kind == "event_name_invalid")
                .count(),
            2
        );
    }

    #[test]
    fn malformed_event_name_cannot_overwrite_a_valid_candidate_key() {
        let root = tempdir().unwrap();
        let event_root = task_events_dir(root.path());
        fs::create_dir_all(&event_root).unwrap();
        let valid = event_root.join("same.events.jsonl");
        let malformed = event_root.join("same");
        fs::write(&valid, b"valid").unwrap();
        fs::write(&malformed, b"malformed").unwrap();
        set_modified(&valid, 10);
        set_modified(&malformed, 10);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            report.metrics_before.task_event_logical_bytes,
            (b"valid".len() + b"malformed".len()) as u64
        );
        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(
            report.retention.protected_logical_bytes,
            b"malformed".len() as u64
        );
        assert_eq!(report.retention.planned_tasks, 1);
        assert_eq!(report.actions[0].storage_key, "same");
        assert_eq!(report.actions[0].logical_bytes, b"valid".len() as u64);
    }

    #[test]
    fn opaque_candidate_identity_is_typed_and_not_a_managed_storage_key() {
        let opaque = CandidateKey::opaque(OpaqueNamespace::Event, OsStr::new("malformed"));
        let report_key = opaque.report_storage_key();

        assert!(matches!(
            opaque,
            CandidateKey::Opaque {
                namespace: OpaqueNamespace::Event,
                ..
            }
        ));
        assert_ne!(opaque, CandidateKey::managed(report_key.clone()));
        assert!(!storage_key_is_safe(&report_key));
    }

    #[test]
    fn opaque_event_namespace_is_disjoint_from_valid_storage_keys() {
        let root = tempdir().unwrap();
        let event_root = task_events_dir(root.path());
        fs::create_dir_all(&event_root).unwrap();
        let malformed_name = OsStr::new("malformed");
        let formerly_colliding_key = format!(
            "__opaque-event-{}",
            blake3::hash(malformed_name.as_encoded_bytes())
        );
        let valid = event_root.join(format!("{formerly_colliding_key}{TASK_EVENT_LOG_SUFFIX}"));
        let malformed = event_root.join(malformed_name);
        fs::write(&valid, b"valid").unwrap();
        fs::write(&malformed, b"malformed").unwrap();
        set_modified(&valid, 10);
        set_modified(&malformed, 10);

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(
            report.metrics_before.task_event_logical_bytes,
            (b"valid".len() + b"malformed".len()) as u64
        );
        assert_eq!(report.retention.protected_tasks, 1);
        assert_eq!(report.retention.planned_tasks, 1);
        assert_eq!(report.actions[0].storage_key, formerly_colliding_key);
    }

    // APFS rejects ill-formed UTF-8 names with EILSEQ; Linux filesystems
    // permit proving that the opaque key hashes raw bytes rather than a lossy
    // display string.
    #[cfg(target_os = "linux")]
    #[test]
    fn distinct_non_utf_event_names_have_distinct_opaque_accounting() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = tempdir().unwrap();
        let event_root = task_events_dir(root.path());
        fs::create_dir_all(&event_root).unwrap();
        let first = event_root.join(OsString::from_vec(b"event-\xff".to_vec()));
        let second = event_root.join(OsString::from_vec(b"event-\xfe".to_vec()));
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"second").unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(None, Some(0))).unwrap();

        assert_eq!(report.retention.protected_tasks, 2);
        assert_eq!(report.retention.protected_logical_bytes, 9);
        assert_eq!(report.metrics_before.task_event_logical_bytes, 9);
        assert_eq!(
            report
                .issues
                .iter()
                .filter(|issue| issue.kind == "event_name_invalid")
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_never_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("keep.txt");
        fs::write(&outside_file, "keep").unwrap();
        let task_dir = task_artifact_dir(root.path(), "linked");
        fs::create_dir_all(&task_dir).unwrap();
        symlink(outside.path(), task_dir.join("escape")).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(
            (
                report.retention.removed_tasks,
                report.retention.protected_tasks,
                outside_file.exists(),
            ),
            (0, 1, true)
        );
        assert_eq!(
            report
                .issues
                .iter()
                .filter(|issue| issue.kind == "symlink_entry")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_managed_root_protects_hidden_task_state() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("keep.txt");
        fs::write(&outside_file, b"keep").unwrap();
        write_registry(root.path(), [inactive_record("old-task", 10)]);
        symlink(outside.path(), task_artifacts_dir(root.path())).unwrap();

        let report =
            retain_task_store(root.path(), 100, RetentionOptions::dry_run(Some(1), None)).unwrap();

        assert_eq!(
            (
                report.retention.planned_tasks,
                report.retention.protected_tasks,
                outside_file.exists(),
            ),
            (0, 1, true)
        );
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "artifact_root"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_apply_removes_artifacts_and_registry_record() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "old-task", b"payload", 10);
        let event_log = task_event_log_path(root.path(), "old-task");
        fs::create_dir_all(event_log.parent().unwrap()).unwrap();
        fs::write(&event_log, b"event\n").unwrap();
        set_modified(&event_log, 10);
        let unrelated = root.path().join(".packet28/index/keep.bin");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        write_registry(
            root.path(),
            [
                inactive_record("old-task", 10),
                inactive_record("keep-task", 100),
            ],
        );
        capability::reset_open_workspace_call_count();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap();
        assert_eq!(
            capability::open_workspace_call_count(),
            1,
            "retention, instance admission, apply, and final rescan must share one root authority"
        );
        let registry = crate::storage::load_task_registry(root.path()).unwrap();

        assert_eq!(
            (
                report.retention.removed_tasks,
                artifact.exists(),
                event_log.exists(),
                registry.tasks.contains_key("old-task"),
                registry.tasks.contains_key("keep-task"),
                unrelated.exists(),
            ),
            (1, false, false, false, true, true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_stage_state_revalidation_error_restores_all_paths_and_registry() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "rollback", b"artifact", 10);
        let event = task_event_log_path(root.path(), "rollback");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        write_registry(root.path(), [inactive_record("rollback", 10)]);
        let registry_before = fs::read(task_registry_path(root.path())).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("rollback").unwrap();

        let error = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || {
                Err(DaemonCoreError::RetentionCandidateChanged {
                    path: root.path().join(STATE_DIR_NAME),
                })
            },
            || Ok(()),
            || Ok(()),
        )
        .unwrap_err();

        assert!(
            matches!(
                &error.error,
                DaemonCoreError::RetentionCandidateChanged { .. }
            ),
            "unexpected rollback error: {error:?}"
        );
        assert_eq!(fs::read(artifact).unwrap(), b"artifact");
        assert_eq!(fs::read(event).unwrap(), b"event");
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            registry_before
        );
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_apply_rejects_unknown_group_entry_before_marker_or_registry_mutation() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "precommit-unknown", b"artifact", 10);
        write_registry(root.path(), [inactive_record("precommit-unknown", 10)]);
        let registry_before = fs::read(task_registry_path(root.path())).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("precommit-unknown").unwrap();
        let quarantine = root.path().join(STATE_DIR_NAME).join(QUARANTINE_DIR_NAME);

        let error = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || {
                let group = fs::read_dir(&quarantine)
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to enumerate injected quarantine",
                            &quarantine,
                            source,
                        )
                    })?
                    .next()
                    .ok_or_else(|| DaemonCoreError::RetentionCandidateChanged {
                        path: quarantine.clone(),
                    })?
                    .map_err(|source| {
                        DaemonCoreError::io(
                            "failed to resolve injected quarantine group",
                            &quarantine,
                            source,
                        )
                    })?
                    .path();
                fs::write(group.join("unexpected"), b"keep").map_err(|source| {
                    DaemonCoreError::io(
                        "failed to inject unknown quarantine entry",
                        group.join("unexpected"),
                        source,
                    )
                })
            },
            || Ok(()),
            || Ok(()),
        )
        .unwrap_err();

        assert!(!error.committed);
        assert!(!error.rollback_confirmed);
        assert_eq!(fs::read(&artifact).unwrap(), b"artifact");
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            registry_before
        );
        let group = fs::read_dir(&quarantine)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(group.join("unexpected")).unwrap(), b"keep");
        assert!(group.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn post_stage_readiness_error_restores_all_paths_and_registry() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "readiness", b"artifact", 10);
        write_registry(root.path(), [inactive_record("readiness", 10)]);
        let registry_before = fs::read(task_registry_path(root.path())).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("readiness").unwrap();
        let readiness = ready_path(root.path());

        let error = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || Ok(()),
            || {
                Err(DaemonCoreError::io(
                    "injected readiness inspection failure",
                    &readiness,
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected readiness inspection failure",
                    ),
                ))
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(matches!(error.error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(artifact).unwrap(), b"artifact");
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            registry_before
        );
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_rollback_failure_never_overwrites_a_recreated_source() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "raii-conflict", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("raii-conflict").unwrap();

        let error = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || {
                write_artifact(root.path(), "raii-conflict", b"new", 20);
                Err(DaemonCoreError::RetentionCandidateChanged {
                    path: task_artifact_dir(root.path(), "raii-conflict"),
                })
            },
            || Ok(()),
            || Ok(()),
        )
        .unwrap_err();
        let recreated = task_artifact_dir(root.path(), "raii-conflict").join("payload.bin");
        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert!(matches!(error.error, DaemonCoreError::Io { .. }));
        assert!(!error.rollback_confirmed);
        assert!(!error.byte_accounting_reliable);
        assert_eq!(fs::read(recreated).unwrap(), b"new");
        assert_eq!(recovery.conflicted_groups, 1);
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_reports_precommit_rollback_conflict_as_failed_not_skipped() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "rollback-report", b"old", 10);
        INJECT_ROLLBACK_CONFLICT_AFTER_STAGE_FOR.with(|configured| {
            configured.replace(Some("rollback-report".to_string()));
        });

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap();
        let action = &report.actions[0];

        assert_eq!(action.outcome, RetentionOutcome::Failed);
        assert_eq!(report.retention.failed_tasks, 1);
        assert_eq!(report.retention.skipped_tasks, 0);
        assert_eq!(report.retention.failed_logical_bytes, action.logical_bytes);
        assert!(!action.byte_accounting_reliable);
        assert!(!report.retention.action_byte_accounting_reliable);
        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "rollback-report").join("payload.bin"))
                .unwrap(),
            b"replacement"
        );
        assert_eq!(report.metrics_after.retention_quarantine_groups, 1);
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "candidate_cleanup_failed"
                && issue
                    .message
                    .contains("failed to roll back precommit task retention")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_group_creation_retries_a_collision_without_touching_it() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "group-collision", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let state = CapabilityDir::open(&root.path().join(STATE_DIR_NAME)).unwrap();
        let quarantine = state
            .ensure_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700)
            .unwrap();
        let collision_name = OsString::from("task-forced-collision");
        let success_name = OsString::from("task-forced-success");
        let collision = quarantine.create_dir(&collision_name, 0o700).unwrap();
        fs::write(collision.display_path().join("owner"), b"unrelated").unwrap();
        INJECT_QUARANTINE_GROUP_NAMES.with(|configured| {
            configured
                .borrow_mut()
                .extend([collision_name.clone(), success_name.clone()]);
        });
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("group-collision").unwrap();

        let transaction = StagingTransaction::new(&snapshot, candidate).unwrap();

        assert_eq!(transaction.group_name, success_name);
        assert_eq!(
            fs::read(collision.display_path().join("owner")).unwrap(),
            b"unrelated"
        );
        assert_eq!(
            quarantine.open_dir(&collision_name).unwrap().identity(),
            collision.identity()
        );
        drop(transaction);
        assert!(quarantine.open_dir(&collision_name).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn precommit_crash_recovery_restores_original_paths() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "precommit", b"payload", 10);
        write_registry(root.path(), [inactive_record("precommit", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("precommit").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        std::mem::forget(transaction);
        assert!(!artifact.exists());

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(
            (
                recovery.restored_precommit_groups,
                recovery.completed_committed_groups,
                recovery.conflicted_groups,
            ),
            (1, 0, 0)
        );
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("precommit"));
    }

    #[cfg(unix)]
    #[test]
    fn committed_crash_recovery_finishes_registry_and_deletion() {
        let root = tempdir().unwrap();
        write_paired_registry(root.path(), [inactive_record("committed", 10)]);
        let artifact = write_artifact(root.path(), "committed", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("committed").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(
            (
                recovery.restored_precommit_groups,
                recovery.completed_committed_groups,
                recovery.conflicted_groups,
            ),
            (0, 1, 0)
        );
        assert!(!artifact.exists());
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("committed"));
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_resumes_wide_components_in_bounded_batches() {
        let root = tempdir().unwrap();
        let (transaction, staged) = stage_committed_artifact_group(root.path(), "wide-committed");
        for index in 0..9 {
            fs::write(staged.join(format!("wide-{index}")), b"x").unwrap();
        }
        drop(transaction);

        let (passes, saw_pending) = finish_committed_recovery_in_tiny_batches(root.path());

        assert!(passes > 1);
        assert!(saw_pending);
        assert!(!task_artifact_dir(root.path(), "wide-committed").exists());
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_flattens_overdepth_components_in_bounded_batches() {
        let root = tempdir().unwrap();
        let (transaction, staged) = stage_committed_artifact_group(root.path(), "deep-committed");
        let mut leaf = staged;
        for _ in 0..=MAX_RETENTION_SCAN_DEPTH + 8 {
            leaf.push("d");
        }
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("deep-leaf"), b"x").unwrap();
        drop(transaction);

        let (passes, saw_pending) = finish_committed_recovery_in_tiny_batches(root.path());

        assert!(passes > 1);
        assert!(saw_pending);
        assert!(!task_artifact_dir(root.path(), "deep-committed").exists());
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_validates_the_group_before_removing_registry_records() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("committed-conflict", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("committed-conflict").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let unexpected = transaction.group.display_path().join("unexpected");
        fs::write(&unexpected, b"keep").unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("committed-conflict"));
        assert_eq!(fs::read(unexpected).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn registry_only_candidate_applies_without_artifact_or_event() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("registry-only", 10)]);

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap();

        assert_eq!(report.actions[0].outcome, RetentionOutcome::Removed);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("registry-only"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_only_precommit_recovery_preserves_record() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("registry-precommit", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("registry-precommit").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.restored_precommit_groups, 1);
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("registry-precommit"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_only_committed_recovery_removes_record() {
        let root = tempdir().unwrap();
        write_paired_registry(root.path(), [inactive_record("registry-committed", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("registry-committed").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 1);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("registry-committed"));
    }

    #[cfg(unix)]
    #[test]
    fn stage_rename_failure_before_sync_restores_candidate() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "stage-sync", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("stage-sync").unwrap();
        let component = candidate.artifact.as_ref().unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();

        let error = transaction
            .stage_component_with_observer(0, component, || {
                Err(DaemonCoreError::io(
                    "injected post-rename staging failure",
                    &component.path,
                    std::io::Error::other("injected pre-sync failure"),
                ))
            })
            .unwrap_err();
        drop(transaction);

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_destination_sync_precedes_source_sync_and_rolls_back_on_source_failure() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "stage-fsync-order", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("stage-fsync-order").unwrap();
        let component = candidate.artifact.as_ref().unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        let (source_parent, _) = transaction
            .open_original_location(&transaction.journal.components[0])
            .unwrap();
        capability::inject_sync_rename_after_destination_once(
            source_parent.display_path(),
            transaction.group.display_path(),
        );

        let error = transaction.stage_component(0, component).unwrap_err();
        drop(transaction);

        assert!(error
            .to_string()
            .contains("source-directory sync failure after destination sync"));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_destination_sync_precedes_source_sync_and_retry_converges() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "restore-fsync-order", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("restore-fsync-order").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let group_path = transaction.group.display_path().to_path_buf();
        let (destination_parent, _) = transaction
            .open_original_location(&transaction.journal.components[0])
            .unwrap();
        let destination_path = destination_parent.display_path().to_path_buf();
        std::mem::forget(transaction);
        capability::inject_sync_rename_after_destination_once(&group_path, &destination_path);

        let first = recover_task_store_quarantine(root.path()).unwrap();
        assert_eq!(first.conflicted_groups, 1);
        assert_eq!(fs::read(&artifact).unwrap(), b"payload");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            1
        );

        let second = recover_task_store_quarantine(root.path()).unwrap();
        assert_eq!(second.restored_precommit_groups, 1);
        assert_eq!(second.conflicted_groups, 0);
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_noreplace_probe_fails_before_candidate_mutation() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "unsupported-noreplace", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("unsupported-noreplace").unwrap();
        capability::inject_noreplace_unsupported_once();

        let error = StagingTransaction::new(&snapshot, candidate).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn constructor_rollback_conflict_is_reported_as_failed_and_left_recoverable() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "constructor-conflict", b"payload", 10);
        INJECT_CONSTRUCTION_ROLLBACK_CONFLICT_FOR.with(|configured| {
            configured.replace(Some("constructor-conflict".to_string()));
        });
        capability::inject_noreplace_unsupported_once();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap();

        assert_eq!(report.actions[0].outcome, RetentionOutcome::Failed);
        assert_eq!(report.retention.failed_tasks, 1);
        assert_eq!(report.retention.skipped_tasks, 0);
        assert_eq!(fs::read(&artifact).unwrap(), b"payload");
        assert!(report.metrics_after.retention_quarantine_groups >= 1);
        assert!(report.issues.iter().any(|issue| {
            issue.kind == "candidate_cleanup_failed"
                && issue
                    .message
                    .contains("failed to clean up an unstarted retention transaction")
        }));

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        assert_eq!(recovery.conflicted_groups, 0);
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_parent_sync_failure_precedes_candidate_mutation() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "quarantine-sync", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("quarantine-sync").unwrap();
        capability::inject_directory_create_sync_failure_once(OsStr::new(QUARANTINE_DIR_NAME));

        let error = StagingTransaction::new(&snapshot, candidate).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_never_replaces_a_preexisting_quarantine_destination() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = write_artifact(root.path(), "staged-destination", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("staged-destination").unwrap();
        let component = candidate.artifact.as_ref().unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        let replacement = transaction
            .group
            .create_dir(OsStr::new("artifacts"), 0o700)
            .unwrap();

        assert!(transaction.stage_component(0, component).is_err());
        assert_eq!(
            replacement.identity(),
            transaction
                .group
                .open_dir(OsStr::new("artifacts"))
                .unwrap()
                .identity()
        );
        drop(transaction);
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn staging_source_swap_is_detected_without_deleting_either_inode() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "stage-source-swap", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("stage-source-swap").unwrap();
        let component = candidate.artifact.as_ref().unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        let held = task_artifacts_dir(root.path()).join("held-stage-source");

        let error = transaction
            .stage_component_with_observers(
                0,
                component,
                || {
                    fs::rename(&component.path, &held).unwrap();
                    write_artifact(root.path(), "stage-source-swap", b"replacement", 20);
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap_err();
        drop(transaction);

        assert!(matches!(
            error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(fs::read(held.join("payload.bin")).unwrap(), b"original");
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn precommit_restore_rechecks_source_after_isolation_race() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "restore-source-swap", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("restore-source-swap").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let component = transaction.journal.components[0].clone();
        let (parent, original_name) = transaction.open_original_location(&component).unwrap();
        let held = OsStr::new("held-restoration-source");

        let error = restore_component_from_group_with_observer(
            &transaction.group,
            &parent,
            &original_name,
            &component,
            || {
                transaction
                    .group
                    .rename_to_noreplace(
                        OsStr::new(component.kind.staged_name()),
                        &transaction.group,
                        held,
                    )
                    .unwrap();
                transaction
                    .group
                    .create_dir(OsStr::new(component.kind.staged_name()), 0o700)
                    .unwrap();
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::Io { .. } | DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(
            transaction.group.open_dir(held).unwrap().identity(),
            component.identity
        );
        assert!(parent.entry_identity(&original_name).unwrap().is_none());
        drop(transaction);
    }

    #[cfg(unix)]
    #[test]
    fn precommit_restore_converges_a_hardlinked_staging_duplicate() {
        let root = tempdir().unwrap();
        let event = task_event_log_path(root.path(), "staging-duplicate");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("staging-duplicate").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let component = transaction.journal.components[0].clone();
        assert_eq!(component.kind, JournalComponentKind::Events);
        let (parent, original_name) = transaction.open_original_location(&component).unwrap();
        let staged_name = OsStr::new(component.kind.staged_name());
        fs::hard_link(transaction.group.display_path().join(staged_name), &event).unwrap();

        restore_component_from_group(&transaction.group, &parent, &original_name, &component)
            .unwrap();

        assert_eq!(fs::read(&event).unwrap(), b"event");
        assert_eq!(
            parent.entry_identity(&original_name).unwrap(),
            Some(component.identity)
        );
        assert!(transaction
            .group
            .entry_identity(staged_name)
            .unwrap()
            .is_none());
        drop(transaction);
    }

    #[cfg(unix)]
    #[test]
    fn precommit_restore_converges_a_hardlinked_restoration_duplicate() {
        let root = tempdir().unwrap();
        let event = task_event_log_path(root.path(), "restoration-duplicate");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("restoration-duplicate").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let component = transaction.journal.components[0].clone();
        assert_eq!(component.kind, JournalComponentKind::Events);
        let (parent, original_name) = transaction.open_original_location(&component).unwrap();
        let staged_name = OsStr::new(component.kind.staged_name());
        let restoration_name = OsStr::new(component.kind.restoration_name());
        transaction
            .group
            .tombstone_entry_to_verified(staged_name, component.identity, restoration_name)
            .unwrap();
        fs::hard_link(
            transaction.group.display_path().join(restoration_name),
            &event,
        )
        .unwrap();

        restore_component_from_group(&transaction.group, &parent, &original_name, &component)
            .unwrap();

        assert_eq!(fs::read(&event).unwrap(), b"event");
        assert_eq!(
            parent.entry_identity(&original_name).unwrap(),
            Some(component.identity)
        );
        assert!(transaction
            .group
            .entry_identity(restoration_name)
            .unwrap()
            .is_none());
        drop(transaction);
    }

    #[cfg(unix)]
    #[test]
    fn precommit_restore_converges_hardlinked_staged_and_restoring_without_original() {
        let root = tempdir().unwrap();
        let event = task_event_log_path(root.path(), "staged-restoring-duplicate");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("staged-restoring-duplicate").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let component = transaction.journal.components[0].clone();
        assert_eq!(component.kind, JournalComponentKind::Events);
        let (parent, original_name) = transaction.open_original_location(&component).unwrap();
        let staged_name = OsStr::new(component.kind.staged_name());
        let restoration_name = OsStr::new(component.kind.restoration_name());
        fs::hard_link(
            transaction.group.display_path().join(staged_name),
            transaction.group.display_path().join(restoration_name),
        )
        .unwrap();

        restore_component_from_group(&transaction.group, &parent, &original_name, &component)
            .unwrap();

        assert_eq!(fs::read(&event).unwrap(), b"event");
        assert_eq!(
            parent.entry_identity(&original_name).unwrap(),
            Some(component.identity)
        );
        assert!(transaction
            .group
            .entry_identity(staged_name)
            .unwrap()
            .is_none());
        assert!(transaction
            .group
            .entry_identity(restoration_name)
            .unwrap()
            .is_none());
        drop(transaction);
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_converges_hardlinked_staged_and_deleting() {
        let root = tempdir().unwrap();
        let task_id = "committed-staged-deleting-duplicate";
        write_paired_registry(root.path(), [inactive_record(task_id, 10)]);
        let event = task_event_log_path(root.path(), task_id);
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate(task_id).unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let component = transaction.journal.components[0].clone();
        assert_eq!(component.kind, JournalComponentKind::Events);
        let staged_name = OsStr::new(component.kind.staged_name());
        let deletion_name = OsStr::new(component.kind.deletion_name());
        transaction.mark_committed().unwrap();
        fs::hard_link(
            transaction.group.display_path().join(staged_name),
            transaction.group.display_path().join(deletion_name),
        )
        .unwrap();
        drop(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 0);
        assert_eq!(report.completed_committed_groups, 1);
        assert!(!event.exists());
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key(task_id));
    }

    #[cfg(unix)]
    #[test]
    fn committed_marker_post_rename_sync_failure_defers_to_recovery() {
        let root = tempdir().unwrap();
        write_paired_registry(root.path(), [inactive_record("marker-sync", 10)]);
        write_artifact(root.path(), "marker-sync", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("marker-sync").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        capability::inject_atomic_after_rename_failure_once(OsStr::new(
            QUARANTINE_JOURNAL_FILE_NAME,
        ));

        assert!(transaction.mark_committed().is_err());
        drop(transaction);
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("marker-sync"));

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 1);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("marker-sync"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_post_rename_sync_failure_keeps_committed_journal_for_recovery() {
        let root = tempdir().unwrap();
        write_paired_registry(root.path(), [inactive_record("registry-sync", 10)]);
        write_artifact(root.path(), "registry-sync", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("registry-sync").unwrap();
        capability::inject_atomic_after_rename_failure_once(OsStr::new(TASK_REGISTRY_FILE_NAME));

        let error = apply_candidate(&snapshot, candidate).unwrap_err();

        assert!(matches!(error.error, DaemonCoreError::Io { .. }));
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("registry-sync"));
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            1
        );

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        assert_eq!(recovery.completed_committed_groups, 1);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("registry-sync"));
    }

    #[cfg(unix)]
    #[test]
    fn wal_reset_post_rename_failure_recovers_committed_retention() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry::default(),
            &WatchRegistry::default(),
        )
        .unwrap();
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default()
                .upsert_task(inactive_record("wal-reset", 10)),
        )
        .unwrap();
        write_artifact(root.path(), "wal-reset", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("wal-reset").unwrap();
        capability::inject_atomic_after_rename_failure_once(OsStr::new(
            crate::storage::registry_delta_wal_path(root.path())
                .file_name()
                .unwrap(),
        ));

        let error = apply_candidate(&snapshot, candidate).unwrap_err();

        assert!(error.committed);
        let committed = crate::storage::load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert!(!committed.tasks.tasks.contains_key("wal-reset"));
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            1
        );

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.conflicted_groups, 0);
        assert_eq!(recovery.completed_committed_groups, 1);
        assert!(
            !crate::storage::load_task_watch_registry_with_deltas(root.path())
                .unwrap()
                .tasks
                .tasks
                .contains_key("wal-reset")
        );
        assert_eq!(
            fs::metadata(crate::storage::registry_delta_wal_path(root.path()))
                .unwrap()
                .len(),
            crate::storage::REGISTRY_DELTA_WAL_HEADER_BYTES as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_preserves_same_value_newer_wal_readmission() {
        let root = tempdir().unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &TaskRegistry::default(),
            &WatchRegistry::default(),
        )
        .unwrap();
        let record = inactive_record("readmitted", 10);
        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(1))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default().upsert_task(record.clone()),
        )
        .unwrap();
        write_artifact(root.path(), "readmitted", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("readmitted").unwrap().clone();
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        assert_eq!(
            transaction.journal.registry_revision,
            Some(crate::storage::RegistryRevision::new(1))
        );
        drop(transaction);

        crate::storage::append_task_watch_registry_delta(
            root.path(),
            crate::storage::RegistryRevisionRange::single(crate::storage::RegistryRevision::new(2))
                .unwrap(),
            &crate::storage::RegistryDeltaBatch::default().upsert_task(record),
        )
        .unwrap();
        write_artifact(root.path(), "readmitted", b"new", 101);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert!(
            crate::storage::load_task_watch_registry_with_deltas(root.path())
                .unwrap()
                .tasks
                .tasks
                .contains_key("readmitted")
        );
        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "readmitted").join("payload.bin")).unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_preserves_same_revision_checkpoint_readmission() {
        let root = tempdir().unwrap();
        let record = inactive_record("checkpoint-readmitted", 10);
        let tasks = TaskRegistry {
            tasks: BTreeMap::from([(record.task_id.clone(), record)]),
        };
        let watches = WatchRegistry::default();
        crate::storage::save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();
        write_artifact(root.path(), "checkpoint-readmitted", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("checkpoint-readmitted").unwrap().clone();
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        let original_generation = transaction
            .journal
            .registry_checkpoint_generation
            .expect("paired checkpoint has a generation");
        drop(transaction);

        crate::storage::save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();
        write_artifact(root.path(), "checkpoint-readmitted", b"new", 101);
        let current = StoreSnapshot::load(root.path(), 101).unwrap();
        assert_eq!(
            current
                .candidate("checkpoint-readmitted")
                .unwrap()
                .registry_revision,
            candidate.registry_revision
        );
        assert!(
            current
                .candidate("checkpoint-readmitted")
                .unwrap()
                .registry_checkpoint_generation
                > Some(original_generation)
        );

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert!(
            crate::storage::load_task_watch_registry_with_deltas(root.path())
                .unwrap()
                .tasks
                .tasks
                .contains_key("checkpoint-readmitted")
        );
        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "checkpoint-readmitted").join("payload.bin"))
                .unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_rejects_absent_record_checkpoint_rollback() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let record = inactive_record("generation-rollback", 10);
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let mut task_value = serde_json::to_value(TaskRegistry {
            tasks: BTreeMap::from([(record.task_id.clone(), record)]),
        })
        .unwrap();
        task_value.as_object_mut().unwrap().insert(
            "task_watch_checkpoint_generation".to_string(),
            serde_json::json!(7),
        );
        fs::write(&task_path, serde_json::to_vec(&task_value).unwrap()).unwrap();
        fs::write(
            &watch_path,
            serde_json::to_vec(&serde_json::json!({
                "watches": [],
                "task_watch_checkpoint_generation": 7,
            }))
            .unwrap(),
        )
        .unwrap();
        write_artifact(root.path(), "generation-rollback", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("generation-rollback").unwrap().clone();
        assert_eq!(candidate.registry_checkpoint_generation, Some(7));
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        drop(transaction);

        fs::write(
            &task_path,
            serde_json::to_vec(&serde_json::json!({
                "tasks": {},
                "task_watch_checkpoint_generation": 6,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            &watch_path,
            serde_json::to_vec(&serde_json::json!({
                "watches": [],
                "task_watch_checkpoint_generation": 6,
            }))
            .unwrap(),
        )
        .unwrap();

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_same_revision_checkpoint_readmission_fails_closed() {
        let root = tempdir().unwrap();
        let record = inactive_record("legacy-commit", 10);
        write_registry(root.path(), [record.clone()]);
        write_artifact(root.path(), "legacy-commit", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("legacy-commit").unwrap().clone();
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.journal.schema_version = LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION;
        transaction.journal.registry_revision = None;
        transaction.journal.registry_checkpoint_generation = None;
        transaction.persist_journal().unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        drop(transaction);

        crate::storage::save_task_registry(
            root.path(),
            &TaskRegistry {
                tasks: BTreeMap::from([(record.task_id.clone(), record)]),
            },
        )
        .unwrap();
        write_artifact(root.path(), "legacy-commit", b"new", 101);
        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        let current = StoreSnapshot::load(root.path(), 101).unwrap();
        let current = current.candidate("legacy-commit").unwrap();
        assert_eq!(current.registry_revision, candidate.registry_revision);
        assert_eq!(current.registry_checkpoint_generation, None);
        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "legacy-commit").join("payload.bin")).unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_committed_journal_completes_when_record_is_already_absent() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("legacy-absent", 10)]);
        write_artifact(root.path(), "legacy-absent", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("legacy-absent").unwrap().clone();
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.journal.schema_version = LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION;
        transaction.journal.registry_revision = None;
        transaction.journal.registry_checkpoint_generation = None;
        transaction.persist_journal().unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        drop(transaction);
        crate::storage::save_task_registry(root.path(), &TaskRegistry::default()).unwrap();

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 1);
        assert_eq!(recovery.conflicted_groups, 0);
        assert!(!task_artifact_dir(root.path(), "legacy-absent").exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_deletion_rejects_a_moved_declared_component() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "moved-component", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("moved-component").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        fs::rename(
            transaction.group.display_path().join("artifacts"),
            transaction.group.display_path().join("held-artifacts"),
        )
        .unwrap();

        let error = transaction.delete_committed().unwrap_err();
        let isolated = transaction
            .quarantine
            .entries()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".deleting-group-"))
            .unwrap();
        let isolated = transaction.quarantine.display_path().join(isolated);

        assert!(matches!(
            error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(
            fs::read(isolated.join("held-artifacts/payload.bin")).unwrap(),
            b"original"
        );
        assert!(isolated.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_deletion_rejects_a_replaced_declared_component() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "replaced-component", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("replaced-component").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let held = transaction
            .quarantine
            .display_path()
            .join("held-replaced-artifacts");
        fs::rename(transaction.group.display_path().join("artifacts"), &held).unwrap();
        fs::create_dir(transaction.group.display_path().join("artifacts")).unwrap();
        fs::write(
            transaction
                .group
                .display_path()
                .join("artifacts/replacement"),
            b"replacement",
        )
        .unwrap();

        let error = transaction.delete_committed().unwrap_err();
        let isolated = transaction
            .quarantine
            .entries()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".deleting-group-"))
            .unwrap();
        let isolated = transaction.quarantine.display_path().join(isolated);

        assert!(matches!(
            error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(fs::read(held.join("payload.bin")).unwrap(), b"original");
        assert_eq!(
            fs::read(isolated.join("artifacts/replacement")).unwrap(),
            b"replacement"
        );
        assert!(isolated.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_deletion_rejects_unexpected_group_entries() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "unexpected-entry", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("unexpected-entry").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        fs::write(transaction.group.display_path().join("unexpected"), b"keep").unwrap();

        let error = transaction.delete_committed().unwrap_err();
        let isolated = transaction
            .quarantine
            .entries()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".deleting-group-"))
            .unwrap();
        let isolated = transaction.quarantine.display_path().join(isolated);

        assert!(matches!(
            error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(
            fs::read(isolated.join("artifacts/payload.bin")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(isolated.join("unexpected")).unwrap(), b"keep");
        assert!(isolated.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_deletion_revalidates_identity_after_enumeration() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "enumeration-swap", b"original", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("enumeration-swap").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let quarantine = transaction.quarantine.duplicate().unwrap();
        let held = quarantine.display_path().join("held-after-enumeration");
        let group_name = transaction.group_name.clone();
        let group_identity = transaction.group.identity();
        let journal = transaction.journal.clone();

        let error = delete_committed_group_with_observer(
            &quarantine,
            &group_name,
            group_identity,
            &journal,
            false,
            |isolated_group| {
                fs::rename(isolated_group.display_path().join("artifacts"), &held).map_err(
                    |source| {
                        DaemonCoreError::io(
                            "failed to move staged component during test",
                            &held,
                            source,
                        )
                    },
                )?;
                fs::create_dir(isolated_group.display_path().join("artifacts")).map_err(
                    |source| {
                        DaemonCoreError::io(
                            "failed to replace staged component during test",
                            isolated_group.display_path().join("artifacts"),
                            source,
                        )
                    },
                )?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::RetentionCandidateChanged { .. }
        ));
        assert_eq!(fs::read(held.join("payload.bin")).unwrap(), b"original");
        let isolated = quarantine
            .entries()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".deleting-group-"))
            .unwrap();
        assert!(quarantine
            .display_path()
            .join(isolated)
            .join(QUARANTINE_JOURNAL_FILE_NAME)
            .exists());
        transaction.rollback_enabled = false;
    }

    #[cfg(unix)]
    #[test]
    fn verified_file_unlink_rejects_a_final_identity_swap() {
        let root = tempdir().unwrap();
        let directory = CapabilityDir::open(root.path()).unwrap();
        let target = root.path().join("target");
        let held = root.path().join("held");
        fs::write(&target, b"original").unwrap();
        let identity = directory
            .entry_identity(OsStr::new("target"))
            .unwrap()
            .unwrap();

        let error = directory
            .remove_tombstone_verified_with_observer(OsStr::new("target"), identity, || {
                fs::rename(&target, &held)?;
                fs::write(&target, b"replacement")
            })
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read(held).unwrap(), b"original");
        assert_eq!(fs::read(target).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn verified_directory_unlink_rejects_a_final_identity_swap() {
        let root = tempdir().unwrap();
        let directory = CapabilityDir::open(root.path()).unwrap();
        let target = root.path().join("target");
        let held = root.path().join("held");
        fs::create_dir(&target).unwrap();
        let identity = directory
            .entry_identity(OsStr::new("target"))
            .unwrap()
            .unwrap();

        let error = directory
            .remove_empty_dir_verified_with_observer(OsStr::new("target"), identity, || {
                fs::rename(&target, &held)?;
                fs::create_dir(&target)
            })
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(held.is_dir());
        assert!(target.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_capability_write_never_follows_a_precreated_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("outside");
        fs::write(&outside_file, b"keep").unwrap();
        let directory = CapabilityDir::open(root.path()).unwrap();
        let mut injected_temp = None;

        directory
            .write_json_atomically_with_observers(
                OsStr::new("target.json"),
                b"{\"safe\":true}",
                capability::TEST_ATOMIC_WRITE_TEMP_PREFIX,
                |temporary| {
                    injected_temp = Some(temporary.to_os_string());
                    symlink(&outside_file, root.path().join(temporary))
                },
                || Ok(()),
                || Ok(()),
            )
            .unwrap();

        assert_eq!(fs::read(outside_file).unwrap(), b"keep");
        assert_eq!(
            fs::read(root.path().join("target.json")).unwrap(),
            b"{\"safe\":true}"
        );
        let injected_temp = root.path().join(injected_temp.unwrap());
        assert!(fs::symlink_metadata(injected_temp)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_capability_write_syncs_the_retained_parent_after_path_replacement() {
        let root = tempdir().unwrap();
        let root_capability = CapabilityDir::open(root.path()).unwrap();
        let parent = root_capability
            .create_dir(OsStr::new("parent"), 0o700)
            .unwrap();
        let parent_path = root.path().join("parent");
        let held_path = root.path().join("parent-held");

        parent
            .write_json_atomically_with_observer(
                OsStr::new("target.json"),
                b"{\"safe\":true}",
                capability::TEST_ATOMIC_WRITE_TEMP_PREFIX,
                || {
                    fs::rename(&parent_path, &held_path)?;
                    fs::create_dir(&parent_path)
                },
            )
            .unwrap();

        assert_eq!(
            fs::read(held_path.join("target.json")).unwrap(),
            b"{\"safe\":true}"
        );
        assert!(!parent_path.join("target.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_removal_advances_the_paired_registry_generation() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "paired-retention", b"payload", 10);
        write_registry(root.path(), [inactive_record("paired-retention", 10)]);
        let tasks = crate::storage::load_task_registry(root.path()).unwrap();
        crate::storage::save_task_watch_registry_checkpoint(
            root.path(),
            &tasks,
            &WatchRegistry::default(),
        )
        .unwrap();
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let before = checkpoint_generation(&task_path).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("paired-retention").unwrap();

        let outcome = apply_candidate(&snapshot, candidate).unwrap();

        assert_eq!(outcome, RetentionOutcome::Removed);
        let task_generation = checkpoint_generation(&task_path).unwrap();
        let watch_generation = checkpoint_generation(&watch_path).unwrap();
        assert!(task_generation > before);
        assert_eq!(watch_generation, task_generation);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("paired-retention"));
    }

    #[cfg(unix)]
    #[test]
    fn retention_apply_rejects_removal_that_would_dangle_a_watch() {
        let root = tempdir().unwrap();
        let (tasks, watches) =
            paired_registry_with_watch(inactive_record("watched-retention", 10), "watch-retained");
        crate::storage::save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();
        write_artifact(root.path(), "watched-retention", b"payload", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("watched-retention").unwrap();

        let error = apply_candidate(&snapshot, candidate).unwrap_err();

        assert!(matches!(
            error.error,
            DaemonCoreError::InvalidTaskWatchRegistry { .. }
        ));
        let (loaded_tasks, loaded_watches, _) =
            crate::storage::load_task_watch_registry_checkpoint_with_event_tails(root.path())
                .unwrap();
        assert!(loaded_tasks.tasks.contains_key("watched-retention"));
        assert_eq!(loaded_watches.watches[0].watch_id, "watch-retained");
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_rejects_removal_that_would_dangle_a_watch() {
        let root = tempdir().unwrap();
        let record = inactive_record("watched-recovery", 10);
        let (tasks, watches) =
            paired_registry_with_watch(record.clone(), "watch-recovery-retained");
        crate::storage::save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();
        let state = CapabilityDir::open(&root.path().join(STATE_DIR_NAME)).unwrap();
        let daemon = state.open_dir(OsStr::new("daemon")).unwrap();
        let expected_records = BTreeMap::from([(
            record.task_id.clone(),
            serde_json::to_value(&tasks.tasks[&record.task_id]).unwrap(),
        )]);
        let expected_generation = StoreSnapshot::load(root.path(), 100)
            .unwrap()
            .candidate("watched-recovery")
            .unwrap()
            .registry_checkpoint_generation;

        let error = finish_anchored_committed_registry_removal(
            &daemon,
            root.path(),
            &expected_records,
            Some(crate::storage::RegistryRevision::ZERO),
            expected_generation,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("invalid task/watch registry checkpoint"));
        let (loaded_tasks, loaded_watches, _) =
            crate::storage::load_task_watch_registry_checkpoint_with_event_tails(root.path())
                .unwrap();
        assert!(loaded_tasks.tasks.contains_key("watched-recovery"));
        assert_eq!(
            loaded_watches.watches[0].watch_id,
            "watch-recovery-retained"
        );
    }

    #[cfg(unix)]
    #[test]
    fn anchored_registry_mutation_survives_daemon_path_replacement() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        write_artifact(root.path(), "daemon-parent-swap", b"payload", 10);
        write_registry(root.path(), [inactive_record("daemon-parent-swap", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("daemon-parent-swap").unwrap();
        let daemon = daemon_dir(root.path());
        let held = root.path().join(STATE_DIR_NAME).join("daemon-held");
        let outside = tempdir().unwrap();
        let outside_registry = outside.path().join(TASK_REGISTRY_FILE_NAME);
        fs::write(&outside_registry, b"outside").unwrap();

        let outcome = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || Ok(()),
            || {
                fs::rename(&daemon, &held).map_err(|source| {
                    DaemonCoreError::io("failed to move daemon root during test", &daemon, source)
                })?;
                symlink(outside.path(), &daemon).map_err(|source| {
                    DaemonCoreError::io(
                        "failed to replace daemon root during test",
                        &daemon,
                        source,
                    )
                })
            },
            || Ok(()),
        )
        .unwrap();

        assert_eq!(outcome, RetentionOutcome::Removed);
        let held_registry: serde_json::Value =
            serde_json::from_slice(&fs::read(held.join(TASK_REGISTRY_FILE_NAME)).unwrap()).unwrap();
        assert!(held_registry["tasks"].get("daemon-parent-swap").is_none());
        assert_eq!(fs::read(outside_registry).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn precommit_recovery_never_overwrites_recreated_source() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "recreated", b"old", 10);
        write_registry(root.path(), [inactive_record("recreated", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("recreated").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let recreated = write_artifact(root.path(), "recreated", b"new", 20);
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        let report = inspect_task_store(root.path(), 100).unwrap();

        assert_eq!(
            (
                recovery.restored_precommit_groups,
                recovery.conflicted_groups,
                fs::read(recreated).unwrap(),
            ),
            (0, 1, b"new".to_vec())
        );
        assert_eq!(report.metrics_before.retention_quarantine_groups, 1);
        assert!(report.metrics_before.retention_quarantine_logical_bytes >= 3);
        assert!(
            report.retention.protected_logical_bytes
                >= report.metrics_before.retention_quarantine_logical_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_delete_failure_is_durable_and_recoverable() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "delete-failure", b"payload", 10);
        write_registry(root.path(), [inactive_record("delete-failure", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("delete-failure").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let expected_records = candidate.record_values.clone();
        let daemon = transaction.daemon.duplicate().unwrap();
        assert!(remove_anchored_registry_records_if_unchanged_with_commit(
            &daemon,
            root.path(),
            &expected_records,
            candidate.registry_revision,
            candidate.registry_checkpoint_generation,
            || transaction.mark_committed(),
        )
        .unwrap());
        let moved_name = OsString::from("moved-committed-group");
        transaction
            .quarantine
            .rename_to_noreplace(
                &transaction.group_name,
                &transaction.quarantine,
                &moved_name,
            )
            .unwrap();
        let _replacement = transaction
            .quarantine
            .create_dir(&transaction.group_name, 0o700)
            .unwrap();

        assert!(transaction.delete_committed().is_err());
        drop(transaction);
        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 1);
        assert_eq!(recovery.conflicted_groups, 0);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("delete-failure"));
        assert_eq!(
            inspect_task_store(root.path(), 100)
                .unwrap()
                .metrics_before
                .retention_quarantine_groups,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_observes_quarantine_created_after_its_initial_snapshot() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();

        let recovery = recover_task_store_quarantine_with_observer(root.path(), || {
            let (_, group) = create_quarantine_group(root.path(), "late-group");
            group
                .write_json_atomically(
                    OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                    b"{\"invalid\":true}",
                    RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
                )
                .unwrap();
        })
        .unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert!(recovery
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_invalid_journal"));
    }

    #[cfg(unix)]
    #[test]
    fn startup_recovery_waits_for_an_inflight_writer() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let writer = acquire_task_store_writer_lease(root.path()).unwrap();
        let root_path = root.path().to_path_buf();
        let (completed_tx, completed_rx) = mpsc::channel();
        let recovery = thread::spawn(move || {
            completed_tx
                .send(recover_task_store_quarantine(&root_path))
                .unwrap();
        });

        assert!(matches!(
            completed_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(writer);
        let report = completed_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(report, TaskStoreRecoveryReport::default());
        recovery.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn startup_recovery_does_not_scan_unrelated_deep_state() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let mut deep = root.path().join(STATE_DIR_NAME).join("unrelated");
        for _ in 0..=MAX_RETENTION_SCAN_DEPTH {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("keep"), b"unrelated").unwrap();

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report, TaskStoreRecoveryReport::default());
        assert_eq!(fs::read(deep.join("keep")).unwrap(), b"unrelated");
    }

    #[cfg(unix)]
    #[test]
    fn startup_handoff_participates_in_lifecycle_lock_when_state_is_initially_absent() {
        let root = tempdir().unwrap();
        assert!(!root.path().join(STATE_DIR_NAME).exists());
        let instance_lease = acquire_daemon_instance_lease(root.path()).unwrap();

        let (report, daemon_lease) =
            recover_task_store_quarantine_and_acquire_daemon_lease(root.path(), &instance_lease)
                .unwrap();

        assert_eq!(report, TaskStoreRecoveryReport::default());
        assert!(root.path().join(STATE_DIR_NAME).is_dir());
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());
        drop(daemon_lease);
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn startup_handoff_rejects_an_instance_lease_for_another_workspace() {
        let root = tempdir().unwrap();
        let other = tempdir().unwrap();
        let other_instance = acquire_daemon_instance_lease(other.path()).unwrap();

        let error =
            recover_task_store_quarantine_and_acquire_daemon_lease(root.path(), &other_instance)
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("requires the matching daemon-instance lease"));
        assert!(!root.path().join(STATE_DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn startup_handoff_stops_after_the_supported_recovery_passes() {
        let root = tempdir().unwrap();
        let instance_lease = acquire_daemon_instance_lease(root.path()).unwrap();
        INJECT_HANDOFF_QUARANTINE_PRESENT_PASSES
            .with(|remaining| remaining.set(MAX_STARTUP_RECOVERY_PASSES));

        let error =
            recover_task_store_quarantine_and_acquire_daemon_lease(root.path(), &instance_lease)
                .unwrap_err();

        assert!(error.to_string().contains("handoff-pass bound"));
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn startup_handoff_recovers_cleanup_that_wins_the_exclusive_to_shared_gap() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "handoff-gap", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let instance_lease = acquire_daemon_instance_lease(root.path()).unwrap();

        let (report, daemon_lease) =
            recover_task_store_quarantine_and_acquire_daemon_lease_with_observer(
                root.path(),
                &instance_lease,
                || {
                    let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
                    let candidate = snapshot.candidate("handoff-gap").unwrap();
                    let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
                    transaction.stage_all(candidate).unwrap();
                    std::mem::forget(transaction);
                },
            )
            .unwrap();

        assert_eq!(report.restored_precommit_groups, 1);
        assert_eq!(report.conflicted_groups, 0);
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());
        drop(daemon_lease);
    }

    #[cfg(unix)]
    #[test]
    fn startup_handoff_refuses_a_conflicted_quarantine_group() {
        let root = tempdir().unwrap();
        let (_quarantine, group) = create_quarantine_group(root.path(), "startup-conflict");
        let residue = group.display_path().join("unknown");
        fs::write(&residue, b"keep").unwrap();
        let instance_lease = acquire_daemon_instance_lease(root.path()).unwrap();

        let error =
            recover_task_store_quarantine_and_acquire_daemon_lease(root.path(), &instance_lease)
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("startup recovery left conflicted quarantine state"));
        assert_eq!(fs::read(residue).unwrap(), b"keep");
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_stays_on_retained_state_after_display_path_replacement() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let state = root.path().join(STATE_DIR_NAME);
        let held = root.path().join(".packet28-held");
        fs::write(state.join("old-marker"), b"old").unwrap();

        let report = recover_task_store_quarantine_with_observer(root.path(), || {
            fs::rename(&state, &held).unwrap();
            fs::create_dir(&state).unwrap();
            fs::create_dir(state.join("daemon")).unwrap();
            fs::write(state.join("new-marker"), b"new").unwrap();
        })
        .unwrap();

        assert_eq!(report, TaskStoreRecoveryReport::default());
        assert_eq!(fs::read(held.join("old-marker")).unwrap(), b"old");
        assert_eq!(fs::read(state.join("new-marker")).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_group_bound_is_checked_before_group_mutation() {
        let root = tempdir().unwrap();
        let (quarantine, _first) = create_quarantine_group(root.path(), "one");
        quarantine.create_dir(OsStr::new("two"), 0o700).unwrap();
        quarantine.create_dir(OsStr::new("three"), 0o700).unwrap();

        let error = bounded_quarantine_group_names_with_limit(
            &quarantine,
            "test quarantine enumeration",
            2,
        )
        .unwrap_err();

        assert!(error.to_string().contains("group bound"));
        assert_eq!(quarantine.entries().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_immediate_entry_bound_preserves_the_group() {
        let root = tempdir().unwrap();
        let (_quarantine, group) = create_quarantine_group(root.path(), "entry-bound");
        for name in ["one", "two", "three"] {
            fs::write(group.display_path().join(name), name).unwrap();
        }

        let error = bounded_quarantine_group_entries_with_limit(
            &group,
            "test quarantine group enumeration",
            2,
        )
        .unwrap_err();

        assert!(error.to_string().contains("immediate-entry bound"));
        assert_eq!(group.entries().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_journal_record_not_bound_to_its_storage_key() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("other-record", 10)]);
        let (_, group) = create_quarantine_group(root.path(), "unbound-record");
        let journal = QuarantineJournal {
            schema_version: QUARANTINE_JOURNAL_SCHEMA_VERSION,
            phase: QuarantinePhase::Committed,
            storage_key: "victim".to_string(),
            record_values: BTreeMap::from([(
                "other-record".to_string(),
                serde_json::to_value(inactive_record("other-record", 10)).unwrap(),
            )]),
            registry_revision: Some(crate::storage::RegistryRevision::ZERO),
            registry_checkpoint_generation: None,
            components: Vec::new(),
        };
        group
            .write_json_atomically(
                OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                &serde_json::to_vec(&journal).unwrap(),
                RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            )
            .unwrap();

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("other-record"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_noncanonical_component_only_journal_storage_keys() {
        let overlong = "a".repeat(crate::storage::MAX_TASK_STORAGE_KEY_BYTES + 1);
        let cases = [
            (QuarantinePhase::Precommit, "LIVE".to_string()),
            (QuarantinePhase::Precommit, "con".to_string()),
            (QuarantinePhase::Precommit, overlong.clone()),
            (QuarantinePhase::Committed, "LIVE".to_string()),
            (QuarantinePhase::Committed, "con".to_string()),
            (QuarantinePhase::Committed, overlong),
        ];
        for (phase, storage_key) in cases {
            let root = tempdir().unwrap();
            let artifact = write_artifact(root.path(), "live", b"payload", 10);
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate("live").unwrap();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            assert!(transaction.journal.record_values.is_empty());
            transaction.journal.phase = phase;
            transaction.journal.storage_key = storage_key.clone();
            transaction.persist_journal().unwrap();
            let group_path = transaction.group.display_path().to_path_buf();
            let journal_path = group_path.join(QUARANTINE_JOURNAL_FILE_NAME);
            let journal_before = fs::read(&journal_path).unwrap();
            let staged_payload = group_path.join("artifacts/payload.bin");
            let staged_identity = file_identity(&fs::symlink_metadata(&staged_payload).unwrap());
            std::mem::forget(transaction);

            let recovery = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(recovery.conflicted_groups, 1, "{phase:?} {storage_key}");
            assert!(!artifact.exists());
            assert_eq!(fs::read(&journal_path).unwrap(), journal_before);
            assert_eq!(fs::read(&staged_payload).unwrap(), b"payload");
            assert_eq!(
                file_identity(&fs::symlink_metadata(&staged_payload).unwrap()),
                staged_identity
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_swapped_component_kind_authority() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "swapped-kind", b"artifact", 10);
        let event = task_event_log_path(root.path(), "swapped-kind");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        set_modified(&event, 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("swapped-kind").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        assert_eq!(transaction.journal.components.len(), 2);
        let first = transaction.journal.components[0].kind;
        transaction.journal.components[0].kind = transaction.journal.components[1].kind;
        transaction.journal.components[1].kind = first;
        transaction.persist_journal().unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        let report = inspect_task_store(root.path(), 100).unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert_eq!(report.metrics_before.retention_quarantine_groups, 1);
        assert!(!task_artifact_dir(root.path(), "swapped-kind").exists());
        assert!(!event.exists());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_bounds_journal_reads_without_mutating_the_group() {
        let root = tempdir().unwrap();
        let (_, group) = create_quarantine_group(root.path(), "oversized-journal");
        let journal_path = group.display_path().join(QUARANTINE_JOURNAL_FILE_NAME);
        File::create(&journal_path)
            .unwrap()
            .set_len(MAX_TASK_RETENTION_JOURNAL_BYTES as u64 + 1)
            .unwrap();

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert_eq!(
            fs::metadata(journal_path).unwrap().len(),
            MAX_TASK_RETENTION_JOURNAL_BYTES as u64 + 1
        );
        assert!(recovery.issues.iter().any(|issue| {
            issue.kind == "retention_recovery_invalid_journal" && issue.message.contains("exceeds")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn journal_encoder_enforces_its_bound_before_managed_state_is_staged() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "oversized-journal", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("oversized-journal").unwrap();
        let transaction = StagingTransaction::new(&snapshot, candidate).unwrap();

        let error = transaction.encode_journal_with_limit(1).unwrap_err();
        assert!(transaction.staged_components.is_empty());
        drop(transaction);
        let report = inspect_task_store(root.path(), 100).unwrap();

        assert!(error.to_string().contains("journal exceeds"));
        assert!(artifact.exists());
        assert_eq!(report.metrics_before.retention_quarantine_groups, 0);
    }

    #[cfg(unix)]
    #[test]
    fn valid_large_registry_record_can_recover_and_apply_retention() {
        let root = tempdir().unwrap();
        let mut record = inactive_record("large-record", 10);
        record.last_error = Some("x".repeat(9 * 1024 * 1024));
        write_registry(root.path(), [record]);
        assert!(fs::metadata(task_registry_path(root.path())).unwrap().len() > 8 * 1024 * 1024);

        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("large-record").unwrap();
        let transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        assert!(
            fs::metadata(
                transaction
                    .group
                    .display_path()
                    .join(QUARANTINE_JOURNAL_FILE_NAME)
            )
            .unwrap()
            .len()
                > 8 * 1024 * 1024
        );
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        assert_eq!(recovery.restored_precommit_groups, 1);
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("large-record"));

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap();
        assert_eq!(report.retention.removed_tasks, 1);
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("large-record"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_removes_an_empty_group_left_outside_the_journal_window() {
        let root = tempdir().unwrap();
        let (_quarantine, group) = create_quarantine_group(root.path(), "empty-crash-window");
        let group_path = group.display_path().to_path_buf();
        drop(group);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 0);
        assert!(!group_path.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_empty_group_removed"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_removes_an_initial_journal_temp_crash_window() {
        let root = tempdir().unwrap();
        let (_quarantine, group) =
            create_quarantine_group(root.path(), "initial-journal-temp-crash");
        let group_path = group.display_path().to_path_buf();
        fs::write(
            group_path.join(format!("{RETENTION_JOURNAL_WRITE_TEMP_PREFIX}-123-0")),
            b"partial journal",
        )
        .unwrap();
        drop(group);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 0);
        assert!(!group_path.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_unstarted_group_removed"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_preserves_an_ambiguous_generic_journal_deletion_tombstone() {
        let root = tempdir().unwrap();
        write_registry(root.path(), [inactive_record("journal-delete-crash", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("journal-delete-crash").unwrap();
        let transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        let journal_name = OsStr::new(QUARANTINE_JOURNAL_FILE_NAME);
        let journal_identity = transaction
            .group
            .entry_identity(journal_name)
            .unwrap()
            .unwrap();
        let group_tombstone = OsString::from(".deleting-group-123-0");
        transaction
            .quarantine
            .rename_to_noreplace(
                &transaction.group_name,
                &transaction.quarantine,
                &group_tombstone,
            )
            .unwrap();
        let journal_tombstone = transaction
            .group
            .tombstone_entry_verified(journal_name, journal_identity, DELETION_TEMP_PREFIX)
            .unwrap();
        let tombstoned_group_path = transaction.quarantine.display_path().join(&group_tombstone);
        assert!(generated_name_matches(
            &journal_tombstone,
            DELETION_TEMP_PREFIX
        ));
        std::mem::forget(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 1);
        assert!(tombstoned_group_path.exists());
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("journal-delete-crash"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_preserves_a_colliding_final_journal_name() {
        let root = tempdir().unwrap();
        let task_id = "final-journal-collision";
        write_registry(root.path(), [inactive_record(task_id, 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate(task_id).unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        fs::write(
            transaction
                .group
                .display_path()
                .join(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
            b"untrusted collision",
        )
        .unwrap();
        let group_path = transaction.group.display_path().to_path_buf();
        std::mem::forget(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 1);
        assert!(group_path.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
        assert!(group_path
            .join(QUARANTINE_JOURNAL_DELETION_FILE_NAME)
            .exists());
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key(task_id));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_preserves_coexisting_hardlinked_final_journal_names() {
        let root = tempdir().unwrap();
        let task_id = "final-journal-coexistence";
        write_registry(root.path(), [inactive_record(task_id, 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate(task_id).unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let normal = transaction
            .group
            .display_path()
            .join(QUARANTINE_JOURNAL_FILE_NAME);
        let final_name = transaction
            .group
            .display_path()
            .join(QUARANTINE_JOURNAL_DELETION_FILE_NAME);
        fs::hard_link(&normal, &final_name).unwrap();
        let group_path = transaction.group.display_path().to_path_buf();
        std::mem::forget(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 1);
        assert!(group_path.join(QUARANTINE_JOURNAL_FILE_NAME).exists());
        assert!(group_path
            .join(QUARANTINE_JOURNAL_DELETION_FILE_NAME)
            .exists());
        assert!(crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key(task_id));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_finishes_an_authenticated_final_journal_tombstone() {
        let root = tempdir().unwrap();
        let task_id = "final-journal-delete-crash";
        write_paired_registry(root.path(), [inactive_record(task_id, 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate(task_id).unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        assert!(finish_anchored_committed_registry_removal(
            &transaction.daemon,
            root.path(),
            &transaction.journal.record_values,
            transaction.journal.registry_revision,
            transaction.journal.registry_checkpoint_generation,
        )
        .unwrap());
        let journal_name = OsStr::new(QUARANTINE_JOURNAL_FILE_NAME);
        let journal_identity = transaction.journal_identity.unwrap();
        let group_tombstone = OsString::from(".deleting-group-456-0");
        transaction
            .quarantine
            .rename_to_noreplace(
                &transaction.group_name,
                &transaction.quarantine,
                &group_tombstone,
            )
            .unwrap();
        transaction
            .group
            .tombstone_entry_to_verified(
                journal_name,
                journal_identity,
                OsStr::new(QUARANTINE_JOURNAL_DELETION_FILE_NAME),
            )
            .unwrap();
        let tombstoned_group_path = transaction.quarantine.display_path().join(&group_tombstone);
        std::mem::forget(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 0);
        assert_eq!(report.completed_committed_groups, 1);
        assert!(!tombstoned_group_path.exists());
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key(task_id));
        assert_eq!(
            recover_task_store_quarantine(root.path()).unwrap(),
            TaskStoreRecoveryReport::default(),
            "a second recovery pass must converge without recreating state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn precommit_recovery_removes_committed_phase_flip_temp_before_restore() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "phase-flip", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("phase-flip").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let mut committed = transaction.journal.clone();
        committed.phase = QuarantinePhase::Committed;
        fs::write(
            transaction
                .group
                .display_path()
                .join(format!("{RETENTION_JOURNAL_WRITE_TEMP_PREFIX}-123-0")),
            serde_json::to_vec(&committed).unwrap(),
        )
        .unwrap();
        std::mem::forget(transaction);

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.restored_precommit_groups, 1);
        assert_eq!(report.completed_committed_groups, 0);
        assert_eq!(report.conflicted_groups, 0);
        assert_eq!(fs::read(&artifact).unwrap(), b"payload");
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_transient_files_removed"));
    }

    #[cfg(unix)]
    #[test]
    fn precommit_recovery_removes_known_probe_transient_crash_windows() {
        for (index, prefix) in [
            NOREPLACE_PROBE_SOURCE_PREFIX,
            NOREPLACE_PROBE_DESTINATION_PREFIX,
        ]
        .into_iter()
        .enumerate()
        {
            let root = tempdir().unwrap();
            let task_id = format!("transient-{index}");
            let artifact = write_artifact(root.path(), &task_id, b"payload", 10);
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate(&task_id).unwrap();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            fs::write(
                transaction
                    .group
                    .display_path()
                    .join(format!("{prefix}-123-0")),
                b"transient",
            )
            .unwrap();
            std::mem::forget(transaction);

            let report = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(report.restored_precommit_groups, 1, "{prefix}");
            assert_eq!(report.conflicted_groups, 0, "{prefix}");
            assert_eq!(fs::read(&artifact).unwrap(), b"payload", "{prefix}");
            assert!(report
                .issues
                .iter()
                .any(|issue| issue.kind == "retention_recovery_transient_files_removed"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn precommit_recovery_cleans_strict_transient_deletion_tombstones() {
        let probe_source_deletion = generated_deletion_prefix(NOREPLACE_PROBE_SOURCE_PREFIX);
        for (index, prefix) in [
            probe_source_deletion.as_ref(),
            RETENTION_JOURNAL_WRITE_DELETION_TEMP_PREFIX,
            DELETION_TEMP_PREFIX,
        ]
        .into_iter()
        .enumerate()
        {
            let root = tempdir().unwrap();
            let task_id = format!("transient-deletion-{index}");
            let artifact = write_artifact(root.path(), &task_id, b"payload", 10);
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate(&task_id).unwrap();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            fs::write(
                transaction
                    .group
                    .display_path()
                    .join(format!("{prefix}-123-0")),
                b"transient",
            )
            .unwrap();
            std::mem::forget(transaction);

            let report = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(report.restored_precommit_groups, 1, "{prefix}");
            assert_eq!(report.conflicted_groups, 0, "{prefix}");
            assert_eq!(fs::read(&artifact).unwrap(), b"payload", "{prefix}");
            assert!(report
                .issues
                .iter()
                .any(|issue| issue.kind == "retention_recovery_transient_files_removed"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn precommit_recovery_preserves_transient_deletion_lookalikes() {
        for (index, name) in [
            ".noreplace-probe-source-deleting-123-0-extra",
            ".retention-journal-write-deleting-123",
            ".deleting-x-0",
        ]
        .into_iter()
        .enumerate()
        {
            let root = tempdir().unwrap();
            let task_id = format!("transient-lookalike-{index}");
            write_artifact(root.path(), &task_id, b"payload", 10);
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate(&task_id).unwrap();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            let residue = transaction.group.display_path().join(name);
            fs::write(&residue, b"keep").unwrap();
            std::mem::forget(transaction);

            let report = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(report.conflicted_groups, 1, "{name}");
            assert_eq!(fs::read(residue).unwrap(), b"keep", "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_unknown_journal_less_residue_without_mutation() {
        let root = tempdir().unwrap();
        let (_quarantine, group) =
            create_quarantine_group(root.path(), "unknown-journal-less-residue");
        let residue = group.display_path().join("unknown.bin");
        fs::write(&residue, b"keep").unwrap();

        let report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(report.conflicted_groups, 1);
        assert_eq!(fs::read(residue).unwrap(), b"keep");
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_invalid_journal"));
    }

    #[cfg(unix)]
    #[test]
    fn journal_validation_bounds_component_and_record_counts() {
        let root = tempdir().unwrap();
        let storage_key = storage_key_for_task(root.path(), "a/b");
        assert_eq!(storage_key_for_task(root.path(), "a?b"), storage_key);
        let too_many_records = QuarantineJournal {
            schema_version: QUARANTINE_JOURNAL_SCHEMA_VERSION,
            phase: QuarantinePhase::Precommit,
            storage_key: storage_key.clone(),
            record_values: BTreeMap::from([
                (
                    "a/b".to_string(),
                    serde_json::to_value(inactive_record("a/b", 10)).unwrap(),
                ),
                (
                    "a?b".to_string(),
                    serde_json::to_value(inactive_record("a?b", 10)).unwrap(),
                ),
            ]),
            registry_revision: Some(crate::storage::RegistryRevision::ZERO),
            registry_checkpoint_generation: None,
            components: Vec::new(),
        };
        let identity = FileIdentity {
            device: 1,
            inode: 1,
        };
        let too_many_components = QuarantineJournal {
            schema_version: QUARANTINE_JOURNAL_SCHEMA_VERSION,
            phase: QuarantinePhase::Precommit,
            storage_key,
            record_values: BTreeMap::new(),
            registry_revision: None,
            registry_checkpoint_generation: None,
            components: vec![
                JournalComponent {
                    kind: JournalComponentKind::Artifacts,
                    identity,
                },
                JournalComponent {
                    kind: JournalComponentKind::Events,
                    identity,
                },
                JournalComponent {
                    kind: JournalComponentKind::Artifacts,
                    identity,
                },
            ],
        };

        assert!(validate_quarantine_journal(root.path(), &too_many_records).is_err());
        assert!(validate_quarantine_journal(root.path(), &too_many_components).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_requires_private_quarantine_root_and_groups() {
        let root = tempdir().unwrap();
        let (_, group) = create_quarantine_group(root.path(), "insecure-group");
        group
            .write_json_atomically(
                OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                b"{\"invalid\":true}",
                RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            )
            .unwrap();
        fs::set_permissions(group.display_path(), fs::Permissions::from_mode(0o777)).unwrap();

        let group_report = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(group_report.conflicted_groups, 1);
        assert!(group_report
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_invalid_group"));
        assert_eq!(
            fs::metadata(group.display_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o777,
            "authenticity-losing mode remains as durable taint evidence"
        );

        fs::set_permissions(
            root.path().join(STATE_DIR_NAME).join(QUARANTINE_DIR_NAME),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let root_error = recover_task_store_quarantine(root.path()).unwrap_err();
        assert!(matches!(root_error, DaemonCoreError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_preserves_unknown_registry_fields() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "future-record", b"artifact", 10);
        let mut target = serde_json::to_value(inactive_record("future-record", 10)).unwrap();
        target["future_record_field"] = serde_json::json!({"version": 2});
        let mut survivor = serde_json::to_value(inactive_record("survivor", 20)).unwrap();
        survivor["future_record_field"] = serde_json::json!(["keep", 42]);
        let registry = serde_json::json!({
            "tasks": {
                "future-record": target.clone(),
                "survivor": survivor.clone(),
            },
            "future_registry_field": {"keep": true},
            "task_watch_checkpoint_generation": 7,
        });
        fs::write(
            task_registry_path(root.path()),
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();
        fs::write(
            watch_registry_path(root.path()),
            serde_json::to_vec_pretty(&serde_json::json!({
                "watches": [],
                "task_watch_checkpoint_generation": 7,
            }))
            .unwrap(),
        )
        .unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("future-record").unwrap();
        assert_eq!(candidate.record_values["future-record"], target);
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();
        let recovered: serde_json::Value =
            serde_json::from_slice(&fs::read(task_registry_path(root.path())).unwrap()).unwrap();
        let recovered_watch: serde_json::Value =
            serde_json::from_slice(&fs::read(watch_registry_path(root.path())).unwrap()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 1);
        assert!(recovered["tasks"].get("future-record").is_none());
        assert_eq!(recovered["tasks"]["survivor"], survivor);
        assert_eq!(
            recovered["future_registry_field"],
            serde_json::json!({"keep": true})
        );
        let recovered_generation = recovered["task_watch_checkpoint_generation"]
            .as_u64()
            .unwrap();
        assert!(recovered_generation > 7);
        assert_eq!(
            recovered_watch["task_watch_checkpoint_generation"],
            serde_json::json!(recovered_generation)
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_rejects_a_one_sided_checkpoint_generation() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        write_artifact(root.path(), "one-sided-recovery", b"artifact", 10);
        let registry = serde_json::json!({
            "tasks": {
                "one-sided-recovery": inactive_record("one-sided-recovery", 10),
            },
            "task_watch_checkpoint_generation": 7,
        });
        let task_path = task_registry_path(root.path());
        let task_before = serde_json::to_vec_pretty(&registry).unwrap();
        fs::write(&task_path, &task_before).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let mut candidate = snapshot.candidate("one-sided-recovery").unwrap().clone();
        candidate.registry_revision = Some(crate::storage::RegistryRevision::ZERO);
        let mut transaction = StagingTransaction::new(&snapshot, &candidate).unwrap();
        transaction.journal.schema_version = LEGACY_QUARANTINE_JOURNAL_SCHEMA_VERSION;
        transaction.journal.registry_revision = None;
        transaction.journal.registry_checkpoint_generation = None;
        transaction.persist_journal().unwrap();
        transaction.stage_all(&candidate).unwrap();
        transaction.mark_committed().unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert_eq!(fs::read(&task_path).unwrap(), task_before);
        assert!(!watch_registry_path(root.path()).exists());
        assert!(recovery.issues.iter().any(|issue| {
            issue
                .message
                .contains("task/watch registry checkpoint generations disagree")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn committed_recovery_refuses_to_rewrite_duplicate_registry_keys() {
        for duplicate_kind in 0..4 {
            let root = tempdir().unwrap();
            crate::storage::ensure_daemon_dir(root.path()).unwrap();
            write_artifact(root.path(), "duplicate", b"artifact", 10);
            write_registry(root.path(), [inactive_record("duplicate", 10)]);
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate("duplicate").unwrap();
            let expected = candidate.record_values["duplicate"].clone();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            transaction.mark_committed().unwrap();
            drop(transaction);
            let record = serde_json::to_string(&expected).unwrap();
            let raw = match duplicate_kind {
                0 => format!(
                    r#"{{"tasks":{{"duplicate":{record}}},"tasks":{{"duplicate":{record}}}}}"#
                ),
                1 => format!(r#"{{"tasks":{{"duplicate":{record},"duplicate":{record}}}}}"#),
                2 => format!(
                    r#"{{"tasks":{{"duplicate":{{"task_id":"duplicate",{}}}}}}}"#,
                    &record[1..]
                ),
                _ => format!(
                    r#"{{"tasks":{{"duplicate":{{"future":1,"future":2,{}}}}}}}"#,
                    &record[1..]
                ),
            };
            fs::write(task_registry_path(root.path()), raw.as_bytes()).unwrap();

            let recovery = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(recovery.completed_committed_groups, 0);
            assert_eq!(recovery.conflicted_groups, 1);
            assert_eq!(
                fs::read(task_registry_path(root.path())).unwrap(),
                raw.as_bytes()
            );
            assert!(recovery.issues.iter().any(|issue| {
                issue.kind == "retention_recovery_commit_failed"
                    && issue.message.contains("duplicate")
            }));
            assert_eq!(
                inspect_task_store(root.path(), 100)
                    .unwrap()
                    .metrics_before
                    .retention_quarantine_groups,
                1
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_duplicate_journal_keys_without_mutation() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "duplicate-journal", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("duplicate-journal").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let journal_path = transaction
            .group
            .display_path()
            .join(QUARANTINE_JOURNAL_FILE_NAME);
        let original = String::from_utf8(fs::read(&journal_path).unwrap()).unwrap();
        let duplicate = format!(r#"{{"phase":"committed",{}"#, &original[1..]);
        fs::write(&journal_path, duplicate.as_bytes()).unwrap();
        let staged_payload = transaction
            .group
            .display_path()
            .join(JournalComponentKind::Artifacts.staged_name())
            .join("payload.bin");
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.restored_precommit_groups, 0);
        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert!(!artifact.exists());
        assert_eq!(fs::read(staged_payload).unwrap(), b"payload");
        assert_eq!(fs::read(journal_path).unwrap(), duplicate.as_bytes());
        assert!(recovery.issues.iter().any(|issue| {
            issue.kind == "retention_recovery_invalid_journal"
                && issue.message.contains("duplicate")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_duplicate_record_authority_keys_without_mutation() {
        for nested_record_key in [false, true] {
            let root = tempdir().unwrap();
            let artifact = write_artifact(root.path(), "victim", b"payload", 10);
            write_registry(root.path(), [inactive_record("victim", 10)]);
            let registry_path = task_registry_path(root.path());
            let registry_before = fs::read(&registry_path).unwrap();
            let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
            let candidate = snapshot.candidate("victim").unwrap();
            let record = serde_json::to_string(&candidate.record_values["victim"]).unwrap();
            let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
            transaction.stage_all(candidate).unwrap();
            transaction.mark_committed().unwrap();
            let group_path = transaction.group.display_path().to_path_buf();
            let journal_path = group_path.join(QUARANTINE_JOURNAL_FILE_NAME);
            let staged_payload = group_path
                .join(JournalComponentKind::Artifacts.staged_name())
                .join("payload.bin");
            let original = String::from_utf8(fs::read(&journal_path).unwrap()).unwrap();
            let duplicate = if nested_record_key {
                let authority = r#""task_id":"victim""#;
                assert!(original.contains(authority));
                original.replacen(authority, r#""task_id":"other","task\u005fid":"victim""#, 1)
            } else {
                let authority = format!(r#""record_values":{{"victim":{record}}}"#);
                assert!(original.contains(&authority));
                original.replacen(
                    &authority,
                    &format!(r#""record_values":{{"victim":{record},"v\u0069ctim":{record}}}"#),
                    1,
                )
            };
            fs::write(&journal_path, duplicate.as_bytes()).unwrap();
            drop(transaction);

            let recovery = recover_task_store_quarantine(root.path()).unwrap();

            assert_eq!(recovery.restored_precommit_groups, 0);
            assert_eq!(recovery.completed_committed_groups, 0);
            assert_eq!(recovery.conflicted_groups, 1);
            assert!(!artifact.exists());
            assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
            assert_eq!(fs::read(&staged_payload).unwrap(), b"payload");
            assert_eq!(fs::read(&journal_path).unwrap(), duplicate.as_bytes());
            assert!(group_path.exists());
            assert!(
                recovery.issues.iter().any(|issue| {
                    issue.kind == "retention_recovery_invalid_journal"
                        && issue.message.contains("duplicate")
                }),
                "nested_record_key={nested_record_key} recovery={recovery:#?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn committed_structural_budget_failure_preserves_registry_component_and_journal() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "budget-committed", b"payload", 10);
        write_registry(root.path(), [inactive_record("budget-committed", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("budget-committed").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        transaction.mark_committed().unwrap();
        let group_path = transaction.group.display_path().to_path_buf();
        let journal_path = group_path.join(QUARANTINE_JOURNAL_FILE_NAME);
        let raw = structurally_over_budget_journal(&fs::read(&journal_path).unwrap());
        fs::write(&journal_path, &raw).unwrap();
        let staged_payload = group_path.join("artifacts/payload.bin");
        let staged_identity = file_identity(&fs::symlink_metadata(&staged_payload).unwrap());
        let registry_path = task_registry_path(root.path());
        let registry_before = fs::read(&registry_path).unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.completed_committed_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert!(!artifact.exists());
        assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
        assert_eq!(fs::read(&journal_path).unwrap(), raw);
        assert_eq!(fs::read(&staged_payload).unwrap(), b"payload");
        assert_eq!(
            file_identity(&fs::symlink_metadata(&staged_payload).unwrap()),
            staged_identity
        );
        assert!(recovery.issues.iter().any(|issue| {
            issue.kind == "retention_recovery_invalid_journal"
                && issue.message.contains("entries per container")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn precommit_structural_budget_failure_preserves_component_transient_and_journal() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "budget-precommit", b"payload", 10);
        write_registry(root.path(), [inactive_record("budget-precommit", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("budget-precommit").unwrap();
        let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
        transaction.stage_all(candidate).unwrap();
        let group_path = transaction.group.display_path().to_path_buf();
        let journal_path = group_path.join(QUARANTINE_JOURNAL_FILE_NAME);
        let raw = structurally_over_budget_journal(&fs::read(&journal_path).unwrap());
        fs::write(&journal_path, &raw).unwrap();
        let transient = group_path.join(format!("{RETENTION_JOURNAL_WRITE_TEMP_PREFIX}-123-0"));
        fs::write(&transient, b"recognized transient").unwrap();
        let staged_payload = group_path.join("artifacts/payload.bin");
        let staged_identity = file_identity(&fs::symlink_metadata(&staged_payload).unwrap());
        let registry_path = task_registry_path(root.path());
        let registry_before = fs::read(&registry_path).unwrap();
        std::mem::forget(transaction);

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.restored_precommit_groups, 0);
        assert_eq!(recovery.conflicted_groups, 1);
        assert!(!artifact.exists());
        assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
        assert_eq!(fs::read(&journal_path).unwrap(), raw);
        assert_eq!(fs::read(&transient).unwrap(), b"recognized transient");
        assert_eq!(fs::read(&staged_payload).unwrap(), b"payload");
        assert_eq!(
            file_identity(&fs::symlink_metadata(&staged_payload).unwrap()),
            staged_identity
        );
    }

    #[cfg(unix)]
    fn structurally_over_budget_journal(valid_journal: &[u8]) -> Vec<u8> {
        assert_eq!(valid_journal.first(), Some(&b'{'));
        let entries = crate::storage::MAX_AUTHORITY_JSON_ENTRIES_PER_CONTAINER + 1;
        let mut raw = Vec::with_capacity(
            valid_journal
                .len()
                .saturating_add(entries.saturating_mul(5)),
        );
        raw.extend_from_slice(br#"{"future":["#);
        for index in 0..entries {
            if index > 0 {
                raw.push(b',');
            }
            raw.extend_from_slice(b"null");
        }
        raw.extend_from_slice(b"],");
        raw.extend_from_slice(&valid_journal[1..]);
        assert!(raw.len() < MAX_TASK_RETENTION_JOURNAL_BYTES);
        raw
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_journal_destination_authority_fields() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(STATE_DIR_NAME)).unwrap();
        let state = CapabilityDir::open(&root.path().join(STATE_DIR_NAME)).unwrap();
        let quarantine = state
            .ensure_dir(OsStr::new(QUARANTINE_DIR_NAME), 0o700)
            .unwrap();
        let group = quarantine
            .create_dir(OsStr::new("malicious"), 0o700)
            .unwrap();
        let identity = group.identity();
        let journal = serde_json::json!({
            "schema_version": QUARANTINE_JOURNAL_SCHEMA_VERSION,
            "phase": "precommit",
            "storage_key": "task",
            "record_values": {},
            "components": [{
                "kind": "artifacts",
                "identity": {
                    "device": identity.device,
                    "inode": identity.inode,
                },
                "original_parent_components": [".."],
                "original_name": "outside",
            }],
        });
        group
            .write_json_atomically(
                OsStr::new(QUARANTINE_JOURNAL_FILE_NAME),
                &serde_json::to_vec(&journal).unwrap(),
                RETENTION_JOURNAL_WRITE_TEMP_PREFIX,
            )
            .unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();

        let recovery = recover_task_store_quarantine(root.path()).unwrap();

        assert_eq!(recovery.conflicted_groups, 1);
        assert_eq!(fs::read(outside).unwrap(), b"keep");
        assert!(recovery
            .issues
            .iter()
            .any(|issue| issue.kind == "retention_recovery_invalid_journal"));
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_symlink_swap_cannot_redirect_committed_deletion() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("keep");
        fs::write(&outside_file, b"keep").unwrap();
        let artifact = write_artifact(root.path(), "swap", b"payload", 10);
        write_registry(root.path(), [inactive_record("swap", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("swap").unwrap();
        let quarantine_path = root.path().join(STATE_DIR_NAME).join(QUARANTINE_DIR_NAME);
        let held_path = root
            .path()
            .join(STATE_DIR_NAME)
            .join(".retention-trash-held");

        let outcome = apply_candidate_with_observers(
            &snapshot,
            candidate,
            || Ok(()),
            || Ok(()),
            || {
                fs::rename(&quarantine_path, &held_path).map_err(|source| {
                    DaemonCoreError::io(
                        "failed to move quarantine during test",
                        &quarantine_path,
                        source,
                    )
                })?;
                symlink(outside.path(), &quarantine_path).map_err(|source| {
                    DaemonCoreError::io(
                        "failed to swap quarantine during test",
                        &quarantine_path,
                        source,
                    )
                })
            },
        )
        .unwrap();

        assert_eq!(outcome, RetentionOutcome::Removed);
        assert!(!artifact.exists());
        assert_eq!(fs::read(&outside_file).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&quarantine_path)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_file(quarantine_path).unwrap();
        fs::remove_dir(held_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn second_candidate_failure_returns_complete_partial_accounting() {
        let root = tempdir().unwrap();
        let first = write_artifact(root.path(), "first", b"one", 10);
        let second = write_artifact(root.path(), "second", b"two", 20);
        INJECT_FAILURE_AFTER_STAGE_FOR.with(|configured| {
            configured.replace(Some("second".to_string()));
        });

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert_eq!(
            report
                .actions
                .iter()
                .map(|action| (action.storage_key.as_str(), action.outcome))
                .collect::<Vec<_>>(),
            vec![
                ("first", RetentionOutcome::Removed),
                ("second", RetentionOutcome::Failed),
            ]
        );
        assert_eq!(
            (
                report.retention.removed_tasks,
                report.retention.removed_logical_bytes,
                report.retention.failed_tasks,
                report.retention.failed_logical_bytes,
                report.retention.remaining_managed_logical_bytes,
            ),
            (1, 3, 1, 3, 3)
        );
        assert!(!first.exists());
        assert_eq!(fs::read(second).unwrap(), b"two");
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "candidate_cleanup_failed"));
    }

    #[cfg(unix)]
    #[test]
    fn committed_partial_deletion_is_failed_with_exact_byte_progress() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "partial-commit", b"artifact", 10);
        let event = task_event_log_path(root.path(), "partial-commit");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event").unwrap();
        set_modified(&event, 10);
        write_registry(root.path(), [inactive_record("partial-commit", 10)]);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("partial-commit").unwrap();
        let expected_total = candidate.logical_bytes();
        let expected_remaining = candidate.event.as_ref().unwrap().scan.logical_bytes;
        INJECT_COMMITTED_PARTIAL_DELETE_FOR.with(|configured| {
            configured.replace(Some("partial-commit".to_string()));
        });

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();
        let action = &report.actions[0];

        assert_eq!(action.outcome, RetentionOutcome::Failed);
        assert_eq!(action.logical_bytes, expected_total);
        assert_eq!(action.remaining_logical_bytes, expected_remaining);
        assert_eq!(
            action.removed_logical_bytes,
            expected_total - expected_remaining
        );
        assert_eq!(report.retention.removed_tasks, 0);
        assert_eq!(
            report.retention.removed_logical_bytes,
            expected_total - expected_remaining
        );
        assert!(action.byte_accounting_reliable);
        assert!(report.retention.action_byte_accounting_reliable);
        assert_eq!(report.retention.failed_tasks, 1);
        assert_eq!(report.retention.failed_logical_bytes, expected_remaining);
        assert!(!artifact.exists());
        assert!(!event.exists());
        assert!(!crate::storage::load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("partial-commit"));
        assert!(report.metrics_after.retention_quarantine_logical_bytes >= expected_remaining);
    }

    #[cfg(unix)]
    #[test]
    fn nested_committed_partial_deletion_measures_only_surviving_children() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let artifact = task_artifact_dir(root.path(), "nested-partial");
        fs::create_dir_all(&artifact).unwrap();
        let removed_child = artifact.join("a-removed.bin");
        let surviving_child = artifact.join("z-survives.bin");
        fs::write(&removed_child, b"one").unwrap();
        fs::write(&surviving_child, b"second").unwrap();
        set_modified(&removed_child, 10);
        set_modified(&surviving_child, 10);
        set_modified(&artifact, 10);
        INJECT_COMMITTED_NESTED_PARTIAL_FAILURE_FOR.with(|configured| {
            configured.replace(Some("nested-partial".to_string()));
        });

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();
        let action = &report.actions[0];

        assert_eq!(action.outcome, RetentionOutcome::Failed);
        assert_eq!(action.logical_bytes, 9);
        assert_eq!(action.removed_logical_bytes, 3);
        assert_eq!(action.remaining_logical_bytes, 6);
        assert!(action.byte_accounting_reliable);
        assert!(report.retention.action_byte_accounting_reliable);
        assert_eq!(report.retention.removed_logical_bytes, 3);
        assert_eq!(report.retention.failed_logical_bytes, 6);
        assert!(!artifact.exists());
        assert!(report.metrics_after.retention_quarantine_logical_bytes >= 6);
    }

    #[cfg(unix)]
    #[test]
    fn committed_measurement_failure_uses_conservative_unreliable_accounting() {
        let root = tempdir().unwrap();
        let artifact_file = write_artifact(root.path(), "measurement-failure", b"payload", 10);
        let artifact = artifact_file.parent().unwrap().to_path_buf();
        INJECT_COMMITTED_MEASUREMENT_FAILURE_FOR.with(|configured| {
            configured.replace(Some("measurement-failure".to_string()));
        });

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();
        let action = &report.actions[0];

        assert_eq!(action.outcome, RetentionOutcome::Failed);
        assert_eq!(action.logical_bytes, 7);
        assert_eq!(action.removed_logical_bytes, 0);
        assert_eq!(action.remaining_logical_bytes, 7);
        assert!(!action.byte_accounting_reliable);
        assert!(!report.retention.action_byte_accounting_reliable);
        assert_eq!(report.retention.removed_logical_bytes, 0);
        assert_eq!(report.retention.failed_logical_bytes, 7);
        assert_eq!(
            fs::read(artifact.join("replacement.bin")).unwrap(),
            b"replacement"
        );
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "candidate_accounting_failed"));
        assert!(report.metrics_after.retention_quarantine_groups >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn post_apply_rescan_failure_preserves_completed_action_results() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "rescan-failure", b"payload", 10);
        INJECT_POST_APPLY_RESCAN_FAILURE.with(|configured| configured.set(true));

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert!(!report.retention.final_rescan_reliable);
        assert_eq!(report.actions[0].outcome, RetentionOutcome::Removed);
        assert_eq!(
            report.actions[0].removed_logical_bytes,
            b"payload".len() as u64
        );
        assert_eq!(report.actions[0].remaining_logical_bytes, 0);
        assert_eq!(
            report.retention.removed_logical_bytes,
            b"payload".len() as u64
        );
        assert!(!artifact.exists());
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "post_apply_rescan_failed"));
    }

    #[cfg(unix)]
    #[test]
    fn identity_change_between_plan_and_apply_is_skipped() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "raced", b"old", 10);
        let mut snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let mut plan = build_plan(&snapshot, RetentionOptions::dry_run(Some(1), None).apply());
        let task_dir = task_artifact_dir(root.path(), "raced");
        let old_directory = fs::File::open(&task_dir).unwrap();
        fs::remove_dir_all(&task_dir).unwrap();
        write_artifact(root.path(), "raced", b"new", 10);
        let (lease, admission) = acquire_retention_guards(root.path());

        apply_plan(&mut snapshot, &mut plan, &lease, &admission).unwrap();

        assert_eq!(plan.actions[0].outcome, RetentionOutcome::Skipped);
        assert_eq!(fs::read(task_dir.join("payload.bin")).unwrap(), b"new");
        drop(old_directory);
    }

    #[cfg(unix)]
    #[test]
    fn same_size_content_change_between_plan_and_apply_is_skipped() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "raced", b"old", 10);
        let mut snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let mut plan = build_plan(&snapshot, RetentionOptions::dry_run(Some(1), None).apply());
        fs::write(&artifact, b"new").unwrap();
        set_modified(&artifact, 11);
        let (lease, admission) = acquire_retention_guards(root.path());

        apply_plan(&mut snapshot, &mut plan, &lease, &admission).unwrap();

        assert_eq!(plan.actions[0].outcome, RetentionOutcome::Skipped);
        assert_eq!(fs::read(artifact).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn apply_revalidates_only_managed_candidate_paths() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("keep.txt");
        fs::write(&outside_file, b"keep").unwrap();
        let artifact = write_artifact(root.path(), "stale", b"old", 10);
        let unrelated = root.path().join(".packet28/index");
        fs::create_dir_all(&unrelated).unwrap();
        symlink(outside.path(), unrelated.join("external")).unwrap();
        let mut snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let mut plan = build_plan(&snapshot, RetentionOptions::dry_run(Some(1), None).apply());
        let (lease, admission) = acquire_retention_guards(root.path());

        apply_plan(&mut snapshot, &mut plan, &lease, &admission).unwrap();

        assert_eq!(
            plan.actions[0].outcome,
            RetentionOutcome::Removed,
            "issues: {:?}",
            snapshot.issues
        );
        assert!(!artifact.exists());
        assert!(outside_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn orphan_that_becomes_active_after_staging_is_preserved() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "newly-active", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("newly-active").unwrap();
        let staged = root.path().join(".packet28/staged-for-test");
        fs::rename(task_artifact_dir(root.path(), "newly-active"), &staged).unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "newly-active".to_string(),
                session_id: None,
                updated_at_unix: 50,
            })
            .unwrap(),
        )
        .unwrap();
        let state = CapabilityDir::open(&snapshot.state_root).unwrap();
        let daemon = state.open_dir(OsStr::new("daemon")).unwrap();

        assert!(!candidate_remains_safe_after_staging_anchored(
            &snapshot, candidate, &state, &daemon
        )
        .unwrap());
        assert!(staged.exists());
    }

    #[test]
    fn apply_requires_an_explicit_bound() {
        let root = tempdir().unwrap();

        let error =
            retain_task_store(root.path(), 100, RetentionOptions::inspect().apply()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRetentionPolicy { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ready_daemon_blocks_explicit_apply() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "old-task", b"payload", 10);
        let ready = ready_path(root.path());
        fs::create_dir_all(ready.parent().unwrap()).unwrap();
        fs::write(&ready, "ready").unwrap();
        let expected_ready = fs::canonicalize(&ready).unwrap();

        let error = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap_err();

        assert!(
            matches!(
            error,
            DaemonCoreError::RetentionBlockedByDaemon { ref path } if path == &expected_ready
            ),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_lease_acquired_first_blocks_apply_without_mutation() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "old-task", b"payload", 10);
        let daemon_lease = acquire_daemon_task_store_lease(root.path()).unwrap();

        let error = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::RetentionBlockedByDaemon { .. }
        ));
        assert!(artifact.exists());
        drop(daemon_lease);
    }

    #[cfg(unix)]
    #[test]
    fn foreign_managed_root_preflight_fails_before_lock_or_temp_mutation() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "foreign-root", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let registry_temp =
            daemon_dir(root.path()).join(format!("{TASK_REGISTRY_WRITE_TEMP_PREFIX}-foreign"));
        fs::write(&registry_temp, b"keep").unwrap();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        crate::task_store_lease::inject_foreign_device_for_retention_preflight(task_artifacts_dir(
            &canonical_root,
        ));

        let error = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("another filesystem"));
        assert!(!task_store_lifecycle_lock_path(root.path()).exists());
        assert!(!daemon_instance_lock_path(root.path()).exists());
        assert_eq!(fs::read(registry_temp).unwrap(), b"keep");
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert!(!root
            .path()
            .join(STATE_DIR_NAME)
            .join(QUARANTINE_DIR_NAME)
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_instance_in_conversion_window_blocks_apply_before_recovery_mutation() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "conversion-window", b"payload", 10);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let registry_temp =
            daemon_dir(root.path()).join(format!("{TASK_REGISTRY_WRITE_TEMP_PREFIX}-blocked"));
        fs::write(&registry_temp, b"keep").unwrap();
        let (_quarantine, group) = create_quarantine_group(root.path(), "blocked-recovery");
        let residue = group.display_path().join("keep");
        fs::write(&residue, b"keep").unwrap();
        let instance_lease = acquire_daemon_instance_lease(root.path()).unwrap();

        let error = retain_task_store_with_lease_observer(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
            || panic!("observer must not run before instance admission"),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::RetentionBlockedByDaemon { ref path }
                if path == &daemon_instance_lock_path(&fs::canonicalize(root.path()).unwrap())
        ));
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(fs::read(registry_temp).unwrap(), b"keep");
        assert_eq!(fs::read(residue).unwrap(), b"keep");
        drop(instance_lease);
    }

    #[cfg(unix)]
    #[test]
    fn apply_fails_closed_on_state_replacement_after_instance_admission() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "retained-root", b"original", 10);
        write_registry(root.path(), [inactive_record("retained-root", 10)]);
        let state = root.path().join(STATE_DIR_NAME);
        let held = root.path().join(".packet28-held");
        let replacement_marker = state.join("replacement-marker");

        let error = retain_task_store_with_lease_observer(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
            || {
                fs::rename(&state, &held).unwrap();
                fs::create_dir(&state).unwrap();
                fs::create_dir(state.join("daemon")).unwrap();
                fs::write(&replacement_marker, b"replacement").unwrap();
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("detached"));
        assert_eq!(
            fs::read(held.join("task/retained-root/payload.bin")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(replacement_marker).unwrap(), b"replacement");
        assert!(!held.join(QUARANTINE_DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn apply_rechecks_quarantine_under_lease_even_when_initial_plan_is_empty() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "late-quarantine", b"payload", 100);
        crate::storage::ensure_daemon_dir(root.path()).unwrap();

        let report = retain_task_store_with_lease_observer(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1_000), None).apply(),
            || {
                let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
                let candidate = snapshot.candidate("late-quarantine").unwrap();
                let mut transaction = StagingTransaction::new(&snapshot, candidate).unwrap();
                transaction.stage_all(candidate).unwrap();
                std::mem::forget(transaction);
            },
        )
        .unwrap();

        assert!(report.actions.is_empty());
        assert_eq!(report.retention.recovered_precommit_groups, 1);
        assert_eq!(report.retention.recovery_conflicted_groups, 0);
        assert_eq!(fs::read(artifact).unwrap(), b"payload");
        assert_eq!(report.metrics_after.retention_quarantine_groups, 0);
    }

    #[cfg(unix)]
    #[test]
    fn apply_replans_size_policy_after_a_conforming_prelease_writer() {
        let root = tempdir().unwrap();
        let artifact_a = write_artifact(root.path(), "older-a", b"same-size", 10);
        let artifact_b = write_artifact(root.path(), "newer-b", b"same-size", 20);
        let initial = StoreSnapshot::load(root.path(), 100).unwrap();
        let limit = initial.candidate("older-a").unwrap().logical_bytes();
        assert_eq!(initial.candidate("newer-b").unwrap().logical_bytes(), limit);
        let initial_plan = build_plan(
            &initial,
            RetentionOptions::dry_run(None, Some(limit)).apply(),
        );
        assert_eq!(initial_plan.actions.len(), 1);
        assert_eq!(initial_plan.actions[0].storage_key, "older-a");
        let unrelated_root = root.path().join(STATE_DIR_NAME).join("unrelated-deep");
        let mut unrelated_leaf = unrelated_root.clone();
        for _ in 0..=MAX_RETENTION_SCAN_DEPTH {
            unrelated_leaf.push("d");
        }
        fs::create_dir_all(&unrelated_leaf).unwrap();

        let writer_lease = acquire_task_store_writer_lease(root.path()).unwrap();
        let cleanup_root = root.path().to_path_buf();
        let (initial_scan_tx, initial_scan_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            retain_task_store_with_lease_observers(
                &cleanup_root,
                100,
                RetentionOptions::dry_run(None, Some(limit)).apply(),
                || {
                    initial_scan_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                },
                || {},
            )
        });
        initial_scan_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        fs::remove_dir_all(unrelated_root).unwrap();
        fs::remove_dir_all(task_artifact_dir(root.path(), "newer-b")).unwrap();
        drop(writer_lease);
        continue_tx.send(()).unwrap();
        let report = cleanup.join().unwrap().unwrap();

        assert!(artifact_a.exists());
        assert!(!artifact_b.exists());
        assert_eq!(
            (
                report.metrics_before.managed_task_logical_bytes,
                report.retention.planned_tasks,
                report.retention.planned_logical_bytes,
                report.retention.protected_tasks,
                report.retention.protected_logical_bytes,
                report.actions.len(),
            ),
            (limit, 0, 0, 0, 0, 0)
        );
        assert_eq!(report.retention.remaining_over_limit_bytes, 0);
        assert_eq!(report.retention.remaining_managed_logical_bytes, limit);
    }

    #[cfg(unix)]
    #[test]
    fn apply_recovers_stale_registry_write_temp_before_reporting_bound() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let temp =
            daemon_dir(root.path()).join(format!("{TASK_REGISTRY_WRITE_TEMP_PREFIX}-4242-7"));
        fs::write(&temp, vec![b'x'; 1024 * 1024]).unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert!(!temp.exists());
        assert!(report.actions.is_empty());
        assert_eq!(report.metrics_after.managed_task_logical_bytes, 0);
        assert_eq!(report.retention.remaining_managed_logical_bytes, 0);
        assert_eq!(report.retention.remaining_over_limit_bytes, 0);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.kind == "task_registry_write_temp_recovered"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_recovers_only_strict_active_task_write_temps_under_its_lock() {
        let root = tempdir().unwrap();
        crate::storage::ensure_daemon_dir(root.path()).unwrap();
        let agent = agent_runtime_dir(root.path());
        fs::create_dir_all(&agent).unwrap();
        let temp = agent.join(format!("{ACTIVE_TASK_WRITE_TEMP_PREFIX}-4242-7"));
        let lookalike = agent.join(format!("{ACTIVE_TASK_WRITE_TEMP_PREFIX}-keep"));
        fs::write(&temp, b"stale").unwrap();
        fs::write(&lookalike, b"keep").unwrap();

        let report = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(None, Some(0)).apply(),
        )
        .unwrap();

        assert!(!temp.exists());
        assert_eq!(fs::read(lookalike).unwrap(), b"keep");
        assert!(report.actions.is_empty());
        let issue = report
            .issues
            .iter()
            .find(|issue| issue.kind == "active_task_write_temp_recovered")
            .unwrap();
        assert!(issue.message.contains("removed 1 stale"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_lease_acquired_first_blocks_daemon_until_cleanup_finishes() {
        let root = tempdir().unwrap();
        let artifact = write_artifact(root.path(), "old-task", b"payload", 10);
        let lease_acquired = Arc::new(Barrier::new(2));
        let continue_cleanup = Arc::new(Barrier::new(2));
        let cleanup_root = root.path().to_path_buf();
        let cleanup_acquired = lease_acquired.clone();
        let cleanup_continue = continue_cleanup.clone();
        let cleanup = thread::spawn(move || {
            retain_task_store_with_lease_observer(
                &cleanup_root,
                100,
                RetentionOptions::dry_run(Some(1), None).apply(),
                || {
                    cleanup_acquired.wait();
                    cleanup_continue.wait();
                },
            )
        });
        lease_acquired.wait();

        let daemon_root = root.path().to_path_buf();
        let (daemon_acquired_tx, daemon_acquired_rx) = mpsc::channel();
        let daemon = thread::spawn(move || {
            let lease = acquire_daemon_task_store_lease(&daemon_root).unwrap();
            daemon_acquired_tx.send(()).unwrap();
            lease
        });
        assert!(matches!(
            daemon_acquired_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        continue_cleanup.wait();
        let report = cleanup.join().unwrap().unwrap();
        daemon_acquired_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(daemon.join().unwrap());

        assert_eq!(report.retention.removed_tasks, 1);
        assert!(!artifact.exists());
    }

    #[cfg(unix)]
    #[test]
    fn writer_attempt_after_final_revalidation_cannot_be_deleted_by_cleanup() {
        let root = tempdir().unwrap();
        write_artifact(root.path(), "writer-race", b"old", 10);
        let snapshot = StoreSnapshot::load(root.path(), 100).unwrap();
        let candidate = snapshot.candidate("writer-race").unwrap().clone();
        let cleanup_root = root.path().to_path_buf();
        let (final_tx, final_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            let lease = try_acquire_task_store_retention_lease(&cleanup_root)
                .unwrap()
                .unwrap();
            let outcome = apply_candidate_with_observers(
                &snapshot,
                &candidate,
                || Ok(()),
                || Ok(()),
                || {
                    final_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                    Ok(())
                },
            );
            drop(lease);
            outcome
        });
        final_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let writer_root = root.path().to_path_buf();
        let (written_tx, written_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let _lease = acquire_task_store_writer_lease(&writer_root).unwrap();
            let active = active_task_path(&writer_root);
            fs::create_dir_all(active.parent().unwrap()).unwrap();
            fs::write(
                active,
                serde_json::to_vec(&ActiveTaskRecord {
                    task_id: "writer-race".to_string(),
                    session_id: Some("new-session".to_string()),
                    updated_at_unix: 101,
                })
                .unwrap(),
            )
            .unwrap();
            write_artifact(&writer_root, "writer-race", b"new", 101);
            written_tx.send(()).unwrap();
        });
        assert!(matches!(
            written_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        continue_tx.send(()).unwrap();
        assert_eq!(cleanup.join().unwrap().unwrap(), RetentionOutcome::Removed);
        written_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.join().unwrap();

        assert_eq!(
            fs::read(task_artifact_dir(root.path(), "writer-race").join("payload.bin")).unwrap(),
            b"new"
        );
        assert_eq!(
            read_active_task(
                root.path(),
                &CapabilityDir::open(&root.path().join(STATE_DIR_NAME)).unwrap(),
                &mut Vec::new(),
            )
            .unwrap()
            .task_id
            .as_deref(),
            Some("writer-race"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_ready_marker_still_blocks_explicit_apply() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        write_artifact(root.path(), "old-task", b"payload", 10);
        let ready = ready_path(root.path());
        fs::create_dir_all(ready.parent().unwrap()).unwrap();
        symlink(root.path().join("missing-ready-target"), &ready).unwrap();
        let expected_ready = ready_path(&fs::canonicalize(root.path()).unwrap());

        let error = retain_task_store(
            root.path(),
            100,
            RetentionOptions::dry_run(Some(1), None).apply(),
        )
        .unwrap_err();

        assert!(
            matches!(
                error,
                DaemonCoreError::RetentionBlockedByDaemon { ref path } if path == &expected_ready
            ),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), root.path().join(STATE_DIR_NAME)).unwrap();

        let error = inspect_task_store(root.path(), 100).unwrap_err();

        assert!(matches!(error, DaemonCoreError::UnsafeStateRoot { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_state_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        symlink(
            root.path().join("missing"),
            root.path().join(STATE_DIR_NAME),
        )
        .unwrap();

        let error = inspect_task_store(root.path(), 100).unwrap_err();

        assert!(matches!(error, DaemonCoreError::UnsafeStateRoot { .. }));
    }
}
