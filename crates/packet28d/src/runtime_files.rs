use super::*;
#[cfg(unix)]
use crate::runtime_files_unix::{
    create_directory_at, link_file_at, open_directory_at, open_directory_path, open_file_at,
    open_lock_file_at, read_directory_names, remove_directory_tree_at, remove_file_at,
    remove_file_if_exists_at, remove_retained_directory_tree_at, rename_file_at,
};
#[cfg(unix)]
use fs2::FileExt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const INDEX_CLEAR_STATE_FILE: &str = "clear-state-v1";
/// Lease for `clear-state-v1`, separate from the retained directory lease
/// that builders hold for a whole generation and that engine detachment takes.
/// Request admission persists durable intent under this lease only, so it
/// never waits for a running build; cross-process writers stay serialized and
/// a crashed holder releases it with its descriptor.
const INDEX_CLEAR_STATE_LOCK_FILE: &str = ".index-clear-state.lock";
const MAX_INDEX_CLEAR_STATE_BYTES: u64 = 128;
pub(crate) const INDEX_CLEAR_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const INDEX_CLEAR_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
const MAPY_WRITER_LOCK_FILE: &str = ".mapy-v1.writer.lock";
const MAPY_GENERATION_HIGH_WATER_FILE: &str = ".mapy-v1.generation-high-water.json";
const MAPY_GENERATION_HIGH_WATER_SCHEMA_VERSION: u32 = 1;
const MAPY_MANIFEST_FILE: &str = "manifest.json";
const MAPY_PREVIOUS_MANIFEST_FILE: &str = "manifest.previous.json";
const MAX_MAPY_GENERATION_METADATA_BYTES: u64 = 64 * 1024;
const REGEX_WRITER_LOCK_FILE: &str = ".regex-v1.writer.lock";
static INDEX_CLEAR_STATE_WRITE_LOCK: Mutex<()> = Mutex::new(());
static INDEX_CLEAR_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedIndexClearPhase {
    Pending,
    PendingWithRebuild,
    Complete,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistedIndexClearState {
    revision: u64,
    phase: PersistedIndexClearPhase,
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedMapyGenerationHighWater {
    schema_version: u32,
    generation: u64,
}

#[cfg(unix)]
enum ClearDirectoryBinding {
    ResolveAtRemoval,
    Retained(Option<fs::File>),
}

pub(crate) fn default_index_manifest(root: &Path) -> DaemonIndexManifest {
    DaemonIndexManifest {
        schema_version: INTERACTIVE_INDEX_SCHEMA_VERSION,
        root: root.to_string_lossy().to_string(),
        generation: 0,
        include_tests: true,
        status: DaemonIndexState::Missing,
        dirty_paths: Vec::new(),
        queued_paths: Vec::new(),
        total_files: 0,
        indexed_files: 0,
        regex_generation: None,
        regex_status: None,
        regex_total_files: 0,
        regex_base_commit: None,
        regex_weight_table_version: None,
        regex_stale_reason: None,
        regex_indexed_files: 0,
        last_build_started_at_unix: None,
        last_build_completed_at_unix: None,
        last_error: None,
    }
}

pub(crate) fn load_index_manifest_file(root: &Path) -> DaemonIndexManifest {
    let path = index_manifest_path(root);
    let Ok(raw) = fs::read(&path) else {
        return default_index_manifest(root);
    };
    let Ok(mut manifest) = serde_json::from_slice::<DaemonIndexManifest>(&raw) else {
        return default_index_manifest(root);
    };
    if manifest.schema_version != INTERACTIVE_INDEX_SCHEMA_VERSION {
        return default_index_manifest(root);
    }
    manifest.root = root.to_string_lossy().to_string();
    manifest
}

pub(crate) fn save_index_manifest_file(root: &Path, manifest: &DaemonIndexManifest) -> Result<()> {
    fs::create_dir_all(index_dir(root))
        .with_context(|| format!("failed to create index dir '{}'", index_dir(root).display()))?;
    fs::write(
        index_manifest_path(root),
        serde_json::to_vec_pretty(manifest)?,
    )
    .with_context(|| {
        format!(
            "failed to write index manifest '{}'",
            index_manifest_path(root).display()
        )
    })?;
    Ok(())
}

pub(crate) fn load_index_runtime_files(
    root: &Path,
    mut manifest: DaemonIndexManifest,
) -> InteractiveIndexRuntime {
    match load_index_clear_state(root) {
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Complete,
            ..
        }) => {
            return InteractiveIndexRuntime {
                manifest: default_index_manifest(root),
                repo_runtime: None,
                regex_runtime: None,
            };
        }
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Pending | PersistedIndexClearPhase::PendingWithRebuild,
            ..
        }) => {
            manifest.status = DaemonIndexState::Queued;
            manifest.last_error = Some("persisted index clear is pending".to_string());
            manifest.regex_status = Some("clear_pending".to_string());
            manifest.regex_stale_reason = Some("persisted index clear is pending".to_string());
        }
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Superseded,
            ..
        })
        | None => {}
    }
    let repo_runtime = mapy_core::load_repo_index_runtime(root).unwrap_or_else(|error| {
        let mut runtime = mapy_core::RepoIndexRuntime::default();
        runtime.manifest.status = "corrupt".to_string();
        runtime.manifest.last_error = Some(error.to_string());
        runtime
    });
    let regex_runtime = packet28_search_core::load_runtime(root).unwrap_or_else(|error| {
        let mut runtime = packet28_search_core::RegexIndexRuntime::default();
        runtime.manifest.status = "corrupt".to_string();
        runtime.manifest.stale_reason = Some(error.to_string());
        runtime.manifest.last_error = Some(error.to_string());
        runtime
    });
    InteractiveIndexRuntime {
        manifest,
        repo_runtime: Some(repo_runtime),
        regex_runtime: Some(regex_runtime),
    }
}

pub(crate) fn clear_index_files(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        clear_index_engine_directory(
            root,
            "mapy-v1",
            MAPY_WRITER_LOCK_FILE,
            "repository index",
            ensure_mapy_generation_high_water,
            || Ok(()),
            true,
        )
    }
    #[cfg(not(unix))]
    {
        mapy_core::clear_repo_index_runtime(root)
            .map_err(|error| anyhow!("failed to clear repository index runtime: {error}"))?;
        let legacy_snapshot = index_snapshot_path(root);
        if legacy_snapshot.exists() {
            fs::remove_file(&legacy_snapshot)
                .with_context(|| format!("failed to remove '{}'", legacy_snapshot.display()))?;
        }
        Ok(())
    }
}

