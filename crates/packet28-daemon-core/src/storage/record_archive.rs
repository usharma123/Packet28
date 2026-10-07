//! Evidence-preserving archival of oversized task-registry records.
//!
//! A dormant task record that grows past the registry pagination bound stays
//! authoritative but cannot be listed. Archival keeps the task identity and
//! moves the complete original record into an immutable, owner-only file named
//! by the blake3 digest of its exact bytes. The registry then carries a compact
//! tombstone whose [`TaskRecordArchive`] pointer authenticates that file.
//!
//! The archive is published, synchronized, and read back before the tombstone
//! is staged, so every durable tombstone names a durable archive. A crash
//! between the two leaves only an unreferenced archive beside the unchanged
//! original record; a retry with the same record reuses the same file.
//!
//! The archive lives in the task's own artifact namespace
//! (`.packet28/task/<task-id>/record-archive/`), so full task retention removes
//! it together with the registry record, event log, and artifacts that it
//! already owns, and never removes it for any other task.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::OsStr;
use std::path::Path;

use packet28_daemon_protocol::paths::{task_artifact_dir, TaskStorageId, TASK_ARTIFACTS_DIR_NAME};
use packet28_daemon_protocol::task::{
    TaskRecord, TaskRecordArchive, TaskRegistry, TASK_RECORD_ARCHIVE_SCHEMA_VERSION,
};

#[cfg(unix)]
use crate::capability::CapabilityDir;
use crate::storage::MAX_TASK_REGISTRY_BYTES;
use crate::{DaemonCoreError, Result};

/// Directory inside a task artifact namespace that holds archived records.
pub const TASK_RECORD_ARCHIVE_DIR_NAME: &str = "record-archive";
/// Suffix of one archived-record file; the stem is the blake3 hex digest.
pub const TASK_RECORD_ARCHIVE_FILE_SUFFIX: &str = ".task-record.json";
/// Largest compact-JSON value kept verbatim in a tombstone field.
pub const MAX_TASK_RECORD_TOMBSTONE_FIELD_BYTES: usize = 4 * 1024;
/// Largest compact-JSON tombstone that archival will commit.
pub const MAX_TASK_RECORD_TOMBSTONE_BYTES: usize = 64 * 1024;
/// Reason recorded for operator-requested archival of an oversized record.
pub const TASK_RECORD_ARCHIVE_REASON_OVERSIZED: &str = "oversized_record";

const DIGEST_PREFIX: &str = "blake3:";
#[cfg(unix)]
const ARCHIVE_WRITE_TEMP_PREFIX: &str = ".record-archive-write";
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: rustix::fs::RawMode = 0o700;

/// Fields that carry identity, lifecycle, sequence authority, relationships,
/// or provenance. A tombstone never drops them, whatever their size.
const PROTECTED_TOMBSTONE_FIELDS: &[&str] = &[
    "task_id",
    "running",
    "cancel_requested",
    "pending_replan",
    "cancelled",
    "recovered_replan",
    "watch_ids",
    "last_event_seq",
    "superseded_by",
    "recovered_from",
    "archived",
];

/// Top-level task-record values that a newer build persisted and this build
/// does not model, keyed by field name. Keys never name a known field.
pub type TaskRecordForwardFields = BTreeMap<String, serde_json::Value>;

/// Prepared archive bytes and the tombstone that will replace the original.
#[derive(Debug, Clone)]
pub struct PreparedTaskRecordArchive {
    /// Exact compact-JSON bytes of the complete original record, including
    /// every forward field.
    pub bytes: Vec<u8>,
    /// Compact replacement carrying the archive pointer.
    pub tombstone: TaskRecord,
    /// Forward fields of the original that the archive includes.
    pub forward_fields: TaskRecordForwardFields,
    /// Forward fields small enough to stay in the tombstone. Checkpoint
    /// encoding keeps them from the raw authority; every other forward field
    /// is listed in the pointer's `omitted_fields` and is not carried over.
    pub retained_forward_fields: TaskRecordForwardFields,
    /// The pointer stored in [`Self::tombstone`] when it was prepared.
    pointer: TaskRecordArchive,
}

impl PreparedTaskRecordArchive {
    /// Returns the archive pointer stored in [`Self::tombstone`].
    pub fn pointer(&self) -> &TaskRecordArchive {
        &self.pointer
    }
}

/// Encodes `record` exactly as it is archived and sized.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Json`] if the record cannot be encoded.
pub fn encode_task_record_compact(record: &TaskRecord) -> Result<Vec<u8>> {
    serde_json::to_vec(record).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode task record for archival of",
            Path::new(&record.task_id),
            source,
        )
    })
}

/// Returns the compact-JSON size of `record` without retaining its encoding.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Json`] if the record cannot be encoded.
pub fn task_record_encoded_len(record: &TaskRecord) -> Result<u64> {
    struct Counter(u64);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, record).map_err(|source| {
        DaemonCoreError::json(
            "failed to size task record",
            Path::new(&record.task_id),
            source,
        )
    })?;
    Ok(counter.0)
}

/// Returns whether `field` is a top-level key of the known task-record schema.
pub fn is_known_task_record_field(field: &str) -> bool {
    static KNOWN: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    KNOWN
        .get_or_init(|| {
            let mut known = match serde_json::to_value(TaskRecord::default()) {
                Ok(serde_json::Value::Object(fields)) => fields.keys().cloned().collect(),
                _ => std::collections::BTreeSet::new(),
            };
            known.extend(
                crate::storage::KNOWN_OPTIONAL_TASK_RECORD_FIELDS
                    .iter()
                    .map(|field| (*field).to_string()),
            );
            known
        })
        .contains(field)
}

