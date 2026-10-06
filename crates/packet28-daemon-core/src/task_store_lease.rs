//! Cross-process ownership for daemon task storage.
//!
//! This module provides three independent lock primitives:
//!
//! - task-store writers and daemons share the lifecycle lock while destructive
//!   maintenance owns it exclusively;
//! - one daemon owns the instance lock exclusively for its entire lifetime,
//!   while retention holds it shared to prevent startup during maintenance.
//! - clients serialize daemon discovery and bootstrap with the startup lock so
//!   concurrent agents do not launch redundant daemon processes.
//!
//! The instance lock prevents two daemons from loading and publishing
//! independent mutable state and closes the lifecycle exclusive-to-shared
//! startup handoff window. The startup lock prevents launch storms but grants
//! no daemon authority. Neither replaces the lifecycle lock because non-daemon
//! writers also need to exclude retention.

#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt;
use packet28_daemon_protocol::paths::daemon_dir;

#[cfg(unix)]
use crate::capability::CapabilityDir;
#[cfg(unix)]
use crate::retention::FileIdentity;
#[cfg(unix)]
use crate::storage::MAX_TASK_STORE_COMPONENT_BYTES;
use crate::{DaemonCoreError, Result};

const TASK_STORE_LIFECYCLE_LOCK_FILE_NAME: &str = ".task-store-lifecycle.lock";
const DAEMON_INSTANCE_LOCK_FILE_NAME: &str = ".daemon-instance.lock";
const DAEMON_STARTUP_LOCK_FILE_NAME: &str = ".daemon-startup.lock";
const REGISTRY_AUTHORITY_LOCK_FILE_NAME: &str = ".registry-authority.lock";

#[cfg(test)]
std::thread_local! {
    static INJECT_FOREIGN_DEVICE_FOR_PREFLIGHT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn inject_foreign_device_for_retention_preflight(path: PathBuf) {
    INJECT_FOREIGN_DEVICE_FOR_PREFLIGHT.with(|configured| {
        configured.replace(Some(path));
    });
}

/// RAII ownership of the workspace task-store lifecycle lock.
///
/// Clones share one underlying advisory lock. The final clone releases it.
/// The persistent lock file itself is deliberately never unlinked, which
/// prevents concurrent processes from locking different inodes for the same
/// workspace.
#[derive(Clone, Debug)]
pub struct TaskStoreLease {
    inner: Arc<LeaseInner>,
}

/// Shared daemon-instance admission held only by destructive maintenance.
///
/// This deliberately is not a [`TaskStoreLease`]: it proves that retention
/// won admission against daemon startup, but it does not grant daemon-instance
/// ownership or task-store lifecycle ownership.
#[derive(Debug)]
pub(crate) struct TaskRetentionAdmission {
    gate: TaskStoreLease,
}

impl TaskRetentionAdmission {
    pub(crate) fn authorizes(&self, retention: &TaskStoreLease) -> bool {
        self.gate.role() == LeaseRole::RetentionInstanceGate
            && retention.role() == LeaseRole::Retention
            && Arc::ptr_eq(&self.gate.inner.authority, &retention.inner.authority)
    }
}

#[derive(Debug)]
struct LeaseInner {
    file: File,
    path: PathBuf,
    role: LeaseRole,
    authority: Arc<TaskStoreAuthority>,
    #[cfg(unix)]
    lock_name: std::ffi::OsString,
    #[cfg(unix)]
    lock_identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseRole {
    Writer,
    DaemonLifecycle,
    DaemonInstance,
    DaemonStartup,
    Recovery,
    Retention,
    RetentionInstanceGate,
    RegistryWriter,
    RegistryAuthority,
}

#[derive(Debug)]
struct OpenedPersistentDaemonLock {
    file: File,
    path: PathBuf,
    authority: Arc<TaskStoreAuthority>,
    #[cfg(unix)]
    lock_name: std::ffi::OsString,
}

#[derive(Debug)]
struct TaskStoreAuthority {
    requested_root: PathBuf,
    workspace_root: PathBuf,
    #[cfg(unix)]
    workspace: CapabilityDir,
    #[cfg(unix)]
    state: CapabilityDir,
    #[cfg(unix)]
    daemon: CapabilityDir,
}

impl TaskStoreLease {
    /// Returns the persistent lock-file path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Returns the canonical workspace root retained by this lease.
    pub fn workspace_root(&self) -> &Path {
        &self.inner.authority.workspace_root
    }

    #[cfg(unix)]
    pub(crate) fn state_capability(&self) -> Result<CapabilityDir> {
        self.inner.authority.state.duplicate().map_err(|source| {
            DaemonCoreError::io(
                "failed to duplicate retained Packet28 state authority",
                self.inner.authority.state.display_path(),
                source,
            )
        })
    }

    #[cfg(unix)]
    pub(crate) fn daemon_capability(&self) -> Result<CapabilityDir> {
        self.inner.authority.daemon.duplicate().map_err(|source| {
            DaemonCoreError::io(
                "failed to duplicate retained daemon authority",
                self.inner.authority.daemon.display_path(),
                source,
            )
        })
    }

    pub(crate) fn role(&self) -> LeaseRole {
        self.inner.role
    }

    pub(crate) fn matches_root_argument(&self, root: &Path) -> bool {
        root == self.inner.authority.requested_root || root == self.inner.authority.workspace_root
    }

    #[cfg(unix)]
    pub(crate) fn validate_namespace_attachment(&self) -> Result<()> {
        self.validate_lock_attachment()?;
        let authority = &self.inner.authority;
        authority
            .workspace
            .validate_display_path_attachment()
            .and_then(|()| authority.state.validate_display_path_attachment())
            .and_then(|()| authority.daemon.validate_display_path_attachment())
            .and_then(|()| {
                if authority
                    .workspace
                    .entry_identity(std::ffi::OsStr::new(".packet28"))?
                    != Some(authority.state.identity())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "retained Packet28 state is detached from the retained workspace",
                    ));
                }
                if authority
                    .state
                    .entry_identity(std::ffi::OsStr::new("daemon"))?
                    != Some(authority.daemon.identity())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "retained daemon state is detached from the retained Packet28 state",
                    ));
                }
                Ok(())
            })
            .map_err(|source| {
                DaemonCoreError::io(
                    "retained task-store namespace is detached",
                    &authority.workspace_root,
                    source,
                )
            })
    }

    #[cfg(unix)]
    fn validate_lock_attachment(&self) -> Result<()> {
        validate_lock_attachment(
            &self.inner.file,
            &self.inner.path,
            &self.inner.authority.daemon,
            &self.inner.lock_name,
            self.inner.lock_identity,
        )
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Returns the persistent lifecycle lock path for a workspace task store.
pub fn task_store_lifecycle_lock_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_STORE_LIFECYCLE_LOCK_FILE_NAME)
}