pub(crate) fn clear_regex_index_files(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        clear_index_engine_directory(
            root,
            "regex-v1",
            REGEX_WRITER_LOCK_FILE,
            "regex index",
            |_| Ok(ClearDirectoryBinding::ResolveAtRemoval),
            || Ok(()),
            false,
        )
    }
    #[cfg(not(unix))]
    {
        packet28_search_core::clear_index(root)
            .map_err(|error| anyhow!("failed to clear regex index runtime: {error}"))
    }
}

#[cfg(unix)]
fn clear_index_engine_directory(
    root: &Path,
    directory_name: &str,
    writer_lock_name: &str,
    description: &str,
    before_remove: impl FnOnce(&RetainedIndexDirectory) -> Result<ClearDirectoryBinding>,
    after_open: impl FnOnce() -> Result<()>,
    remove_legacy_snapshot: bool,
) -> Result<()> {
    let directory = match RetainedIndexDirectory::open(root, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to retain {} parent '{}'",
                    description,
                    index_dir(root).display()
                )
            });
        }
    };
    let _directory_guard = directory.lock_exclusive().with_context(|| {
        format!(
            "failed to lock {} parent '{}'",
            description,
            directory.path.display()
        )
    })?;
    let _writer_guard = RetainedEngineWriterLock::acquire(&directory, writer_lock_name)
        .with_context(|| {
            format!(
                "failed to lock {} writer in '{}'",
                description,
                directory.path.display()
            )
        })?;
    directory
        .validate_binding()
        .with_context(|| format!("{description} parent binding changed before clear"))?;
    let binding = before_remove(&directory)?;
    after_open()?;
    let removal = match binding {
        ClearDirectoryBinding::ResolveAtRemoval => directory.remove_directory_tree(directory_name),
        ClearDirectoryBinding::Retained(Some(expected)) => {
            directory.remove_retained_directory_tree(directory_name, &expected)
        }
        ClearDirectoryBinding::Retained(None) => directory.ensure_directory_absent(directory_name),
    };
    removal.with_context(|| {
        format!(
            "failed to remove {} directory beneath retained '{}'",
            description,
            directory.path.display()
        )
    })?;
    if remove_legacy_snapshot {
        let legacy_path = index_snapshot_path(root);
        let legacy_name = legacy_path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| anyhow!("legacy index snapshot name is not UTF-8"))?;
        directory
            .remove_file_if_exists(legacy_name)
            .with_context(|| {
                format!(
                    "failed to remove legacy index snapshot beneath retained '{}'",
                    directory.path.display()
                )
            })?;
    }
    directory
        .validate_binding()
        .with_context(|| format!("{description} parent binding changed during clear"))
}

#[cfg(unix)]
fn ensure_mapy_generation_high_water(
    directory: &RetainedIndexDirectory,
) -> Result<ClearDirectoryBinding> {
    ensure_mapy_generation_high_water_with_hook(directory, |_| Ok(()))
}

#[cfg(unix)]
fn ensure_mapy_generation_high_water_with_hook(
    directory: &RetainedIndexDirectory,
    before_publish: impl FnOnce(&Path) -> Result<()>,
) -> Result<ClearDirectoryBinding> {
    let (observed, retained_mapy) = discover_mapy_generation_high_water(directory)?;
    let path = directory.path.join(MAPY_GENERATION_HIGH_WATER_FILE);
    let existing = read_optional_file_at(
        &directory.directory,
        MAPY_GENERATION_HIGH_WATER_FILE,
        &path,
        MAX_MAPY_GENERATION_METADATA_BYTES,
    )?;
    if let Some(raw) = existing {
        validate_mapy_generation_high_water(&path, &raw, observed)?;
        return Ok(ClearDirectoryBinding::Retained(retained_mapy));
    }

    persist_mapy_generation_high_water_with_hook(directory, observed, before_publish)?;
    Ok(ClearDirectoryBinding::Retained(retained_mapy))
}

#[cfg(unix)]
fn validate_mapy_generation_high_water(path: &Path, raw: &[u8], observed: u64) -> Result<()> {
    let high_water =
        serde_json::from_slice::<PersistedMapyGenerationHighWater>(raw).with_context(|| {
            format!(
                "failed to decode repository generation high-water '{}'",
                path.display()
            )
        })?;
    if high_water.schema_version != MAPY_GENERATION_HIGH_WATER_SCHEMA_VERSION {
        return Err(anyhow!(
            "repository generation high-water '{}' has schema {}, expected {}",
            path.display(),
            high_water.schema_version,
            MAPY_GENERATION_HIGH_WATER_SCHEMA_VERSION
        ));
    }
    if high_water.generation < observed {
        return Err(anyhow!(
            "repository generation high-water {} trails observed generation {observed}",
            high_water.generation
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn discover_mapy_generation_high_water(
    directory: &RetainedIndexDirectory,
) -> Result<(u64, Option<fs::File>)> {
    let mapy_directory = match open_directory_at(&directory.directory, "mapy-v1") {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, None)),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to retain repository index directory '{}'",
                    directory.path.join("mapy-v1").display()
                )
            });
        }
    };
    let mapy_path = directory.path.join("mapy-v1");
    let mut generation = 0;
    for name in [MAPY_MANIFEST_FILE, MAPY_PREVIOUS_MANIFEST_FILE] {
        if let Some(raw) = read_optional_file_at(
            &mapy_directory,
            name,
            &mapy_path.join(name),
            MAX_MAPY_GENERATION_METADATA_BYTES,
        )? {
            if let Ok(manifest) =
                serde_json::from_slice::<mapy_core::RepoIndexRuntimeManifest>(&raw)
            {
                generation = generation.max(manifest.generation);
            }
        }
    }
    for name in read_directory_names(&mapy_directory).with_context(|| {
        format!(
            "failed to inspect repository index directory '{}'",
            mapy_path.display()
        )
    })? {
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(observed) = mapy_managed_artifact_generation(name)? {
            generation = generation.max(observed);
        }
    }
    let current = open_directory_at(&directory.directory, "mapy-v1").with_context(|| {
        format!(
            "repository index directory binding changed while fencing '{}'",
            mapy_path.display()
        )
    })?;
    ensure_same_object(&mapy_directory, &current).with_context(|| {
        format!(
            "repository index directory binding changed while fencing '{}'",
            mapy_path.display()
        )
    })?;
    directory
        .validate_binding()
        .context("repository index parent binding changed while fencing")?;
    Ok((generation, Some(mapy_directory)))
}

