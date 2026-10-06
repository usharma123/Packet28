//! Offline exact repair of canonical task/watch registry images.
//!
//! Startup rejects canonical registry bytes that match no journaled checkpoint
//! publication phase. This module restores only bytes that the checkpoint
//! authority already authenticates: a strict canonical re-encoding whose
//! length and BLAKE3 digest equal the committed manifest, or the retained
//! journal base image while that base is still the committed checkpoint. It
//! never adopts edited content, invents missing records, deletes checkpoint
//! metadata, or rewrites the registry delta WAL.
//!
//! An apply first archives every affected original and the checkpoint
//! authority in an owner-only directory, binds that archive with a receipt
//! digest in a durable repair journal, and only then replaces canonical
//! images. While the repair journal exists, normal checkpoint resolution
//! refuses to start, so an interruption can never be read as committed state;
//! rerunning the apply verifies the archive and completes the same plan.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use serde::{Deserialize, Serialize};

use super::checkpoint::{
    self, CanonicalFileAction, CanonicalRepairAssessment, CanonicalRepairPlan,
    CHECKPOINT_AUTHORITY_FILE_NAMES, CHECKPOINT_REPAIR_JOURNAL_FILE_NAME,
    CHECKPOINT_REPAIR_JOURNAL_WRITE_TEMP_PREFIX, MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES,
};
pub use super::checkpoint::{RegistryCheckpointAuthority, RegistryRepairCandidateSource};
pub use super::registry_delta::RegistryWalReplayVerification;
use super::*;

/// Daemon-state subdirectory holding owner-only repair archives.
pub const REGISTRY_REPAIR_ARCHIVE_DIR_NAME: &str = "registry-repair";
const REPAIR_SCHEMA_VERSION: u32 = 1;
const REPAIR_RECEIPT_FILE_NAME: &str = "receipt.json";
const REPAIR_COMPLETED_JOURNAL_FILE_NAME: &str = "repair-journal.json";
const REPAIR_ARCHIVE_WRITE_TEMP_PREFIX: &str = ".registry-repair-archive.packet28-write.";
const MAX_REPAIR_ARCHIVE_ATTEMPTS: u32 = 64;
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: rustix::fs::RawMode = 0o700;
const CANONICAL_REGISTRY_FILES: [(&str, usize); 2] = [
    (TASK_REGISTRY_FILE_NAME, MAX_TASK_REGISTRY_BYTES),
    (WATCH_REGISTRY_FILE_NAME, MAX_WATCH_REGISTRY_BYTES),
];

#[cfg(test)]
std::thread_local! {
    static INJECT_REPAIR_INTERRUPTION_AFTER: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Overall outcome of a canonical registry repair inspection or apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryCheckpointRepairStatus {
    /// Normal checkpoint resolution already accepts the canonical images.
    Clean,
    /// Dry run: an exact authoritative restore is available.
    Repairable,
    /// Apply: canonical images were restored to exact checkpoint authority.
    Repaired,
    /// Dry run: an earlier apply was interrupted and can be completed.
    InterruptedRepair,
    /// Apply: an interrupted repair was verified and completed.
    Resumed,
    /// No exact authority can restore the images; nothing was changed.
    Unrecoverable,
}

/// Per-file disposition in a repair report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryRepairFileState {
    /// Bytes already equal checkpoint authority.
    Matches,
    /// Dry run: bytes would be replaced by the exact authoritative image.
    Restore,
    /// Bytes were replaced by the exact authoritative image.
    Restored,
    /// No exact authoritative image exists for these bytes.
    Unrecoverable,
}

/// Byte length and BLAKE3 digest of one stored artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryArtifactDigest {
    /// Exact byte length.
    pub bytes: u64,
    /// Lowercase hexadecimal BLAKE3 digest.
    pub blake3: String,
}

impl RegistryArtifactDigest {
    fn of(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.len() as u64,
            blake3: blake3::hash(bytes).to_hex().to_string(),
        }
    }

    fn matches(&self, bytes: &[u8]) -> bool {
        *self == Self::of(bytes)
    }
}

/// Repair evidence for one canonical registry image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryRepairFileReport {
    /// Canonical file name in the daemon state directory.
    pub file: String,
    /// Canonical file path.
    pub path: PathBuf,
    /// Disposition of this file.
    pub state: RegistryRepairFileState,
    /// Digest of the bytes found before repair, if the file exists.
    pub current: Option<RegistryArtifactDigest>,
    /// Digest required by checkpoint authority.
    pub expected: RegistryArtifactDigest,
    /// Origin of the restored image.
    pub source: Option<RegistryRepairCandidateSource>,
    /// For a journal-base restore, whether the replaced bytes were only a
    /// re-encoding of the restored image. `false` means substantive content
    /// that is archived but not adopted.
    pub replaced_bytes_were_reencoding: Option<bool>,
    /// Explanation for unrecoverable files.
    pub detail: Option<String>,
}

/// Result of inspecting or repairing canonical task/watch registry images.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryCheckpointRepairReport {
    /// Overall outcome.
    pub status: RegistryCheckpointRepairStatus,
    /// Whether this run was permitted to write.
    pub applied: bool,
    /// The error normal startup reports for the inspected state.
    pub startup_error: Option<String>,
    /// Authority used to authenticate the restore.
    pub authority: Option<RegistryCheckpointAuthority>,
    /// Checkpoint generation of the restored authority.
    pub generation: Option<u64>,
    /// Registry delta revision recorded by the restored authority.
    pub applied_delta_revision: Option<u64>,
    /// Per-file evidence for both canonical registry images.
    pub files: Vec<RegistryRepairFileReport>,
    /// Read-only WAL replay evidence for the restored checkpoint.
    pub wal: Option<RegistryWalReplayVerification>,
    /// Owner-only archive holding every affected original and its receipt.
    pub archive_path: Option<PathBuf>,
    /// Why no exact restore is possible.
    pub reason: Option<String>,
}

impl RegistryCheckpointRepairReport {
    fn clean(apply: bool) -> Self {
        Self {
            status: RegistryCheckpointRepairStatus::Clean,
            applied: apply,
            startup_error: None,
            authority: None,
            generation: None,
            applied_delta_revision: None,
            files: Vec::new(),
            wal: None,
            archive_path: None,
            reason: None,
        }
    }

    /// Whether startup remains blocked by canonical registry state that no
    /// exact authority can restore.
    pub fn is_unrecoverable(&self) -> bool {
        self.status == RegistryCheckpointRepairStatus::Unrecoverable
    }

    /// Whether normal checkpoint resolution accepts the canonical images now.
    pub fn startup_registry_ready(&self) -> bool {
        matches!(
            self.status,
            RegistryCheckpointRepairStatus::Clean
                | RegistryCheckpointRepairStatus::Repaired
                | RegistryCheckpointRepairStatus::Resumed
        )
    }
}

/// Durable intent naming the archive that authorizes an in-progress repair.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairJournal {
    schema_version: u32,
    archive: String,
    receipt: RegistryArtifactDigest,
}

