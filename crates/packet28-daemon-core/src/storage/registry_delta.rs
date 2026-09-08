use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry, WatchRegistration, WatchRegistry};
#[cfg(not(unix))]
use packet28_state_fs::{FileAccess, StateDir, StateFile};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::*;

pub(crate) const REGISTRY_DELTA_WAL_FILE_NAME: &str = "task-watch-registry-delta-v1.wal";
const WAL_MAGIC: [u8; 8] = *b"P28RDW01";
const FRAME_MAGIC: [u8; 8] = *b"P28RDF01";
const FRAME_FOOTER_MAGIC: [u8; 8] = *b"P28RDE01";
const WAL_FORMAT_VERSION: u32 = 1;
const FRAME_FORMAT_VERSION: u32 = 1;
const WAL_HEADER_BYTES: usize = 56;
#[cfg(test)]
pub(crate) const REGISTRY_DELTA_WAL_HEADER_BYTES: usize = WAL_HEADER_BYTES;
const FRAME_HEADER_BYTES: usize = 72;
const FRAME_FOOTER_BYTES: usize = 56;

#[cfg(test)]
std::thread_local! {
    static FAST_TAIL_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static APPLY_WATCH_RECORDS_SCANNED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Maximum encoded JSON payload accepted for one atomic registry delta.
///
/// One transition may legitimately replace both registries at their existing
/// public size ceilings. The extra MiB covers the delta envelope and keys, so
/// adopting the WAL does not narrow the pre-existing checkpoint contract.
pub const MAX_REGISTRY_DELTA_FRAME_BYTES: usize =
    MAX_TASK_REGISTRY_BYTES + MAX_WATCH_REGISTRY_BYTES + 1024 * 1024;
/// Maximum durable registry-delta WAL size before a checkpoint is required.
pub const MAX_REGISTRY_DELTA_WAL_BYTES: usize = 256 * 1024 * 1024;
const _: () =
    assert!(MAX_REGISTRY_DELTA_FRAME_BYTES >= MAX_TASK_REGISTRY_BYTES + MAX_WATCH_REGISTRY_BYTES);
const _: () = assert!(MAX_REGISTRY_DELTA_FRAME_BYTES < MAX_REGISTRY_DELTA_WAL_BYTES);

/// Monotonic durable revision of task/watch registry authority.
///
/// Revision zero denotes legacy checkpoint state before the first WAL frame.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct RegistryRevision(u64);

impl RegistryRevision {
    /// Legacy checkpoint watermark before the first delta.
    pub const ZERO: Self = Self(0);

    /// Creates a revision from its durable integer representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the durable integer representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the following revision, or `None` at exhaustion.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Contiguous revisions represented by one coalesced WAL frame.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRevisionRange {
    /// First revision represented by the frame.
    pub first: RegistryRevision,
    /// Last revision represented by the frame.
    pub last: RegistryRevision,
}

impl RegistryRevisionRange {
    /// Creates a non-zero, ascending revision range.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryDeltaValidationError::InvalidRevisionRange`] for a
    /// zero first revision or descending bounds.
    pub fn new(
        first: RegistryRevision,
        last: RegistryRevision,
    ) -> std::result::Result<Self, RegistryDeltaValidationError> {
        let range = Self { first, last };
        range.validate()?;
        Ok(range)
    }

    /// Creates one single-revision range.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryDeltaValidationError::InvalidRevisionRange`] for
    /// revision zero.
    pub fn single(
        revision: RegistryRevision,
    ) -> std::result::Result<Self, RegistryDeltaValidationError> {
        Self::new(revision, revision)
    }

    fn validate(self) -> std::result::Result<(), RegistryDeltaValidationError> {
        if self.first == RegistryRevision::ZERO || self.first > self.last {
            return Err(RegistryDeltaValidationError::InvalidRevisionRange {
                first: self.first.get(),
                last: self.last.get(),
            });
        }
        Ok(())
    }
}

/// Invalid caller-provided registry delta.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RegistryDeltaValidationError {
    /// A revision range is zero-based or descending.
    #[error("registry revision range must be non-zero and ascending, received {first}..={last}")]
    InvalidRevisionRange {
        /// First supplied revision.
        first: u64,
        /// Last supplied revision.
        last: u64,
    },
    /// A task upsert map key does not equal its embedded identifier.
    #[error("task upsert key '{key}' does not match embedded task identifier '{record_id}'")]
    TaskIdentifierMismatch {
        /// Map key supplied by the caller.
        key: String,
        /// Identifier carried by the task record.
        record_id: String,
    },
    /// A watch upsert map key does not equal its embedded identifier.
    #[error("watch upsert key '{key}' does not match embedded watch identifier '{record_id}'")]
    WatchIdentifierMismatch {
        /// Map key supplied by the caller.
        key: String,
        /// Identifier carried by the watch record.
        record_id: String,
    },
    /// A delta contains an empty task identifier.
    #[error("registry delta contains an empty task identifier")]
    EmptyTaskIdentifier,
    /// A delta contains an empty watch identifier.
    #[error("registry delta contains an empty watch identifier")]
    EmptyWatchIdentifier,
    /// One task is both upserted and removed by the same atomic batch.
    #[error("registry delta both upserts and removes task '{task_id}'")]
    ConflictingTaskMutation {
        /// Ambiguous task identifier.
        task_id: String,
    },
    /// The explicit watch insertion order is incomplete or contains extras.
    #[error("watch upsert order does not name every upsert exactly once")]
    InvalidWatchUpsertOrder,
    /// The watch insertion order names an identifier with no upsert.
    #[error("watch upsert order names missing upsert '{watch_id}'")]
    UnknownOrderedWatchUpsert {
        /// Identifier named only by the order vector.
        watch_id: String,
    },
    /// The watch insertion order repeats an identifier.
    #[error("watch upsert order repeats identifier '{watch_id}'")]
    DuplicateOrderedWatchUpsert {
        /// Repeated identifier.
        watch_id: String,
    },
    /// The input watch registry already contains duplicate identifiers.
    #[error("watch registry contains duplicate identifier '{watch_id}'")]
    DuplicateWatchIdentifier {
        /// Duplicated watch identifier.
        watch_id: String,
    },
}

/// One atomic task/watch registry mutation.
///
/// The task and watch halves are encoded in one checksummed WAL frame. Public
/// fields keep daemon-side mutation assembly allocation-efficient while
/// [`Self::apply_to`] rejects ambiguous or identifier-inconsistent batches.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryDeltaBatch {
    /// Complete task records to insert or replace by task identifier.
    pub task_upserts: BTreeMap<String, TaskRecord>,
    /// Task identifiers to remove.
    pub task_removals: BTreeSet<String>,
    /// Complete watch records to insert or replace by watch identifier.
    pub watch_upserts: BTreeMap<String, WatchRegistration>,
    /// Observable insertion order for `watch_upserts`.
    ///
    /// This must contain every `watch_upserts` key exactly once. A watch also
    /// present in `watch_removals` is removed first and then appended at this
    /// position, explicitly representing remove-then-upsert reinsertion.
    pub watch_upsert_order: Vec<String>,
    /// Watch identifiers to remove.
    pub watch_removals: BTreeSet<String>,
}

impl RegistryDeltaBatch {
    /// Adds a later task upsert and clears an earlier removal of the same key.
    #[must_use]
    pub fn upsert_task(mut self, task: TaskRecord) -> Self {
        let task_id = task.task_id.clone();
        self.task_removals.remove(&task_id);
        self.task_upserts.insert(task_id, task);
        self
    }

    /// Adds a later task removal and clears an earlier upsert of the same key.
    #[must_use]
    pub fn remove_task(mut self, task_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        self.task_upserts.remove(&task_id);
        self.task_removals.insert(task_id);
        self
    }

    /// Adds a later watch upsert while preserving its sequential insertion
    /// position.
    ///
    /// If an earlier removal exists, the overlap is retained to represent
    /// remove-then-upsert reinsertion at the end of the observable watch list.
    #[must_use]
    pub fn upsert_watch(mut self, watch: WatchRegistration) -> Self {
        let watch_id = watch.watch_id.clone();
        if !self.watch_upserts.contains_key(&watch_id) {
            self.watch_upsert_order.push(watch_id.clone());
        }
        self.watch_upserts.insert(watch_id, watch);
        self
    }

    /// Adds a later watch removal and clears an earlier upsert/order entry.
    #[must_use]
    pub fn remove_watch(mut self, watch_id: impl Into<String>) -> Self {
        let watch_id = watch_id.into();
        self.watch_upserts.remove(&watch_id);
        self.watch_upsert_order
            .retain(|candidate| candidate != &watch_id);
        self.watch_removals.insert(watch_id);
        self
    }

    /// Returns whether the batch has no observable registry mutation.
    pub fn is_empty(&self) -> bool {
        self.task_upserts.is_empty()
            && self.task_removals.is_empty()
            && self.watch_upserts.is_empty()
            && self.watch_upsert_order.is_empty()
            && self.watch_removals.is_empty()
    }

    /// Coalesces a later batch into this batch using per-identifier
    /// last-mutation-wins semantics.
    ///
    /// # Errors
    ///
    /// Returns an identifier or overlap error if either input was assembled
    /// inconsistently before coalescing.
    pub fn merge_later_wins(
        &mut self,
        later: Self,
    ) -> std::result::Result<(), RegistryDeltaValidationError> {
        self.validate()?;
        later.validate()?;
        let mut merged = self.clone();

        for task_id in later.task_removals {
            merged.task_upserts.remove(&task_id);
            merged.task_removals.insert(task_id);
        }
        for (task_id, task) in later.task_upserts {
            merged.task_removals.remove(&task_id);
            merged.task_upserts.insert(task_id, task);
        }
        for watch_id in &later.watch_removals {
            merged.watch_upserts.remove(watch_id);
            merged
                .watch_upsert_order
                .retain(|candidate| candidate != watch_id);
            merged.watch_removals.insert(watch_id.clone());
        }
        for watch_id in &later.watch_upsert_order {
            let watch = later.watch_upserts.get(watch_id).cloned().ok_or_else(|| {
                RegistryDeltaValidationError::UnknownOrderedWatchUpsert {
                    watch_id: watch_id.clone(),
                }
            })?;
            let later_reinserts = later.watch_removals.contains(watch_id);
            let already_upserted = merged.watch_upserts.contains_key(watch_id);
            let already_removed = merged.watch_removals.contains(watch_id);
            if later_reinserts {
                merged.watch_removals.insert(watch_id.clone());
                merged
                    .watch_upsert_order
                    .retain(|candidate| candidate != watch_id);
                merged.watch_upsert_order.push(watch_id.clone());
            } else if already_removed && !already_upserted {
                // An earlier removal followed by this later upsert is a
                // reinsertion, so retain the removal marker and append last.
                merged.watch_upsert_order.push(watch_id.clone());
            } else if !already_upserted {
                merged.watch_upsert_order.push(watch_id.clone());
            }
            merged.watch_upserts.insert(watch_id.clone(), watch);
        }
        merged.validate()?;
        *self = merged;
        Ok(())
    }

    /// Atomically applies this batch to task and watch registries.
    ///
    /// Both inputs remain unchanged if validation fails. Existing watch order
    /// is retained for replacements; newly admitted and explicitly reinserted
    /// watches append in [`Self::watch_upsert_order`].
    ///
    /// # Errors
    ///
    /// Returns a typed error for empty or mismatched identifiers, ambiguous
    /// upsert/removal pairs, or duplicate identifiers in the input watch
    /// registry.
    pub fn apply_to(
        &self,
        tasks: &mut TaskRegistry,
        watches: &mut WatchRegistry,
    ) -> std::result::Result<(), RegistryDeltaValidationError> {
        self.validate()?;
        let mut admitted_watch_ids = BTreeSet::new();
        for watch in &watches.watches {
            record_apply_watch_scan();
            if !admitted_watch_ids.insert(watch.watch_id.as_str()) {
                return Err(RegistryDeltaValidationError::DuplicateWatchIdentifier {
                    watch_id: watch.watch_id.clone(),
                });
            }
        }

        // All fallible validation finishes above. Mutations below touch only
        // dirty task/watch records plus one watch-position index, so applying
        // a small delta never clones the O(total tasks) registry.
        for task_id in &self.task_removals {
            tasks.tasks.remove(task_id);
        }
        for (task_id, task) in &self.task_upserts {
            tasks.tasks.insert(task_id.clone(), task.clone());
        }
        watches
            .watches
            .retain(|watch| !self.watch_removals.contains(&watch.watch_id));
        let mut watch_positions = watches
            .watches
            .iter()
            .enumerate()
            .map(|(position, watch)| (watch.watch_id.clone(), position))
            .collect::<BTreeMap<_, _>>();
        for watch_id in &self.watch_upsert_order {
            let watch = &self.watch_upserts[watch_id];
            if let Some(position) = watch_positions.get(watch_id).copied() {
                watches.watches[position] = watch.clone();
            } else {
                let position = watches.watches.len();
                watches.watches.push(watch.clone());
                watch_positions.insert(watch_id.clone(), position);
            }
        }
        Ok(())
    }