#[cfg(unix)]
fn mapy_managed_artifact_generation(name: &str) -> Result<Option<u64>> {
    let digits = if let Some(digits) = name
        .strip_prefix("generation-")
        .and_then(|name| name.strip_suffix(".json"))
    {
        Some(digits)
    } else if let Some(digits) = name
        .strip_prefix("base-")
        .and_then(|name| name.strip_suffix(".bin"))
    {
        Some(digits)
    } else if let Some(segment) = name
        .strip_prefix("segment-")
        .and_then(|name| name.strip_suffix(".bin"))
    {
        Some(segment.strip_suffix("-compacted").unwrap_or(segment))
    } else {
        None
    };
    let Some(digits) = digits else {
        return Ok(None);
    };
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(anyhow!(
            "repository generation artifact '{name}' has an invalid generation"
        ));
    }
    let generation = digits.parse::<u64>().with_context(|| {
        format!("repository generation artifact '{name}' has an invalid generation")
    })?;
    if generation == 0 {
        return Err(anyhow!(
            "repository generation artifact '{name}' uses reserved generation zero"
        ));
    }
    Ok(Some(generation))
}

#[cfg(unix)]
fn read_optional_file_at(
    directory: &fs::File,
    name: &str,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    let mut file = match open_file_at(
        directory,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open retained file '{}'", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect retained file '{}'", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(anyhow!(
            "retained file '{}' is not a bounded regular file",
            path.display()
        ));
    }
    let mut raw = Vec::new();
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut raw)
        .with_context(|| format!("failed to read retained file '{}'", path.display()))?;
    if raw.len() as u64 > max_bytes {
        return Err(anyhow!(
            "retained file '{}' exceeds {max_bytes} bytes",
            path.display()
        ));
    }
    let current = open_file_at(
        directory,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .with_context(|| format!("retained file binding changed for '{}'", path.display()))?;
    ensure_same_object(&file, &current)
        .with_context(|| format!("retained file binding changed for '{}'", path.display()))?;
    Ok(Some(raw))
}

#[cfg(unix)]
fn persist_mapy_generation_high_water_with_hook(
    directory: &RetainedIndexDirectory,
    generation: u64,
    before_publish: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&PersistedMapyGenerationHighWater {
        schema_version: MAPY_GENERATION_HIGH_WATER_SCHEMA_VERSION,
        generation,
    })
    .context("failed to encode repository generation high-water")?;
    let mut created = None;
    for _ in 0..128 {
        let nonce = INDEX_CLEAR_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".{MAPY_GENERATION_HIGH_WATER_FILE}.{}.mapy-{nonce:016x}.tmp",
            std::process::id()
        );
        match directory.create_file(&name) {
            Ok(file) => {
                created = Some((name, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create repository generation high-water temporary file '{}'",
                        directory.path.join(name).display()
                    )
                });
            }
        }
    }
    let (temporary_name, mut file) =
        created.ok_or_else(|| anyhow!("repository generation high-water namespace exhausted"))?;
    let result = (|| {
        file.write_all(&bytes).with_context(|| {
            format!(
                "failed to write repository generation high-water '{}'",
                directory.path.join(&temporary_name).display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "failed to sync repository generation high-water '{}'",
                directory.path.join(&temporary_name).display()
            )
        })?;
        before_publish(&directory.path.join(&temporary_name))?;
        match directory.link_file(&temporary_name, MAPY_GENERATION_HIGH_WATER_FILE) {
            Ok(()) => {
                let published = directory
                    .open_file(MAPY_GENERATION_HIGH_WATER_FILE)
                    .with_context(|| {
                        format!(
                            "failed to retain published repository generation high-water '{}'",
                            directory
                                .path
                                .join(MAPY_GENERATION_HIGH_WATER_FILE)
                                .display()
                        )
                    })?;
                ensure_same_object(&file, &published).with_context(|| {
                    format!(
                        "repository generation high-water publication binding changed for '{}'",
                        directory
                            .path
                            .join(MAPY_GENERATION_HIGH_WATER_FILE)
                            .display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let path = directory.path.join(MAPY_GENERATION_HIGH_WATER_FILE);
                let raw = read_optional_file_at(
                    &directory.directory,
                    MAPY_GENERATION_HIGH_WATER_FILE,
                    &path,
                    MAX_MAPY_GENERATION_METADATA_BYTES,
                )?
                .ok_or_else(|| {
                    anyhow!(
                        "concurrently published repository generation high-water '{}' disappeared",
                        path.display()
                    )
                })?;
                validate_mapy_generation_high_water(&path, &raw, generation)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to publish repository generation high-water '{}'",
                        directory
                            .path
                            .join(MAPY_GENERATION_HIGH_WATER_FILE)
                            .display()
                    )
                });
            }
        }
        directory.directory.sync_all().with_context(|| {
            format!(
                "failed to sync repository generation high-water parent '{}'",
                directory.path.display()
            )
        })?;
        directory.validate_binding().with_context(|| {
            format!(
                "repository generation high-water parent changed while publishing '{}'",
                directory.path.display()
            )
        })
    })();
    directory.remove_file_if_owned(&temporary_name, &file);
    result
}

#[cfg(all(test, unix))]
pub(crate) fn clear_index_files_with_binding_hook_for_test(
    root: &Path,
    after_open: impl FnOnce() -> Result<()>,
) -> Result<()> {
    clear_index_engine_directory(
        root,
        "mapy-v1",
        MAPY_WRITER_LOCK_FILE,
        "repository index",
        ensure_mapy_generation_high_water,
        after_open,
        true,
    )
}

#[cfg(all(test, unix))]
pub(crate) fn clear_index_files_with_generation_fence_hook_for_test(
    root: &Path,
    before_publish: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    clear_index_engine_directory(
        root,
        "mapy-v1",
        MAPY_WRITER_LOCK_FILE,
        "repository index",
        |directory| ensure_mapy_generation_high_water_with_hook(directory, before_publish),
        || Ok(()),
        true,
    )
}

pub(crate) fn index_clear_is_pending(root: &Path) -> bool {
    pending_index_clear(root).is_some()
}

#[cfg(test)]
pub(crate) fn index_clear_requires_rebuild(root: &Path) -> bool {
    pending_index_clear(root).is_some_and(|(_, rebuild)| rebuild)
}

pub(crate) fn pending_index_clear(root: &Path) -> Option<(u64, bool)> {
    match load_index_clear_state(root) {
        Some(PersistedIndexClearState {
            revision,
            phase: PersistedIndexClearPhase::Pending,
        }) => Some((revision.max(1), false)),
        Some(PersistedIndexClearState {
            revision,
            phase: PersistedIndexClearPhase::PendingWithRebuild,
        }) => Some((revision.max(1), true)),
        _ => None,
    }
}

pub(crate) fn index_clear_is_complete(root: &Path) -> bool {
    matches!(
        load_index_clear_state(root),
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Complete,
            ..
        })
    )
}