/// Archive manifest binding originals, authority, and planned writes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairReceipt {
    schema_version: u32,
    created_at_unix: u64,
    authority: RegistryCheckpointAuthority,
    generation: Option<u64>,
    applied_delta_revision: u64,
    startup_error: String,
    archived: Vec<ArchivedFile>,
    writes: Vec<PlannedWrite>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedFile {
    source: String,
    archive: String,
    digest: RegistryArtifactDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedWrite {
    file: String,
    original: Option<RegistryArtifactDigest>,
    target: RegistryArtifactDigest,
    target_archive: String,
    source: RegistryRepairCandidateSource,
    replaced_bytes_were_reencoding: Option<bool>,
}

/// Classifies canonical registry images that startup rejects and reports the
/// exact restore, if any, without changing any byte.
///
/// Requires the same nonblocking offline admission as event-log repair: a
/// running or starting daemon, or any task-store writer, makes this fail busy.
///
/// # Errors
///
/// Returns busy, filesystem-authority, or bounded-read errors. Integrity
/// failures that prevent an exact restore are reported as
/// [`RegistryCheckpointRepairStatus::Unrecoverable`] instead.
pub fn inspect_task_watch_registry_checkpoint_repair(
    root: &Path,
) -> Result<RegistryCheckpointRepairReport> {
    run_registry_checkpoint_repair(root, false)
}

/// Restores canonical registry images to exact checkpoint authority.
///
/// Every affected original and the checkpoint authority are archived under
/// `daemon/registry-repair/` before any canonical write. Unrecoverable state
/// is reported and left untouched.
///
/// # Errors
///
/// Returns the same errors as
/// [`inspect_task_watch_registry_checkpoint_repair`], plus archive, journal,
/// and publication errors. After an interrupted publication, startup refuses
/// until this function completes the journaled repair.
pub fn repair_task_watch_registry_checkpoint(
    root: &Path,
) -> Result<RegistryCheckpointRepairReport> {
    run_registry_checkpoint_repair(root, true)
}

#[cfg(not(unix))]
fn run_registry_checkpoint_repair(
    root: &Path,
    _apply: bool,
) -> Result<RegistryCheckpointRepairReport> {
    Err(DaemonCoreError::io(
        "offline registry repair is unsupported on this platform",
        root,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "authenticated maintenance admission requires Unix",
        ),
    ))
}

#[cfg(unix)]
fn run_registry_checkpoint_repair(
    root: &Path,
    apply: bool,
) -> Result<RegistryCheckpointRepairReport> {
    let (lease, _admission) = registry_delta::acquire_offline_maintenance_admission(
        root,
        "offline registry repair requires exclusive task-store access; stop the daemon and retry",
    )?;
    let daemon = lease.daemon_capability()?;
    with_anchored_task_registry_lock(
        root,
        RegistryLockMode::Exclusive,
        || Ok(()),
        |locked| {
            registry_delta::validate_retained_registry_daemon(root, locked, &daemon)?;
            with_anchored_watch_registry_lock(&daemon, RegistryLockMode::Exclusive, || {
                lease.validate_namespace_attachment()?;
                match read_repair_journal(root, &daemon)? {
                    Some(journal) => resume_repair(root, &daemon, journal, apply),
                    None => start_repair(root, &daemon, apply),
                }
            })
        },
    )
}

#[cfg(unix)]
fn reader(daemon: &CapabilityDir) -> impl FnMut(&str, usize) -> Result<Option<Vec<u8>>> + '_ {
    move |name, max_bytes| read_optional(daemon, name, max_bytes)
}

#[cfg(unix)]
fn read_optional(daemon: &CapabilityDir, name: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    match daemon.read_file_limited(OsStr::new(name), max_bytes) {
        Ok(raw) => Ok(Some(raw)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DaemonCoreError::io(
            "failed to read registry repair input",
            daemon.display_path().join(name),
            source,
        )),
    }
}

