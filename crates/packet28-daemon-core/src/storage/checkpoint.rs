use super::*;

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_MANIFEST_FILE_NAME: &str = "task-watch-checkpoint-v1.json";
const CHECKPOINT_JOURNAL_FILE_NAME: &str = ".task-watch-checkpoint-v1.journal.json";
const CHECKPOINT_JOURNAL_TASK_FILE_NAME: &str = ".task-watch-checkpoint-v1.journal.tasks";
const CHECKPOINT_JOURNAL_WATCH_FILE_NAME: &str = ".task-watch-checkpoint-v1.journal.watches";
const CHECKPOINT_MANIFEST_WRITE_TEMP_PREFIX: &str =
    ".task-watch-checkpoint-v1.json.packet28-write.";
const CHECKPOINT_JOURNAL_WRITE_TEMP_PREFIX: &str =
    ".task-watch-checkpoint-v1.journal.packet28-write.";
const CHECKPOINT_JOURNAL_TASK_WRITE_TEMP_PREFIX: &str =
    ".task-watch-checkpoint-v1.journal.tasks.packet28-write.";
const CHECKPOINT_JOURNAL_WATCH_WRITE_TEMP_PREFIX: &str =
    ".task-watch-checkpoint-v1.journal.watches.packet28-write.";
const MAX_CHECKPOINT_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct RegistryRawPair {
    pub(super) tasks: Option<Vec<u8>>,
    pub(super) watches: Option<Vec<u8>>,
    applied_delta_revision: u64,
    canonical_recovery: CanonicalRecovery,
}

impl RegistryRawPair {
    pub(super) fn canonical_recovery(&self) -> CanonicalRecovery {
        self.canonical_recovery
    }

    pub(super) fn applied_delta_revision(&self) -> u64 {
        self.applied_delta_revision
    }

    pub(super) fn materialize(self, root: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
        let tasks = match self.tasks {
            Some(tasks) => tasks,
            None => encode_task_registry(&task_registry_path(root), &TaskRegistry::default())?,
        };
        let watches = match self.watches {
            Some(watches) => watches,
            None => {
                encode_watch_registry(&watch_registry_path(root), &WatchRegistry::default(), None)?
            }
        };
        Ok((tasks, watches))
    }

    fn with_canonical_recovery(mut self, canonical_recovery: CanonicalRecovery) -> Self {
        self.canonical_recovery = canonical_recovery;
        self
    }

    fn with_applied_delta_revision(mut self, applied_delta_revision: u64) -> Self {
        self.applied_delta_revision = applied_delta_revision;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CanonicalRecovery {
    None,
    Watch,
    TaskThenWatch,
}

#[derive(Clone, Copy)]
pub(super) struct RevisionedRegistryPair<'a> {
    tasks: &'a [u8],
    watches: &'a [u8],
    applied_delta_revision: u64,
}

impl<'a> RevisionedRegistryPair<'a> {
    pub(super) const fn new(
        tasks: &'a [u8],
        watches: &'a [u8],
        applied_delta_revision: u64,
    ) -> Self {
        Self {
            tasks,
            watches,
            applied_delta_revision,
        }
    }