pub(crate) fn persist_index_clear_pending(root: &Path) -> Result<u64> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    let directory = RetainedIndexDirectory::open(root, true).with_context(|| {
        format!(
            "failed to retain index clear state parent '{}'",
            index_dir(root).display()
        )
    })?;
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    ensure_clear_state_attached(&_directory_guard, &directory, "before reading revision")?;
    let revision = load_index_clear_state_from_directory(&directory)
        .map(|state| state.revision)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("index clear revision exhausted"))?;
    persist_index_clear_state_unlocked(
        &directory,
        &_directory_guard,
        PersistedIndexClearState {
            revision,
            phase: PersistedIndexClearPhase::Pending,
        },
    )?;
    Ok(revision)
}

pub(crate) fn complete_index_clear_revision(root: &Path, expected_revision: u64) -> Result<bool> {
    complete_index_clear_with_sync(root, expected_revision, sync_retained_directory)
}

fn complete_index_clear_with_sync(
    root: &Path,
    expected_revision: u64,
    sync: impl FnOnce(&Path, &fs::File) -> Result<()>,
) -> Result<bool> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    let directory = RetainedIndexDirectory::open(root, true).with_context(|| {
        format!(
            "failed to retain index clear state parent '{}'",
            index_dir(root).display()
        )
    })?;
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    ensure_clear_state_attached(&_directory_guard, &directory, "before reading revision")?;
    let current = load_index_clear_state_from_directory(&directory);
    let revision = expected_revision.max(1);
    if current.is_some_and(|state| state.revision != 0 && state.revision != revision) {
        return Ok(false);
    }
    let phase = if current.is_some_and(|state| {
        matches!(
            state.phase,
            PersistedIndexClearPhase::PendingWithRebuild | PersistedIndexClearPhase::Superseded
        )
    }) {
        PersistedIndexClearPhase::Superseded
    } else {
        PersistedIndexClearPhase::Complete
    };
    persist_index_clear_state_with_sync_unlocked(
        &directory,
        &_directory_guard,
        PersistedIndexClearState { revision, phase },
        |_| Ok(()),
        sync,
    )?;
    Ok(true)
}

#[cfg(test)]
pub(crate) fn complete_index_clear_with_sync_for_test(
    root: &Path,
    sync: impl FnOnce(&Path, &fs::File) -> Result<()>,
) -> Result<()> {
    let revision = pending_index_clear(root)
        .map(|(revision, _)| revision)
        .unwrap_or(1);
    complete_index_clear_with_sync(root, revision, sync).map(|_| ())
}

#[cfg(test)]
pub(crate) fn complete_index_clear(root: &Path) -> Result<()> {
    let revision = pending_index_clear(root)
        .map(|(revision, _)| revision)
        .unwrap_or(1);
    complete_index_clear_revision(root, revision).map(|_| ())
}

pub(crate) fn record_index_work_after_clear(root: &Path) -> Result<()> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    let directory = match RetainedIndexDirectory::open(root, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to retain index clear state parent '{}'",
                    index_dir(root).display()
                )
            });
        }
    };
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    ensure_clear_state_attached(&_directory_guard, &directory, "before reading revision")?;
    let Some(state) = load_index_clear_state_from_directory(&directory) else {
        return Ok(());
    };
    let phase = match state.phase {
        PersistedIndexClearPhase::Pending => PersistedIndexClearPhase::PendingWithRebuild,
        PersistedIndexClearPhase::Complete => PersistedIndexClearPhase::Superseded,
        PersistedIndexClearPhase::PendingWithRebuild | PersistedIndexClearPhase::Superseded => {
            return Ok(());
        }
    };
    persist_index_clear_state_unlocked(
        &directory,
        &_directory_guard,
        PersistedIndexClearState {
            revision: state.revision.max(1),
            phase,
        },
    )
}

fn load_index_clear_state(root: &Path) -> Option<PersistedIndexClearState> {
    let _read_guard = match INDEX_CLEAR_STATE_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => return Some(corrupt_index_clear_state()),
    };
    let directory = match RetainedIndexDirectory::open(root, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(corrupt_index_clear_state()),
    };
    let directory_guard = match directory.lock_clear_state_shared() {
        Ok(guard) => guard,
        Err(_) => return Some(corrupt_index_clear_state()),
    };
    let state = load_index_clear_state_from_directory(&directory);
    if directory_guard.validate_attachment().is_err() {
        return Some(corrupt_index_clear_state());
    }
    state
}

/// Revalidates the retained ancestry and the held clear-state lock at a
/// transaction boundary (`stage` names it in the error).
fn ensure_clear_state_attached(
    lease: &RetainedClearStateLock<'_>,
    directory: &RetainedIndexDirectory,
    stage: &str,
) -> Result<()> {
    directory.validate_binding().with_context(|| {
        format!(
            "index clear state parent changed {stage} '{}'",
            directory.path.display()
        )
    })?;
    lease.validate_attachment().with_context(|| {
        format!(
            "index clear state lock detached {stage} '{}'",
            directory.path.display()
        )
    })
}

fn load_index_clear_state_from_directory(
    directory: &RetainedIndexDirectory,
) -> Option<PersistedIndexClearState> {
    let file = match directory.open_file(INDEX_CLEAR_STATE_FILE) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return directory
                .validate_binding()
                .is_err()
                .then(corrupt_index_clear_state);
        }
        Err(_) => return Some(corrupt_index_clear_state()),
    };
    load_index_clear_state_from_open_file(directory, file)
}

fn load_index_clear_state_from_open_file(
    directory: &RetainedIndexDirectory,
    file: fs::File,
) -> Option<PersistedIndexClearState> {
    load_index_clear_state_from_open_file_with_binding_hook(directory, file, |_| Ok(()))
}

fn load_index_clear_state_from_open_file_with_binding_hook(
    directory: &RetainedIndexDirectory,
    mut file: fs::File,
    after_file_identity: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Option<PersistedIndexClearState> {
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        _ => return Some(corrupt_index_clear_state()),
    };
    if metadata.len() > MAX_INDEX_CLEAR_STATE_BYTES {
        return Some(corrupt_index_clear_state());
    }
    let mut raw = String::new();
    if Read::by_ref(&mut file)
        .take(MAX_INDEX_CLEAR_STATE_BYTES + 1)
        .read_to_string(&mut raw)
        .is_err()
        || raw.len() as u64 > MAX_INDEX_CLEAR_STATE_BYTES
        || validate_index_clear_file_binding(directory, &file, after_file_identity).is_err()
    {
        return Some(corrupt_index_clear_state());
    };
    let mut fields = raw.split_whitespace();
    let phase = match fields.next() {
        Some("pending") => Some(PersistedIndexClearPhase::Pending),
        Some("pending-rebuild") => Some(PersistedIndexClearPhase::PendingWithRebuild),
        Some("complete") => Some(PersistedIndexClearPhase::Complete),
        Some("superseded") => Some(PersistedIndexClearPhase::Superseded),
        _ => None,
    };
    let revision = fields.next().and_then(|value| value.parse::<u64>().ok());
    match (phase, revision, fields.next()) {
        (Some(phase), Some(revision), None) if revision > 0 => {
            Some(PersistedIndexClearState { revision, phase })
        }
        _ => Some(corrupt_index_clear_state()),
    }
}