#[cfg(unix)]
fn start_repair(
    root: &Path,
    daemon: &CapabilityDir,
    apply: bool,
) -> Result<RegistryCheckpointRepairReport> {
    let (startup_error, plan) = match checkpoint::assess_canonical_repair(root, reader(daemon))? {
        CanonicalRepairAssessment::Clean => {
            return Ok(RegistryCheckpointRepairReport::clean(apply))
        }
        CanonicalRepairAssessment::NoAuthority {
            startup_error,
            reason,
        } => {
            let mut report = RegistryCheckpointRepairReport::clean(apply);
            report.status = RegistryCheckpointRepairStatus::Unrecoverable;
            report.startup_error = Some(startup_error);
            report.reason = Some(reason);
            return Ok(report);
        }
        CanonicalRepairAssessment::Plan {
            startup_error,
            plan,
        } => (startup_error, plan),
    };
    let mut report = plan_report(root, &plan, apply, startup_error.clone());
    let unrecoverable = report
        .files
        .iter()
        .filter(|file| file.state == RegistryRepairFileState::Unrecoverable)
        .map(|file| file.file.as_str())
        .collect::<Vec<_>>();
    if !unrecoverable.is_empty() {
        report.status = RegistryCheckpointRepairStatus::Unrecoverable;
        report.reason = Some(format!(
            "no exact authoritative image exists for {}; edited bytes were left in place and \
             are not adopted",
            unrecoverable.join(", ")
        ));
        return Ok(report);
    }
    let [tasks, watches] = candidate_pair(&plan);
    if let Err(reason) = verify_candidate(root, daemon, tasks, watches, &mut report)? {
        report.status = RegistryCheckpointRepairStatus::Unrecoverable;
        report.reason = Some(reason);
        return Ok(report);
    }
    if !apply {
        report.status = RegistryCheckpointRepairStatus::Repairable;
        return Ok(report);
    }

    let (archive_name, archive) = create_repair_archive(daemon)?;
    report.archive_path = Some(archive.display_path().to_path_buf());
    let receipt = archive_repair_inputs(daemon, &archive, &plan, startup_error)?;
    let receipt_bytes = encode_repair_json(&archive, REPAIR_RECEIPT_FILE_NAME, &receipt)?;
    write_repair_file(&archive, REPAIR_RECEIPT_FILE_NAME, &receipt_bytes)?;
    repair_phase("archive")?;
    let journal = RepairJournal {
        schema_version: REPAIR_SCHEMA_VERSION,
        archive: archive_name,
        receipt: RegistryArtifactDigest::of(&receipt_bytes),
    };
    let journal_bytes = encode_repair_json(daemon, CHECKPOINT_REPAIR_JOURNAL_FILE_NAME, &journal)?;
    daemon
        .write_json_atomically(
            OsStr::new(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
            &journal_bytes,
            CHECKPOINT_REPAIR_JOURNAL_WRITE_TEMP_PREFIX,
        )
        .map_err(|error| {
            DaemonCoreError::io(
                "failed to publish task/watch registry repair journal",
                daemon
                    .display_path()
                    .join(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
                error.source,
            )
        })?;
    repair_phase("journal")?;
    complete_repair(root, daemon, &archive, &receipt, &mut report)?;
    report.status = RegistryCheckpointRepairStatus::Repaired;
    Ok(report)
}

fn plan_report(
    root: &Path,
    plan: &CanonicalRepairPlan,
    apply: bool,
    startup_error: String,
) -> RegistryCheckpointRepairReport {
    let files = plan
        .files
        .iter()
        .map(|file| {
            let (state, source, replaced_bytes_were_reencoding, detail) = match &file.action {
                CanonicalFileAction::Keep => (RegistryRepairFileState::Matches, None, None, None),
                CanonicalFileAction::Restore {
                    source,
                    reencoding_matched,
                    ..
                } => (
                    RegistryRepairFileState::Restore,
                    Some(*source),
                    *reencoding_matched,
                    None,
                ),
                CanonicalFileAction::Unrecoverable { reason } => (
                    RegistryRepairFileState::Unrecoverable,
                    None,
                    None,
                    Some(reason.clone()),
                ),
            };
            RegistryRepairFileReport {
                file: file.name.to_string(),
                path: daemon_dir(root).join(file.name),
                state,
                current: file.current.as_deref().map(RegistryArtifactDigest::of),
                expected: RegistryArtifactDigest {
                    bytes: file.expected_bytes,
                    blake3: file.expected_blake3.clone(),
                },
                source,
                replaced_bytes_were_reencoding,
                detail,
            }
        })
        .collect();
    RegistryCheckpointRepairReport {
        status: RegistryCheckpointRepairStatus::Repairable,
        applied: apply,
        startup_error: Some(startup_error),
        authority: Some(plan.authority),
        generation: plan.generation,
        applied_delta_revision: Some(plan.applied_delta_revision),
        files,
        wal: None,
        archive_path: None,
        reason: None,
    }
}

fn candidate_pair(plan: &CanonicalRepairPlan) -> [Option<Vec<u8>>; 2] {
    let mut pair = [None, None];
    for (slot, file) in pair.iter_mut().zip(&plan.files) {
        *slot = match &file.action {
            CanonicalFileAction::Restore { bytes, .. } => Some(bytes.clone()),
            CanonicalFileAction::Keep | CanonicalFileAction::Unrecoverable { .. } => {
                file.current.clone()
            }
        };
    }
    pair
}

/// Runs the candidate through the startup resolver, registry decoding, and a
/// read-only WAL replay. Integrity failures become a refusal reason.
#[cfg(unix)]
fn verify_candidate(
    root: &Path,
    daemon: &CapabilityDir,
    tasks: Option<Vec<u8>>,
    watches: Option<Vec<u8>>,
    report: &mut RegistryCheckpointRepairReport,
) -> Result<std::result::Result<(), String>> {
    let (tasks, watches, revision) =
        match checkpoint::verify_canonical_candidate(root, tasks, watches, reader(daemon)) {
            Ok(verified) => verified,
            Err(error) if is_integrity_error(&error) => {
                return Ok(Err(format!(
                    "the restored images would not pass startup checkpoint validation: {error}"
                )));
            }
            Err(error) => return Err(error),
        };
    match registry_delta::verify_registry_wal_replay(root, daemon, tasks, watches, revision) {
        Ok(wal) => {
            report.wal = Some(wal);
            Ok(Ok(()))
        }
        Err(error) if is_integrity_error(&error) => Ok(Err(format!(
            "registry delta WAL history does not continue from the restored checkpoint \
             revision {revision}: {error}"
        ))),
        Err(error) => Err(error),
    }
}

fn is_integrity_error(error: &DaemonCoreError) -> bool {
    matches!(
        error,
        DaemonCoreError::InvalidTaskWatchRegistry { .. }
            | DaemonCoreError::RegistryCheckpointGenerationMismatch { .. }
            | DaemonCoreError::InvalidTaskRegistry { .. }
            | DaemonCoreError::InvalidRegistryDeltaWal { .. }
            | DaemonCoreError::RegistryDeltaWalTooLarge { .. }
            | DaemonCoreError::AuthorityJsonLimitExceeded { .. }
            | DaemonCoreError::Json { .. }
    )
}

#[cfg(unix)]
fn create_repair_archive(daemon: &CapabilityDir) -> Result<(String, CapabilityDir)> {
    let parent_path = daemon.display_path().join(REGISTRY_REPAIR_ARCHIVE_DIR_NAME);
    let parent = daemon
        .ensure_dir(
            OsStr::new(REGISTRY_REPAIR_ARCHIVE_DIR_NAME),
            PRIVATE_DIRECTORY_MODE,
        )
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to prepare private registry repair archive",
                &parent_path,
                source,
            )
        })?;
    let stamp = now_unix();
    for attempt in 0..MAX_REPAIR_ARCHIVE_ATTEMPTS {
        let name = if attempt == 0 {
            format!("repair-{stamp}")
        } else {
            format!("repair-{stamp}-{attempt}")
        };
        match parent.create_dir(OsStr::new(&name), PRIVATE_DIRECTORY_MODE) {
            Ok(archive) => {
                parent.sync().map_err(|source| {
                    DaemonCoreError::io(
                        "failed to synchronize registry repair archive directory",
                        &parent_path,
                        source,
                    )
                })?;
                return Ok((name, archive));
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(DaemonCoreError::io(
                    "failed to create private registry repair archive",
                    parent_path.join(&name),
                    source,
                ));
            }
        }
    }
    Err(DaemonCoreError::io(
        "failed to reserve a unique registry repair archive",
        parent_path,
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "registry repair archive names are exhausted for this second",
        ),
    ))
}

#[cfg(unix)]
fn open_repair_archive(daemon: &CapabilityDir, name: &str) -> Result<CapabilityDir> {
    let path = daemon
        .display_path()
        .join(REGISTRY_REPAIR_ARCHIVE_DIR_NAME)
        .join(name);
    let invalid = |message: &str| {
        DaemonCoreError::io(
            "registry repair journal names an invalid archive",
            &path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_string()),
        )
    };
    if !name.starts_with("repair-")
        || !name[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid("archive name is not a generated repair archive"));
    }
    daemon
        .open_private_dir(
            OsStr::new(REGISTRY_REPAIR_ARCHIVE_DIR_NAME),
            PRIVATE_DIRECTORY_MODE,
        )
        .and_then(|parent| parent.open_private_dir(OsStr::new(name), PRIVATE_DIRECTORY_MODE))
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open private registry repair archive",
                &path,
                source,
            )
        })
}

fn archive_file_name(prefix: &str, source: &str) -> String {
    format!("{prefix}-{}", source.trim_start_matches('.'))
}