/// Returns the persistent single-daemon instance lock path for a workspace.
pub fn daemon_instance_lock_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(DAEMON_INSTANCE_LOCK_FILE_NAME)
}

/// Returns the persistent lock path used to serialize daemon bootstrap.
pub fn daemon_startup_lock_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(DAEMON_STARTUP_LOCK_FILE_NAME)
}

/// Acquires a shared task-store writer lease.
///
/// Every production mutation of task registry, active-task state, task
/// artifacts, or task events must retain this guard through the complete
/// filesystem transaction. This call blocks while destructive retention owns
/// the exclusive lifecycle lease.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the daemon directory or lifecycle lock
/// cannot be safely created, opened, validated, or locked.
pub fn acquire_task_store_writer_lease(root: &Path) -> Result<TaskStoreLease> {
    let path = task_store_lifecycle_lock_path(root);
    let opened = open_persistent_daemon_lock(root, &path)?;
    FileExt::lock_shared(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire task-store writer lease", &path, source)
    })?;
    validated_lease(root, opened, LeaseRole::Writer)
}

/// Acquires the shared lease intended for daemon startup through shutdown.
///
/// This call blocks while destructive task-store maintenance owns the
/// exclusive lease. Acquire it before loading mutable daemon state, and retain
/// the returned value until all daemon workers have stopped and runtime-file
/// cleanup has completed.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the daemon directory or lifecycle lock
/// cannot be safely created, opened, validated, or locked.
pub fn acquire_daemon_task_store_lease(root: &Path) -> Result<TaskStoreLease> {
    let path = task_store_lifecycle_lock_path(root);
    let opened = open_persistent_daemon_lock(root, &path)?;
    FileExt::lock_shared(&opened.file).map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire daemon task-store lifecycle lease",
            &path,
            source,
        )
    })?;
    validated_lease(root, opened, LeaseRole::DaemonLifecycle)
}

pub(crate) fn acquire_registry_writer_admission(writer: &TaskStoreLease) -> Result<TaskStoreLease> {
    require_lease_role(writer, LeaseRole::Writer)?;
    let path = daemon_dir(writer.workspace_root()).join(REGISTRY_AUTHORITY_LOCK_FILE_NAME);
    let opened = open_persistent_daemon_lock(writer.workspace_root(), &path)?;
    FileExt::lock_shared(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire registry writer admission", &path, source)
    })?;
    validated_lease(writer.workspace_root(), opened, LeaseRole::RegistryWriter)
}