/// Returns the forward fields of one persisted task-record object: every
/// top-level value outside the known schema, exactly the values that
/// checkpoint encoding preserves when it replaces the record.
pub fn task_record_forward_fields(
    record: &serde_json::Map<String, serde_json::Value>,
) -> TaskRecordForwardFields {
    record
        .iter()
        .filter(|(field, _)| !is_known_task_record_field(field))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect()
}

/// Encodes `record` with its `forward` fields as one compact JSON object.
///
/// The known fields keep their [`encode_task_record_compact`] encoding and
/// order; forward fields follow in key order. Without forward fields the bytes
/// equal [`encode_task_record_compact`].
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] if a forward key names a
/// known field, and [`DaemonCoreError::Json`] for encoding failures.
pub fn encode_task_record_with_forward_fields(
    record: &TaskRecord,
    forward: &TaskRecordForwardFields,
) -> Result<Vec<u8>> {
    let mut bytes = encode_task_record_compact(record)?;
    if forward.is_empty() {
        return Ok(bytes);
    }
    if let Some(field) = forward
        .keys()
        .find(|field| is_known_task_record_field(field))
    {
        return Err(DaemonCoreError::InvalidTaskRegistry {
            path: Path::new(&record.task_id).to_path_buf(),
            message: format!("forward task-record field {field:?} names a known field"),
        });
    }
    if bytes.pop() != Some(b'}') {
        return Err(DaemonCoreError::InvalidTaskRegistry {
            path: Path::new(&record.task_id).to_path_buf(),
            message: "task record did not encode as a JSON object".to_string(),
        });
    }
    for (field, value) in forward {
        bytes.push(b',');
        bytes.extend(encode_forward_value(
            &record.task_id,
            &serde_json::json!(field),
        )?);
        bytes.push(b':');
        bytes.extend(encode_forward_value(&record.task_id, value)?);
    }
    bytes.push(b'}');
    Ok(bytes)
}

fn encode_forward_value(task_id: &str, value: &serde_json::Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode forward task-record field of",
            Path::new(task_id),
            source,
        )
    })
}

/// Returns the compact size of `record` together with its `forward` fields,
/// the size that the registry persists and archival copies.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Json`] if the record cannot be encoded.
pub fn task_record_full_encoded_len(
    record: &TaskRecord,
    forward: Option<&TaskRecordForwardFields>,
) -> Result<u64> {
    let mut len = task_record_encoded_len(record)?;
    for (field, value) in forward.into_iter().flatten() {
        if is_known_task_record_field(field) {
            continue;
        }
        // `,"field":value`
        len = len
            .saturating_add(2)
            .saturating_add(
                encode_forward_value(&record.task_id, &serde_json::json!(field))?.len() as u64,
            )
            .saturating_add(encode_forward_value(&record.task_id, value)?.len() as u64);
    }
    Ok(len)
}

/// Returns the `blake3:<hex>` digest that names and authenticates `bytes`.
pub fn task_record_archive_digest(bytes: &[u8]) -> String {
    format!("{DIGEST_PREFIX}{}", blake3::hash(bytes).to_hex())
}

/// Returns the archive file location relative to the workspace `.packet28`
/// directory.
pub fn task_record_archive_relative_path(task_id: &str, digest: &str) -> String {
    format!(
        "{TASK_ARTIFACTS_DIR_NAME}/{task_id}/{TASK_RECORD_ARCHIVE_DIR_NAME}/{}",
        archive_file_name(digest)
    )
}

/// Returns the absolute archive path for diagnostics.
pub fn task_record_archive_path(
    root: &Path,
    task_id: &TaskStorageId,
    digest: &str,
) -> std::path::PathBuf {
    task_artifact_dir(root, task_id)
        .join(TASK_RECORD_ARCHIVE_DIR_NAME)
        .join(archive_file_name(digest))
}

fn archive_file_name(digest: &str) -> String {
    format!(
        "{}{TASK_RECORD_ARCHIVE_FILE_SUFFIX}",
        digest.strip_prefix(DIGEST_PREFIX).unwrap_or(digest)
    )
}

/// Returns the command that retrieves and verifies an archived original.
pub fn task_record_archive_inspect_command(task_id: &str) -> String {
    format!("Packet28 daemon storage show-archived-record --task-id {task_id}")
}