/// Archives every present canonical and checkpoint-authority file, then the
/// exact restore images, and returns the receipt binding them.
#[cfg(unix)]
fn archive_repair_inputs(
    daemon: &CapabilityDir,
    archive: &CapabilityDir,
    plan: &CanonicalRepairPlan,
    startup_error: String,
) -> Result<RepairReceipt> {
    let mut archived = Vec::new();
    for (name, max_bytes) in CANONICAL_REGISTRY_FILES
        .into_iter()
        .chain(CHECKPOINT_AUTHORITY_FILE_NAMES)
    {
        let Some(raw) = read_optional(daemon, name, max_bytes)? else {
            continue;
        };
        let archive_name = archive_file_name("original", name);
        write_repair_file(archive, &archive_name, &raw)?;
        archived.push(ArchivedFile {
            source: name.to_string(),
            archive: archive_name,
            digest: RegistryArtifactDigest::of(&raw),
        });
    }
    let mut writes = Vec::new();
    for file in &plan.files {
        let CanonicalFileAction::Restore {
            bytes,
            source,
            reencoding_matched,
        } = &file.action
        else {
            continue;
        };
        let original = file.current.as_deref().map(RegistryArtifactDigest::of);
        let archived_original = archived
            .iter()
            .find(|entry| entry.source == file.name)
            .map(|entry| &entry.digest);
        if original.as_ref() != archived_original {
            return Err(DaemonCoreError::InvalidTaskWatchRegistry {
                root: daemon.display_path().to_path_buf(),
                message: format!(
                    "{} changed while its repair archive was being written",
                    file.name
                ),
            });
        }
        let target_archive = archive_file_name("target", file.name);
        write_repair_file(archive, &target_archive, bytes)?;
        writes.push(PlannedWrite {
            file: file.name.to_string(),
            original,
            target: RegistryArtifactDigest::of(bytes),
            target_archive,
            source: *source,
            replaced_bytes_were_reencoding: *reencoding_matched,
        });
    }
    Ok(RepairReceipt {
        schema_version: REPAIR_SCHEMA_VERSION,
        created_at_unix: now_unix(),
        authority: plan.authority,
        generation: plan.generation,
        applied_delta_revision: plan.applied_delta_revision,
        startup_error,
        archived,
        writes,
    })
}

#[cfg(unix)]
fn write_repair_file(directory: &CapabilityDir, name: &str, bytes: &[u8]) -> Result<()> {
    directory
        .write_json_atomically(OsStr::new(name), bytes, REPAIR_ARCHIVE_WRITE_TEMP_PREFIX)
        .map_err(|error| {
            DaemonCoreError::io(
                "failed to archive registry repair evidence",
                directory.display_path().join(name),
                error.source,
            )
        })
}