pub(crate) fn acquire_daemon_registry_authority(daemon: &TaskStoreLease) -> Result<TaskStoreLease> {
    require_lease_role(daemon, LeaseRole::DaemonLifecycle)?;
    let path = daemon_dir(daemon.workspace_root()).join(REGISTRY_AUTHORITY_LOCK_FILE_NAME);
    let opened = open_persistent_daemon_lock(daemon.workspace_root(), &path)?;
    FileExt::lock_exclusive(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire daemon registry authority", &path, source)
    })?;
    validated_lease(
        daemon.workspace_root(),
        opened,
        LeaseRole::RegistryAuthority,
    )
}

/// Acquires exclusive ownership of the daemon instance for a workspace.
///
/// This is intentionally non-blocking: a second daemon must fail before
/// loading mutable state instead of waiting and later publishing stale state.
/// Retain the returned guard until worker shutdown and runtime-file cleanup
/// have completed.
///
/// # Errors
///
/// Returns [`DaemonCoreError::DaemonInstanceAlreadyRunning`] when another
/// daemon owns the workspace. Returns [`DaemonCoreError::Io`] if the lock path
/// cannot be safely created, opened, validated, or locked.
pub fn acquire_daemon_instance_lease(root: &Path) -> Result<TaskStoreLease> {
    let path = daemon_instance_lock_path(root);
    let opened = open_persistent_daemon_lock(root, &path)?;
    match FileExt::try_lock_exclusive(&opened.file) {
        Ok(()) => validated_lease(root, opened, LeaseRole::DaemonInstance),
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
            Err(DaemonCoreError::DaemonInstanceAlreadyRunning { path })
        }
        Err(source) => Err(DaemonCoreError::io(
            "failed to acquire daemon instance lease",
            &path,
            source,
        )),
    }
}

/// Probes, without blocking, whether a daemon still owns the workspace.
///
/// Returns `true` when no instance lock exists or it can be acquired; the
/// probe releases any lease it took before returning. Only
/// [`DaemonCoreError::DaemonInstanceAlreadyRunning`] means a daemon still owns
/// the workspace, so an integrity or I/O failure is returned instead of being
/// mistaken for a stopped daemon.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the lock path cannot be inspected or
/// safely opened, validated, or locked.
pub fn daemon_instance_released(root: &Path) -> Result<bool> {
    let path = daemon_instance_lock_path(root);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        // No daemon has created an instance lock for this workspace.
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to inspect daemon instance lease",
                &path,
                source,
            ))
        }
    }
    match acquire_daemon_instance_lease(root) {
        Ok(lease) => {
            drop(lease);
            Ok(true)
        }
        Err(DaemonCoreError::DaemonInstanceAlreadyRunning { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Serializes daemon discovery, stale-file cleanup, and process bootstrap.
///
/// The daemon instance lease prevents two daemons from becoming live, while
/// this separate lease prevents concurrent clients from spawning a losing
/// daemon process for the same workspace. Retain it until the selected daemon
/// has published readiness or startup has failed.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the lock path cannot be safely created,
/// opened, validated, or locked.
pub fn acquire_daemon_startup_lease(root: &Path) -> Result<TaskStoreLease> {
    let path = daemon_startup_lock_path(root);
    let opened = open_persistent_daemon_lock(root, &path)?;
    FileExt::lock_exclusive(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire daemon startup lease", &path, source)
    })?;
    validated_lease(root, opened, LeaseRole::DaemonStartup)
}

/// Acquires exclusive task-store ownership for daemon-startup recovery.
///
/// Unlike explicit cleanup, startup recovery waits for an in-flight supported
/// writer or maintenance operation to finish. Acquire this before loading
/// mutable daemon state, perform recovery, then release it before taking the
/// daemon's long-lived shared lifecycle lease.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the daemon directory or lifecycle lock
/// cannot be safely created, opened, validated, or locked.
pub fn acquire_task_store_recovery_lease(root: &Path) -> Result<TaskStoreLease> {
    let path = task_store_lifecycle_lock_path(root);
    let opened = open_persistent_daemon_lock(root, &path)?;
    FileExt::lock_exclusive(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire task-store recovery lease", &path, source)
    })?;
    validated_lease(root, opened, LeaseRole::Recovery)
}

#[cfg(unix)]
pub(crate) fn acquire_task_store_recovery_lease_from(
    daemon_instance: &TaskStoreLease,
) -> Result<TaskStoreLease> {
    require_lease_role(daemon_instance, LeaseRole::DaemonInstance)?;
    let path = task_store_lifecycle_lock_path(daemon_instance.workspace_root());
    let opened = open_persistent_daemon_lock_from_lease(daemon_instance, &path)?;
    FileExt::lock_exclusive(&opened.file).map_err(|source| {
        DaemonCoreError::io("failed to acquire task-store recovery lease", &path, source)
    })?;
    validated_lease(
        daemon_instance.workspace_root(),
        opened,
        LeaseRole::Recovery,
    )
}

#[cfg(unix)]
pub(crate) fn acquire_daemon_task_store_lease_from(
    daemon_instance: &TaskStoreLease,
) -> Result<TaskStoreLease> {
    require_lease_role(daemon_instance, LeaseRole::DaemonInstance)?;
    let path = task_store_lifecycle_lock_path(daemon_instance.workspace_root());
    let opened = open_persistent_daemon_lock_from_lease(daemon_instance, &path)?;
    FileExt::lock_shared(&opened.file).map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire daemon task-store lifecycle lease",
            &path,
            source,
        )
    })?;
    validated_lease(
        daemon_instance.workspace_root(),
        opened,
        LeaseRole::DaemonLifecycle,
    )
}

