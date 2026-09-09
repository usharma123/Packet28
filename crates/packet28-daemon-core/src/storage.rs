//! Durable daemon runtime metadata, registries, and append-only task events.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
#[cfg(any(not(unix), test))]
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use packet28_daemon_protocol::hooks::ActiveTaskRecord;
use packet28_daemon_protocol::message::{DaemonEvent, DaemonEventFrame, DaemonRuntimeInfo};
#[cfg(any(not(unix), test))]
use packet28_daemon_protocol::paths::task_artifact_dir;
use packet28_daemon_protocol::paths::{
    active_task_path, agent_runtime_dir, daemon_dir, pid_path, ready_path, runtime_path,
    socket_path, task_artifacts_dir, task_event_log_path, task_events_dir, task_registry_path,
    watch_registry_path, workspace_socket_path, TaskStorageId, AGENT_ACTIVE_TASK_FILE_NAME,
    MAX_TASK_STORAGE_ID_BYTES, PID_FILE_NAME, RUNTIME_FILE_NAME, TASK_ARTIFACTS_DIR_NAME,
    TASK_EVENTS_DIR_NAME, TASK_EVENT_LOG_SUFFIX, TASK_REGISTRY_FILE_NAME, WATCH_REGISTRY_FILE_NAME,
};
use packet28_daemon_protocol::task::{TaskRegistry, WatchRegistry};
use unicode_casefold::UnicodeCaseFold as _;
use unicode_normalization::UnicodeNormalization as _;

#[cfg(all(unix, test))]
use crate::capability::{generated_name_matches, inject_authenticated_read_after_open_once};
#[cfg(unix)]
use crate::capability::{
    sync_file_barrier, CapabilityDir, ACTIVE_TASK_WRITE_TEMP_PREFIX,
    TASK_REGISTRY_WRITE_TEMP_PREFIX,
};
use crate::task_store_lease::{acquire_registry_writer_admission, acquire_task_store_writer_lease};
use crate::{DaemonCoreError, Result};

mod checkpoint;
mod event_tail;
mod registry_delta;

pub use event_tail::{
    append_next_task_event, append_next_task_event_with_authority,
    load_task_registry_with_event_tails, load_task_watch_registry_checkpoint_with_event_tails,
    task_event_log_tail_sequence, MAX_TASK_EVENT_TAIL_SCAN_BYTES,
};
#[cfg(all(test, unix))]
use event_tail::{
    append_next_task_event_admitted_with_observers,
    task_event_log_tail_sequence_admitted_with_observer,
};
#[cfg(all(unix, test))]
pub(crate) use registry_delta::REGISTRY_DELTA_WAL_HEADER_BYTES;
pub use registry_delta::{
    append_task_watch_registry_delta, append_task_watch_registry_delta_with_authority,
    load_registry_admission_authority, load_task_watch_registry_recovering_corrupt_event_logs,
    load_task_watch_registry_with_deltas, load_task_watch_registry_with_deltas_and_event_tails,
    registry_delta_wal_path, save_task_watch_registry_checkpoint_at_revision,
    save_task_watch_registry_checkpoint_at_revision_with_authority, LoadedTaskWatchRegistry,
    QuarantinedCorruptTaskEventLog, RegistryAdmissionAuthority, RegistryDeltaBatch,
    RegistryDeltaValidationError, RegistryRevision, RegistryRevisionRange,
    MAX_REGISTRY_DELTA_FRAME_BYTES, MAX_REGISTRY_DELTA_WAL_BYTES,
};
#[cfg(unix)]
pub(crate) use registry_delta::{
    load_retained_registry_snapshot_under_task_lock,
    remove_retained_registry_records_under_task_lock, REGISTRY_DELTA_WAL_FILE_NAME,
};

#[cfg(any(not(unix), test))]
static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Maximum supported encoded size of the task registry.
///
/// Writers validate this bound before acquiring a task-store lease or
/// mutating state. Readers use the same constant for their bounded read.
pub const MAX_TASK_REGISTRY_BYTES: usize = 64 * 1024 * 1024;
/// Maximum supported encoded size of the watch registry.
///
/// Writers validate this bound before acquiring a task-store lease or
/// mutating state. Readers use the same constant for their bounded read.
pub const MAX_WATCH_REGISTRY_BYTES: usize = 64 * 1024 * 1024;
/// Maximum supported encoded size of the active-task record.
///
/// All task-store readers and writers must use this shared contract so
/// retention cannot observe a record that a producer was allowed to grow
/// beyond its bounded read.
pub const MAX_ACTIVE_TASK_RECORD_BYTES: usize = 1024 * 1024;
/// Largest path-component size supported by Packet28 task storage.
///
/// Supported Linux and Apple filesystems expose a 255-byte `NAME_MAX`.
pub const MAX_TASK_STORE_COMPONENT_BYTES: usize = 255;
/// Largest derived task storage key accepted by public writers.
///
/// The event-log suffix is reserved so both the artifact directory and
/// `{storage_key}.events.jsonl` remain valid single path components.
pub const MAX_TASK_STORAGE_KEY_BYTES: usize = MAX_TASK_STORAGE_ID_BYTES;
/// Maximum nesting depth accepted before authority JSON is materialized.
pub const MAX_AUTHORITY_JSON_DEPTH: usize = 64;
/// Maximum JSON value nodes accepted before authority JSON is materialized.
pub const MAX_AUTHORITY_JSON_VALUE_NODES: usize = 262_144;
/// Maximum aggregate array items and object members in authority JSON.
pub const MAX_AUTHORITY_JSON_CONTAINER_ENTRIES: usize = 262_144;
/// Maximum entries in any one authority JSON object or array.
pub const MAX_AUTHORITY_JSON_ENTRIES_PER_CONTAINER: usize = 65_536;
/// Maximum aggregate value and object-key tokens in authority JSON.
pub const MAX_AUTHORITY_JSON_TOKENS: usize = 524_288;
/// Task-registry value-node budget for the declared multi-thousand-record
/// registry surface.
///
/// Task records contain substantially more scalar fields than the other
/// authority documents. This remains bounded well below the 64 MiB byte limit
/// while allowing at least 5,000 fully materialized records.
pub const MAX_TASK_REGISTRY_AUTHORITY_JSON_VALUE_NODES: usize = MAX_AUTHORITY_JSON_VALUE_NODES * 2;
/// Task-registry aggregate container-entry budget.
pub const MAX_TASK_REGISTRY_AUTHORITY_JSON_CONTAINER_ENTRIES: usize =
    MAX_AUTHORITY_JSON_CONTAINER_ENTRIES * 2;
/// Task-registry aggregate key/value token budget.
pub const MAX_TASK_REGISTRY_AUTHORITY_JSON_TOKENS: usize = MAX_AUTHORITY_JSON_TOKENS * 2;
/// Maximum task records accepted in one persisted task registry.
pub const MAX_TASK_REGISTRY_RECORDS: usize = 65_536;
/// Maximum watch records accepted in one persisted watch registry.
pub const MAX_WATCH_REGISTRY_RECORDS: usize = 65_536;
/// Maximum event-frame bytes returned by one paginated task-event read.
///
/// A resumed read may additionally inspect one bounded predecessor frame to
/// verify sequence continuity at the supplied cursor.
pub const MAX_TASK_EVENT_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum decoded frames returned by one paginated task-event read.
pub const MAX_TASK_EVENT_PAGE_FRAMES: usize = 4_096;
/// Maximum supported encoded size of one complete task-event JSON line.
pub const MAX_TASK_EVENT_LINE_BYTES: usize = 1024 * 1024;
/// Maximum bytes accumulated by the compatibility whole-log reader.
pub const MAX_TASK_EVENT_LOAD_BYTES: usize = 64 * 1024 * 1024;
/// Maximum frames accumulated by the compatibility whole-log reader.
pub const MAX_TASK_EVENT_LOAD_FRAMES: usize = MAX_TASK_REGISTRY_RECORDS;
#[cfg(unix)]
pub(crate) const TASK_REGISTRY_LOCK_FILE_NAME: &str = ".task-registry-v1.json.lock";
#[cfg(unix)]
const WATCH_REGISTRY_LOCK_FILE_NAME: &str = ".watch-registry-v1.json.lock";
#[cfg(unix)]
const WATCH_REGISTRY_WRITE_TEMP_PREFIX: &str = ".watch-registry-v1.json.packet28-write.";
#[cfg(unix)]
const RUNTIME_INFO_WRITE_TEMP_PREFIX: &str = ".runtime.json.packet28-write.";
#[cfg(unix)]
const PID_WRITE_TEMP_PREFIX: &str = ".pid.packet28-write.";
const MAX_DAEMON_RUNTIME_INFO_BYTES: usize = 64 * 1024;
pub(crate) const ACTIVE_TASK_LOCK_FILE_NAME: &str = ".active-task.json.lock";
const REGISTRY_CHECKPOINT_GENERATION_FIELD: &str = "task_watch_checkpoint_generation";

#[cfg(test)]
std::thread_local! {
    static INJECT_PARENT_SYNC_FAILURE_FOR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static INJECT_TASK_EVENT_SYNC_FAILURE_FOR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Events read from one byte offset in an append-only task event log.
#[derive(Debug, Clone)]
pub struct TaskEventLogRead {
    /// Complete JSON-line event frames decoded from the requested offset.
    pub events: Vec<DaemonEventFrame>,
    /// Byte offset immediately after the final complete line that was read.
    pub next_offset: u64,
}

/// Creates the daemon state directory for `root`.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the directory cannot be created.
pub fn ensure_daemon_dir(root: &Path) -> Result<PathBuf> {
    let dir = daemon_dir(root);
    fs::create_dir_all(&dir)
        .map_err(|source| DaemonCoreError::io("failed to create daemon directory", &dir, source))?;
    #[cfg(not(unix))]
    ensure_daemon_socket_dir(root)?;
    Ok(dir)
}

/// Creates and authenticates the preferred daemon socket directory for `root`.
///
/// Unix endpoints use a private effective-user-specific directory with exact
/// `0700` permissions. Existing directories are accepted only when their
/// ownership, mode, ACL, file type, and namespace ancestry remain safe.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] when the directory cannot be created or its
/// authority cannot be authenticated.
pub fn ensure_daemon_socket_dir(root: &Path) -> Result<PathBuf> {
    let socket_dir = socket_path(root)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(unix)]
    ensure_private_socket_directory(&socket_dir)?;
    #[cfg(not(unix))]
    fs::create_dir_all(&socket_dir).map_err(|source| {
        DaemonCoreError::io(
            "failed to create daemon socket directory",
            &socket_dir,
            source,
        )
    })?;
    Ok(socket_dir)
}

#[cfg(unix)]
fn ensure_private_socket_directory(path: &Path) -> Result<()> {
    let created = match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to create private daemon socket directory",
                path,
                source,
            ));
        }
    };
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            DaemonCoreError::io(
                "failed to set private daemon socket directory permissions",
                path,
                source,
            )
        })?;
    }
    validate_socket_namespace_aliases(path).map_err(|source| {
        DaemonCoreError::io(
            "failed to authenticate daemon socket namespace ancestry",
            path,
            source,
        )
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve daemon socket namespace ancestry",
            path,
            source,
        )
    })?;
    validate_socket_namespace_aliases(&canonical_path).map_err(|source| {
        DaemonCoreError::io(
            "failed to authenticate resolved daemon socket namespace ancestry",
            &canonical_path,
            source,
        )
    })?;
    CapabilityDir::open_private(path, 0o700)
        .map(|_| ())
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to authenticate private daemon socket directory",
                path,
                source,
            )
        })
}

#[cfg(unix)]
fn validate_socket_namespace_aliases(path: &Path) -> std::io::Result<()> {
    let effective_uid = rustix::process::geteuid().as_raw();
    let mut child_uid = fs::symlink_metadata(path)?.uid();
    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor)?;
        let file_type = metadata.file_type();
        if !file_type.is_dir() && !file_type.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "daemon socket namespace ancestor is neither a directory nor a symlink: {}",
                    ancestor.display()
                ),
            ));
        }
        let owner_uid = metadata.uid();
        if owner_uid != effective_uid && owner_uid != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "daemon socket namespace ancestor is owned by uid {owner_uid}; \
                     expected uid {effective_uid} or root: {}",
                    ancestor.display()
                ),
            ));
        }
        if file_type.is_dir() {
            let mode = metadata.mode();
            let non_owner_writable = (mode & 0o022) != 0;
            let sticky = (mode & 0o1000) != 0;
            if non_owner_writable && !(sticky && child_uid == effective_uid) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "daemon socket namespace ancestor permits replacement without safe \
                         sticky ownership semantics: {}",
                        ancestor.display()
                    ),
                ));
            }
        }
        child_uid = owner_uid;
    }
    Ok(())
}

/// Loads the workspace active-task record through the shared bounded contract.
///
/// Returns `Ok(None)` only when no record exists. Unsafe file types, malformed
/// JSON, empty task identifiers, and read failures remain explicit errors.
///
/// # Errors
///
/// Returns [`DaemonCoreError::ActiveTaskRecordTooLarge`] when the record
/// exceeds [`MAX_ACTIVE_TASK_RECORD_BYTES`],
/// [`DaemonCoreError::InvalidActiveTaskRecord`] when its exact task identifier
/// is empty or whitespace-only, [`DaemonCoreError::Json`] for malformed JSON,
/// or [`DaemonCoreError::Io`] for filesystem and file-type failures.
pub fn load_active_task_record(root: &Path) -> Result<Option<ActiveTaskRecord>> {
    let path = active_task_path(root);
    #[cfg(unix)]
    {
        load_active_task_record_anchored(root, &path)
    }
    #[cfg(not(unix))]
    {
        load_active_task_record_portable(&path)
    }
}

/// Persists the workspace active-task record through the shared bounded
/// contract.
///
/// Encoding and validation complete before a task-store lease is acquired or
/// filesystem state is changed. On Unix, the final replacement is relative to
/// retained no-follow directory capabilities and serialized by a dedicated
/// active-task lock.
///
/// # Errors
///
/// Returns [`DaemonCoreError::ActiveTaskRecordTooLarge`] when the encoded
/// record exceeds [`MAX_ACTIVE_TASK_RECORD_BYTES`],
/// [`DaemonCoreError::InvalidActiveTaskRecord`] for an empty or
/// whitespace-only task identifier, [`DaemonCoreError::Json`] for encoding
/// failures, or [`DaemonCoreError::Io`] for lease, lock, and filesystem
/// failures.
pub fn save_active_task_record(root: &Path, record: &ActiveTaskRecord) -> Result<()> {
    let path = active_task_path(root);
    let bytes = encode_active_task_record(&path, record)?;
    let _writer_lease = acquire_task_store_writer_lease(root)?;
    #[cfg(unix)]
    {
        save_active_task_record_anchored(root, &path, &bytes)
    }
    #[cfg(not(unix))]
    {
        save_active_task_record_portable(&path, &bytes)
    }
}

fn encode_active_task_record(path: &Path, record: &ActiveTaskRecord) -> Result<Vec<u8>> {
    validate_active_task_record(path, record)?;
    let bytes = serde_json::to_vec_pretty(record).map_err(|source| {
        DaemonCoreError::json("failed to encode active-task record for", path, source)
    })?;
    validate_active_task_record_size(path, bytes.len() as u64)?;
    validate_authority_json(&bytes, AuthorityJsonProfile::ActiveTask).map_err(|error| {
        map_authority_json_error(
            path,
            AuthorityJsonProfile::ActiveTask,
            "failed to validate encoded active-task record for",
            error,
        )
    })?;
    Ok(bytes)
}

pub(crate) fn decode_active_task_record(path: &Path, raw: &[u8]) -> Result<ActiveTaskRecord> {
    validate_active_task_record_size(path, raw.len() as u64)?;
    let value = decode_json_value_without_duplicate_keys(raw, AuthorityJsonProfile::ActiveTask)
        .map_err(|error| {
            map_authority_json_error(
                path,
                AuthorityJsonProfile::ActiveTask,
                "failed to decode active-task record from",
                error,
            )
        })?;
    if !value
        .as_object()
        .and_then(|object| object.get("task_id"))
        .is_some_and(serde_json::Value::is_string)
    {
        return Err(DaemonCoreError::InvalidActiveTaskRecord {
            path: path.to_path_buf(),
            message: "persisted record must contain a string-valued task_id field".to_string(),
        });
    }
    let record = serde_json::from_value(value).map_err(|source| {
        DaemonCoreError::json("failed to decode active-task record from", path, source)
    })?;
    validate_active_task_record(path, &record)?;
    Ok(record)
}

fn validate_active_task_record(path: &Path, record: &ActiveTaskRecord) -> Result<()> {
    if let Some(message) = task_identifier_shape_error(&record.task_id) {
        return Err(DaemonCoreError::InvalidActiveTaskRecord {
            path: path.to_path_buf(),
            message,
        });
    }
    Ok(())
}

fn validate_active_task_record_size(path: &Path, encoded_bytes: u64) -> Result<()> {
    if encoded_bytes > MAX_ACTIVE_TASK_RECORD_BYTES as u64 {
        return Err(DaemonCoreError::ActiveTaskRecordTooLarge {
            path: path.to_path_buf(),
            encoded_bytes,
            max_bytes: MAX_ACTIVE_TASK_RECORD_BYTES as u64,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn load_active_task_record_anchored(root: &Path, path: &Path) -> Result<Option<ActiveTaskRecord>> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for active-task read",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for active-task read",
            &canonical_root,
            source,
        )
    })?;
    let state = match workspace.open_dir(OsStr::new(".packet28")) {
        Ok(state) => state,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open Packet28 state for active-task read",
                canonical_root.join(".packet28"),
                source,
            ));
        }
    };
    ensure_capability_same_device(
        &workspace,
        &state,
        canonical_root.join(".packet28"),
        "Packet28 state for active-task read is on another filesystem",
    )?;
    let agent = match state.open_dir(OsStr::new("agent")) {
        Ok(agent) => agent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open agent state for active-task read",
                agent_runtime_dir(&canonical_root),
                source,
            ));
        }
    };
    ensure_capability_same_device(
        &state,
        &agent,
        agent_runtime_dir(&canonical_root),
        "agent state for active-task read is on another filesystem",
    )?;
    let raw = match agent.read_file_limited(
        OsStr::new(AGENT_ACTIVE_TASK_FILE_NAME),
        MAX_ACTIVE_TASK_RECORD_BYTES,
    ) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            if source.kind() == std::io::ErrorKind::InvalidData
                && matches!(
                    agent.entry_is_regular_file(OsStr::new(AGENT_ACTIVE_TASK_FILE_NAME)),
                    Ok(Some(true))
                )
            {
                if let Ok(Some((encoded_bytes, _))) =
                    agent.entry_storage_bytes(OsStr::new(AGENT_ACTIVE_TASK_FILE_NAME))
                {
                    if encoded_bytes > MAX_ACTIVE_TASK_RECORD_BYTES as u64 {
                        return Err(DaemonCoreError::ActiveTaskRecordTooLarge {
                            path: path.to_path_buf(),
                            encoded_bytes,
                            max_bytes: MAX_ACTIVE_TASK_RECORD_BYTES as u64,
                        });
                    }
                }
            }
            return Err(DaemonCoreError::io(
                "failed to read anchored active-task record",
                path,
                source,
            ));
        }
    };
    decode_active_task_record(path, &raw).map(Some)
}

#[cfg(unix)]
fn save_active_task_record_anchored(root: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for active-task write",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for active-task write",
            &canonical_root,
            source,
        )
    })?;
    let state = workspace
        .ensure_dir_open(OsStr::new(".packet28"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state for active-task write",
                canonical_root.join(".packet28"),
                source,
            )
        })?;
    ensure_capability_same_device(
        &workspace,
        &state,
        canonical_root.join(".packet28"),
        "Packet28 state for active-task write is on another filesystem",
    )?;
    let agent = state
        .ensure_dir_open(OsStr::new("agent"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open agent state for active-task write",
                agent_runtime_dir(&canonical_root),
                source,
            )
        })?;
    ensure_capability_same_device(
        &state,
        &agent,
        agent_runtime_dir(&canonical_root),
        "agent state for active-task write is on another filesystem",
    )?;
    let lock_path = agent.display_path().join(ACTIVE_TASK_LOCK_FILE_NAME);
    let lock = agent
        .open_lock_file(OsStr::new(ACTIVE_TASK_LOCK_FILE_NAME))
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open anchored active-task lock",
                &lock_path,
                source,
            )
        })?;
    FileExt::lock_exclusive(&lock).map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire anchored active-task lock",
            &lock_path,
            source,
        )
    })?;
    let result = agent
        .write_json_atomically(
            OsStr::new(AGENT_ACTIVE_TASK_FILE_NAME),
            bytes,
            ACTIVE_TASK_WRITE_TEMP_PREFIX,
        )
        .map_err(|error| {
            DaemonCoreError::io(
                if error.renamed {
                    "failed to synchronize anchored active-task replacement"
                } else {
                    "failed to write anchored active-task record"
                },
                path,
                error.source,
            )
        });
    let unlock = FileExt::unlock(&lock).map_err(|source| {
        DaemonCoreError::io(
            "failed to unlock anchored active-task record",
            &lock_path,
            source,
        )
    });
    match (result, unlock) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

#[cfg(any(not(unix), test))]
fn load_active_task_record_portable(path: &Path) -> Result<Option<ActiveTaskRecord>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to inspect active-task record",
                path,
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DaemonCoreError::io(
            "refused unsafe active-task record",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "active-task record is not a regular file",
            ),
        ));
    }
    validate_active_task_record_size(path, metadata.len())?;
    let file = fs::File::open(path)
        .map_err(|source| DaemonCoreError::io("failed to open active-task record", path, source))?;
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|source| DaemonCoreError::io("failed to read active-task record", path, source))?;
    decode_active_task_record(path, &raw).map(Some)
}

#[cfg(any(not(unix), test))]
fn save_active_task_record_portable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        DaemonCoreError::io(
            "failed to resolve active-task directory",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "active-task path has no parent directory",
            ),
        )
    })?;
    fs::create_dir_all(parent).map_err(|source| {
        DaemonCoreError::io("failed to create active-task directory", parent, source)
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|source| {
        DaemonCoreError::io("failed to inspect active-task directory", parent, source)
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DaemonCoreError::io(
            "refused unsafe active-task directory",
            parent,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "active-task directory is not a real directory",
            ),
        ));
    }
    let lock_path = parent.join(ACTIVE_TASK_LOCK_FILE_NAME);
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open portable active-task lock",
                &lock_path,
                source,
            )
        })?;
    FileExt::lock_exclusive(&lock).map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire portable active-task lock",
            &lock_path,
            source,
        )
    })?;
    let result = write_atomically(path, bytes);
    let unlock = FileExt::unlock(&lock).map_err(|source| {
        DaemonCoreError::io(
            "failed to unlock portable active-task record",
            &lock_path,
            source,
        )
    });
    match (result, unlock) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

#[cfg(unix)]
fn ensure_capability_same_device(
    parent: &CapabilityDir,
    child: &CapabilityDir,
    path: impl AsRef<Path>,
    message: &'static str,
) -> Result<()> {
    if parent.identity().device == child.identity().device {
        return Ok(());
    }
    Err(DaemonCoreError::io(
        message,
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    ))
}

/// Persists process and runtime discovery metadata for a daemon.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if daemon directories or metadata files
/// cannot be created, written, synchronized, or replaced. Returns
/// [`DaemonCoreError::Json`] if `info` cannot be encoded.
pub fn write_runtime_info(root: &Path, info: &DaemonRuntimeInfo) -> Result<()> {
    ensure_daemon_dir(root)?;
    let path = runtime_path(root);
    let bytes = serde_json::to_vec_pretty(info).map_err(|source| {
        DaemonCoreError::json("failed to encode runtime metadata for", &path, source)
    })?;
    if bytes.len() > MAX_DAEMON_RUNTIME_INFO_BYTES {
        return Err(DaemonCoreError::io(
            "refused oversized daemon runtime metadata",
            &path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runtime metadata is {} bytes; maximum is {MAX_DAEMON_RUNTIME_INFO_BYTES}",
                    bytes.len()
                ),
            ),
        ));
    }
    #[cfg(unix)]
    {
        let daemon = open_daemon_runtime_capability(root, true)?;
        write_anchored_runtime_file(
            &daemon,
            PID_FILE_NAME,
            &pid_path(root),
            format!("{}\n", info.pid).as_bytes(),
            PID_WRITE_TEMP_PREFIX,
            "daemon pid",
        )?;
        write_anchored_runtime_file(
            &daemon,
            RUNTIME_FILE_NAME,
            &path,
            &bytes,
            RUNTIME_INFO_WRITE_TEMP_PREFIX,
            "daemon runtime metadata",
        )
    }
    #[cfg(not(unix))]
    {
        write_atomically(&pid_path(root), format!("{}\n", info.pid).as_bytes())?;
        write_atomically(&path, &bytes)
    }
}

/// Loads persisted runtime discovery metadata for a daemon.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the runtime file cannot be read, or
/// [`DaemonCoreError::Json`] if it is not valid runtime metadata.
pub fn read_runtime_info(root: &Path) -> Result<DaemonRuntimeInfo> {
    let path = runtime_path(root);
    #[cfg(unix)]
    let authenticated_read = {
        let daemon = open_daemon_runtime_capability(root, false)?;
        daemon
            .read_file_limited_with_metadata(
                OsStr::new(RUNTIME_FILE_NAME),
                MAX_DAEMON_RUNTIME_INFO_BYTES,
            )
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to read authenticated runtime metadata",
                    &path,
                    source,
                )
            })?
    };
    #[cfg(unix)]
    let raw = authenticated_read.bytes;
    #[cfg(not(unix))]
    let raw = fs::read(&path)
        .map_err(|source| DaemonCoreError::io("failed to read runtime metadata", &path, source))?;
    let runtime: DaemonRuntimeInfo = serde_json::from_slice(&raw).map_err(|source| {
        DaemonCoreError::json("failed to decode runtime metadata from", &path, source)
    })?;
    #[cfg(unix)]
    if runtime.transport_auth.is_some() && (authenticated_read.mode & 0o077) != 0 {
        return Err(DaemonCoreError::io(
            "refused non-owner-readable daemon transport capability",
            &path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "runtime metadata containing transport authentication has mode {:o}; \
                     group and other permission bits must be zero",
                    authenticated_read.mode
                ),
            ),
        ));
    }
    Ok(runtime)
}

#[cfg(unix)]
fn open_daemon_runtime_capability(root: &Path, create: bool) -> Result<CapabilityDir> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for daemon runtime discovery",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open_workspace(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for daemon runtime discovery",
            &canonical_root,
            source,
        )
    })?;
    let state_path = canonical_root.join(".packet28");
    let state = if create {
        workspace.ensure_dir_open(OsStr::new(".packet28"), 0o755)
    } else {
        workspace.open_dir(OsStr::new(".packet28"))
    }
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open Packet28 state for daemon runtime discovery",
            &state_path,
            source,
        )
    })?;
    ensure_capability_same_device(
        &workspace,
        &state,
        &state_path,
        "Packet28 runtime discovery state is on another filesystem",
    )?;
    let daemon_path = daemon_dir(&canonical_root);
    let daemon = if create {
        state.ensure_dir_open(OsStr::new("daemon"), 0o755)
    } else {
        state.open_dir(OsStr::new("daemon"))
    }
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open daemon runtime discovery capability",
            &daemon_path,
            source,
        )
    })?;
    ensure_capability_same_device(
        &state,
        &daemon,
        &daemon_path,
        "daemon runtime discovery state is on another filesystem",
    )?;
    Ok(daemon)
}

#[cfg(unix)]
fn write_anchored_runtime_file(
    daemon: &CapabilityDir,
    name: &str,
    path: &Path,
    bytes: &[u8],
    temporary_prefix: &str,
    description: &'static str,
) -> Result<()> {
    daemon
        .write_json_atomically(OsStr::new(name), bytes, temporary_prefix)
        .map_err(|error| {
            DaemonCoreError::io(
                if error.renamed {
                    "failed to synchronize authenticated daemon runtime publication"
                } else {
                    "failed to publish authenticated daemon runtime file"
                },
                path,
                std::io::Error::new(
                    error.source.kind(),
                    format!("{description}: {}", error.source),
                ),
            )
        })
}

/// Removes daemon socket and runtime discovery files that currently exist.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] when an existing runtime file cannot be
/// removed. Files that are already absent are ignored.
pub fn remove_runtime_files(root: &Path) -> Result<()> {
    for path in [
        socket_path(root),
        workspace_socket_path(root),
        pid_path(root),
        runtime_path(root),
        ready_path(root),
    ] {
        if path.exists() {
            fs::remove_file(&path).map_err(|source| {
                DaemonCoreError::io("failed to remove daemon runtime file", &path, source)
            })?;
        }
    }
    Ok(())
}

/// Loads the workspace watch registry under a shared interprocess lock.
///
/// Returns an empty registry when no file has been persisted.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the registry or lock file cannot be
/// opened, read, locked, or unlocked. Returns
/// [`DaemonCoreError::WatchRegistryTooLarge`] when the persisted registry
/// exceeds 64 MiB, or [`DaemonCoreError::Json`] if the bounded authority JSON
/// is malformed. Returns
/// [`DaemonCoreError::RegistryCheckpointGenerationMismatch`] or
/// [`DaemonCoreError::InvalidTaskWatchRegistry`] rather than exposing a watch
/// half that is not part of the committed task/watch checkpoint.
pub fn load_watch_registry(root: &Path) -> Result<WatchRegistry> {
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Shared,
            || Ok(()),
            |daemon| {
                let (_, watches, _, _) =
                    load_task_watch_registry_checkpoint_under_task_lock(root, daemon)?;
                Ok(watches)
            },
        )
    }
    #[cfg(not(unix))]
    {
        let task_path = task_registry_path(root);
        with_registry_lock(root, &task_path, RegistryLockMode::Shared, || {
            let (_, watches, _, _) =
                load_task_watch_registry_checkpoint_portable_under_task_lock(root)?;
            Ok(watches)
        })
    }
}