#[cfg(unix)]
fn encode_repair_json(
    directory: &CapabilityDir,
    name: &str,
    value: &impl Serialize,
) -> Result<Vec<u8>> {
    let path = directory.display_path().join(name);
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode registry repair metadata for",
            &path,
            source,
        )
    })?;
    if bytes.len() > MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES {
        return Err(DaemonCoreError::io(
            "registry repair metadata exceeds its bound",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} bytes; maximum is {MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES}",
                    bytes.len()
                ),
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_repair_journal(root: &Path, daemon: &CapabilityDir) -> Result<Option<RepairJournal>> {
    let Some(raw) = read_optional(
        daemon,
        CHECKPOINT_REPAIR_JOURNAL_FILE_NAME,
        MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES,
    )?
    else {
        return Ok(None);
    };
    let journal: RepairJournal = serde_json::from_slice(&raw).map_err(|source| {
        DaemonCoreError::json(
            "failed to decode task/watch registry repair journal from",
            daemon_dir(root).join(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
            source,
        )
    })?;
    if journal.schema_version != REPAIR_SCHEMA_VERSION {
        return Err(DaemonCoreError::InvalidTaskWatchRegistry {
            root: root.to_path_buf(),
            message: format!(
                "registry repair journal schema version {} is unsupported",
                journal.schema_version
            ),
        });
    }
    Ok(Some(journal))
}

/// Verifies an interrupted repair against its archive and either reports it
/// (dry run) or completes exactly the archived plan.
#[cfg(unix)]
fn resume_repair(
    root: &Path,
    daemon: &CapabilityDir,
    journal: RepairJournal,
    apply: bool,
) -> Result<RegistryCheckpointRepairReport> {
    let archive = open_repair_archive(daemon, &journal.archive)?;
    let receipt_raw = read_optional(
        &archive,
        REPAIR_RECEIPT_FILE_NAME,
        MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES,
    )?
    .ok_or_else(|| interrupted_refusal(root, "the repair archive receipt is missing"))?;
    if !journal.receipt.matches(&receipt_raw) {
        return Err(interrupted_refusal(
            root,
            "the repair archive receipt does not match the repair journal digest",
        ));
    }
    let receipt: RepairReceipt = serde_json::from_slice(&receipt_raw).map_err(|source| {
        DaemonCoreError::json(
            "failed to decode registry repair receipt from",
            archive.display_path().join(REPAIR_RECEIPT_FILE_NAME),
            source,
        )
    })?;
    if receipt.schema_version != REPAIR_SCHEMA_VERSION {
        return Err(interrupted_refusal(
            root,
            "the repair archive receipt schema is unsupported",
        ));
    }
    // Checkpoint authority must be byte-identical to what the plan was
    // verified against; only planned canonical files may have advanced.
    for (name, max_bytes) in CANONICAL_REGISTRY_FILES
        .into_iter()
        .chain(CHECKPOINT_AUTHORITY_FILE_NAMES)
    {
        let current = read_optional(daemon, name, max_bytes)?;
        let current = current.as_deref().map(RegistryArtifactDigest::of);
        let original = receipt
            .archived
            .iter()
            .find(|entry| entry.source == name)
            .map(|entry| entry.digest.clone());
        let unchanged = current == original;
        let advanced = receipt
            .writes
            .iter()
            .find(|write| write.file == name)
            .is_some_and(|write| current.as_ref() == Some(&write.target));
        if !unchanged && !advanced {
            return Err(interrupted_refusal(
                root,
                &format!("{name} changed outside the interrupted repair"),
            ));
        }
    }
    let mut targets = BTreeMap::new();
    for write in &receipt.writes {
        let (_, max_bytes) = CANONICAL_REGISTRY_FILES
            .into_iter()
            .find(|(name, _)| *name == write.file)
            .ok_or_else(|| interrupted_refusal(root, "the receipt plans a non-registry write"))?;
        let image = read_optional(&archive, &write.target_archive, max_bytes)?
            .filter(|image| write.target.matches(image))
            .ok_or_else(|| {
                interrupted_refusal(root, "an archived restore image is missing or altered")
            })?;
        targets.insert(write.file.clone(), image);
    }

    let mut report = RegistryCheckpointRepairReport {
        status: RegistryCheckpointRepairStatus::InterruptedRepair,
        applied: apply,
        startup_error: Some(receipt.startup_error.clone()),
        authority: Some(receipt.authority),
        generation: receipt.generation,
        applied_delta_revision: Some(receipt.applied_delta_revision),
        files: Vec::new(),
        wal: None,
        archive_path: Some(archive.display_path().to_path_buf()),
        reason: None,
    };
    let mut candidate = Vec::with_capacity(2);
    for (name, max_bytes) in CANONICAL_REGISTRY_FILES {
        let write = receipt.writes.iter().find(|write| write.file == name);
        let image = match targets.get(name) {
            Some(image) => Some(image.clone()),
            None => read_optional(daemon, name, max_bytes)?,
        };
        let original = receipt
            .archived
            .iter()
            .find(|entry| entry.source == name)
            .map(|entry| entry.digest.clone());
        report.files.push(RegistryRepairFileReport {
            file: name.to_string(),
            path: daemon_dir(root).join(name),
            state: if write.is_some() {
                RegistryRepairFileState::Restore
            } else {
                RegistryRepairFileState::Matches
            },
            current: original,
            expected: image.as_deref().map_or_else(
                || RegistryArtifactDigest::of(&[]),
                RegistryArtifactDigest::of,
            ),
            source: write.map(|write| write.source),
            replaced_bytes_were_reencoding: write
                .and_then(|write| write.replaced_bytes_were_reencoding),
            detail: None,
        });
        candidate.push(image);
    }
    let watches = candidate.pop().flatten();
    let tasks = candidate.pop().flatten();
    if let Err(reason) = verify_candidate(root, daemon, tasks, watches, &mut report)? {
        return Err(interrupted_refusal(root, &reason));
    }
    if !apply {
        return Ok(report);
    }
    complete_repair(root, daemon, &archive, &receipt, &mut report)?;
    report.status = RegistryCheckpointRepairStatus::Resumed;
    Ok(report)
}

/// Publishes every planned image that is not yet in place, verifies the
/// result through normal resolution, and retires the repair journal into the
/// archive so it remains as evidence.
#[cfg(unix)]
fn complete_repair(
    root: &Path,
    daemon: &CapabilityDir,
    archive: &CapabilityDir,
    receipt: &RepairReceipt,
    report: &mut RegistryCheckpointRepairReport,
) -> Result<()> {
    // Mirror checkpoint publication order: watch before task.
    for name in [WATCH_REGISTRY_FILE_NAME, TASK_REGISTRY_FILE_NAME] {
        let Some(write) = receipt.writes.iter().find(|write| write.file == name) else {
            continue;
        };
        let (_, max_bytes) = CANONICAL_REGISTRY_FILES
            .into_iter()
            .find(|(file, _)| *file == name)
            .unwrap_or((name, MAX_TASK_REGISTRY_BYTES));
        if read_optional(daemon, name, max_bytes)?
            .is_some_and(|current| write.target.matches(&current))
        {
            continue;
        }
        let image = read_optional(archive, &write.target_archive, max_bytes)?
            .filter(|image| write.target.matches(image))
            .ok_or_else(|| {
                interrupted_refusal(root, "an archived restore image is missing or altered")
            })?;
        if name == TASK_REGISTRY_FILE_NAME {
            write_anchored_task_registry(daemon, &task_registry_path(root), &image, || Ok(()))?;
        } else {
            write_anchored_watch_registry(daemon, &watch_registry_path(root), &image)?;
        }
        repair_phase(name)?;
    }
    let tasks = read_optional(daemon, TASK_REGISTRY_FILE_NAME, MAX_TASK_REGISTRY_BYTES)?;
    let watches = read_optional(daemon, WATCH_REGISTRY_FILE_NAME, MAX_WATCH_REGISTRY_BYTES)?;
    if let Err(reason) = verify_candidate(root, daemon, tasks, watches, report)? {
        return Err(interrupted_refusal(root, &reason));
    }
    daemon
        .rename_to_noreplace(
            OsStr::new(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
            archive,
            OsStr::new(REPAIR_COMPLETED_JOURNAL_FILE_NAME),
        )
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to retire completed task/watch registry repair journal",
                daemon
                    .display_path()
                    .join(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
                source,
            )
        })?;
    for file in &mut report.files {
        if file.state == RegistryRepairFileState::Restore {
            file.state = RegistryRepairFileState::Restored;
        }
    }
    Ok(())
}

fn interrupted_refusal(root: &Path, reason: &str) -> DaemonCoreError {
    DaemonCoreError::InvalidTaskWatchRegistry {
        root: root.to_path_buf(),
        message: format!(
            "interrupted registry repair cannot be completed safely: {reason}; the repair \
             journal, archive, and current files were left unchanged for manual inspection"
        ),
    }
}

/// Deterministic interruption points for crash-recovery tests.
fn repair_phase(phase: &'static str) -> Result<()> {
    #[cfg(test)]
    if INJECT_REPAIR_INTERRUPTION_AFTER.with(|configured| configured.get() == Some(phase)) {
        INJECT_REPAIR_INTERRUPTION_AFTER.with(|configured| configured.set(None));
        return Err(DaemonCoreError::io(
            "injected registry repair interruption",
            PathBuf::from(phase),
            std::io::Error::other("injected interruption"),
        ));
    }
    #[cfg(any(test, debug_assertions))]
    if std::env::var("PACKET28_REGISTRY_REPAIR_EXIT_AFTER").as_deref() == Ok(phase) {
        std::process::exit(87);
    }
    let _ = phase;
    Ok(())
}

#[cfg(test)]
pub(super) fn inject_repair_interruption_after(phase: &'static str) {
    INJECT_REPAIR_INTERRUPTION_AFTER.with(|configured| configured.set(Some(phase)));
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use packet28_daemon_protocol::commands::WatchSpec;
    use packet28_daemon_protocol::task::WatchRegistration;
    use tempfile::{tempdir, TempDir};

    use super::*;
    use crate::storage::{
        append_task_watch_registry_delta, load_task_watch_registry_with_deltas, RegistryDeltaBatch,
        RegistryRevision, RegistryRevisionRange,
    };
    use crate::task_store_lease::{acquire_daemon_instance_lease, acquire_task_store_writer_lease};

    const MANIFEST: &str = "task-watch-checkpoint-v1.json";
    const JOURNAL: &str = ".task-watch-checkpoint-v1.journal.json";
    const JOURNAL_TASKS: &str = ".task-watch-checkpoint-v1.journal.tasks";
    const JOURNAL_WATCHES: &str = ".task-watch-checkpoint-v1.journal.watches";
    const WAL: &str = "task-watch-registry-delta-v1.wal";

    fn task(task_id: &str, watch_ids: &[&str]) -> TaskRecord {
        TaskRecord {
            task_id: task_id.to_string(),
            watch_ids: watch_ids.iter().map(|id| (*id).to_string()).collect(),
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

    fn checkpoint(root: &Path, tasks: &[TaskRecord], watches: &[WatchRegistration]) {
        save_task_watch_registry_checkpoint(
            root,
            &TaskRegistry {
                tasks: tasks
                    .iter()
                    .map(|task| (task.task_id.clone(), task.clone()))
                    .collect(),
            },
            &WatchRegistry {
                watches: watches.to_vec(),
            },
        )
        .unwrap();
    }

    fn file(root: &Path, name: &str) -> PathBuf {
        daemon_dir(root).join(name)
    }

    /// Every regular state file except advisory locks, which maintenance
    /// admission may create.
    fn state(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(daemon_dir(root))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| entry.file_name().into_string().unwrap())
            .filter(|name| !name.ends_with(".lock"))
            .map(|name| {
                let bytes = fs::read(file(root, &name)).unwrap();
                (name, bytes)
            })
            .collect()
    }

    fn archives(root: &Path) -> Vec<PathBuf> {
        match fs::read_dir(file(root, REGISTRY_REPAIR_ARCHIVE_DIR_NAME)) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{error}"),
        }
    }

    /// Re-serializes JSON compactly: the same document, different bytes.
    fn reformat(path: &Path) -> Vec<u8> {
        let original = fs::read(path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let compact = serde_json::to_vec(&value).unwrap();
        assert_ne!(compact, original);
        fs::write(path, &compact).unwrap();
        original
    }

    fn committed_store() -> (TempDir, Vec<u8>) {
        let root = tempdir().unwrap();
        // Unknown fields written by a newer writer must survive canonical
        // re-encoding and still reproduce the committed digest.
        fs::create_dir_all(daemon_dir(root.path())).unwrap();
        fs::write(
            file(root.path(), TASK_REGISTRY_FILE_NAME),
            br#"{"future_root":{"z":[3,1,2]},"tasks":{"alpha":{"task_id":"alpha","future_field":"kept"}}}"#,
        )
        .unwrap();
        checkpoint(
            root.path(),
            &[task("alpha", &["w"]), task("beta", &[]), task("gamma", &[])],
            &[watch("w", "alpha")],
        );
        let committed = fs::read(file(root.path(), TASK_REGISTRY_FILE_NAME)).unwrap();
        assert!(String::from_utf8_lossy(&committed).contains("future_field"));
        (root, committed)
    }

    fn assert_startup_rejects(root: &Path) -> String {
        load_task_registry(root).unwrap_err().to_string()
    }

    #[test]
    fn reformatted_committed_task_registry_is_restored_exactly() {
        let (root, committed) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let edited = {
            reformat(&task_path);
            fs::read(&task_path).unwrap()
        };
        assert!(assert_startup_rejects(root.path()).contains("storage repair"));
        let before = state(root.path());

        let dry = inspect_task_watch_registry_checkpoint_repair(root.path()).unwrap();
        assert_eq!(dry.status, RegistryCheckpointRepairStatus::Repairable);
        assert_eq!(
            dry.authority,
            Some(RegistryCheckpointAuthority::CommittedManifest)
        );
        assert_eq!(dry.files[0].state, RegistryRepairFileState::Restore);
        assert_eq!(
            dry.files[0].source,
            Some(RegistryRepairCandidateSource::CanonicalReencoding)
        );
        assert_eq!(dry.files[1].state, RegistryRepairFileState::Matches);
        assert_eq!(state(root.path()), before, "dry run must not write");
        assert!(archives(root.path()).is_empty());

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();
        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(applied.files[0].state, RegistryRepairFileState::Restored);
        assert_eq!(fs::read(&task_path).unwrap(), committed);
        let loaded = load_task_registry(root.path()).unwrap();
        assert_eq!(
            loaded.tasks.keys().collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        assert!(!file(root.path(), CHECKPOINT_REPAIR_JOURNAL_FILE_NAME).exists());
        for name in [MANIFEST, JOURNAL, JOURNAL_TASKS, JOURNAL_WATCHES] {
            assert_eq!(fs::read(file(root.path(), name)).unwrap(), before[name]);
        }

        let archive = applied.archive_path.unwrap();
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::read(archive.join("original-task-registry-v1.json")).unwrap(),
            edited
        );
        assert_eq!(
            fs::read(archive.join("target-task-registry-v1.json")).unwrap(),
            committed
        );
        for name in [MANIFEST, JOURNAL, JOURNAL_TASKS, JOURNAL_WATCHES] {
            assert_eq!(
                fs::read(archive.join(archive_file_name("original", name))).unwrap(),
                before[name]
            );
        }
        let journal: RepairJournal = serde_json::from_slice(
            &fs::read(archive.join(REPAIR_COMPLETED_JOURNAL_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert!(journal
            .receipt
            .matches(&fs::read(archive.join(REPAIR_RECEIPT_FILE_NAME)).unwrap()));

        let again = repair_task_watch_registry_checkpoint(root.path()).unwrap();
        assert_eq!(again.status, RegistryCheckpointRepairStatus::Clean);
    }

    #[test]
    fn trailing_whitespace_on_committed_watch_registry_is_restored_exactly() {
        let (root, _) = committed_store();
        let watch_path = file(root.path(), WATCH_REGISTRY_FILE_NAME);
        let committed = fs::read(&watch_path).unwrap();
        fs::write(&watch_path, [committed.as_slice(), b" \n"].concat()).unwrap();
        assert_startup_rejects(root.path());

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(applied.files[1].state, RegistryRepairFileState::Restored);
        assert_eq!(fs::read(&watch_path).unwrap(), committed);
        assert_eq!(load_watch_registry(root.path()).unwrap().watches.len(), 1);
    }

    fn assert_unrecoverable_without_change(root: &Path, detail: &str) {
        let before = state(root);
        for report in [
            inspect_task_watch_registry_checkpoint_repair(root).unwrap(),
            repair_task_watch_registry_checkpoint(root).unwrap(),
        ] {
            assert_eq!(report.status, RegistryCheckpointRepairStatus::Unrecoverable);
            assert!(report.is_unrecoverable());
            assert_eq!(
                report.files[0].state,
                RegistryRepairFileState::Unrecoverable
            );
            assert!(
                report.files[0]
                    .detail
                    .as_deref()
                    .is_some_and(|text| text.contains(detail)),
                "{report:?}"
            );
            assert!(report.archive_path.is_none());
        }
        assert_eq!(state(root), before, "unrecoverable state must not change");
        assert!(archives(root).is_empty());
        assert_startup_rejects(root);
    }

    #[test]
    fn substantive_committed_edit_is_refused_without_adoption() {
        let (root, _) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&task_path).unwrap()).unwrap();
        value["tasks"]["beta"]["last_event_seq"] = serde_json::json!(41);
        fs::write(&task_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        assert_unrecoverable_without_change(root.path(), "content differs");
    }

    #[test]
    fn duplicate_keys_torn_and_missing_images_are_refused() {
        let (root, committed) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let mut duplicate = b"{\"tasks\":{},".to_vec();
        duplicate.extend_from_slice(&committed[1..]);
        fs::write(&task_path, duplicate).unwrap();
        assert_unrecoverable_without_change(root.path(), "duplicate");

        fs::write(&task_path, &committed[..committed.len() / 2]).unwrap();
        assert_unrecoverable_without_change(root.path(), "not strict registry JSON");

        fs::remove_file(&task_path).unwrap();
        assert_unrecoverable_without_change(root.path(), "missing");
    }

    #[test]
    fn oversized_canonical_image_is_refused_by_the_bounded_reader() {
        let (root, _) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        fs::OpenOptions::new()
            .write(true)
            .open(&task_path)
            .unwrap()
            .set_len(MAX_TASK_REGISTRY_BYTES as u64 + 1)
            .unwrap();
        let before = fs::metadata(&task_path).unwrap().len();

        assert!(inspect_task_watch_registry_checkpoint_repair(root.path()).is_err());
        assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        assert_eq!(fs::metadata(&task_path).unwrap().len(), before);
        assert!(archives(root.path()).is_empty());
    }

    #[test]
    fn unrecoverable_checkpoint_metadata_is_reported_not_guessed() {
        let (root, _) = committed_store();
        reformat(&file(root.path(), TASK_REGISTRY_FILE_NAME));
        fs::write(file(root.path(), MANIFEST), b"{\"schema_version\":1,").unwrap();
        let before = state(root.path());

        let report = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(report.status, RegistryCheckpointRepairStatus::Unrecoverable);
        assert!(report
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("checkpoint authority is not usable")));
        assert_eq!(state(root.path()), before);
    }

    /// Composes the durable state left by a crash after the watch phase of a
    /// second publication: manifest and task still name generation 1, while
    /// the watch image and journal belong to the unfinished generation 2.
    fn precommit_store() -> (TempDir, BTreeMap<String, Vec<u8>>) {
        let root = tempdir().unwrap();
        checkpoint(
            root.path(),
            &[task("alpha", &["w"])],
            &[watch("w", "alpha")],
        );
        let first = state(root.path());
        checkpoint(
            root.path(),
            &[task("alpha", &["w"]), task("beta", &[])],
            &[watch("w", "alpha")],
        );
        for name in [TASK_REGISTRY_FILE_NAME, MANIFEST] {
            fs::write(file(root.path(), name), &first[name]).unwrap();
        }
        assert_eq!(
            fs::read(file(root.path(), JOURNAL_TASKS)).unwrap(),
            first[TASK_REGISTRY_FILE_NAME]
        );
        assert_ne!(
            fs::read(file(root.path(), WATCH_REGISTRY_FILE_NAME)).unwrap(),
            first[WATCH_REGISTRY_FILE_NAME]
        );
        assert_eq!(
            load_task_registry(root.path()).unwrap().tasks.len(),
            1,
            "an untouched precommit crash recovers the journal base"
        );
        (root, first)
    }

    #[test]
    fn precommit_mixed_generation_edit_restores_the_authenticated_base() {
        let (root, first) = precommit_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        fs::write(
            &task_path,
            [first[TASK_REGISTRY_FILE_NAME].as_slice(), b" "].concat(),
        )
        .unwrap();
        assert!(assert_startup_rejects(root.path()).contains("generations disagree"));

        let dry = inspect_task_watch_registry_checkpoint_repair(root.path()).unwrap();
        assert_eq!(dry.status, RegistryCheckpointRepairStatus::Repairable);
        assert_eq!(
            dry.authority,
            Some(RegistryCheckpointAuthority::PrecommitJournalBase)
        );
        assert_eq!(dry.files[0].replaced_bytes_were_reencoding, Some(true));
        assert_eq!(dry.wal.as_ref().map(|wal| wal.present), Some(false));

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();
        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        for name in [TASK_REGISTRY_FILE_NAME, WATCH_REGISTRY_FILE_NAME, MANIFEST] {
            assert_eq!(fs::read(file(root.path(), name)).unwrap(), first[name]);
        }
        let loaded = load_task_registry(root.path()).unwrap();
        assert_eq!(loaded.tasks.keys().collect::<Vec<_>>(), ["alpha"]);
        // The committed state accepts a further checkpoint normally.
        checkpoint(root.path(), &[task("alpha", &[]), task("delta", &[])], &[]);
        assert_eq!(load_task_registry(root.path()).unwrap().tasks.len(), 2);
    }

    #[test]
    fn precommit_substantive_edit_is_archived_and_reported_not_adopted() {
        let (root, first) = precommit_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let mut value: serde_json::Value =
            serde_json::from_slice(&first[TASK_REGISTRY_FILE_NAME]).unwrap();
        value["tasks"]["alpha"]["last_error"] = serde_json::json!("hand edit");
        let edited = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&task_path, &edited).unwrap();

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(applied.files[0].replaced_bytes_were_reencoding, Some(false));
        assert_eq!(
            fs::read(&task_path).unwrap(),
            first[TASK_REGISTRY_FILE_NAME]
        );
        let archive = applied.archive_path.unwrap();
        assert_eq!(
            fs::read(archive.join("original-task-registry-v1.json")).unwrap(),
            edited
        );
        assert!(load_task_registry(root.path()).unwrap().tasks["alpha"]
            .last_error
            .is_none());
    }

    fn write_wal_header(root: &Path, base_revision: u64) {
        let mut header = Vec::with_capacity(56);
        header.extend_from_slice(b"P28RDW01");
        header.extend_from_slice(&1_u32.to_le_bytes());
        header.extend_from_slice(&56_u32.to_le_bytes());
        header.extend_from_slice(&base_revision.to_le_bytes());
        let checksum = blake3::hash(&header);
        header.extend_from_slice(checksum.as_bytes());
        let path = file(root, WAL);
        fs::write(&path, header).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn precommit_restore_refuses_when_wal_history_does_not_reach_the_base() {
        let (root, first) = precommit_store();
        fs::write(
            file(root.path(), TASK_REGISTRY_FILE_NAME),
            [first[TASK_REGISTRY_FILE_NAME].as_slice(), b" "].concat(),
        )
        .unwrap();
        write_wal_header(root.path(), 5);
        let before = state(root.path());

        for report in [
            inspect_task_watch_registry_checkpoint_repair(root.path()).unwrap(),
            repair_task_watch_registry_checkpoint(root.path()).unwrap(),
        ] {
            assert_eq!(report.status, RegistryCheckpointRepairStatus::Unrecoverable);
            assert!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("WAL")
                        && reason.contains("newer than committed checkpoint")),
                "{report:?}"
            );
        }
        assert_eq!(state(root.path()), before);
        assert!(archives(root.path()).is_empty());
    }

    #[test]
    fn unpublished_first_checkpoint_restores_the_journal_base() {
        let root = tempdir().unwrap();
        checkpoint(root.path(), &[task("alpha", &[])], &[]);
        fs::remove_file(file(root.path(), MANIFEST)).unwrap();
        let base_tasks = fs::read(file(root.path(), JOURNAL_TASKS)).unwrap();
        fs::write(
            file(root.path(), TASK_REGISTRY_FILE_NAME),
            [base_tasks.as_slice(), b"\n"].concat(),
        )
        .unwrap();
        assert_startup_rejects(root.path());

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(
            applied.authority,
            Some(RegistryCheckpointAuthority::UnpublishedJournalBase)
        );
        assert_eq!(
            fs::read(file(root.path(), TASK_REGISTRY_FILE_NAME)).unwrap(),
            base_tasks
        );
        assert!(
            !file(root.path(), MANIFEST).exists(),
            "no manifest is invented"
        );
        assert!(load_task_registry(root.path()).unwrap().tasks.is_empty());
    }

    #[test]
    fn pending_wal_frames_are_preserved_and_replayed_after_restore() {
        let (root, committed) = committed_store();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(task("pending", &[])),
        )
        .unwrap();
        let wal = fs::read(file(root.path(), WAL)).unwrap();
        reformat(&file(root.path(), TASK_REGISTRY_FILE_NAME));

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        let replay = applied.wal.unwrap();
        assert_eq!(
            (replay.checkpoint_revision, replay.replayed_revision),
            (0, 1)
        );
        assert_eq!(replay.torn_suffix_bytes, 0);
        assert_eq!(fs::read(file(root.path(), WAL)).unwrap(), wal);
        assert_eq!(
            fs::read(file(root.path(), TASK_REGISTRY_FILE_NAME)).unwrap(),
            committed
        );
        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert_eq!(
            loaded.tasks.tasks.keys().collect::<Vec<_>>(),
            ["alpha", "beta", "gamma", "pending"]
        );
    }

    #[test]
    fn interrupted_repair_blocks_startup_until_resumed() {
        for phase in ["journal", TASK_REGISTRY_FILE_NAME] {
            let (root, committed) = committed_store();
            let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
            reformat(&task_path);

            inject_repair_interruption_after(phase);
            assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
            assert!(file(root.path(), CHECKPOINT_REPAIR_JOURNAL_FILE_NAME).exists());
            assert!(
                assert_startup_rejects(root.path()).contains("interrupted"),
                "{phase}: a pending repair must never be read as committed state"
            );
            let before = state(root.path());

            let dry = inspect_task_watch_registry_checkpoint_repair(root.path()).unwrap();
            assert_eq!(
                dry.status,
                RegistryCheckpointRepairStatus::InterruptedRepair
            );
            assert_eq!(state(root.path()), before);

            let resumed = repair_task_watch_registry_checkpoint(root.path()).unwrap();
            assert_eq!(
                resumed.status,
                RegistryCheckpointRepairStatus::Resumed,
                "{phase}"
            );
            assert_eq!(fs::read(&task_path).unwrap(), committed);
            assert!(!file(root.path(), CHECKPOINT_REPAIR_JOURNAL_FILE_NAME).exists());
            assert_eq!(load_task_registry(root.path()).unwrap().tasks.len(), 3);
            assert_eq!(archives(root.path()).len(), 1);
        }
    }

    #[test]
    fn interruption_before_the_journal_leaves_state_unchanged_and_retries() {
        let (root, committed) = committed_store();
        reformat(&file(root.path(), TASK_REGISTRY_FILE_NAME));
        let before = state(root.path());

        inject_repair_interruption_after("archive");
        assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        assert_eq!(state(root.path()), before);
        assert_eq!(
            archives(root.path()).len(),
            1,
            "partial archive is retained"
        );

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();
        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(
            fs::read(file(root.path(), TASK_REGISTRY_FILE_NAME)).unwrap(),
            committed
        );
        assert_eq!(archives(root.path()).len(), 2);
    }

    #[test]
    fn resume_refuses_when_files_changed_outside_the_repair() {
        let (root, _) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        reformat(&task_path);
        inject_repair_interruption_after("journal");
        assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        fs::write(&task_path, b"{\"tasks\":{}}").unwrap();
        let before = state(root.path());

        let error = repair_task_watch_registry_checkpoint(root.path()).unwrap_err();

        assert!(error.to_string().contains("changed outside"), "{error}");
        assert_eq!(state(root.path()), before);
    }

    #[test]
    fn live_daemon_or_writer_refuses_repair_without_mutation() {
        let (root, _) = committed_store();
        reformat(&file(root.path(), TASK_REGISTRY_FILE_NAME));
        let before = state(root.path());
        {
            let _instance = acquire_daemon_instance_lease(root.path()).unwrap();
            let error = repair_task_watch_registry_checkpoint(root.path()).unwrap_err();
            assert!(error.to_string().contains("exclusive task-store access"));
        }
        {
            let _writer = acquire_task_store_writer_lease(root.path()).unwrap();
            assert!(inspect_task_watch_registry_checkpoint_repair(root.path()).is_err());
            assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        }
        assert_eq!(state(root.path()), before);
        assert!(archives(root.path()).is_empty());
    }

    #[test]
    fn symlinked_registry_and_state_directory_are_refused() {
        let (root, committed) = committed_store();
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let outside = root.path().join("outside.json");
        fs::write(&outside, [committed.as_slice(), b" "].concat()).unwrap();
        fs::remove_file(&task_path).unwrap();
        symlink(&outside, &task_path).unwrap();

        assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        assert!(fs::symlink_metadata(&task_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(&outside).unwrap(),
            [committed.as_slice(), b" "].concat()
        );

        fs::remove_file(&task_path).unwrap();
        fs::write(&task_path, [committed.as_slice(), b" "].concat()).unwrap();
        let state_dir = root.path().join(".packet28");
        let moved = root.path().join("moved-state");
        fs::rename(&state_dir, &moved).unwrap();
        symlink(&moved, &state_dir).unwrap();

        assert!(repair_task_watch_registry_checkpoint(root.path()).is_err());
        assert_eq!(
            fs::read(moved.join("daemon").join(TASK_REGISTRY_FILE_NAME)).unwrap(),
            [committed.as_slice(), b" "].concat()
        );
        assert!(!moved
            .join("daemon")
            .join(REGISTRY_REPAIR_ARCHIVE_DIR_NAME)
            .exists());
    }

    #[test]
    fn escaped_unicode_reformatting_reproduces_the_committed_bytes() {
        let root = tempdir().unwrap();
        let mut record = task("alpha", &[]);
        record.last_error = Some("caf\u{e9} \u{2603}".to_string());
        checkpoint(root.path(), &[record], &[]);
        let task_path = file(root.path(), TASK_REGISTRY_FILE_NAME);
        let committed = fs::read(&task_path).unwrap();
        let text = String::from_utf8(committed.clone()).unwrap();
        let escaped = text
            .replace('\u{e9}', "\\u00e9")
            .replace('\u{2603}', "\\u2603");
        assert_ne!(escaped.as_bytes(), committed.as_slice());
        fs::write(&task_path, escaped).unwrap();

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        assert_eq!(fs::read(&task_path).unwrap(), committed);
    }

    #[test]
    fn torn_wal_suffix_is_tolerated_without_rewriting_the_wal() {
        let (root, _) = committed_store();
        append_task_watch_registry_delta(
            root.path(),
            RegistryRevisionRange::single(RegistryRevision::new(1)).unwrap(),
            &RegistryDeltaBatch::default().upsert_task(task("pending", &[])),
        )
        .unwrap();
        let wal_path = file(root.path(), WAL);
        let mut wal = fs::read(&wal_path).unwrap();
        wal.extend_from_slice(b"P28RDF01torn");
        fs::write(&wal_path, &wal).unwrap();
        reformat(&file(root.path(), TASK_REGISTRY_FILE_NAME));

        let applied = repair_task_watch_registry_checkpoint(root.path()).unwrap();

        assert_eq!(applied.status, RegistryCheckpointRepairStatus::Repaired);
        let replay = applied.wal.unwrap();
        assert_eq!(replay.replayed_revision, 1);
        assert_eq!(replay.torn_suffix_bytes, 12);
        assert_eq!(
            fs::read(&wal_path).unwrap(),
            wal,
            "repair never rewrites the WAL"
        );
        let loaded = load_task_watch_registry_with_deltas(root.path()).unwrap();
        assert!(loaded.tasks.tasks.contains_key("pending"));
    }

    #[test]
    fn clean_store_reports_clean_without_writes() {
        let (root, _) = committed_store();
        let before = state(root.path());
        let report = repair_task_watch_registry_checkpoint(root.path()).unwrap();
        assert_eq!(report.status, RegistryCheckpointRepairStatus::Clean);
        assert!(report.startup_registry_ready());
        assert_eq!(state(root.path()), before);
        assert!(archives(root.path()).is_empty());
    }
}