fn validate_index_clear_file_binding(
    directory: &RetainedIndexDirectory,
    file: &fs::File,
    after_file_identity: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let current = directory.open_file(INDEX_CLEAR_STATE_FILE)?;
        ensure_same_object(file, &current)?;
    }
    after_file_identity(&directory.path)?;
    #[cfg(unix)]
    {
        let current = directory.open_file(INDEX_CLEAR_STATE_FILE)?;
        ensure_same_object(file, &current)?;
    }
    directory.validate_binding()?;
    #[cfg(unix)]
    {
        let current = directory.open_file(INDEX_CLEAR_STATE_FILE)?;
        ensure_same_object(file, &current)?;
    }
    directory.validate_binding()
}

#[cfg(test)]
pub(crate) fn index_clear_is_pending_with_read_hook_for_test(
    root: &Path,
    after_open: impl FnOnce(&Path) -> Result<()>,
) -> bool {
    let _read_guard = match INDEX_CLEAR_STATE_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => return true,
    };
    let directory = match RetainedIndexDirectory::open(root, false) {
        Ok(directory) => directory,
        Err(_) => return true,
    };
    let directory_guard = match directory.lock_clear_state_shared() {
        Ok(guard) => guard,
        Err(_) => return true,
    };
    let file = match directory.open_file(INDEX_CLEAR_STATE_FILE) {
        Ok(file) => file,
        Err(_) => return true,
    };
    if after_open(&directory.path).is_err() {
        return true;
    }
    let state = load_index_clear_state_from_open_file(&directory, file);
    if directory_guard.validate_attachment().is_err() {
        return true;
    }
    matches!(
        state,
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Pending | PersistedIndexClearPhase::PendingWithRebuild,
            ..
        })
    )
}

#[cfg(all(test, unix))]
pub(crate) fn index_clear_is_pending_with_final_binding_hook_for_test(
    root: &Path,
    after_file_identity: impl FnOnce(&Path) -> std::io::Result<()>,
) -> bool {
    let _read_guard = match INDEX_CLEAR_STATE_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => return true,
    };
    let directory = match RetainedIndexDirectory::open(root, false) {
        Ok(directory) => directory,
        Err(_) => return true,
    };
    let directory_guard = match directory.lock_clear_state_shared() {
        Ok(guard) => guard,
        Err(_) => return true,
    };
    let file = match directory.open_file(INDEX_CLEAR_STATE_FILE) {
        Ok(file) => file,
        Err(_) => return true,
    };
    let state = load_index_clear_state_from_open_file_with_binding_hook(
        &directory,
        file,
        after_file_identity,
    );
    if directory_guard.validate_attachment().is_err() {
        return true;
    }
    matches!(
        state,
        Some(PersistedIndexClearState {
            phase: PersistedIndexClearPhase::Pending | PersistedIndexClearPhase::PendingWithRebuild,
            ..
        })
    )
}

fn corrupt_index_clear_state() -> PersistedIndexClearState {
    PersistedIndexClearState {
        revision: 0,
        phase: PersistedIndexClearPhase::Pending,
    }
}

fn persist_index_clear_state_unlocked(
    directory: &RetainedIndexDirectory,
    lease: &RetainedClearStateLock<'_>,
    state: PersistedIndexClearState,
) -> Result<()> {
    persist_index_clear_state_with_sync_unlocked(
        directory,
        lease,
        state,
        |_| Ok(()),
        sync_retained_directory,
    )
}

fn persist_index_clear_state_with_sync_unlocked(
    directory: &RetainedIndexDirectory,
    lease: &RetainedClearStateLock<'_>,
    state: PersistedIndexClearState,
    after_read: impl FnOnce(&Path) -> Result<()>,
    sync: impl FnOnce(&Path, &fs::File) -> Result<()>,
) -> Result<()> {
    persist_index_clear_state_with_temp_unlocked(directory, lease, state, None, after_read, sync)
}

#[cfg(test)]
fn persist_index_clear_state_with_nonce_unlocked(
    root: &Path,
    state: PersistedIndexClearState,
    nonce: u64,
    sync: impl FnOnce(&Path, &fs::File) -> Result<()>,
) -> Result<()> {
    let directory = RetainedIndexDirectory::open(root, true).with_context(|| {
        format!(
            "failed to retain index clear state parent '{}'",
            index_dir(root).display()
        )
    })?;
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    persist_index_clear_state_with_temp_unlocked(
        &directory,
        &_directory_guard,
        state,
        Some(index_clear_test_temporary_name(nonce)),
        |_| Ok(()),
        sync,
    )
}

fn persist_index_clear_state_with_temp_unlocked(
    directory: &RetainedIndexDirectory,
    lease: &RetainedClearStateLock<'_>,
    state: PersistedIndexClearState,
    fixed_temporary_name: Option<String>,
    after_read: impl FnOnce(&Path) -> Result<()>,
    sync: impl FnOnce(&Path, &fs::File) -> Result<()>,
) -> Result<()> {
    after_read(&directory.path)?;
    let (temporary_name, mut file) = if let Some(name) = fixed_temporary_name {
        let file = directory.create_file(&name).with_context(|| {
            format!(
                "failed to create unique index clear state '{}'",
                directory.path.join(&name).display()
            )
        })?;
        (name, file)
    } else {
        let mut created = None;
        for _ in 0..128 {
            let name = next_index_clear_temporary_name();
            match directory.create_file(&name) {
                Ok(file) => {
                    created = Some((name, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create unique index clear state '{}'",
                            directory.path.join(&name).display()
                        )
                    });
                }
            }
        }
        created.ok_or_else(|| anyhow!("index clear state temporary namespace exhausted"))?
    };
    let phase = match state.phase {
        PersistedIndexClearPhase::Pending => "pending",
        PersistedIndexClearPhase::PendingWithRebuild => "pending-rebuild",
        PersistedIndexClearPhase::Complete => "complete",
        PersistedIndexClearPhase::Superseded => "superseded",
    };
    let mut renamed = false;
    let result = (|| {
        file.write_all(format!("{phase} {}\n", state.revision).as_bytes())
            .with_context(|| {
                format!(
                    "failed to write index clear state '{}'",
                    directory.path.join(&temporary_name).display()
                )
            })?;
        file.sync_all().with_context(|| {
            format!(
                "failed to sync index clear state '{}'",
                directory.path.join(&temporary_name).display()
            )
        })?;
        ensure_clear_state_attached(lease, directory, "before publishing")?;
        directory
            .rename_file(&temporary_name, INDEX_CLEAR_STATE_FILE)
            .with_context(|| {
                format!(
                    "failed to publish index clear state '{}'",
                    directory.path.join(INDEX_CLEAR_STATE_FILE).display()
                )
            })?;
        renamed = true;
        sync(&directory.path, &directory.directory)?;
        // After the rename the marker is published; a detachment detected
        // here is reported as a failure but cannot be undone.
        ensure_clear_state_attached(lease, directory, "while publishing")
    })();
    if !renamed {
        directory.remove_file(&temporary_name);
    }
    result
}