/// Persists the workspace watch registry under an exclusive interprocess lock.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Json`] if the registry cannot be encoded.
/// Returns [`DaemonCoreError::WatchRegistryTooLarge`] before taking a
/// lifecycle lease or changing state when its encoding exceeds 64 MiB.
/// Returns [`DaemonCoreError::RegistryCheckpointRequired`] when either
/// registry already belongs to paired checkpoint authority; callers must then
/// use [`save_task_watch_registry_checkpoint`].
/// Returns [`DaemonCoreError::Io`] if the daemon directory, lock, or registry
/// file cannot be created, written, synchronized, replaced, or unlocked.
pub fn save_watch_registry(root: &Path, registry: &WatchRegistry) -> Result<()> {
    let path = watch_registry_path(root);
    let _ = encode_watch_registry(&path, registry, None)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |daemon| {
                let task_path = task_registry_path(root);
                let (tasks, task_generation) =
                    match read_anchored_task_registry(daemon, &task_path)? {
                        Some(raw) => {
                            decode_task_registry_with_checkpoint_generation(&task_path, &raw)?
                        }
                        None => (TaskRegistry::default(), None),
                    };
                save_watch_registry_under_task_lock(root, daemon, registry, &tasks, task_generation)
            },
        )
    }
    #[cfg(not(unix))]
    {
        let task_path = task_registry_path(root);
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            let (tasks, task_generation) = match read_task_registry_portable(&task_path)? {
                Some(raw) => decode_task_registry_with_checkpoint_generation(&task_path, &raw)?,
                None => (TaskRegistry::default(), None),
            };
            save_watch_registry_under_task_lock(root, registry, &tasks, task_generation)
        })
    }
}

/// Loads the task registry under a shared interprocess lock.
///
/// Returns an empty registry when no file has been persisted.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if the registry or lock file cannot be
/// opened, read, locked, or unlocked. Returns [`DaemonCoreError::Json`] if the
/// persisted registry is malformed. Returns
/// [`DaemonCoreError::TaskRegistryTooLarge`] when the persisted registry
/// exceeds 64 MiB, or [`DaemonCoreError::InvalidTaskRegistry`] when a map key
/// and its embedded task identifier disagree. Returns
/// [`DaemonCoreError::RegistryCheckpointGenerationMismatch`] or
/// [`DaemonCoreError::InvalidTaskWatchRegistry`] when the paired watch
/// authority is not generation-consistent and bijective.
pub fn load_task_registry(root: &Path) -> Result<TaskRegistry> {
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Shared,
            || Ok(()),
            |daemon| {
                let (registry, _, _, _) =
                    load_task_watch_registry_checkpoint_under_task_lock(root, daemon)?;
                Ok(registry)
            },
        )
    }
    #[cfg(not(unix))]
    load_task_registry_portable(root)
}

#[cfg(unix)]
fn task_registry_read_error(
    daemon: &CapabilityDir,
    path: &Path,
    source: std::io::Error,
) -> DaemonCoreError {
    if source.kind() == std::io::ErrorKind::InvalidData
        && matches!(
            daemon.entry_is_regular_file(OsStr::new(TASK_REGISTRY_FILE_NAME)),
            Ok(Some(true))
        )
    {
        if let Ok(Some((encoded_bytes, _))) =
            daemon.entry_storage_bytes(OsStr::new(TASK_REGISTRY_FILE_NAME))
        {
            if encoded_bytes > MAX_TASK_REGISTRY_BYTES as u64 {
                return DaemonCoreError::TaskRegistryTooLarge {
                    path: path.to_path_buf(),
                    encoded_bytes,
                    max_bytes: MAX_TASK_REGISTRY_BYTES as u64,
                };
            }
        }
    }
    DaemonCoreError::io("failed to read anchored task registry", path, source)
}

#[cfg(unix)]
pub(super) fn load_task_watch_registry_checkpoint_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<(TaskRegistry, WatchRegistry, Option<u64>, Option<u64>)> {
    let loaded =
        load_task_watch_registry_checkpoint_with_delta_revision_under_task_lock(root, daemon)?;
    Ok((
        loaded.tasks,
        loaded.watches,
        loaded.task_generation,
        loaded.watch_generation,
    ))
}

pub(crate) struct LoadedRegistryCheckpoint {
    pub(crate) tasks: TaskRegistry,
    pub(crate) watches: WatchRegistry,
    pub(crate) task_bytes: Vec<u8>,
    pub(crate) task_generation: Option<u64>,
    pub(crate) watch_generation: Option<u64>,
    pub(crate) applied_delta_revision: u64,
}

#[cfg(unix)]
pub(crate) fn load_task_watch_registry_checkpoint_with_delta_revision_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<LoadedRegistryCheckpoint> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    with_anchored_watch_registry_lock(daemon, RegistryLockMode::Shared, || {
        let resolved = checkpoint::resolve_anchored(
            root,
            daemon,
            read_anchored_task_registry(daemon, &task_path)?,
            read_anchored_watch_registry(daemon, &watch_path)?,
        )?;
        let applied_delta_revision = resolved.applied_delta_revision();
        let (task_bytes, watch_bytes) = resolved.materialize(root)?;
        let (tasks, watches, task_generation, watch_generation) =
            decode_registry_checkpoint_pair(root, &task_bytes, &watch_bytes)?;
        Ok(LoadedRegistryCheckpoint {
            tasks,
            watches,
            task_bytes,
            task_generation,
            watch_generation,
            applied_delta_revision,
        })
    })
}

#[cfg(any(not(unix), test))]
fn load_task_watch_registry_checkpoint_portable_under_task_lock(
    root: &Path,
) -> Result<(TaskRegistry, WatchRegistry, Option<u64>, Option<u64>)> {
    let loaded =
        load_task_watch_registry_checkpoint_with_delta_revision_portable_under_task_lock(root)?;
    Ok((
        loaded.tasks,
        loaded.watches,
        loaded.task_generation,
        loaded.watch_generation,
    ))
}

#[cfg(any(not(unix), test))]
pub(crate) fn load_task_watch_registry_checkpoint_with_delta_revision_portable_under_task_lock(
    root: &Path,
) -> Result<LoadedRegistryCheckpoint> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    with_registry_lock(root, &watch_path, RegistryLockMode::Shared, || {
        let resolved = checkpoint::resolve_portable(
            root,
            read_task_registry_portable(&task_path)?,
            read_watch_registry(&watch_path)?,
        )?;
        let applied_delta_revision = resolved.applied_delta_revision();
        let (task_bytes, watch_bytes) = resolved.materialize(root)?;
        let (tasks, watches, task_generation, watch_generation) =
            decode_registry_checkpoint_pair(root, &task_bytes, &watch_bytes)?;
        Ok(LoadedRegistryCheckpoint {
            tasks,
            watches,
            task_bytes,
            task_generation,
            watch_generation,
            applied_delta_revision,
        })
    })
}

/// Persists the task registry under an exclusive interprocess lock.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Json`] if the registry cannot be encoded.
/// Returns [`DaemonCoreError::TaskRegistryTooLarge`] before taking a lifecycle
/// lease or changing state when its encoding exceeds 64 MiB. Returns
/// [`DaemonCoreError::TaskRegistryRetentionEnvelopeTooLarge`] on the same
/// boundary if a record cannot fit the crash-recovery journal. Returns
/// [`DaemonCoreError::InvalidTaskRegistry`] on the same no-mutation boundary
/// when a map key and its embedded task identifier disagree.
/// Returns [`DaemonCoreError::RegistryCheckpointRequired`] when either
/// registry already belongs to paired checkpoint authority; callers must then
/// use [`save_task_watch_registry_checkpoint`].
/// Returns [`DaemonCoreError::Io`] if the daemon directory, lock, or registry
/// file cannot be created, written, synchronized, replaced, or unlocked.
pub fn save_task_registry(root: &Path, registry: &TaskRegistry) -> Result<()> {
    #[cfg(unix)]
    {
        save_task_registry_with_observer(root, registry, || Ok(()))
    }
    #[cfg(not(unix))]
    {
        save_task_registry_portable(root, registry)
    }
}

/// Persists task and watch registries as one monotonic durable generation.
///
/// Both documents are fully encoded before either is replaced. Before
/// publication, the prior committed pair and a hash-bound transition journal
/// are synchronized. The watch and task images are then published, followed
/// by one atomic commit manifest. A crash before that manifest leaves the
/// prior pair recoverable; unrelated bytes that do not match a journaled
/// publication phase remain corruption.
///
/// Workspace downgrade to writers that predate paired checkpoints is not
/// supported once either document carries a generation. An older task-only
/// writer can preserve the unknown generation field while replacing task
/// content, which cannot be distinguished from a complete checkpoint by the
/// generation alone. Operators must not run such writers against upgraded
/// workspace state.
///
/// # Errors
///
/// Returns the same validation, encoding, locking, and filesystem errors as
/// [`save_task_registry`] and [`save_watch_registry`]. Returns
/// [`DaemonCoreError::RegistryCheckpointGenerationExhausted`] without
/// mutation when the stored generation cannot advance.
pub fn save_task_watch_registry_checkpoint(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
) -> Result<()> {
    validate_task_watch_registry_relationships(root, tasks, watches)?;
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    let task_bytes = encode_task_registry(&task_path, tasks)?;
    let _ = encode_watch_registry(&watch_path, watches, None)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;

    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |daemon| {
                save_task_watch_registry_checkpoint_anchored(
                    root, daemon, tasks, watches, task_bytes, None, None,
                )
            },
        )
    }
    #[cfg(not(unix))]
    {
        with_registry_lock(root, &task_path, RegistryLockMode::Exclusive, || {
            save_task_watch_registry_checkpoint_portable(
                root, tasks, watches, task_bytes, None, None,
            )
        })
    }
}

#[cfg(any(not(unix), test))]
fn save_task_registry_portable(root: &Path, registry: &TaskRegistry) -> Result<()> {
    let path = task_registry_path(root);
    let bytes = encode_task_registry(&path, registry)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    with_registry_lock(root, &path, RegistryLockMode::Exclusive, || {
        let existing = read_task_registry_portable(&path)?;
        let task_generation = existing
            .as_deref()
            .map(|raw| {
                registry_checkpoint_generation(&path, raw, AuthorityJsonProfile::TaskRegistry)
            })
            .transpose()?
            .flatten();
        let (watches, watch_generation) =
            load_watch_registry_with_generation_portable_under_task_lock(root)?;
        reject_standalone_registry_write(root, "task", task_generation, watch_generation)?;
        validate_task_watch_registry_relationships(root, registry, &watches)?;
        let bytes = encode_task_registry_preserving_existing(
            root,
            &path,
            registry,
            existing.as_deref(),
            bytes,
            None,
        )?;
        write_atomically(&path, &bytes)
    })
}

#[cfg(unix)]
fn save_task_registry_with_observer(
    root: &Path,
    registry: &TaskRegistry,
    after_daemon_open: impl FnOnce() -> Result<()>,
) -> Result<()> {
    save_task_registry_with_observers(root, registry, after_daemon_open, || Ok(()))
}

#[cfg(unix)]
fn save_task_registry_with_observers(
    root: &Path,
    registry: &TaskRegistry,
    after_daemon_open: impl FnOnce() -> Result<()>,
    after_temp_sync: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    let path = task_registry_path(root);
    let bytes = encode_task_registry(&path, registry)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    with_anchored_task_registry_lock(
        root,
        RegistryLockMode::Exclusive,
        after_daemon_open,
        |daemon| {
            let existing = read_anchored_task_registry(daemon, &path)?;
            let task_generation = existing
                .as_deref()
                .map(|raw| {
                    registry_checkpoint_generation(&path, raw, AuthorityJsonProfile::TaskRegistry)
                })
                .transpose()?
                .flatten();
            let (watches, watch_generation) =
                load_watch_registry_with_generation_under_task_lock(root, daemon)?;
            reject_standalone_registry_write(root, "task", task_generation, watch_generation)?;
            validate_task_watch_registry_relationships(root, registry, &watches)?;
            let bytes = encode_task_registry_preserving_existing(
                root,
                &path,
                registry,
                existing.as_deref(),
                bytes,
                None,
            )?;
            write_anchored_task_registry(daemon, &path, &bytes, after_temp_sync)
        },
    )
}

#[cfg(unix)]
fn read_anchored_task_registry(daemon: &CapabilityDir, path: &Path) -> Result<Option<Vec<u8>>> {
    match daemon.read_file_limited(OsStr::new(TASK_REGISTRY_FILE_NAME), MAX_TASK_REGISTRY_BYTES) {
        Ok(raw) => Ok(Some(raw)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(task_registry_read_error(daemon, path, source)),
    }
}

#[cfg(unix)]
fn write_anchored_task_registry(
    daemon: &CapabilityDir,
    path: &Path,
    bytes: &[u8],
    after_temp_sync: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    daemon
        .write_json_atomically_with_observers(
            OsStr::new(TASK_REGISTRY_FILE_NAME),
            bytes,
            TASK_REGISTRY_WRITE_TEMP_PREFIX,
            |_| Ok(()),
            after_temp_sync,
            || Ok(()),
        )
        .map_err(|error| {
            DaemonCoreError::io(
                if error.renamed {
                    "failed to synchronize anchored task registry replacement"
                } else {
                    "failed to write anchored task registry"
                },
                path,
                error.source,
            )
        })
}

#[cfg(unix)]
fn write_anchored_watch_registry(daemon: &CapabilityDir, path: &Path, bytes: &[u8]) -> Result<()> {
    daemon
        .write_json_atomically(
            OsStr::new(WATCH_REGISTRY_FILE_NAME),
            bytes,
            WATCH_REGISTRY_WRITE_TEMP_PREFIX,
        )
        .map_err(|error| {
            DaemonCoreError::io(
                if error.renamed {
                    "failed to synchronize anchored watch registry replacement"
                } else {
                    "failed to write anchored watch registry"
                },
                path,
                error.source,
            )
        })
}

#[cfg(unix)]
pub(crate) fn save_task_watch_registry_checkpoint_anchored(
    root: &Path,
    daemon: &CapabilityDir,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    task_bytes: Vec<u8>,
    target_delta_revision: Option<u64>,
    wal_admitted_task_ids: Option<&BTreeSet<String>>,
) -> Result<()> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    with_anchored_watch_registry_lock(daemon, RegistryLockMode::Exclusive, || {
        let resolved = checkpoint::resolve_anchored(
            root,
            daemon,
            read_anchored_task_registry(daemon, &task_path)?,
            read_anchored_watch_registry(daemon, &watch_path)?,
        )?;
        let task_bytes = encode_task_registry_preserving_existing(
            root,
            &task_path,
            tasks,
            resolved.tasks.as_deref(),
            task_bytes,
            wal_admitted_task_ids,
        )?;
        let canonical_recovery = resolved.canonical_recovery();
        let applied_delta_revision = resolved.applied_delta_revision();
        let target_delta_revision = target_delta_revision.unwrap_or(applied_delta_revision);
        if target_delta_revision < applied_delta_revision {
            return Err(DaemonCoreError::InvalidRegistryDeltaBatch {
                root: root.to_path_buf(),
                message: format!(
                    "checkpoint delta revision {target_delta_revision} precedes committed revision \
                     {applied_delta_revision}"
                ),
            });
        }
        let (base_task, base_watch) = resolved.materialize(root)?;
        let (task_generation, watch_generation) =
            registry_checkpoint_generations_from_raw(root, &base_task, &base_watch)?;
        let generation =
            next_registry_checkpoint_generation(root, task_generation, watch_generation)?;
        let task_bytes = inject_registry_checkpoint_generation(&task_path, task_bytes, generation)?;
        validate_encoded_task_registry(&task_path, tasks, &task_bytes)?;
        let watch_bytes = encode_watch_registry(&watch_path, watches, Some(generation))?;
        checkpoint::publish_anchored(
            root,
            daemon,
            canonical_recovery,
            checkpoint::RevisionedRegistryPair::new(
                &base_task,
                &base_watch,
                applied_delta_revision,
            ),
            checkpoint::RevisionedRegistryPair::new(
                &task_bytes,
                &watch_bytes,
                target_delta_revision,
            ),
            |bytes| write_anchored_watch_registry(daemon, &watch_path, bytes),
            |bytes| write_anchored_task_registry(daemon, &task_path, bytes, || Ok(())),
        )
    })
}

#[cfg(not(unix))]
pub(crate) fn save_task_watch_registry_checkpoint_portable(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
    task_bytes: Vec<u8>,
    target_delta_revision: Option<u64>,
    wal_admitted_task_ids: Option<&BTreeSet<String>>,
) -> Result<()> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    with_registry_lock(root, &watch_path, RegistryLockMode::Exclusive, || {
        let resolved = checkpoint::resolve_portable(
            root,
            read_task_registry_portable(&task_path)?,
            read_watch_registry(&watch_path)?,
        )?;
        let task_bytes = encode_task_registry_preserving_existing(
            root,
            &task_path,
            tasks,
            resolved.tasks.as_deref(),
            task_bytes,
            wal_admitted_task_ids,
        )?;
        let canonical_recovery = resolved.canonical_recovery();
        let applied_delta_revision = resolved.applied_delta_revision();
        let target_delta_revision = target_delta_revision.unwrap_or(applied_delta_revision);
        if target_delta_revision < applied_delta_revision {
            return Err(DaemonCoreError::InvalidRegistryDeltaBatch {
                root: root.to_path_buf(),
                message: format!(
                    "checkpoint delta revision {target_delta_revision} precedes committed revision \
                     {applied_delta_revision}"
                ),
            });
        }
        let (base_task, base_watch) = resolved.materialize(root)?;
        let (task_generation, watch_generation) =
            registry_checkpoint_generations_from_raw(root, &base_task, &base_watch)?;
        let generation =
            next_registry_checkpoint_generation(root, task_generation, watch_generation)?;
        let task_bytes = inject_registry_checkpoint_generation(&task_path, task_bytes, generation)?;
        validate_encoded_task_registry(&task_path, tasks, &task_bytes)?;
        let watch_bytes = encode_watch_registry(&watch_path, watches, Some(generation))?;
        checkpoint::publish_portable(
            root,
            canonical_recovery,
            checkpoint::RevisionedRegistryPair::new(
                &base_task,
                &base_watch,
                applied_delta_revision,
            ),
            checkpoint::RevisionedRegistryPair::new(
                &task_bytes,
                &watch_bytes,
                target_delta_revision,
            ),
            |bytes| write_atomically(&watch_path, bytes),
            |bytes| write_atomically(&task_path, bytes),
        )
    })
}

fn registry_checkpoint_generations_from_raw(
    root: &Path,
    task_bytes: &[u8],
    watch_bytes: &[u8],
) -> Result<(Option<u64>, Option<u64>)> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    let task_generation =
        registry_checkpoint_generation(&task_path, task_bytes, AuthorityJsonProfile::TaskRegistry)?;
    let watch_generation = registry_checkpoint_generation(
        &watch_path,
        watch_bytes,
        AuthorityJsonProfile::WatchRegistry,
    )?;
    validate_registry_checkpoint_generations(root, task_generation, watch_generation)?;
    Ok((task_generation, watch_generation))
}

fn decode_registry_checkpoint_pair(
    root: &Path,
    task_bytes: &[u8],
    watch_bytes: &[u8],
) -> Result<(TaskRegistry, WatchRegistry, Option<u64>, Option<u64>)> {
    let task_path = task_registry_path(root);
    let watch_path = watch_registry_path(root);
    let (tasks, task_generation) =
        decode_task_registry_with_checkpoint_generation(&task_path, task_bytes)?;
    let (watches, watch_generation) =
        decode_watch_registry_with_generation(&watch_path, watch_bytes)?;
    validate_registry_checkpoint_generations(root, task_generation, watch_generation)?;
    validate_task_watch_registry_relationships(root, &tasks, &watches)?;
    Ok((tasks, watches, task_generation, watch_generation))
}

fn next_registry_checkpoint_generation(
    root: &Path,
    task_generation: Option<u64>,
    watch_generation: Option<u64>,
) -> Result<u64> {
    task_generation
        .into_iter()
        .chain(watch_generation)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| DaemonCoreError::RegistryCheckpointGenerationExhausted {
            root: root.to_path_buf(),
            task_generation,
            watch_generation,
        })
}

#[cfg(any(test, debug_assertions))]
fn maybe_exit_after_registry_checkpoint_phase(phase: &str) {
    if std::env::var("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER").as_deref() == Ok(phase) {
        std::process::exit(86);
    }
}

#[cfg(not(any(test, debug_assertions)))]
fn maybe_exit_after_registry_checkpoint_phase(_phase: &str) {}

fn encode_task_registry(path: &Path, registry: &TaskRegistry) -> Result<Vec<u8>> {
    validate_task_registry(path, registry)?;
    let bytes = serde_json::to_vec_pretty(registry).map_err(|source| {
        DaemonCoreError::json("failed to encode task registry for", path, source)
    })?;
    validate_encoded_task_registry(path, registry, &bytes)?;
    Ok(bytes)
}

fn validate_encoded_task_registry(
    path: &Path,
    registry: &TaskRegistry,
    bytes: &[u8],
) -> Result<()> {
    if bytes.len() > MAX_TASK_REGISTRY_BYTES {
        return Err(DaemonCoreError::TaskRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: bytes.len() as u64,
            max_bytes: MAX_TASK_REGISTRY_BYTES as u64,
        });
    }
    validate_authority_json(bytes, AuthorityJsonProfile::TaskRegistry).map_err(|error| {
        map_authority_json_error(
            path,
            AuthorityJsonProfile::TaskRegistry,
            "failed to validate encoded task registry for",
            error,
        )
    })?;
    #[cfg(unix)]
    crate::retention::validate_task_registry_retention_envelopes(path, registry, bytes.len())?;
    Ok(())
}

fn encode_task_registry_preserving_existing(
    root: &Path,
    path: &Path,
    registry: &TaskRegistry,
    existing_raw: Option<&[u8]>,
    new_registry_bytes: Vec<u8>,
    wal_admitted_task_ids: Option<&BTreeSet<String>>,
) -> Result<Vec<u8>> {
    let Some(existing_raw) = existing_raw else {
        validate_task_registry_namespace_bindings(
            root,
            registry,
            None,
            wal_admitted_task_ids,
            path,
        )?;
        return Ok(new_registry_bytes);
    };
    // A present authority must be strict and supported before it can influence
    // a replacement. This prevents a normal save from laundering corrupt or
    // legacy-ambiguous state into a newly trusted registry.
    let existing_registry = decode_task_registry(path, existing_raw)?;
    let _ = registry_checkpoint_generation(path, existing_raw, AuthorityJsonProfile::TaskRegistry)?;
    validate_task_registry_namespace_bindings(
        root,
        registry,
        Some(&existing_registry),
        wal_admitted_task_ids,
        path,
    )?;
    let mut root =
        decode_json_value_without_duplicate_keys(existing_raw, AuthorityJsonProfile::TaskRegistry)
            .map_err(|error| {
                map_authority_json_error(
                    path,
                    AuthorityJsonProfile::TaskRegistry,
                    "failed to decode task registry before preserving unknown fields from",
                    error,
                )
            })?;
    let root_object = root.as_object_mut().ok_or_else(|| {
        DaemonCoreError::json(
            "failed to preserve task registry root from",
            path,
            <serde_json::Error as serde::de::Error>::custom(
                "task registry root must be a JSON object",
            ),
        )
    })?;
    let existing_tasks = root_object
        .remove("tasks")
        .and_then(|tasks| tasks.as_object().cloned())
        .ok_or_else(|| {
            DaemonCoreError::json(
                "failed to preserve task registry records from",
                path,
                <serde_json::Error as serde::de::Error>::custom(
                    "task registry tasks field must be a JSON object",
                ),
            )
        })?;
    let mut merged_tasks = serde_json::Map::new();
    for (task_id, record) in &registry.tasks {
        let known = serde_json::to_value(record).map_err(|source| {
            DaemonCoreError::json("failed to encode task record for", path, source)
        })?;
        let serde_json::Value::Object(known) = known else {
            return Err(DaemonCoreError::json(
                "failed to encode task record for",
                path,
                <serde_json::Error as serde::ser::Error>::custom(
                    "task record must serialize as a JSON object",
                ),
            ));
        };
        let mut merged = existing_tasks
            .get(task_id)
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        // These additive lifecycle markers are known fields even when their
        // false value is represented by omission. Remove an older true marker
        // before overlaying the newly serialized record so forward-field
        // preservation cannot resurrect a completed transition.
        for known_optional_field in ["cancelled", "recovered_replan"] {
            merged.remove(known_optional_field);
        }
        for (field, value) in known {
            merged.insert(field, value);
        }
        merged_tasks.insert(task_id.clone(), serde_json::Value::Object(merged));
    }
    root_object.insert("tasks".to_string(), serde_json::Value::Object(merged_tasks));
    let bytes = serde_json::to_vec_pretty(&root).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode task registry with preserved unknown fields for",
            path,
            source,
        )
    })?;
    validate_encoded_task_registry(path, registry, &bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
pub(super) fn load_watch_registry_with_generation_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
) -> Result<(WatchRegistry, Option<u64>)> {
    let path = watch_registry_path(root);
    with_anchored_watch_registry_lock(daemon, RegistryLockMode::Shared, || {
        let Some(raw) = read_anchored_watch_registry(daemon, &path)? else {
            return Ok((WatchRegistry::default(), None));
        };
        decode_watch_registry_with_generation(&path, &raw)
    })
}

#[cfg(any(not(unix), test))]
fn load_watch_registry_with_generation_portable_under_task_lock(
    root: &Path,
) -> Result<(WatchRegistry, Option<u64>)> {
    let path = watch_registry_path(root);
    with_registry_lock(root, &path, RegistryLockMode::Shared, || {
        let Some(raw) = read_watch_registry(&path)? else {
            return Ok((WatchRegistry::default(), None));
        };
        decode_watch_registry_with_generation(&path, &raw)
    })
}

#[cfg(unix)]
fn save_watch_registry_under_task_lock(
    root: &Path,
    daemon: &CapabilityDir,
    registry: &WatchRegistry,
    tasks: &TaskRegistry,
    task_generation: Option<u64>,
) -> Result<()> {
    let path = watch_registry_path(root);
    with_anchored_watch_registry_lock(daemon, RegistryLockMode::Exclusive, || {
        let existing = read_anchored_watch_registry(daemon, &path)?;
        save_watch_registry_locked(
            root,
            &path,
            registry,
            tasks,
            task_generation,
            existing.as_deref(),
            |bytes| write_anchored_watch_registry(daemon, &path, bytes),
        )
    })
}

#[cfg(not(unix))]
fn save_watch_registry_under_task_lock(
    root: &Path,
    registry: &WatchRegistry,
    tasks: &TaskRegistry,
    task_generation: Option<u64>,
) -> Result<()> {
    let path = watch_registry_path(root);
    with_registry_lock(root, &path, RegistryLockMode::Exclusive, || {
        let existing = read_watch_registry(&path)?;
        save_watch_registry_locked(
            root,
            &path,
            registry,
            tasks,
            task_generation,
            existing.as_deref(),
            |bytes| write_atomically(&path, bytes),
        )
    })
}

fn save_watch_registry_locked(
    root: &Path,
    path: &Path,
    registry: &WatchRegistry,
    tasks: &TaskRegistry,
    task_generation: Option<u64>,
    existing: Option<&[u8]>,
    write: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<()> {
    let watch_generation = existing
        .map(|raw| registry_checkpoint_generation(path, raw, AuthorityJsonProfile::WatchRegistry))
        .transpose()?
        .flatten();
    reject_standalone_registry_write(root, "watch", task_generation, watch_generation)?;
    validate_task_watch_registry_relationships(root, tasks, registry)?;
    let bytes = encode_watch_registry(path, registry, None)?;
    write(&bytes)
}

#[cfg(unix)]
fn read_anchored_watch_registry(daemon: &CapabilityDir, path: &Path) -> Result<Option<Vec<u8>>> {
    match daemon.read_file_limited(
        OsStr::new(WATCH_REGISTRY_FILE_NAME),
        MAX_WATCH_REGISTRY_BYTES,
    ) {
        Ok(raw) => Ok(Some(raw)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(watch_registry_read_error(daemon, path, source)),
    }
}

#[cfg(unix)]
fn watch_registry_read_error(
    daemon: &CapabilityDir,
    path: &Path,
    source: std::io::Error,
) -> DaemonCoreError {
    if source.kind() == std::io::ErrorKind::InvalidData
        && matches!(
            daemon.entry_is_regular_file(OsStr::new(WATCH_REGISTRY_FILE_NAME)),
            Ok(Some(true))
        )
    {
        if let Ok(Some((encoded_bytes, _))) =
            daemon.entry_storage_bytes(OsStr::new(WATCH_REGISTRY_FILE_NAME))
        {
            if encoded_bytes > MAX_WATCH_REGISTRY_BYTES as u64 {
                return DaemonCoreError::WatchRegistryTooLarge {
                    path: path.to_path_buf(),
                    encoded_bytes,
                    max_bytes: MAX_WATCH_REGISTRY_BYTES as u64,
                };
            }
        }
    }
    DaemonCoreError::io("failed to read anchored watch registry", path, source)
}

#[cfg(any(not(unix), test))]
fn read_watch_registry(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open watch registry",
                path,
                source,
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|source| DaemonCoreError::io("failed to inspect watch registry", path, source))?;
    if metadata.len() > MAX_WATCH_REGISTRY_BYTES as u64 {
        return Err(DaemonCoreError::WatchRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: metadata.len(),
            max_bytes: MAX_WATCH_REGISTRY_BYTES as u64,
        });
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_WATCH_REGISTRY_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|source| DaemonCoreError::io("failed to read watch registry", path, source))?;
    if raw.len() > MAX_WATCH_REGISTRY_BYTES {
        return Err(DaemonCoreError::WatchRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: raw.len() as u64,
            max_bytes: MAX_WATCH_REGISTRY_BYTES as u64,
        });
    }
    Ok(Some(raw))
}

fn encode_watch_registry(
    path: &Path,
    registry: &WatchRegistry,
    generation: Option<u64>,
) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(registry).map_err(|source| {
        DaemonCoreError::json("failed to encode watch registry for", path, source)
    })?;
    if let Some(generation) = generation {
        let object = value.as_object_mut().ok_or_else(|| {
            DaemonCoreError::json(
                "failed to encode watch registry for",
                path,
                <serde_json::Error as serde::ser::Error>::custom(
                    "watch registry root must serialize as a JSON object",
                ),
            )
        })?;
        object.insert(
            REGISTRY_CHECKPOINT_GENERATION_FIELD.to_string(),
            serde_json::Value::from(generation),
        );
    }
    let bytes = serde_json::to_vec_pretty(&value).map_err(|source| {
        DaemonCoreError::json("failed to encode watch registry for", path, source)
    })?;
    validate_encoded_watch_registry(path, &bytes)?;
    Ok(bytes)
}

fn validate_encoded_watch_registry(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_WATCH_REGISTRY_BYTES {
        return Err(DaemonCoreError::WatchRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: bytes.len() as u64,
            max_bytes: MAX_WATCH_REGISTRY_BYTES as u64,
        });
    }
    validate_authority_json(bytes, AuthorityJsonProfile::WatchRegistry).map_err(|error| {
        map_authority_json_error(
            path,
            AuthorityJsonProfile::WatchRegistry,
            "failed to validate encoded watch registry for",
            error,
        )
    })
}