/// Returns why `task_id` cannot be archived from `registry`, or `None`.
///
/// Archival is limited to dormant records that no other record depends on:
/// running, cancelling, replan-pending, and agent-active records are refused,
/// as are recovery predecessors/successors, records named by another record's
/// handoff or bootstrap ownership, and the agent's active task.
pub fn task_record_archive_refusal(
    registry: &TaskRegistry,
    task_id: &str,
    active_task_id: Option<&str>,
) -> Option<String> {
    let Some(record) = registry.tasks.get(task_id) else {
        return Some("task is not present in the registry".to_string());
    };
    if record.archived.is_some() {
        return Some("task record is already archived".to_string());
    }
    let lifecycle = record.lifecycle;
    if lifecycle.is_running() || lifecycle.is_cancelling() || lifecycle.has_pending_replan() {
        return Some(format!("task lifecycle is active ({lifecycle:?})"));
    }
    if agent_is_active(record) {
        return Some("task has a launched agent without a recorded completion".to_string());
    }
    if active_task_id == Some(task_id) {
        return Some("task is the agent's active task pointer".to_string());
    }
    if record.superseded_by.is_some() {
        return Some("task is a superseded recovery predecessor".to_string());
    }
    if record.recovered_from.is_some() {
        return Some("task is a recovery successor that owns predecessor evidence".to_string());
    }
    for other in registry.tasks.values() {
        if other.task_id == task_id {
            continue;
        }
        let named = other
            .superseded_by
            .as_ref()
            .is_some_and(|link| link.successor_task_id == task_id)
            || other
                .recovered_from
                .as_ref()
                .is_some_and(|link| link.predecessor_task_id == task_id)
            || other
                .handoffs
                .iter()
                .any(|handoff| handoff.task_id == task_id)
            || other.latest_hook_bootstrap_owner_task_id.as_deref() == Some(task_id);
        if named {
            return Some(format!(
                "task is referenced as a recovery, handoff, or bootstrap owner by task {:?}",
                other.task_id
            ));
        }
    }
    None
}