#[cfg(test)]
pub(crate) fn persist_index_clear_pending_with_nonce_for_test(
    root: &Path,
    nonce: u64,
) -> Result<()> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    persist_index_clear_state_with_nonce_unlocked(
        root,
        PersistedIndexClearState {
            revision: 1,
            phase: PersistedIndexClearPhase::Pending,
        },
        nonce,
        sync_retained_directory,
    )
}

#[cfg(test)]
pub(crate) fn index_clear_temporary_path_for_test(root: &Path, nonce: u64) -> PathBuf {
    index_clear_temporary_path(root, nonce)
}

#[cfg(test)]
pub(crate) fn persist_index_clear_pending_with_parent_hook_for_test(
    root: &Path,
    after_retain: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    let directory = RetainedIndexDirectory::open(root, true).with_context(|| {
        format!(
            "failed to retain index clear state parent '{}'",
            index_dir(root).display()
        )
    })?;
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    persist_index_clear_state_with_temp_unlocked(
        &directory,
        &_directory_guard,
        PersistedIndexClearState {
            revision: 1,
            phase: PersistedIndexClearPhase::Pending,
        },
        Some(index_clear_test_temporary_name(u64::MAX - 1)),
        after_retain,
        sync_retained_directory,
    )
}

#[cfg(test)]
pub(crate) fn complete_index_clear_with_transition_hook_for_test(
    root: &Path,
    after_read: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let _write_guard = INDEX_CLEAR_STATE_WRITE_LOCK.lock().map_err(lock_err)?;
    let directory = RetainedIndexDirectory::open(root, true).with_context(|| {
        format!(
            "failed to retain index clear state parent '{}'",
            index_dir(root).display()
        )
    })?;
    let _directory_guard = directory.lock_clear_state_exclusive().with_context(|| {
        format!(
            "failed to lock index clear state in '{}'",
            directory.path.display()
        )
    })?;
    let current = load_index_clear_state_from_directory(&directory);
    let revision = current
        .map(|state| state.revision)
        .filter(|revision| *revision > 0)
        .unwrap_or(1);
    let phase = if current.is_some_and(|state| {
        matches!(
            state.phase,
            PersistedIndexClearPhase::PendingWithRebuild | PersistedIndexClearPhase::Superseded
        )
    }) {
        PersistedIndexClearPhase::Superseded
    } else {
        PersistedIndexClearPhase::Complete
    };
    persist_index_clear_state_with_sync_unlocked(
        &directory,
        &_directory_guard,
        PersistedIndexClearState { revision, phase },
        after_read,
        sync_retained_directory,
    )
}

#[cfg(all(test, unix))]
pub(crate) fn open_index_clear_parent_with_sync_for_test(
    root: &Path,
    sync_parent: impl FnMut(&str, &fs::File) -> std::io::Result<()>,
) -> Result<()> {
    RetainedIndexDirectory::open_with_parent_sync(root, true, sync_parent)
        .map(|_| ())
        .with_context(|| {
            format!(
                "failed to retain index clear state parent '{}'",
                index_dir(root).display()
            )
        })
}

#[cfg(test)]
fn index_clear_temporary_path(root: &Path, nonce: u64) -> PathBuf {
    index_dir(root).join(index_clear_test_temporary_name(nonce))
}

#[cfg(test)]
fn index_clear_test_temporary_name(nonce: u64) -> String {
    format!(
        ".{INDEX_CLEAR_STATE_FILE}.{}.test-{nonce}.tmp",
        std::process::id()
    )
}

fn next_index_clear_temporary_name() -> String {
    static PROCESS_NONCE: OnceLock<u128> = OnceLock::new();
    let process_nonce = PROCESS_NONCE.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            ^ u128::from(std::process::id())
    });
    let counter = INDEX_CLEAR_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".{INDEX_CLEAR_STATE_FILE}.{}.{process_nonce:032x}.{counter:016x}.tmp",
        std::process::id()
    )
}

fn sync_retained_directory(path: &Path, directory: &fs::File) -> Result<()> {
    directory
        .sync_all()
        .with_context(|| format!("failed to sync retained directory '{}'", path.display()))
}

#[cfg(unix)]
struct RetainedIndexDirectory {
    path: PathBuf,
    root_path: PathBuf,
    root: fs::File,
    packet28: fs::File,
    directory: fs::File,
}

#[cfg(unix)]
impl RetainedIndexDirectory {
    fn open(root: &Path, create: bool) -> std::io::Result<Self> {
        Self::open_with_parent_sync(root, create, |_, directory| directory.sync_all())
    }

    fn open_with_parent_sync(
        root: &Path,
        create: bool,
        mut sync_parent: impl FnMut(&str, &fs::File) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let root_path = fs::canonicalize(root)?;
        let root_handle = open_directory_path(&root_path)?;
        if create {
            let _created = create_directory_at(&root_handle, ".packet28")?;
        }
        let packet28 = open_directory_at(&root_handle, ".packet28")?;
        if create {
            sync_parent("root", &root_handle)?;
        }
        if create {
            let _created = create_directory_at(&packet28, "index")?;
        }
        let directory = open_directory_at(&packet28, "index")?;
        if create {
            sync_parent("packet28", &packet28)?;
        }
        let retained = Self {
            path: root_path.join(".packet28/index"),
            root_path,
            root: root_handle,
            packet28,
            directory,
        };
        retained.validate_binding()?;
        Ok(retained)
    }

    fn open_file(&self, name: &str) -> std::io::Result<fs::File> {
        open_file_at(
            &self.directory,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    }

    fn create_file(&self, name: &str) -> std::io::Result<fs::File> {
        open_file_at(
            &self.directory,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        )
    }

    fn rename_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        rename_file_at(&self.directory, source, destination)
    }