fn decode_watch_registry_with_generation(
    path: &Path,
    raw: &[u8],
) -> Result<(WatchRegistry, Option<u64>)> {
    let value = decode_json_value_without_duplicate_keys(raw, AuthorityJsonProfile::WatchRegistry)
        .map_err(|error| {
            map_authority_json_error(
                path,
                AuthorityJsonProfile::WatchRegistry,
                "failed to decode watch registry from",
                error,
            )
        })?;
    let generation = registry_checkpoint_generation_from_value(path, &value)?;
    let registry = serde_json::from_value(value).map_err(|source| {
        DaemonCoreError::json("failed to decode watch registry from", path, source)
    })?;
    Ok((registry, generation))
}

pub(super) fn decode_task_registry_with_checkpoint_generation(
    path: &Path,
    raw: &[u8],
) -> Result<(TaskRegistry, Option<u64>)> {
    let registry = decode_task_registry(path, raw)?;
    let generation = registry_checkpoint_generation(path, raw, AuthorityJsonProfile::TaskRegistry)?;
    Ok((registry, generation))
}

fn registry_checkpoint_generation(
    path: &Path,
    raw: &[u8],
    profile: AuthorityJsonProfile,
) -> Result<Option<u64>> {
    let value = decode_json_value_without_duplicate_keys(raw, profile).map_err(|error| {
        map_authority_json_error(
            path,
            profile,
            "failed to decode registry checkpoint generation from",
            error,
        )
    })?;
    registry_checkpoint_generation_from_value(path, &value)
}

fn registry_checkpoint_generation_from_value(
    path: &Path,
    value: &serde_json::Value,
) -> Result<Option<u64>> {
    let object = value.as_object().ok_or_else(|| {
        DaemonCoreError::json(
            "failed to decode registry checkpoint generation from",
            path,
            <serde_json::Error as serde::de::Error>::custom("registry root must be a JSON object"),
        )
    })?;
    let Some(generation) = object.get(REGISTRY_CHECKPOINT_GENERATION_FIELD) else {
        return Ok(None);
    };
    generation.as_u64().map(Some).ok_or_else(|| {
        DaemonCoreError::json(
            "failed to decode registry checkpoint generation from",
            path,
            <serde_json::Error as serde::de::Error>::custom(format!(
                "{REGISTRY_CHECKPOINT_GENERATION_FIELD} must be an unsigned 64-bit integer"
            )),
        )
    })
}

fn inject_registry_checkpoint_generation(
    path: &Path,
    bytes: Vec<u8>,
    generation: u64,
) -> Result<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|source| {
        DaemonCoreError::json(
            "failed to inject registry checkpoint generation into",
            path,
            source,
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        DaemonCoreError::json(
            "failed to inject registry checkpoint generation into",
            path,
            <serde_json::Error as serde::de::Error>::custom("registry root must be a JSON object"),
        )
    })?;
    object.insert(
        REGISTRY_CHECKPOINT_GENERATION_FIELD.to_string(),
        serde_json::Value::from(generation),
    );
    serde_json::to_vec_pretty(&value).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode registry checkpoint generation for",
            path,
            source,
        )
    })
}

pub(super) fn validate_registry_checkpoint_generations(
    root: &Path,
    task_generation: Option<u64>,
    watch_generation: Option<u64>,
) -> Result<()> {
    if task_generation == watch_generation {
        return Ok(());
    }
    Err(DaemonCoreError::RegistryCheckpointGenerationMismatch {
        root: root.to_path_buf(),
        task_generation,
        watch_generation,
    })
}

pub(crate) fn validate_task_watch_registry_relationships(
    root: &Path,
    tasks: &TaskRegistry,
    watches: &WatchRegistry,
) -> Result<()> {
    let mut watch_owners = BTreeMap::<&str, &str>::new();
    for watch in &watches.watches {
        if watch.watch_id.trim().is_empty() {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: root.to_path_buf(),
                message: "watch registry contains an empty watch identifier".to_string(),
            });
        }
        let task_id = watch.spec.task_id.as_str();
        if task_id.trim().is_empty() {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: root.to_path_buf(),
                message: format!("watch '{}' has an empty task identifier", watch.watch_id),
            });
        }
        if !tasks.tasks.contains_key(task_id) {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: root.to_path_buf(),
                message: format!(
                    "watch '{}' refers to missing task '{task_id}'",
                    watch.watch_id
                ),
            });
        }
        if let Some(existing_owner) = watch_owners.insert(&watch.watch_id, task_id) {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: root.to_path_buf(),
                message: format!(
                    "watch identifier '{}' is duplicated for tasks '{existing_owner}' and \
                     '{task_id}'",
                    watch.watch_id
                ),
            });
        }
    }
    for (task_id, task) in &tasks.tasks {
        let mut task_watch_ids = BTreeSet::new();
        for watch_id in &task.watch_ids {
            if !task_watch_ids.insert(watch_id.as_str()) {
                return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                    root: root.to_path_buf(),
                    message: format!("task '{task_id}' repeats watch identifier '{watch_id}'"),
                });
            }
            match watch_owners.get(watch_id.as_str()) {
                Some(owner) if *owner == task_id => {}
                Some(owner) => {
                    return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                        root: root.to_path_buf(),
                        message: format!(
                            "task '{task_id}' lists watch '{watch_id}' owned by task '{owner}'"
                        ),
                    });
                }
                None => {
                    return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                        root: root.to_path_buf(),
                        message: format!("task '{task_id}' refers to missing watch '{watch_id}'"),
                    });
                }
            }
        }
    }
    for (watch_id, task_id) in watch_owners {
        if !tasks.tasks[task_id]
            .watch_ids
            .iter()
            .any(|candidate| candidate == watch_id)
        {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: root.to_path_buf(),
                message: format!("watch '{watch_id}' is not listed by its task record '{task_id}'"),
            });
        }
    }
    Ok(())
}

fn reject_standalone_registry_write(
    root: &Path,
    registry: &'static str,
    task_generation: Option<u64>,
    watch_generation: Option<u64>,
) -> Result<()> {
    if task_generation.is_none() && watch_generation.is_none() {
        return Ok(());
    }
    Err(DaemonCoreError::RegistryCheckpointRequired {
        root: Box::new(root.to_path_buf()),
        registry,
        task_generation,
        watch_generation,
    })
}

fn require_daemon_lifecycle_lease(
    root: &Path,
    lease: &crate::task_store_lease::TaskStoreLease,
) -> Result<()> {
    if lease.role() == crate::task_store_lease::LeaseRole::DaemonLifecycle
        && lease.matches_root_argument(root)
    {
        #[cfg(unix)]
        lease.validate_namespace_attachment()?;
        return Ok(());
    }
    Err(DaemonCoreError::io(
        "daemon admission requires the matching daemon task-store lease",
        root,
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "task-store lease does not own this daemon lifecycle",
        ),
    ))
}

fn validate_task_registry_namespace_bindings(
    root: &Path,
    registry: &TaskRegistry,
    existing_registry: Option<&TaskRegistry>,
    wal_admitted_task_ids: Option<&BTreeSet<String>>,
    registry_path: &Path,
) -> Result<()> {
    let mut existing_task_ids = existing_registry
        .map(|existing| existing.tasks.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    if let Some(wal_admitted_task_ids) = wal_admitted_task_ids {
        existing_task_ids.extend(wal_admitted_task_ids.iter().cloned());
    }
    let expected_artifacts = registry
        .tasks
        .keys()
        .map(|task_id| {
            (
                task_storage_key_alias_class(task_id),
                (task_id.clone(), task_id.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_events = registry
        .tasks
        .keys()
        .map(|task_id| {
            let file_name = format!("{task_id}{TASK_EVENT_LOG_SUFFIX}");
            (
                task_storage_key_alias_class(&file_name),
                (task_id.clone(), file_name),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for (managed_root, expected) in [
        (task_artifacts_dir(root), &expected_artifacts),
        (task_events_dir(root), &expected_events),
    ] {
        let metadata = match fs::symlink_metadata(&managed_root) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(DaemonCoreError::io(
                    "failed to inspect task storage namespace before registry save",
                    &managed_root,
                    source,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DaemonCoreError::InvalidTaskRegistry {
                path: registry_path.to_path_buf(),
                message: format!(
                    "managed task namespace {} is not a real directory",
                    managed_root.display()
                ),
            });
        }
        let entries = fs::read_dir(&managed_root).map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate task storage namespace before registry save",
                &managed_root,
                source,
            )
        })?;
        let mut entries_seen = 0_usize;
        for entry in entries {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > MAX_TASK_REGISTRY_RECORDS {
                return Err(DaemonCoreError::InvalidTaskRegistry {
                    path: registry_path.to_path_buf(),
                    message: format!(
                        "managed task namespace {} exceeds the supported {}-entry validation bound",
                        managed_root.display(),
                        MAX_TASK_REGISTRY_RECORDS
                    ),
                });
            }
            let entry = entry.map_err(|source| {
                DaemonCoreError::io(
                    "failed to enumerate task storage namespace before registry save",
                    &managed_root,
                    source,
                )
            })?;
            let Some(actual_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let alias_class = task_storage_key_alias_class(&actual_name);
            let Some((task_id, expected_name)) = expected.get(&alias_class) else {
                continue;
            };
            if actual_name.as_str() != expected_name {
                return Err(DaemonCoreError::InvalidTaskRegistry {
                    path: registry_path.to_path_buf(),
                    message: format!(
                        "managed entry {actual_name:?} aliases the canonical spelling {expected_name:?}"
                    ),
                });
            }
            if !existing_task_ids.contains(task_id) {
                return Err(DaemonCoreError::InvalidTaskRegistry {
                    path: registry_path.to_path_buf(),
                    message: format!(
                        "new task {task_id:?} cannot adopt pre-existing managed entry {}",
                        entry.path().display()
                    ),
                });
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;

                let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                    DaemonCoreError::io(
                        "failed to inspect managed task entry before registry save",
                        entry.path(),
                        source,
                    )
                })?;
                if metadata.is_file() && metadata.nlink() > 1 {
                    return Err(DaemonCoreError::InvalidTaskRegistry {
                        path: registry_path.to_path_buf(),
                        message: format!(
                            "managed task entry {} has multiple physical links",
                            entry.path().display()
                        ),
                    });
                }
            }
        }
    }
    Ok(())
}

fn decode_task_registry(path: &Path, raw: &[u8]) -> Result<TaskRegistry> {
    let value = decode_json_value_without_duplicate_keys(raw, AuthorityJsonProfile::TaskRegistry)
        .map_err(|error| {
        map_authority_json_error(
            path,
            AuthorityJsonProfile::TaskRegistry,
            "failed to decode task registry from",
            error,
        )
    })?;
    if let Some(message) = task_registry_value_shape_error(&value) {
        let source = <serde_json::Error as serde::de::Error>::custom(message);
        return Err(DaemonCoreError::json(
            "failed to decode task registry from",
            path,
            source,
        ));
    }
    let registry = serde_json::from_value(value).map_err(|source| {
        DaemonCoreError::json("failed to decode task registry from", path, source)
    })?;
    validate_task_registry(path, &registry)?;
    Ok(registry)
}

pub(crate) fn task_registry_value_shape_error(value: &serde_json::Value) -> Option<&'static str> {
    let Some(root) = value.as_object() else {
        return Some("task registry root must be a JSON object");
    };
    if !root.get("tasks").is_some_and(serde_json::Value::is_object) {
        return Some("persisted task registry must contain an object-valued tasks field");
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AuthorityJsonProfile {
    ActiveTask,
    TaskRegistry,
    WatchRegistry,
    TaskEventFrame,
    RetentionJournal { max_bytes: usize },
}

impl AuthorityJsonProfile {
    const fn authority(self) -> &'static str {
        match self {
            Self::ActiveTask => "active-task",
            Self::TaskRegistry => "task-registry",
            Self::WatchRegistry => "watch-registry",
            Self::TaskEventFrame => "task-event-frame",
            Self::RetentionJournal { .. } => "retention-journal",
        }
    }

    const fn max_decoded_string_bytes(self) -> usize {
        match self {
            Self::ActiveTask => MAX_ACTIVE_TASK_RECORD_BYTES,
            Self::TaskRegistry => MAX_TASK_REGISTRY_BYTES,
            Self::WatchRegistry => MAX_WATCH_REGISTRY_BYTES,
            Self::TaskEventFrame => MAX_TASK_EVENT_LINE_BYTES,
            Self::RetentionJournal { max_bytes } => max_bytes,
        }
    }

    const fn max_registry_records(self) -> usize {
        match self {
            Self::WatchRegistry => MAX_WATCH_REGISTRY_RECORDS,
            Self::ActiveTask
            | Self::TaskRegistry
            | Self::TaskEventFrame
            | Self::RetentionJournal { .. } => MAX_TASK_REGISTRY_RECORDS,
        }
    }

    const fn max_value_nodes(self) -> usize {
        match self {
            Self::TaskRegistry => MAX_TASK_REGISTRY_AUTHORITY_JSON_VALUE_NODES,
            Self::ActiveTask
            | Self::WatchRegistry
            | Self::TaskEventFrame
            | Self::RetentionJournal { .. } => MAX_AUTHORITY_JSON_VALUE_NODES,
        }
    }

    const fn max_container_entries(self) -> usize {
        match self {
            Self::TaskRegistry => MAX_TASK_REGISTRY_AUTHORITY_JSON_CONTAINER_ENTRIES,
            Self::ActiveTask
            | Self::WatchRegistry
            | Self::TaskEventFrame
            | Self::RetentionJournal { .. } => MAX_AUTHORITY_JSON_CONTAINER_ENTRIES,
        }
    }

    const fn max_tokens(self) -> usize {
        match self {
            Self::TaskRegistry => MAX_TASK_REGISTRY_AUTHORITY_JSON_TOKENS,
            Self::ActiveTask
            | Self::WatchRegistry
            | Self::TaskEventFrame
            | Self::RetentionJournal { .. } => MAX_AUTHORITY_JSON_TOKENS,
        }
    }
}

#[derive(Debug)]
pub(crate) enum AuthorityJsonError {
    Json(serde_json::Error),
    Limit {
        resource: &'static str,
        observed: usize,
        max: usize,
    },
}

impl std::fmt::Display for AuthorityJsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(source) => source.fmt(formatter),
            Self::Limit {
                resource,
                observed,
                max,
            } => write!(
                formatter,
                "authority JSON exceeds the {resource} budget: observed {observed}, maximum {max}"
            ),
        }
    }
}

impl std::error::Error for AuthorityJsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(source) => Some(source),
            Self::Limit { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AuthorityJsonLimits {
    max_depth: usize,
    max_value_nodes: usize,
    max_container_entries: usize,
    max_entries_per_container: usize,
    max_tokens: usize,
    max_decoded_string_bytes: usize,
    max_registry_records: usize,
}

impl AuthorityJsonLimits {
    const fn for_profile(profile: AuthorityJsonProfile) -> Self {
        Self {
            max_depth: MAX_AUTHORITY_JSON_DEPTH,
            max_value_nodes: profile.max_value_nodes(),
            max_container_entries: profile.max_container_entries(),
            max_entries_per_container: MAX_AUTHORITY_JSON_ENTRIES_PER_CONTAINER,
            max_tokens: profile.max_tokens(),
            max_decoded_string_bytes: profile.max_decoded_string_bytes(),
            max_registry_records: profile.max_registry_records(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorityJsonPosition {
    Root,
    RegistryTasks,
    RegistryWatches,
    JournalRecordValues,
    JournalComponents,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct AuthorityJsonViolation {
    resource: &'static str,
    observed: usize,
    max: usize,
}

struct AuthorityJsonBudget {
    profile: AuthorityJsonProfile,
    limits: AuthorityJsonLimits,
    value_nodes: usize,
    container_entries: usize,
    tokens: usize,
    decoded_string_bytes: usize,
    violation: Option<AuthorityJsonViolation>,
}

impl AuthorityJsonBudget {
    fn new(profile: AuthorityJsonProfile, limits: AuthorityJsonLimits) -> Self {
        Self {
            profile,
            limits,
            value_nodes: 0,
            container_entries: 0,
            tokens: 0,
            decoded_string_bytes: 0,
            violation: None,
        }
    }

    fn reject<E: serde::de::Error>(
        &mut self,
        resource: &'static str,
        observed: usize,
        max: usize,
    ) -> std::result::Result<(), E> {
        self.violation.get_or_insert(AuthorityJsonViolation {
            resource,
            observed,
            max,
        });
        Err(E::custom("authority JSON resource budget exceeded"))
    }

    fn consume_value<E: serde::de::Error>(&mut self, depth: usize) -> std::result::Result<(), E> {
        if depth > self.limits.max_depth {
            return self.reject("nesting depth", depth, self.limits.max_depth);
        }
        let observed = self.value_nodes.saturating_add(1);
        if observed > self.limits.max_value_nodes {
            return self.reject("value nodes", observed, self.limits.max_value_nodes);
        }
        self.value_nodes = observed;
        self.consume_token()
    }

    fn consume_container_entry<E: serde::de::Error>(&mut self) -> std::result::Result<(), E> {
        let observed = self.container_entries.saturating_add(1);
        if observed > self.limits.max_container_entries {
            return self.reject(
                "container entries",
                observed,
                self.limits.max_container_entries,
            );
        }
        self.container_entries = observed;
        Ok(())
    }

    fn consume_token<E: serde::de::Error>(&mut self) -> std::result::Result<(), E> {
        let observed = self.tokens.saturating_add(1);
        if observed > self.limits.max_tokens {
            return self.reject("tokens", observed, self.limits.max_tokens);
        }
        self.tokens = observed;
        Ok(())
    }

    fn consume_string<E: serde::de::Error>(&mut self, length: usize) -> std::result::Result<(), E> {
        let observed = self.decoded_string_bytes.saturating_add(length);
        if observed > self.limits.max_decoded_string_bytes {
            return self.reject(
                "decoded string bytes",
                observed,
                self.limits.max_decoded_string_bytes,
            );
        }
        self.decoded_string_bytes = observed;
        Ok(())
    }

    fn check_container_length<E: serde::de::Error>(
        &mut self,
        entries: usize,
    ) -> std::result::Result<(), E> {
        if entries > self.limits.max_entries_per_container {
            return self.reject(
                "entries per container",
                entries,
                self.limits.max_entries_per_container,
            );
        }
        Ok(())
    }

    fn check_profile_container_length<E: serde::de::Error>(
        &mut self,
        position: AuthorityJsonPosition,
        entries: usize,
    ) -> std::result::Result<(), E> {
        let (limit, resource) = match position {
            AuthorityJsonPosition::RegistryTasks => {
                (self.limits.max_registry_records, "task-registry records")
            }
            AuthorityJsonPosition::RegistryWatches => {
                (self.limits.max_registry_records, "watch-registry records")
            }
            AuthorityJsonPosition::JournalRecordValues => (1, "journal record values"),
            AuthorityJsonPosition::JournalComponents => (2, "journal components"),
            AuthorityJsonPosition::Root | AuthorityJsonPosition::Other => return Ok(()),
        };
        if entries > limit {
            return self.reject(resource, entries, limit);
        }
        Ok(())
    }

    fn child_position(&self, parent: AuthorityJsonPosition, key: &str) -> AuthorityJsonPosition {
        if parent != AuthorityJsonPosition::Root {
            return AuthorityJsonPosition::Other;
        }
        match (self.profile, key) {
            (AuthorityJsonProfile::TaskRegistry, "tasks") => AuthorityJsonPosition::RegistryTasks,
            (AuthorityJsonProfile::WatchRegistry, "watches") => {
                AuthorityJsonPosition::RegistryWatches
            }
            (AuthorityJsonProfile::RetentionJournal { .. }, "record_values") => {
                AuthorityJsonPosition::JournalRecordValues
            }
            (AuthorityJsonProfile::RetentionJournal { .. }, "components") => {
                AuthorityJsonPosition::JournalComponents
            }
            _ => AuthorityJsonPosition::Other,
        }
    }
}

struct AuthorityJsonSeed<'a> {
    budget: &'a mut AuthorityJsonBudget,
    depth: usize,
    position: AuthorityJsonPosition,
}

impl<'de> serde::de::DeserializeSeed<'de> for AuthorityJsonSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget.consume_value::<D::Error>(self.depth)?;
        deserializer.deserialize_any(AuthorityJsonVisitor {
            budget: self.budget,
            depth: self.depth,
            position: self.position,
        })
    }
}

struct AuthorityJsonVisitor<'a> {
    budget: &'a mut AuthorityJsonBudget,
    depth: usize,
    position: AuthorityJsonPosition,
}

impl<'de> serde::de::Visitor<'de> for AuthorityJsonVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded authority JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.consume_string::<E>(value.len())
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.consume_string::<E>(value.len())
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut entries = 0_usize;
        while let Some(()) = sequence.next_element_seed(AuthorityJsonSeed {
            budget: self.budget,
            depth: self.depth.saturating_add(1),
            position: AuthorityJsonPosition::Other,
        })? {
            entries = entries.saturating_add(1);
            self.budget
                .check_profile_container_length::<A::Error>(self.position, entries)?;
            self.budget.check_container_length::<A::Error>(entries)?;
            self.budget.consume_container_entry::<A::Error>()?;
        }
        Ok(())
    }

    fn visit_map<A>(self, mut object: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut keys = BTreeMap::<String, ()>::new();
        let mut entries = 0_usize;
        while let Some(key) = object.next_key::<String>()? {
            self.budget.consume_token::<A::Error>()?;
            self.budget.consume_string::<A::Error>(key.len())?;
            if keys.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
            entries = entries.saturating_add(1);
            self.budget
                .check_profile_container_length::<A::Error>(self.position, entries)?;
            self.budget.check_container_length::<A::Error>(entries)?;
            self.budget.consume_container_entry::<A::Error>()?;
            let child_position = self.budget.child_position(self.position, &key);
            keys.insert(key, ());
            object.next_value_seed(AuthorityJsonSeed {
                budget: self.budget,
                depth: self.depth.saturating_add(1),
                position: child_position,
            })?;
        }
        Ok(())
    }
}

fn validate_authority_json_with_limits(
    raw: &[u8],
    profile: AuthorityJsonProfile,
    limits: AuthorityJsonLimits,
) -> std::result::Result<(), AuthorityJsonError> {
    use serde::de::DeserializeSeed as _;

    let mut budget = AuthorityJsonBudget::new(profile, limits);
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let parsed = AuthorityJsonSeed {
        budget: &mut budget,
        depth: 1,
        position: AuthorityJsonPosition::Root,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    match parsed {
        Ok(()) => Ok(()),
        Err(source) => match budget.violation {
            Some(AuthorityJsonViolation {
                resource,
                observed,
                max,
            }) => Err(AuthorityJsonError::Limit {
                resource,
                observed,
                max,
            }),
            None => Err(AuthorityJsonError::Json(source)),
        },
    }
}

pub(crate) fn validate_authority_json(
    raw: &[u8],
    profile: AuthorityJsonProfile,
) -> std::result::Result<(), AuthorityJsonError> {
    validate_authority_json_with_limits(raw, profile, AuthorityJsonLimits::for_profile(profile))
}

pub(crate) fn decode_json_value_without_duplicate_keys(
    raw: &[u8],
    profile: AuthorityJsonProfile,
) -> std::result::Result<serde_json::Value, AuthorityJsonError> {
    validate_authority_json(raw, profile)?;
    serde_json::from_slice(raw).map_err(AuthorityJsonError::Json)
}

pub(crate) fn map_authority_json_error(
    path: &Path,
    profile: AuthorityJsonProfile,
    operation: &'static str,
    error: AuthorityJsonError,
) -> DaemonCoreError {
    match error {
        AuthorityJsonError::Json(source) => DaemonCoreError::json(operation, path, source),
        AuthorityJsonError::Limit {
            resource,
            observed,
            max,
        } => DaemonCoreError::AuthorityJsonLimitExceeded {
            path: path.to_path_buf(),
            authority: profile.authority(),
            resource,
            observed: observed as u64,
            max: max as u64,
        },
    }
}

fn validate_task_registry(path: &Path, registry: &TaskRegistry) -> Result<()> {
    if let Some(message) = task_registry_shape_error(registry) {
        return Err(DaemonCoreError::InvalidTaskRegistry {
            path: path.to_path_buf(),
            message,
        });
    }
    Ok(())
}

pub(crate) fn task_registry_shape_error(registry: &TaskRegistry) -> Option<String> {
    let mut storage_key_owners = BTreeMap::<String, &str>::new();
    for (task_id, record) in &registry.tasks {
        if let Some(message) = task_identifier_shape_error(task_id) {
            return Some(format!(
                "task map contains an unsupported identifier: {message}"
            ));
        }
        let storage_key = derived_task_storage_key(task_id);
        let alias_class = task_storage_key_alias_class(&storage_key);
        if let Some(existing) = storage_key_owners.insert(alias_class, task_id) {
            return Some(format!(
                "task identifiers {existing:?} and {task_id:?} derive filesystem-aliasing \
                 storage keys"
            ));
        }
        if record.task_id != *task_id {
            return Some(format!(
                "task map key {task_id:?} does not match embedded identifier {:?}",
                record.task_id
            ));
        }
    }
    None
}

pub(crate) fn task_identifier_shape_error(task_id: &str) -> Option<String> {
    TaskStorageId::try_from(task_id)
        .err()
        .map(|error| error.to_string())
}

/// Validates the injective portable task-identifier contract used by every
/// task-store producer.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskStorageIdentifier`] when `task_id`
/// is empty, outside the lowercase portable ASCII domain, reserved on
/// Windows, or too long for an event-log path component.
pub fn validate_task_storage_identifier(root: &Path, task_id: &str) -> Result<()> {
    let path = task_artifacts_dir(root).join(task_id);
    validate_task_identifier_for_path(&path, task_id)
}

/// Removes storage created by the failed first run of a newly admitted task.
///
/// The task must still be present in the durable registry. The registry
/// admission check and removal are serialized against supported event and
/// checkpoint writers, so this operation cannot adopt or erase an
/// unregistered namespace. On Unix, exact entries are removed relative to
/// retained no-follow directory capabilities and are never followed through
/// symbolic links.
///
/// Callers must remove the task from the durable registry only after this
/// operation succeeds. A failure therefore leaves the task admitted, even if
/// one of its two optional storage entries was already removed.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskStorageIdentifier`] when `task_id`
/// violates the portable storage contract, [`DaemonCoreError::InvalidTaskRegistry`]
/// when it is not durably admitted, or [`DaemonCoreError::Io`] when the writer
/// lease, registry lock, capability validation, or exact removal fails.
pub fn remove_failed_initial_task_storage(root: &Path, task_id: &str) -> Result<()> {
    let task_id = checked_task_storage_id(root, task_id)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    remove_failed_initial_task_storage_admitted(root, &task_id)
}

/// Removes failed-initial-task storage while retaining daemon registry authority.
///
/// This is the daemon-owner counterpart to
/// [`remove_failed_initial_task_storage`]. Its mutable authority borrow
/// mechanically excludes concurrent authority-based event publication. The
/// caller must also serialize the operation with queued event publication
/// through the persistence owner.
///
/// # Errors
///
/// Returns an invalid-registry error when `task_id` is absent from the
/// authenticated authority, an I/O permission error when `authority` belongs
/// to another root, or the cleanup errors documented by
/// [`remove_failed_initial_task_storage`].
pub fn remove_failed_initial_task_storage_with_authority(
    root: &Path,
    authority: &mut RegistryAdmissionAuthority,
    task_id: &str,
) -> Result<()> {
    let task_id = checked_task_storage_id(root, task_id)?;
    authority.require_task(root, &task_id)?;
    remove_failed_initial_task_storage_admitted(root, &task_id)
}

fn remove_failed_initial_task_storage_admitted(root: &Path, task_id: &TaskStorageId) -> Result<()> {
    with_exclusively_registered_task_storage_id(root, task_id, || {
        #[cfg(unix)]
        {
            remove_failed_initial_task_storage_anchored(root, task_id)
        }
        #[cfg(not(unix))]
        {
            remove_failed_initial_task_storage_portable(root, task_id)
        }
    })
}

fn validate_task_identifier_for_path(path: &Path, task_id: &str) -> Result<()> {
    if let Some(message) = task_identifier_shape_error(task_id) {
        return Err(DaemonCoreError::InvalidTaskStorageIdentifier {
            path: path.to_path_buf(),
            message,
        });
    }
    Ok(())
}

pub(crate) fn task_storage_key_alias_class(storage_key: &str) -> String {
    // Use a deliberately conservative portable alias class. Compatibility
    // normalization captures canonically equivalent and compatibility
    // spellings used by case-insensitive Apple filesystems; full default
    // case-folding captures multi-scalar folds such as `ß` -> `ss`. A final
    // NFKC pass re-normalizes any expansion introduced by case folding.
    let mut normalized = storage_key.nfkc().case_fold().nfkc().collect::<String>();
    let trimmed_len = normalized.trim_end_matches([' ', '.']).len();
    normalized.truncate(trimmed_len);
    normalized
}

pub(crate) fn task_storage_key_is_portable(storage_key: &str) -> bool {
    !storage_key.is_empty()
        && storage_key.len() <= MAX_TASK_STORAGE_KEY_BYTES
        && storage_key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && !windows_storage_key_is_reserved(storage_key)
}

fn derived_task_storage_key(task_id: &str) -> String {
    task_id.to_string()
}

fn windows_storage_key_is_reserved(storage_key: &str) -> bool {
    let normalized = storage_key
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(normalized.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || normalized
            .strip_prefix("COM")
            .or_else(|| normalized.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

#[cfg(any(not(unix), test))]
fn load_task_registry_portable(root: &Path) -> Result<TaskRegistry> {
    let path = task_registry_path(root);
    with_registry_lock(root, &path, RegistryLockMode::Shared, || {
        let (registry, _, _, _) =
            load_task_watch_registry_checkpoint_portable_under_task_lock(root)?;
        Ok(registry)
    })
}

#[cfg(any(not(unix), test))]
fn read_task_registry_portable(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open task registry",
                path,
                source,
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|source| DaemonCoreError::io("failed to inspect task registry", path, source))?;
    if metadata.len() > MAX_TASK_REGISTRY_BYTES as u64 {
        return Err(DaemonCoreError::TaskRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: metadata.len(),
            max_bytes: MAX_TASK_REGISTRY_BYTES as u64,
        });
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_TASK_REGISTRY_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|source| DaemonCoreError::io("failed to read task registry", path, source))?;
    if raw.len() > MAX_TASK_REGISTRY_BYTES {
        return Err(DaemonCoreError::TaskRegistryTooLarge {
            path: path.to_path_buf(),
            encoded_bytes: raw.len() as u64,
            max_bytes: MAX_TASK_REGISTRY_BYTES as u64,
        });
    }
    Ok(Some(raw))
}

#[cfg(test)]
pub(crate) fn remove_task_registry_records_if_unchanged(
    root: &Path,
    expected_records: &BTreeMap<String, Vec<u8>>,
) -> Result<bool> {
    if expected_records.is_empty() {
        return Ok(true);
    }

    let path = task_registry_path(root);
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Exclusive,
            || Ok(()),
            |daemon| {
                let raw = daemon
                    .read_file_limited(OsStr::new(TASK_REGISTRY_FILE_NAME), MAX_TASK_REGISTRY_BYTES)
                    .map_err(|source| {
                        DaemonCoreError::io("failed to read anchored task registry", &path, source)
                    })?;
                let mut registry = decode_task_registry(&path, &raw)?;
                for (task_id, expected) in expected_records {
                    let Some(current) = registry.tasks.get(task_id) else {
                        return Ok(false);
                    };
                    let current = serde_json::to_vec(current).map_err(|source| {
                        DaemonCoreError::json(
                            "failed to verify task registry record in",
                            &path,
                            source,
                        )
                    })?;
                    if &current != expected {
                        return Ok(false);
                    }
                }
                for task_id in expected_records.keys() {
                    registry.tasks.remove(task_id);
                }
                let bytes = serde_json::to_vec_pretty(&registry).map_err(|source| {
                    DaemonCoreError::json("failed to encode task registry for", &path, source)
                })?;
                daemon
                    .write_json_atomically(
                        OsStr::new(TASK_REGISTRY_FILE_NAME),
                        &bytes,
                        TASK_REGISTRY_WRITE_TEMP_PREFIX,
                    )
                    .map_err(|error| {
                        DaemonCoreError::io(
                            if error.renamed {
                                "failed to synchronize anchored task registry replacement"
                            } else {
                                "failed to write anchored task registry"
                            },
                            &path,
                            error.source,
                        )
                    })?;
                Ok(true)
            },
        )
    }
    #[cfg(not(unix))]
    with_registry_lock(root, &path, RegistryLockMode::Exclusive, || {
        let raw = fs::read(&path)
            .map_err(|source| DaemonCoreError::io("failed to read task registry", &path, source))?;
        let mut registry = decode_task_registry(&path, &raw)?;
        for (task_id, expected) in expected_records {
            let Some(current) = registry.tasks.get(task_id) else {
                return Ok(false);
            };
            let current = serde_json::to_vec(current).map_err(|source| {
                DaemonCoreError::json("failed to verify task registry record in", &path, source)
            })?;
            if &current != expected {
                return Ok(false);
            }
        }
        for task_id in expected_records.keys() {
            registry.tasks.remove(task_id);
        }
        let bytes = serde_json::to_vec_pretty(&registry).map_err(|source| {
            DaemonCoreError::json("failed to encode task registry for", &path, source)
        })?;
        write_atomically(&path, &bytes)?;
        Ok(true)
    })
}

/// Appends one contiguous JSON-line event to a task's durable event log.
///
/// # Errors
///
/// Returns an error without changing the event namespace when the frame task
/// identifier is invalid, has not already been admitted by the durable task
/// registry, or does not continue a fully valid existing log. Returns
/// [`DaemonCoreError::Json`] if `frame` cannot be encoded. Returns
/// [`DaemonCoreError::Io`] if the event directory or log cannot be opened,
/// locked, appended, synchronized, or unlocked.
pub fn append_task_event(root: &Path, frame: &DaemonEventFrame) -> Result<()> {
    let task_id = checked_task_storage_id(root, &frame.task_id)?;
    append_task_event_for(root, &task_id, frame)
}

/// Appends one event for an already validated task storage identifier.
///
/// The exact identifier must already exist in the durable task registry. This
/// admission check and the append are serialized by the task-store writer
/// lease so a first event cannot create an orphan or adopt an aliasing entry.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] if `task_id` has not been
/// durably admitted, or [`DaemonCoreError::InvalidTaskEventFrame`] if it does
/// not exactly match `frame.task_id`. Returns
/// [`DaemonCoreError::AuthorityJsonLimitExceeded`] if the encoded frame
/// exceeds structural or line-size budgets. Returns [`DaemonCoreError::Io`]
/// for capability, lock, append, durability, or unlock failures.
pub fn append_task_event_for(
    root: &Path,
    task_id: &TaskStorageId,
    frame: &DaemonEventFrame,
) -> Result<()> {
    let path = task_event_log_path(root, task_id);
    let bytes = encode_task_event_frame(root, task_id, frame)?;

    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    let file_name = event_log_file_name(task_id);
    with_registered_task_storage_id(root, task_id, || {
        #[cfg(unix)]
        {
            let events = open_task_events_capability_for_write(root)?;
            validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
            let file = events
                .open_append_file(OsStr::new(&file_name))
                .map_err(|source| {
                    DaemonCoreError::io("failed to open anchored task event log", &path, source)
                })?;
            // Re-enumerate before writing so a case-folding alias raced between
            // the first scan and open cannot receive event bytes.
            validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
            append_locked_task_event(file, &path, task_id, frame, &bytes)
        }
        #[cfg(not(unix))]
        {
            let dir = task_events_dir(root);
            ensure_portable_real_directory(&dir)?;
            validate_portable_event_namespace_aliases(&dir, &file_name, &path)?;
            validate_portable_event_file_type(&path)?;
            let file = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(&path)
                .map_err(|source| {
                    DaemonCoreError::io("failed to open portable task event log", &path, source)
                })?;
            validate_portable_event_namespace_aliases(&dir, &file_name, &path)?;
            append_locked_task_event(file, &path, task_id, frame, &bytes)
        }
    })
}

fn encode_task_event_frame(
    root: &Path,
    task_id: &TaskStorageId,
    frame: &DaemonEventFrame,
) -> Result<Vec<u8>> {
    let path = task_event_log_path(root, task_id);
    if frame.task_id != task_id.as_str() {
        return Err(DaemonCoreError::InvalidTaskEventFrame {
            path,
            message: format!(
                "event frame identifier {:?} does not match admitted identifier {:?}",
                frame.task_id,
                task_id.as_str()
            ),
        });
    }
    let mut bytes = serde_json::to_vec(frame).map_err(|source| {
        DaemonCoreError::json("failed to encode task event for", &path, source)
    })?;
    validate_task_event_frame_bytes(&path, &bytes)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn decode_complete_task_event_frame(
    path: &Path,
    task_id: &TaskStorageId,
    offset: u64,
    encoded: &[u8],
) -> Result<DaemonEventFrame> {
    let encoded = encoded.strip_suffix(b"\r").unwrap_or(encoded);
    if encoded.len() > MAX_TASK_EVENT_LINE_BYTES {
        return Err(task_event_limit_error(
            path,
            "event-line bytes",
            encoded.len() as u64,
            MAX_TASK_EVENT_LINE_BYTES as u64,
        ));
    }
    validate_authority_json(encoded, AuthorityJsonProfile::TaskEventFrame).map_err(|error| {
        match error {
            AuthorityJsonError::Json(source) => DaemonCoreError::InvalidTaskEventFrame {
                path: path.to_path_buf(),
                message: format!("malformed task event JSON at byte {offset}: {source}"),
            },
            error @ AuthorityJsonError::Limit { .. } => map_authority_json_error(
                path,
                AuthorityJsonProfile::TaskEventFrame,
                "failed to validate task event frame from",
                error,
            ),
        }
    })?;
    let frame = serde_json::from_slice::<DaemonEventFrame>(encoded).map_err(|source| {
        DaemonCoreError::InvalidTaskEventFrame {
            path: path.to_path_buf(),
            message: format!("failed to decode task event frame at byte {offset}: {source}"),
        }
    })?;
    if frame.task_id != task_id.as_str() {
        return Err(DaemonCoreError::InvalidTaskEventFrame {
            path: path.to_path_buf(),
            message: format!(
                "task event at byte {offset} belongs to {:?}, expected {:?}",
                frame.task_id,
                task_id.as_str()
            ),
        });
    }
    if frame.seq == 0 {
        return Err(DaemonCoreError::InvalidTaskEventFrame {
            path: path.to_path_buf(),
            message: format!("task event sequence at byte {offset} must be greater than zero"),
        });
    }
    Ok(frame)
}

fn next_task_event_sequence(path: &Path, current: Option<u64>) -> Result<u64> {
    current
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| DaemonCoreError::InvalidTaskEventFrame {
            path: path.to_path_buf(),
            message: "task event sequence is exhausted at u64::MAX".to_string(),
        })
}

/// Loads all complete, valid event frames for one task.
///
/// This compatibility API is bounded by [`MAX_TASK_EVENT_LOAD_BYTES`] and
/// [`MAX_TASK_EVENT_LOAD_FRAMES`]. Call
/// [`load_task_events_from_offset`] directly for larger logs.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskEventFrame`] when any complete frame
/// is malformed, semantically invalid, cross-task, or non-contiguous. Returns
/// [`DaemonCoreError::AuthorityJsonLimitExceeded`] if the whole-log, per-line,
/// or structural budgets are exceeded, or [`DaemonCoreError::Io`] if the event
/// log cannot be safely opened, locked, inspected, read, sought, or unlocked.
pub fn load_task_events(root: &Path, task_id: &str) -> Result<Vec<DaemonEventFrame>> {
    let task_id = checked_task_storage_id(root, task_id)?;
    let path = task_event_log_path(root, &task_id);
    let log_len = task_event_log_len(root, task_id.as_str())?;
    if log_len > MAX_TASK_EVENT_LOAD_BYTES as u64 {
        return Err(task_event_limit_error(
            &path,
            "whole-log bytes",
            log_len,
            MAX_TASK_EVENT_LOAD_BYTES as u64,
        ));
    }
    let mut events = Vec::new();
    let mut offset = 0_u64;
    loop {
        let outcome = load_task_event_page(root, &task_id, offset)?;
        if outcome.log_len > MAX_TASK_EVENT_LOAD_BYTES as u64 {
            return Err(task_event_limit_error(
                &path,
                "whole-log bytes",
                outcome.log_len,
                MAX_TASK_EVENT_LOAD_BYTES as u64,
            ));
        }
        let next_count = events.len().saturating_add(outcome.read.events.len());
        if next_count > MAX_TASK_EVENT_LOAD_FRAMES {
            return Err(task_event_limit_error(
                &path,
                "whole-log decoded frames",
                next_count as u64,
                MAX_TASK_EVENT_LOAD_FRAMES as u64,
            ));
        }
        let next_offset = outcome.read.next_offset;
        events.extend(outcome.read.events);
        if outcome.at_end || next_offset <= offset {
            break;
        }
        offset = next_offset;
    }
    Ok(events)
}

/// Returns the current byte length of a task's event log.
///
/// A missing log has length zero.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] if metadata for an existing log cannot be
/// read.
pub fn task_event_log_len(root: &Path, task_id: &str) -> Result<u64> {
    let task_id = checked_task_storage_id(root, task_id)?;
    let path = task_event_log_path(root, &task_id);
    let Some(file) = open_task_event_file_for_read(root, &task_id, &path)? else {
        return Ok(0);
    };
    FileExt::lock_shared(&file)
        .map_err(|source| DaemonCoreError::io("failed to lock task event log", &path, source))?;
    let result = file
        .metadata()
        .map(|metadata| metadata.len())
        .map_err(|source| DaemonCoreError::io("failed to inspect task event log", &path, source));
    let unlock = FileExt::unlock(&file)
        .map_err(|source| DaemonCoreError::io("failed to unlock task event log", &path, source));
    match (result, unlock) {
        (Ok(len), Ok(())) => Ok(len),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Loads complete, valid event frames beginning at a byte offset.
///
/// The offset is clamped to the current log length. The bounded predecessor
/// frame and every returned complete frame are decoded strictly to preserve
/// sequence continuity across page boundaries. Daemon startup separately
/// streams each complete log from byte zero before publishing readiness. A
/// trailing partial line is left unread so a caller can retry it after the
/// append completes.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskEventFrame`] when any complete frame
/// is malformed, semantically invalid, cross-task, or non-contiguous. Returns
/// [`DaemonCoreError::AuthorityJsonLimitExceeded`] for an excessive frame, or
/// [`DaemonCoreError::Io`] if the event log cannot be opened, locked,
/// inspected, sought, read, or unlocked.
pub fn load_task_events_from_offset(
    root: &Path,
    task_id: &str,
    offset: u64,
) -> Result<TaskEventLogRead> {
    let task_id = checked_task_storage_id(root, task_id)?;
    Ok(load_task_event_page(root, &task_id, offset)?.read)
}

struct TaskEventPageOutcome {
    read: TaskEventLogRead,
    at_end: bool,
    log_len: u64,
}

fn load_task_event_page(
    root: &Path,
    task_id: &TaskStorageId,
    offset: u64,
) -> Result<TaskEventPageOutcome> {
    let path = task_event_log_path(root, task_id);
    let Some(mut file) = open_task_event_file_for_read(root, task_id, &path)? else {
        return Ok(TaskEventPageOutcome {
            read: TaskEventLogRead {
                events: Vec::new(),
                next_offset: 0,
            },
            at_end: true,
            log_len: 0,
        });
    };
    FileExt::lock_shared(&file)
        .map_err(|source| DaemonCoreError::io("failed to lock task event log", &path, source))?;
    let result = read_locked_task_event_page(&mut file, &path, task_id, offset);
    let unlock = FileExt::unlock(&file)
        .map_err(|source| DaemonCoreError::io("failed to unlock task event log", &path, source));
    match (result, unlock) {
        (Ok(page), Ok(())) => Ok(page),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn read_locked_task_event_page(
    file: &mut fs::File,
    path: &Path,
    task_id: &TaskStorageId,
    offset: u64,
) -> Result<TaskEventPageOutcome> {
    let len = file
        .metadata()
        .map_err(|source| DaemonCoreError::io("failed to inspect task event log", path, source))?
        .len();
    let start = offset.min(len);
    let mut previous_sequence = task_event_sequence_before_offset(file, path, task_id, start)?;
    file.seek(SeekFrom::Start(start))
        .map_err(|source| DaemonCoreError::io("failed to seek task event log", path, source))?;
    let mut reader = BufReader::new(file);
    let mut next_offset = start;
    let mut page_bytes = 0_usize;
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut at_end = false;
    loop {
        if events.len() >= MAX_TASK_EVENT_PAGE_FRAMES {
            break;
        }
        let remaining = MAX_TASK_EVENT_PAGE_BYTES.saturating_sub(page_bytes);
        if remaining == 0
            || (page_bytes > 0 && remaining < MAX_TASK_EVENT_LINE_BYTES.saturating_add(1))
        {
            break;
        }
        line.clear();
        let read_limit = remaining.min(MAX_TASK_EVENT_LINE_BYTES.saturating_add(1));
        let read = reader
            .by_ref()
            .take(read_limit as u64)
            .read_until(b'\n', &mut line)
            .map_err(|source| DaemonCoreError::io("failed to read task event log", path, source))?;
        if read == 0 {
            at_end = true;
            break;
        }
        if !line.ends_with(b"\n") {
            if line.len() > MAX_TASK_EVENT_LINE_BYTES {
                return Err(task_event_limit_error(
                    path,
                    "event-line bytes",
                    line.len() as u64,
                    MAX_TASK_EVENT_LINE_BYTES as u64,
                ));
            }
            // A short non-newline-terminated read is the current trailing
            // partial frame. It remains unread from the caller's offset.
            at_end = read < read_limit;
            break;
        }
        let encoded_len = line.len().saturating_sub(1);
        if encoded_len > MAX_TASK_EVENT_LINE_BYTES {
            return Err(task_event_limit_error(
                path,
                "event-line bytes",
                encoded_len as u64,
                MAX_TASK_EVENT_LINE_BYTES as u64,
            ));
        }
        page_bytes = page_bytes.saturating_add(read);
        next_offset = next_offset.saturating_add(read as u64);
        let frame = decode_complete_task_event_frame(
            path,
            task_id,
            next_offset.saturating_sub(read as u64),
            &line[..encoded_len],
        )?;
        let expected = next_task_event_sequence(path, previous_sequence)?;
        if frame.seq != expected {
            return Err(DaemonCoreError::InvalidTaskEventFrame {
                path: path.to_path_buf(),
                message: format!(
                    "task event sequence is not contiguous at byte {}: expected {expected}, found {}",
                    next_offset.saturating_sub(read as u64),
                    frame.seq
                ),
            });
        }
        previous_sequence = Some(frame.seq);
        events.push(frame);
    }
    Ok(TaskEventPageOutcome {
        read: TaskEventLogRead {
            events,
            next_offset,
        },
        at_end,
        log_len: len,
    })
}

fn task_event_sequence_before_offset(
    file: &mut fs::File,
    path: &Path,
    task_id: &TaskStorageId,
    offset: u64,
) -> Result<Option<u64>> {
    if offset == 0 {
        return Ok(None);
    }
    let window_bytes = (MAX_TASK_EVENT_LINE_BYTES as u64).saturating_add(3);
    let window_start = offset.saturating_sub(window_bytes);
    file.seek(SeekFrom::Start(window_start)).map_err(|source| {
        DaemonCoreError::io("failed to seek task event predecessor", path, source)
    })?;
    let expected_len = usize::try_from(offset - window_start).map_err(|_| {
        DaemonCoreError::io(
            "task event predecessor window does not fit memory",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bounded predecessor window does not fit usize",
            ),
        )
    })?;
    let mut window = Vec::new();
    window.try_reserve_exact(expected_len).map_err(|source| {
        DaemonCoreError::io(
            "failed to reserve task event predecessor window",
            path,
            std::io::Error::new(std::io::ErrorKind::OutOfMemory, source),
        )
    })?;
    file.take(expected_len as u64)
        .read_to_end(&mut window)
        .map_err(|source| {
            DaemonCoreError::io("failed to read task event predecessor", path, source)
        })?;
    if window.len() != expected_len || !window.ends_with(b"\n") {
        return Err(DaemonCoreError::InvalidTaskEventFrame {
            path: path.to_path_buf(),
            message: format!("task event replay offset {offset} is not a complete-frame boundary"),
        });
    }
    let predecessor_end = window.len() - 1;
    let predecessor_start = window[..predecessor_end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if window_start != 0 && predecessor_start == 0 {
        return Err(task_event_limit_error(
            path,
            "event-line bytes",
            window_bytes,
            MAX_TASK_EVENT_LINE_BYTES as u64,
        ));
    }
    let absolute_start = window_start + predecessor_start as u64;
    let frame = decode_complete_task_event_frame(
        path,
        task_id,
        absolute_start,
        &window[predecessor_start..predecessor_end],
    )?;
    Ok(Some(frame.seq))
}

fn checked_task_storage_id(root: &Path, task_id: &str) -> Result<TaskStorageId> {
    TaskStorageId::try_from(task_id).map_err(|error| {
        DaemonCoreError::InvalidTaskStorageIdentifier {
            path: task_artifacts_dir(root).join(task_id),
            message: error.to_string(),
        }
    })
}

fn event_log_file_name(task_id: &TaskStorageId) -> String {
    format!("{}{TASK_EVENT_LOG_SUFFIX}", task_id.as_str())
}

fn require_registered_task_storage_id(
    registry: &TaskRegistry,
    registry_path: &Path,
    task_id: &TaskStorageId,
) -> Result<()> {
    let admitted = registry
        .tasks
        .get(task_id.as_str())
        .is_some_and(|record| record.task_id == task_id.as_str());
    if admitted {
        return Ok(());
    }
    Err(DaemonCoreError::InvalidTaskRegistry {
        path: registry_path.to_path_buf(),
        message: format!(
            "task {:?} must be durably admitted before its first event append",
            task_id.as_str()
        ),
    })
}

fn with_registered_task_storage_id<T>(
    root: &Path,
    task_id: &TaskStorageId,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_registered_task_storage_id_lock(root, task_id, RegistryLockMode::Shared, operation)
}

fn with_exclusively_registered_task_storage_id<T>(
    root: &Path,
    task_id: &TaskStorageId,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_registered_task_storage_id_lock(root, task_id, RegistryLockMode::Exclusive, operation)
}

fn with_registered_task_storage_id_lock<T>(
    root: &Path,
    task_id: &TaskStorageId,
    mode: RegistryLockMode,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let path = task_registry_path(root);
    #[cfg(unix)]
    {
        with_anchored_task_registry_lock(
            root,
            mode,
            || Ok(()),
            |daemon| {
                let registry = match daemon
                    .read_file_limited(OsStr::new(TASK_REGISTRY_FILE_NAME), MAX_TASK_REGISTRY_BYTES)
                {
                    Ok(raw) => decode_task_registry(&path, &raw)?,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                        TaskRegistry::default()
                    }
                    Err(source) => return Err(task_registry_read_error(daemon, &path, source)),
                };
                require_registered_task_storage_id(&registry, &path, task_id)?;
                operation()
            },
        )
    }
    #[cfg(not(unix))]
    {
        with_registry_lock(root, &path, mode, || {
            let registry = match read_task_registry_portable(&path)? {
                Some(raw) => decode_task_registry(&path, &raw)?,
                None => TaskRegistry::default(),
            };
            require_registered_task_storage_id(&registry, &path, task_id)?;
            operation()
        })
    }
}

#[cfg(unix)]
fn remove_failed_initial_task_storage_anchored(root: &Path, task_id: &TaskStorageId) -> Result<()> {
    let workspace = CapabilityDir::open_workspace(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace for failed initial task cleanup",
            root,
            source,
        )
    })?;
    let Some(state) = open_optional_cleanup_directory(&workspace, OsStr::new(".packet28"))? else {
        return Ok(());
    };

    if let Some(artifacts) =
        open_optional_cleanup_directory(&state, OsStr::new(TASK_ARTIFACTS_DIR_NAME))?
    {
        remove_optional_cleanup_entry(&artifacts, OsStr::new(task_id.as_str()))?;
    }

    let Some(daemon) = open_optional_cleanup_directory(&state, OsStr::new("daemon"))? else {
        return Ok(());
    };
    let Some(events) = open_optional_cleanup_directory(&daemon, OsStr::new(TASK_EVENTS_DIR_NAME))?
    else {
        return Ok(());
    };
    let event_name = event_log_file_name(task_id);
    remove_optional_cleanup_entry(&events, OsStr::new(&event_name))
}

#[cfg(unix)]
fn open_optional_cleanup_directory(
    parent: &CapabilityDir,
    name: &OsStr,
) -> Result<Option<CapabilityDir>> {
    let path = parent.display_path().join(name);
    let directory = match parent.open_dir(name) {
        Ok(directory) => directory,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open directory for failed initial task cleanup",
                &path,
                source,
            ));
        }
    };
    ensure_capability_same_device(
        parent,
        &directory,
        &path,
        "failed initial task cleanup crossed a filesystem boundary",
    )?;
    Ok(Some(directory))
}