fn agent_is_active(record: &TaskRecord) -> bool {
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

fn normalize_timestamp_seconds(value: u64) -> u64 {
    if value < 100_000_000_000 {
        value
    } else {
        value / 1_000
    }
}

/// Builds the archive bytes and compact tombstone for `original` and the
/// `forward_fields` that its committed raw record carries.
///
/// The archive is the complete persisted record: the known fields together
/// with every forward field. Every protected field is kept. Other top-level values larger than
/// [`MAX_TASK_RECORD_TOMBSTONE_FIELD_BYTES`] are omitted, then the largest
/// remaining unprotected values are omitted until the tombstone fits
/// [`MAX_TASK_RECORD_TOMBSTONE_BYTES`]. Each omitted field and its size is
/// listed in the pointer; the value itself remains in the archive.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] when the record is already
/// archived, exceeds the registry read bound, or its protected fields alone do
/// not fit a tombstone, and [`DaemonCoreError::Json`] for encoding failures.
pub fn prepare_task_record_archive(
    original: &TaskRecord,
    forward_fields: &TaskRecordForwardFields,
    reason: &str,
    archived_at_unix: u64,
) -> Result<PreparedTaskRecordArchive> {
    let invalid = |message: String| DaemonCoreError::InvalidTaskRegistry {
        path: Path::new(&original.task_id).to_path_buf(),
        message,
    };
    if original.archived.is_some() {
        return Err(invalid("task record is already archived".to_string()));
    }
    let bytes = encode_task_record_with_forward_fields(original, forward_fields)?;
    if bytes.len() > MAX_TASK_REGISTRY_BYTES {
        return Err(invalid(format!(
            "task record is {} bytes, above the {MAX_TASK_REGISTRY_BYTES}-byte registry bound",
            bytes.len()
        )));
    }
    let digest = task_record_archive_digest(&bytes);
    let serde_json::Value::Object(mut fields) =
        serde_json::to_value(original).map_err(|source| {
            DaemonCoreError::json(
                "failed to decompose task record for archival of",
                Path::new(&original.task_id),
                source,
            )
        })?
    else {
        return Err(invalid(
            "task record did not encode as a JSON object".to_string(),
        ));
    };
    // Forward fields are sized and shed exactly like unprotected known
    // fields; none of them carries identity this build can recognize.
    for (field, value) in forward_fields {
        fields.insert(field.clone(), value.clone());
    }
    let mut sizes = BTreeMap::new();
    for (field, value) in &fields {
        let size = serde_json::to_vec(value)
            .map_err(|source| {
                DaemonCoreError::json(
                    "failed to size task record field for archival of",
                    Path::new(&original.task_id),
                    source,
                )
            })?
            .len() as u64;
        sizes.insert(field.clone(), size);
    }
    let mut omitted_fields = BTreeMap::new();
    for (field, size) in &sizes {
        if *size > MAX_TASK_RECORD_TOMBSTONE_FIELD_BYTES as u64
            && !PROTECTED_TOMBSTONE_FIELDS.contains(&field.as_str())
        {
            fields.remove(field);
            omitted_fields.insert(field.clone(), *size);
        }
    }
    let mut pointer = TaskRecordArchive {
        schema_version: TASK_RECORD_ARCHIVE_SCHEMA_VERSION,
        digest: digest.clone(),
        original_encoded_bytes: bytes.len() as u64,
        tombstone_encoded_bytes: 0,
        archived_at_unix,
        reason: reason.to_string(),
        archive_file: task_record_archive_relative_path(&original.task_id, &digest),
        omitted_fields: BTreeMap::new(),
        inspect_command: task_record_archive_inspect_command(&original.task_id),
    };
    loop {
        let retained_forward_fields = forward_fields
            .iter()
            .filter(|(field, _)| fields.contains_key(*field))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect::<TaskRecordForwardFields>();
        let known_fields = fields
            .iter()
            .filter(|(field, _)| !forward_fields.contains_key(*field))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let mut tombstone: TaskRecord =
            serde_json::from_value(serde_json::Value::Object(known_fields)).map_err(|source| {
                DaemonCoreError::json(
                    "failed to rebuild task record tombstone for",
                    Path::new(&original.task_id),
                    source,
                )
            })?;
        pointer.omitted_fields = omitted_fields.clone();
        // The pointer records its own tombstone size; iterate once so the
        // stored value is exact for the final encoding.
        for _ in 0..4 {
            tombstone.archived = Some(pointer.clone());
            let encoded = task_record_full_encoded_len(&tombstone, Some(&retained_forward_fields))?;
            if encoded == pointer.tombstone_encoded_bytes {
                break;
            }
            pointer.tombstone_encoded_bytes = encoded;
        }
        tombstone.archived = Some(pointer.clone());
        let encoded = task_record_full_encoded_len(&tombstone, Some(&retained_forward_fields))?;
        if encoded != pointer.tombstone_encoded_bytes {
            return Err(invalid(
                "task record tombstone size did not converge".to_string(),
            ));
        }
        if encoded <= MAX_TASK_RECORD_TOMBSTONE_BYTES as u64 {
            return Ok(PreparedTaskRecordArchive {
                bytes,
                tombstone,
                forward_fields: forward_fields.clone(),
                retained_forward_fields,
                pointer,
            });
        }
        let largest = fields
            .keys()
            .filter(|field| !PROTECTED_TOMBSTONE_FIELDS.contains(&field.as_str()))
            .max_by_key(|field| (sizes.get(*field).copied().unwrap_or(0), (*field).clone()))
            .cloned();
        match largest {
            Some(field) if sizes.get(&field).copied().unwrap_or(0) > 2 => {
                fields.remove(&field);
                omitted_fields.insert(field.clone(), sizes.get(&field).copied().unwrap_or(0));
            }
            _ => {
                return Err(invalid(format!(
                    "task record tombstone is {encoded} bytes even after omitting every \
                     unprotected field; the {MAX_TASK_RECORD_TOMBSTONE_BYTES}-byte bound \
                     cannot be met without dropping identity or provenance"
                )));
            }
        }
    }
}

/// Publishes the archived bytes for `task_id` and returns whether this call
/// created the file.
///
/// The file is owner-only, synchronized with its directory, and read back
/// before return. An existing file with the same digest name must contain the
/// identical bytes; it is never replaced.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] for capability, write, synchronization, or
/// verification failures, and [`DaemonCoreError::InvalidTaskRegistry`] when an
/// existing archive with this name contains different bytes.
#[cfg(unix)]
pub fn publish_task_record_archive(
    root: &Path,
    task_id: &TaskStorageId,
    bytes: &[u8],
) -> Result<bool> {
    let digest = task_record_archive_digest(bytes);
    let path = task_record_archive_path(root, task_id, &digest);
    let io =
        |message: &'static str, source: std::io::Error| DaemonCoreError::io(message, &path, source);
    let workspace = CapabilityDir::open_workspace(root)
        .map_err(|source| io("failed to open workspace for task record archive", source))?;
    let state = workspace
        .ensure_dir_open(OsStr::new(".packet28"), PRIVATE_DIRECTORY_MODE)
        .map_err(|source| {
            io(
                "failed to open state directory for task record archive",
                source,
            )
        })?;
    let artifacts = state
        .ensure_dir_open(OsStr::new(TASK_ARTIFACTS_DIR_NAME), PRIVATE_DIRECTORY_MODE)
        .map_err(|source| {
            io(
                "failed to open task artifact root for record archive",
                source,
            )
        })?;
    let namespace = artifacts
        .ensure_dir_open(OsStr::new(task_id.as_str()), PRIVATE_DIRECTORY_MODE)
        .map_err(|source| {
            io(
                "failed to open task artifact namespace for record archive",
                source,
            )
        })?;
    let archive_dir = namespace
        .ensure_dir(
            OsStr::new(TASK_RECORD_ARCHIVE_DIR_NAME),
            PRIVATE_DIRECTORY_MODE,
        )
        .map_err(|source| {
            io(
                "failed to open private task record archive directory",
                source,
            )
        })?;
    let name = archive_file_name(&digest);
    if archive_dir
        .entry_identity(OsStr::new(&name))
        .map_err(|source| io("failed to inspect task record archive", source))?
        .is_some()
    {
        let existing = archive_dir
            .read_file_limited(OsStr::new(&name), bytes.len())
            .map_err(|source| io("failed to verify existing task record archive", source))?;
        if existing != bytes {
            return Err(DaemonCoreError::InvalidTaskRegistry {
                path,
                message:
                    "an existing task record archive with this digest name has different bytes"
                        .to_string(),
            });
        }
        archive_dir.sync().map_err(|source| {
            io(
                "failed to synchronize task record archive directory",
                source,
            )
        })?;
        return Ok(false);
    }
    maybe_fail_archive_publication(&name)?;
    archive_dir
        .write_json_atomically(OsStr::new(&name), bytes, ARCHIVE_WRITE_TEMP_PREFIX)
        .map_err(|error| io("failed to publish task record archive", error.source))?;
    // Make every newly created ancestor name durable as well; the archive
    // must survive any crash after which its tombstone could be durable.
    for directory in [&namespace, &artifacts, &state] {
        directory
            .sync()
            .map_err(|source| io("failed to synchronize task record archive ancestry", source))?;
    }
    Ok(true)
}

/// Archival needs the anchored capability layer, which is Unix-only, as is the
/// daemon that performs it.
///
/// # Errors
///
/// Always returns [`DaemonCoreError::Io`] with [`std::io::ErrorKind::Unsupported`].
#[cfg(not(unix))]
pub fn publish_task_record_archive(
    root: &Path,
    task_id: &TaskStorageId,
    bytes: &[u8],
) -> Result<bool> {
    let digest = task_record_archive_digest(bytes);
    Err(DaemonCoreError::io(
        "task record archival is unsupported on this platform",
        task_record_archive_path(root, task_id, &digest),
        std::io::Error::from(std::io::ErrorKind::Unsupported),
    ))
}

/// Reads and authenticates the complete original of an archived `tombstone`.
///
/// The read is bounded by the pointer's recorded byte length, which is itself
/// bounded by the registry read limit. The bytes must match the recorded
/// digest and decode to a record with the same identity and event high-water.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] when `tombstone` is not an
/// archived record or the archive does not authenticate, and
/// [`DaemonCoreError::Io`] when it cannot be read.
pub fn read_task_record_archive(root: &Path, tombstone: &TaskRecord) -> Result<Vec<u8>> {
    let task_id = TaskStorageId::try_from(tombstone.task_id.as_str()).map_err(|error| {
        DaemonCoreError::InvalidTaskStorageIdentifier {
            path: Path::new(&tombstone.task_id).to_path_buf(),
            message: error.to_string(),
        }
    })?;
    let Some(pointer) = tombstone.archived.as_ref() else {
        return Err(DaemonCoreError::InvalidTaskRegistry {
            path: task_artifact_dir(root, &task_id),
            message: format!("task {:?} is not an archived record", tombstone.task_id),
        });
    };
    let path = task_record_archive_path(root, &task_id, &pointer.digest);
    let invalid = |message: String| DaemonCoreError::InvalidTaskRegistry {
        path: path.clone(),
        message,
    };
    let hex = pointer
        .digest
        .strip_prefix(DIGEST_PREFIX)
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid(format!("unsupported archive digest {:?}", pointer.digest)))?;
    let expected_len = usize::try_from(pointer.original_encoded_bytes)
        .ok()
        .filter(|length| *length <= MAX_TASK_REGISTRY_BYTES)
        .ok_or_else(|| {
            invalid(format!(
                "archive length {} exceeds the {MAX_TASK_REGISTRY_BYTES}-byte record bound",
                pointer.original_encoded_bytes
            ))
        })?;
    let bytes = read_archive_bytes(
        root,
        &task_id,
        &format!("{hex}{TASK_RECORD_ARCHIVE_FILE_SUFFIX}"),
        expected_len,
        &path,
    )?;
    if bytes.len() != expected_len {
        return Err(invalid(format!(
            "archive is {} bytes; its pointer records {expected_len}",
            bytes.len()
        )));
    }
    if task_record_archive_digest(&bytes) != pointer.digest {
        return Err(invalid(
            "archive bytes do not match the recorded digest".to_string(),
        ));
    }
    let original: TaskRecord = serde_json::from_slice(&bytes).map_err(|source| {
        DaemonCoreError::json("failed to decode archived task record", &path, source)
    })?;
    if original.task_id != tombstone.task_id || original.last_event_seq != tombstone.last_event_seq
    {
        return Err(invalid(
            "archived record identity or event high-water does not match its tombstone".to_string(),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_archive_bytes(
    root: &Path,
    task_id: &TaskStorageId,
    name: &str,
    max_bytes: usize,
    path: &Path,
) -> Result<Vec<u8>> {
    let io = |source: std::io::Error| {
        DaemonCoreError::io("failed to read task record archive", path, source)
    };
    let workspace = CapabilityDir::open_workspace(root).map_err(io)?;
    let archive_dir = workspace
        .open_relative_dir(
            &Path::new(".packet28")
                .join(TASK_ARTIFACTS_DIR_NAME)
                .join(task_id.as_str())
                .join(TASK_RECORD_ARCHIVE_DIR_NAME),
        )
        .map_err(io)?;
    archive_dir
        .read_file_limited(OsStr::new(name), max_bytes)
        .map_err(io)
}

#[cfg(not(unix))]
fn read_archive_bytes(
    _root: &Path,
    _task_id: &TaskStorageId,
    _name: &str,
    max_bytes: usize,
    path: &Path,
) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|source| {
        DaemonCoreError::io("failed to read task record archive", path, source)
    })?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| {
            DaemonCoreError::io("failed to read task record archive", path, source)
        })?;
    Ok(bytes)
}

#[cfg(all(unix, any(test, debug_assertions)))]
fn maybe_fail_archive_publication(name: &str) -> Result<()> {
    if std::env::var("PACKET28_TASK_RECORD_ARCHIVE_FAIL_PUBLISH").as_deref() == Ok("1") {
        return Err(DaemonCoreError::io(
            "injected task record archive publication failure",
            Path::new(name),
            std::io::Error::other("injected"),
        ));
    }
    Ok(())
}

#[cfg(all(unix, not(any(test, debug_assertions))))]
fn maybe_fail_archive_publication(_name: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use packet28_daemon_protocol::task::{TaskHistoryRecovery, TaskLifecycle};

    fn oversized(task_id: &str, error_bytes: usize) -> TaskRecord {
        TaskRecord {
            task_id: task_id.to_string(),
            last_event_seq: 7,
            last_completed_at_unix: Some(100),
            watch_ids: vec!["watch-a".to_string()],
            last_error: Some("e".repeat(error_bytes)),
            question_texts: BTreeMap::from([("q1".to_string(), "small".to_string())]),
            ..TaskRecord::default()
        }
    }

    #[test]
    fn tombstone_keeps_identity_and_small_fields_and_lists_every_omission() {
        let original = oversized("task-big", 1024 * 1024);
        let prepared = prepare_task_record_archive(
            &original,
            &TaskRecordForwardFields::new(),
            TASK_RECORD_ARCHIVE_REASON_OVERSIZED,
            500,
        )
        .unwrap();
        let tombstone = &prepared.tombstone;
        let pointer = prepared.pointer();

        assert_eq!(
            prepared.bytes,
            encode_task_record_compact(&original).unwrap()
        );
        assert_eq!(pointer.digest, task_record_archive_digest(&prepared.bytes));
        assert_eq!(pointer.original_encoded_bytes, prepared.bytes.len() as u64);
        assert_eq!(
            pointer.tombstone_encoded_bytes,
            task_record_encoded_len(tombstone).unwrap()
        );
        assert!(pointer.tombstone_encoded_bytes <= MAX_TASK_RECORD_TOMBSTONE_BYTES as u64);
        assert_eq!(tombstone.task_id, "task-big");
        assert_eq!(tombstone.last_event_seq, 7);
        assert_eq!(tombstone.watch_ids, original.watch_ids);
        assert_eq!(tombstone.last_completed_at_unix, Some(100));
        assert_eq!(tombstone.question_texts, original.question_texts);
        assert_eq!(tombstone.last_error, None);
        assert_eq!(
            pointer.omitted_fields.keys().collect::<Vec<_>>(),
            vec!["last_error"]
        );
        assert!(pointer.inspect_command.contains("--task-id task-big"));
        assert_eq!(
            pointer.archive_file,
            format!(
                "task/task-big/record-archive/{}.task-record.json",
                pointer.digest.trim_start_matches("blake3:")
            )
        );
    }

    #[test]
    fn many_moderate_fields_are_shed_largest_first_until_the_tombstone_fits() {
        let mut original = oversized("task-many", 16);
        for index in 0..64 {
            original
                .question_texts
                .insert(format!("q{index:03}"), "x".repeat(3 * 1024));
        }
        original.last_error = Some("y".repeat(3 * 1024));
        let prepared = prepare_task_record_archive(
            &original,
            &TaskRecordForwardFields::new(),
            TASK_RECORD_ARCHIVE_REASON_OVERSIZED,
            1,
        )
        .unwrap();
        // question_texts is a single 190 KiB value; the small last_error stays.
        assert!(prepared
            .pointer()
            .omitted_fields
            .contains_key("question_texts"));
        assert_eq!(prepared.tombstone.last_error, original.last_error);
    }

    #[test]
    fn protected_provenance_is_never_dropped() {
        let mut original = oversized("task-protected", 2 * 1024 * 1024);
        original.watch_ids = (0..10_000)
            .map(|index| format!("watch-{index:05}"))
            .collect();
        let error =
            prepare_task_record_archive(&original, &TaskRecordForwardFields::new(), "test", 1)
                .unwrap_err();
        assert!(error.to_string().contains("without dropping identity"));
    }

    #[test]
    fn refusal_covers_active_referenced_and_pointer_tasks_but_not_dormant_ones() {
        let mut registry = TaskRegistry::default();
        let mut insert = |record: TaskRecord| {
            registry.tasks.insert(record.task_id.clone(), record);
        };
        insert(oversized("dormant", 16));
        insert(TaskRecord {
            lifecycle: TaskLifecycle::Running,
            ..oversized("running", 16)
        });
        insert(TaskRecord {
            lifecycle: TaskLifecycle::ReplanPending,
            ..oversized("replan", 16)
        });
        insert(TaskRecord {
            latest_agent_started_at_unix: Some(10),
            ..oversized("agent", 16)
        });
        insert(TaskRecord {
            lifecycle: TaskLifecycle::Cancelled,
            ..oversized("cancelled", 16)
        });
        let link = TaskHistoryRecovery {
            predecessor_task_id: "predecessor".to_string(),
            successor_task_id: "successor".to_string(),
            ..TaskHistoryRecovery::default()
        };
        insert(TaskRecord {
            superseded_by: Some(link.clone()),
            ..oversized("predecessor", 16)
        });
        insert(TaskRecord {
            recovered_from: Some(link),
            ..oversized("successor", 16)
        });
        insert(TaskRecord {
            latest_hook_bootstrap_owner_task_id: Some("owner".to_string()),
            ..oversized("child", 16)
        });
        insert(oversized("owner", 16));
        insert(oversized("pointer", 16));

        let refused =
            |task_id: &str| task_record_archive_refusal(&registry, task_id, Some("pointer"));
        assert_eq!(refused("dormant"), None);
        assert_eq!(refused("cancelled"), None);
        assert_eq!(refused("child"), None);
        for task_id in [
            "running",
            "replan",
            "agent",
            "predecessor",
            "successor",
            "owner",
            "pointer",
            "missing",
        ] {
            assert!(refused(task_id).is_some(), "{task_id} must be refused");
        }
    }

    #[test]
    fn archived_tombstone_fences_standalone_appends_and_sheds_checkpoint_values() {
        use crate::storage::{
            append_next_task_event, load_task_events, save_task_watch_registry_checkpoint,
        };
        use packet28_daemon_protocol::broker::BrokerHandoffDescriptor;
        use packet28_daemon_protocol::message::DaemonEvent;
        use packet28_daemon_protocol::paths::task_registry_path;
        use packet28_daemon_protocol::task::WatchRegistry;

        let root = tempfile::tempdir().unwrap();
        let mut original = oversized("task-big", 300 * 1024);
        original.watch_ids.clear();
        original.last_event_seq = 0;
        original.handoffs = (0..64)
            .map(|index| BrokerHandoffDescriptor {
                handoff_id: format!("handoff-{index}"),
                task_id: "task-big".to_string(),
                artifact_id: "h".repeat(128),
                ..BrokerHandoffDescriptor::default()
            })
            .collect();
        let healthy = TaskRecord {
            task_id: "task-good".to_string(),
            ..TaskRecord::default()
        };
        let mut registry = TaskRegistry::default();
        registry
            .tasks
            .insert(original.task_id.clone(), original.clone());
        registry
            .tasks
            .insert(healthy.task_id.clone(), healthy.clone());
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();
        let event = DaemonEvent {
            kind: "probe".to_string(),
            occurred_at_unix: 1,
            data: serde_json::json!({}),
        };
        let frame = append_next_task_event(root.path(), "task-big", &event).unwrap();
        original.last_event_seq = frame.seq;
        registry
            .tasks
            .insert(original.task_id.clone(), original.clone());
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();
        let events_before = load_task_events(root.path(), "task-big").unwrap();

        let prepared =
            prepare_task_record_archive(&original, &TaskRecordForwardFields::new(), "test", 2)
                .unwrap();
        assert!(prepared.pointer().omitted_fields.contains_key("handoffs"));
        registry
            .tasks
            .insert(original.task_id.clone(), prepared.tombstone.clone());
        save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
            .unwrap();
        let raw = std::fs::read_to_string(task_registry_path(root.path())).unwrap();
        assert!(
            !raw.contains("eeee"),
            "archived last_error must not be preserved"
        );
        assert!(
            !raw.contains("hhhh"),
            "archived handoffs must not be resurrected"
        );
        assert!(raw.contains(&prepared.pointer().digest));

        let error = append_next_task_event(root.path(), "task-big", &event).unwrap_err();
        assert!(
            matches!(error, DaemonCoreError::TaskArchived { ref task_id, .. } if task_id == "task-big"),
            "{error}"
        );
        assert_eq!(
            serde_json::to_value(load_task_events(root.path(), "task-big").unwrap()).unwrap(),
            serde_json::to_value(events_before).unwrap()
        );
        // Unselected records stay writable.
        append_next_task_event(root.path(), "task-good", &event).unwrap();
    }

    fn future_evidence() -> serde_json::Value {
        serde_json::json!({"note": "future-evidence-marker", "bytes": "Y".repeat(70_000)})
    }

    #[test]
    fn forward_fields_are_archived_sized_and_shed_or_retained() {
        let original = oversized("task-big", 1024 * 1024);
        let forward = TaskRecordForwardFields::from([
            ("future_evidence".to_string(), future_evidence()),
            ("future_small".to_string(), serde_json::json!({"v": 7})),
        ]);
        let prepared = prepare_task_record_archive(&original, &forward, "test", 3).unwrap();
        let pointer = prepared.pointer();

        // The archive is the complete persisted record, known and forward.
        let archived: serde_json::Value = serde_json::from_slice(&prepared.bytes).unwrap();
        let mut expected = serde_json::to_value(&original).unwrap();
        for (field, value) in &forward {
            expected[field] = value.clone();
        }
        assert_eq!(archived, expected);
        assert_eq!(
            prepared.bytes.len() as u64,
            task_record_full_encoded_len(&original, Some(&forward)).unwrap()
        );
        assert_eq!(pointer.original_encoded_bytes, prepared.bytes.len() as u64);
        assert_eq!(pointer.digest, task_record_archive_digest(&prepared.bytes));
        // Large forward values are shed like any unprotected field.
        assert_eq!(
            pointer.omitted_fields.keys().collect::<Vec<_>>(),
            vec!["future_evidence", "last_error"]
        );
        assert_eq!(prepared.forward_fields, forward);
        assert_eq!(
            prepared.retained_forward_fields,
            TaskRecordForwardFields::from([(
                "future_small".to_string(),
                serde_json::json!({"v": 7})
            )])
        );
        // The recorded tombstone size is the persisted size, retained forward
        // fields included.
        assert_eq!(
            pointer.tombstone_encoded_bytes,
            encode_task_record_with_forward_fields(
                &prepared.tombstone,
                &prepared.retained_forward_fields
            )
            .unwrap()
            .len() as u64
        );
        // Without forward fields the archive bytes are unchanged from R1.
        let plain =
            prepare_task_record_archive(&original, &TaskRecordForwardFields::new(), "test", 3)
                .unwrap();
        assert_eq!(plain.bytes, encode_task_record_compact(&original).unwrap());
    }

    #[test]
    fn forward_only_oversized_record_is_sized_and_archivable() {
        let small = oversized("task-forward", 16);
        let forward = TaskRecordForwardFields::from([(
            "future_blob".to_string(),
            serde_json::json!("f".repeat(1024 * 1024)),
        )]);
        assert!(task_record_encoded_len(&small).unwrap() < 4 * 1024);
        assert!(
            task_record_full_encoded_len(&small, Some(&forward)).unwrap() > 1024 * 1024,
            "forward bytes count toward the persisted record size"
        );
        let prepared = prepare_task_record_archive(&small, &forward, "test", 1).unwrap();
        assert!(prepared
            .pointer()
            .omitted_fields
            .contains_key("future_blob"));
        assert!(prepared.retained_forward_fields.is_empty());
        let archived: serde_json::Value = serde_json::from_slice(&prepared.bytes).unwrap();
        assert_eq!(archived["future_blob"], forward["future_blob"]);
        // A forward key may never shadow a known field.
        let shadow =
            TaskRecordForwardFields::from([("last_error".to_string(), serde_json::json!("x"))]);
        assert!(encode_task_record_with_forward_fields(&small, &shadow).is_err());
    }

    #[test]
    fn checkpoint_keeps_retained_forward_fields_and_never_resurrects_shed_ones() {
        use crate::storage::{
            ensure_daemon_dir, load_task_record_forward_fields, load_task_registry,
            save_task_watch_registry_checkpoint,
        };
        use packet28_daemon_protocol::paths::task_registry_path;
        use packet28_daemon_protocol::task::WatchRegistry;

        let root = tempfile::tempdir().unwrap();
        ensure_daemon_dir(root.path()).unwrap();
        let path = task_registry_path(root.path());
        let raw = serde_json::json!({
            "future_root": {"enabled": true},
            "tasks": {
                "task-big": {
                    "task_id": "task-big",
                    "running": false,
                    "last_completed_at_unix": 100,
                    "last_error": "e".repeat(300 * 1024),
                    "future_evidence": future_evidence(),
                    "future_small": {"v": 7}
                },
                "task-good": {
                    "task_id": "task-good",
                    "running": false,
                    "future_neighbor": {"keep": "neighbor-marker"}
                }
            }
        });
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

        let forward = load_task_record_forward_fields(root.path()).unwrap();
        assert_eq!(
            forward.keys().collect::<Vec<_>>(),
            vec!["task-big", "task-good"]
        );
        assert_eq!(forward["task-big"]["future_evidence"], future_evidence());
        let mut registry = load_task_registry(root.path()).unwrap();
        let original = registry.tasks["task-big"].clone();
        let prepared =
            prepare_task_record_archive(&original, &forward["task-big"], "test", 9).unwrap();
        let archived: serde_json::Value = serde_json::from_slice(&prepared.bytes).unwrap();
        for (field, value) in raw["tasks"]["task-big"].as_object().unwrap() {
            assert_eq!(
                &archived[field], value,
                "archive must hold raw field {field}"
            );
        }

        registry
            .tasks
            .insert("task-big".to_string(), prepared.tombstone.clone());
        for _ in 0..2 {
            // The first checkpoint replaces the original; the second proves
            // the tombstone is a fixed point of forward-field preservation.
            save_task_watch_registry_checkpoint(root.path(), &registry, &WatchRegistry::default())
                .unwrap();
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let tombstone = &saved["tasks"]["task-big"];
            assert!(tombstone.get("future_evidence").is_none());
            assert_eq!(tombstone["last_error"], serde_json::Value::Null);
            assert_eq!(tombstone["future_small"], serde_json::json!({"v": 7}));
            assert_eq!(
                tombstone["archived"]["digest"],
                serde_json::json!(prepared.pointer().digest)
            );
            assert_eq!(
                serde_json::to_vec(tombstone).unwrap().len() as u64,
                prepared.pointer().tombstone_encoded_bytes,
                "the pointer records the persisted tombstone size"
            );
            for (field, value) in raw["tasks"]["task-good"].as_object().unwrap() {
                assert_eq!(
                    &saved["tasks"]["task-good"][field], value,
                    "unselected records keep every field, including {field}"
                );
            }
            assert_eq!(saved["future_root"], raw["future_root"]);
        }
        let after = load_task_record_forward_fields(root.path()).unwrap();
        assert_eq!(after["task-big"], prepared.retained_forward_fields);
        assert_eq!(after["task-good"], forward["task-good"]);
    }

    #[cfg(unix)]
    #[test]
    fn publication_is_private_idempotent_and_authenticated_on_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let original = oversized("task-big", 200 * 1024);
        let prepared =
            prepare_task_record_archive(&original, &TaskRecordForwardFields::new(), "test", 1)
                .unwrap();
        let storage_id = TaskStorageId::try_from("task-big").unwrap();

        assert!(publish_task_record_archive(root.path(), &storage_id, &prepared.bytes).unwrap());
        assert!(!publish_task_record_archive(root.path(), &storage_id, &prepared.bytes).unwrap());
        let path = task_record_archive_path(root.path(), &storage_id, &prepared.pointer().digest);
        assert_eq!(std::fs::read(&path).unwrap(), prepared.bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            read_task_record_archive(root.path(), &prepared.tombstone).unwrap(),
            prepared.bytes
        );

        // A tampered archive is rejected rather than returned.
        let mut tampered = prepared.bytes.clone();
        let last = tampered.len() - 3;
        tampered[last] = b'z';
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, &tampered).unwrap();
        let error = read_task_record_archive(root.path(), &prepared.tombstone).unwrap_err();
        assert!(error.to_string().contains("digest"), "{error}");
        // Publication never replaces a differing file under the same name.
        let error =
            publish_task_record_archive(root.path(), &storage_id, &prepared.bytes).unwrap_err();
        assert!(error.to_string().contains("different bytes"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), tampered);
    }
}