    fn link_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        link_file_at(&self.directory, source, destination)
    }

    fn remove_file(&self, name: &str) {
        remove_file_at(&self.directory, name);
    }

    fn remove_file_if_owned(&self, name: &str, expected: &fs::File) {
        let Ok(current) = self.open_file(name) else {
            return;
        };
        if ensure_same_object(expected, &current).is_ok() {
            self.remove_file(name);
        }
    }

    fn remove_file_if_exists(&self, name: &str) -> std::io::Result<()> {
        remove_file_if_exists_at(&self.directory, name)
    }

    fn remove_directory_tree(&self, name: &str) -> std::io::Result<()> {
        remove_directory_tree_at(&self.directory, name)
    }

    fn remove_retained_directory_tree(
        &self,
        name: &str,
        expected: &fs::File,
    ) -> std::io::Result<()> {
        remove_retained_directory_tree_at(&self.directory, name, expected)
    }

    fn ensure_directory_absent(&self, name: &str) -> std::io::Result<()> {
        match open_directory_at(&self.directory, name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("directory '{name}' appeared after its generation fence"),
            )),
            Err(error) => Err(error),
        }
    }

    fn open_lock_file(&self, name: &str) -> std::io::Result<fs::File> {
        open_lock_file_at(&self.directory, name)
    }

    fn lock_exclusive(&self) -> std::io::Result<RetainedDirectoryLock<'_>> {
        RetainedDirectoryLock::acquire(&self.directory, false)
    }

    fn lock_clear_state_shared(&self) -> std::io::Result<RetainedClearStateLock<'_>> {
        RetainedClearStateLock::acquire(self, true)
    }

    fn lock_clear_state_exclusive(&self) -> std::io::Result<RetainedClearStateLock<'_>> {
        RetainedClearStateLock::acquire(self, false)
    }

    fn validate_binding(&self) -> std::io::Result<()> {
        let current_root = open_directory_path(&self.root_path)?;
        ensure_same_object(&self.root, &current_root)?;
        let current_packet28 = open_directory_at(&current_root, ".packet28")?;
        ensure_same_object(&self.packet28, &current_packet28)?;
        let current_index = open_directory_at(&current_packet28, "index")?;
        ensure_same_object(&self.directory, &current_index)
    }
}

#[cfg(unix)]
struct RetainedDirectoryLock<'a> {
    directory: &'a fs::File,
}

#[cfg(unix)]
impl<'a> RetainedDirectoryLock<'a> {
    fn acquire(directory: &'a fs::File, shared: bool) -> std::io::Result<Self> {
        lock_file_with_timeout(directory, shared)?;
        Ok(Self { directory })
    }
}

#[cfg(unix)]
impl Drop for RetainedDirectoryLock<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.directory);
    }
}

/// Bounded lease on the durable clear state beneath a retained index
/// directory (`INDEX_CLEAR_STATE_LOCK_FILE`). Shared for readers, exclusive
/// for writers.
///
/// The lease is bound to the *current* child entry, not only to its
/// ancestors: after the descriptor is locked, the current
/// `.index-clear-state.lock` entry is reopened relative to the retained index
/// descriptor (no symlink following, no blocking open) and must be a regular,
/// singly linked file with the same device/inode as the locked descriptor.
/// A locked descriptor whose name was renamed or unlinked while this process
/// waited is therefore never treated as the lease; acquisition releases it and
/// contends on the current entry under the same absolute deadline, so a
/// replacement held by another owner still bounds this caller to a timeout
/// instead of a publication. The guard keeps the retained directory so the
/// clear-state transactions can revalidate attachment again before reading
/// the revision, before publishing the marker, and before reporting success
/// (or accepting a read). Detection after the publishing rename can only
/// report the failure; the published marker is not undone.
///
/// The lock file is opened if it exists and otherwise created exclusively.
/// Concurrent first-time creation of one name (and lookups racing it) can
/// report a transient `NotFound`/`AlreadyExists` on macOS, so acquisition
/// retries those outcomes within the same bounded deadline as the lock wait;
/// a real detachment keeps failing until the deadline and is still reported.
#[cfg(unix)]
struct RetainedClearStateLock<'a> {
    directory: &'a RetainedIndexDirectory,
    file: fs::File,
}

#[cfg(unix)]
enum ClearStateLockAttachment {
    Attached,
    Detached(&'static str),
}

#[cfg(unix)]
impl<'a> RetainedClearStateLock<'a> {
    fn acquire(directory: &'a RetainedIndexDirectory, shared: bool) -> std::io::Result<Self> {
        let deadline = std::time::Instant::now() + INDEX_CLEAR_LOCK_TIMEOUT;
        loop {
            match Self::try_acquire(directory, shared, deadline) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(INDEX_CLEAR_LOCK_RETRY_DELAY);
                }
                Ok(None) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {}ms waiting for retained index lock: \
                             the clear-state lock entry kept changing while waiting",
                            INDEX_CLEAR_LOCK_TIMEOUT.as_millis()
                        ),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
                    ) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(INDEX_CLEAR_LOCK_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Locks the current child entry. `Ok(None)` means the locked descriptor
    /// was no longer the current entry once the lock was granted (renamed or
    /// unlinked while waiting); the caller contends on the current entry again
    /// under the same deadline. Special leaves and ancestry changes fail.
    fn try_acquire(
        directory: &'a RetainedIndexDirectory,
        shared: bool,
        deadline: std::time::Instant,
    ) -> std::io::Result<Option<Self>> {
        let file = match directory.open_file(INDEX_CLEAR_STATE_LOCK_FILE) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                directory.create_file(INDEX_CLEAR_STATE_LOCK_FILE)?
            }
            Err(error) => return Err(error),
        };
        require_regular_clear_state_lock(&file.metadata()?)?;
        #[cfg(test)]
        clear_state_lock_wait_hook_for_test(&file);
        lock_file_until(&file, shared, deadline)?;
        let lock = Self { directory, file };
        if let Err(error) = directory.validate_binding() {
            drop(lock);
            return Err(error);
        }
        match lock.child_attachment() {
            Ok(ClearStateLockAttachment::Attached) => {}
            Ok(ClearStateLockAttachment::Detached(_)) => {
                drop(lock);
                return Ok(None);
            }
            Err(error) => {
                drop(lock);
                return Err(error);
            }
        }
        if let Err(error) = directory.validate_binding() {
            drop(lock);
            return Err(error);
        }
        Ok(Some(lock))
    }

    /// Compares the locked descriptor with the current child entry of the
    /// retained index directory. Errors are integrity failures (special
    /// leaf, extra hard links, symlink); `Detached` means the entry was
    /// renamed, unlinked, or replaced by another regular file.
    fn child_attachment(&self) -> std::io::Result<ClearStateLockAttachment> {
        use std::os::unix::fs::MetadataExt;

        let locked = self.file.metadata()?;
        if locked.nlink() > 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "retained index clear-state lock has multiple hard links",
            ));
        }
        let current = match self.directory.open_file(INDEX_CLEAR_STATE_LOCK_FILE) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ClearStateLockAttachment::Detached("was unlinked"));
            }
            Err(error) => return Err(error),
        };
        let current = current.metadata()?;
        require_regular_clear_state_lock(&current)?;
        if locked.nlink() == 0 {
            return Ok(ClearStateLockAttachment::Detached("was unlinked"));
        }
        if locked.dev() == current.dev() && locked.ino() == current.ino() {
            Ok(ClearStateLockAttachment::Attached)
        } else {
            Ok(ClearStateLockAttachment::Detached("was replaced"))
        }
    }

    /// Revalidates the retained ancestry and the locked child entry. Used at
    /// the clear-state transaction boundaries; any detachment is an error.
    fn validate_attachment(&self) -> std::io::Result<()> {
        self.directory.validate_binding()?;
        match self.child_attachment()? {
            ClearStateLockAttachment::Attached => self.directory.validate_binding(),
            ClearStateLockAttachment::Detached(reason) => Err(std::io::Error::other(format!(
                "retained index clear-state lock {reason} while held"
            ))),
        }
    }
}