    /// Applies this batch to a registry whose task/watch relationships have
    /// already been authenticated.
    ///
    /// Relationship-neutral task updates avoid scanning the watch registry.
    /// Relationship-changing batches retain the full failure-atomic validation
    /// performed by [`Self::apply_to`].
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::apply_to`].
    #[doc(hidden)]
    pub fn apply_to_authenticated(
        &self,
        tasks: &mut TaskRegistry,
        watches: &mut WatchRegistry,
    ) -> std::result::Result<bool, RegistryDeltaValidationError> {
        self.validate()?;
        if self.preserves_authenticated_relationships(tasks) {
            for (task_id, task) in &self.task_upserts {
                tasks.tasks.insert(task_id.clone(), task.clone());
            }
            return Ok(true);
        }
        self.apply_to(tasks, watches)?;
        Ok(false)
    }

    fn preserves_authenticated_relationships(&self, tasks: &TaskRegistry) -> bool {
        self.task_removals.is_empty()
            && self.watch_upserts.is_empty()
            && self.watch_upsert_order.is_empty()
            && self.watch_removals.is_empty()
            && self.task_upserts.iter().all(|(task_id, candidate)| {
                tasks
                    .tasks
                    .get(task_id)
                    .is_some_and(|current| current.watch_ids == candidate.watch_ids)
            })
    }

    fn validate(&self) -> std::result::Result<(), RegistryDeltaValidationError> {
        for (task_id, task) in &self.task_upserts {
            validate_task_identifier(task_id)?;
            if task.task_id != *task_id {
                return Err(RegistryDeltaValidationError::TaskIdentifierMismatch {
                    key: task_id.clone(),
                    record_id: task.task_id.clone(),
                });
            }
            if self.task_removals.contains(task_id) {
                return Err(RegistryDeltaValidationError::ConflictingTaskMutation {
                    task_id: task_id.clone(),
                });
            }
        }
        for task_id in &self.task_removals {
            validate_task_identifier(task_id)?;
        }
        for (watch_id, watch) in &self.watch_upserts {
            validate_watch_identifier(watch_id)?;
            if watch.watch_id != *watch_id {
                return Err(RegistryDeltaValidationError::WatchIdentifierMismatch {
                    key: watch_id.clone(),
                    record_id: watch.watch_id.clone(),
                });
            }
        }
        let mut ordered = BTreeSet::new();
        for watch_id in &self.watch_upsert_order {
            if !self.watch_upserts.contains_key(watch_id) {
                return Err(RegistryDeltaValidationError::UnknownOrderedWatchUpsert {
                    watch_id: watch_id.clone(),
                });
            }
            if !ordered.insert(watch_id.as_str()) {
                return Err(RegistryDeltaValidationError::DuplicateOrderedWatchUpsert {
                    watch_id: watch_id.clone(),
                });
            }
        }
        if ordered.len() != self.watch_upserts.len() {
            return Err(RegistryDeltaValidationError::InvalidWatchUpsertOrder);
        }
        for watch_id in &self.watch_removals {
            validate_watch_identifier(watch_id)?;
        }
        Ok(())
    }
}

fn validate_task_identifier(
    task_id: &str,
) -> std::result::Result<(), RegistryDeltaValidationError> {
    if task_id.trim().is_empty() {
        return Err(RegistryDeltaValidationError::EmptyTaskIdentifier);
    }
    Ok(())
}

fn validate_watch_identifier(
    watch_id: &str,
) -> std::result::Result<(), RegistryDeltaValidationError> {
    if watch_id.trim().is_empty() {
        return Err(RegistryDeltaValidationError::EmptyWatchIdentifier);
    }
    Ok(())
}

/// Authoritative task/watch state after checkpoint loading and WAL replay.
#[derive(Clone, Debug)]
pub struct LoadedTaskWatchRegistry {
    /// Materialized task registry.
    pub tasks: TaskRegistry,
    /// Materialized watch registry.
    pub watches: WatchRegistry,
    /// WAL revision included by the committed checkpoint manifest.
    pub checkpoint_revision: RegistryRevision,
    /// Latest revision after valid WAL frames were replayed.
    pub replayed_revision: RegistryRevision,
}

/// Authenticated task-admission state loaded from durable registry authority.
///
/// Fields are private so callers cannot manufacture task membership or a WAL
/// revision. The authority advances only after a checksummed WAL append
/// succeeds.
#[derive(Debug)]
pub struct RegistryAdmissionAuthority {
    root: PathBuf,
    tasks: TaskRegistry,
    watches: WatchRegistry,
    revision: RegistryRevision,
    lease: crate::task_store_lease::TaskStoreLease,
    _registry_authority_lease: crate::task_store_lease::TaskStoreLease,
}

impl RegistryAdmissionAuthority {
    /// Returns the durable WAL revision represented by this authority.
    #[must_use]
    pub const fn revision(&self) -> RegistryRevision {
        self.revision
    }

    /// Returns whether the authenticated durable registry contains `task_id`.
    #[must_use]
    pub fn contains_task(&self, task_id: &str) -> bool {
        self.tasks.tasks.contains_key(task_id)
    }

    /// Returns the authenticated task identifiers in stable order.
    pub fn task_ids(&self) -> impl Iterator<Item = &str> {
        self.tasks.tasks.keys().map(String::as_str)
    }

    /// Returns whether `tasks` and `watches` are the exact registry image
    /// authenticated when this authority was acquired.
    ///
    /// Watch ordering is part of the durable image and is compared exactly.
    ///
    /// # Errors
    ///
    /// Returns a JSON encoding error if either registry cannot be represented
    /// for an exact structural comparison.
    pub fn matches_registry(&self, tasks: &TaskRegistry, watches: &WatchRegistry) -> Result<bool> {
        let expected = serde_json::to_value((&self.tasks, &self.watches)).map_err(|source| {
            DaemonCoreError::json(
                "failed to compare authenticated task/watch registry for",
                task_registry_path(&self.root),
                source,
            )
        })?;
        let supplied = serde_json::to_value((tasks, watches)).map_err(|source| {
            DaemonCoreError::json(
                "failed to compare supplied task/watch registry for",
                task_registry_path(&self.root),
                source,
            )
        })?;
        Ok(expected == supplied)
    }

    fn matches_root(&self, root: &Path) -> bool {
        self.root == root
    }

    pub(super) fn require_task(&self, root: &Path, task_id: &TaskStorageId) -> Result<()> {
        if !self.matches_root(root) {
            return Err(invalid_registry_authority_root(root));
        }
        if self.tasks.tasks.contains_key(task_id.as_str()) {
            return Ok(());
        }
        Err(DaemonCoreError::InvalidTaskRegistry {
            path: task_registry_path(root),
            message: format!(
                "task identifier {:?} is not present in authenticated registry authority",
                task_id.as_str()
            ),
        })
    }

    pub(super) const fn lease(&self) -> &crate::task_store_lease::TaskStoreLease {
        &self.lease
    }

    fn apply_committed_batch(
        &mut self,
        revisions: RegistryRevisionRange,
        batch: &RegistryDeltaBatch,
        candidate: Option<(TaskRegistry, WatchRegistry)>,
    ) {
        if let Some((tasks, watches)) = candidate {
            self.tasks = tasks;
            self.watches = watches;
        } else {
            debug_assert!(batch.preserves_authenticated_relationships(&self.tasks));
            for (task_id, task) in &batch.task_upserts {
                self.tasks.tasks.insert(task_id.clone(), task.clone());
            }
        }
        self.revision = revisions.last;
    }
}

/// Returns the diagnostic path of the task/watch registry delta WAL.
pub fn registry_delta_wal_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(REGISTRY_DELTA_WAL_FILE_NAME)
}

/// Loads compact task-admission authority from checkpoint-plus-WAL state.
///
/// The returned value has private fields and therefore cannot be forged from a
/// caller-provided registry snapshot. It consumes and retains `lease`, keeping
/// lifecycle ownership continuous from the authenticated load through every
/// WAL and event append that uses the authority.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if `lease` is not the daemon lifecycle lease
/// for `root`. Other errors match [`load_task_watch_registry_with_deltas`].
pub fn load_registry_admission_authority(
    root: &Path,
    lease: crate::task_store_lease::TaskStoreLease,
) -> Result<RegistryAdmissionAuthority> {
    require_daemon_lifecycle_lease(root, &lease)?;
    #[cfg(unix)]
    let daemon = lease.daemon_capability()?;
    let registry_authority_lease =
        crate::task_store_lease::acquire_daemon_registry_authority(&lease)?;
    #[cfg(unix)]
    let loaded = load_task_watch_registry_with_deltas_under_admission(root, &daemon)?;
    #[cfg(not(unix))]
    let loaded = load_task_watch_registry_with_deltas_under_admission(root)?;
    Ok(RegistryAdmissionAuthority {
        root: root.to_path_buf(),
        tasks: loaded.tasks,
        watches: loaded.watches,
        revision: loaded.replayed_revision,
        lease,
        _registry_authority_lease: registry_authority_lease,
    })
}

/// Appends and synchronizes one atomic task/watch registry delta.
///
/// The revision range must begin immediately after the durable WAL tail.
/// Multiple logical revisions may be coalesced into one frame, but replay
/// treats the resulting task/watch mutation as one indivisible transaction.
///
/// # Errors
///
/// Returns typed batch, revision, size, corruption, path-safety, locking, and
/// synchronization errors without appending a partial in-process frame.
pub fn append_task_watch_registry_delta(
    root: &Path,
    revisions: RegistryRevisionRange,
    batch: &RegistryDeltaBatch,
) -> Result<()> {
    let prepared = prepare_registry_delta(root, revisions, batch)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;

    #[cfg(unix)]
    {
        let retained_daemon = writer_lease.daemon_capability()?;
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |locked_daemon| {
                validate_retained_registry_daemon(root, locked_daemon, &retained_daemon)?;
                let current = load_under_task_lock_anchored(root, &retained_daemon)?;
                validate_registry_delta_namespace_admission(root, &current.tasks, batch)?;
                prepare_registry_delta_candidate(root, &current.tasks, &current.watches, batch)?;
                append_under_task_lock(
                    root,
                    &retained_daemon,
                    revisions,
                    &prepared.header,
                    &prepared.payload,
                    &prepared.footer,
                    || Ok(current.checkpoint_revision),
                )
            },
        )
    }
    #[cfg(not(unix))]
    {
        let task_path = task_registry_path(root);
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            let current = load_under_task_lock_portable(root)?;
            validate_registry_delta_namespace_admission(root, &current.tasks, batch)?;
            prepare_registry_delta_candidate(root, &current.tasks, &current.watches, batch)?;
            append_under_task_lock(
                root,
                revisions,
                &prepared.header,
                &prepared.payload,
                &prepared.footer,
                || Ok(current.checkpoint_revision),
            )
        })
    }
}

/// Appends a registry delta using the daemon's compact in-memory admission.
///
/// This preserves the same locking, WAL continuity, and namespace checks as
/// [`append_task_watch_registry_delta`] while avoiding a full checkpoint and
/// WAL replay on the daemon's hot path.
///
/// # Errors
///
/// Returns lease, revision, namespace-admission, batch, size, corruption,
/// locking, and synchronization errors without appending an unauthorized
/// frame.
pub fn append_task_watch_registry_delta_with_authority(
    root: &Path,
    authority: &mut RegistryAdmissionAuthority,
    revisions: RegistryRevisionRange,
    batch: &RegistryDeltaBatch,
) -> Result<()> {
    require_daemon_lifecycle_lease(root, &authority.lease)?;
    if !authority.matches_root(root) {
        return Err(invalid_registry_authority_root(root));
    }
    let prepared = prepare_registry_delta(root, revisions, batch)?;
    let expected_first = authority.revision.checked_next().ok_or_else(|| {
        invalid_wal(
            &registry_delta_wal_path(root),
            "registry revision is exhausted",
        )
    })?;
    if revisions.first != expected_first {
        return Err(DaemonCoreError::RegistryDeltaRevisionMismatch {
            path: registry_delta_wal_path(root),
            expected_first: expected_first.get(),
            actual_first: revisions.first.get(),
            actual_last: revisions.last.get(),
        });
    }

    #[cfg(unix)]
    let candidate = {
        let retained_daemon = authority.lease.daemon_capability()?;
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |locked_daemon| {
                validate_retained_registry_daemon(root, locked_daemon, &retained_daemon)?;
                validate_registry_delta_namespace_admission(root, &authority.tasks, batch)?;
                let candidate = prepare_registry_delta_candidate(
                    root,
                    &authority.tasks,
                    &authority.watches,
                    batch,
                )?;
                append_under_task_lock(
                    root,
                    &retained_daemon,
                    revisions,
                    &prepared.header,
                    &prepared.payload,
                    &prepared.footer,
                    || Ok(authority.revision),
                )?;
                Ok(candidate)
            },
        )
    };
    #[cfg(not(unix))]
    let candidate = {
        let task_path = task_registry_path(root);
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            validate_registry_delta_namespace_admission(root, &authority.tasks, batch)?;
            let candidate = prepare_registry_delta_candidate(
                root,
                &authority.tasks,
                &authority.watches,
                batch,
            )?;
            append_under_task_lock(
                root,
                revisions,
                &prepared.header,
                &prepared.payload,
                &prepared.footer,
                || Ok(authority.revision),
            )?;
            Ok(candidate)
        })
    };
    let candidate = candidate?;
    authority.apply_committed_batch(revisions, batch, candidate);
    Ok(())
}

struct PreparedRegistryDelta {
    payload: Vec<u8>,
    header: [u8; FRAME_HEADER_BYTES],
    footer: [u8; FRAME_FOOTER_BYTES],
}

fn prepare_registry_delta(
    root: &Path,
    revisions: RegistryRevisionRange,
    batch: &RegistryDeltaBatch,
) -> Result<PreparedRegistryDelta> {
    revisions
        .validate()
        .map_err(|error| invalid_batch(root, error))?;
    batch
        .validate()
        .map_err(|error| invalid_batch(root, error))?;
    let path = registry_delta_wal_path(root);
    let payload = serde_json::to_vec(batch).map_err(|source| {
        DaemonCoreError::json("failed to encode registry delta for", &path, source)
    })?;
    if payload.len() > MAX_REGISTRY_DELTA_FRAME_BYTES {
        return Err(DaemonCoreError::RegistryDeltaFrameTooLarge {
            path,
            encoded_bytes: payload.len() as u64,
            max_bytes: MAX_REGISTRY_DELTA_FRAME_BYTES as u64,
        });
    }
    let header = encode_frame_header(revisions, &payload);
    let footer = encode_frame_footer(&header, payload.len());
    Ok(PreparedRegistryDelta {
        payload,
        header,
        footer,
    })
}

fn validate_registry_delta_namespace_admission(
    root: &Path,
    current: &TaskRegistry,
    batch: &RegistryDeltaBatch,
) -> Result<()> {
    for task_id in batch.task_upserts.keys().chain(batch.task_removals.iter()) {
        if let Some(message) = task_identifier_shape_error(task_id) {
            return Err(DaemonCoreError::InvalidTaskRegistry {
                path: task_registry_path(root),
                message,
            });
        }
    }
    let new_tasks = batch
        .task_upserts
        .iter()
        .filter(|(task_id, _)| !current.tasks.contains_key(*task_id))
        .map(|(task_id, task)| (task_id.clone(), task.clone()))
        .collect();
    let new_registry = TaskRegistry { tasks: new_tasks };
    if new_registry.tasks.is_empty() {
        return Ok(());
    }
    let mut alias_owners = current
        .tasks
        .keys()
        .map(|task_id| (task_storage_key_alias_class(task_id), task_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    for task_id in new_registry.tasks.keys() {
        let alias = task_storage_key_alias_class(task_id);
        if let Some(existing) = alias_owners.insert(alias, task_id) {
            return Err(DaemonCoreError::InvalidTaskRegistry {
                path: task_registry_path(root),
                message: format!(
                    "task identifiers {existing:?} and {task_id:?} derive filesystem-aliasing \
                     storage keys"
                ),
            });
        }
    }
    validate_task_registry_namespace_bindings(
        root,
        &new_registry,
        Some(current),
        None,
        &task_registry_path(root),
    )
}

fn prepare_registry_delta_candidate(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    batch: &RegistryDeltaBatch,
) -> Result<Option<(TaskRegistry, WatchRegistry)>> {
    if batch.preserves_authenticated_relationships(tasks) {
        return Ok(None);
    }
    let mut candidate_tasks = tasks.clone();
    let mut candidate_watches = watches.clone();
    batch
        .apply_to(&mut candidate_tasks, &mut candidate_watches)
        .map_err(|error| invalid_batch(root, error))?;
    validate_task_watch_registry_relationships(root, &candidate_tasks, &candidate_watches)?;
    Ok(Some((candidate_tasks, candidate_watches)))
}

fn invalid_registry_authority_root(root: &Path) -> DaemonCoreError {
    DaemonCoreError::io(
        "registry admission authority does not own the requested root",
        root,
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "registry authority belongs to another workspace",
        ),
    )
}

#[cfg(unix)]
fn validate_retained_registry_daemon(
    root: &Path,
    locked: &CapabilityDir,
    retained: &CapabilityDir,
) -> Result<()> {
    retained
        .validate_display_path_attachment()
        .map_err(|source| DaemonCoreError::StorageMutationAuthorityLost {
            operation: "registry-delta retained daemon validation",
            path: daemon_dir(root),
            source,
        })?;
    if locked.identity() != retained.identity() {
        return Err(DaemonCoreError::StorageMutationAuthorityLost {
            operation: "registry-delta retained daemon validation",
            path: daemon_dir(root),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task-registry lock and retained daemon authority name different directories",
            ),
        });
    }
    Ok(())
}

/// Loads one committed checkpoint and replays every newer valid WAL frame.
///
/// A crash-torn final frame is truncated and synchronized under the exclusive
/// registry lock. Complete checksum, schema, revision, or relationship
/// corruption fails closed.
///
/// # Errors
///
/// Returns the same checkpoint errors as
/// [`load_task_watch_registry_checkpoint_with_event_tails`], plus typed WAL
/// corruption, size, path-safety, and repair errors.
pub fn load_task_watch_registry_with_deltas(root: &Path) -> Result<LoadedTaskWatchRegistry> {
    let writer_lease = acquire_task_store_writer_lease(root)?;
    #[cfg(unix)]
    {
        let daemon = writer_lease.daemon_capability()?;
        load_task_watch_registry_with_deltas_under_admission(root, &daemon)
    }
    #[cfg(not(unix))]
    load_task_watch_registry_with_deltas_under_admission(root)
}

#[cfg(unix)]
fn load_task_watch_registry_with_deltas_under_admission(
    root: &Path,
    retained_daemon: &CapabilityDir,
) -> Result<LoadedTaskWatchRegistry> {
    with_anchored_task_registry_lock(
        root,
        RegistryLockMode::Exclusive,
        || Ok(()),
        |locked_daemon| {
            validate_retained_registry_daemon(root, locked_daemon, retained_daemon)?;
            load_under_task_lock_anchored(root, retained_daemon)
        },
    )
}

#[cfg(not(unix))]
fn load_task_watch_registry_with_deltas_under_admission(
    root: &Path,
) -> Result<LoadedTaskWatchRegistry> {
    let task_path = task_registry_path(root);
    with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
        load_under_task_lock_portable(root)
    })
}

/// Loads checkpoint-plus-WAL registry authority and authenticated event tails
/// beneath the same task-registry lock.
///
/// # Errors
///
/// Returns the same errors as [`load_task_watch_registry_with_deltas`] and the
/// strict task-event tail reader.
pub fn load_task_watch_registry_with_deltas_and_event_tails(
    root: &Path,
) -> Result<(LoadedTaskWatchRegistry, BTreeMap<String, Option<u64>>)> {
    let writer_lease = acquire_task_store_writer_lease(root)?;
    #[cfg(unix)]
    {
        let retained_daemon = writer_lease.daemon_capability()?;
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |locked_daemon| {
                validate_retained_registry_daemon(root, locked_daemon, &retained_daemon)?;
                let loaded = load_under_task_lock_anchored(root, &retained_daemon)?;
                let mut tails = BTreeMap::new();
                for task_id in loaded.tasks.tasks.keys() {
                    let storage_id = checked_task_storage_id(root, task_id)?;
                    let tail = event_tail::task_event_log_tail_sequence_admitted(
                        root,
                        &storage_id,
                        &writer_lease,
                    )?;
                    tails.insert(task_id.clone(), tail);
                }
                Ok((loaded, tails))
            },
        )
    }
    #[cfg(not(unix))]
    {
        let task_path = task_registry_path(root);
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            let loaded = load_under_task_lock_portable(root)?;
            let mut tails = BTreeMap::new();
            for task_id in loaded.tasks.tasks.keys() {
                let storage_id = checked_task_storage_id(root, task_id)?;
                let tail = event_tail::task_event_log_tail_sequence_portable(root, &storage_id)?;
                tails.insert(task_id.clone(), tail);
            }
            let _ = &writer_lease;
            Ok((loaded, tails))
        })
    }
}

/// Upper bound on tasks auto-quarantined in a single recovering load. A store
/// with more corrupt event logs than this fails closed so a human can inspect
/// it rather than silently discarding a large amount of task state.
pub const MAX_CORRUPT_EVENT_LOG_QUARANTINE_TASKS: usize = 64;

/// A task whose event log failed integrity validation during a recovering load
/// and was moved aside so the daemon could start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedCorruptTaskEventLog {
    /// Identifier of the task whose event log was quarantined.
    pub task_id: String,
    /// Canonical event-log path that was found corrupt.
    pub event_log_path: PathBuf,
    /// Where the corrupt log was moved for inspection, when a move occurred.
    pub quarantined_path: Option<PathBuf>,
    /// Human-readable integrity failure that triggered the quarantine.
    pub reason: String,
}

/// Returns true for durable event-log integrity failures that recovery can
/// safely quarantine, as opposed to environmental IO, lock, or lease failures
/// that must still fail closed.
fn is_recoverable_event_log_corruption(error: &DaemonCoreError) -> bool {
    matches!(
        error,
        DaemonCoreError::InvalidTaskEventFrame { .. }
            | DaemonCoreError::AuthorityJsonLimitExceeded { .. }
    )
}

/// Identifies every admitted task whose durable event log fails integrity
/// validation. Non-corruption errors (IO, lease, lock) propagate unchanged so
/// only genuine event-log corruption is treated as recoverable.
fn scan_corrupt_task_event_logs(root: &Path) -> Result<Vec<QuarantinedCorruptTaskEventLog>> {
    let loaded = load_task_watch_registry_with_deltas(root)?;
    let mut corrupt = Vec::new();
    for task_id in loaded.tasks.tasks.keys() {
        match event_tail::task_event_log_tail_sequence(root, task_id) {
            Ok(_) => {}
            Err(error) if is_recoverable_event_log_corruption(&error) => {
                let storage_id = checked_task_storage_id(root, task_id)?;
                corrupt.push(QuarantinedCorruptTaskEventLog {
                    task_id: task_id.clone(),
                    event_log_path: task_event_log_path(root, &storage_id),
                    quarantined_path: None,
                    reason: error.to_string(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(corrupt)
}

/// Moves each corrupt task event log aside (renaming it to a sibling
/// `*.corrupt-<unix>` file) so the task's canonical event-log path becomes
/// absent and its durable tail reads as empty.
///
/// This deliberately does not touch the task registry: mutating the durable
/// checkpoint here would race the persistence owner that assumes ownership at
/// startup. The caller resets the quarantined task's event high-water through
/// the normal persistence path instead. The renamed file is preserved for
/// inspection. A log that is already absent is ignored.
fn move_corrupt_event_logs_aside(corrupt: &mut [QuarantinedCorruptTaskEventLog]) -> Result<()> {
    let stamp = now_unix();
    for record in corrupt.iter_mut() {
        let mut dest = record.event_log_path.clone().into_os_string();
        dest.push(format!(".corrupt-{stamp}"));
        let dest = PathBuf::from(dest);
        match fs::rename(&record.event_log_path, &dest) {
            Ok(()) => record.quarantined_path = Some(dest),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DaemonCoreError::io(
                    "failed to move a corrupt task event log aside",
                    &record.event_log_path,
                    error,
                ));
            }
        }
    }
    Ok(())
}

/// Loads the durable task/watch registry and event tails, quarantining any task
/// whose durable event log fails integrity validation instead of failing the
/// entire load.
///
/// A single corrupted task event log otherwise takes the whole daemon down at
/// startup (`packet28d did not become ready`). This wrapper isolates the bad
/// task: it moves the corrupt event log aside (to a sibling `*.corrupt-<unix>`
/// file, preserved for inspection) so the task's tail reads as empty, then
/// returns the registry — the affected task is still present but its returned
/// tail is `None`.
///
/// The registry is intentionally left unmutated so it does not race the
/// persistence owner that assumes checkpoint ownership at startup. The caller
/// must reset each quarantined task's event high-water (`last_event_seq` to 0)
/// through the normal persistence path so reconciliation stays consistent; the
/// returned vector lists every quarantined task for that purpose and for
/// reporting.
///
/// Only genuine event-log integrity failures are recovered, and at most
/// [`MAX_CORRUPT_EVENT_LOG_QUARANTINE_TASKS`] tasks are quarantined before the
/// load fails closed. All other errors propagate unchanged.
///
/// # Errors
///
/// Returns the same errors as
/// [`load_task_watch_registry_with_deltas_and_event_tails`] for non-recoverable
/// failures, plus filesystem errors from moving a corrupt log aside.
pub fn load_task_watch_registry_recovering_corrupt_event_logs(
    root: &Path,
) -> Result<(
    LoadedTaskWatchRegistry,
    BTreeMap<String, Option<u64>>,
    Vec<QuarantinedCorruptTaskEventLog>,
)> {
    let mut quarantined: Vec<QuarantinedCorruptTaskEventLog> = Vec::new();
    loop {
        match load_task_watch_registry_with_deltas_and_event_tails(root) {
            Ok((loaded, tails)) => return Ok((loaded, tails, quarantined)),
            Err(error) => {
                if !is_recoverable_event_log_corruption(&error) {
                    return Err(error);
                }
                let mut corrupt = scan_corrupt_task_event_logs(root)?;
                if corrupt.is_empty()
                    || quarantined.len().saturating_add(corrupt.len())
                        > MAX_CORRUPT_EVENT_LOG_QUARANTINE_TASKS
                {
                    return Err(error);
                }
                move_corrupt_event_logs_aside(&mut corrupt)?;
                quarantined.extend(corrupt);
            }
        }
    }
}

/// Commits a full task/watch checkpoint at one replayed WAL revision.
///
/// The supplied registries must equal checkpoint-plus-WAL replay at
/// `revision`. The new manifest is committed before the WAL is atomically
/// reset. If reset fails, the committed watermark remains authoritative and a
/// later load safely skips retained frames at or below it.
///
/// # Errors
///
/// Returns validation, replay, revision mismatch, checkpoint publication,
/// path-safety, and WAL-reset errors.
pub fn save_task_watch_registry_checkpoint_at_revision(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    revision: RegistryRevision,
) -> Result<()> {
    validate_task_watch_registry_relationships(root, tasks, watches)?;
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    let task_bytes = encode_task_registry(&task_path, tasks)?;
    let _ = encode_watch_registry(&watch_path, watches, None)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    #[cfg(unix)]
    let daemon = writer_lease.daemon_capability()?;
    save_task_watch_registry_checkpoint_at_revision_under_admission(
        root,
        tasks,
        watches,
        revision,
        task_bytes,
        #[cfg(unix)]
        &daemon,
    )
}

/// Commits a full checkpoint while retaining daemon registry authority.
///
/// This is the daemon-owner counterpart to
/// [`save_task_watch_registry_checkpoint_at_revision`]. The authority's
/// exclusive admission lease prevents supported external writers from
/// replacing checkpoint or WAL membership between its authenticated load and
/// this publication.
///
/// # Errors
///
/// Returns an I/O permission error when `authority` belongs to another root.
/// Other errors match [`save_task_watch_registry_checkpoint_at_revision`].
pub fn save_task_watch_registry_checkpoint_at_revision_with_authority(
    root: &Path,
    authority: &RegistryAdmissionAuthority,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    revision: RegistryRevision,
) -> Result<()> {
    if !authority.matches_root(root) {
        return Err(invalid_registry_authority_root(root));
    }
    require_daemon_lifecycle_lease(root, authority.lease())?;
    validate_task_watch_registry_relationships(root, tasks, watches)?;
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    let task_bytes = encode_task_registry(&task_path, tasks)?;
    let _ = encode_watch_registry(&watch_path, watches, None)?;
    #[cfg(unix)]
    let daemon = authority.lease().daemon_capability()?;
    save_task_watch_registry_checkpoint_at_revision_under_admission(
        root,
        tasks,
        watches,
        revision,
        task_bytes,
        #[cfg(unix)]
        &daemon,
    )
}

fn save_task_watch_registry_checkpoint_at_revision_under_admission(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    revision: RegistryRevision,
    task_bytes: Vec<u8>,
    #[cfg(unix)] retained_daemon: &CapabilityDir,
) -> Result<()> {
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |locked_daemon| {
                validate_retained_registry_daemon(root, locked_daemon, retained_daemon)?;
                let authority =
                    load_under_task_lock_anchored_with_admissions(root, retained_daemon)?;
                validate_checkpoint_candidate(root, &authority.loaded, tasks, watches, revision)?;
                save_task_watch_registry_checkpoint_anchored(
                    root,
                    retained_daemon,
                    tasks,
                    watches,
                    task_bytes,
                    Some(revision.get()),
                    Some(&authority.wal_admitted_task_ids),
                )?;
                reset_wal(root, retained_daemon, revision)
            },
        )
    }
    #[cfg(not(unix))]
    {
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            let authority = load_under_task_lock_portable_with_admissions(root)?;
            validate_checkpoint_candidate(root, &authority.loaded, tasks, watches, revision)?;
            save_task_watch_registry_checkpoint_portable(
                root,
                tasks,
                watches,
                task_bytes,
                Some(revision.get()),
                Some(&authority.wal_admitted_task_ids),
            )?;
            reset_wal(root, revision)
        })
    }
}

#[cfg(unix)]
fn load_under_task_lock_anchored(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<LoadedTaskWatchRegistry> {
    Ok(load_under_task_lock_anchored_with_admissions(root, daemon)?.loaded)
}

#[cfg(unix)]
fn load_under_task_lock_anchored_with_admissions(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<LoadedRegistryAuthority> {
    let loaded =
        load_task_watch_registry_checkpoint_with_delta_revision_under_task_lock(root, daemon)?;
    let wal = open_anchored_registry_wal(daemon, root)?;
    replay_wal_with_admissions(
        root,
        wal,
        loaded.tasks,
        loaded.watches,
        RegistryRevision::new(loaded.applied_delta_revision),
        None,
        true,
    )
}

#[cfg(not(unix))]
fn load_under_task_lock_portable(root: &Path) -> Result<LoadedTaskWatchRegistry> {
    Ok(load_under_task_lock_portable_with_admissions(root)?.loaded)
}

#[cfg(not(unix))]
fn load_under_task_lock_portable_with_admissions(root: &Path) -> Result<LoadedRegistryAuthority> {
    let loaded =
        load_task_watch_registry_checkpoint_with_delta_revision_portable_under_task_lock(root)?;
    let path = registry_delta_wal_path(root);
    let state = open_daemon_state(root)?;
    let wal = state
        .open_existing(REGISTRY_DELTA_WAL_FILE_NAME, FileAccess::ReadWrite)
        .map_err(|source| wal_io("failed to open registry delta WAL", &path, source))?;
    replay_wal_with_admissions(
        root,
        wal,
        loaded.tasks,
        loaded.watches,
        RegistryRevision::new(loaded.applied_delta_revision),
        None,
        true,
    )
}

struct LoadedRegistryAuthority {
    loaded: LoadedTaskWatchRegistry,
    wal_admitted_task_ids: BTreeSet<String>,
}

#[cfg(unix)]
pub(crate) struct RetainedRegistrySnapshot {
    pub(crate) registry: TaskRegistry,
    pub(crate) record_values: BTreeMap<String, serde_json::Value>,
    pub(crate) revision: RegistryRevision,
    pub(crate) checkpoint_generation: Option<u64>,
    pub(crate) wal_file_bytes: u64,
    pub(crate) wal_allocated_bytes: u64,
}

#[cfg(unix)]
struct RetainedRegistryImage {
    authority: LoadedRegistryAuthority,
    record_values: BTreeMap<String, serde_json::Value>,
    checkpoint_generation: Option<u64>,
}

#[cfg(unix)]
pub(crate) fn load_retained_registry_snapshot_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<RetainedRegistrySnapshot> {
    let image = load_retained_registry_image_under_task_lock(root, daemon, false)?;
    let (wal_file_bytes, wal_allocated_bytes) = daemon
        .entry_storage_bytes(OsStr::new(REGISTRY_DELTA_WAL_FILE_NAME))
        .map_err(|source| {
            wal_io(
                "failed to measure retained registry delta WAL",
                registry_delta_wal_path(root),
                source,
            )
        })?
        .unwrap_or_default();
    Ok(RetainedRegistrySnapshot {
        registry: image.authority.loaded.tasks,
        record_values: image.record_values,
        revision: image.authority.loaded.replayed_revision,
        checkpoint_generation: image.checkpoint_generation,
        wal_file_bytes,
        wal_allocated_bytes,
    })
}

#[cfg(unix)]
fn load_retained_registry_image_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
    repair_torn_suffix: bool,
) -> Result<RetainedRegistryImage> {
    let loaded =
        load_task_watch_registry_checkpoint_with_delta_revision_under_task_lock(root, daemon)?;
    debug_assert_eq!(loaded.task_generation, loaded.watch_generation);
    let checkpoint_generation = loaded.task_generation;
    let path = task_registry_path(root);
    let mut value = crate::storage::decode_json_value_without_duplicate_keys(
        &loaded.task_bytes,
        crate::storage::AuthorityJsonProfile::TaskRegistry,
    )
    .map_err(|error| {
        crate::storage::map_authority_json_error(
            &path,
            crate::storage::AuthorityJsonProfile::TaskRegistry,
            "failed to decode retained task registry from",
            error,
        )
    })?;
    if let Some(message) = crate::storage::task_registry_value_shape_error(&value) {
        return Err(DaemonCoreError::InvalidTaskRegistry {
            path,
            message: message.to_string(),
        });
    }
    let records = value
        .get_mut("tasks")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| DaemonCoreError::InvalidTaskRegistry {
            path: task_registry_path(root),
            message: "retained task registry has no object-valued tasks field".to_string(),
        })?;
    let wal = open_anchored_registry_wal(daemon, root)?;
    let authority = replay_wal_with_admissions(
        root,
        wal,
        loaded.tasks,
        loaded.watches,
        RegistryRevision::new(loaded.applied_delta_revision),
        Some(records),
        repair_torn_suffix,
    )?;
    Ok(RetainedRegistryImage {
        authority,
        record_values: records.clone().into_iter().collect(),
        checkpoint_generation,
    })
}

#[cfg(unix)]
pub(crate) fn remove_retained_registry_records_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
    expected_records: &BTreeMap<String, serde_json::Value>,
    expected_revision: Option<RegistryRevision>,
    expected_checkpoint_generation: Option<u64>,
    allow_already_removed: bool,
    before_remove: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    if expected_records.is_empty() {
        before_remove()?;
        return Ok(true);
    }
    let mut image = load_retained_registry_image_under_task_lock(root, daemon, true)?;
    let current_revision = image.authority.loaded.replayed_revision;
    for (task_id, expected) in expected_records {
        match image.record_values.get(task_id) {
            Some(current)
                if current == expected
                    && expected_revision.is_some_and(|revision| revision == current_revision)
                    && expected_checkpoint_generation == image.checkpoint_generation
                    && (!allow_already_removed || expected_checkpoint_generation.is_some()) => {}
            None if allow_already_removed => {}
            _ => return Ok(false),
        }
    }
    if allow_already_removed
        && expected_revision.is_some_and(|revision| current_revision < revision)
    {
        return Ok(false);
    }
    if allow_already_removed
        && expected_checkpoint_generation.is_some_and(|expected| {
            image
                .checkpoint_generation
                .is_none_or(|current| current < expected)
        })
    {
        return Ok(false);
    }
    for task_id in expected_records.keys() {
        image.authority.loaded.tasks.tasks.remove(task_id);
    }
    validate_task_watch_registry_relationships(
        root,
        &image.authority.loaded.tasks,
        &image.authority.loaded.watches,
    )?;
    let task_bytes =
        encode_task_registry(&task_registry_path(root), &image.authority.loaded.tasks)?;
    before_remove()?;
    save_task_watch_registry_checkpoint_anchored(
        root,
        daemon,
        &image.authority.loaded.tasks,
        &image.authority.loaded.watches,
        task_bytes,
        Some(image.authority.loaded.replayed_revision.get()),
        Some(&image.authority.wal_admitted_task_ids),
    )?;
    reset_wal(root, daemon, image.authority.loaded.replayed_revision)?;
    Ok(true)
}

fn replay_wal_with_admissions<W: RegistryWalFile>(
    root: &Path,
    wal: Option<W>,
    mut tasks: TaskRegistry,
    mut watches: WatchRegistry,
    checkpoint_revision: RegistryRevision,
    mut raw_task_records: Option<&mut serde_json::Map<String, serde_json::Value>>,
    repair_torn_suffix: bool,
) -> Result<LoadedRegistryAuthority> {
    let checkpoint_task_ids = tasks.tasks.keys().cloned().collect::<BTreeSet<_>>();
    let path = registry_delta_wal_path(root);
    let Some(mut wal) = wal else {
        return Ok(LoadedRegistryAuthority {
            loaded: LoadedTaskWatchRegistry {
                tasks,
                watches,
                checkpoint_revision,
                replayed_revision: checkpoint_revision,
            },
            wal_admitted_task_ids: BTreeSet::new(),
        });
    };

    let mut replayed_revision = checkpoint_revision;
    let inspection = scan_wal(&mut wal, &path, repair_torn_suffix, |revisions, batch| {
        if revisions.last <= checkpoint_revision {
            return Ok(());
        }
        let expected = replayed_revision.checked_next().ok_or_else(|| {
            invalid_wal(
                &path,
                "registry revision is exhausted before a replayed frame",
            )
        })?;
        if revisions.first != expected {
            return Err(invalid_wal(
                &path,
                format!(
                    "frame {}..={} does not continue checkpoint/replay revision {}",
                    revisions.first.get(),
                    revisions.last.get(),
                    replayed_revision.get()
                ),
            ));
        }
        let relationships_unchanged = batch
            .apply_to_authenticated(&mut tasks, &mut watches)
            .map_err(|error| invalid_wal(&path, error.to_string()))?;
        if !relationships_unchanged {
            validate_task_watch_registry_relationships(root, &tasks, &watches)
                .map_err(|error| invalid_wal(&path, error.to_string()))?;
        }
        if let Some(records) = raw_task_records.as_deref_mut() {
            for task_id in &batch.task_removals {
                records.remove(task_id);
            }
            for (task_id, task) in &batch.task_upserts {
                records.insert(
                    task_id.clone(),
                    serde_json::to_value(task).map_err(|source| {
                        DaemonCoreError::json(
                            "failed to materialize registry delta task for",
                            &path,
                            source,
                        )
                    })?,
                );
            }
        }
        replayed_revision = revisions.last;
        Ok(())
    })?;
    if inspection.base_revision > checkpoint_revision {
        return Err(invalid_wal(
            &path,
            format!(
                "WAL base revision {} is newer than committed checkpoint revision {}",
                inspection.base_revision.get(),
                checkpoint_revision.get()
            ),
        ));
    }
    if inspection.last_revision < checkpoint_revision {
        // This is valid after a committed checkpoint whose obsolete WAL was
        // retained or externally archived before the reset header appeared.
        replayed_revision = checkpoint_revision;
    }
    let wal_admitted_task_ids = tasks
        .tasks
        .keys()
        .filter(|task_id| !checkpoint_task_ids.contains(*task_id))
        .cloned()
        .collect();
    Ok(LoadedRegistryAuthority {
        loaded: LoadedTaskWatchRegistry {
            tasks,
            watches,
            checkpoint_revision,
            replayed_revision,
        },
        wal_admitted_task_ids,
    })
}

fn append_to_wal(
    root: &Path,
    mut wal: impl RegistryWalFile,
    revisions: RegistryRevisionRange,
    frame_header: &[u8; FRAME_HEADER_BYTES],
    payload: &[u8],
    frame_footer: &[u8; FRAME_FOOTER_BYTES],
) -> Result<()> {
    let path = registry_delta_wal_path(root);
    let inspection = inspect_wal_tail(&mut wal, &path)?;
    if let Some(last_frame) = inspection.last_frame {
        verify_last_frame_payload(&mut wal, &path, last_frame)?;
    }
    if let Some(last_frame) = inspection.last_frame {
        if revisions == last_frame.revisions {
            let payload_checksum = blake3::hash(payload);
            if payload_checksum.as_bytes() != &last_frame.payload_checksum {
                return Err(invalid_wal(
                    &path,
                    format!(
                        "retry for durable revision range {}..={} carries different payload bytes",
                        revisions.first.get(),
                        revisions.last.get()
                    ),
                ));
            }
            wal.file_mut().sync_all().map_err(|source| {
                wal_io(
                    "failed to synchronize idempotent registry delta WAL retry",
                    &path,
                    source,
                )
            })?;
            return wal.validate_attachment().map_err(|source| {
                DaemonCoreError::StorageMutationAuthorityLost {
                    operation: "registry-delta WAL idempotent retry",
                    path,
                    source,
                }
            });
        }
    }
    let expected_first = inspection
        .last_revision
        .checked_next()
        .ok_or_else(|| invalid_wal(&path, "registry revision is exhausted"))?;
    if revisions.first != expected_first {
        return Err(DaemonCoreError::RegistryDeltaRevisionMismatch {
            path,
            expected_first: expected_first.get(),
            actual_first: revisions.first.get(),
            actual_last: revisions.last.get(),
        });
    }

    let frame_bytes = FRAME_HEADER_BYTES
        .checked_add(payload.len())
        .and_then(|bytes| bytes.checked_add(FRAME_FOOTER_BYTES))
        .ok_or_else(|| invalid_wal(&path, "registry delta frame length overflow"))?;
    let resulting_bytes = inspection
        .complete_len
        .checked_add(frame_bytes as u64)
        .ok_or_else(|| invalid_wal(&path, "registry delta WAL length overflow"))?;
    if resulting_bytes > MAX_REGISTRY_DELTA_WAL_BYTES as u64 {
        return Err(DaemonCoreError::RegistryDeltaWalTooLarge {
            path,
            encoded_bytes: resulting_bytes,
            max_bytes: MAX_REGISTRY_DELTA_WAL_BYTES as u64,
        });
    }

    wal.file_mut()
        .seek(SeekFrom::Start(inspection.complete_len))
        .and_then(|_| wal.file_mut().write_all(frame_header))
        .and_then(|_| wal.file_mut().write_all(payload))
        .and_then(|_| wal.file_mut().write_all(frame_footer))
        .and_then(|_| wal.file_mut().sync_all())
        .map_err(|source| {
            wal_io(
                "failed to append and synchronize registry delta WAL",
                &path,
                source,
            )
        })?;
    wal.validate_attachment()
        .map_err(|source| DaemonCoreError::StorageMutationAuthorityLost {
            operation: "registry-delta WAL append",
            path,
            source,
        })
}

#[cfg(unix)]
fn append_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
    revisions: RegistryRevisionRange,
    frame_header: &[u8; FRAME_HEADER_BYTES],
    payload: &[u8],
    frame_footer: &[u8; FRAME_FOOTER_BYTES],
    checkpoint_revision: impl FnOnce() -> Result<RegistryRevision>,
) -> Result<()> {
    let wal = match open_anchored_registry_wal(daemon, root)? {
        Some(wal) => wal,
        None => {
            initialize_anchored_registry_wal(daemon, root, checkpoint_revision()?)?;
            open_anchored_registry_wal(daemon, root)?.ok_or_else(|| {
                wal_io(
                    "failed to reopen initialized registry delta WAL",
                    registry_delta_wal_path(root),
                    std::io::Error::new(std::io::ErrorKind::NotFound, "initialized WAL is missing"),
                )
            })?
        }
    };
    append_to_wal(root, wal, revisions, frame_header, payload, frame_footer)
}

#[cfg(not(unix))]
fn append_under_task_lock(
    root: &Path,
    revisions: RegistryRevisionRange,
    frame_header: &[u8; FRAME_HEADER_BYTES],
    payload: &[u8],
    frame_footer: &[u8; FRAME_FOOTER_BYTES],
    checkpoint_revision: impl FnOnce() -> Result<RegistryRevision>,
) -> Result<()> {
    let path = registry_delta_wal_path(root);
    let state = open_daemon_state(root)?;
    let wal = match state
        .open_existing(REGISTRY_DELTA_WAL_FILE_NAME, FileAccess::ReadWrite)
        .map_err(|source| wal_io("failed to open registry delta WAL", &path, source))?
    {
        Some(wal) => wal,
        None => {
            state
                .write_atomic(
                    REGISTRY_DELTA_WAL_FILE_NAME,
                    &encode_wal_header(checkpoint_revision()?),
                )
                .map_err(|source| {
                    wal_io("failed to initialize registry delta WAL", &path, source)
                })?;
            state
                .open_existing(REGISTRY_DELTA_WAL_FILE_NAME, FileAccess::ReadWrite)
                .map_err(|source| {
                    wal_io(
                        "failed to reopen initialized registry delta WAL",
                        &path,
                        source,
                    )
                })?
                .ok_or_else(|| {
                    wal_io(
                        "failed to reopen initialized registry delta WAL",
                        &path,
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "initialized WAL is missing",
                        ),
                    )
                })?
        }
    };
    append_to_wal(root, wal, revisions, frame_header, payload, frame_footer)
}

#[cfg(unix)]
fn open_anchored_registry_wal<'a>(
    daemon: &'a CapabilityDir,
    root: &Path,
) -> Result<Option<AnchoredRegistryWal<'a>>> {
    let name = OsStr::new(REGISTRY_DELTA_WAL_FILE_NAME);
    let path = registry_delta_wal_path(root);
    let Some(metadata) = daemon
        .entry_metadata(name)
        .map_err(|source| wal_io("failed to inspect registry delta WAL", &path, source))?
    else {
        return Ok(None);
    };
    let Some(file) = daemon
        .open_existing_append_file(name)
        .map_err(|source| wal_io("failed to open registry delta WAL", &path, source))?
    else {
        return Err(wal_io(
            "failed to open registry delta WAL",
            path,
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "registry delta WAL disappeared during authenticated open",
            ),
        ));
    };
    let wal = AnchoredRegistryWal {
        daemon,
        file,
        identity: metadata.identity,
    };
    wal.validate_attachment()
        .map_err(|source| DaemonCoreError::StorageMutationAuthorityLost {
            operation: "registry-delta WAL open",
            path,
            source,
        })?;
    Ok(Some(wal))
}

#[cfg(unix)]
fn initialize_anchored_registry_wal(
    daemon: &CapabilityDir,
    root: &Path,
    revision: RegistryRevision,
) -> Result<()> {
    let path = registry_delta_wal_path(root);
    daemon
        .write_json_atomically(
            OsStr::new(REGISTRY_DELTA_WAL_FILE_NAME),
            &encode_wal_header(revision),
            ".registry-delta-wal-write",
        )
        .map_err(|error| {
            wal_io(
                "failed to initialize registry delta WAL",
                path,
                error.source,
            )
        })
}

#[cfg(unix)]
fn reset_wal(root: &Path, daemon: &CapabilityDir, revision: RegistryRevision) -> Result<()> {
    let path = registry_delta_wal_path(root);
    let _authenticated_existing_wal = open_anchored_registry_wal(daemon, root)?;
    daemon
        .write_json_atomically(
            OsStr::new(REGISTRY_DELTA_WAL_FILE_NAME),
            &encode_wal_header(revision),
            ".registry-delta-wal-write",
        )
        .map_err(|error| {
            wal_io(
                "failed to reset committed registry delta WAL",
                path,
                error.source,
            )
        })
}

#[cfg(not(unix))]
fn reset_wal(root: &Path, revision: RegistryRevision) -> Result<()> {
    let path = registry_delta_wal_path(root);
    let state = open_daemon_state(root)?;
    state
        .write_atomic(REGISTRY_DELTA_WAL_FILE_NAME, &encode_wal_header(revision))
        .map_err(|source| wal_io("failed to reset committed registry delta WAL", path, source))
}

fn validate_checkpoint_candidate(
    root: &Path,
    loaded: &LoadedTaskWatchRegistry,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    revision: RegistryRevision,
) -> Result<()> {
    if revision != loaded.replayed_revision {
        return Err(DaemonCoreError::RegistryDeltaRevisionMismatch {
            path: registry_delta_wal_path(root),
            expected_first: loaded.replayed_revision.get(),
            actual_first: revision.get(),
            actual_last: revision.get(),
        });
    }
    let expected_tasks = serde_json::to_value(&loaded.tasks).map_err(|source| {
        DaemonCoreError::json(
            "failed to compare replayed task registry for",
            task_registry_path(root),
            source,
        )
    })?;
    let supplied_tasks = serde_json::to_value(tasks).map_err(|source| {
        DaemonCoreError::json(
            "failed to compare checkpoint task registry for",
            task_registry_path(root),
            source,
        )
    })?;
    if expected_tasks != supplied_tasks {
        return Err(DaemonCoreError::InvalidRegistryDeltaBatch {
            root: root.to_path_buf(),
            message: format!(
                "checkpoint task registry does not equal WAL replay at revision {}",
                revision.get()
            ),
        });
    }
    let expected_watches = serde_json::to_value(&loaded.watches).map_err(|source| {
        DaemonCoreError::json(
            "failed to compare replayed watch registry for",
            watch_registry_path(root),
            source,
        )
    })?;
    let supplied_watches = serde_json::to_value(watches).map_err(|source| {
        DaemonCoreError::json(
            "failed to compare checkpoint watch registry for",
            watch_registry_path(root),
            source,
        )
    })?;
    if expected_watches != supplied_watches {
        return Err(DaemonCoreError::InvalidRegistryDeltaBatch {
            root: root.to_path_buf(),
            message: format!(
                "checkpoint watch registry does not equal WAL replay at revision {}",
                revision.get()
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn open_daemon_state(root: &Path) -> Result<StateDir> {
    let path = daemon_dir(root);
    StateDir::open(root, &[".packet28", "daemon"], true)
        .map_err(|source| wal_io("failed to open registry delta WAL directory", path, source))
}

trait RegistryWalFile {
    fn file_mut(&mut self) -> &mut std::fs::File;
    fn len(&self) -> std::io::Result<u64>;
    fn validate_attachment(&self) -> std::io::Result<()>;
}

#[cfg(not(unix))]
impl RegistryWalFile for StateFile {
    fn file_mut(&mut self) -> &mut std::fs::File {
        StateFile::file_mut(self)
    }

    fn len(&self) -> std::io::Result<u64> {
        StateFile::len(self)
    }

    fn validate_attachment(&self) -> std::io::Result<()> {
        StateFile::validate_attachment(self)
    }
}

#[cfg(unix)]
struct AnchoredRegistryWal<'a> {
    daemon: &'a CapabilityDir,
    file: File,
    identity: crate::retention::FileIdentity,
}

#[cfg(unix)]
impl RegistryWalFile for AnchoredRegistryWal<'_> {
    fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    fn len(&self) -> std::io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn validate_attachment(&self) -> std::io::Result<()> {
        self.daemon.validate_display_path_attachment()?;
        self.daemon.authenticate_regular_file_with_link_count(
            std::ffi::OsStr::new(REGISTRY_DELTA_WAL_FILE_NAME),
            self.identity,
            1,
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct WalInspection {
    base_revision: RegistryRevision,
    last_revision: RegistryRevision,
    complete_len: u64,
    last_frame: Option<FrameTail>,
}

#[derive(Clone, Copy, Debug)]
struct FrameTail {
    offset: u64,
    payload_len: u64,
    payload_checksum: [u8; 32],
    revisions: RegistryRevisionRange,
}

fn scan_wal(
    wal: &mut impl RegistryWalFile,
    path: &Path,
    repair_torn_suffix: bool,
    mut visit: impl FnMut(RegistryRevisionRange, RegistryDeltaBatch) -> Result<()>,
) -> Result<WalInspection> {
    let file_len = wal
        .len()
        .map_err(|source| wal_io("failed to inspect registry delta WAL", path, source))?;
    if file_len > MAX_REGISTRY_DELTA_WAL_BYTES as u64 {
        return Err(DaemonCoreError::RegistryDeltaWalTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: file_len,
            max_bytes: MAX_REGISTRY_DELTA_WAL_BYTES as u64,
        });
    }
    if file_len < WAL_HEADER_BYTES as u64 {
        return Err(invalid_wal(
            path,
            format!("WAL header is truncated: {file_len} bytes, expected {WAL_HEADER_BYTES}"),
        ));
    }
    wal.file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| wal_io("failed to seek registry delta WAL", path, source))?;
    let mut wal_header = [0_u8; WAL_HEADER_BYTES];
    wal.file_mut()
        .read_exact(&mut wal_header)
        .map_err(|source| wal_io("failed to read registry delta WAL header", path, source))?;
    let base_revision = decode_wal_header(path, &wal_header)?;
    let mut last_revision = base_revision;
    let mut last_frame = None;
    let mut offset = WAL_HEADER_BYTES as u64;

    loop {
        let remaining = file_len.saturating_sub(offset);
        if remaining == 0 {
            break;
        }
        if remaining < FRAME_HEADER_BYTES as u64 {
            return repair_or_reject_torn_suffix(
                wal,
                path,
                repair_torn_suffix,
                offset,
                base_revision,
                last_revision,
                last_frame,
            );
        }

        wal.file_mut()
            .seek(SeekFrom::Start(offset))
            .map_err(|source| wal_io("failed to seek registry delta frame", path, source))?;
        let mut frame_header = [0_u8; FRAME_HEADER_BYTES];
        wal.file_mut()
            .read_exact(&mut frame_header)
            .map_err(|source| wal_io("failed to read registry delta frame header", path, source))?;
        let decoded = decode_frame_header(path, offset, &frame_header)?;
        let frame_len = (FRAME_HEADER_BYTES as u64)
            .checked_add(decoded.payload_len)
            .and_then(|bytes| bytes.checked_add(FRAME_FOOTER_BYTES as u64))
            .ok_or_else(|| invalid_wal(path, "registry delta frame length overflow"))?;
        if remaining < frame_len {
            if let Some(authenticated_payload_len) = payload_len_authenticated_by_final_footer(
                wal,
                path,
                offset,
                file_len,
                &frame_header,
            )? {
                return Err(invalid_wal(
                    path,
                    format!(
                        "complete frame at byte {offset} has corrupted payload length: header \
                         encodes {}, footer authenticates {authenticated_payload_len}",
                        decoded.payload_len
                    ),
                ));
            }
            return repair_or_reject_torn_suffix(
                wal,
                path,
                repair_torn_suffix,
                offset,
                base_revision,
                last_revision,
                last_frame,
            );
        }
        let expected_first = last_revision
            .checked_next()
            .ok_or_else(|| invalid_wal(path, "registry revision is exhausted before WAL end"))?;
        if decoded.revisions.first != expected_first {
            return Err(invalid_wal(
                path,
                format!(
                    "revision gap at byte {offset}: expected {}, found {}..={}",
                    expected_first.get(),
                    decoded.revisions.first.get(),
                    decoded.revisions.last.get()
                ),
            ));
        }

        let payload_len = usize::try_from(decoded.payload_len)
            .map_err(|_| invalid_wal(path, "registry delta payload length does not fit memory"))?;
        let mut payload = Vec::new();
        payload.try_reserve_exact(payload_len).map_err(|source| {
            wal_io(
                "failed to reserve registry delta frame",
                path,
                source.into(),
            )
        })?;
        payload.resize(payload_len, 0);
        wal.file_mut()
            .read_exact(&mut payload)
            .map_err(|source| wal_io("failed to read registry delta frame", path, source))?;
        if blake3::hash(&payload).as_bytes() != &decoded.payload_checksum {
            return Err(invalid_wal(
                path,
                format!("payload checksum mismatch for complete frame at byte {offset}"),
            ));
        }
        let batch: RegistryDeltaBatch = serde_json::from_slice(&payload).map_err(|source| {
            invalid_wal(
                path,
                format!("complete frame at byte {offset} has invalid JSON: {source}"),
            )
        })?;
        batch.validate().map_err(|error| {
            invalid_wal(
                path,
                format!("complete frame at byte {offset} is invalid: {error}"),
            )
        })?;
        let mut footer = [0_u8; FRAME_FOOTER_BYTES];
        wal.file_mut()
            .read_exact(&mut footer)
            .map_err(|source| wal_io("failed to read registry delta frame footer", path, source))?;
        decode_frame_footer(path, offset, frame_len, &decoded, &frame_header, &footer)?;
        visit(decoded.revisions, batch)?;
        last_revision = decoded.revisions.last;
        last_frame = Some(FrameTail {
            offset,
            payload_len: decoded.payload_len,
            payload_checksum: decoded.payload_checksum,
            revisions: decoded.revisions,
        });
        offset = offset
            .checked_add(frame_len)
            .ok_or_else(|| invalid_wal(path, "registry delta WAL offset overflow"))?;
    }

    wal.validate_attachment()
        .map_err(|source| wal_io("registry delta WAL detached during read", path, source))?;
    Ok(WalInspection {
        base_revision,
        last_revision,
        complete_len: offset,
        last_frame,
    })
}

fn payload_len_authenticated_by_final_footer(
    wal: &mut impl RegistryWalFile,
    path: &Path,
    frame_offset: u64,
    file_len: u64,
    frame_header: &[u8; FRAME_HEADER_BYTES],
) -> Result<Option<u64>> {
    let frame_len = file_len.saturating_sub(frame_offset);
    let minimum_frame_len = (FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES) as u64;
    let Some(payload_len) = frame_len.checked_sub(minimum_frame_len) else {
        return Ok(None);
    };
    wal.file_mut()
        .seek(SeekFrom::Start(file_len - FRAME_FOOTER_BYTES as u64))
        .map_err(|source| {
            wal_io(
                "failed to seek final registry delta frame footer",
                path,
                source,
            )
        })?;
    let mut footer = [0_u8; FRAME_FOOTER_BYTES];
    wal.file_mut().read_exact(&mut footer).map_err(|source| {
        wal_io(
            "failed to read final registry delta frame footer",
            path,
            source,
        )
    })?;
    if frame_length_from_footer(&footer) != Some(frame_len)
        || footer[16..24] != frame_header[32..40]
    {
        return Ok(None);
    }

    let mut authenticated_header = *frame_header;
    authenticated_header[16..24].copy_from_slice(&payload_len.to_le_bytes());
    if footer[24..56] != blake3::hash(&authenticated_header).as_bytes()[..] {
        return Ok(None);
    }
    Ok(Some(payload_len))
}

fn repair_or_reject_torn_suffix(
    wal: &mut impl RegistryWalFile,
    path: &Path,
    repair: bool,
    complete_len: u64,
    base_revision: RegistryRevision,
    last_revision: RegistryRevision,
    last_frame: Option<FrameTail>,
) -> Result<WalInspection> {
    if !repair {
        return Err(invalid_wal(path, "registry delta WAL ends in a torn frame"));
    }
    wal.file_mut()
        .set_len(complete_len)
        .and_then(|_| wal.file_mut().sync_all())
        .map_err(|source| {
            wal_io(
                "failed to truncate and synchronize torn registry delta WAL suffix",
                path,
                source,
            )
        })?;
    wal.validate_attachment()
        .map_err(|source| DaemonCoreError::StorageMutationAuthorityLost {
            operation: "registry-delta WAL suffix repair",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(WalInspection {
        base_revision,
        last_revision,
        complete_len,
        last_frame,
    })
}

fn inspect_wal_tail(wal: &mut impl RegistryWalFile, path: &Path) -> Result<WalInspection> {
    let file_len = wal
        .len()
        .map_err(|source| wal_io("failed to inspect registry delta WAL", path, source))?;
    if file_len > MAX_REGISTRY_DELTA_WAL_BYTES as u64 {
        return Err(DaemonCoreError::RegistryDeltaWalTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: file_len,
            max_bytes: MAX_REGISTRY_DELTA_WAL_BYTES as u64,
        });
    }
    if file_len < WAL_HEADER_BYTES as u64 {
        return Err(invalid_wal(
            path,
            format!("WAL header is truncated: {file_len} bytes, expected {WAL_HEADER_BYTES}"),
        ));
    }
    wal.file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| wal_io("failed to seek registry delta WAL", path, source))?;
    let mut wal_header = [0_u8; WAL_HEADER_BYTES];
    wal.file_mut()
        .read_exact(&mut wal_header)
        .map_err(|source| wal_io("failed to read registry delta WAL header", path, source))?;
    record_fast_tail_read(WAL_HEADER_BYTES);
    let base_revision = decode_wal_header(path, &wal_header)?;
    if file_len == WAL_HEADER_BYTES as u64 {
        return Ok(WalInspection {
            base_revision,
            last_revision: base_revision,
            complete_len: file_len,
            last_frame: None,
        });
    }
    if file_len - (WAL_HEADER_BYTES as u64) < (FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES) as u64 {
        return scan_wal(wal, path, true, |_, _| Ok(()));
    }

    wal.file_mut()
        .seek(SeekFrom::Start(file_len - FRAME_FOOTER_BYTES as u64))
        .map_err(|source| wal_io("failed to seek registry delta WAL footer", path, source))?;
    let mut footer = [0_u8; FRAME_FOOTER_BYTES];
    wal.file_mut()
        .read_exact(&mut footer)
        .map_err(|source| wal_io("failed to read registry delta WAL footer", path, source))?;
    record_fast_tail_read(FRAME_FOOTER_BYTES);
    let Some(frame_len) = frame_length_from_footer(&footer) else {
        return scan_wal(wal, path, true, |_, _| Ok(()));
    };
    let minimum_frame_len = (FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES) as u64;
    if frame_len < minimum_frame_len || frame_len > file_len.saturating_sub(WAL_HEADER_BYTES as u64)
    {
        return scan_wal(wal, path, true, |_, _| Ok(()));
    }
    let frame_offset = file_len - frame_len;
    wal.file_mut()
        .seek(SeekFrom::Start(frame_offset))
        .map_err(|source| wal_io("failed to seek final registry delta frame", path, source))?;
    let mut frame_header = [0_u8; FRAME_HEADER_BYTES];
    wal.file_mut()
        .read_exact(&mut frame_header)
        .map_err(|source| wal_io("failed to read final registry delta frame", path, source))?;
    record_fast_tail_read(FRAME_HEADER_BYTES);
    let decoded = match decode_frame_header(path, frame_offset, &frame_header) {
        Ok(decoded) => decoded,
        Err(_) => return scan_wal(wal, path, true, |_, _| Ok(())),
    };
    let expected_frame_len = (FRAME_HEADER_BYTES as u64)
        .checked_add(decoded.payload_len)
        .and_then(|bytes| bytes.checked_add(FRAME_FOOTER_BYTES as u64))
        .ok_or_else(|| invalid_wal(path, "registry delta frame length overflow"))?;
    if expected_frame_len != frame_len {
        return scan_wal(wal, path, true, |_, _| Ok(()));
    }
    if decode_frame_footer(
        path,
        frame_offset,
        frame_len,
        &decoded,
        &frame_header,
        &footer,
    )
    .is_err()
    {
        return scan_wal(wal, path, true, |_, _| Ok(()));
    }

    Ok(WalInspection {
        base_revision,
        last_revision: decoded.revisions.last,
        complete_len: file_len,
        last_frame: Some(FrameTail {
            offset: frame_offset,
            payload_len: decoded.payload_len,
            payload_checksum: decoded.payload_checksum,
            revisions: decoded.revisions,
        }),
    })
}

fn verify_last_frame_payload(
    wal: &mut impl RegistryWalFile,
    path: &Path,
    tail: FrameTail,
) -> Result<()> {
    wal.file_mut()
        .seek(SeekFrom::Start(
            tail.offset
                .checked_add(FRAME_HEADER_BYTES as u64)
                .ok_or_else(|| invalid_wal(path, "registry delta payload offset overflow"))?,
        ))
        .map_err(|source| wal_io("failed to seek final registry delta payload", path, source))?;
    let payload_len = usize::try_from(tail.payload_len)
        .map_err(|_| invalid_wal(path, "final registry delta payload does not fit memory"))?;
    let mut payload = vec![0_u8; payload_len];
    wal.file_mut()
        .read_exact(&mut payload)
        .map_err(|source| wal_io("failed to read final registry delta payload", path, source))?;
    record_fast_tail_read(payload_len);
    if blake3::hash(&payload).as_bytes() != &tail.payload_checksum {
        return Err(invalid_wal(
            path,
            "final durable frame payload checksum does not match its header",
        ));
    }
    Ok(())
}

fn encode_wal_header(base_revision: RegistryRevision) -> [u8; WAL_HEADER_BYTES] {
    let mut header = [0_u8; WAL_HEADER_BYTES];
    header[0..8].copy_from_slice(&WAL_MAGIC);
    header[8..12].copy_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(WAL_HEADER_BYTES as u32).to_le_bytes());
    header[16..24].copy_from_slice(&base_revision.get().to_le_bytes());
    let checksum = blake3::hash(&header[..24]);
    header[24..56].copy_from_slice(checksum.as_bytes());
    header
}

fn decode_u32_le<const N: usize>(bytes: &[u8; N], start: usize) -> Option<u32> {
    let end = start.checked_add(std::mem::size_of::<u32>())?;
    let encoded = <[u8; std::mem::size_of::<u32>()]>::try_from(bytes.get(start..end)?).ok()?;
    Some(u32::from_le_bytes(encoded))
}

fn decode_u64_le<const N: usize>(bytes: &[u8; N], start: usize) -> Option<u64> {
    let end = start.checked_add(std::mem::size_of::<u64>())?;
    let encoded = <[u8; std::mem::size_of::<u64>()]>::try_from(bytes.get(start..end)?).ok()?;
    Some(u64::from_le_bytes(encoded))
}

fn decode_wal_header(path: &Path, header: &[u8; WAL_HEADER_BYTES]) -> Result<RegistryRevision> {
    if header[0..8] != WAL_MAGIC {
        return Err(invalid_wal(
            path,
            "WAL magic does not identify Packet28 registry deltas",
        ));
    }
    let version = decode_u32_le(header, 8)
        .ok_or_else(|| invalid_wal(path, "WAL format version is truncated"))?;
    if version != WAL_FORMAT_VERSION {
        return Err(invalid_wal(
            path,
            format!("unsupported WAL format version {version}; expected {WAL_FORMAT_VERSION}"),
        ));
    }
    let header_len = decode_u32_le(header, 12)
        .ok_or_else(|| invalid_wal(path, "WAL header length is truncated"))?;
    if header_len as usize != WAL_HEADER_BYTES {
        return Err(invalid_wal(
            path,
            format!("invalid WAL header length {header_len}"),
        ));
    }
    let expected_checksum = blake3::hash(&header[..24]);
    if header[24..56] != expected_checksum.as_bytes()[..] {
        return Err(invalid_wal(path, "WAL header checksum mismatch"));
    }
    let base_revision = decode_u64_le(header, 16)
        .ok_or_else(|| invalid_wal(path, "WAL base revision is truncated"))?;
    Ok(RegistryRevision::new(base_revision))
}

fn encode_frame_header(
    revisions: RegistryRevisionRange,
    payload: &[u8],
) -> [u8; FRAME_HEADER_BYTES] {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    header[0..8].copy_from_slice(&FRAME_MAGIC);
    header[8..12].copy_from_slice(&FRAME_FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(FRAME_HEADER_BYTES as u32).to_le_bytes());
    header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[24..32].copy_from_slice(&revisions.first.get().to_le_bytes());
    header[32..40].copy_from_slice(&revisions.last.get().to_le_bytes());
    header[40..72].copy_from_slice(blake3::hash(payload).as_bytes());
    header
}

fn encode_frame_footer(
    frame_header: &[u8; FRAME_HEADER_BYTES],
    payload_len: usize,
) -> [u8; FRAME_FOOTER_BYTES] {
    let mut footer = [0_u8; FRAME_FOOTER_BYTES];
    footer[0..8].copy_from_slice(&FRAME_FOOTER_MAGIC);
    let frame_len = FRAME_HEADER_BYTES + payload_len + FRAME_FOOTER_BYTES;
    footer[8..16].copy_from_slice(&(frame_len as u64).to_le_bytes());
    footer[16..24].copy_from_slice(&frame_header[32..40]);
    footer[24..56].copy_from_slice(blake3::hash(frame_header).as_bytes());
    footer
}

fn frame_length_from_footer(footer: &[u8; FRAME_FOOTER_BYTES]) -> Option<u64> {
    if footer[0..8] != FRAME_FOOTER_MAGIC {
        return None;
    }
    decode_u64_le(footer, 8)
}

fn decode_frame_footer(
    path: &Path,
    offset: u64,
    frame_len: u64,
    decoded: &DecodedFrameHeader,
    frame_header: &[u8; FRAME_HEADER_BYTES],
    footer: &[u8; FRAME_FOOTER_BYTES],
) -> Result<()> {
    if footer[0..8] != FRAME_FOOTER_MAGIC {
        return Err(invalid_wal(
            path,
            format!("invalid complete frame footer magic at byte {offset}"),
        ));
    }
    let encoded_frame_len = decode_u64_le(footer, 8)
        .ok_or_else(|| invalid_wal(path, "complete frame footer length is truncated"))?;
    if encoded_frame_len != frame_len {
        return Err(invalid_wal(
            path,
            format!(
                "complete frame footer length mismatch at byte {offset}: expected {frame_len}, \
                 found {encoded_frame_len}"
            ),
        ));
    }
    let encoded_last = decode_u64_le(footer, 16)
        .ok_or_else(|| invalid_wal(path, "complete frame footer revision is truncated"))?;
    if encoded_last != decoded.revisions.last.get() {
        return Err(invalid_wal(
            path,
            format!(
                "complete frame footer revision mismatch at byte {offset}: expected {}, found \
                 {encoded_last}",
                decoded.revisions.last.get()
            ),
        ));
    }
    if footer[24..56] != blake3::hash(frame_header).as_bytes()[..] {
        return Err(invalid_wal(
            path,
            format!("complete frame footer checksum mismatch at byte {offset}"),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct DecodedFrameHeader {
    payload_len: u64,
    payload_checksum: [u8; 32],
    revisions: RegistryRevisionRange,
}

fn decode_frame_header(
    path: &Path,
    offset: u64,
    header: &[u8; FRAME_HEADER_BYTES],
) -> Result<DecodedFrameHeader> {
    if header[0..8] != FRAME_MAGIC {
        return Err(invalid_wal(
            path,
            format!("invalid complete frame magic at byte {offset}"),
        ));
    }
    let version = decode_u32_le(header, 8)
        .ok_or_else(|| invalid_wal(path, "frame format version is truncated"))?;
    if version != FRAME_FORMAT_VERSION {
        return Err(invalid_wal(
            path,
            format!(
                "unsupported frame format version {version} at byte {offset}; expected \
                 {FRAME_FORMAT_VERSION}"
            ),
        ));
    }
    let header_len = decode_u32_le(header, 12)
        .ok_or_else(|| invalid_wal(path, "frame header length is truncated"))?;
    if header_len as usize != FRAME_HEADER_BYTES {
        return Err(invalid_wal(
            path,
            format!("invalid frame header length {header_len} at byte {offset}"),
        ));
    }
    let payload_len = decode_u64_le(header, 16)
        .ok_or_else(|| invalid_wal(path, "frame payload length is truncated"))?;
    if payload_len > MAX_REGISTRY_DELTA_FRAME_BYTES as u64 {
        return Err(DaemonCoreError::RegistryDeltaFrameTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: payload_len,
            max_bytes: MAX_REGISTRY_DELTA_FRAME_BYTES as u64,
        });
    }
    let first = RegistryRevision::new(
        decode_u64_le(header, 24)
            .ok_or_else(|| invalid_wal(path, "frame first revision is truncated"))?,
    );
    let last = RegistryRevision::new(
        decode_u64_le(header, 32)
            .ok_or_else(|| invalid_wal(path, "frame last revision is truncated"))?,
    );
    let revisions = RegistryRevisionRange::new(first, last).map_err(|error| {
        invalid_wal(
            path,
            format!("invalid revision range at byte {offset}: {error}"),
        )
    })?;
    let mut payload_checksum = [0_u8; 32];
    payload_checksum.copy_from_slice(&header[40..72]);
    Ok(DecodedFrameHeader {
        payload_len,
        payload_checksum,
        revisions,
    })
}

fn invalid_batch(root: &Path, error: RegistryDeltaValidationError) -> DaemonCoreError {
    DaemonCoreError::InvalidRegistryDeltaBatch {
        root: root.to_path_buf(),
        message: error.to_string(),
    }
}

fn invalid_wal(path: &Path, message: impl Into<String>) -> DaemonCoreError {
    DaemonCoreError::InvalidRegistryDeltaWal {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn wal_io(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> DaemonCoreError {
    DaemonCoreError::io(operation, path.as_ref(), source)
}

fn record_fast_tail_read(bytes: usize) {
    #[cfg(test)]
    FAST_TAIL_BYTES_READ.with(|observed| {
        observed.set(observed.get().saturating_add(bytes as u64));
    });
    #[cfg(not(test))]
    let _ = bytes;
}

fn record_apply_watch_scan() {
    #[cfg(test)]
    APPLY_WATCH_RECORDS_SCANNED.with(|observed| {
        observed.set(observed.get().saturating_add(1));
    });
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Seek as _, SeekFrom, Write as _};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(target_vendor = "apple")]
    use std::process::Command;
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    use packet28_daemon_protocol::commands::WatchSpec;
    use tempfile::tempdir;

    use super::*;

    fn task(task_id: &str, watch_ids: &[&str]) -> TaskRecord {
        TaskRecord {
            task_id: task_id.to_string(),
            watch_ids: watch_ids
                .iter()
                .map(|watch_id| (*watch_id).to_string())
                .collect(),
            ..TaskRecord::default()
        }
    }

    fn watch(watch_id: &str, task_id: &str) -> WatchRegistration {
        WatchRegistration {
            watch_id: watch_id.to_string(),
            spec: WatchSpec {
                task_id: task_id.to_string(),
                ..WatchSpec::default()
            },
            active: true,
            ..WatchRegistration::default()
        }
    }

    fn checkpoint(
        root: &Path,
        task_records: impl IntoIterator<Item = TaskRecord>,
        watch_records: impl IntoIterator<Item = WatchRegistration>,
    ) {
        let tasks = TaskRegistry {
            tasks: task_records
                .into_iter()
                .map(|task| (task.task_id.clone(), task))
                .collect(),
        };
        let watches = WatchRegistry {
            watches: watch_records.into_iter().collect(),
        };
        save_task_watch_registry_checkpoint(root, &tasks, &watches).unwrap();
    }

    fn add_watch_delta(task_id: &str, existing: &[&str], added: &str) -> RegistryDeltaBatch {
        let mut watch_ids = existing
            .iter()
            .map(|watch_id| (*watch_id).to_string())
            .collect::<Vec<_>>();
        watch_ids.push(added.to_string());
        RegistryDeltaBatch {
            task_upserts: BTreeMap::from([(
                task_id.to_string(),
                TaskRecord {
                    task_id: task_id.to_string(),
                    watch_ids,
                    ..TaskRecord::default()
                },
            )]),
            watch_upserts: BTreeMap::from([(added.to_string(), watch(added, task_id))]),
            watch_upsert_order: vec![added.to_string()],
            ..RegistryDeltaBatch::default()
        }
    }

    fn encoded_frame(revisions: RegistryRevisionRange, batch: &RegistryDeltaBatch) -> Vec<u8> {
        let payload = serde_json::to_vec(batch).unwrap();
        let header = encode_frame_header(revisions, &payload);
        let footer = encode_frame_footer(&header, payload.len());
        [header.as_slice(), payload.as_slice(), footer.as_slice()].concat()
    }

    #[test]
    fn fixed_width_decoders_are_little_endian_and_bounds_checked() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

        assert_eq!(decode_u32_le(&bytes, 0), Some(0x0403_0201));
        assert_eq!(decode_u32_le(&bytes, 4), Some(0x0807_0605));
        assert_eq!(decode_u64_le(&bytes, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(decode_u32_le(&bytes, 5), None);
        assert_eq!(decode_u64_le(&bytes, 1), None);
        assert_eq!(decode_u32_le(&bytes, usize::MAX), None);
    }

    #[test]
    fn apply_is_failure_atomic_and_preserves_observable_watch_order() {
        let mut tasks = TaskRegistry {
            tasks: BTreeMap::from([("task".to_string(), task("task", &["one", "two", "three"]))]),
        };
        let mut watches = WatchRegistry {
            watches: vec![
                watch("one", "task"),
                watch("two", "task"),
                watch("three", "task"),
            ],
        };
        let before = serde_json::to_value((&tasks, &watches)).unwrap();
        let invalid = RegistryDeltaBatch {
            task_upserts: BTreeMap::from([("task".to_string(), task("other", &[]))]),
            ..RegistryDeltaBatch::default()
        };

        assert!(matches!(
            invalid.apply_to(&mut tasks, &mut watches),
            Err(RegistryDeltaValidationError::TaskIdentifierMismatch { .. })
        ));
        assert_eq!(serde_json::to_value((&tasks, &watches)).unwrap(), before);

        let delta = RegistryDeltaBatch {
            watch_upserts: BTreeMap::from([
                ("one".to_string(), watch("one", "task")),
                ("two".to_string(), watch("two", "task")),
            ]),
            // `two` is explicitly removed then reinserted. `one` is a plain
            // replacement and therefore keeps its original position.
            watch_upsert_order: vec!["two".to_string(), "one".to_string()],
            watch_removals: BTreeSet::from(["two".to_string()]),
            ..RegistryDeltaBatch::default()
        };
        delta.apply_to(&mut tasks, &mut watches).unwrap();

        assert_eq!(
            watches
                .watches
                .iter()
                .map(|watch| watch.watch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "three", "two"]
        );
    }

    #[test]
    fn task_only_apply_preserves_duplicate_watch_validation_before_mutation() {
        let mut tasks = TaskRegistry {
            tasks: BTreeMap::from([(
                "task".to_string(),
                TaskRecord {
                    task_id: "task".to_string(),
                    watch_ids: vec!["duplicate".to_string()],
                    ..TaskRecord::default()
                },
            )]),
        };
        let mut watches = WatchRegistry {
            watches: vec![watch("duplicate", "task"), watch("duplicate", "task")],
        };
        let mut updated = tasks.tasks["task"].clone();
        updated.last_event_seq = 9;
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| observed.set(0));

        let error = RegistryDeltaBatch::default()
            .upsert_task(updated)
            .apply_to(&mut tasks, &mut watches)
            .unwrap_err();

        assert!(matches!(
            error,
            RegistryDeltaValidationError::DuplicateWatchIdentifier { ref watch_id }
                if watch_id == "duplicate"
        ));
        assert_eq!(tasks.tasks["task"].last_event_seq, 0);
        assert_eq!(watches.watches.len(), 2);
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| assert_eq!(observed.get(), 2));
    }

    #[test]
    fn authenticated_task_only_apply_does_not_scan_or_index_watches() {
        let watch_ids = (0..1_024)
            .map(|ordinal| format!("watch-{ordinal}"))
            .collect::<Vec<_>>();
        let mut tasks = TaskRegistry {
            tasks: BTreeMap::from([(
                "task".to_string(),
                TaskRecord {
                    task_id: "task".to_string(),
                    watch_ids: watch_ids.clone(),
                    ..TaskRecord::default()
                },
            )]),
        };
        let mut watches = WatchRegistry {
            watches: watch_ids
                .iter()
                .map(|watch_id| watch(watch_id, "task"))
                .collect(),
        };
        let mut updated = tasks.tasks["task"].clone();
        updated.last_event_seq = 9;
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| observed.set(0));

        let relationships_unchanged = RegistryDeltaBatch::default()
            .upsert_task(updated)
            .apply_to_authenticated(&mut tasks, &mut watches)
            .unwrap();

        assert!(relationships_unchanged);
        assert_eq!(tasks.tasks["task"].last_event_seq, 9);
        assert_eq!(watches.watches.len(), 1_024);
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| assert_eq!(observed.get(), 0));
    }

    #[test]
    fn watch_mutation_validates_every_existing_watch_before_apply() {
        let mut tasks = TaskRegistry {
            tasks: BTreeMap::from([("task".to_string(), task("task", &["one", "two"]))]),
        };
        let mut watches = WatchRegistry {
            watches: vec![watch("one", "task"), watch("two", "task")],
        };
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| observed.set(0));

        RegistryDeltaBatch::default()
            .upsert_watch(watch("one", "task"))
            .apply_to(&mut tasks, &mut watches)
            .unwrap();

        APPLY_WATCH_RECORDS_SCANNED.with(|observed| assert_eq!(observed.get(), 2));
        assert_eq!(
            watches
                .watches
                .iter()
                .map(|watch| watch.watch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn merge_distinguishes_upsert_then_remove_from_remove_then_upsert() {
        let upsert = RegistryDeltaBatch {
            watch_upserts: BTreeMap::from([("watch".to_string(), watch("watch", "task"))]),
            watch_upsert_order: vec!["watch".to_string()],
            ..RegistryDeltaBatch::default()
        };
        let removal = RegistryDeltaBatch {
            watch_removals: BTreeSet::from(["watch".to_string()]),
            ..RegistryDeltaBatch::default()
        };

        let mut upsert_then_remove = upsert.clone();
        upsert_then_remove
            .merge_later_wins(removal.clone())
            .unwrap();
        assert!(upsert_then_remove.watch_upserts.is_empty());
        assert!(upsert_then_remove.watch_upsert_order.is_empty());
        assert_eq!(
            upsert_then_remove.watch_removals,
            BTreeSet::from(["watch".to_string()])
        );

        let mut remove_then_upsert = removal;
        remove_then_upsert.merge_later_wins(upsert).unwrap();
        assert!(remove_then_upsert.watch_upserts.contains_key("watch"));
        assert_eq!(
            remove_then_upsert.watch_removals,
            BTreeSet::from(["watch".to_string()])
        );
        assert_eq!(remove_then_upsert.watch_upsert_order, vec!["watch"]);
    }

    #[test]
    fn merge_keeps_one_order_entry_across_remove_then_repeated_upsert() {
        let mut first_upsert = watch("watch", "task");
        first_upsert.last_error = Some("first".to_string());
        let mut latest_upsert = watch("watch", "task");
        latest_upsert.last_error = Some("latest".to_string());
        let mut merged = RegistryDeltaBatch::default().remove_watch("watch");

        merged
            .merge_later_wins(RegistryDeltaBatch::default().upsert_watch(first_upsert))
            .unwrap();
        merged
            .merge_later_wins(RegistryDeltaBatch::default().upsert_watch(latest_upsert))
            .unwrap();

        merged.validate().unwrap();
        assert_eq!(merged.watch_upsert_order, vec!["watch"]);
        assert_eq!(
            merged.watch_upserts["watch"].last_error.as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn consuming_builders_preserve_later_wins_and_reinsertion_order() {
        let task_removed = RegistryDeltaBatch::default()
            .upsert_task(task("task", &[]))
            .remove_task("task");
        assert!(task_removed.task_upserts.is_empty());
        assert_eq!(
            task_removed.task_removals,
            BTreeSet::from(["task".to_string()])
        );

        let task_upserted = task_removed.upsert_task(task("task", &[]));
        assert!(task_upserted.task_removals.is_empty());
        assert!(task_upserted.task_upserts.contains_key("task"));

        let watches = RegistryDeltaBatch::default()
            .upsert_watch(watch("first", "task"))
            .upsert_watch(watch("second", "task"))
            .upsert_watch(watch("first", "task"))
            .remove_watch("second")
            .upsert_watch(watch("second", "task"));

        assert_eq!(watches.watch_upsert_order, vec!["first", "second"]);
        assert_eq!(
            watches.watch_removals,
            BTreeSet::from(["second".to_string()])
        );
        watches.validate().unwrap();

        let removed_last = watches.remove_watch("first");
        assert!(!removed_last.watch_upserts.contains_key("first"));
        assert_eq!(removed_last.watch_upsert_order, vec!["second"]);
        assert!(removed_last.watch_removals.contains("first"));
    }

    #[test]
    fn append_replays_a_coalesced_atomic_task_watch_delta() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one"])],
            [watch("one", "task")],
        );
        let delta = add_watch_delta("task", &["one"], "two");

        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::new(RegistryRevision::new(1), RegistryRevision::new(3)).unwrap(),
            &delta,
        )
        .unwrap();
        let (loaded, tails) =
            load_task_watch_registry_with_deltas_and_event_tails(root.path()).unwrap();

        assert_eq!(loaded.checkpoint_revision, RegistryRevision::ZERO);
        assert_eq!(loaded.replayed_revision, RegistryRevision::new(3));
        assert_eq!(loaded.tasks.tasks["task"].watch_ids, vec!["one", "two"]);
        assert_eq!(
            loaded
                .watches
                .watches
                .iter()
                .map(|watch| watch.watch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(tails, BTreeMap::from([("task".to_string(), None)]));
    }

    #[test]
    fn recovering_load_quarantines_a_corrupt_task_event_log_and_keeps_healthy_tasks() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [task("bad", &[]), task("good", &[])], []);

        {
            let lease =
                crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
            let authority = load_registry_admission_authority(root.path(), lease).unwrap();
            for task_id in ["bad", "good"] {
                append_next_task_event_with_authority(
                    root.path(),
                    &authority,
                    task_id,
                    &DaemonEvent {
                        kind: "seed".to_string(),
                        occurred_at_unix: 1,
                        data: serde_json::Value::Null,
                    },
                )
                .unwrap();
            }
            // A second event for "bad" so dropping the first frame leaves a
            // non-contiguous log that starts at sequence 2.
            append_next_task_event_with_authority(
                root.path(),
                &authority,
                "bad",
                &DaemonEvent {
                    kind: "seed-2".to_string(),
                    occurred_at_unix: 2,
                    data: serde_json::Value::Null,
                },
            )
            .unwrap();
        }

        let bad_storage = checked_task_storage_id(root.path(), "bad").unwrap();
        let bad_log = task_event_log_path(root.path(), &bad_storage);
        let contents = fs::read_to_string(&bad_log).unwrap();
        let mut frames = contents.lines().filter(|line| !line.is_empty());
        let _first = frames.next().expect("first frame present");
        let second = frames.next().expect("second frame present");
        fs::write(&bad_log, format!("{second}\n")).unwrap();

        // Baseline: the strict loader fails closed on the corrupt event log.
        assert!(matches!(
            load_task_watch_registry_with_deltas_and_event_tails(root.path()),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));

        // Recovering: the corrupt log is moved aside so both tasks load and
        // "bad" reports an empty tail.
        let (loaded, tails, quarantined) =
            load_task_watch_registry_recovering_corrupt_event_logs(root.path()).unwrap();
        assert!(loaded.tasks.tasks.contains_key("good"));
        assert!(loaded.tasks.tasks.contains_key("bad"));
        assert_eq!(tails.get("good"), Some(&Some(1)));
        assert_eq!(tails.get("bad"), Some(&None));
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].task_id, "bad");

        // The corrupt log was physically moved aside for inspection.
        let quarantined_path = quarantined[0]
            .quarantined_path
            .clone()
            .expect("corrupt log should be moved aside");
        assert!(!bad_log.exists());
        assert!(quarantined_path.exists());

        // The recovery is stable: a plain reload now succeeds with an empty
        // tail for "bad" (no corruption remains).
        let (reloaded, reloaded_tails) =
            load_task_watch_registry_with_deltas_and_event_tails(root.path()).unwrap();
        assert!(reloaded.tasks.tasks.contains_key("bad"));
        assert_eq!(reloaded_tails.get("bad"), Some(&None));
    }

    #[test]
    fn wal_append_rejects_new_task_that_would_adopt_a_managed_entry() {
        for (event_namespace, alias_spelling) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let root = tempdir().unwrap();
            checkpoint(root.path(), [], []);
            let expected_name = if event_namespace {
                "new-task.events.jsonl"
            } else {
                "new-task"
            };
            let actual_name = if alias_spelling {
                if event_namespace {
                    "NEW-TASK.events.jsonl"
                } else {
                    "NEW-TASK"
                }
            } else {
                expected_name
            };
            let namespace = if event_namespace {
                task_events_dir(root.path())
            } else {
                task_artifacts_dir(root.path())
            };
            fs::create_dir_all(&namespace).unwrap();
            let managed = namespace.join(actual_name);
            if event_namespace {
                fs::write(&managed, b"event-before\n").unwrap();
            } else {
                fs::create_dir(&managed).unwrap();
            }
            let wal_path = registry_delta_wal_path(root.path());
            assert!(!wal_path.exists());
            let delta = RegistryDeltaBatch::default().upsert_task(task("new-task", &[]));

            let error = append_task_watch_registry_delta(
                root.path(),
                RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
                &delta,
            )
            .unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert!(!wal_path.exists());
            assert!(managed.exists());
        }
    }

    #[test]
    fn admitted_wal_append_rejects_new_task_that_would_adopt_a_managed_entry() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let managed = task_events_dir(root.path()).join("new-task.events.jsonl");
        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        fs::write(&managed, b"event-before\n").unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let mut authority = load_registry_admission_authority(root.path(), lease).unwrap();
        let delta = RegistryDeltaBatch::default().upsert_task(task("new-task", &[]));
        let wal_path = registry_delta_wal_path(root.path());

        let error = append_task_watch_registry_delta_with_authority(
            root.path(),
            &mut authority,
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert!(!wal_path.exists());
        assert_eq!(fs::read(managed).unwrap(), b"event-before\n");
    }

    #[test]
    fn wal_append_rejects_nonportable_and_aliasing_task_identifiers() {
        for (existing, rejected) in [(None, "Task"), (Some("task"), "TASK")] {
            let root = tempdir().unwrap();
            checkpoint(
                root.path(),
                existing.into_iter().map(|task_id| task(task_id, &[])),
                [],
            );
            let wal_path = registry_delta_wal_path(root.path());
            let before = fs::read(&wal_path).ok();
            let delta = RegistryDeltaBatch::default().upsert_task(task(rejected, &[]));

            let error = append_task_watch_registry_delta(
                root.path(),
                RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
                &delta,
            )
            .unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert_eq!(fs::read(&wal_path).ok(), before);
        }
    }

    #[test]
    fn wal_append_allows_an_existing_task_to_reuse_its_managed_entries() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [task("existing", &[])], []);
        let artifact = task_artifacts_dir(root.path()).join("existing");
        let events = task_events_dir(root.path()).join("existing.events.jsonl");
        fs::create_dir_all(&artifact).unwrap();
        fs::create_dir_all(events.parent().unwrap()).unwrap();
        fs::write(&events, b"existing\n").unwrap();
        let mut updated = task("existing", &[]);
        updated.last_event_seq = 7;

        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(updated),
        )
        .unwrap();

        assert!(registry_delta_wal_path(root.path()).exists());
        assert!(artifact.exists());
        assert_eq!(fs::read(events).unwrap(), b"existing\n");
    }

    #[test]
    fn registry_delta_admission_requires_a_daemon_lifecycle_lease() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let writer = acquire_task_store_writer_lease(root.path()).unwrap();

        let error = load_registry_admission_authority(root.path(), writer).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
    }

    #[test]
    fn registry_delta_admission_rejects_a_lease_for_another_root() {
        let root = tempdir().unwrap();
        let other = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        ensure_daemon_dir(other.path()).unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(other.path()).unwrap();

        let error = load_registry_admission_authority(root.path(), lease).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
    }

    #[test]
    fn registry_authority_rejects_a_different_root_without_mutation() {
        let root = tempdir().unwrap();
        let other = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        ensure_daemon_dir(other.path()).unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let mut authority = load_registry_admission_authority(root.path(), lease).unwrap();
        let wal_path = registry_delta_wal_path(other.path());

        let error = append_task_watch_registry_delta_with_authority(
            other.path(),
            &mut authority,
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(task("new-task", &[])),
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert!(!wal_path.exists());
        assert_eq!(authority.revision(), RegistryRevision::ZERO);
        assert!(!authority.contains_task("new-task"));
    }

    #[test]
    fn registry_authority_retains_lifecycle_ownership_until_drop() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let authority = load_registry_admission_authority(root.path(), lease).unwrap();

        assert!(
            crate::task_store_lease::try_acquire_task_store_retention_lease(root.path())
                .unwrap()
                .is_none()
        );
        drop(authority);
        assert!(
            crate::task_store_lease::try_acquire_task_store_retention_lease(root.path())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn registry_authority_excludes_supported_replacement_writers_until_drop() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [task("existing", &[])], []);
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let mut authority = load_registry_admission_authority(root.path(), lease).unwrap();
        let root_path = root.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(save_task_registry(&root_path, &TaskRegistry::default()))
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let frame = append_next_task_event_with_authority(
            root.path(),
            &authority,
            "existing",
            &DaemonEvent {
                kind: "authority-held".to_string(),
                occurred_at_unix: 1,
                data: serde_json::Value::Null,
            },
        )
        .unwrap();
        assert_eq!(frame.seq, 1);
        let mut updated = task("existing", &[]);
        updated.last_event_seq = 1;
        append_task_watch_registry_delta_with_authority(
            root.path(),
            &mut authority,
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(updated),
        )
        .unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        drop(authority);
        let writer_result = finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            writer_result,
            Err(DaemonCoreError::RegistryCheckpointRequired { .. })
        ));
        writer.join().unwrap();
        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert_eq!(loaded.tasks.tasks["existing"].last_event_seq, 1);
        assert_eq!(loaded.replayed_revision, RegistryRevision::new(1));
    }

    #[test]
    fn checkpoint_binds_revision_before_reset_and_skips_retained_wal() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one"])],
            [watch("one", "task")],
        );
        let delta = add_watch_delta("task", &["one"], "two");
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap();
        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
        let retained_wal = fs::read(registry_delta_wal_path(root.path())).unwrap();

        save_task_watch_registry_checkpoint_at_revision(
            root.path(),
            &loaded.tasks,
            &loaded.watches,
            loaded.replayed_revision,
        )
        .unwrap();
        let reset_wal = fs::read(registry_delta_wal_path(root.path())).unwrap();
        assert_eq!(reset_wal.len(), WAL_HEADER_BYTES);
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(daemon_dir(root.path()).join("task-watch-checkpoint-v1.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["checkpoint"]["applied_delta_revision"],
            serde_json::Value::from(1)
        );

        // Model a crash after manifest commit but before WAL reset. Retained
        // frames are safe because replay skips the committed watermark.
        fs::write(registry_delta_wal_path(root.path()), retained_wal).unwrap();
        let restarted = load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert_eq!(restarted.checkpoint_revision, RegistryRevision::new(1));
        assert_eq!(restarted.replayed_revision, RegistryRevision::new(1));
        assert_eq!(restarted.tasks.tasks["task"].watch_ids, vec!["one", "two"]);
    }

    #[test]
    fn legacy_unpaired_registries_load_at_revision_zero() {
        let root = tempdir().unwrap();
        let tasks = TaskRegistry {
            tasks: BTreeMap::from([("task".to_string(), task("task", &[]))]),
        };
        save_task_registry(root.path(), &tasks).unwrap();
        save_watch_registry(root.path(), &WatchRegistry::default()).unwrap();

        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();

        assert!(loaded.tasks.tasks.contains_key("task"));
        assert_eq!(loaded.checkpoint_revision, RegistryRevision::ZERO);
        assert_eq!(loaded.replayed_revision, RegistryRevision::ZERO);
    }

    #[test]
    fn torn_final_frame_is_repaired_without_discarding_complete_frames() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one"])],
            [watch("one", "task")],
        );
        let first = add_watch_delta("task", &["one"], "two");
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &first,
        )
        .unwrap();
        let complete_len = fs::metadata(registry_delta_wal_path(root.path()))
            .unwrap()
            .len();
        let second = add_watch_delta("task", &["one", "two"], "three");
        let encoded = encoded_frame(
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &second,
        );
        let torn_len = FRAME_HEADER_BYTES + 7;
        let mut wal = OpenOptions::new()
            .append(true)
            .open(registry_delta_wal_path(root.path()))
            .unwrap();
        wal.write_all(&encoded[..torn_len]).unwrap();
        wal.sync_all().unwrap();

        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();

        assert_eq!(loaded.replayed_revision, RegistryRevision::new(1));
        assert_eq!(
            fs::metadata(registry_delta_wal_path(root.path()))
                .unwrap()
                .len(),
            complete_len
        );
    }

    #[test]
    fn complete_checksum_corruption_is_rejected_without_truncation() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one"])],
            [watch("one", "task")],
        );
        let delta = add_watch_delta("task", &["one"], "two");
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let original_len = fs::metadata(&path).unwrap().len();
        let mut wal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        wal.seek(SeekFrom::Start(
            (WAL_HEADER_BYTES + FRAME_HEADER_BYTES) as u64,
        ))
        .unwrap();
        wal.write_all(b"!").unwrap();
        wal.sync_all().unwrap();

        let error = load_task_watch_registry_with_deltas(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
        assert_eq!(fs::metadata(path).unwrap().len(), original_len);
    }

    #[test]
    fn complete_frame_with_corrupted_payload_length_is_rejected_without_truncation() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one"])],
            [watch("one", "task")],
        );
        let delta = add_watch_delta("task", &["one"], "two");
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let mut wal_bytes = fs::read(&path).unwrap();
        let payload_len_offset = WAL_HEADER_BYTES + 16;
        let payload_len = u64::from_le_bytes(
            wal_bytes[payload_len_offset..payload_len_offset + 8]
                .try_into()
                .unwrap(),
        );
        wal_bytes[payload_len_offset..payload_len_offset + 8]
            .copy_from_slice(&(payload_len + 1).to_le_bytes());
        fs::write(&path, &wal_bytes).unwrap();

        let error = load_task_watch_registry_with_deltas(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), wal_bytes);
    }

    #[test]
    fn interior_checksum_corruption_is_rejected_without_truncation() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        for revision in 1..=2 {
            append_task_watch_registry_delta(
                root.path(),
                RegistryRevisionRange::single(RegistryRevision::new(revision)).unwrap(),
                &batch,
            )
            .unwrap();
        }
        let path = registry_delta_wal_path(root.path());
        let original_len = fs::metadata(&path).unwrap().len();
        let mut wal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        wal.seek(SeekFrom::Start(
            (WAL_HEADER_BYTES + FRAME_HEADER_BYTES) as u64,
        ))
        .unwrap();
        wal.write_all(b"!").unwrap();
        wal.sync_all().unwrap();

        let error = load_task_watch_registry_with_deltas(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
        assert_eq!(fs::metadata(path).unwrap().len(), original_len);
    }

    #[test]
    fn complete_revision_gap_is_rejected() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let path = registry_delta_wal_path(root.path());
        let batch = RegistryDeltaBatch::default();
        let frame = encoded_frame(
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &batch,
        );
        fs::write(
            &path,
            [
                encode_wal_header(RegistryRevision::ZERO).as_slice(),
                frame.as_slice(),
            ]
            .concat(),
        )
        .unwrap();

        let error = load_task_watch_registry_with_deltas(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
    }

    #[test]
    fn relationship_invalid_delta_is_rejected_before_wal_mutation() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [task("task", &[])], []);
        let path = registry_delta_wal_path(root.path());
        let before = fs::read(&path).ok();
        let delta = RegistryDeltaBatch {
            watch_upserts: BTreeMap::from([("orphan".to_string(), watch("orphan", "task"))]),
            watch_upsert_order: vec!["orphan".to_string()],
            ..RegistryDeltaBatch::default()
        };
        let error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskWatchRegistry { .. }
        ));
        assert_eq!(fs::read(path).ok(), before);
    }

    #[test]
    fn relationship_breaking_task_and_watch_removals_leave_wal_unchanged() {
        let cases = [
            RegistryDeltaBatch::default().upsert_task(task("task", &["missing"])),
            RegistryDeltaBatch::default().remove_task("task"),
            RegistryDeltaBatch::default().remove_watch("one"),
        ];
        for delta in cases {
            let root = tempdir().unwrap();
            checkpoint(
                root.path(),
                [task("task", &["one"])],
                [watch("one", "task")],
            );
            let path = registry_delta_wal_path(root.path());
            let before = fs::read(&path).ok();

            let error = append_task_watch_registry_delta(
                root.path(),
                RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
                &delta,
            )
            .unwrap_err();

            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskWatchRegistry { .. }
            ));
            assert_eq!(fs::read(path).ok(), before);
        }
    }

    #[test]
    fn task_only_wal_replay_preserves_relationships_without_watch_scans() {
        let watch_ids = (0..1_024)
            .map(|ordinal| format!("watch-{ordinal}"))
            .collect::<Vec<_>>();
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [TaskRecord {
                task_id: "task".to_string(),
                watch_ids: watch_ids.clone(),
                ..TaskRecord::default()
            }],
            watch_ids.iter().map(|watch_id| watch(watch_id, "task")),
        );
        let mut updated = task("task", &[]);
        updated.watch_ids = watch_ids;
        updated.last_event_seq = 9;
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(updated),
        )
        .unwrap();
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| observed.set(0));

        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();

        assert_eq!(loaded.tasks.tasks["task"].last_event_seq, 9);
        assert_eq!(loaded.watches.watches.len(), 1_024);
        APPLY_WATCH_RECORDS_SCANNED.with(|observed| assert_eq!(observed.get(), 0));
    }

    #[test]
    fn exact_tail_retry_is_idempotent_but_changed_payload_is_rejected() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let revisions = RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap();
        let batch = RegistryDeltaBatch::default();
        append_task_watch_registry_delta(root.path(), revisions, &batch).unwrap();
        let path = registry_delta_wal_path(root.path());
        let committed_len = fs::metadata(&path).unwrap().len();

        append_task_watch_registry_delta(root.path(), revisions, &batch).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), committed_len);

        let changed = RegistryDeltaBatch {
            task_removals: BTreeSet::from(["missing".to_string()]),
            ..RegistryDeltaBatch::default()
        };
        let error = append_task_watch_registry_delta(root.path(), revisions, &changed).unwrap_err();
        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
        assert_eq!(fs::metadata(path).unwrap().len(), committed_len);
    }

    #[test]
    fn append_rejects_a_corrupted_previous_payload() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &batch,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let mut wal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        wal.seek(SeekFrom::Start(
            (WAL_HEADER_BYTES + FRAME_HEADER_BYTES) as u64,
        ))
        .unwrap();
        wal.write_all(b"!").unwrap();
        wal.sync_all().unwrap();
        let corrupted_len = fs::metadata(&path).unwrap().len();

        let error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &batch,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaWal { .. }
        ));
        assert_eq!(fs::metadata(path).unwrap().len(), corrupted_len);
    }

    #[test]
    fn concurrent_identical_retry_commits_one_frame() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let root_path = root.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let batch = Arc::new(RegistryDeltaBatch::default());
        let revisions = RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap();

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let barrier = Arc::clone(&barrier);
                let batch = Arc::clone(&batch);
                let root_path = root_path.clone();
                scope.spawn(move || {
                    barrier.wait();
                    append_task_watch_registry_delta(&root_path, revisions, &batch).unwrap();
                });
            }
        });

        let payload_len = serde_json::to_vec(batch.as_ref()).unwrap().len();
        assert_eq!(
            fs::metadata(registry_delta_wal_path(root.path()))
                .unwrap()
                .len(),
            (WAL_HEADER_BYTES + FRAME_HEADER_BYTES + payload_len + FRAME_FOOTER_BYTES) as u64
        );
    }

    #[test]
    fn steady_state_append_reads_a_constant_size_tail() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        for revision in 1..=64 {
            append_task_watch_registry_delta(
                root.path(),
                RegistryRevisionRange::single(RegistryRevision::new(revision)).unwrap(),
                &batch,
            )
            .unwrap();
        }
        FAST_TAIL_BYTES_READ.with(|observed| observed.set(0));

        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(65)).unwrap(),
            &batch,
        )
        .unwrap();

        FAST_TAIL_BYTES_READ.with(|observed| {
            let payload_len = serde_json::to_vec(&batch).unwrap().len() as u64;
            assert_eq!(
                observed.get(),
                (WAL_HEADER_BYTES + FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES) as u64 + payload_len
            );
        });
    }

    #[test]
    fn checkpoint_rejects_a_snapshot_with_different_watch_order() {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            [task("task", &["one", "two"])],
            [watch("one", "task"), watch("two", "task")],
        );
        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
        let mut reordered = loaded.watches.clone();
        reordered.watches.reverse();

        let error = save_task_watch_registry_checkpoint_at_revision(
            root.path(),
            &loaded.tasks,
            &reordered,
            loaded.replayed_revision,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidRegistryDeltaBatch { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn wal_symlink_never_redirects_an_append() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let victim = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let victim_path = victim.path().join("victim.wal");
        fs::write(&victim_path, b"victim").unwrap();
        symlink(&victim_path, registry_delta_wal_path(root.path())).unwrap();

        let error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default(),
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(victim_path).unwrap(), b"victim");
    }

    #[cfg(unix)]
    #[test]
    fn wal_with_non_owner_write_authority_is_rejected_without_mutation() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &batch,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();

        let error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &batch,
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o660
        );
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_wal_is_rejected_without_append_or_reset() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &batch,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let alias = root.path().join("wal-alias");
        fs::hard_link(&path, &alias).unwrap();
        let before = fs::read(&path).unwrap();

        let append_error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &batch,
        )
        .unwrap_err();
        assert!(matches!(append_error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&alias).unwrap(), before);

        let lease = acquire_task_store_writer_lease(root.path()).unwrap();
        let daemon = lease.daemon_capability().unwrap();
        let reset_error = reset_wal(root.path(), &daemon, RegistryRevision::new(1)).unwrap_err();
        assert!(matches!(reset_error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(alias).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn detached_retained_daemon_never_authorizes_a_replacement_namespace() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default(),
        )
        .unwrap();
        let lease = acquire_task_store_writer_lease(root.path()).unwrap();
        let retained = lease.daemon_capability().unwrap();
        let daemon_path = daemon_dir(root.path());
        let detached_path = root.path().join(".packet28/detached-daemon");
        fs::rename(&daemon_path, &detached_path).unwrap();
        fs::create_dir(&daemon_path).unwrap();
        fs::set_permissions(&daemon_path, fs::Permissions::from_mode(0o700)).unwrap();
        let replacement = CapabilityDir::open(&daemon_path).unwrap();

        let error =
            validate_retained_registry_daemon(root.path(), &replacement, &retained).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::StorageMutationAuthorityLost { .. }
        ));
        assert!(!registry_delta_wal_path(root.path()).exists());
        assert!(detached_path.join(REGISTRY_DELTA_WAL_FILE_NAME).is_file());
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn wal_with_extended_acl_is_rejected_without_mutation() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), [], []);
        let batch = RegistryDeltaBatch::default();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &batch,
        )
        .unwrap();
        let path = registry_delta_wal_path(root.path());
        let before = fs::read(&path).unwrap();
        assert!(Command::new("chmod")
            .arg("+a")
            .arg("everyone allow read")
            .arg(&path)
            .status()
            .unwrap()
            .success());

        let error = append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(2)).unwrap(),
            &batch,
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