    const fn bytes(self) -> (&'a [u8], &'a [u8]) {
        (self.tasks, self.watches)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryArtifactDescriptor {
    bytes: u64,
    blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryCheckpointDescriptor {
    generation: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    applied_delta_revision: u64,
    tasks: RegistryArtifactDescriptor,
    watches: RegistryArtifactDescriptor,
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryCheckpointManifest {
    schema_version: u32,
    checkpoint: RegistryCheckpointDescriptor,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryCheckpointJournal {
    schema_version: u32,
    base: RegistryCheckpointDescriptor,
    target: RegistryCheckpointDescriptor,
}

pub(super) fn resolve_anchored(
    root: &Path,
    daemon: &CapabilityDir,
    tasks: Option<Vec<u8>>,
    watches: Option<Vec<u8>>,
) -> Result<RegistryRawPair> {
    let repair_journal = daemon
        .entry_metadata(OsStr::new(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME))
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to inspect task/watch registry repair journal",
                daemon
                    .display_path()
                    .join(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME),
                source,
            )
        })?;
    if repair_journal.is_some() {
        return Err(interrupted_repair(root));
    }
    resolve_with_reader(root, tasks, watches, |name, max_bytes| {
        let path = daemon.display_path().join(name);
        match daemon.read_file_limited(OsStr::new(name), max_bytes) {
            Ok(raw) => Ok(Some(raw)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DaemonCoreError::io(
                "failed to read task/watch checkpoint recovery file",
                path,
                source,
            )),
        }
    })
}

#[cfg(any(not(unix), test))]
pub(super) fn resolve_portable(
    root: &Path,
    tasks: Option<Vec<u8>>,
    watches: Option<Vec<u8>>,
) -> Result<RegistryRawPair> {
    let repair_journal = daemon_dir(root).join(CHECKPOINT_REPAIR_JOURNAL_FILE_NAME);
    match fs::symlink_metadata(&repair_journal) {
        Ok(_) => return Err(interrupted_repair(root)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to inspect task/watch registry repair journal",
                repair_journal,
                source,
            ));
        }
    }
    resolve_with_reader(root, tasks, watches, |name, max_bytes| {
        read_portable_recovery_file(&daemon_dir(root).join(name), max_bytes)
    })
}

fn interrupted_repair(root: &Path) -> DaemonCoreError {
    invalid_checkpoint(
        root,
        "an interrupted task/watch registry repair is pending; run \
         `Packet28 daemon storage repair --apply` to complete it",
    )
}

fn resolve_with_reader(
    root: &Path,
    tasks: Option<Vec<u8>>,
    watches: Option<Vec<u8>>,
    mut read: impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<RegistryRawPair> {
    let canonical = RegistryRawPair {
        tasks,
        watches,
        applied_delta_revision: 0,
        canonical_recovery: CanonicalRecovery::None,
    };
    let manifest_raw = read(CHECKPOINT_MANIFEST_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES)?;
    if let Some(manifest_raw) = manifest_raw {
        let manifest: RegistryCheckpointManifest =
            decode_checkpoint_json(root, CHECKPOINT_MANIFEST_FILE_NAME, &manifest_raw)?;
        validate_schema_version(root, manifest.schema_version, "checkpoint manifest")?;
        if manifest.checkpoint.generation.is_none() {
            return Err(invalid_checkpoint(
                root,
                "checkpoint manifest must name a generated checkpoint",
            ));
        }
        if pair_matches_descriptor(root, &canonical, &manifest.checkpoint)? {
            return Ok(
                canonical.with_applied_delta_revision(manifest.checkpoint.applied_delta_revision)
            );
        }

        let (journal, recovered) = read_journal(root, &mut read)?;
        if journal.base != manifest.checkpoint {
            return reject_unjournaled_canonical_state(root, &canonical);
        }
        validate_pair_descriptor(root, &recovered, &journal.base, "checkpoint journal base")?;
        let recovery = canonical_recovery(root, &canonical, &journal)?;
        return Ok(recovered
            .with_applied_delta_revision(journal.base.applied_delta_revision)
            .with_canonical_recovery(recovery));
    }

    let Some(journal_raw) = read(CHECKPOINT_JOURNAL_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES)?
    else {
        return Ok(canonical);
    };
    let journal: RegistryCheckpointJournal =
        decode_checkpoint_json(root, CHECKPOINT_JOURNAL_FILE_NAME, &journal_raw)?;
    validate_journal(root, &journal)?;
    let recovered = read_journal_pair(root, &mut read)?;
    validate_pair_descriptor(root, &recovered, &journal.base, "checkpoint journal base")?;
    let recovery = canonical_recovery(root, &canonical, &journal)?;
    Ok(recovered
        .with_applied_delta_revision(journal.base.applied_delta_revision)
        .with_canonical_recovery(recovery))
}

fn read_journal(
    root: &Path,
    read: &mut impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<(RegistryCheckpointJournal, RegistryRawPair)> {
    let raw =
        read(CHECKPOINT_JOURNAL_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES)?.ok_or_else(|| {
            invalid_checkpoint(
            root,
            "committed registry bytes disagree with the manifest and no recovery journal exists",
        )
        })?;
    let journal: RegistryCheckpointJournal =
        decode_checkpoint_json(root, CHECKPOINT_JOURNAL_FILE_NAME, &raw)?;
    validate_journal(root, &journal)?;
    let pair = read_journal_pair(root, read)?;
    Ok((journal, pair))
}

fn read_journal_pair(
    root: &Path,
    read: &mut impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<RegistryRawPair> {
    let tasks = read(CHECKPOINT_JOURNAL_TASK_FILE_NAME, MAX_TASK_REGISTRY_BYTES)?
        .ok_or_else(|| invalid_checkpoint(root, "checkpoint journal task image is missing"))?;
    let watches = read(CHECKPOINT_JOURNAL_WATCH_FILE_NAME, MAX_WATCH_REGISTRY_BYTES)?
        .ok_or_else(|| invalid_checkpoint(root, "checkpoint journal watch image is missing"))?;
    Ok(RegistryRawPair {
        tasks: Some(tasks),
        watches: Some(watches),
        applied_delta_revision: 0,
        canonical_recovery: CanonicalRecovery::None,
    })
}

fn validate_journal(root: &Path, journal: &RegistryCheckpointJournal) -> Result<()> {
    validate_schema_version(root, journal.schema_version, "checkpoint journal")?;
    if journal.target.generation.is_none() {
        return Err(invalid_checkpoint(
            root,
            "checkpoint journal target must name a generated checkpoint",
        ));
    }
    if journal.base == journal.target {
        return Err(invalid_checkpoint(
            root,
            "checkpoint journal base and target must differ",
        ));
    }
    Ok(())
}

fn validate_schema_version(root: &Path, actual: u32, artifact: &str) -> Result<()> {
    if actual == CHECKPOINT_SCHEMA_VERSION {
        return Ok(());
    }
    Err(invalid_checkpoint(
        root,
        format!(
            "{artifact} schema version {actual} is unsupported; expected {CHECKPOINT_SCHEMA_VERSION}"
        ),
    ))
}

fn pair_matches_descriptor(
    root: &Path,
    pair: &RegistryRawPair,
    descriptor: &RegistryCheckpointDescriptor,
) -> Result<bool> {
    let (Some(tasks), Some(watches)) = (&pair.tasks, &pair.watches) else {
        return Ok(false);
    };
    if !artifact_matches(tasks, &descriptor.tasks)
        || !artifact_matches(watches, &descriptor.watches)
    {
        return Ok(false);
    }
    validate_pair_descriptor(root, pair, descriptor, "checkpoint manifest")?;
    Ok(true)
}

fn validate_pair_descriptor(
    root: &Path,
    pair: &RegistryRawPair,
    descriptor: &RegistryCheckpointDescriptor,
    artifact: &str,
) -> Result<()> {
    let (Some(tasks), Some(watches)) = (&pair.tasks, &pair.watches) else {
        return Err(invalid_checkpoint(
            root,
            format!("{artifact} does not contain both registry images"),
        ));
    };
    if !artifact_matches(tasks, &descriptor.tasks)
        || !artifact_matches(watches, &descriptor.watches)
    {
        return Err(invalid_checkpoint(
            root,
            format!("{artifact} digest or byte length does not match its descriptor"),
        ));
    }
    let task_generation = registry_checkpoint_generation(
        &task_registry_path(root),
        tasks,
        AuthorityJsonProfile::TaskRegistry,
    )?;
    let watch_generation = registry_checkpoint_generation(
        &watch_registry_path(root),
        watches,
        AuthorityJsonProfile::WatchRegistry,
    )?;
    if task_generation != descriptor.generation || watch_generation != descriptor.generation {
        return Err(invalid_checkpoint(
            root,
            format!(
                "{artifact} generation does not match its registry images: descriptor={:?}, task={task_generation:?}, watch={watch_generation:?}",
                descriptor.generation
            ),
        ));
    }
    Ok(())
}

fn artifact_matches(bytes: &[u8], descriptor: &RegistryArtifactDescriptor) -> bool {
    descriptor.bytes == bytes.len() as u64
        && descriptor.blake3 == blake3::hash(bytes).to_hex().as_str()
}

fn canonical_recovery(
    root: &Path,
    canonical: &RegistryRawPair,
    journal: &RegistryCheckpointJournal,
) -> Result<CanonicalRecovery> {
    let task_is_base = optional_artifact_matches(
        canonical.tasks.as_deref(),
        &journal.base.tasks,
        journal.base.generation.is_none(),
    );
    let watch_is_base = optional_artifact_matches(
        canonical.watches.as_deref(),
        &journal.base.watches,
        journal.base.generation.is_none(),
    );
    let task_is_target = canonical
        .tasks
        .as_deref()
        .is_some_and(|raw| artifact_matches(raw, &journal.target.tasks));
    let watch_is_target = canonical
        .watches
        .as_deref()
        .is_some_and(|raw| artifact_matches(raw, &journal.target.watches));
    if task_is_base && watch_is_base {
        return Ok(CanonicalRecovery::None);
    }
    if task_is_base && watch_is_target {
        return Ok(CanonicalRecovery::Watch);
    }
    if task_is_target && watch_is_target {
        return Ok(CanonicalRecovery::TaskThenWatch);
    }

    reject_unjournaled_canonical_state(root, canonical)
}

fn reject_unjournaled_canonical_state<T>(root: &Path, canonical: &RegistryRawPair) -> Result<T> {
    let task_generation = canonical
        .tasks
        .as_deref()
        .map(|raw| {
            registry_checkpoint_generation(
                &task_registry_path(root),
                raw,
                AuthorityJsonProfile::TaskRegistry,
            )
        })
        .transpose()?
        .flatten();
    let watch_generation = canonical
        .watches
        .as_deref()
        .map(|raw| {
            registry_checkpoint_generation(
                &watch_registry_path(root),
                raw,
                AuthorityJsonProfile::WatchRegistry,
            )
        })
        .transpose()?
        .flatten();
    validate_registry_checkpoint_generations(root, task_generation, watch_generation)?;
    Err(invalid_checkpoint(
        root,
        "canonical registry bytes do not match a journaled checkpoint publication phase; \
         run `Packet28 daemon storage repair` to diagnose exact recovery",
    ))
}

fn optional_artifact_matches(
    bytes: Option<&[u8]>,
    descriptor: &RegistryArtifactDescriptor,
    legacy_absence_is_empty: bool,
) -> bool {
    match bytes {
        Some(bytes) => artifact_matches(bytes, descriptor),
        None => legacy_absence_is_empty,
    }
}

#[cfg(unix)]
pub(super) fn publish_anchored(
    root: &Path,
    daemon: &CapabilityDir,
    canonical_recovery: CanonicalRecovery,
    base: RevisionedRegistryPair<'_>,
    target: RevisionedRegistryPair<'_>,
    write_watch: impl Fn(&[u8]) -> Result<()>,
    write_task: impl Fn(&[u8]) -> Result<()>,
) -> Result<()> {
    let (base_tasks, base_watches) = base.bytes();
    let (target_tasks, target_watches) = target.bytes();
    let prepared = PreparedCheckpoint::new(root, base, target)?;
    heal_canonical_pair(canonical_recovery, base.bytes(), &write_watch, &write_task)?;
    write_anchored_recovery_file(
        daemon,
        CHECKPOINT_JOURNAL_TASK_FILE_NAME,
        base_tasks,
        CHECKPOINT_JOURNAL_TASK_WRITE_TEMP_PREFIX,
    )?;
    write_anchored_recovery_file(
        daemon,
        CHECKPOINT_JOURNAL_WATCH_FILE_NAME,
        base_watches,
        CHECKPOINT_JOURNAL_WATCH_WRITE_TEMP_PREFIX,
    )?;
    write_anchored_recovery_file(
        daemon,
        CHECKPOINT_JOURNAL_FILE_NAME,
        &prepared.journal,
        CHECKPOINT_JOURNAL_WRITE_TEMP_PREFIX,
    )?;
    super::maybe_exit_after_registry_checkpoint_phase("journal");
    write_watch(target_watches)?;
    super::maybe_exit_after_registry_checkpoint_phase("watch");
    write_task(target_tasks)?;
    super::maybe_exit_after_registry_checkpoint_phase("task");
    write_anchored_recovery_file(
        daemon,
        CHECKPOINT_MANIFEST_FILE_NAME,
        &prepared.manifest,
        CHECKPOINT_MANIFEST_WRITE_TEMP_PREFIX,
    )?;
    super::maybe_exit_after_registry_checkpoint_phase("manifest");
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn publish_portable(
    root: &Path,
    canonical_recovery: CanonicalRecovery,
    base: RevisionedRegistryPair<'_>,
    target: RevisionedRegistryPair<'_>,
    write_watch: impl Fn(&[u8]) -> Result<()>,
    write_task: impl Fn(&[u8]) -> Result<()>,
) -> Result<()> {
    let (base_tasks, base_watches) = base.bytes();
    let (target_tasks, target_watches) = target.bytes();
    let prepared = PreparedCheckpoint::new(root, base, target)?;
    heal_canonical_pair(canonical_recovery, base.bytes(), &write_watch, &write_task)?;
    write_atomically(
        &daemon_dir(root).join(CHECKPOINT_JOURNAL_TASK_FILE_NAME),
        base_tasks,
    )?;
    write_atomically(
        &daemon_dir(root).join(CHECKPOINT_JOURNAL_WATCH_FILE_NAME),
        base_watches,
    )?;
    write_atomically(
        &daemon_dir(root).join(CHECKPOINT_JOURNAL_FILE_NAME),
        &prepared.journal,
    )?;
    super::maybe_exit_after_registry_checkpoint_phase("journal");
    write_watch(target_watches)?;
    super::maybe_exit_after_registry_checkpoint_phase("watch");
    write_task(target_tasks)?;
    super::maybe_exit_after_registry_checkpoint_phase("task");
    write_atomically(
        &daemon_dir(root).join(CHECKPOINT_MANIFEST_FILE_NAME),
        &prepared.manifest,
    )?;
    super::maybe_exit_after_registry_checkpoint_phase("manifest");
    Ok(())
}

fn heal_canonical_pair(
    recovery: CanonicalRecovery,
    base: (&[u8], &[u8]),
    write_watch: &impl Fn(&[u8]) -> Result<()>,
    write_task: &impl Fn(&[u8]) -> Result<()>,
) -> Result<()> {
    let (base_tasks, base_watches) = base;
    match recovery {
        CanonicalRecovery::None => {}
        CanonicalRecovery::Watch => write_watch(base_watches)?,
        CanonicalRecovery::TaskThenWatch => {
            write_task(base_tasks)?;
            write_watch(base_watches)?;
        }
    }
    Ok(())
}

struct PreparedCheckpoint {
    journal: Vec<u8>,
    manifest: Vec<u8>,
}

impl PreparedCheckpoint {
    fn new(
        root: &Path,
        base: RevisionedRegistryPair<'_>,
        target: RevisionedRegistryPair<'_>,
    ) -> Result<Self> {
        let base = describe_pair(root, base.tasks, base.watches, base.applied_delta_revision)?;
        let target = describe_pair(
            root,
            target.tasks,
            target.watches,
            target.applied_delta_revision,
        )?;
        if target.generation.is_none() {
            return Err(invalid_checkpoint(
                root,
                "checkpoint publication target has no generation",
            ));
        }
        let journal = encode_checkpoint_json(
            root,
            CHECKPOINT_JOURNAL_FILE_NAME,
            &RegistryCheckpointJournal {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                base,
                target: target.clone(),
            },
        )?;
        let manifest = encode_checkpoint_json(
            root,
            CHECKPOINT_MANIFEST_FILE_NAME,
            &RegistryCheckpointManifest {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                checkpoint: target,
            },
        )?;
        Ok(Self { journal, manifest })
    }
}

fn describe_pair(
    root: &Path,
    tasks: &[u8],
    watches: &[u8],
    applied_delta_revision: u64,
) -> Result<RegistryCheckpointDescriptor> {
    let task_generation = registry_checkpoint_generation(
        &task_registry_path(root),
        tasks,
        AuthorityJsonProfile::TaskRegistry,
    )?;
    let watch_generation = registry_checkpoint_generation(
        &watch_registry_path(root),
        watches,
        AuthorityJsonProfile::WatchRegistry,
    )?;
    validate_registry_checkpoint_generations(root, task_generation, watch_generation)?;
    Ok(RegistryCheckpointDescriptor {
        generation: task_generation,
        applied_delta_revision,
        tasks: describe_artifact(tasks),
        watches: describe_artifact(watches),
    })
}

fn describe_artifact(bytes: &[u8]) -> RegistryArtifactDescriptor {
    RegistryArtifactDescriptor {
        bytes: bytes.len() as u64,
        blake3: blake3::hash(bytes).to_hex().to_string(),
    }
}

fn encode_checkpoint_json(
    root: &Path,
    name: &str,
    value: &impl serde::Serialize,
) -> Result<Vec<u8>> {
    let path = daemon_dir(root).join(name);
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| {
        DaemonCoreError::json(
            "failed to encode task/watch checkpoint metadata for",
            &path,
            source,
        )
    })?;
    if bytes.len() > MAX_CHECKPOINT_MANIFEST_BYTES {
        return Err(invalid_checkpoint(
            root,
            format!(
                "checkpoint metadata {name} is {} bytes; maximum is {MAX_CHECKPOINT_MANIFEST_BYTES}",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

fn decode_checkpoint_json<T: serde::de::DeserializeOwned>(
    root: &Path,
    name: &str,
    raw: &[u8],
) -> Result<T> {
    let path = daemon_dir(root).join(name);
    serde_json::from_slice(raw).map_err(|source| {
        DaemonCoreError::json(
            "failed to decode task/watch checkpoint metadata from",
            path,
            source,
        )
    })
}

#[cfg(unix)]
fn write_anchored_recovery_file(
    daemon: &CapabilityDir,
    name: &str,
    bytes: &[u8],
    temporary_prefix: &str,
) -> Result<()> {
    let path = daemon.display_path().join(name);
    daemon
        .write_json_atomically(OsStr::new(name), bytes, temporary_prefix)
        .map_err(|error| {
            DaemonCoreError::io(
                if error.renamed {
                    "failed to synchronize task/watch checkpoint recovery publication"
                } else {
                    "failed to publish task/watch checkpoint recovery file"
                },
                path,
                error.source,
            )
        })
}

#[cfg(any(not(unix), test))]
fn read_portable_recovery_file(path: &Path, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to inspect task/watch checkpoint recovery file",
                path,
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DaemonCoreError::io(
            "refused unsafe task/watch checkpoint recovery file",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checkpoint recovery entry is not a regular file",
            ),
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(invalid_checkpoint(
            path.parent().unwrap_or(path),
            format!(
                "checkpoint recovery file {} is {} bytes; maximum is {max_bytes}",
                path.display(),
                metadata.len()
            ),
        ));
    }
    let file = fs::File::open(path).map_err(|source| {
        DaemonCoreError::io(
            "failed to open task/watch checkpoint recovery file",
            path,
            source,
        )
    })?;
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to read task/watch checkpoint recovery file",
                path,
                source,
            )
        })?;
    if raw.len() > max_bytes {
        return Err(invalid_checkpoint(
            path.parent().unwrap_or(path),
            format!(
                "checkpoint recovery file {} exceeds {max_bytes} bytes",
                path.display()
            ),
        ));
    }
    Ok(Some(raw))
}

/// Durable intent for an offline canonical-registry repair.
///
/// While present, normal checkpoint resolution fails closed so a partially
/// applied repair can never be read as committed authority.
pub(super) const CHECKPOINT_REPAIR_JOURNAL_FILE_NAME: &str =
    ".task-watch-checkpoint-v1.repair.json";
pub(super) const CHECKPOINT_REPAIR_JOURNAL_WRITE_TEMP_PREFIX: &str =
    ".task-watch-checkpoint-v1.repair.packet28-write.";
pub(super) const MAX_CHECKPOINT_REPAIR_JOURNAL_BYTES: usize = MAX_CHECKPOINT_MANIFEST_BYTES;

/// Checkpoint metadata and images that authorize a canonical repair, in the
/// order they are archived.
pub(super) const CHECKPOINT_AUTHORITY_FILE_NAMES: [(&str, usize); 4] = [
    (CHECKPOINT_MANIFEST_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES),
    (CHECKPOINT_JOURNAL_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES),
    (CHECKPOINT_JOURNAL_TASK_FILE_NAME, MAX_TASK_REGISTRY_BYTES),
    (CHECKPOINT_JOURNAL_WATCH_FILE_NAME, MAX_WATCH_REGISTRY_BYTES),
];

/// Authority from which a canonical registry image can be restored exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryCheckpointAuthority {
    /// The manifest names a completed publication; only its digests survive.
    CommittedManifest,
    /// A publication stopped before its manifest; the manifest still names the
    /// journal base, whose images are retained and digest-authenticated.
    PrecommitJournalBase,
    /// The first publication stopped before any manifest; the journal base is
    /// the only committed state.
    UnpublishedJournalBase,
}

/// How a restored canonical image was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryRepairCandidateSource {
    /// Strict canonical re-encoding of the present bytes.
    CanonicalReencoding,
    /// The digest-authenticated checkpoint journal base image.
    JournalBaseImage,
}

pub(super) enum CanonicalFileAction {
    Keep,
    Restore {
        bytes: Vec<u8>,
        source: RegistryRepairCandidateSource,
        /// For journal-base restores: whether the replaced bytes were only a
        /// re-encoding of the authoritative image.
        reencoding_matched: Option<bool>,
    },
    Unrecoverable {
        reason: String,
    },
}

pub(super) struct CanonicalFilePlan {
    pub(super) name: &'static str,
    pub(super) current: Option<Vec<u8>>,
    pub(super) expected_bytes: u64,
    pub(super) expected_blake3: String,
    pub(super) action: CanonicalFileAction,
}

pub(super) struct CanonicalRepairPlan {
    pub(super) authority: RegistryCheckpointAuthority,
    pub(super) generation: Option<u64>,
    pub(super) applied_delta_revision: u64,
    pub(super) files: Vec<CanonicalFilePlan>,
}

pub(super) enum CanonicalRepairAssessment {
    /// Normal checkpoint resolution accepts the present canonical bytes.
    Clean,
    /// Checkpoint metadata itself cannot authorize any exact restore.
    NoAuthority {
        startup_error: String,
        reason: String,
    },
    Plan {
        startup_error: String,
        plan: CanonicalRepairPlan,
    },
}

/// Classifies a canonical registry pair that normal startup rejects.
///
/// Only exact authority is ever proposed: a committed manifest digest that a
/// strict canonical re-encoding reproduces, or a digest-authenticated journal
/// base image while that base is still the committed checkpoint. Metadata
/// decode failures are reported as absent authority rather than guessed past.
pub(super) fn assess_canonical_repair(
    root: &Path,
    mut read: impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<CanonicalRepairAssessment> {
    let tasks = read(TASK_REGISTRY_FILE_NAME, MAX_TASK_REGISTRY_BYTES)?;
    let watches = read(WATCH_REGISTRY_FILE_NAME, MAX_WATCH_REGISTRY_BYTES)?;
    let startup_error = match resolve_with_reader(root, tasks.clone(), watches.clone(), &mut read) {
        Ok(_) => return Ok(CanonicalRepairAssessment::Clean),
        Err(error) => error.to_string(),
    };
    let authority = match read_repair_authority(root, &mut read) {
        Ok(Some(authority)) => authority,
        Ok(None) => {
            return Ok(CanonicalRepairAssessment::NoAuthority {
                startup_error,
                reason: "no checkpoint manifest or journal exists to authenticate a restore"
                    .to_string(),
            });
        }
        Err(error) if is_metadata_error(&error) => {
            return Ok(CanonicalRepairAssessment::NoAuthority {
                startup_error,
                reason: format!("checkpoint authority is not usable: {error}"),
            });
        }
        Err(error) => return Err(error),
    };
    let descriptor = &authority.descriptor;
    let legacy_absence = authority.images.is_some() && descriptor.generation.is_none();
    let mut files = Vec::with_capacity(2);
    for (name, current, expected, profile, image) in [
        (
            TASK_REGISTRY_FILE_NAME,
            tasks,
            &descriptor.tasks,
            AuthorityJsonProfile::TaskRegistry,
            authority.images.as_ref().map(|(tasks, _)| tasks),
        ),
        (
            WATCH_REGISTRY_FILE_NAME,
            watches,
            &descriptor.watches,
            AuthorityJsonProfile::WatchRegistry,
            authority.images.as_ref().map(|(_, watches)| watches),
        ),
    ] {
        let action = if optional_artifact_matches(current.as_deref(), expected, legacy_absence) {
            CanonicalFileAction::Keep
        } else {
            let reencoded = current
                .as_deref()
                .map(|raw| canonical_reencoding(raw, profile))
                .transpose();
            match (image, reencoded) {
                (Some(image), reencoded) => CanonicalFileAction::Restore {
                    bytes: image.clone(),
                    source: RegistryRepairCandidateSource::JournalBaseImage,
                    reencoding_matched: Some(matches!(
                        reencoded,
                        Ok(Some(ref candidate)) if candidate == image
                    )),
                },
                (None, Ok(Some(candidate))) if artifact_matches(&candidate, expected) => {
                    CanonicalFileAction::Restore {
                        bytes: candidate,
                        source: RegistryRepairCandidateSource::CanonicalReencoding,
                        reencoding_matched: None,
                    }
                }
                (None, Ok(Some(candidate))) => CanonicalFileAction::Unrecoverable {
                    reason: format!(
                        "content differs from the committed checkpoint: canonical re-encoding is \
                         {} bytes with blake3 {}, not the committed digest; no retained image can \
                         restore it",
                        candidate.len(),
                        blake3::hash(&candidate).to_hex()
                    ),
                },
                (None, Ok(None)) => CanonicalFileAction::Unrecoverable {
                    reason: "committed registry image is missing and no retained image can \
                             restore it"
                        .to_string(),
                },
                (None, Err(reason)) => CanonicalFileAction::Unrecoverable {
                    reason: format!(
                        "bytes are not strict registry JSON ({reason}); no retained image can \
                         restore them"
                    ),
                },
            }
        };
        files.push(CanonicalFilePlan {
            name,
            current,
            expected_bytes: expected.bytes,
            expected_blake3: expected.blake3.clone(),
            action,
        });
    }
    Ok(CanonicalRepairAssessment::Plan {
        startup_error,
        plan: CanonicalRepairPlan {
            authority: authority.kind,
            generation: descriptor.generation,
            applied_delta_revision: descriptor.applied_delta_revision,
            files,
        },
    })
}

/// Requires the candidate pair to resolve through the unmodified startup
/// resolver as an exact, already-healed checkpoint: either both images equal
/// the committed manifest, or both equal the committed journal base. Returns
/// the decoded authority and its delta revision.
pub(super) fn verify_canonical_candidate(
    root: &Path,
    tasks: Option<Vec<u8>>,
    watches: Option<Vec<u8>>,
    read: impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<(TaskRegistry, WatchRegistry, u64)> {
    let resolved = resolve_with_reader(root, tasks, watches, read)?;
    if resolved.canonical_recovery() != CanonicalRecovery::None {
        return Err(invalid_checkpoint(
            root,
            "repair candidate still depends on an unfinished publication phase",
        ));
    }
    let applied_delta_revision = resolved.applied_delta_revision();
    let (tasks, watches) = resolved.materialize(root)?;
    let (tasks, watches, _, _) = decode_registry_checkpoint_pair(root, &tasks, &watches)?;
    Ok((tasks, watches, applied_delta_revision))
}

struct RepairAuthority {
    kind: RegistryCheckpointAuthority,
    descriptor: RegistryCheckpointDescriptor,
    images: Option<(Vec<u8>, Vec<u8>)>,
}

fn read_repair_authority(
    root: &Path,
    read: &mut impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
) -> Result<Option<RepairAuthority>> {
    let manifest = read(CHECKPOINT_MANIFEST_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES)?
        .map(|raw| {
            let manifest: RegistryCheckpointManifest =
                decode_checkpoint_json(root, CHECKPOINT_MANIFEST_FILE_NAME, &raw)?;
            validate_schema_version(root, manifest.schema_version, "checkpoint manifest")?;
            if manifest.checkpoint.generation.is_none() {
                return Err(invalid_checkpoint(
                    root,
                    "checkpoint manifest must name a generated checkpoint",
                ));
            }
            Ok(manifest)
        })
        .transpose()?;
    let journal = read(CHECKPOINT_JOURNAL_FILE_NAME, MAX_CHECKPOINT_MANIFEST_BYTES)?
        .map(|raw| {
            let journal: RegistryCheckpointJournal =
                decode_checkpoint_json(root, CHECKPOINT_JOURNAL_FILE_NAME, &raw)?;
            validate_journal(root, &journal)?;
            Ok::<_, DaemonCoreError>(journal)
        })
        .transpose()?;
    match (manifest, journal) {
        (Some(manifest), Some(journal)) if journal.base == manifest.checkpoint => {
            let images = journal_base_images(root, read, &journal.base)?;
            Ok(Some(RepairAuthority {
                kind: RegistryCheckpointAuthority::PrecommitJournalBase,
                descriptor: journal.base,
                images: Some(images),
            }))
        }
        (Some(manifest), _) => Ok(Some(RepairAuthority {
            kind: RegistryCheckpointAuthority::CommittedManifest,
            descriptor: manifest.checkpoint,
            images: None,
        })),
        (None, Some(journal)) => {
            let images = journal_base_images(root, read, &journal.base)?;
            Ok(Some(RepairAuthority {
                kind: RegistryCheckpointAuthority::UnpublishedJournalBase,
                descriptor: journal.base,
                images: Some(images),
            }))
        }
        (None, None) => Ok(None),
    }
}

fn journal_base_images(
    root: &Path,
    read: &mut impl FnMut(&str, usize) -> Result<Option<Vec<u8>>>,
    base: &RegistryCheckpointDescriptor,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let pair = read_journal_pair(root, read)?;
    validate_pair_descriptor(root, &pair, base, "checkpoint journal base")?;
    let (Some(tasks), Some(watches)) = (pair.tasks, pair.watches) else {
        return Err(invalid_checkpoint(
            root,
            "checkpoint journal base does not contain both registry images",
        ));
    };
    Ok((tasks, watches))
}

fn is_metadata_error(error: &DaemonCoreError) -> bool {
    matches!(
        error,
        DaemonCoreError::InvalidTaskWatchRegistry { .. }
            | DaemonCoreError::RegistryCheckpointGenerationMismatch { .. }
            | DaemonCoreError::AuthorityJsonLimitExceeded { .. }
            | DaemonCoreError::Json { .. }
    )
}

/// Strict, bounded, duplicate-key-rejecting decode followed by the writer's
/// canonical encoding. Unknown fields survive because the value is generic.
fn canonical_reencoding(
    raw: &[u8],
    profile: AuthorityJsonProfile,
) -> std::result::Result<Vec<u8>, String> {
    let value = decode_json_value_without_duplicate_keys(raw, profile)
        .map_err(|error| error.to_string())?;
    serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())
}

fn invalid_checkpoint(root: &Path, message: impl Into<String>) -> DaemonCoreError {
    DaemonCoreError::InvalidTaskWatchRegistry {
        root: root.to_path_buf(),
        message: message.into(),
    }
}