/// Attempts to acquire the exclusive lease for destructive task maintenance.
///
/// Returns `Ok(None)` without waiting when a daemon already owns the shared
/// lease. The returned lease must remain alive for the complete maintenance
/// window.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the daemon directory or lifecycle lock
/// cannot be safely created, opened, validated, or locked.
pub fn try_acquire_task_store_retention_lease(root: &Path) -> Result<Option<TaskStoreLease>> {
    let path = task_store_lifecycle_lock_path(root);
    let opened = open_persistent_daemon_lock_for_retention(root, &path)?;
    match FileExt::try_lock_exclusive(&opened.file) {
        Ok(()) => validated_lease(root, opened, LeaseRole::Retention).map(Some),
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(DaemonCoreError::io(
            "failed to acquire task-retention lease",
            &path,
            source,
        )),
    }
}

#[cfg(unix)]
pub(crate) fn try_acquire_task_retention_instance_gate_from(
    retention: &TaskStoreLease,
) -> Result<Option<TaskRetentionAdmission>> {
    require_lease_role(retention, LeaseRole::Retention)?;
    let path = daemon_instance_lock_path(retention.workspace_root());
    let opened = open_persistent_daemon_lock_from_lease(retention, &path)?;
    match FileExt::try_lock_shared(&opened.file) {
        Ok(()) => validated_lease(
            retention.workspace_root(),
            opened,
            LeaseRole::RetentionInstanceGate,
        )
        .map(|gate| Some(TaskRetentionAdmission { gate })),
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(DaemonCoreError::io(
            "failed to acquire task-retention daemon-instance gate",
            &path,
            source,
        )),
    }
}

fn require_lease_role(lease: &TaskStoreLease, expected: LeaseRole) -> Result<()> {
    if lease.role() == expected {
        return Ok(());
    }
    Err(DaemonCoreError::io(
        "task-store lease has the wrong authority role",
        lease.path(),
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expected {expected:?} authority, received {:?}",
                lease.role()
            ),
        ),
    ))
}

impl TaskStoreLease {
    fn new(
        opened: OpenedPersistentDaemonLock,
        role: LeaseRole,
        #[cfg(unix)] lock_identity: FileIdentity,
    ) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                file: opened.file,
                path: opened.path,
                role,
                authority: opened.authority,
                #[cfg(unix)]
                lock_name: opened.lock_name,
                #[cfg(unix)]
                lock_identity,
            }),
        }
    }
}

#[cfg(unix)]
fn validated_lease(
    root: &Path,
    opened: OpenedPersistentDaemonLock,
    role: LeaseRole,
) -> Result<TaskStoreLease> {
    let lock_identity = validate_open_lock_file(root, &opened)?;
    Ok(TaskStoreLease::new(opened, role, lock_identity))
}

#[cfg(not(unix))]
fn validated_lease(
    root: &Path,
    opened: OpenedPersistentDaemonLock,
    role: LeaseRole,
) -> Result<TaskStoreLease> {
    validate_open_lock_file(root, &opened)?;
    Ok(TaskStoreLease::new(opened, role))
}