#[cfg(unix)]
fn remove_optional_cleanup_entry(parent: &CapabilityDir, name: &OsStr) -> Result<()> {
    let path = parent.display_path().join(name);
    let Some(identity) = parent.entry_identity(name).map_err(|source| {
        DaemonCoreError::io(
            "failed to inspect failed initial task storage",
            &path,
            source,
        )
    })?
    else {
        return Ok(());
    };
    parent
        .remove_tree_entry_verified(name, identity)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to remove failed initial task storage",
                &path,
                source,
            )
        })
}

#[cfg(not(unix))]
fn remove_failed_initial_task_storage_portable(root: &Path, task_id: &TaskStorageId) -> Result<()> {
    remove_portable_cleanup_entry(
        &task_artifact_dir(root, task_id),
        true,
        "failed to remove failed initial task artifacts",
    )?;
    remove_portable_cleanup_entry(
        &task_event_log_path(root, task_id),
        false,
        "failed to remove failed initial task event log",
    )
}

#[cfg(not(unix))]
fn remove_portable_cleanup_entry(
    path: &Path,
    expect_directory: bool,
    operation: &'static str,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(DaemonCoreError::io(operation, path, source)),
    };
    if metadata.file_type().is_symlink()
        || (expect_directory && !metadata.is_dir())
        || (!expect_directory && !metadata.is_file())
    {
        return Err(DaemonCoreError::io(
            operation,
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "failed initial task storage entry has an unsupported file type",
            ),
        ));
    }
    if expect_directory {
        fs::remove_dir_all(path).map_err(|source| DaemonCoreError::io(operation, path, source))
    } else {
        fs::remove_file(path).map_err(|source| DaemonCoreError::io(operation, path, source))
    }
}

fn validate_task_event_frame_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_TASK_EVENT_LINE_BYTES {
        return Err(task_event_limit_error(
            path,
            "event-line bytes",
            bytes.len() as u64,
            MAX_TASK_EVENT_LINE_BYTES as u64,
        ));
    }
    validate_authority_json(bytes, AuthorityJsonProfile::TaskEventFrame).map_err(|error| {
        map_authority_json_error(
            path,
            AuthorityJsonProfile::TaskEventFrame,
            "failed to validate encoded task event for",
            error,
        )
    })
}

fn task_event_limit_error(
    path: &Path,
    resource: &'static str,
    observed: u64,
    max: u64,
) -> DaemonCoreError {
    DaemonCoreError::AuthorityJsonLimitExceeded {
        path: path.to_path_buf(),
        authority: AuthorityJsonProfile::TaskEventFrame.authority(),
        resource,
        observed,
        max,
    }
}

fn append_locked_task_event(
    mut file: fs::File,
    path: &Path,
    task_id: &TaskStorageId,
    frame: &DaemonEventFrame,
    bytes: &[u8],
) -> Result<()> {
    FileExt::lock_exclusive(&file)
        .map_err(|source| DaemonCoreError::io("failed to lock task event log", path, source))?;
    let result = (|| {
        let inspection = event_tail::inspect_locked_task_event_tail(&mut file, path, task_id)?;
        let expected =
            next_task_event_sequence(path, inspection.tail.as_ref().map(|tail| tail.seq))?;
        if frame.seq != expected {
            return Err(DaemonCoreError::InvalidTaskEventFrame {
                path: path.to_path_buf(),
                message: format!(
                    "task event append is not contiguous: expected {expected}, found {}",
                    frame.seq
                ),
            });
        }
        if inspection.has_partial_suffix {
            file.set_len(inspection.complete_len).map_err(|source| {
                DaemonCoreError::io("failed to truncate partial task event tail", path, source)
            })?;
            sync_task_event_file(&file, path)?;
        }
        file.write_all(bytes).map_err(|source| {
            DaemonCoreError::io("failed to append task event log", path, source)
        })?;
        sync_task_event_file(&file, path)
    })();
    let unlock = FileExt::unlock(&file)
        .map_err(|source| DaemonCoreError::io("failed to unlock task event log", path, source));
    match (result, unlock) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

fn sync_task_event_file(file: &fs::File, path: &Path) -> Result<()> {
    #[cfg(test)]
    if INJECT_TASK_EVENT_SYNC_FAILURE_FOR.with(|configured| {
        let matches = configured
            .borrow()
            .as_ref()
            .is_some_and(|configured| configured == path);
        if matches {
            configured.replace(None);
        }
        matches
    }) {
        return Err(DaemonCoreError::io(
            "failed to synchronize task event log",
            path,
            std::io::Error::other("injected task-event data sync failure"),
        ));
    }
    #[cfg(unix)]
    let result = sync_file_barrier(file);
    #[cfg(not(unix))]
    let result = file.sync_all();
    result
        .map_err(|source| DaemonCoreError::io("failed to synchronize task event log", path, source))
}

#[cfg(test)]
fn inject_task_event_sync_failure_once(path: &Path) {
    INJECT_TASK_EVENT_SYNC_FAILURE_FOR.with(|configured| {
        *configured.borrow_mut() = Some(path.to_path_buf());
    });
}

#[cfg(unix)]
fn open_task_events_capability_for_write(root: &Path) -> Result<CapabilityDir> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for task event append",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for task event append",
            &canonical_root,
            source,
        )
    })?;
    let state = workspace
        .ensure_dir_open(OsStr::new(".packet28"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state for task event append",
                canonical_root.join(".packet28"),
                source,
            )
        })?;
    ensure_capability_same_device(
        &workspace,
        &state,
        canonical_root.join(".packet28"),
        "Packet28 state for task event append is on another filesystem",
    )?;
    let daemon = state
        .ensure_dir_open(OsStr::new("daemon"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open daemon state for task event append",
                daemon_dir(&canonical_root),
                source,
            )
        })?;
    ensure_capability_same_device(
        &state,
        &daemon,
        daemon_dir(&canonical_root),
        "daemon state for task event append is on another filesystem",
    )?;
    let events = daemon
        .ensure_dir_open(OsStr::new("tasks"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open task events for append",
                task_events_dir(&canonical_root),
                source,
            )
        })?;
    ensure_capability_same_device(
        &daemon,
        &events,
        task_events_dir(&canonical_root),
        "task events for append are on another filesystem",
    )?;
    Ok(events)
}

#[cfg(unix)]
fn open_task_events_capability_for_read(root: &Path) -> Result<Option<CapabilityDir>> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for task event read",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for task event read",
            &canonical_root,
            source,
        )
    })?;
    let state = match workspace.open_dir(OsStr::new(".packet28")) {
        Ok(state) => state,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open Packet28 state for task event read",
                canonical_root.join(".packet28"),
                source,
            ));
        }
    };
    ensure_capability_same_device(
        &workspace,
        &state,
        canonical_root.join(".packet28"),
        "Packet28 state for task event read is on another filesystem",
    )?;
    let daemon = match state.open_dir(OsStr::new("daemon")) {
        Ok(daemon) => daemon,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open daemon state for task event read",
                daemon_dir(&canonical_root),
                source,
            ));
        }
    };
    ensure_capability_same_device(
        &state,
        &daemon,
        daemon_dir(&canonical_root),
        "daemon state for task event read is on another filesystem",
    )?;
    let events = match daemon.open_dir(OsStr::new("tasks")) {
        Ok(events) => events,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open task events for read",
                task_events_dir(&canonical_root),
                source,
            ));
        }
    };
    ensure_capability_same_device(
        &daemon,
        &events,
        task_events_dir(&canonical_root),
        "task events for read are on another filesystem",
    )?;
    Ok(Some(events))
}

#[cfg(unix)]
fn validate_anchored_event_namespace_aliases(
    events: &CapabilityDir,
    expected_name: &str,
    path: &Path,
) -> Result<()> {
    let expected_alias = task_storage_key_alias_class(expected_name);
    let entries = events
        .entries_bounded(MAX_TASK_REGISTRY_RECORDS)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate anchored task event namespace",
                events.display_path(),
                source,
            )
        })?;
    for entry in entries {
        let Some(actual_name) = entry.to_str() else {
            continue;
        };
        if task_storage_key_alias_class(actual_name) == expected_alias
            && actual_name != expected_name
        {
            return Err(DaemonCoreError::InvalidTaskStorageIdentifier {
                path: path.to_path_buf(),
                message: format!(
                    "managed event entry {actual_name:?} aliases canonical spelling {expected_name:?}"
                ),
            });
        }
    }
    Ok(())
}