#[cfg(unix)]
impl Drop for RetainedClearStateLock<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// The clear-state lock leaf must be a regular file with exactly one link:
/// directories and FIFOs are rejected without blocking (the open is
/// non-blocking and never follows symlinks) and a multiply linked leaf could
/// be reached under another name.
#[cfg(unix)]
fn require_regular_clear_state_lock(metadata: &fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "retained index clear-state lock is not a regular file",
        ));
    }
    if metadata.nlink() > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "retained index clear-state lock has multiple hard links",
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) type ClearStateLockWaitHook = Box<dyn FnMut(&fs::File)>;

#[cfg(all(test, unix))]
thread_local! {
    static CLEAR_STATE_LOCK_WAIT_HOOK: std::cell::RefCell<Option<ClearStateLockWaitHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only barrier invoked on the calling thread after the clear-state lock
/// descriptor is opened and before the bounded lock wait begins.
#[cfg(all(test, unix))]
pub(crate) fn set_clear_state_lock_wait_hook_for_test(hook: Option<ClearStateLockWaitHook>) {
    CLEAR_STATE_LOCK_WAIT_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(all(test, unix))]
fn clear_state_lock_wait_hook_for_test(file: &fs::File) {
    let hook = CLEAR_STATE_LOCK_WAIT_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(file);
        CLEAR_STATE_LOCK_WAIT_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

#[cfg(unix)]
struct RetainedEngineWriterLock {
    _file: fs::File,
}

#[cfg(unix)]
impl RetainedEngineWriterLock {
    fn acquire(directory: &RetainedIndexDirectory, name: &str) -> std::io::Result<Self> {
        let file = directory.open_lock_file(name)?;
        lock_file_with_timeout(&file, false)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock_file_with_timeout(file: &fs::File, shared: bool) -> std::io::Result<()> {
    lock_file_until(
        file,
        shared,
        std::time::Instant::now() + INDEX_CLEAR_LOCK_TIMEOUT,
    )
}

#[cfg(unix)]
fn lock_file_until(
    file: &fs::File,
    shared: bool,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    loop {
        let result = if shared {
            FileExt::try_lock_shared(file)
        } else {
            FileExt::try_lock_exclusive(file)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => return Err(error),
            Err(_) => {}
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out after {}ms waiting for retained index lock",
                    INDEX_CLEAR_LOCK_TIMEOUT.as_millis()
                ),
            ));
        }
        std::thread::sleep(INDEX_CLEAR_LOCK_RETRY_DELAY.min(deadline - now));
    }
}

#[cfg(unix)]
fn ensure_same_object(expected: &fs::File, actual: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let expected = expected.metadata()?;
    let actual = actual.metadata()?;
    if expected.dev() == actual.dev() && expected.ino() == actual.ino() {
        Ok(())
    } else {
        Err(std::io::Error::other("retained directory binding changed"))
    }
}

#[cfg(not(unix))]
struct RetainedIndexDirectory {
    path: PathBuf,
    directory: fs::File,
}

#[cfg(not(unix))]
impl RetainedIndexDirectory {
    fn open(root: &Path, create: bool) -> std::io::Result<Self> {
        let root = fs::canonicalize(root)?;
        let packet28 = root.join(".packet28");
        let path = packet28.join("index");
        if create {
            create_plain_directory(&packet28)?;
            create_plain_directory(&path)?;
        }
        reject_symlink_directory(&packet28)?;
        reject_symlink_directory(&path)?;
        let directory = fs::File::open(&path)?;
        Ok(Self { path, directory })
    }

    fn open_file(&self, name: &str) -> std::io::Result<fs::File> {
        fs::OpenOptions::new().read(true).open(self.path.join(name))
    }

    fn create_file(&self, name: &str) -> std::io::Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.join(name))
    }

    fn rename_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        fs::rename(self.path.join(source), self.path.join(destination))
    }

    fn remove_file(&self, name: &str) {
        let _ = fs::remove_file(self.path.join(name));
    }

    fn lock_exclusive(&self) -> std::io::Result<RetainedDirectoryLock<'_>> {
        Ok(RetainedDirectoryLock {
            _directory: &self.directory,
        })
    }

    fn lock_clear_state_shared(&self) -> std::io::Result<RetainedClearStateLock<'_>> {
        Ok(RetainedClearStateLock { directory: self })
    }

    fn lock_clear_state_exclusive(&self) -> std::io::Result<RetainedClearStateLock<'_>> {
        Ok(RetainedClearStateLock { directory: self })
    }

    fn validate_binding(&self) -> std::io::Result<()> {
        reject_symlink_directory(&self.path)
    }
}

#[cfg(not(unix))]
struct RetainedDirectoryLock<'a> {
    _directory: &'a fs::File,
}

#[cfg(not(unix))]
struct RetainedClearStateLock<'a> {
    directory: &'a RetainedIndexDirectory,
}

#[cfg(not(unix))]
impl RetainedClearStateLock<'_> {
    fn validate_attachment(&self) -> std::io::Result<()> {
        self.directory.validate_binding()
    }
}

#[cfg(not(unix))]
fn create_plain_directory(path: &Path) -> std::io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn reject_symlink_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "index state parent is not a directory",
        ))
    }
}