#[cfg(unix)]
fn open_persistent_daemon_lock(root: &Path, path: &Path) -> Result<OpenedPersistentDaemonLock> {
    open_persistent_daemon_lock_with_preflight(root, path, false)
}

#[cfg(unix)]
fn open_persistent_daemon_lock_for_retention(
    root: &Path,
    path: &Path,
) -> Result<OpenedPersistentDaemonLock> {
    open_persistent_daemon_lock_with_preflight(root, path, true)
}

#[cfg(unix)]
fn open_persistent_daemon_lock_with_preflight(
    root: &Path,
    path: &Path,
    preflight_existing_managed_roots: bool,
) -> Result<OpenedPersistentDaemonLock> {
    let workspace = CapabilityDir::open_workspace(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for task-store lease",
            root,
            source,
        )
    })?;
    let canonical_root = workspace.display_path().to_path_buf();
    let name_max = workspace.name_max().map_err(|source| {
        DaemonCoreError::io(
            "failed to read workspace filesystem component limit",
            &canonical_root,
            source,
        )
    })?;
    if name_max < MAX_TASK_STORE_COMPONENT_BYTES {
        return Err(DaemonCoreError::io(
            "unsupported workspace filesystem component limit",
            &canonical_root,
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "filesystem supports {name_max}-byte names; Packet28 task storage requires \
                     {MAX_TASK_STORE_COMPONENT_BYTES}"
                ),
            ),
        ));
    }
    if preflight_existing_managed_roots {
        preflight_retention_managed_roots(&workspace)?;
    }
    let state = workspace
        .ensure_dir_open(std::ffi::OsStr::new(".packet28"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state capability for task-store lease",
                canonical_root.join(".packet28"),
                source,
            )
        })?;
    if workspace.identity().device != state.identity().device {
        return Err(DaemonCoreError::io(
            "refused Packet28 state on another filesystem",
            canonical_root.join(".packet28"),
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Packet28 state is not on the workspace filesystem",
            ),
        ));
    }
    let daemon = state
        .ensure_dir_open(std::ffi::OsStr::new("daemon"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open daemon capability for task-store lease",
                canonical_root.join(".packet28").join("daemon"),
                source,
            )
        })?;
    if state.identity().device != daemon.identity().device {
        return Err(DaemonCoreError::io(
            "refused task-store lock directory on another filesystem",
            canonical_root.join(".packet28").join("daemon"),
            io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon lock directory is not on the Packet28 state filesystem",
            ),
        ));
    }
    let lock_name = path.file_name().ok_or_else(|| {
        DaemonCoreError::io(
            "failed to resolve task-store lock name",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "lock path has no file name"),
        )
    })?;
    let file = daemon.open_lock_file(lock_name).map_err(|source| {
        DaemonCoreError::io("failed to open persistent daemon lock", path, source)
    })?;
    let authority = Arc::new(TaskStoreAuthority {
        requested_root: root.to_path_buf(),
        workspace_root: canonical_root,
        workspace,
        state,
        daemon,
    });
    Ok(OpenedPersistentDaemonLock {
        file,
        path: authority.daemon.display_path().join(lock_name),
        authority,
        lock_name: lock_name.to_os_string(),
    })
}

#[cfg(unix)]
fn preflight_retention_managed_roots(workspace: &CapabilityDir) -> Result<()> {
    let state = match workspace.open_dir(std::ffi::OsStr::new(".packet28")) {
        Ok(state) => state,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open existing Packet28 state before retention admission",
                workspace.display_path().join(".packet28"),
                source,
            ));
        }
    };
    ensure_preflight_same_device(
        workspace.identity(),
        &state,
        "Packet28 state for retention is on another filesystem",
    )?;

    let daemon = preflight_optional_managed_root(
        &state,
        std::ffi::OsStr::new("daemon"),
        "daemon state for retention is on another filesystem",
    )?;
    preflight_optional_managed_root(
        &state,
        std::ffi::OsStr::new("task"),
        "task artifact root for retention is on another filesystem",
    )?;
    preflight_optional_managed_root(
        &state,
        std::ffi::OsStr::new("agent"),
        "agent runtime root for retention is on another filesystem",
    )?;
    preflight_optional_managed_root(
        &state,
        std::ffi::OsStr::new(".retention-trash"),
        "retention quarantine is on another filesystem",
    )?;
    if let Some(daemon) = daemon.as_ref() {
        preflight_optional_managed_root(
            daemon,
            std::ffi::OsStr::new("tasks"),
            "task event root for retention is on another filesystem",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn preflight_optional_managed_root(
    parent: &CapabilityDir,
    name: &std::ffi::OsStr,
    operation: &'static str,
) -> Result<Option<CapabilityDir>> {
    let directory = match parent.open_dir(name) {
        Ok(directory) => directory,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open existing managed root before retention admission",
                parent.display_path().join(name),
                source,
            ));
        }
    };
    ensure_preflight_same_device(parent.identity(), &directory, operation)?;
    Ok(Some(directory))
}