fn open_task_event_file_for_read(
    root: &Path,
    task_id: &TaskStorageId,
    path: &Path,
) -> Result<Option<fs::File>> {
    let file_name = event_log_file_name(task_id);
    #[cfg(unix)]
    {
        let Some(events) = open_task_events_capability_for_read(root)? else {
            return Ok(None);
        };
        validate_anchored_event_namespace_aliases(&events, &file_name, path)?;
        match events.open_read_file(OsStr::new(&file_name)) {
            Ok(file) => Ok(Some(file)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DaemonCoreError::io(
                "failed to open anchored task event log",
                path,
                source,
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let dir = task_events_dir(root);
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(DaemonCoreError::io(
                    "failed to authenticate portable task event directory",
                    &dir,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "task event namespace is not a real directory",
                    ),
                ));
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(DaemonCoreError::io(
                    "failed to inspect portable task event directory",
                    &dir,
                    source,
                ));
            }
        }
        validate_portable_event_namespace_aliases(&dir, &file_name, path)?;
        validate_portable_event_file_type(path)?;
        match fs::File::open(path) {
            Ok(file) => Ok(Some(file)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DaemonCoreError::io(
                "failed to open portable task event log",
                path,
                source,
            )),
        }
    }
}

#[cfg(not(unix))]
fn ensure_portable_real_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(DaemonCoreError::io(
            "failed to authenticate portable task event directory",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "task event namespace is not a real directory",
            ),
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| {
                DaemonCoreError::io(
                    "failed to create portable task event directory",
                    path,
                    source,
                )
            }),
        Err(source) => Err(DaemonCoreError::io(
            "failed to inspect portable task event directory",
            path,
            source,
        )),
    }
}

#[cfg(not(unix))]
fn validate_portable_event_namespace_aliases(
    directory: &Path,
    expected_name: &str,
    path: &Path,
) -> Result<()> {
    let expected_alias = task_storage_key_alias_class(expected_name);
    let mut count = 0_usize;
    for entry in fs::read_dir(directory).map_err(|source| {
        DaemonCoreError::io(
            "failed to enumerate portable task event namespace",
            directory,
            source,
        )
    })? {
        count = count.saturating_add(1);
        if count > MAX_TASK_REGISTRY_RECORDS {
            return Err(DaemonCoreError::io(
                "failed to enumerate portable task event namespace",
                directory,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "task event namespace exceeds the supported entry bound",
                ),
            ));
        }
        let entry = entry.map_err(|source| {
            DaemonCoreError::io(
                "failed to enumerate portable task event namespace",
                directory,
                source,
            )
        })?;
        let Some(actual_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if task_storage_key_alias_class(&actual_name) == expected_alias
            && actual_name != expected_name
        {
            return Err(DaemonCoreError::InvalidTaskStorageIdentifier {
                path: path.to_path_buf(),
                message: format!(
                    "managed event entry {actual_name:?} aliases canonical spelling {expected_name:?}"
                ),
            });
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_portable_event_file_type(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(DaemonCoreError::io(
            "failed to authenticate portable task event log",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "task event log is not a real regular file",
            ),
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonCoreError::io(
            "failed to inspect portable task event log",
            path,
            source,
        )),
    }
}

/// Returns the current Unix timestamp in whole seconds.
///
/// If the system clock is before the Unix epoch, this returns zero.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(any(not(unix), test))]
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = atomic_temp_path(path);
    let mut file = fs::File::create(&temp_path).map_err(|source| {
        DaemonCoreError::io("failed to create atomic temporary file", &temp_path, source)
    })?;
    file.write_all(bytes).map_err(|source| {
        DaemonCoreError::io("failed to write atomic temporary file", &temp_path, source)
    })?;
    file.sync_all().map_err(|source| {
        DaemonCoreError::io(
            "failed to synchronize atomic temporary file",
            &temp_path,
            source,
        )
    })?;
    fs::rename(&temp_path, path)
        .map_err(|source| DaemonCoreError::io("failed to atomically replace", path, source))?;
    sync_parent_directory(path)
}

#[cfg(test)]
pub(crate) fn inject_parent_sync_failure_once(path: &Path) {
    INJECT_PARENT_SYNC_FAILURE_FOR.with(|configured| {
        *configured.borrow_mut() = Some(path.to_path_buf());
    });
}

#[cfg(any(not(unix), test))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(test)]
    if INJECT_PARENT_SYNC_FAILURE_FOR.with(|configured| {
        let mut configured = configured.borrow_mut();
        if configured.as_deref() == Some(path) {
            configured.take();
            true
        } else {
            false
        }
    }) {
        return Err(DaemonCoreError::io(
            "failed to synchronize atomic replacement directory",
            path,
            std::io::Error::other("injected parent-directory sync failure"),
        ));
    }

    #[cfg(unix)]
    {
        let parent = path.parent().ok_or_else(|| {
            DaemonCoreError::io(
                "failed to resolve atomic replacement directory",
                path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "atomic replacement path has no parent directory",
                ),
            )
        })?;
        let directory = fs::File::open(parent).map_err(|source| {
            DaemonCoreError::io(
                "failed to open atomic replacement directory",
                parent,
                source,
            )
        })?;
        directory.sync_all().map_err(|source| {
            DaemonCoreError::io(
                "failed to synchronize atomic replacement directory",
                parent,
                source,
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RegistryLockMode {
    Shared,
    Exclusive,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum AnchoredFileLockMode {
    Shared,
    Exclusive,
}

/// Why an anchored lock could not finish its authenticated lock scope.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum AnchoredFileLockFinishError {
    /// The canonical filename stopped naming the locked descriptor.
    Attachment(std::io::Error),
    /// The descriptor remained attached, but advisory unlock failed.
    Unlock(std::io::Error),
}

/// Advisory lock whose canonical filename remains bound to its descriptor.
///
/// Construction reauthenticates immediately after `flock`; [`Self::finish`]
/// reauthenticates again before unlock. Callers that completed a durable
/// mutation must map an `Attachment` finish error to an outcome-uncertain
/// error rather than imply rollback.
#[cfg(unix)]
pub(crate) struct AnchoredFileLock<'a> {
    parent: &'a CapabilityDir,
    name: OsString,
    path: PathBuf,
    file: fs::File,
    locked: bool,
}

#[cfg(unix)]
impl<'a> AnchoredFileLock<'a> {
    pub(crate) fn acquire(
        parent: &'a CapabilityDir,
        name: &OsStr,
        path: PathBuf,
        mode: AnchoredFileLockMode,
    ) -> std::io::Result<Self> {
        let file = parent.open_lock_file(name)?;
        Self::lock_open_file(parent, name, path, file, mode)
    }

    fn lock_open_file(
        parent: &'a CapabilityDir,
        name: &OsStr,
        path: PathBuf,
        file: fs::File,
        mode: AnchoredFileLockMode,
    ) -> std::io::Result<Self> {
        match mode {
            AnchoredFileLockMode::Shared => FileExt::lock_shared(&file)?,
            AnchoredFileLockMode::Exclusive => FileExt::lock_exclusive(&file)?,
        }
        let guard = Self {
            parent,
            name: name.to_os_string(),
            path,
            file,
            locked: true,
        };
        guard.validate_attachment()?;
        Ok(guard)
    }

    pub(crate) fn lock_existing(
        parent: &'a CapabilityDir,
        name: &OsStr,
        path: PathBuf,
        file: fs::File,
        mode: AnchoredFileLockMode,
    ) -> std::io::Result<Self> {
        Self::lock_open_file(parent, name, path, file, mode)
    }

    pub(crate) fn validate_attachment(&self) -> std::io::Result<()> {
        self.validate_named_attachment(&self.name)
    }

    fn validate_named_attachment(&self, name: &OsStr) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = self.file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "locked descriptor is not a single-link regular file: {}",
                    self.path.display()
                ),
            ));
        }
        self.parent.authenticate_regular_file_with_link_count(
            name,
            crate::retention::FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            1,
        )
    }

    fn file(&self) -> &fs::File {
        &self.file
    }

    fn file_mut(&mut self) -> &mut fs::File {
        &mut self.file
    }

    pub(crate) fn finish(mut self) -> std::result::Result<(), AnchoredFileLockFinishError> {
        let attachment = self.validate_attachment();
        let unlock = FileExt::unlock(&self.file);
        self.locked = false;
        match (attachment, unlock) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), _) => Err(AnchoredFileLockFinishError::Attachment(source)),
            (Ok(()), Err(source)) => Err(AnchoredFileLockFinishError::Unlock(source)),
        }
    }

    pub(crate) fn finish_renamed(
        mut self,
        destination_name: &OsStr,
    ) -> std::result::Result<(), AnchoredFileLockFinishError> {
        let attachment = self.validate_named_attachment(destination_name);
        let unlock = FileExt::unlock(&self.file);
        self.locked = false;
        match (attachment, unlock) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), _) => Err(AnchoredFileLockFinishError::Attachment(source)),
            (Ok(()), Err(source)) => Err(AnchoredFileLockFinishError::Unlock(source)),
        }
    }
}

#[cfg(unix)]
impl Drop for AnchoredFileLock<'_> {
    fn drop(&mut self) {
        if self.locked {
            let _ = FileExt::unlock(&self.file);
            self.locked = false;
        }
    }
}

#[cfg(unix)]
fn with_anchored_task_registry_lock<T>(
    root: &Path,
    mode: RegistryLockMode,
    after_daemon_open: impl FnOnce() -> Result<()>,
    operation: impl FnOnce(&CapabilityDir) -> Result<T>,
) -> Result<T> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        DaemonCoreError::io(
            "failed to resolve workspace for task registry",
            root,
            source,
        )
    })?;
    let workspace = CapabilityDir::open(&canonical_root).map_err(|source| {
        DaemonCoreError::io(
            "failed to open workspace capability for task registry",
            &canonical_root,
            source,
        )
    })?;
    let state = workspace
        .ensure_dir_open(OsStr::new(".packet28"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open Packet28 state capability for task registry",
                canonical_root.join(".packet28"),
                source,
            )
        })?;
    let daemon = state
        .ensure_dir_open(OsStr::new("daemon"), 0o755)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open daemon capability for task registry",
                canonical_root.join(".packet28").join("daemon"),
                source,
            )
        })?;
    let lock_path = daemon.display_path().join(TASK_REGISTRY_LOCK_FILE_NAME);
    let lock = AnchoredFileLock::acquire(
        &daemon,
        OsStr::new(TASK_REGISTRY_LOCK_FILE_NAME),
        lock_path.clone(),
        match mode {
            RegistryLockMode::Shared => AnchoredFileLockMode::Shared,
            RegistryLockMode::Exclusive => AnchoredFileLockMode::Exclusive,
        },
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open, acquire, or authenticate anchored task registry lock",
            &lock_path,
            source,
        )
    })?;
    let result = after_daemon_open()
        .and_then(|()| {
            lock.validate_attachment().map_err(|source| {
                DaemonCoreError::io(
                    "anchored task registry lock detached before operation",
                    &lock_path,
                    source,
                )
            })
        })
        .and_then(|()| operation(&daemon));
    let finish = lock.finish();
    match (result, finish) {
        (Ok(value), Ok(())) => Ok(value),
        (_, Err(AnchoredFileLockFinishError::Attachment(source)))
            if matches!(mode, RegistryLockMode::Exclusive) =>
        {
            Err(DaemonCoreError::StorageMutationAuthorityLost {
                operation: "task-registry mutation",
                path: task_registry_path(root),
                source,
            })
        }
        (Err(error), _) => Err(error),
        (Ok(_), Err(AnchoredFileLockFinishError::Attachment(source))) => Err(DaemonCoreError::io(
            "anchored task registry lock detached during read",
            &lock_path,
            source,
        )),
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock anchored task registry",
            &lock_path,
            source,
        )),
    }
}

#[cfg(unix)]
fn with_anchored_watch_registry_lock<T>(
    daemon: &CapabilityDir,
    mode: RegistryLockMode,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock_path = daemon.display_path().join(WATCH_REGISTRY_LOCK_FILE_NAME);
    let lock = AnchoredFileLock::acquire(
        daemon,
        OsStr::new(WATCH_REGISTRY_LOCK_FILE_NAME),
        lock_path.clone(),
        match mode {
            RegistryLockMode::Shared => AnchoredFileLockMode::Shared,
            RegistryLockMode::Exclusive => AnchoredFileLockMode::Exclusive,
        },
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to open, acquire, or authenticate anchored watch registry lock",
            &lock_path,
            source,
        )
    })?;
    let result = operation();
    let finish = lock.finish();
    match (result, finish) {
        (Ok(value), Ok(())) => Ok(value),
        (_, Err(AnchoredFileLockFinishError::Attachment(source)))
            if matches!(mode, RegistryLockMode::Exclusive) =>
        {
            Err(DaemonCoreError::StorageMutationAuthorityLost {
                operation: "watch-registry mutation",
                path: daemon.display_path().join(WATCH_REGISTRY_FILE_NAME),
                source,
            })
        }
        (Err(error), _) => Err(error),
        (Ok(_), Err(AnchoredFileLockFinishError::Attachment(source))) => Err(DaemonCoreError::io(
            "anchored watch registry lock detached during read",
            &lock_path,
            source,
        )),
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock anchored watch registry",
            &lock_path,
            source,
        )),
    }
}

#[cfg(any(not(unix), test))]
fn with_registry_lock<T>(
    root: &Path,
    registry_path: &Path,
    mode: RegistryLockMode,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ensure_daemon_dir(root)?;
    let lock_path = registry_lock_path(registry_path);
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| {
            DaemonCoreError::io("failed to open registry lock", &lock_path, source)
        })?;
    match mode {
        RegistryLockMode::Shared => FileExt::lock_shared(&file).map_err(|source| {
            DaemonCoreError::io("failed to acquire shared lock for", registry_path, source)
        })?,
        RegistryLockMode::Exclusive => FileExt::lock_exclusive(&file).map_err(|source| {
            DaemonCoreError::io(
                "failed to acquire exclusive lock for",
                registry_path,
                source,
            )
        })?,
    }

    let result = operation();
    let unlock_result = FileExt::unlock(&file)
        .map_err(|source| DaemonCoreError::io("failed to unlock registry", registry_path, source));
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

#[cfg(any(not(unix), test))]
fn registry_lock_path(registry_path: &Path) -> PathBuf {
    let file_name = registry_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("registry");
    registry_path.with_file_name(format!(".{file_name}.lock"))
}

#[cfg(any(not(unix), test))]
fn atomic_temp_path(path: &Path) -> PathBuf {
    let counter = ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("packet28");
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        counter
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use packet28_daemon_protocol::message::{
        DaemonEvent, DaemonTransportAuth, DAEMON_TRANSPORT_SECRET_BYTES,
    };
    use packet28_daemon_protocol::task::{TaskLifecycle, TaskRecord, WatchRegistration};
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn task_storage_id(task_id: &str) -> TaskStorageId {
        TaskStorageId::try_from(task_id).unwrap()
    }

    fn task_event_path(root: &Path, task_id: &str) -> PathBuf {
        task_event_log_path(root, &task_storage_id(task_id))
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_directory_is_created_with_exact_owner_only_permissions() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempdir().unwrap();
        let socket_dir = root.path().join("socket-parent");

        ensure_private_socket_directory(&socket_dir).unwrap();

        let metadata = fs::symlink_metadata(&socket_dir).unwrap();
        // SAFETY: `geteuid` has no preconditions and does not retain pointers.
        let effective_uid = unsafe { libc::geteuid() };
        assert_eq!(
            (metadata.uid(), metadata.permissions().mode() & 0o777),
            (effective_uid, 0o700)
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_directory_rejects_existing_permissive_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let socket_dir = root.path().join("socket-parent");
        fs::create_dir(&socket_dir).unwrap();
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let error = ensure_private_socket_directory(&socket_dir).unwrap_err();

        assert!(format!("{error:#}").contains("expected mode 700"));
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_directory_rejects_symlink_substitution() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let target = root.path().join("target");
        let socket_dir = root.path().join("socket-parent");
        fs::create_dir(&target).unwrap();
        symlink(&target, &socket_dir).unwrap();

        let error = ensure_private_socket_directory(&socket_dir).unwrap_err();

        assert!(format!("{error:#}").contains("authenticate private daemon socket directory"));
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_directory_rejects_replaceable_parent_ancestry() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let replaceable_parent = root.path().join("replaceable-parent");
        let socket_dir = replaceable_parent.join("socket-parent");
        fs::create_dir(&replaceable_parent).unwrap();
        fs::set_permissions(&replaceable_parent, fs::Permissions::from_mode(0o777)).unwrap();

        let error = ensure_private_socket_directory(&socket_dir).unwrap_err();

        assert!(format!("{error:#}")
            .contains("namespace ancestor permits replacement without safe sticky ownership"));
    }

    #[cfg(unix)]
    #[test]
    fn private_socket_directory_accepts_sticky_shared_parent_ancestry() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let sticky_parent = root.path().join("sticky-parent");
        let socket_dir = sticky_parent.join("socket-parent");
        fs::create_dir(&sticky_parent).unwrap();
        fs::set_permissions(&sticky_parent, fs::Permissions::from_mode(0o1777)).unwrap();

        ensure_private_socket_directory(&socket_dir).unwrap();

        assert!(socket_dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_publication_is_owner_only_and_round_trips_tcp_capability() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x2a; DAEMON_TRANSPORT_SECRET_BYTES]);
        let runtime = DaemonRuntimeInfo {
            pid: 4242,
            socket_path: "tcp://127.0.0.1:4242".to_string(),
            transport_auth: Some(auth.clone()),
            ..DaemonRuntimeInfo::default()
        };

        write_runtime_info(root.path(), &runtime).unwrap();

        let runtime_mode = fs::metadata(runtime_path(root.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let pid_mode = fs::metadata(pid_path(root.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!((runtime_mode, pid_mode), (0o600, 0o600));
        let loaded = read_runtime_info(root.path()).unwrap();
        assert!(loaded
            .transport_auth
            .as_ref()
            .is_some_and(|candidate| auth.authenticates(candidate)));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_publication_replaces_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let target = root.path().join("outside-runtime");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, runtime_path(root.path())).unwrap();
        let runtime = DaemonRuntimeInfo {
            pid: 7,
            socket_path: socket_path(root.path()).to_string_lossy().to_string(),
            ..DaemonRuntimeInfo::default()
        };

        write_runtime_info(root.path(), &runtime).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(fs::symlink_metadata(runtime_path(root.path()))
            .unwrap()
            .file_type()
            .is_file());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_rejects_non_owner_write_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        write_runtime_info(root.path(), &DaemonRuntimeInfo::default()).unwrap();
        fs::set_permissions(runtime_path(root.path()), fs::Permissions::from_mode(0o622)).unwrap();

        let error = read_runtime_info(root.path()).unwrap_err();

        assert!(format!("{error:#}").contains("non-owner write authority"));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_rejects_readable_tcp_capability_but_accepts_legacy_unix_metadata() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x3c; DAEMON_TRANSPORT_SECRET_BYTES]);
        write_runtime_info(
            root.path(),
            &DaemonRuntimeInfo {
                socket_path: "tcp://127.0.0.1:4242".to_string(),
                transport_auth: Some(auth),
                ..DaemonRuntimeInfo::default()
            },
        )
        .unwrap();
        fs::set_permissions(runtime_path(root.path()), fs::Permissions::from_mode(0o644)).unwrap();

        let error = read_runtime_info(root.path()).unwrap_err();

        assert!(format!("{error:#}").contains("non-owner-readable daemon transport capability"));

        write_runtime_info(
            root.path(),
            &DaemonRuntimeInfo {
                socket_path: socket_path(root.path()).to_string_lossy().to_string(),
                ..DaemonRuntimeInfo::default()
            },
        )
        .unwrap();
        fs::set_permissions(runtime_path(root.path()), fs::Permissions::from_mode(0o644)).unwrap();

        assert!(read_runtime_info(root.path()).is_ok());
    }

    #[cfg(unix)]
    fn replace_locked_path(path: &Path, detached: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;

        fs::rename(path, detached)?;
        let replacement = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        replacement.sync_all()
    }

    fn admit_task(root: &Path, task_id: &str) {
        save_task_registry(root, &registry_for_tasks(&[task_id])).unwrap();
    }

    fn registry_for_tasks(task_ids: &[&str]) -> TaskRegistry {
        TaskRegistry {
            tasks: task_ids
                .iter()
                .map(|task_id| {
                    (
                        (*task_id).to_string(),
                        TaskRecord {
                            task_id: (*task_id).to_string(),
                            ..TaskRecord::default()
                        },
                    )
                })
                .collect(),
        }
    }

    fn registry_with_watch(task_id: &str, watch_id: &str) -> TaskRegistry {
        let mut registry = registry_for_tasks(&[task_id]);
        registry.tasks.get_mut(task_id).unwrap().watch_ids = vec![watch_id.to_string()];
        registry
    }

    fn watch_registry_with_marker(marker: &str, task_id: &str) -> WatchRegistry {
        WatchRegistry {
            watches: vec![WatchRegistration {
                watch_id: marker.to_string(),
                spec: packet28_daemon_protocol::commands::WatchSpec {
                    task_id: task_id.to_string(),
                    ..packet28_daemon_protocol::commands::WatchSpec::default()
                },
                ..WatchRegistration::default()
            }],
        }
    }

    #[test]
    fn failed_initial_cleanup_removes_only_the_admitted_tasks_exact_namespace() {
        let root = tempdir().unwrap();
        save_task_registry(root.path(), &registry_for_tasks(&["retry", "keep"])).unwrap();
        for task_id in ["retry", "keep"] {
            let artifact = task_artifact_dir(root.path(), &task_storage_id(task_id));
            fs::create_dir_all(&artifact).unwrap();
            fs::write(artifact.join("payload.bin"), task_id.as_bytes()).unwrap();
            let event = task_event_path(root.path(), task_id);
            fs::create_dir_all(event.parent().unwrap()).unwrap();
            fs::write(event, format!("{task_id}\n")).unwrap();
        }

        remove_failed_initial_task_storage(root.path(), "retry").unwrap();

        assert!(!task_artifact_dir(root.path(), &task_storage_id("retry")).exists());
        assert!(!task_event_path(root.path(), "retry").exists());
        assert!(task_artifact_dir(root.path(), &task_storage_id("keep")).exists());
        assert!(task_event_path(root.path(), "keep").exists());
        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("retry"));
    }

    #[test]
    fn registry_authority_excludes_failed_initial_cleanup_during_event_append() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "retry");
        append_next_task_event(
            root.path(),
            "retry",
            &DaemonEvent {
                kind: "before-authority".to_string(),
                occurred_at_unix: 1,
                data: serde_json::Value::Null,
            },
        )
        .unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let authority = load_registry_admission_authority(root.path(), lease).unwrap();
        let root_path = root.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let cleanup = thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(remove_failed_initial_task_storage(&root_path, "retry"))
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let appended = append_next_task_event_with_authority(
            root.path(),
            &authority,
            "retry",
            &DaemonEvent {
                kind: "while-authority-held".to_string(),
                occurred_at_unix: 2,
                data: serde_json::Value::Null,
            },
        )
        .unwrap();
        assert_eq!(appended.seq, 2);
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        drop(authority);
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        cleanup.join().unwrap();
        assert!(!task_event_path(root.path(), "retry").exists());
    }

    #[test]
    fn authority_cleanup_requires_exclusive_authority_borrow() {
        let cleanup: fn(&Path, &mut RegistryAdmissionAuthority, &str) -> Result<()> =
            remove_failed_initial_task_storage_with_authority;
        let _ = cleanup;
    }

    #[test]
    fn failed_initial_cleanup_rejects_unregistered_preexisting_storage() {
        let root = tempdir().unwrap();
        save_task_registry(root.path(), &TaskRegistry::default()).unwrap();
        let artifact = task_artifact_dir(root.path(), &task_storage_id("unregistered"));
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("payload.bin"), b"preserve").unwrap();
        let event = task_event_path(root.path(), "unregistered");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"preserve\n").unwrap();

        let error = remove_failed_initial_task_storage(root.path(), "unregistered").unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(artifact.join("payload.bin")).unwrap(), b"preserve");
        assert_eq!(fs::read(event).unwrap(), b"preserve\n");
    }

    #[cfg(unix)]
    #[test]
    fn failed_initial_cleanup_unlinks_symlinks_without_following_targets() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        admit_task(root.path(), "linked");
        fs::write(outside.path().join("artifact.bin"), b"outside-artifact").unwrap();
        fs::write(outside.path().join("event.jsonl"), b"outside-event").unwrap();
        fs::create_dir_all(task_artifacts_dir(root.path())).unwrap();
        let artifact = task_artifact_dir(root.path(), &task_storage_id("linked"));
        symlink(outside.path(), &artifact).unwrap();
        let event = task_event_path(root.path(), "linked");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        symlink(outside.path().join("event.jsonl"), &event).unwrap();

        remove_failed_initial_task_storage(root.path(), "linked").unwrap();

        assert!(!artifact.exists());
        assert!(!event.exists());
        assert_eq!(
            fs::read(outside.path().join("artifact.bin")).unwrap(),
            b"outside-artifact"
        );
        assert_eq!(
            fs::read(outside.path().join("event.jsonl")).unwrap(),
            b"outside-event"
        );
    }

    fn set_checkpoint_generation(path: &Path, generation: u64) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        value.as_object_mut().unwrap().insert(
            REGISTRY_CHECKPOINT_GENERATION_FIELD.to_string(),
            serde_json::Value::from(generation),
        );
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn authenticate_checkpoint_manifest_for_current_pair(root: &Path) {
        let task_raw = fs::read(task_registry_path(root)).unwrap();
        let watch_raw = fs::read(watch_registry_path(root)).unwrap();
        let generation = registry_checkpoint_generation(
            &task_registry_path(root),
            &task_raw,
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap();
        let path = daemon_dir(root).join("task-watch-checkpoint-v1.json");
        let manifest = serde_json::json!({
            "schema_version": 1,
            "checkpoint": {
                "generation": generation,
                "tasks": {
                    "bytes": task_raw.len(),
                    "blake3": blake3::hash(&task_raw).to_hex().to_string(),
                },
                "watches": {
                    "bytes": watch_raw.len(),
                    "blake3": blake3::hash(&watch_raw).to_hex().to_string(),
                },
            },
        });
        fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    fn write_frozen_legacy_registry_pair(
        root: &Path,
        tasks: &TaskRegistry,
        watches: &WatchRegistry,
    ) {
        ensure_daemon_dir(root).unwrap();
        fs::write(
            task_registry_path(root),
            serde_json::to_vec_pretty(tasks).unwrap(),
        )
        .unwrap();
        fs::write(
            watch_registry_path(root),
            serde_json::to_vec_pretty(watches).unwrap(),
        )
        .unwrap();
    }

    fn invalid_task_watch_registry_pairs() -> Vec<(TaskRegistry, WatchRegistry)> {
        let mut watch_not_listed = registry_with_watch("task", "watch");
        watch_not_listed
            .tasks
            .get_mut("task")
            .unwrap()
            .watch_ids
            .clear();

        let mut missing_watch = registry_with_watch("task", "watch");
        missing_watch
            .tasks
            .get_mut("task")
            .unwrap()
            .watch_ids
            .push("missing-watch".to_string());

        let mut missing_task_watch = watch_registry_with_marker("watch", "missing-task");
        missing_task_watch.watches[0].spec.task_id = "missing-task".to_string();

        let duplicate_watch = {
            let mut watches = watch_registry_with_marker("watch", "task");
            watches.watches.push(watches.watches[0].clone());
            watches
        };

        let mut wrong_owner_tasks = registry_with_watch("task", "watch");
        wrong_owner_tasks.tasks.insert(
            "other".to_string(),
            TaskRecord {
                task_id: "other".to_string(),
                ..TaskRecord::default()
            },
        );

        vec![
            (
                watch_not_listed,
                watch_registry_with_marker("watch", "task"),
            ),
            (missing_watch, watch_registry_with_marker("watch", "task")),
            (registry_with_watch("task", "watch"), missing_task_watch),
            (registry_with_watch("task", "watch"), duplicate_watch),
            (
                wrong_owner_tasks,
                watch_registry_with_marker("watch", "other"),
            ),
        ]
    }

    #[test]
    fn paired_registry_checkpoint_rejects_standalone_half_writes_without_mutation() {
        let root = tempdir().unwrap();
        let mut tasks = registry_with_watch("paired", "watch-first");
        let mut watches = watch_registry_with_marker("watch-first", "paired");
        save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();

        let (loaded_tasks, loaded_watches, tails) =
            load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap();
        assert!(loaded_tasks.tasks.contains_key("paired"));
        assert_eq!(loaded_watches.watches[0].watch_id, "watch-first");
        assert_eq!(tails["paired"], None);
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let generation = registry_checkpoint_generation(
            &task_path,
            &fs::read(&task_path).unwrap(),
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap();
        assert_eq!(
            registry_checkpoint_generation(
                &watch_path,
                &fs::read(&watch_path).unwrap(),
                AuthorityJsonProfile::WatchRegistry,
            )
            .unwrap(),
            generation
        );
        let task_before = fs::read(&task_path).unwrap();
        let watch_before = fs::read(&watch_path).unwrap();

        tasks.tasks.get_mut("paired").unwrap().last_error =
            Some("standalone-task-save".to_string());
        let task_error = save_task_registry(root.path(), &tasks).unwrap_err();
        assert!(matches!(
            task_error,
            DaemonCoreError::RegistryCheckpointRequired {
                registry: "task",
                ..
            }
        ));
        watches.watches[0].last_error = Some("standalone-watch-save".to_string());
        let watch_error = save_watch_registry(root.path(), &watches).unwrap_err();
        assert!(matches!(
            watch_error,
            DaemonCoreError::RegistryCheckpointRequired {
                registry: "watch",
                ..
            }
        ));
        assert_eq!(fs::read(&task_path).unwrap(), task_before);
        assert_eq!(fs::read(&watch_path).unwrap(), watch_before);

        save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();

        let replacement_generation = registry_checkpoint_generation(
            &task_path,
            &fs::read(&task_path).unwrap(),
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap();
        assert_ne!(replacement_generation, generation);
        assert_eq!(
            registry_checkpoint_generation(
                &watch_path,
                &fs::read(&watch_path).unwrap(),
                AuthorityJsonProfile::WatchRegistry,
            )
            .unwrap(),
            replacement_generation
        );
        load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap();
    }

    #[test]
    fn paired_registry_save_rejects_non_bijective_or_non_unique_relationships() {
        for (tasks, watches) in invalid_task_watch_registry_pairs() {
            let root = tempdir().unwrap();

            let error =
                save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap_err();

            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskWatchRegistry { .. }
            ));
            assert!(!task_registry_path(root.path()).exists());
            assert!(!watch_registry_path(root.path()).exists());
        }
    }

    #[test]
    fn paired_registry_load_rejects_non_bijective_or_non_unique_relationships() {
        for (tasks, watches) in invalid_task_watch_registry_pairs() {
            let root = tempdir().unwrap();
            write_frozen_legacy_registry_pair(root.path(), &tasks, &watches);

            let error =
                load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap_err();

            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskWatchRegistry { .. }
            ));
        }
    }

    #[test]
    fn paired_registry_loader_accepts_legacy_and_rejects_mixed_generations() {
        let root = tempdir().unwrap();
        write_frozen_legacy_registry_pair(
            root.path(),
            &registry_with_watch("legacy", "legacy-watch"),
            &watch_registry_with_marker("legacy-watch", "legacy"),
        );
        load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap();

        save_task_watch_registry_checkpoint(
            root.path(),
            &registry_with_watch("next", "next-watch"),
            &watch_registry_with_marker("next-watch", "next"),
        )
        .unwrap();
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let task_generation = registry_checkpoint_generation(
            &task_path,
            &fs::read(&task_path).unwrap(),
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap()
        .unwrap();
        set_checkpoint_generation(&watch_path, task_generation + 1);

        let error = load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::RegistryCheckpointGenerationMismatch {
                task_generation: Some(task),
                watch_generation: Some(watch),
                ..
            } if task == task_generation && watch == task_generation + 1
        ));
    }

    #[test]
    fn paired_registry_loader_rejects_a_frozen_old_writer_watch_only_crash() {
        let root = tempdir().unwrap();
        let tasks = registry_with_watch("legacy", "legacy-watch");
        let mut watches = watch_registry_with_marker("legacy-watch", "legacy");
        save_task_watch_registry_checkpoint(root.path(), &tasks, &watches).unwrap();
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let generation = registry_checkpoint_generation(
            &task_path,
            &fs::read(&task_path).unwrap(),
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap()
        .unwrap();
        watches.watches[0].last_error = Some("old-writer-watch-phase".to_string());
        fs::write(&watch_path, serde_json::to_vec_pretty(&watches).unwrap()).unwrap();

        let error = load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::RegistryCheckpointGenerationMismatch {
                task_generation: Some(task_generation),
                watch_generation: None,
                ..
            } if task_generation == generation
        ));
    }

    #[test]
    fn checkpoint_generation_rejects_duplicate_top_level_keys_in_both_registries() {
        let root = tempdir().unwrap();
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        let task_raw = br#"{
            "tasks": {},
            "task_watch_checkpoint_generation": 7,
            "task_watch_checkpoint_generation": 7
        }"#;
        let watch_raw = br#"{
            "watches": [],
            "task_watch_checkpoint_generation": 7,
            "task_watch_checkpoint_generation": 7
        }"#;

        for (path, raw, profile) in [
            (
                task_path.as_path(),
                task_raw.as_slice(),
                AuthorityJsonProfile::TaskRegistry,
            ),
            (
                watch_path.as_path(),
                watch_raw.as_slice(),
                AuthorityJsonProfile::WatchRegistry,
            ),
        ] {
            let error = registry_checkpoint_generation(path, raw, profile).unwrap_err();
            assert!(matches!(error, DaemonCoreError::Json { .. }));
            assert!(error.to_string().contains("duplicate JSON object key"));
        }

        let error = decode_watch_registry_with_generation(&watch_path, watch_raw).unwrap_err();
        assert!(matches!(error, DaemonCoreError::Json { .. }));
        assert!(error.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    fn watch_registry_profile_rejects_an_excessive_record_count() {
        let root = tempdir().unwrap();
        let path = watch_registry_path(root.path());
        let raw = format!(
            r#"{{"watches":[{}]}}"#,
            vec!["{}"; MAX_WATCH_REGISTRY_RECORDS + 1].join(",")
        );

        let error = decode_watch_registry_with_generation(&path, raw.as_bytes()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                authority: "watch-registry",
                resource: "watch-registry records",
                observed,
                max,
                ..
            } if observed == (MAX_WATCH_REGISTRY_RECORDS + 1) as u64
                && max == MAX_WATCH_REGISTRY_RECORDS as u64
        ));
    }

    #[test]
    fn exhausted_registry_checkpoint_generation_rejects_without_mutation() {
        let root = tempdir().unwrap();
        save_task_watch_registry_checkpoint(
            root.path(),
            &registry_with_watch("existing", "existing-watch"),
            &watch_registry_with_marker("existing-watch", "existing"),
        )
        .unwrap();
        let task_path = task_registry_path(root.path());
        let watch_path = watch_registry_path(root.path());
        set_checkpoint_generation(&task_path, u64::MAX);
        set_checkpoint_generation(&watch_path, u64::MAX);
        authenticate_checkpoint_manifest_for_current_pair(root.path());
        let task_before = fs::read(&task_path).unwrap();
        let watch_before = fs::read(&watch_path).unwrap();

        let error = save_task_watch_registry_checkpoint(
            root.path(),
            &registry_with_watch("replacement", "replacement-watch"),
            &watch_registry_with_marker("replacement-watch", "replacement"),
        )
        .unwrap_err();

        assert!(matches!(
            &error,
            DaemonCoreError::RegistryCheckpointGenerationExhausted {
                root: exhausted_root,
                task_generation: Some(u64::MAX),
                watch_generation: Some(u64::MAX),
            } if exhausted_root == root.path()
        ));
        let diagnostic = error.to_string();
        assert!(diagnostic.contains(&root.path().display().to_string()));
        assert!(diagnostic.contains(&format!("task=Some({})", u64::MAX)));
        assert!(diagnostic.contains(&format!("watch=Some({})", u64::MAX)));
        assert_eq!(fs::read(task_path).unwrap(), task_before);
        assert_eq!(fs::read(watch_path).unwrap(), watch_before);
    }

    #[cfg(unix)]
    #[test]
    fn registry_checkpoint_process_child() {
        let Some(root) = std::env::var_os("PACKET28_REGISTRY_CHECKPOINT_CHILD_ROOT") else {
            return;
        };
        let phase = std::env::var("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER").unwrap();
        save_task_watch_registry_checkpoint(
            Path::new(&root),
            &registry_with_watch(phase.as_str(), &format!("watch-{phase}")),
            &watch_registry_with_marker(&format!("watch-{phase}"), phase.as_str()),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn abrupt_checkpoint_exit_recovers_prior_pair_until_manifest_commit() {
        let executable = std::env::current_exe().unwrap();
        for phase in ["watch", "task", "manifest"] {
            let root = tempdir().unwrap();
            save_task_watch_registry_checkpoint(
                root.path(),
                &registry_with_watch("existing", "watch-existing"),
                &watch_registry_with_marker("watch-existing", "existing"),
            )
            .unwrap();
            let output = Command::new(&executable)
                .arg("--exact")
                .arg("storage::tests::registry_checkpoint_process_child")
                .env("PACKET28_REGISTRY_CHECKPOINT_CHILD_ROOT", root.path())
                .env("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER", phase)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "checkpoint child failed unexpectedly: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let (tasks, watches, _) =
                load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap();
            if phase == "manifest" {
                assert!(tasks.tasks.contains_key("manifest"));
                assert_eq!(watches.watches[0].watch_id, "watch-manifest");
            } else {
                assert!(tasks.tasks.contains_key("existing"));
                assert_eq!(watches.watches[0].watch_id, "watch-existing");
                assert!(load_task_registry(root.path())
                    .unwrap()
                    .tasks
                    .contains_key("existing"));
                assert_eq!(
                    load_watch_registry(root.path()).unwrap().watches[0].watch_id,
                    "watch-existing"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_retry_heals_the_base_before_replacing_the_recovery_journal() {
        let executable = std::env::current_exe().unwrap();
        for first_phase in ["watch", "task"] {
            let root = tempdir().unwrap();
            save_task_watch_registry_checkpoint(
                root.path(),
                &registry_with_watch("existing", "watch-existing"),
                &watch_registry_with_marker("watch-existing", "existing"),
            )
            .unwrap();
            let task_path = task_registry_path(root.path());
            let watch_path = watch_registry_path(root.path());
            let committed_tasks = fs::read(&task_path).unwrap();
            let committed_watches = fs::read(&watch_path).unwrap();

            for phase in [first_phase, "journal"] {
                let output = Command::new(&executable)
                    .arg("--exact")
                    .arg("storage::tests::registry_checkpoint_process_child")
                    .env("PACKET28_REGISTRY_CHECKPOINT_CHILD_ROOT", root.path())
                    .env("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER", phase)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(86),
                    "checkpoint child failed at {phase} after {first_phase}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            assert_eq!(
                (
                    fs::read(&task_path).unwrap(),
                    fs::read(&watch_path).unwrap()
                ),
                (committed_tasks, committed_watches),
                "the second journal replaced recovery authority before healing \
                 the canonical {first_phase} interruption"
            );
            let (tasks, watches, _) =
                load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap();
            assert_eq!(
                (
                    tasks.tasks.contains_key("existing"),
                    watches.watches[0].watch_id.as_str(),
                ),
                (true, "watch-existing")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_checkpoint_rejects_a_corrupt_recovery_image() {
        let root = tempdir().unwrap();
        save_task_watch_registry_checkpoint(
            root.path(),
            &registry_with_watch("existing", "watch-existing"),
            &watch_registry_with_marker("watch-existing", "existing"),
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("storage::tests::registry_checkpoint_process_child")
            .env("PACKET28_REGISTRY_CHECKPOINT_CHILD_ROOT", root.path())
            .env("PACKET28_REGISTRY_CHECKPOINT_EXIT_AFTER", "watch")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86));
        fs::write(
            daemon_dir(root.path()).join(".task-watch-checkpoint-v1.journal.tasks"),
            b"corrupt",
        )
        .unwrap();

        let error = load_task_watch_registry_checkpoint_with_event_tails(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskWatchRegistry { .. }
        ));
    }

    #[test]
    fn checkpoint_loader_rejects_a_corrupt_commit_manifest() {
        let root = tempdir().unwrap();
        save_task_watch_registry_checkpoint(
            root.path(),
            &registry_with_watch("existing", "watch-existing"),
            &watch_registry_with_marker("watch-existing", "existing"),
        )
        .unwrap();
        fs::write(
            daemon_dir(root.path()).join("task-watch-checkpoint-v1.json"),
            b"{not-json",
        )
        .unwrap();

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Json { .. }));
    }

    fn task_event_frame(task_id: &str, seq: u64) -> DaemonEventFrame {
        DaemonEventFrame {
            seq,
            task_id: task_id.to_string(),
            event: DaemonEvent {
                kind: "test".to_string(),
                occurred_at_unix: seq,
                data: serde_json::json!({}),
            },
        }
    }

    fn test_daemon_event(value: u64) -> DaemonEvent {
        DaemonEvent {
            kind: "test".to_string(),
            occurred_at_unix: value,
            data: serde_json::json!({"value": value}),
        }
    }

    fn task_event_frame_with_encoded_size(task_id: &str, target: usize) -> DaemonEventFrame {
        let mut frame = task_event_frame(task_id, 1);
        frame.event.data = serde_json::json!({"padding": ""});
        let base = serde_json::to_vec(&frame).unwrap().len();
        assert!(base <= target);
        frame.event.data = serde_json::json!({"padding": "x".repeat(target - base)});
        assert_eq!(serde_json::to_vec(&frame).unwrap().len(), target);
        frame
    }

    fn directory_contains_exact_name(directory: &Path, expected: &str) -> bool {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name() == OsStr::new(expected))
    }

    fn registry_with_encoded_size(target_bytes: usize) -> TaskRegistry {
        registry_with_id_pattern_and_encoded_size("sized-registry", "x", target_bytes)
    }

    fn registry_with_id_pattern_and_encoded_size(
        task_id: &str,
        pattern: &str,
        target_bytes: usize,
    ) -> TaskRegistry {
        let task_id = task_id.to_string();
        let mut registry = TaskRegistry {
            tasks: BTreeMap::from([(
                task_id.clone(),
                TaskRecord {
                    task_id: task_id.clone(),
                    last_error: Some(String::new()),
                    ..TaskRecord::default()
                },
            )]),
        };
        let base_bytes = serde_json::to_vec_pretty(&registry).unwrap().len();
        let padding = target_bytes
            .checked_sub(base_bytes)
            .expect("target must fit the base registry");
        let encoded_pattern_bytes = serde_json::to_vec(pattern).unwrap().len() - 2;
        assert!(encoded_pattern_bytes > 0);
        let value = pattern.repeat(padding / encoded_pattern_bytes)
            + &"x".repeat(padding % encoded_pattern_bytes);
        registry.tasks.get_mut(&task_id).unwrap().last_error = Some(value);
        assert_eq!(
            serde_json::to_vec_pretty(&registry).unwrap().len(),
            target_bytes
        );
        registry
    }

    fn active_record_with_encoded_size(target_bytes: usize) -> ActiveTaskRecord {
        let mut record = ActiveTaskRecord {
            task_id: "bounded".to_string(),
            session_id: Some(String::new()),
            updated_at_unix: 1,
        };
        let base_bytes = serde_json::to_vec_pretty(&record).unwrap().len();
        let padding = target_bytes
            .checked_sub(base_bytes)
            .expect("target must fit the base active-task record");
        record.session_id = Some("x".repeat(padding));
        assert_eq!(
            serde_json::to_vec_pretty(&record).unwrap().len(),
            target_bytes
        );
        record
    }

    fn authority_test_limits() -> AuthorityJsonLimits {
        AuthorityJsonLimits {
            max_depth: 16,
            max_value_nodes: 64,
            max_container_entries: 64,
            max_entries_per_container: 64,
            max_tokens: 128,
            max_decoded_string_bytes: 128,
            max_registry_records: 64,
        }
    }

    fn assert_authority_limit(raw: &[u8], limits: AuthorityJsonLimits, expected_resource: &str) {
        let error =
            validate_authority_json_with_limits(raw, AuthorityJsonProfile::TaskRegistry, limits)
                .unwrap_err();
        assert!(
            matches!(
                error,
                AuthorityJsonError::Limit { resource, .. }
                    if resource == expected_resource
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn authority_json_preflight_enforces_every_structural_budget_at_the_boundary() {
        let limits = authority_test_limits();

        validate_authority_json_with_limits(
            br#"[[null]]"#,
            AuthorityJsonProfile::TaskRegistry,
            AuthorityJsonLimits {
                max_depth: 3,
                ..limits
            },
        )
        .unwrap();
        assert_authority_limit(
            br#"[[[null]]]"#,
            AuthorityJsonLimits {
                max_depth: 3,
                ..limits
            },
            "nesting depth",
        );

        validate_authority_json_with_limits(
            br#"[null,null]"#,
            AuthorityJsonProfile::TaskRegistry,
            AuthorityJsonLimits {
                max_value_nodes: 3,
                max_container_entries: 2,
                max_entries_per_container: 2,
                max_tokens: 3,
                ..limits
            },
        )
        .unwrap();
        assert_authority_limit(
            br#"[null,null]"#,
            AuthorityJsonLimits {
                max_value_nodes: 2,
                ..limits
            },
            "value nodes",
        );
        assert_authority_limit(
            br#"[null,null]"#,
            AuthorityJsonLimits {
                max_container_entries: 1,
                ..limits
            },
            "container entries",
        );
        assert_authority_limit(
            br#"[null,null]"#,
            AuthorityJsonLimits {
                max_entries_per_container: 1,
                ..limits
            },
            "entries per container",
        );
        assert_authority_limit(
            br#"[null,null]"#,
            AuthorityJsonLimits {
                max_tokens: 2,
                ..limits
            },
            "tokens",
        );

        validate_authority_json_with_limits(
            br#"{"a":"bc"}"#,
            AuthorityJsonProfile::TaskRegistry,
            AuthorityJsonLimits {
                max_decoded_string_bytes: 3,
                ..limits
            },
        )
        .unwrap();
        assert_authority_limit(
            br#"{"a":"bc"}"#,
            AuthorityJsonLimits {
                max_decoded_string_bytes: 2,
                ..limits
            },
            "decoded string bytes",
        );

        validate_authority_json_with_limits(
            br#"{"tasks":{"a":{},"b":{}}}"#,
            AuthorityJsonProfile::TaskRegistry,
            AuthorityJsonLimits {
                max_registry_records: 2,
                ..limits
            },
        )
        .unwrap();
        assert_authority_limit(
            br#"{"tasks":{"a":{},"b":{}}}"#,
            AuthorityJsonLimits {
                max_registry_records: 1,
                ..limits
            },
            "task-registry records",
        );
    }

    #[test]
    fn authority_json_preflight_rejects_decoded_duplicate_keys_and_trailing_input() {
        for raw in [
            br#"{"tasks":{},"ta\u0073ks":{}}"#.as_slice(),
            br#"{"tasks":{"task":{"future":1,"f\u0075ture":2}}}"#.as_slice(),
        ] {
            let error =
                decode_json_value_without_duplicate_keys(raw, AuthorityJsonProfile::TaskRegistry)
                    .unwrap_err();
            assert!(error.to_string().contains("duplicate JSON object key"));
        }

        let error = decode_json_value_without_duplicate_keys(
            br#"{"tasks":{}} []"#,
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap_err();
        assert!(matches!(error, AuthorityJsonError::Json(_)));
    }

    #[test]
    fn active_task_record_accepts_the_exact_shared_limit() {
        let root = tempdir().unwrap();
        let record = active_record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES);

        save_active_task_record(root.path(), &record).unwrap();
        let loaded = load_active_task_record(root.path()).unwrap().unwrap();

        assert_eq!(loaded.task_id, "bounded");
        assert_eq!(
            fs::metadata(active_task_path(root.path())).unwrap().len(),
            MAX_ACTIVE_TASK_RECORD_BYTES as u64
        );
    }

    #[test]
    fn active_task_record_one_over_limit_is_rejected_before_state_mutation() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        let record = active_record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES + 1);

        let error = save_active_task_record(root.path(), &record).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::ActiveTaskRecordTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1
                && max_bytes == MAX_ACTIVE_TASK_RECORD_BYTES as u64
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn active_task_record_one_over_limit_preserves_the_old_file() {
        let root = tempdir().unwrap();
        let existing = ActiveTaskRecord {
            task_id: "existing".to_string(),
            session_id: None,
            updated_at_unix: 1,
        };
        save_active_task_record(root.path(), &existing).unwrap();
        let path = active_task_path(root.path());
        let before = fs::read(&path).unwrap();
        let record = active_record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES + 1);

        let error = save_active_task_record(root.path(), &record).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::ActiveTaskRecordTooLarge { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            load_active_task_record(root.path())
                .unwrap()
                .unwrap()
                .task_id,
            "existing"
        );
    }

    #[test]
    fn active_task_record_rejects_legacy_nonportable_identifier_without_mutation() {
        let root = tempdir().unwrap();
        let record = ActiveTaskRecord {
            task_id: " λ/live ".to_string(),
            session_id: None,
            updated_at_unix: 1,
        };

        let error = save_active_task_record(root.path(), &record).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidActiveTaskRecord { .. }
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn active_task_reader_rejects_duplicate_authority_keys_without_mutation() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for raw in [
            br#"{"task_id":"live","ta\u0073k_id":"victim","updated_at_unix":1}"#.as_slice(),
            br#"{"task_id":"live","updated_at_unix":1,"future":{"phase":1,"ph\u0061se":2}}"#
                .as_slice(),
        ] {
            fs::write(&path, raw).unwrap();
            let before = blake3::hash(raw);

            let error = load_active_task_record(root.path()).unwrap_err();

            assert!(matches!(error, DaemonCoreError::Json { .. }));
            assert!(error.to_string().contains("duplicate JSON object key"));
            assert_eq!(blake3::hash(&fs::read(&path).unwrap()), before);
        }
    }

    #[test]
    fn active_task_reader_requires_an_explicit_string_task_identity() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for raw in [
            br#"{}"#.as_slice(),
            br#"{"task_id":null}"#.as_slice(),
            br#"{"task_id":1}"#.as_slice(),
        ] {
            fs::write(&path, raw).unwrap();
            let error = load_active_task_record(root.path()).unwrap_err();
            assert!(matches!(
                error,
                DaemonCoreError::InvalidActiveTaskRecord { .. }
            ));
        }
    }

    #[test]
    fn active_task_identifier_accepts_the_exact_storage_key_limit() {
        let root = tempdir().unwrap();
        let task_id = "a".repeat(MAX_TASK_STORAGE_KEY_BYTES);
        let record = ActiveTaskRecord {
            task_id: task_id.clone(),
            session_id: None,
            updated_at_unix: 1,
        };

        save_active_task_record(root.path(), &record).unwrap();

        assert_eq!(
            load_active_task_record(root.path())
                .unwrap()
                .unwrap()
                .task_id,
            task_id
        );
        assert_eq!(
            task_event_path(root.path(), &record.task_id)
                .file_name()
                .unwrap()
                .as_encoded_bytes()
                .len(),
            MAX_TASK_STORE_COMPONENT_BYTES
        );
    }

    #[test]
    fn active_task_identifier_one_over_storage_key_limit_is_rejected_without_mutation() {
        let root = tempdir().unwrap();
        let record = ActiveTaskRecord {
            task_id: "a".repeat(MAX_TASK_STORAGE_KEY_BYTES + 1),
            session_id: None,
            updated_at_unix: 1,
        };

        let error = save_active_task_record(root.path(), &record).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidActiveTaskRecord { .. }
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn blank_active_task_identifier_is_rejected_before_state_mutation() {
        let root = tempdir().unwrap();
        let record = ActiveTaskRecord {
            task_id: " \t ".to_string(),
            session_id: None,
            updated_at_unix: 1,
        };

        let error = save_active_task_record(root.path(), &record).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidActiveTaskRecord { .. }
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn oversized_persisted_active_task_record_has_a_typed_error() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1)
            .unwrap();

        let error = load_active_task_record(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::ActiveTaskRecordTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_ACTIVE_TASK_RECORD_BYTES as u64 + 1
                && max_bytes == MAX_ACTIVE_TASK_RECORD_BYTES as u64
        ));
    }

    #[test]
    fn portable_active_task_reader_accepts_the_exact_limit() {
        let root = tempdir().unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = active_record_with_encoded_size(MAX_ACTIVE_TASK_RECORD_BYTES);
        let bytes = encode_active_task_record(&path, &record).unwrap();
        save_active_task_record_portable(&path, &bytes).unwrap();

        let loaded = load_active_task_record_portable(&path).unwrap().unwrap();

        assert_eq!(loaded.task_id, "bounded");
    }

    #[cfg(unix)]
    #[test]
    fn active_task_reader_rejects_a_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_path = outside.path().join("active-task.json");
        fs::write(
            &outside_path,
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "outside".to_string(),
                session_id: None,
                updated_at_unix: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let path = active_task_path(root.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(&outside_path, &path).unwrap();

        let error = load_active_task_record(root.path()).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(
            fs::read(outside_path).unwrap(),
            serde_json::to_vec(&ActiveTaskRecord {
                task_id: "outside".to_string(),
                session_id: None,
                updated_at_unix: 1,
            })
            .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn active_task_reader_rejects_directories_and_fifos_without_reading() {
        let directory_root = tempdir().unwrap();
        let directory_path = active_task_path(directory_root.path());
        fs::create_dir_all(&directory_path).unwrap();
        assert!(matches!(
            load_active_task_record(directory_root.path()),
            Err(DaemonCoreError::Io { .. })
        ));

        let fifo_root = tempdir().unwrap();
        let fifo_path = active_task_path(fifo_root.path());
        fs::create_dir_all(fifo_path.parent().unwrap()).unwrap();
        assert!(Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .unwrap()
            .success());
        assert!(matches!(
            load_active_task_record(fifo_root.path()),
            Err(DaemonCoreError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn active_task_reader_reopens_when_atomic_replacement_detaches_open_inode() {
        let root = tempdir().unwrap();
        let old = ActiveTaskRecord {
            task_id: "old".to_string(),
            session_id: Some("old-session".to_string()),
            updated_at_unix: 1,
        };
        let replacement = ActiveTaskRecord {
            task_id: "replacement".to_string(),
            session_id: Some("replacement-session".to_string()),
            updated_at_unix: 2,
        };
        save_active_task_record(root.path(), &old).unwrap();
        let writer_root = root.path().to_path_buf();
        let writer_record = replacement.clone();
        inject_authenticated_read_after_open_once(
            OsStr::new(AGENT_ACTIVE_TASK_FILE_NAME),
            move || save_active_task_record(&writer_root, &writer_record).unwrap(),
        );

        let loaded = load_active_task_record(root.path()).unwrap().unwrap();

        assert_eq!(
            (
                loaded.task_id.as_str(),
                loaded.session_id.as_deref(),
                loaded.updated_at_unix,
            ),
            (
                replacement.task_id.as_str(),
                replacement.session_id.as_deref(),
                replacement.updated_at_unix,
            )
        );
    }

    #[test]
    fn concurrent_active_task_readers_observe_only_complete_records() {
        let root = tempdir().unwrap();
        let old_session = "a".repeat(8 * 1024);
        let new_session = "b".repeat(16 * 1024);
        let old = ActiveTaskRecord {
            task_id: "old".to_string(),
            session_id: Some(old_session.clone()),
            updated_at_unix: 1,
        };
        let new = ActiveTaskRecord {
            task_id: "new".to_string(),
            session_id: Some(new_session.clone()),
            updated_at_unix: 2,
        };
        save_active_task_record(root.path(), &old).unwrap();
        let reader_root = root.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader = thread::spawn(move || {
            while !reader_stop.load(AtomicOrdering::Acquire) {
                let record = match load_active_task_record(&reader_root) {
                    Ok(Some(record)) => record,
                    Err(DaemonCoreError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::InvalidData
                            && source
                                .to_string()
                                .contains("consecutive authenticated read attempts") =>
                    {
                        continue;
                    }
                    other => panic!("atomic read returned an unexpected result: {other:?}"),
                };
                match record.task_id.as_str() {
                    "old" => assert_eq!(record.session_id.as_deref(), Some(old_session.as_str())),
                    "new" => assert_eq!(record.session_id.as_deref(), Some(new_session.as_str())),
                    other => panic!("reader observed unexpected record {other:?}"),
                }
            }
        });
        for index in 0..32 {
            let record = if index % 2 == 0 { &new } else { &old };
            save_active_task_record(root.path(), record).unwrap();
        }
        stop.store(true, AtomicOrdering::Release);
        reader.join().unwrap();
        let published = load_active_task_record(root.path()).unwrap().unwrap();
        assert_eq!(
            (
                published.task_id.as_str(),
                published.session_id.as_deref(),
                published.updated_at_unix,
            ),
            (
                old.task_id.as_str(),
                old.session_id.as_deref(),
                old.updated_at_unix,
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn active_task_writer_cleans_only_strict_stale_atomic_residue_under_its_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().unwrap();
        let first = ActiveTaskRecord {
            task_id: "first".to_string(),
            session_id: None,
            updated_at_unix: 1,
        };
        save_active_task_record(root.path(), &first).unwrap();
        let agent = agent_runtime_dir(root.path());
        let source = agent.join(".active-task-write-4242-1");
        let tombstone = agent.join(".active-task-write-deleting-4242-2");
        let lookalike = agent.join(".active-task-write-user-data");
        for path in [&source, &tombstone, &lookalike] {
            fs::write(path, b"stale").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let replacement = ActiveTaskRecord {
            task_id: "replacement".to_string(),
            session_id: None,
            updated_at_unix: 2,
        };

        save_active_task_record(root.path(), &replacement).unwrap();

        assert!(!source.exists());
        assert!(!tombstone.exists());
        assert_eq!(fs::read(lookalike).unwrap(), b"stale");
        assert_eq!(
            load_active_task_record(root.path())
                .unwrap()
                .unwrap()
                .task_id,
            "replacement"
        );
    }

    #[test]
    fn appends_and_loads_task_events() {
        let dir = tempdir().unwrap();
        admit_task(dir.path(), "task-demo");
        let frame = DaemonEventFrame {
            seq: 1,
            task_id: "task-demo".to_string(),
            event: DaemonEvent {
                kind: "task_started".to_string(),
                occurred_at_unix: 1,
                data: serde_json::json!({"task_id":"task-demo"}),
            },
        };
        append_task_event(dir.path(), &frame).unwrap();
        append_task_event(
            dir.path(),
            &DaemonEventFrame {
                seq: 2,
                task_id: "task-demo".to_string(),
                event: DaemonEvent {
                    kind: "task_completed".to_string(),
                    occurred_at_unix: 2,
                    data: serde_json::json!({"task_id":"task-demo"}),
                },
            },
        )
        .unwrap();

        let loaded = load_task_events(dir.path(), "task-demo").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].seq, 1);
        assert_eq!(loaded[1].event.kind, "task_completed");
    }

    #[cfg(unix)]
    #[test]
    fn event_tail_and_locked_next_append_establish_sequence_authority() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "sequence-owner");

        assert_eq!(
            task_event_log_tail_sequence(root.path(), "sequence-owner").unwrap(),
            None
        );
        assert!(!task_event_path(root.path(), "sequence-owner").exists());

        let first =
            append_next_task_event(root.path(), "sequence-owner", &test_daemon_event(10)).unwrap();
        let second =
            append_next_task_event(root.path(), "sequence-owner", &test_daemon_event(20)).unwrap();

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(first.task_id, "sequence-owner");
        assert_eq!(
            task_event_log_tail_sequence(root.path(), "sequence-owner").unwrap(),
            Some(2)
        );
        assert_eq!(
            load_task_events(root.path(), "sequence-owner")
                .unwrap()
                .into_iter()
                .map(|frame| frame.seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn explicit_event_append_rejects_duplicate_and_gap_without_mutation() {
        let root = tempdir().unwrap();
        let task_id = "explicit-sequence";
        admit_task(root.path(), task_id);
        append_task_event(root.path(), &task_event_frame(task_id, 1)).unwrap();
        let path = task_event_path(root.path(), task_id);
        let before = fs::read(&path).unwrap();

        for invalid_sequence in [1, 3] {
            let error =
                append_task_event(root.path(), &task_event_frame(task_id, invalid_sequence))
                    .unwrap_err();
            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskEventFrame { .. }
            ));
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn explicit_event_append_recovers_a_bounded_partial_tail() {
        let root = tempdir().unwrap();
        let task_id = "explicit-partial";
        admit_task(root.path(), task_id);
        append_task_event(root.path(), &task_event_frame(task_id, 1)).unwrap();
        let path = task_event_path(root.path(), task_id);
        let complete_prefix = fs::read(&path).unwrap();
        let partial = serde_json::to_vec(&task_event_frame(task_id, 2)).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&partial[..partial.len() / 2]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        append_task_event(root.path(), &task_event_frame(task_id, 2)).unwrap();

        let mut expected = complete_prefix;
        expected.extend(serde_json::to_vec(&task_event_frame(task_id, 2)).unwrap());
        expected.push(b'\n');
        assert_eq!(fs::read(path).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn authoritative_event_tail_rejects_invalid_complete_history_without_mutation() {
        let mut cases = vec![
            (
                "zero-sequence",
                serde_json::to_vec(&task_event_frame("zero-sequence", 0)).unwrap(),
            ),
            (
                "first-not-one",
                serde_json::to_vec(&task_event_frame("first-not-one", 2)).unwrap(),
            ),
            ("malformed-tail", b"{not-json}".to_vec()),
            ("blank-tail", Vec::new()),
            (
                "semantic-tail",
                br#"{"seq":"not-a-number","task_id":"semantic-tail","event":{}}"#.to_vec(),
            ),
            (
                "cross-task",
                serde_json::to_vec(&task_event_frame("other-task", 1)).unwrap(),
            ),
        ];
        let mut duplicate = serde_json::to_vec(&task_event_frame("duplicate-tail", 1)).unwrap();
        duplicate.push(b'\n');
        duplicate.extend(serde_json::to_vec(&task_event_frame("duplicate-tail", 1)).unwrap());
        cases.push(("duplicate-tail", duplicate));
        let mut gap = serde_json::to_vec(&task_event_frame("gap-tail", 1)).unwrap();
        gap.push(b'\n');
        gap.extend(serde_json::to_vec(&task_event_frame("gap-tail", 3)).unwrap());
        cases.push(("gap-tail", gap));

        for (task_id, mut bytes) in cases {
            let root = tempdir().unwrap();
            admit_task(root.path(), task_id);
            let path = task_event_path(root.path(), task_id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            bytes.push(b'\n');
            fs::write(&path, &bytes).unwrap();

            let error = task_event_log_tail_sequence(root.path(), task_id).unwrap_err();
            assert!(
                matches!(error, DaemonCoreError::InvalidTaskEventFrame { .. }),
                "{task_id}: {error:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);

            let error =
                append_next_task_event(root.path(), task_id, &test_daemon_event(4)).unwrap_err();
            assert!(
                matches!(error, DaemonCoreError::InvalidTaskEventFrame { .. }),
                "{task_id}: {error:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);

            let error = load_task_events_from_offset(root.path(), task_id, 0).unwrap_err();
            assert!(
                matches!(error, DaemonCoreError::InvalidTaskEventFrame { .. }),
                "{task_id}: {error:?}"
            );
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn interior_event_corruption_outside_former_tail_window_fails_closed_everywhere() {
        let root = tempdir().unwrap();
        let task_id = "interior-corruption";
        admit_task(root.path(), task_id);
        let path = task_event_path(root.path(), task_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut raw = Vec::new();
        serde_json::to_writer(&mut raw, &task_event_frame(task_id, 1)).unwrap();
        raw.push(b'\n');
        let corruption_start = raw.len();
        serde_json::to_writer(&mut raw, &task_event_frame(task_id, 2)).unwrap();
        raw.push(b'\n');
        let corruption_end = raw.len();
        for seq in 3..=6 {
            let mut frame = task_event_frame_with_encoded_size(task_id, 900 * 1024);
            frame.seq = seq;
            frame.event.occurred_at_unix = seq;
            serde_json::to_writer(&mut raw, &frame).unwrap();
            raw.push(b'\n');
        }
        assert!(raw.len() - corruption_end > MAX_TASK_EVENT_TAIL_SCAN_BYTES);
        fs::write(&path, &raw).unwrap();

        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(corruption_start as u64)).unwrap();
        file.write_all(&vec![b'x'; corruption_end - corruption_start - 1])
            .unwrap();
        file.sync_all().unwrap();
        let before = fs::read(&path).unwrap();

        assert!(matches!(
            task_event_log_tail_sequence(root.path(), task_id),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));
        assert!(matches!(
            load_task_registry_with_event_tails(root.path()),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));
        assert!(matches!(
            load_task_events_from_offset(root.path(), task_id, 0),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));
        assert!(matches!(
            append_next_task_event(root.path(), task_id, &test_daemon_event(7)),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));
        assert!(matches!(
            append_task_event(root.path(), &task_event_frame(task_id, 7)),
            Err(DaemonCoreError::InvalidTaskEventFrame { .. })
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn exhausted_event_sequence_is_rejected() {
        let path = Path::new("exhausted.events.jsonl");
        let error = next_task_event_sequence(path, Some(u64::MAX)).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskEventFrame { ref message, .. }
                if message.contains("exhausted")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn next_event_append_recovers_every_partial_frame_truncation_boundary() {
        let task_id = "partial-property";
        let partial = serde_json::to_vec(&task_event_frame(task_id, 2)).unwrap();

        for cut in 0..=partial.len() {
            let root = tempdir().unwrap();
            admit_task(root.path(), task_id);
            let first =
                append_next_task_event(root.path(), task_id, &test_daemon_event(1)).unwrap();
            let path = task_event_path(root.path(), task_id);
            let complete_prefix = fs::read(&path).unwrap();
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&partial[..cut]).unwrap();
            file.sync_all().unwrap();

            assert_eq!(
                task_event_log_tail_sequence(root.path(), task_id).unwrap(),
                Some(1),
                "cut={cut}"
            );
            let recovered =
                append_next_task_event(root.path(), task_id, &test_daemon_event(2)).unwrap();

            assert_eq!(first.seq, 1, "cut={cut}");
            assert_eq!(recovered.seq, 2, "cut={cut}");
            let mut expected = complete_prefix;
            expected.extend(serde_json::to_vec(&recovered).unwrap());
            expected.push(b'\n');
            assert_eq!(fs::read(&path).unwrap(), expected, "cut={cut}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn oversized_partial_event_tail_is_rejected_without_mutation() {
        let root = tempdir().unwrap();
        let task_id = "oversized-partial";
        admit_task(root.path(), task_id);
        append_next_task_event(root.path(), task_id, &test_daemon_event(1)).unwrap();
        let path = task_event_path(root.path(), task_id);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_TASK_EVENT_LINE_BYTES + 1])
            .unwrap();
        file.sync_all().unwrap();
        let before = fs::read(&path).unwrap();

        let error = task_event_log_tail_sequence(root.path(), task_id).unwrap_err();
        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "crash-partial tail bytes",
                ..
            }
        ));
        assert_eq!(fs::read(&path).unwrap(), before);

        let error =
            append_next_task_event(root.path(), task_id, &test_daemon_event(2)).unwrap_err();
        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "crash-partial tail bytes",
                ..
            }
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn event_tail_discards_result_when_file_binding_changes_after_read() {
        let root = tempdir().unwrap();
        let task_id = task_storage_id("tail-replaced");
        admit_task(root.path(), task_id.as_str());
        append_next_task_event(root.path(), task_id.as_str(), &test_daemon_event(1)).unwrap();
        let path = task_event_path(root.path(), task_id.as_str());
        let detached = root.path().join("detached-event-tail");
        let lease = acquire_task_store_writer_lease(root.path()).unwrap();

        let error = with_registered_task_storage_id(root.path(), &task_id, || {
            task_event_log_tail_sequence_admitted_with_observer(
                root.path(),
                &task_id,
                &lease,
                || {
                    replace_locked_path(&path, &detached).map_err(|source| {
                        DaemonCoreError::io(
                            "failed to inject event-tail replacement",
                            &path,
                            source,
                        )
                    })
                },
            )
        })
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }), "{error:?}");
        assert!(detached.exists());
    }

    #[cfg(unix)]
    #[test]
    fn next_event_append_revalidates_binding_before_and_after_durable_bytes() {
        for replace_after_sync in [false, true] {
            let root = tempdir().unwrap();
            let task_id = task_storage_id(if replace_after_sync {
                "append-replaced-after"
            } else {
                "append-replaced-before"
            });
            admit_task(root.path(), task_id.as_str());
            append_next_task_event(root.path(), task_id.as_str(), &test_daemon_event(1)).unwrap();
            let path = task_event_path(root.path(), task_id.as_str());
            let detached = root.path().join("detached-event-append");
            let before = fs::read(&path).unwrap();
            let lease = acquire_task_store_writer_lease(root.path()).unwrap();

            let error = with_registered_task_storage_id(root.path(), &task_id, || {
                append_next_task_event_admitted_with_observers(
                    root.path(),
                    &task_id,
                    &lease,
                    &test_daemon_event(2),
                    || {
                        if replace_after_sync {
                            return Ok(());
                        }
                        replace_locked_path(&path, &detached).map_err(|source| {
                            DaemonCoreError::io(
                                "failed to inject pre-append replacement",
                                &path,
                                source,
                            )
                        })
                    },
                    || {
                        if !replace_after_sync {
                            return Ok(());
                        }
                        replace_locked_path(&path, &detached).map_err(|source| {
                            DaemonCoreError::io(
                                "failed to inject post-append replacement",
                                &path,
                                source,
                            )
                        })
                    },
                )
            })
            .unwrap_err();

            if replace_after_sync {
                assert!(matches!(
                    error,
                    DaemonCoreError::StorageMutationAuthorityLost { .. }
                ));
                assert!(fs::read(&detached).unwrap().len() > before.len());
            } else {
                assert!(!matches!(
                    error,
                    DaemonCoreError::StorageMutationAuthorityLost { .. }
                ));
                assert_eq!(fs::read(&detached).unwrap(), before);
            }
            assert_eq!(fs::read(path).unwrap(), b"");
        }
    }

    #[cfg(unix)]
    #[test]
    fn append_next_task_event_process_child() {
        let Some(root) = std::env::var_os("PACKET28_APPEND_NEXT_EVENT_CHILD_ROOT") else {
            return;
        };
        append_next_task_event(Path::new(&root), "process-next", &test_daemon_event(1)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cross_process_next_event_allocation_is_unique_and_contiguous() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "process-next");
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..8 {
            children.push(
                Command::new(&executable)
                    .arg("--exact")
                    .arg("storage::tests::append_next_task_event_process_child")
                    .env("PACKET28_APPEND_NEXT_EVENT_CHILD_ROOT", root.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let all_finished = children.iter_mut().all(|child| {
                child
                    .try_wait()
                    .expect("failed to poll next-event child")
                    .is_some()
            });
            if all_finished {
                break;
            }
            if Instant::now() >= deadline {
                for child in &mut children {
                    if child
                        .try_wait()
                        .expect("failed to poll timed-out next-event child")
                        .is_none()
                    {
                        let _ = child.kill();
                    }
                }
                for child in &mut children {
                    let _ = child.wait();
                }
                panic!("next-event children exceeded 20-second shared deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
        for child in children {
            let output = child
                .wait_with_output()
                .expect("failed to reap next-event child");
            assert!(
                output.status.success(),
                "next-event child failed: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert_eq!(
            load_task_events(root.path(), "process-next")
                .unwrap()
                .into_iter()
                .map(|frame| frame.seq)
                .collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(
            task_event_log_tail_sequence(root.path(), "process-next").unwrap(),
            Some(8)
        );
    }

    #[test]
    fn task_event_append_requires_durable_exact_registry_admission() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "orphan");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"orphan-before\n").unwrap();

        let error = append_task_event(root.path(), &task_event_frame("orphan", 1)).unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"orphan-before\n");
        assert!(!task_registry_path(root.path()).exists());
    }

    #[test]
    fn compact_event_admission_uses_wal_authority_without_a_checkpoint_read() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let task_id = "wal-admitted";
        let delta = RegistryDeltaBatch::default().upsert_task(TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        });
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::new(RegistryRevision::new(1), RegistryRevision::new(1)).unwrap(),
            &delta,
        )
        .unwrap();
        assert!(!task_registry_path(root.path()).exists());
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let authority = load_registry_admission_authority(root.path(), lease).unwrap();
        // A malformed checkpoint written after authority loading must not be
        // consulted by the event fast path.
        fs::write(task_registry_path(root.path()), b"not-json").unwrap();

        let frame = append_next_task_event_with_authority(
            root.path(),
            &authority,
            task_id,
            &task_event_frame(task_id, 1).event,
        )
        .unwrap();

        assert_eq!(frame.seq, 1);
        assert_eq!(frame.task_id, task_id);
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            b"not-json"
        );
        fs::remove_file(task_registry_path(root.path())).unwrap();
        let (recovered, tails) =
            load_task_watch_registry_with_deltas_and_event_tails(root.path()).unwrap();
        assert!(recovered.tasks.tasks.contains_key(task_id));
        assert_eq!(recovered.replayed_revision, RegistryRevision::new(1));
        assert_eq!(tails[task_id], Some(1));
        assert!(!task_registry_path(root.path()).exists());
    }

    #[test]
    fn compact_event_admission_rejects_missing_task_without_event_mutation() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let task_id = "not-admitted";
        let event_path = task_event_path(root.path(), task_id);
        fs::create_dir_all(event_path.parent().unwrap()).unwrap();
        fs::write(&event_path, b"sentinel\n").unwrap();
        let lease = crate::task_store_lease::acquire_daemon_task_store_lease(root.path()).unwrap();
        let authority = load_registry_admission_authority(root.path(), lease).unwrap();

        let error = append_next_task_event_with_authority(
            root.path(),
            &authority,
            task_id,
            &task_event_frame(task_id, 1).event,
        )
        .unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(event_path).unwrap(), b"sentinel\n");
    }

    #[test]
    fn compact_event_admission_requires_the_matching_daemon_lifecycle_lease() {
        let root = tempdir().unwrap();
        let other_root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        ensure_daemon_dir(other_root.path()).unwrap();
        admit_task(root.path(), "lease-bound");
        let writer_lease = acquire_task_store_writer_lease(root.path()).unwrap();
        let wrong_role = load_registry_admission_authority(root.path(), writer_lease).unwrap_err();
        assert!(matches!(wrong_role, DaemonCoreError::Io { .. }));

        let other_lease =
            crate::task_store_lease::acquire_daemon_task_store_lease(other_root.path()).unwrap();
        let wrong_root = load_registry_admission_authority(root.path(), other_lease).unwrap_err();
        assert!(matches!(wrong_root, DaemonCoreError::Io { .. }));
        assert!(!task_event_path(root.path(), "lease-bound").exists());
    }

    #[test]
    fn invalid_event_identifier_does_not_normalize_into_an_admitted_task() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "live");
        let registry_before = fs::read(task_registry_path(root.path())).unwrap();
        let canonical = task_event_path(root.path(), "live");

        for invalid in [" live ", "LIVE", "a/b", "λ", "con"] {
            let error = append_task_event(root.path(), &task_event_frame(invalid, 1)).unwrap_err();
            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskStorageIdentifier { .. }
            ));
        }

        assert!(!canonical.exists());
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            registry_before
        );
    }

    #[test]
    fn task_event_identifier_enforces_exact_242_byte_boundary_without_mutation() {
        let accepted = "a".repeat(MAX_TASK_STORAGE_KEY_BYTES);
        let accepted_root = tempdir().unwrap();
        admit_task(accepted_root.path(), &accepted);
        append_task_event(
            accepted_root.path(),
            &task_event_frame(accepted.as_str(), 1),
        )
        .unwrap();
        assert_eq!(
            task_event_path(accepted_root.path(), &accepted)
                .file_name()
                .unwrap()
                .as_encoded_bytes()
                .len(),
            MAX_TASK_STORE_COMPONENT_BYTES
        );

        for rejected_len in [243, 255, 256, 4_096] {
            let root = tempdir().unwrap();
            admit_task(root.path(), "existing");
            let registry_path = task_registry_path(root.path());
            let registry_before = fs::read(&registry_path).unwrap();
            let rejected = "a".repeat(rejected_len);

            let error =
                append_task_event(root.path(), &task_event_frame(&rejected, 1)).unwrap_err();

            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskStorageIdentifier { .. }
            ));
            assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
            assert!(!task_events_dir(root.path()).exists());
        }
    }

    #[test]
    fn task_event_append_rejects_case_alias_without_creating_canonical_log() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "live");
        let canonical = task_event_path(root.path(), "live");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        let alias = canonical.parent().unwrap().join("LIVE.events.jsonl");
        fs::write(&alias, b"alias-before\n").unwrap();
        let registry_before = fs::read(task_registry_path(root.path())).unwrap();

        let error = append_task_event(root.path(), &task_event_frame("live", 1)).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskStorageIdentifier { .. }
        ));
        assert_eq!(fs::read(&alias).unwrap(), b"alias-before\n");
        assert!(!directory_contains_exact_name(
            canonical.parent().unwrap(),
            "live.events.jsonl"
        ));
        assert_eq!(
            fs::read(task_registry_path(root.path())).unwrap(),
            registry_before
        );
    }

    #[cfg(unix)]
    #[test]
    fn task_event_append_rejects_symlink_without_mutating_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        admit_task(root.path(), "linked");
        let path = task_event_path(root.path(), "linked");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let outside = root.path().join("outside-event");
        fs::write(&outside, b"outside-before\n").unwrap();
        symlink(&outside, &path).unwrap();

        assert!(append_task_event(root.path(), &task_event_frame("linked", 1)).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside-before\n");
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn task_event_append_rejects_multiply_linked_file_without_mutation() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "linked");
        let path = task_event_path(root.path(), "linked");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let outside = root.path().join("outside-event");
        fs::write(&outside, b"outside-before\n").unwrap();
        fs::hard_link(&outside, &path).unwrap();

        assert!(append_task_event(root.path(), &task_event_frame("linked", 1)).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside-before\n");
        assert_eq!(fs::read(&path).unwrap(), b"outside-before\n");
    }

    #[cfg(unix)]
    #[test]
    fn task_event_fifo_is_rejected_without_blocking_append_or_read() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "fifo");
        let path = task_event_path(root.path(), "fifo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());

        assert!(append_task_event(root.path(), &task_event_frame("fifo", 1)).is_err());
        assert!(load_task_events_from_offset(root.path(), "fifo", 0).is_err());
        assert!(task_event_log_len(root.path(), "fifo").is_err());
    }

    #[test]
    fn task_event_sync_failure_is_explicit_and_releases_the_log_lock() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "sync");
        let path = task_event_path(root.path(), "sync");
        inject_task_event_sync_failure_once(&path);

        let error = append_task_event(root.path(), &task_event_frame("sync", 1)).unwrap_err();
        assert!(error
            .to_string()
            .contains("failed to synchronize task event log"));

        append_task_event(root.path(), &task_event_frame("sync", 2)).unwrap();
        let events = load_task_events(root.path(), "sync").unwrap();
        assert_eq!(
            events.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn task_event_append_accepts_exact_line_limit_and_rejects_one_over() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "bounded");
        let exact = task_event_frame_with_encoded_size("bounded", MAX_TASK_EVENT_LINE_BYTES);
        append_task_event(root.path(), &exact).unwrap();
        let path = task_event_path(root.path(), "bounded");
        let before = fs::read(&path).unwrap();

        let over = task_event_frame_with_encoded_size("bounded", MAX_TASK_EVENT_LINE_BYTES + 1);
        let error = append_task_event(root.path(), &over).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "event-line bytes",
                ..
            }
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn concurrent_event_appends_are_complete_and_serialized() {
        let root = Arc::new(tempdir().unwrap());
        admit_task(root.path(), "concurrent");
        let mut handles = Vec::new();
        for seq in 1..=8 {
            let root = Arc::clone(&root);
            handles.push(thread::spawn(move || {
                append_next_task_event(root.path(), "concurrent", &test_daemon_event(seq)).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let mut sequences = load_task_events(root.path(), "concurrent")
            .unwrap()
            .into_iter()
            .map(|frame| frame.seq)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
    }

    #[cfg(unix)]
    #[test]
    fn task_event_append_process_child() {
        let Some(root) = std::env::var_os("PACKET28_TASK_EVENT_APPEND_CHILD_ROOT") else {
            return;
        };
        let seq = std::env::var("PACKET28_TASK_EVENT_APPEND_CHILD_SEQ")
            .unwrap()
            .parse()
            .unwrap();
        append_next_task_event(Path::new(&root), "process", &test_daemon_event(seq)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cross_process_first_event_publication_is_serialized() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "process");
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for seq in 1..=8_u64 {
            children.push(
                Command::new(&executable)
                    .arg("--exact")
                    .arg("storage::tests::task_event_append_process_child")
                    .env("PACKET28_TASK_EVENT_APPEND_CHILD_ROOT", root.path())
                    .env("PACKET28_TASK_EVENT_APPEND_CHILD_SEQ", seq.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "event append child failed: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let mut sequences = load_task_events(root.path(), "process")
            .unwrap()
            .into_iter()
            .map(|frame| frame.seq)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
    }

    #[cfg(unix)]
    #[test]
    fn append_directory_lock_is_released_when_owner_process_exits() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "process");
        let executable = std::env::current_exe().unwrap();
        let output = Command::new(&executable)
            .arg("--exact")
            .arg("storage::tests::task_event_append_process_child")
            .env("PACKET28_TASK_EVENT_APPEND_CHILD_ROOT", root.path())
            .env("PACKET28_TASK_EVENT_APPEND_CHILD_SEQ", "1")
            .env(
                "PACKET28_TEST_EXIT_AFTER_APPEND_DIRECTORY_LOCK",
                "process.events.jsonl",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86));

        append_next_task_event(root.path(), "process", &test_daemon_event(2)).unwrap();
        assert_eq!(
            load_task_events(root.path(), "process")
                .unwrap()
                .into_iter()
                .map(|frame| frame.seq)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn event_append_retains_registry_admission_lock_through_file_append() {
        let root = Arc::new(tempdir().unwrap());
        admit_task(root.path(), "locked");
        let event_path = task_event_path(root.path(), "locked");
        fs::create_dir_all(event_path.parent().unwrap()).unwrap();
        fs::write(&event_path, b"").unwrap();
        let event_gate = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&event_path)
            .unwrap();
        FileExt::lock_exclusive(&event_gate).unwrap();

        let append_root = Arc::clone(&root);
        let append = thread::spawn(move || {
            append_task_event(append_root.path(), &task_event_frame("locked", 1))
        });

        let registry_lock_path = daemon_dir(root.path()).join(TASK_REGISTRY_LOCK_FILE_NAME);
        let registry_probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&registry_lock_path)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match FileExt::try_lock_exclusive(&registry_probe) {
                Ok(()) => {
                    FileExt::unlock(&registry_probe).unwrap();
                    assert!(
                        Instant::now() < deadline,
                        "append did not retain the shared registry lock before its file lock"
                    );
                    thread::yield_now();
                }
                Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(source) => panic!("failed to probe registry lock: {source}"),
            }
        }

        let (saved_tx, saved_rx) = mpsc::channel();
        let save_root = Arc::clone(&root);
        let save = thread::spawn(move || {
            let mut registry = load_task_registry(save_root.path()).unwrap();
            registry.tasks.get_mut("locked").unwrap().last_error = Some("saved".to_string());
            let result = save_task_registry(save_root.path(), &registry);
            saved_tx.send(()).unwrap();
            result
        });
        assert!(
            saved_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "registry replacement passed the append's admission lock"
        );

        FileExt::unlock(&event_gate).unwrap();
        append.join().unwrap().unwrap();
        save.join().unwrap().unwrap();
        saved_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(load_task_events(root.path(), "locked").unwrap().len(), 1);
    }

    #[test]
    fn task_event_reads_reject_corrupt_complete_lines_without_mutation() {
        let dir = tempdir().unwrap();
        let path = task_event_path(dir.path(), "task-demo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            concat!(
                "{\"seq\":1,\"task_id\":\"task-demo\",\"event\":{\"kind\":\"task_started\",\"occurred_at_unix\":1,\"data\":{}}}\n",
                "{not-json}\n",
                "{\"seq\":2,\"task_id\":\"task-demo\",\"event\":{\"kind\":\"task_completed\",\"occurred_at_unix\":2,\"data\":{}}}\n"
            ),
        )
        .unwrap();

        let before = fs::read(&path).unwrap();
        let error = load_task_events_from_offset(dir.path(), "task-demo", 0).unwrap_err();
        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskEventFrame { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn task_event_reads_do_not_advance_past_partial_trailing_line() {
        let dir = tempdir().unwrap();
        let path = task_event_path(dir.path(), "task-demo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let complete = "{\"seq\":1,\"task_id\":\"task-demo\",\"event\":{\"kind\":\"task_started\",\"occurred_at_unix\":1,\"data\":{}}}\n";
        fs::write(
            &path,
            format!(
                "{complete}{{\"seq\":2,\"task_id\":\"task-demo\",\"event\":{{\"kind\":\"task_completed\""
            ),
        )
        .unwrap();

        let read = load_task_events_from_offset(dir.path(), "task-demo", 0).unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].seq, 1);
        assert_eq!(read.next_offset, complete.len() as u64);
    }

    #[test]
    fn task_event_reader_rejects_offsets_inside_complete_frames() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "offset-boundary");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = Vec::new();
        serde_json::to_writer(&mut raw, &task_event_frame("offset-boundary", 1)).unwrap();
        raw.push(b'\n');
        let second_start = raw.len() as u64;
        serde_json::to_writer(&mut raw, &task_event_frame("offset-boundary", 2)).unwrap();
        raw.push(b'\n');
        fs::write(&path, &raw).unwrap();

        for offset in [1, second_start + 1] {
            let error =
                load_task_events_from_offset(root.path(), "offset-boundary", offset).unwrap_err();
            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskEventFrame { .. }
            ));
        }
        assert_eq!(fs::read(path).unwrap(), raw);
    }

    #[test]
    fn task_event_page_caps_decoded_frames_and_resumes_exactly() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "paged");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = Vec::new();
        for seq in 1..=MAX_TASK_EVENT_PAGE_FRAMES as u64 + 1 {
            serde_json::to_writer(&mut raw, &task_event_frame("paged", seq)).unwrap();
            raw.push(b'\n');
        }
        fs::write(&path, &raw).unwrap();

        let first = load_task_events_from_offset(root.path(), "paged", 0).unwrap();
        assert_eq!(first.events.len(), MAX_TASK_EVENT_PAGE_FRAMES);
        assert!(first.next_offset < raw.len() as u64);
        let second = load_task_events_from_offset(root.path(), "paged", first.next_offset).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.next_offset, raw.len() as u64);
    }

    #[test]
    fn task_event_page_rejects_a_gap_at_the_page_boundary_without_mutation() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "page-gap");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = Vec::new();
        for seq in 1..=MAX_TASK_EVENT_PAGE_FRAMES as u64 {
            serde_json::to_writer(&mut raw, &task_event_frame("page-gap", seq)).unwrap();
            raw.push(b'\n');
        }
        serde_json::to_writer(
            &mut raw,
            &task_event_frame("page-gap", MAX_TASK_EVENT_PAGE_FRAMES as u64 + 2),
        )
        .unwrap();
        raw.push(b'\n');
        fs::write(&path, &raw).unwrap();

        let first = load_task_events_from_offset(root.path(), "page-gap", 0).unwrap();
        assert_eq!(first.events.len(), MAX_TASK_EVENT_PAGE_FRAMES);
        let error =
            load_task_events_from_offset(root.path(), "page-gap", first.next_offset).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskEventFrame { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), raw);
    }

    #[test]
    fn task_event_page_caps_bytes_at_complete_line_boundary() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "paged");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line_bytes = 900 * 1024;
        let mut raw = Vec::new();
        for seq in 1..=5 {
            let mut frame = task_event_frame_with_encoded_size("paged", line_bytes);
            frame.seq = seq;
            frame.event.occurred_at_unix = seq;
            serde_json::to_writer(&mut raw, &frame).unwrap();
            raw.push(b'\n');
        }
        fs::write(&path, &raw).unwrap();

        let first = load_task_events_from_offset(root.path(), "paged", 0).unwrap();
        assert_eq!(first.events.len(), 4);
        assert_eq!(first.next_offset, (4_usize * (line_bytes + 1)) as u64);
        assert!(first.next_offset <= MAX_TASK_EVENT_PAGE_BYTES as u64);
        let second = load_task_events_from_offset(root.path(), "paged", first.next_offset).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.next_offset, raw.len() as u64);
    }

    #[test]
    fn task_event_reader_rejects_overlong_line_without_allocating_the_log() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "overlong");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_TASK_EVENT_LINE_BYTES + 1) as u64)
            .unwrap();

        let error = load_task_events_from_offset(root.path(), "overlong", 0).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "event-line bytes",
                ..
            }
        ));
    }

    #[test]
    fn task_event_writer_and_reader_reject_structurally_overbudget_frame() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "structured");
        let mut frame = task_event_frame("structured", 1);
        frame.event.data = serde_json::Value::Array(vec![
            serde_json::Value::Null;
            MAX_AUTHORITY_JSON_ENTRIES_PER_CONTAINER
                + 1
        ]);

        let write_error = append_task_event(root.path(), &frame).unwrap_err();
        assert!(matches!(
            write_error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "entries per container",
                ..
            }
        ));
        let path = task_event_path(root.path(), "structured");
        assert!(!path.exists());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = serde_json::to_vec(&frame).unwrap();
        raw.push(b'\n');
        fs::write(&path, raw).unwrap();
        let read_error = load_task_events_from_offset(root.path(), "structured", 0).unwrap_err();
        assert!(matches!(
            read_error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "entries per container",
                ..
            }
        ));
    }

    #[test]
    fn task_event_reader_fails_on_identity_mismatch_without_advancing() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "expected");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = serde_json::to_vec(&task_event_frame("different", 1)).unwrap();
        raw.push(b'\n');
        fs::write(&path, &raw).unwrap();

        let error = load_task_events_from_offset(root.path(), "expected", 0).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskEventFrame { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), raw);
    }

    #[test]
    fn task_event_reader_requires_explicit_string_identity() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "expected");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for raw in [
            br#"{"seq":1,"event":{}}"#.as_slice(),
            br#"{"seq":1,"task_id":null,"event":{}}"#.as_slice(),
            br#"{"seq":1,"task_id":7,"event":{}}"#.as_slice(),
        ] {
            let mut line = raw.to_vec();
            line.push(b'\n');
            fs::write(&path, &line).unwrap();
            let error = load_task_events_from_offset(root.path(), "expected", 0).unwrap_err();
            assert!(matches!(
                error,
                DaemonCoreError::InvalidTaskEventFrame { .. }
            ));
            assert_eq!(fs::read(&path).unwrap(), line);
        }
    }

    #[test]
    fn whole_event_reader_rejects_oversized_log_before_materialization() {
        let root = tempdir().unwrap();
        let path = task_event_path(root.path(), "oversized");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_TASK_EVENT_LOAD_BYTES + 1) as u64)
            .unwrap();

        let error = load_task_events(root.path(), "oversized").unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "whole-log bytes",
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn task_event_reader_rejects_symlink_and_hardlink_snapshots() {
        use std::os::unix::fs::symlink;

        for hard_link in [false, true] {
            let root = tempdir().unwrap();
            let path = task_event_path(root.path(), "linked");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let outside = root.path().join("outside-log");
            let mut raw = serde_json::to_vec(&task_event_frame("linked", 1)).unwrap();
            raw.push(b'\n');
            fs::write(&outside, &raw).unwrap();
            if hard_link {
                fs::hard_link(&outside, &path).unwrap();
            } else {
                symlink(&outside, &path).unwrap();
            }

            assert!(load_task_events_from_offset(root.path(), "linked", 0).is_err());
            assert_eq!(fs::read(&outside).unwrap(), raw);
        }
    }

    #[test]
    fn task_event_reader_rejects_case_alias_namespace() {
        let root = tempdir().unwrap();
        let canonical = task_event_path(root.path(), "live");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        let alias = canonical.parent().unwrap().join("LIVE.events.jsonl");
        fs::write(&alias, b"alias\n").unwrap();

        let error = load_task_events_from_offset(root.path(), "live", 0).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskStorageIdentifier { .. }
        ));
        assert_eq!(fs::read(alias).unwrap(), b"alias\n");
        assert!(!directory_contains_exact_name(
            canonical.parent().unwrap(),
            "live.events.jsonl"
        ));
    }

    #[test]
    fn task_event_reader_rejects_unicode_alias_namespace() {
        let root = tempdir().unwrap();
        let canonical = task_event_path(root.path(), "k");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        let alias = canonical.parent().unwrap().join("\u{212a}.events.jsonl");
        fs::write(&alias, b"unicode-alias\n").unwrap();

        let error = load_task_events_from_offset(root.path(), "k", 0).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskStorageIdentifier { .. }
        ));
        assert_eq!(fs::read(alias).unwrap(), b"unicode-alias\n");
        assert!(!directory_contains_exact_name(
            canonical.parent().unwrap(),
            "k.events.jsonl"
        ));
    }

    #[test]
    fn atomic_temp_paths_are_unique_for_same_target() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let first = atomic_temp_path(&path);
        let second = atomic_temp_path(&path);
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(dir.path()));
        assert_eq!(second.parent(), Some(dir.path()));
    }

    #[test]
    fn atomic_replacement_reports_parent_sync_failure_after_visible_rename() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.json");
        fs::write(&path, b"before").unwrap();
        inject_parent_sync_failure_once(&path);

        let error = write_atomically(&path, b"after").unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to synchronize atomic replacement directory"));
        assert_eq!(fs::read(path).unwrap(), b"after");
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_save_uses_the_retained_daemon_capability() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let daemon = daemon_dir(root.path());
        let held = root.path().join(".packet28/daemon-held");
        let outside = tempdir().unwrap();
        let outside_registry = outside.path().join(TASK_REGISTRY_FILE_NAME);
        fs::write(&outside_registry, b"outside").unwrap();
        let mut registry = TaskRegistry::default();
        registry.tasks.insert(
            "anchored".to_string(),
            TaskRecord {
                task_id: "anchored".to_string(),
                lifecycle: TaskLifecycle::Idle,
                ..TaskRecord::default()
            },
        );

        save_task_registry_with_observer(root.path(), &registry, || {
            fs::rename(&daemon, &held).map_err(|source| {
                DaemonCoreError::io("failed to move daemon root during test", &daemon, source)
            })?;
            symlink(outside.path(), &daemon).map_err(|source| {
                DaemonCoreError::io("failed to replace daemon root during test", &daemon, source)
            })
        })
        .unwrap();

        let saved: TaskRegistry =
            serde_json::from_slice(&fs::read(held.join(TASK_REGISTRY_FILE_NAME)).unwrap()).unwrap();
        assert!(saved.tasks.contains_key("anchored"));
        assert_eq!(fs::read(outside_registry).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_save_cleans_a_pre_rename_crash_temp_before_retry() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        let replacement = TaskRegistry {
            tasks: BTreeMap::from([(
                "replacement".to_string(),
                TaskRecord {
                    task_id: "replacement".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry(root.path(), &existing).unwrap();

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = save_task_registry_with_observers(
                root.path(),
                &replacement,
                || Ok(()),
                || panic!("simulated process death after registry temp fsync"),
            );
        }));
        assert!(crashed.is_err());
        let daemon = daemon_dir(root.path());
        let crash_temps = fs::read_dir(&daemon)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| generated_name_matches(name, TASK_REGISTRY_WRITE_TEMP_PREFIX))
            .collect::<Vec<_>>();
        assert_eq!(crash_temps.len(), 1);
        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("existing"));

        save_task_registry(root.path(), &replacement).unwrap();

        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("replacement"));
        assert!(!fs::read_dir(daemon)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .any(|name| generated_name_matches(&name, TASK_REGISTRY_WRITE_TEMP_PREFIX)));
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_save_rejects_a_symlinked_lock() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let outside = root.path().join("outside-lock");
        fs::write(&outside, b"keep").unwrap();
        symlink(
            &outside,
            daemon_dir(root.path()).join(TASK_REGISTRY_LOCK_FILE_NAME),
        )
        .unwrap();

        let error = save_task_registry(root.path(), &TaskRegistry::default()).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert_eq!(fs::read(outside).unwrap(), b"keep");
        assert!(!task_registry_path(root.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_save_rejects_lock_replacement_before_mutation() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let lock_path = daemon_dir(root.path()).join(TASK_REGISTRY_LOCK_FILE_NAME);
        let detached = daemon_dir(root.path()).join("detached-task-registry.lock");

        let error = save_task_registry_with_observer(root.path(), &TaskRegistry::default(), || {
            replace_locked_path(&lock_path, &detached).map_err(|source| {
                DaemonCoreError::io(
                    "failed to replace task registry lock during test",
                    &lock_path,
                    source,
                )
            })
        })
        .unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::StorageMutationAuthorityLost { .. }
        ));
        assert!(!task_registry_path(root.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn watch_registry_rejects_symlinked_registry_and_lock_aliases() {
        use std::os::unix::fs::symlink;

        for alias_lock in [false, true] {
            let root = tempdir().unwrap();
            ensure_daemon_dir(root.path()).unwrap();
            let outside = root.path().join(if alias_lock {
                "outside-watch-lock"
            } else {
                "outside-watch-registry"
            });
            fs::write(&outside, b"keep").unwrap();
            let alias = if alias_lock {
                daemon_dir(root.path()).join(WATCH_REGISTRY_LOCK_FILE_NAME)
            } else {
                watch_registry_path(root.path())
            };
            symlink(&outside, &alias).unwrap();

            let error = save_watch_registry(root.path(), &WatchRegistry::default()).unwrap_err();

            assert!(matches!(error, DaemonCoreError::Io { .. }));
            assert_eq!(fs::read(&outside).unwrap(), b"keep");
            assert!(fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[cfg(unix)]
    #[test]
    fn watch_registry_rejects_hardlinked_registry_and_lock_aliases() {
        for alias_lock in [false, true] {
            let root = tempdir().unwrap();
            ensure_daemon_dir(root.path()).unwrap();
            let outside = root.path().join(if alias_lock {
                "outside-watch-lock"
            } else {
                "outside-watch-registry"
            });
            fs::write(&outside, b"keep").unwrap();
            let alias = if alias_lock {
                daemon_dir(root.path()).join(WATCH_REGISTRY_LOCK_FILE_NAME)
            } else {
                watch_registry_path(root.path())
            };
            fs::hard_link(&outside, &alias).unwrap();

            let error = save_watch_registry(root.path(), &WatchRegistry::default()).unwrap_err();

            assert!(matches!(error, DaemonCoreError::Io { .. }));
            assert_eq!(fs::read(&outside).unwrap(), b"keep");
            assert_eq!(fs::metadata(alias).unwrap().len(), 4);
        }
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_supports_a_symlinked_workspace_root() {
        use std::os::unix::fs::symlink;

        let real = tempdir().unwrap();
        let links = tempdir().unwrap();
        let linked_root = links.path().join("workspace");
        symlink(real.path(), &linked_root).unwrap();
        let mut registry = TaskRegistry::default();
        registry.tasks.insert(
            "linked".to_string(),
            TaskRecord {
                task_id: "linked".to_string(),
                lifecycle: TaskLifecycle::Idle,
                ..TaskRecord::default()
            },
        );

        save_task_registry(&linked_root, &registry).unwrap();
        let loaded = load_task_registry(&linked_root).unwrap();

        assert!(loaded.tasks.contains_key("linked"));
        assert!(task_registry_path(real.path()).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn task_registry_rejects_a_symlinked_state_root_without_external_mutation() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), root.path().join(".packet28")).unwrap();

        let error = save_task_registry(root.path(), &TaskRegistry::default()).unwrap_err();

        assert!(matches!(error, DaemonCoreError::Io { .. }));
        assert!(!outside.path().join("daemon").exists());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn task_registry_encoding_accepts_the_exact_supported_limit() {
        let root = tempdir().unwrap();
        let path = task_registry_path(root.path());
        let registry = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES);

        let encoded = encode_task_registry(&path, &registry).unwrap();

        assert_eq!(encoded.len(), MAX_TASK_REGISTRY_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn escaped_exact_limit_registry_fits_its_complete_retention_journal() {
        let root = tempdir().unwrap();
        let task_id = "a".repeat(MAX_TASK_STORAGE_KEY_BYTES);
        let registry =
            registry_with_id_pattern_and_encoded_size(&task_id, "\"\\\n", MAX_TASK_REGISTRY_BYTES);

        save_task_registry(root.path(), &registry).unwrap();
        let loaded = load_task_registry(root.path()).unwrap();

        assert_eq!(
            fs::metadata(task_registry_path(root.path())).unwrap().len(),
            MAX_TASK_REGISTRY_BYTES as u64
        );
        let last_error = loaded
            .tasks
            .get(&task_id)
            .and_then(|record| record.last_error.as_deref())
            .unwrap();
        assert!(last_error.contains('"'));
        assert!(last_error.contains('\\'));
        assert!(last_error.contains('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn journal_envelope_rejection_preserves_the_old_readable_registry() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();
        let replacement = TaskRegistry {
            tasks: BTreeMap::from([(
                "replacement".to_string(),
                TaskRecord {
                    task_id: "replacement".to_string(),
                    last_error: Some("\"\\\n".repeat(64)),
                    ..TaskRecord::default()
                },
            )]),
        };
        crate::retention::inject_task_registry_journal_limit_once(1);

        let error = save_task_registry(root.path(), &replacement).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryRetentionEnvelopeTooLarge { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        let loaded = load_task_registry(root.path()).unwrap();
        assert!(loaded.tasks.contains_key("existing"));
        assert!(!loaded.tasks.contains_key("replacement"));
    }

    #[test]
    fn task_registry_identifier_accepts_the_exact_storage_key_limit() {
        let root = tempdir().unwrap();
        let task_id = "a".repeat(MAX_TASK_STORAGE_KEY_BYTES);
        let registry = TaskRegistry {
            tasks: BTreeMap::from([(
                task_id.clone(),
                TaskRecord {
                    task_id: task_id.clone(),
                    ..TaskRecord::default()
                },
            )]),
        };

        save_task_registry(root.path(), &registry).unwrap();

        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key(&task_id));
        assert_eq!(
            task_event_path(root.path(), &task_id)
                .file_name()
                .unwrap()
                .as_encoded_bytes()
                .len(),
            MAX_TASK_STORE_COMPONENT_BYTES
        );
    }

    #[test]
    fn task_registry_identifier_one_over_storage_key_limit_is_rejected_without_mutation() {
        let root = tempdir().unwrap();
        let task_id = "a".repeat(MAX_TASK_STORAGE_KEY_BYTES + 1);
        let registry = TaskRegistry {
            tasks: BTreeMap::from([(
                task_id.clone(),
                TaskRecord {
                    task_id,
                    ..TaskRecord::default()
                },
            )]),
        };

        let error = save_task_registry(root.path(), &registry).unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn filesystem_aliasing_registry_keys_are_rejected_and_preserve_old_state() {
        for (first, second) in [("a/b", "a?b"), ("Task", "task"), ("λ", "界")] {
            let root = tempdir().unwrap();
            let existing = TaskRegistry {
                tasks: BTreeMap::from([(
                    "existing".to_string(),
                    TaskRecord {
                        task_id: "existing".to_string(),
                        ..TaskRecord::default()
                    },
                )]),
            };
            save_task_registry(root.path(), &existing).unwrap();
            let path = task_registry_path(root.path());
            let before = fs::read(&path).unwrap();
            let colliding = TaskRegistry {
                tasks: BTreeMap::from([
                    (
                        first.to_string(),
                        TaskRecord {
                            task_id: first.to_string(),
                            ..TaskRecord::default()
                        },
                    ),
                    (
                        second.to_string(),
                        TaskRecord {
                            task_id: second.to_string(),
                            ..TaskRecord::default()
                        },
                    ),
                ]),
            };

            let error = save_task_registry(root.path(), &colliding).unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(load_task_registry(root.path())
                .unwrap()
                .tasks
                .contains_key("existing"));
        }
    }

    #[test]
    fn windows_reserved_storage_keys_are_classified_with_or_without_event_suffix() {
        for reserved in [
            "CON", "con", "PRN", "aux", "NUL", "COM1", "com9", "LPT1", "lpt9",
        ] {
            assert!(windows_storage_key_is_reserved(reserved), "{reserved}");
        }
        for allowed in ["CONSOLE", "COM0", "COM10", "LPT0", "task"] {
            assert!(!windows_storage_key_is_reserved(allowed), "{allowed}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_task_registry_path_accepts_the_exact_supported_limit() {
        let root = tempdir().unwrap();
        let registry = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES);

        save_task_registry(root.path(), &registry).unwrap();
        let loaded = load_task_registry(root.path()).unwrap();

        assert_eq!(
            fs::metadata(task_registry_path(root.path())).unwrap().len(),
            MAX_TASK_REGISTRY_BYTES as u64
        );
        assert!(loaded.tasks.contains_key("sized-registry"));
    }

    #[test]
    fn portable_task_registry_path_accepts_the_exact_supported_limit() {
        let root = tempdir().unwrap();
        let registry = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES);

        save_task_registry_portable(root.path(), &registry).unwrap();
        let loaded = load_task_registry_portable(root.path()).unwrap();

        assert_eq!(
            fs::metadata(task_registry_path(root.path())).unwrap().len(),
            MAX_TASK_REGISTRY_BYTES as u64
        );
        assert!(loaded.tasks.contains_key("sized-registry"));
    }

    #[test]
    fn oversized_task_registry_is_rejected_before_state_mutation() {
        let root = tempdir().unwrap();
        let path = task_registry_path(root.path());
        let registry = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES + 1);

        let error = save_task_registry(root.path(), &registry).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == (MAX_TASK_REGISTRY_BYTES + 1) as u64
                && max_bytes == MAX_TASK_REGISTRY_BYTES as u64
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn portable_oversized_task_registry_rejection_preserves_existing_state() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry_portable(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();
        let oversized = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES + 1);

        let error = save_task_registry_portable(root.path(), &oversized).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == (MAX_TASK_REGISTRY_BYTES + 1) as u64
                && max_bytes == MAX_TASK_REGISTRY_BYTES as u64
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(load_task_registry_portable(root.path())
            .unwrap()
            .tasks
            .contains_key("existing"));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_persisted_task_registry_has_a_typed_load_error() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_TASK_REGISTRY_BYTES as u64 + 1).unwrap();

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_TASK_REGISTRY_BYTES as u64 + 1
                && max_bytes == MAX_TASK_REGISTRY_BYTES as u64
        ));
    }

    #[test]
    fn portable_oversized_persisted_task_registry_has_a_typed_load_error() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_TASK_REGISTRY_BYTES as u64 + 1).unwrap();

        let error = load_task_registry_portable(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_TASK_REGISTRY_BYTES as u64 + 1
                && max_bytes == MAX_TASK_REGISTRY_BYTES as u64
        ));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_persisted_watch_registry_has_a_typed_load_error() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = watch_registry_path(root.path());
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_WATCH_REGISTRY_BYTES as u64 + 1).unwrap();

        let error = load_watch_registry(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::WatchRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_WATCH_REGISTRY_BYTES as u64 + 1
                && max_bytes == MAX_WATCH_REGISTRY_BYTES as u64
        ));
    }

    #[test]
    fn portable_oversized_persisted_watch_registry_has_a_typed_load_error() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = watch_registry_path(root.path());
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_WATCH_REGISTRY_BYTES as u64 + 1).unwrap();

        let error = read_watch_registry(&path).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::WatchRegistryTooLarge {
                path: error_path,
                encoded_bytes,
                max_bytes,
            } if error_path == path
                && encoded_bytes == MAX_WATCH_REGISTRY_BYTES as u64 + 1
                && max_bytes == MAX_WATCH_REGISTRY_BYTES as u64
        ));
    }

    #[test]
    fn oversized_task_registry_rejection_preserves_existing_state() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();
        let oversized = registry_with_encoded_size(MAX_TASK_REGISTRY_BYTES + 1);

        let error = save_task_registry(root.path(), &oversized).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::TaskRegistryTooLarge { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("existing"));
    }

    #[test]
    fn mismatched_task_registry_identifier_is_rejected_before_state_mutation() {
        let root = tempdir().unwrap();
        let registry = TaskRegistry {
            tasks: BTreeMap::from([(
                "registry-key".to_string(),
                TaskRecord {
                    task_id: "embedded-id".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };

        let error = save_task_registry(root.path(), &registry).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskRegistry { ref path, .. }
                if path == &task_registry_path(root.path())
        ));
        assert!(!root.path().join(".packet28").exists());
    }

    #[test]
    fn mismatched_task_registry_rejection_preserves_existing_state() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();
        let invalid = TaskRegistry {
            tasks: BTreeMap::from([(
                "registry-key".to_string(),
                TaskRecord {
                    task_id: "embedded-id".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };

        let error = save_task_registry(root.path(), &invalid).unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(&path).unwrap(), before);
        let loaded = load_task_registry(root.path()).unwrap();
        assert!(loaded.tasks.contains_key("existing"));
        assert_eq!(loaded.tasks.len(), 1);
    }

    #[test]
    fn portable_mismatched_task_registry_rejection_preserves_existing_state() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry_portable(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();
        let invalid = TaskRegistry {
            tasks: BTreeMap::from([(
                "registry-key".to_string(),
                TaskRecord {
                    task_id: "embedded-id".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };

        let error = save_task_registry_portable(root.path(), &invalid).unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(&path).unwrap(), before);
        let loaded = load_task_registry_portable(root.path()).unwrap();
        assert!(loaded.tasks.contains_key("existing"));
        assert_eq!(loaded.tasks.len(), 1);
    }

    #[test]
    fn persisted_mismatched_task_registry_is_rejected_on_load() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        fs::write(
            &path,
            br#"{"tasks":{"registry-key":{"task_id":"embedded-id"}}}"#,
        )
        .unwrap();

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::InvalidTaskRegistry { path: error_path, .. }
                if error_path == path
        ));
    }

    #[test]
    fn duplicate_task_registry_keys_are_rejected_instead_of_last_value_winning() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        fs::write(
            &path,
            br#"{"tasks":{"duplicate":{"task_id":"duplicate","last_error":"first"},"duplicate":{"task_id":"duplicate","last_error":"second"}}}"#,
        )
        .unwrap();

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(
            &error,
            DaemonCoreError::Json {
                path: error_path,
                ..
            } if error_path == &path
        ));
        assert!(error.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    fn duplicate_top_level_registry_keys_are_rejected() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        fs::write(&path, br#"{"tasks":{},"tasks":{}}"#).unwrap();

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(
            &error,
            DaemonCoreError::Json {
                path: error_path,
                ..
            } if error_path == &path
        ));
        assert!(error.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    fn present_task_registry_requires_an_object_valued_tasks_field() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());

        for raw in [
            br#"{}"#.as_slice(),
            br#"{"future":true}"#.as_slice(),
            br#"{"tasks":null}"#.as_slice(),
            br#"{"tasks":[]}"#.as_slice(),
            br#"[]"#.as_slice(),
        ] {
            fs::write(&path, raw).unwrap();
            let error = load_task_registry(root.path()).unwrap_err();
            assert!(matches!(error, DaemonCoreError::Json { .. }));
        }

        fs::write(&path, br#"{"tasks":{}}"#).unwrap();
        assert!(load_task_registry(root.path()).unwrap().tasks.is_empty());
    }

    #[test]
    fn normal_registry_load_change_save_preserves_unknown_root_and_record_fields() {
        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        fs::write(
            &path,
            br#"{
                "future_root": {"enabled": true},
                "tasks": {
                    "live": {
                        "task_id": "live",
                        "running": false,
                        "future_record": {"version": 7}
                    }
                }
            }"#,
        )
        .unwrap();

        let mut registry = load_task_registry(root.path()).unwrap();
        registry.tasks.get_mut("live").unwrap().lifecycle = TaskLifecycle::Running;
        save_task_registry(root.path(), &registry).unwrap();

        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["future_root"]["enabled"], true);
        assert_eq!(saved["tasks"]["live"]["future_record"]["version"], 7);
        assert_eq!(saved["tasks"]["live"]["running"], true);
    }

    #[test]
    fn paired_checkpoint_drops_completed_recovery_lifecycle_marker() {
        let root = tempdir().unwrap();
        let mut registry = TaskRegistry {
            tasks: BTreeMap::from([(
                "recovered".to_string(),
                TaskRecord {
                    task_id: "recovered".to_string(),
                    lifecycle: TaskLifecycle::RunningRecoveredReplan,
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();

        registry.tasks.get_mut("recovered").unwrap().lifecycle = TaskLifecycle::Idle;
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();

        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(task_registry_path(root.path())).unwrap()).unwrap();
        assert!(raw["tasks"]["recovered"].get("recovered_replan").is_none());
        assert_eq!(
            load_task_registry(root.path()).unwrap().tasks["recovered"].lifecycle,
            TaskLifecycle::Idle
        );
    }

    #[test]
    fn new_registry_task_cannot_adopt_exact_or_aliasing_managed_entries() {
        for (event_namespace, alias_spelling) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let root = tempdir().unwrap();
            admit_task(root.path(), "existing");
            let registry_path = task_registry_path(root.path());
            let registry_before = fs::read(&registry_path).unwrap();
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
                fs::write(managed.join("payload"), b"artifact-before").unwrap();
            }

            let error =
                save_task_registry(root.path(), &registry_for_tasks(&["existing", "new-task"]))
                    .unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
            if event_namespace {
                assert_eq!(fs::read(&managed).unwrap(), b"event-before\n");
            } else {
                assert_eq!(
                    fs::read(managed.join("payload")).unwrap(),
                    b"artifact-before"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_registry_task_cannot_adopt_unicode_aliasing_managed_entries() {
        for (event_namespace, task_id, actual_name) in [
            (false, "k", "\u{212a}"),
            (false, "s", "\u{017f}"),
            (true, "k", "\u{212a}.events.jsonl"),
            (true, "s", "\u{017f}.events.jsonl"),
        ] {
            let root = tempdir().unwrap();
            admit_task(root.path(), "existing");
            let registry_path = task_registry_path(root.path());
            let registry_before = fs::read(&registry_path).unwrap();
            let namespace = if event_namespace {
                task_events_dir(root.path())
            } else {
                task_artifacts_dir(root.path())
            };
            fs::create_dir_all(&namespace).unwrap();
            let managed = namespace.join(actual_name);
            if event_namespace {
                fs::write(&managed, b"unicode-event-before\n").unwrap();
            } else {
                fs::create_dir(&managed).unwrap();
                fs::write(managed.join("payload"), b"unicode-artifact-before").unwrap();
            }

            let error =
                save_task_registry(root.path(), &registry_for_tasks(&["existing", task_id]))
                    .unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
            assert!(managed.exists());
        }
    }

    #[test]
    fn previously_admitted_task_keeps_exact_artifact_and_event_bindings() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "existing");
        let artifact = task_artifacts_dir(root.path()).join("existing");
        let event = task_event_path(root.path(), "existing");
        fs::create_dir_all(&artifact).unwrap();
        fs::write(artifact.join("payload"), b"artifact").unwrap();
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        fs::write(&event, b"event\n").unwrap();
        let mut registry = load_task_registry(root.path()).unwrap();
        registry.tasks.get_mut("existing").unwrap().last_error = Some("changed".to_string());

        save_task_registry(root.path(), &registry).unwrap();

        assert_eq!(
            load_task_registry(root.path())
                .unwrap()
                .tasks
                .get("existing")
                .unwrap()
                .last_error
                .as_deref(),
            Some("changed")
        );
        assert_eq!(fs::read(artifact.join("payload")).unwrap(), b"artifact");
        assert_eq!(fs::read(event).unwrap(), b"event\n");
    }

    #[cfg(unix)]
    #[test]
    fn registry_save_rejects_multiply_linked_existing_event_binding() {
        let root = tempdir().unwrap();
        admit_task(root.path(), "existing");
        let event = task_event_path(root.path(), "existing");
        fs::create_dir_all(event.parent().unwrap()).unwrap();
        let outside = root.path().join("outside-event");
        fs::write(&outside, b"event\n").unwrap();
        fs::hard_link(&outside, &event).unwrap();
        let registry_path = task_registry_path(root.path());
        let before = fs::read(&registry_path).unwrap();
        let mut registry = load_task_registry(root.path()).unwrap();
        registry.tasks.get_mut("existing").unwrap().last_error = Some("changed".to_string());

        let error = save_task_registry(root.path(), &registry).unwrap_err();

        assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
        assert_eq!(fs::read(registry_path).unwrap(), before);
        assert_eq!(fs::read(outside).unwrap(), b"event\n");
    }

    #[test]
    fn duplicate_nested_registry_keys_are_rejected() {
        let error = decode_json_value_without_duplicate_keys(
            br#"{"tasks":{"task":{"task_id":"task","metadata":{"future":1,"future":2}}}}"#,
            AuthorityJsonProfile::TaskRegistry,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    fn nonportable_registry_keys_are_rejected_and_preserve_old_state() {
        let root = tempdir().unwrap();
        let existing = TaskRegistry {
            tasks: BTreeMap::from([(
                "existing".to_string(),
                TaskRecord {
                    task_id: "existing".to_string(),
                    ..TaskRecord::default()
                },
            )]),
        };
        save_task_registry(root.path(), &existing).unwrap();
        let path = task_registry_path(root.path());
        let before = fs::read(&path).unwrap();

        for task_id in ["", "   ", "Task", "a/b", "λ"] {
            let invalid = TaskRegistry {
                tasks: BTreeMap::from([(
                    task_id.to_string(),
                    TaskRecord {
                        task_id: task_id.to_string(),
                        ..TaskRecord::default()
                    },
                )]),
            };

            let error = save_task_registry(root.path(), &invalid).unwrap_err();

            assert!(matches!(error, DaemonCoreError::InvalidTaskRegistry { .. }));
            assert_eq!(fs::read(&path).unwrap(), before);
        }

        assert!(load_task_registry(root.path())
            .unwrap()
            .tasks
            .contains_key("existing"));
    }

    #[test]
    fn registry_lock_path_is_hidden_sibling() {
        let dir = tempdir().unwrap();
        let registry = task_registry_path(dir.path());

        assert_eq!(
            registry_lock_path(&registry),
            daemon_dir(dir.path()).join(".task-registry-v1.json.lock")
        );
    }

    #[test]
    fn save_task_registry_waits_for_registry_lock() {
        let dir = tempdir().unwrap();
        ensure_daemon_dir(dir.path()).unwrap();
        let lock_path = registry_lock_path(&task_registry_path(dir.path()));
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lock_file).unwrap();

        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            tx.send(save_task_registry(&root, &TaskRegistry::default()))
                .unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(75)).is_err());
        FileExt::unlock(&lock_file).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn load_task_registry_waits_for_registry_lock() {
        let dir = tempdir().unwrap();
        save_task_registry(dir.path(), &TaskRegistry::default()).unwrap();
        let lock_path = registry_lock_path(&task_registry_path(dir.path()));
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lock_file).unwrap();

        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || tx.send(load_task_registry(&root)).unwrap());

        assert!(rx.recv_timeout(Duration::from_millis(75)).is_err());
        FileExt::unlock(&lock_file).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn conditional_registry_removal_preserves_a_changed_record() {
        let dir = tempdir().unwrap();
        let original = TaskRecord {
            task_id: "task".to_string(),
            lifecycle: TaskLifecycle::Idle,
            last_completed_at_unix: Some(10),
            ..TaskRecord::default()
        };
        let expected =
            BTreeMap::from([("task".to_string(), serde_json::to_vec(&original).unwrap())]);
        let mut changed = original;
        changed.lifecycle = TaskLifecycle::Running;
        let registry = TaskRegistry {
            tasks: BTreeMap::from([("task".to_string(), changed)]),
        };
        save_task_registry(dir.path(), &registry).unwrap();

        assert!(!remove_task_registry_records_if_unchanged(dir.path(), &expected).unwrap());
        assert!(load_task_registry(dir.path())
            .unwrap()
            .tasks
            .contains_key("task"));
    }

    #[cfg(unix)]
    #[test]
    fn authority_json_structural_amplification_child() {
        if std::env::var_os("PACKET28_AUTHORITY_JSON_AMPLIFICATION_CHILD").is_none() {
            return;
        }

        let root = tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        let elements = 1_000_000_usize;
        let mut raw = Vec::with_capacity(elements.saturating_mul(5).saturating_add(32));
        raw.extend_from_slice(br#"{"tasks":{},"future":["#);
        for index in 0..elements {
            if index > 0 {
                raw.push(b',');
            }
            raw.extend_from_slice(b"null");
        }
        raw.extend_from_slice(b"]}");
        assert!(raw.len() < MAX_TASK_REGISTRY_BYTES);
        let before = blake3::hash(&raw);
        fs::write(&path, &raw).unwrap();
        drop(raw);

        let error = load_task_registry(root.path()).unwrap_err();

        assert!(matches!(
            error,
            DaemonCoreError::AuthorityJsonLimitExceeded {
                resource: "entries per container",
                ..
            }
        ));
        assert_eq!(blake3::hash(&fs::read(&path).unwrap()), before);
        assert!(!task_artifacts_dir(root.path()).exists());
        assert!(!task_events_dir(root.path()).exists());

        let peak_rss = peak_resident_set_bytes();
        assert!(
            peak_rss < 256 * 1024 * 1024,
            "authority preflight used {peak_rss} peak resident bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn authority_json_structural_amplification_is_bounded_in_a_subprocess() {
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("storage::tests::authority_json_structural_amplification_child")
            .arg("--nocapture")
            .env("PACKET28_AUTHORITY_JSON_AMPLIFICATION_CHILD", "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "amplification child failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("authority amplification child exceeded 10-second deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn peak_resident_set_bytes() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `usage` points to writable storage for one `rusage`, and
        // `RUSAGE_SELF` is a valid selector. A successful call initializes it.
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(
            result,
            0,
            "getrusage failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: success from `getrusage` initialized every field.
        let usage = unsafe { usage.assume_init() };
        #[cfg(target_vendor = "apple")]
        {
            usage.ru_maxrss as u64
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            (usage.ru_maxrss as u64).saturating_mul(1024)
        }
    }
}