#[cfg(unix)]
fn ensure_preflight_same_device(
    parent: crate::retention::FileIdentity,
    child: &CapabilityDir,
    operation: &'static str,
) -> Result<()> {
    let child_identity = preflight_identity(child.display_path(), child.identity());
    if parent.device == child_identity.device {
        return Ok(());
    }
    Err(DaemonCoreError::io(
        operation,
        child.display_path(),
        io::Error::new(
            io::ErrorKind::InvalidData,
            "managed root crosses the retained parent filesystem",
        ),
    ))
}

#[cfg(all(unix, not(test)))]
fn preflight_identity(
    _path: &Path,
    identity: crate::retention::FileIdentity,
) -> crate::retention::FileIdentity {
    identity
}

#[cfg(all(unix, test))]
fn preflight_identity(
    path: &Path,
    identity: crate::retention::FileIdentity,
) -> crate::retention::FileIdentity {
    let inject = INJECT_FOREIGN_DEVICE_FOR_PREFLIGHT.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() == Some(path) {
            configured.take();
            true
        } else {
            false
        }
    });
    if inject {
        crate::retention::FileIdentity {
            device: identity.device.wrapping_add(1),
            inode: identity.inode,
        }
    } else {
        identity
    }
}

#[cfg(unix)]
fn open_persistent_daemon_lock_from_lease(
    lease: &TaskStoreLease,
    path: &Path,
) -> Result<OpenedPersistentDaemonLock> {
    let lock_name = path.file_name().ok_or_else(|| {
        DaemonCoreError::io(
            "failed to resolve task-store lock name",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "lock path has no file name"),
        )
    })?;
    let authority = Arc::clone(&lease.inner.authority);
    let file = authority
        .daemon
        .open_lock_file(lock_name)
        .map_err(|source| {
            DaemonCoreError::io("failed to open persistent daemon lock", path, source)
        })?;
    Ok(OpenedPersistentDaemonLock {
        file,
        path: authority.daemon.display_path().join(lock_name),
        authority,
        lock_name: lock_name.to_os_string(),
    })
}

#[cfg(not(unix))]
fn open_persistent_daemon_lock(_root: &Path, path: &Path) -> Result<OpenedPersistentDaemonLock> {
    Err(DaemonCoreError::io(
        "handle-safe task-store leases are unsupported on this platform",
        path,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Packet28 requires retained no-follow directory handles for task-store mutation",
        ),
    ))
}

#[cfg(unix)]
fn validate_open_lock_file(
    _root: &Path,
    opened: &OpenedPersistentDaemonLock,
) -> Result<FileIdentity> {
    let identity = open_lock_identity(&opened.file, &opened.path)?;
    validate_lock_attachment(
        &opened.file,
        &opened.path,
        &opened.authority.daemon,
        &opened.lock_name,
        identity,
    )?;
    Ok(identity)
}

#[cfg(unix)]
fn open_lock_identity(file: &File, path: &Path) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let file_metadata = file.metadata().map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect open persistent task-store lock",
            path,
            source,
        )
    })?;
    Ok(FileIdentity {
        device: file_metadata.dev(),
        inode: file_metadata.ino(),
    })
}

#[cfg(unix)]
fn validate_lock_attachment(
    file: &File,
    path: &Path,
    daemon: &CapabilityDir,
    lock_name: &std::ffi::OsStr,
    expected_identity: FileIdentity,
) -> Result<()> {
    let actual_identity = open_lock_identity(file, path)?;
    if actual_identity != expected_identity {
        return Err(DaemonCoreError::io(
            "persistent lock changed after lease acquisition",
            path,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "retained lock descriptor identity changed",
            ),
        ));
    }
    daemon
        .authenticate_regular_file_with_link_count(lock_name, expected_identity, 1)
        .map_err(|source| {
            DaemonCoreError::io(
                "persistent lock changed after lease acquisition",
                path,
                source,
            )
        })
}

#[cfg(not(unix))]
fn validate_open_lock_file(_root: &Path, _opened: &OpenedPersistentDaemonLock) -> Result<()> {
    Err(DaemonCoreError::io(
        "handle-safe task-store lease validation is unsupported on this platform",
        Path::new("<unsupported-task-store-lease>"),
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Packet28 requires retained no-follow directory handles for task-store mutation",
        ),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    fn replace_persistent_lock(path: &Path) {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt as _;

        let detached = path.with_extension("detached");
        fs::rename(path, detached).unwrap();
        let replacement = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        replacement.sync_all().unwrap();
    }

    #[test]
    fn daemon_first_prevents_retention_lease_without_waiting() {
        let root = tempdir().unwrap();
        let daemon_lease = acquire_daemon_task_store_lease(root.path()).unwrap();

        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());

        drop(daemon_lease);
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn writer_first_prevents_retention_lease_without_waiting() {
        let root = tempdir().unwrap();
        let writer_lease = acquire_task_store_writer_lease(root.path()).unwrap();

        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());

        drop(writer_lease);
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn writer_first_blocks_recovery_until_release() {
        let root = tempdir().unwrap();
        let writer_lease = acquire_task_store_writer_lease(root.path()).unwrap();
        let root_path = root.path().to_path_buf();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let recovery = thread::spawn(move || {
            let lease = acquire_task_store_recovery_lease(&root_path).unwrap();
            acquired_tx.send(()).unwrap();
            lease
        });

        assert!(matches!(
            acquired_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(writer_lease);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(recovery.join().unwrap());
    }

    #[test]
    fn cloned_writer_lease_unlocks_only_after_final_drop() {
        let root = tempdir().unwrap();
        let first = acquire_task_store_writer_lease(root.path()).unwrap();
        let final_clone = first.clone();
        drop(first);

        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());

        drop(final_clone);
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn only_one_daemon_instance_can_own_a_workspace() {
        let root = tempdir().unwrap();
        let first = acquire_daemon_instance_lease(root.path()).unwrap();

        let error = acquire_daemon_instance_lease(root.path()).unwrap_err();
        assert!(matches!(
            error,
            DaemonCoreError::DaemonInstanceAlreadyRunning { path }
                if path == daemon_instance_lock_path(root.path())
        ));

        drop(first);
        assert!(acquire_daemon_instance_lease(root.path()).is_ok());
    }

    #[test]
    fn daemon_startup_lease_serializes_concurrent_clients() {
        let root = tempdir().unwrap();
        let first = acquire_daemon_startup_lease(root.path()).unwrap();
        let root_path = root.path().to_path_buf();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            let lease = acquire_daemon_startup_lease(&root_path).unwrap();
            acquired_tx.send(()).unwrap();
            lease
        });

        assert!(matches!(
            acquired_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(second.join().unwrap());
    }

    #[test]
    fn daemon_instance_released_probe_reports_ownership_without_retaining_it() {
        let root = tempdir().unwrap();
        assert!(daemon_instance_released(root.path()).unwrap());
        assert!(!daemon_instance_lock_path(root.path()).exists());

        let daemon = acquire_daemon_instance_lease(root.path()).unwrap();
        assert!(!daemon_instance_released(root.path()).unwrap());
        drop(daemon);

        assert!(daemon_instance_released(root.path()).unwrap());
        // The probe released the lease it took.
        drop(acquire_daemon_instance_lease(root.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_instance_released_probe_fails_closed_on_a_substituted_lock() {
        let root = tempdir().unwrap();
        drop(acquire_daemon_instance_lease(root.path()).unwrap());
        let lock = daemon_instance_lock_path(root.path());
        fs::remove_file(&lock).unwrap();
        fs::write(root.path().join("lock-target"), b"").unwrap();
        std::os::unix::fs::symlink(root.path().join("lock-target"), &lock).unwrap();

        let error = daemon_instance_released(root.path()).unwrap_err();
        assert!(!matches!(
            error,
            DaemonCoreError::DaemonInstanceAlreadyRunning { .. }
        ));
    }

    #[test]
    fn daemon_startup_and_instance_leases_are_independent() {
        let root = tempdir().unwrap();
        let _startup = acquire_daemon_startup_lease(root.path()).unwrap();
        let _instance = acquire_daemon_instance_lease(root.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_name_max_rejects_before_state_or_lock_mutation() {
        let root = tempdir().unwrap();
        crate::capability::inject_name_max_once(MAX_TASK_STORE_COMPONENT_BYTES - 1);

        let error = acquire_daemon_instance_lease(root.path()).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert!(!root.path().join(".packet28").exists());
        assert!(!daemon_instance_lock_path(root.path()).exists());
        assert!(!task_store_lifecycle_lock_path(root.path()).exists());
    }

    #[test]
    fn retention_instance_gate_excludes_daemon_startup_without_waiting() {
        let root = tempdir().unwrap();
        let daemon = acquire_daemon_instance_lease(root.path()).unwrap();
        let retention = try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .unwrap();
        assert!(try_acquire_task_retention_instance_gate_from(&retention)
            .unwrap()
            .is_none());
        drop(daemon);

        let gate = try_acquire_task_retention_instance_gate_from(&retention)
            .unwrap()
            .unwrap();
        let other_root = tempdir().unwrap();
        let other_retention = try_acquire_task_store_retention_lease(other_root.path())
            .unwrap()
            .unwrap();
        assert!(gate.authorizes(&retention));
        assert!(!gate.authorizes(&gate.gate));
        assert!(!gate.authorizes(&other_retention));
        assert!(matches!(
            acquire_daemon_instance_lease(root.path()),
            Err(DaemonCoreError::DaemonInstanceAlreadyRunning { .. })
        ));
        drop(other_retention);
        drop(gate);
        assert!(acquire_daemon_instance_lease(root.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_lease_derivation_reuses_one_workspace_authority() {
        let root = tempdir().unwrap();
        crate::capability::reset_open_workspace_call_count();

        let instance = acquire_daemon_instance_lease(root.path()).unwrap();
        let recovery = acquire_task_store_recovery_lease_from(&instance).unwrap();
        drop(recovery);
        let lifecycle = acquire_daemon_task_store_lease_from(&instance).unwrap();

        assert_eq!(crate::capability::open_workspace_call_count(), 1);
        drop(lifecycle);
        drop(instance);
    }

    #[test]
    fn instance_and_task_store_leases_are_independent() {
        let root = tempdir().unwrap();
        let _instance = acquire_daemon_instance_lease(root.path()).unwrap();
        let lifecycle = acquire_daemon_task_store_lease(root.path()).unwrap();

        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_none());
        assert!(matches!(
            acquire_daemon_instance_lease(root.path()),
            Err(DaemonCoreError::DaemonInstanceAlreadyRunning { .. })
        ));

        drop(lifecycle);
        assert!(try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn retention_first_blocks_daemon_start_until_release() {
        let root = tempdir().unwrap();
        let retention_lease = try_acquire_task_store_retention_lease(root.path())
            .unwrap()
            .unwrap();
        let root_path = root.path().to_path_buf();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let daemon = thread::spawn(move || {
            let lease = acquire_daemon_task_store_lease(&root_path).unwrap();
            acquired_tx.send(()).unwrap();
            lease
        });

        assert!(matches!(
            acquired_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(retention_lease);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(daemon.join().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_lease_acquisition_rejects_lock_replacement_after_flock() {
        let root = tempdir().unwrap();
        let path = task_store_lifecycle_lock_path(root.path());
        let opened = open_persistent_daemon_lock(root.path(), &path).unwrap();
        FileExt::lock_shared(&opened.file).unwrap();
        replace_persistent_lock(&path);

        let error = validated_lease(root.path(), opened, LeaseRole::Writer).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_instance_lease_rejects_lock_replacement_after_operation_barrier() {
        let root = tempdir().unwrap();
        let lease = acquire_daemon_instance_lease(root.path()).unwrap();
        lease.validate_namespace_attachment().unwrap();
        let lock_path = lease.path().to_path_buf();
        let (replace_tx, replace_rx) = mpsc::channel();
        let (replaced_tx, replaced_rx) = mpsc::channel();
        let replacer = thread::spawn(move || {
            replace_rx.recv().unwrap();
            replace_persistent_lock(&lock_path);
            replaced_tx.send(()).unwrap();
        });

        replace_tx.send(()).unwrap();
        replaced_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        replacer.join().unwrap();
        let error = lease.validate_namespace_attachment().unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lifecycle_lock_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("keep");
        fs::write(&target, b"keep").unwrap();
        let daemon_root = daemon_dir(root.path());
        fs::create_dir_all(&daemon_root).unwrap();
        symlink(&target, task_store_lifecycle_lock_path(root.path())).unwrap();

        assert!(acquire_daemon_task_store_lease(root.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_instance_lock_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("keep");
        fs::write(&target, b"keep").unwrap();
        let daemon_root = daemon_dir(root.path());
        fs::create_dir_all(&daemon_root).unwrap();
        symlink(&target, daemon_instance_lock_path(root.path())).unwrap();

        assert!(acquire_daemon_instance_lease(root.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }
}
