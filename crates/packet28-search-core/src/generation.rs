//! Generation publication, recovery, retention, and writer serialization.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crate::error::{Result, SearchError};
use crate::layer::{
    artifact_digest, build_layer, discover_document_paths, index_document, load_layer,
    populate_layer_digests, scan_documents_with_progress, validate_layer_file_names, write_atomic,
    IndexedDocument,
};
use crate::model::{
    LayerFiles, LoadedIndex, LoadedOverlaySegment, OverlaySegmentRecord, OverlayState,
    RegexGenerationRecord, RegexIndexManifest, RegexIndexRuntime, MANIFEST_FILE_NAME,
    OVERLAY_COMPACTION_SEGMENTS, PREVIOUS_MANIFEST_FILE_NAME, REGEX_INDEX_SCHEMA_VERSION,
};
use crate::paths::{
    generation_record_path, manifest_path, normalize_changed_paths, overlay_state_path,
    previous_manifest_path, regex_index_dir,
};
use crate::publication::{
    acquire_writer_lock, capture_manifest_files, ensure_manifest_files_unchanged,
    fence_generation_before_clear, generation_record_fingerprint, load_generation_record,
    read_generation_high_water, read_optional_file, reserve_generation,
    restore_owned_manifest_files, save_generation_record, seal_generation_record,
    GenerationWriterLock, ManifestFilesSnapshot,
};
use crate::support::{ensure_valid_index, now_unix, ResultContext};
use crate::{state, weights::WEIGHT_TABLE_VERSION, workspace};

/// Loads and validates the index beneath `root`.
///
/// Stale or corrupt artifacts are represented as an unloaded runtime whose
/// manifest records the reason. The `Result` shape is retained for source
/// compatibility and future load failures that cannot be represented as index
/// state. If the current generation is corrupt, only the explicitly retained
/// previous manifest may be recovered; unreferenced artifacts are never
/// promoted.
///
/// # Errors
///
/// The current loader converts artifact validation failures into an unloaded
/// [`RegexIndexRuntime`]; it does not otherwise return an error.
pub fn load_runtime(root: &Path) -> Result<RegexIndexRuntime> {
    load_runtime_with_freshness(root, true)
}

pub(crate) fn load_runtime_for_query(root: &Path) -> Result<RegexIndexRuntime> {
    load_runtime_with_freshness(root, false)
}

fn load_runtime_with_freshness(root: &Path, verify_workspace: bool) -> Result<RegexIndexRuntime> {
    let manifest = match load_manifest_strict(root) {
        Ok(manifest) => manifest,
        Err(error) => return recover_previous_runtime(root, None, error, verify_workspace),
    };
    if manifest.schema_version == 0 {
        if read_optional_file(&previous_manifest_path(root))?.is_some() {
            return recover_previous_runtime(
                root,
                Some(manifest),
                SearchError::corrupt("current regex manifest is missing or has schema zero"),
                verify_workspace,
            );
        }
        return Ok(RegexIndexRuntime {
            manifest,
            loaded: None,
            publication_fingerprint: None,
        });
    }
    match load_runtime_from_manifest(root, manifest.clone(), verify_workspace) {
        Ok(runtime) => Ok(runtime),
        Err(error) => recover_previous_runtime(root, Some(manifest), error, verify_workspace),
    }
}

pub(crate) fn load_runtime_from_manifest(
    root: &Path,
    mut manifest: RegexIndexManifest,
    verify_workspace: bool,
) -> Result<RegexIndexRuntime> {
    if manifest.schema_version != REGEX_INDEX_SCHEMA_VERSION
        || manifest.weight_table_version != WEIGHT_TABLE_VERSION
    {
        let found_schema = manifest.schema_version;
        let found_weight = manifest.weight_table_version;
        mark_manifest_unloaded(
            &mut manifest,
            "stale",
            format!(
                "regex index weight/schema mismatch (found schema={found_schema}, \
                 weight={found_weight}, expected schema={REGEX_INDEX_SCHEMA_VERSION}, \
                 weight={WEIGHT_TABLE_VERSION})"
            ),
        );
        return Ok(RegexIndexRuntime {
            manifest,
            loaded: None,
            publication_fingerprint: None,
        });
    }
    if manifest.status != "ready" {
        return Ok(RegexIndexRuntime {
            manifest,
            loaded: None,
            publication_fingerprint: None,
        });
    }
    let record_exists = state::open_optional_state_file(
        &generation_record_path(root, manifest.generation),
        state::MAX_REGEX_METADATA_BYTES,
    )?
    .is_some();
    ensure_valid_index!(
        record_exists || manifest.publication_fingerprint.is_none(),
        "regex generation {} is missing its authenticated generation record",
        manifest.generation
    );
    let mut runtime = if record_exists {
        let record = load_generation_record(root, manifest.generation)?;
        load_recorded_generation(root, manifest, record)?
    } else {
        load_legacy_generation(root, manifest)?
    };
    let freshness_reason = verify_workspace
        .then(|| {
            runtime.loaded.as_ref().and_then(|loaded| {
                workspace::workspace_freshness_reason(
                    root,
                    &runtime.manifest,
                    &loaded.overlay_state.workspace_entries,
                )
            })
        })
        .flatten();
    if let Some(reason) = freshness_reason {
        mark_manifest_unloaded(&mut runtime.manifest, "stale", reason);
        runtime.loaded = None;
        runtime.publication_fingerprint = None;
    }
    Ok(runtime)
}
/// Rebuilds and manifest-last publishes every searchable file beneath `root`.
///
/// A repository-local exclusive writer lock covers the complete build and
/// publication. Artifact digests are validated whenever the generation is
/// loaded. After publication, best-effort pruning retains the current and
/// previous generation.
///
/// # Errors
///
/// Returns [`SearchError::Io`], [`SearchError::BinaryEncode`],
/// [`SearchError::BinaryDecode`], or [`SearchError::Json`] (possibly wrapped in
/// [`SearchError::Context`]) when discovery, index construction, validation, or
/// publication fails. [`SearchError::IndexNotReady`] rejects a Git workspace whose
/// HEAD or dirty bytes change during the build. [`SearchError::FailureProvenance`] reports the rarer case
/// where both the build and recording its failure fail.
pub fn rebuild_full_index(root: &Path, include_tests: bool) -> Result<RegexIndexRuntime> {
    rebuild_full_index_with_progress(root, include_tests, |_, _| {})
}
/// Rebuilds the full index and reports `(indexed_files, total_files)` progress.
///
/// The callback is invoked before scanning and after each discovered file.
///
/// # Errors
///
/// Returns the same typed failures as [`rebuild_full_index`].
pub fn rebuild_full_index_with_progress<F>(
    root: &Path,
    include_tests: bool,
    mut on_progress: F,
) -> Result<RegexIndexRuntime>
where
    F: FnMut(usize, usize),
{
    let _writer = acquire_writer_lock(root)?;
    let publication_snapshot = capture_manifest_files(root)?;
    let started = now_unix();
    let workspace_before = workspace::begin_full_build_workspace(root)?;
    let previous = load_published_runtime(root)
        .ok()
        .flatten()
        .or_else(|| load_runtime(root).ok().filter(RegexIndexRuntime::is_loaded))
        .map(|runtime| durable_manifest(&runtime.manifest));
    let generation = reserve_generation(root, &_writer)?;
    let mut manifest = RegexIndexManifest {
        schema_version: REGEX_INDEX_SCHEMA_VERSION,
        weight_table_version: WEIGHT_TABLE_VERSION,
        generation,
        include_tests,
        status: "ready".to_string(),
        last_build_started_at_unix: Some(started),
        ..RegexIndexManifest::default()
    };

    let docs = scan_documents_with_progress(root, &mut on_progress)?;
    let mut base_files = LayerFiles::base(generation);
    let base_layer = build_layer(root, &docs, &mut base_files)?;
    manifest.total_files = docs.len();
    manifest.indexed_files = docs.len();
    manifest.overlay_files = 0;
    manifest.overlay_segments = 0;
    let reported_paths = discover_document_paths(root)?
        .into_iter()
        .map(|path| {
            path.strip_prefix(root)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .map_err(|_| SearchError::corrupt("discovered path escaped the repository root"))
        })
        .collect::<Result<Vec<_>>>()?;
    let fingerprints = docs
        .iter()
        .map(|doc| (doc.path.clone(), doc.fingerprint.clone()))
        .collect::<BTreeMap<_, _>>();
    let overlay_state = workspace::authenticate_full_build_workspace(
        root,
        workspace_before.as_ref(),
        &reported_paths,
        &fingerprints,
        &mut manifest,
    )?;
    manifest.overlay_state_digest = Some(overlay_state_digest(&overlay_state)?);
    manifest.last_build_completed_at_unix = Some(now_unix());
    let mut record = RegexGenerationRecord {
        schema_version: REGEX_INDEX_SCHEMA_VERSION,
        generation,
        manifest: manifest.clone(),
        base: base_files.clone(),
        segments: Vec::new(),
        overlay_state: overlay_state.clone(),
    };
    let publication_fingerprint = seal_generation_record(&mut manifest, &mut record)?;
    validate_generation_record(&record)?;
    save_generation_record(root, &record)?;
    let runtime = RegexIndexRuntime {
        manifest: manifest.clone(),
        loaded: Some(Arc::new(LoadedIndex {
            base: Arc::new(base_layer),
            base_files,
            overlays: Vec::new(),
            overlay_state,
        })),
        publication_fingerprint: Some(publication_fingerprint),
    };
    publish_manifest(
        root,
        &_writer,
        &publication_snapshot,
        previous.as_ref(),
        &manifest,
    )?;
    let _ = prune_generation_artifacts(root, &_writer);
    Ok(runtime)
}

/// Appends one immutable overlay segment for `changed_paths`.
///
/// A missing current runtime or an empty change set intentionally triggers a
/// full rebuild to preserve the historical behavior. Each non-empty update
/// indexes only the supplied paths, retains the validated base `Arc`, and
/// publishes its generation record before atomically renaming the manifest.
/// Eight segments trigger compaction of live overlay documents; the base is
/// still retained. A repository-local writer lock serializes publishers, and a
/// stale `current` handle returns [`SearchError::ConcurrentWriter`].
///
/// # Errors
///
/// Returns [`SearchError::IndexNotLoaded`] when a supplied runtime has no
/// validated layers. Filesystem, codec, JSON, corruption, and failure-provenance
/// errors use the corresponding [`SearchError`] variants.
pub fn update_overlay_index(
    root: &Path,
    current: Option<&RegexIndexRuntime>,
    changed_paths: &[String],
) -> Result<RegexIndexRuntime> {
    let Some(current) = current else {
        return rebuild_full_index(root, true);
    };
    if changed_paths.is_empty() {
        return rebuild_full_index(root, true);
    }
    let loaded = current.loaded.as_ref().ok_or(SearchError::IndexNotLoaded)?;
    let caller_fingerprint =
        generation_record_fingerprint(&generation_record_from_loaded(&current.manifest, loaded))?;
    let normalized = normalize_changed_paths(root, changed_paths)?;
    let _writer = acquire_writer_lock(root)?;
    let publication_snapshot = capture_manifest_files(root)?;
    let published = load_published_runtime(root)?.ok_or(SearchError::ConcurrentWriter {
        expected: current.manifest.generation,
        actual: 0,
    })?;
    if published.manifest.generation != current.manifest.generation
        || published.publication_fingerprint != current.publication_fingerprint
        || current.publication_fingerprint.as_deref() != Some(caller_fingerprint.as_str())
    {
        return Err(SearchError::ConcurrentWriter {
            expected: current.manifest.generation,
            actual: published.manifest.generation,
        });
    }
    let workspace_before = workspace::authenticate_incremental_workspace(
        root,
        &published.manifest,
        &loaded.overlay_state.workspace_entries,
        &normalized,
    )?;
    let generation = reserve_generation(root, &_writer)?;
    let mut overlay_state = loaded.overlay_state.clone();
    let mut overlay_docs = Vec::<IndexedDocument>::new();
    let mut indexed_fingerprints = BTreeMap::new();
    for path in &normalized {
        overlay_state.shadowed_paths.insert(path.clone());
        let full_path = root.join(path);
        let indexed = if full_path.exists() {
            index_document(root, &full_path)?
        } else {
            None
        };
        if let Some(mut indexed) = indexed {
            indexed.doc_id = overlay_docs.len() as u32;
            overlay_state.deleted_paths.remove(path);
            overlay_state.owners.insert(path.clone(), generation);
            indexed_fingerprints.insert(path.clone(), indexed.fingerprint.clone());
            overlay_docs.push(indexed);
        } else {
            overlay_state.deleted_paths.insert(path.clone());
            overlay_state.owners.remove(path);
        }
    }
    overlay_docs.sort_by(|left, right| left.path.cmp(&right.path));
    for (idx, doc) in overlay_docs.iter_mut().enumerate() {
        doc.doc_id = idx as u32;
    }
    let started = now_unix();
    let mut overlays = loaded.overlays.clone();
    if !overlay_docs.is_empty() {
        let mut files = LayerFiles::overlay(generation, false);
        let layer = build_layer(root, &overlay_docs, &mut files)?;
        overlays.push(LoadedOverlaySegment {
            generation,
            layer: Arc::new(layer),
            files,
        });
    }
    if overlays.len() >= OVERLAY_COMPACTION_SEGMENTS {
        let mut compacted_docs = collect_live_overlay_documents(root, &mut overlay_state)?;
        for (idx, doc) in compacted_docs.iter_mut().enumerate() {
            doc.doc_id = idx as u32;
            overlay_state.owners.insert(doc.path.clone(), generation);
        }
        let mut files = LayerFiles::overlay(generation, true);
        let layer = build_layer(root, &compacted_docs, &mut files)?;
        overlays = vec![LoadedOverlaySegment {
            generation,
            layer: Arc::new(layer),
            files,
        }];
    }
    if let Some(entries) = workspace::authenticate_indexed_workspace_after(
        root,
        &normalized,
        workspace_before,
        &indexed_fingerprints,
    )? {
        overlay_state.workspace_entries = entries;
    }
    let loaded_index = LoadedIndex {
        base: Arc::clone(&loaded.base),
        base_files: loaded.base_files.clone(),
        overlays,
        overlay_state,
    };
    validate_loaded_overlay_state(&loaded_index)?;
    let mut manifest = durable_manifest(&published.manifest);
    manifest.generation = generation;
    manifest.status = "ready".to_string();
    manifest.last_build_started_at_unix = Some(started);
    manifest.total_files = loaded_index.all_indexed_paths(None).len();
    manifest.overlay_files = loaded_index.overlay_state.owners.len();
    manifest.overlay_segments = loaded_index.overlays.len();
    manifest.overlay_state_digest = Some(overlay_state_digest(&loaded_index.overlay_state)?);
    manifest.stale_reason = None;
    manifest.last_error = None;
    manifest.last_build_completed_at_unix = Some(now_unix());
    let mut record = generation_record_from_loaded(&manifest, &loaded_index);
    let publication_fingerprint = seal_generation_record(&mut manifest, &mut record)?;
    validate_generation_record(&record)?;
    save_generation_record(root, &record)?;
    publish_manifest(
        root,
        &_writer,
        &publication_snapshot,
        Some(&published.manifest),
        &manifest,
    )?;
    let _ = prune_generation_artifacts(root, &_writer);

    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(loaded_index)),
        publication_fingerprint: Some(publication_fingerprint),
    })
}
/// Removes the persisted regex index beneath `root`.
///
/// # Errors
///
/// Returns [`SearchError::Context`] with a nested [`SearchError::Io`] when the
/// index directory cannot be removed.
pub fn clear_index(root: &Path) -> Result<()> {
    let writer = acquire_writer_lock(root)?;
    fence_generation_before_clear(root, &writer)?;
    let path = regex_index_dir(root);
    state::index_parent_state_dir(root, false)?
        .remove_tree_if_exists(crate::model::REGEX_DIR_NAME)
        .with_context(|| format!("failed to remove regex index dir '{}'", path.display()))?;
    Ok(())
}
pub(crate) fn mark_manifest_unloaded(
    manifest: &mut RegexIndexManifest,
    status: &str,
    reason: String,
) {
    manifest.status = status.to_string();
    manifest.stale_reason = Some(reason.clone());
    manifest.last_error = Some(reason);
}
pub(crate) fn load_manifest_strict(root: &Path) -> Result<RegexIndexManifest> {
    let path = manifest_path(root);
    let Some(raw) = state::read_optional_state_file(&path, state::MAX_REGEX_METADATA_BYTES)
        .with_context(|| format!("failed to read regex manifest '{}'", path.display()))?
    else {
        return Ok(RegexIndexManifest::default());
    };
    decode_manifest_file(&path, &raw)
}

pub(crate) fn load_manifest_file(path: &Path) -> Result<RegexIndexManifest> {
    let raw = state::read_state_file(path, state::MAX_REGEX_METADATA_BYTES)
        .with_context(|| format!("failed to read regex manifest '{}'", path.display()))?;
    decode_manifest_file(path, &raw)
}

fn decode_manifest_file(path: &Path, raw: &[u8]) -> Result<RegexIndexManifest> {
    serde_json::from_slice(raw)
        .with_context(|| format!("failed to decode regex manifest '{}'", path.display()))
}

pub(crate) fn save_manifest(root: &Path, manifest: &RegexIndexManifest) -> Result<()> {
    write_atomic(manifest_path(root), &serde_json::to_vec_pretty(manifest)?)
}

pub(crate) fn publish_manifest(
    root: &Path,
    _writer: &GenerationWriterLock,
    expected: &ManifestFilesSnapshot,
    previous: Option<&RegexIndexManifest>,
    current: &RegexIndexManifest,
) -> Result<String> {
    ensure_manifest_files_unchanged(root, expected)?;
    let high_water = read_generation_high_water(root)?;
    ensure_valid_index!(
        high_water == Some(current.generation),
        "regex generation {} is not the reserved high-water generation {:?}",
        current.generation,
        high_water
    );
    let candidate = load_generation_for_manifest(root, current.clone())?;
    let fingerprint = candidate.publication_fingerprint.ok_or_else(|| {
        SearchError::corrupt(format!(
            "regex generation {} has no publication fingerprint",
            current.generation
        ))
    })?;
    ensure_valid_index!(
        current.publication_fingerprint.as_deref() == Some(fingerprint.as_str()),
        "regex generation {} manifest is not bound to its generation record",
        current.generation
    );
    let previous_bytes = previous
        .filter(|manifest| manifest.generation > 0)
        .map(durable_manifest)
        .map(|manifest| {
            load_generation_for_manifest(root, manifest.clone())?;
            serde_json::to_vec_pretty(&manifest).map_err(SearchError::from)
        })
        .transpose()?;
    let published_snapshot = ManifestFilesSnapshot {
        current: Some(serde_json::to_vec_pretty(current)?),
        previous: previous_bytes.clone().or_else(|| expected.previous.clone()),
    };
    ensure_manifest_files_unchanged(root, expected)?;
    let publication = (|| {
        if let Some(bytes) = previous_bytes.as_ref() {
            write_atomic(previous_manifest_path(root), bytes)?;
        }
        save_manifest(root, current)
    })();
    match publication {
        Ok(()) => Ok(fingerprint),
        Err(build) => match restore_owned_manifest_files(root, &published_snapshot, expected) {
            Ok(()) => Err(build),
            Err(persistence) => Err(SearchError::FailureProvenance {
                build: Box::new(build),
                persistence: Box::new(
                    persistence.context("failed to restore pre-publication regex manifests"),
                ),
            }),
        },
    }
}

pub(crate) fn durable_manifest(manifest: &RegexIndexManifest) -> RegexIndexManifest {
    let mut durable = manifest.clone();
    if durable.status == "ready" {
        durable.stale_reason = None;
        durable.last_error = None;
    }
    durable
}

pub(crate) fn load_overlay_state(root: &Path) -> Result<OverlayState> {
    let path = overlay_state_path(root);
    let raw =
        state::read_state_file(&path, state::MAX_REGEX_METADATA_BYTES).with_context(|| {
            format!(
                "failed to read legacy regex overlay state '{}'",
                path.display()
            )
        })?;
    serde_json::from_slice(&raw).with_context(|| {
        format!(
            "failed to decode legacy regex overlay state '{}'",
            path.display()
        )
    })
}

pub(crate) fn generation_record_from_loaded(
    manifest: &RegexIndexManifest,
    loaded: &LoadedIndex,
) -> RegexGenerationRecord {
    RegexGenerationRecord {
        schema_version: REGEX_INDEX_SCHEMA_VERSION,
        generation: manifest.generation,
        manifest: durable_manifest(manifest),
        base: loaded.base_files.clone(),
        segments: loaded
            .overlays
            .iter()
            .map(|segment| OverlaySegmentRecord {
                generation: segment.generation,
                files: segment.files.clone(),
            })
            .collect(),
        overlay_state: loaded.overlay_state.clone(),
    }
}

pub(crate) fn load_published_runtime(root: &Path) -> Result<Option<RegexIndexRuntime>> {
    let manifest = load_manifest_strict(root)?;
    if manifest.schema_version == 0 {
        let path = previous_manifest_path(root);
        if read_optional_file(&path)?.is_none() {
            return Ok(None);
        }
        return load_generation_for_manifest(root, load_manifest_file(&path)?).map(Some);
    }
    load_generation_for_manifest(root, manifest).map(Some)
}

fn load_generation_for_manifest(
    root: &Path,
    manifest: RegexIndexManifest,
) -> Result<RegexIndexRuntime> {
    ensure_valid_index!(
        manifest.schema_version == REGEX_INDEX_SCHEMA_VERSION
            && manifest.weight_table_version == WEIGHT_TABLE_VERSION
            && manifest.generation > 0
            && manifest.status == "ready",
        "regex generation {} cannot authenticate a non-ready or incompatible manifest",
        manifest.generation
    );
    let record_exists = state::open_optional_state_file(
        &generation_record_path(root, manifest.generation),
        state::MAX_REGEX_METADATA_BYTES,
    )?
    .is_some();
    ensure_valid_index!(
        record_exists || manifest.publication_fingerprint.is_none(),
        "regex generation {} is missing its authenticated generation record",
        manifest.generation
    );
    if record_exists {
        let record = load_generation_record(root, manifest.generation)?;
        load_recorded_generation(root, manifest, record)
    } else {
        load_legacy_generation(root, manifest)
    }
}

fn generation_record_from_runtime(runtime: &RegexIndexRuntime) -> Result<RegexGenerationRecord> {
    let loaded = runtime.loaded.as_ref().ok_or_else(|| {
        SearchError::corrupt(format!(
            "regex generation {} has no validated layers",
            runtime.manifest.generation
        ))
    })?;
    let record = generation_record_from_loaded(&runtime.manifest, loaded);
    validate_generation_record(&record)?;
    let fingerprint = generation_record_fingerprint(&record)?;
    ensure_valid_index!(
        runtime.publication_fingerprint.as_deref() == Some(fingerprint.as_str()),
        "regex generation {} publication fingerprint changed",
        runtime.manifest.generation
    );
    Ok(record)
}

pub(crate) fn prune_generation_artifacts(
    root: &Path,
    _writer: &GenerationWriterLock,
) -> Result<()> {
    let snapshot = capture_manifest_files(root)?;
    let current_manifest = load_manifest_strict(root)?;
    if current_manifest.schema_version == 0 {
        return Ok(());
    }
    let current_runtime = load_generation_for_manifest(root, current_manifest)?;
    let current = generation_record_from_runtime(&current_runtime)?;
    let previous = match load_manifest_file(&previous_manifest_path(root)) {
        Ok(manifest) => {
            let runtime = load_generation_for_manifest(root, manifest)?;
            Some(generation_record_from_runtime(&runtime)?)
        }
        Err(SearchError::Context { source, .. })
            if matches!(
                source.as_ref(),
                SearchError::Io { source }
                    if source.kind() == std::io::ErrorKind::NotFound
            ) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    ensure_manifest_files_unchanged(root, &snapshot)?;
    let mut retained = BTreeSet::from([
        MANIFEST_FILE_NAME.to_string(),
        PREVIOUS_MANIFEST_FILE_NAME.to_string(),
        format!("generation-{:020}.json", current.generation),
    ]);
    retain_layer_files(&mut retained, &current.base);
    for segment in &current.segments {
        retain_layer_files(&mut retained, &segment.files);
    }
    if let Some(previous) = previous.as_ref() {
        retained.insert(format!("generation-{:020}.json", previous.generation));
        retain_layer_files(&mut retained, &previous.base);
        for segment in &previous.segments {
            retain_layer_files(&mut retained, &segment.files);
        }
    }
    let directory = regex_index_dir(root);
    let state_dir = state::regex_state_dir(root, false)?;
    for entry in state_dir.names()? {
        let Some(name) = entry.to_str() else {
            continue;
        };
        if is_managed_generation_artifact(name) && !retained.contains(name) {
            state_dir.remove_file_if_exists(name).with_context(|| {
                format!(
                    "failed to prune regex index artifact '{}'",
                    directory.join(name).display()
                )
            })?;
        }
    }
    Ok(())
}

pub(crate) fn retain_layer_files(retained: &mut BTreeSet<String>, files: &LayerFiles) {
    retained.insert(files.lookup.clone());
    retained.insert(files.postings.clone());
    retained.insert(files.docs.clone());
}

pub(crate) fn is_managed_generation_artifact(name: &str) -> bool {
    (name.starts_with("generation-") && name.ends_with(".json"))
        || (name.starts_with("base-") && name.ends_with(".dat"))
        || (name.starts_with("overlay-") && name.ends_with(".dat"))
        || (name.starts_with('.') && name.ends_with(".tmp"))
}

pub(crate) fn validate_generation_record(record: &RegexGenerationRecord) -> Result<()> {
    ensure_valid_index!(
        record.schema_version == REGEX_INDEX_SCHEMA_VERSION
            && record.generation > 0
            && record.manifest.schema_version == REGEX_INDEX_SCHEMA_VERSION
            && record.manifest.weight_table_version == WEIGHT_TABLE_VERSION
            && record.manifest.generation == record.generation
            && record.manifest.status == "ready",
        "regex generation {} has inconsistent schema, weight, manifest, or status",
        record.generation
    );
    ensure_valid_index!(
        record.manifest.overlay_segments == record.segments.len(),
        "regex generation {} declares {} overlay segments but references {}",
        record.generation,
        record.manifest.overlay_segments,
        record.segments.len()
    );
    if let Some(expected) = record.manifest.publication_fingerprint.as_deref() {
        let actual = generation_record_fingerprint(record)?;
        ensure_valid_index!(
            actual == expected,
            "regex generation {} failed publication fingerprint validation (expected {expected}, found {actual})",
            record.generation
        );
    }
    validate_layer_file_names(&record.base)?;
    ensure_valid_index!(
        record.base.has_digests(),
        "regex generation {} base is missing artifact digests",
        record.generation
    );
    let mut artifact_names = BTreeSet::from([
        record.base.lookup.as_str(),
        record.base.postings.as_str(),
        record.base.docs.as_str(),
    ]);
    let mut segment_generations = BTreeSet::new();
    let mut previous_generation = None;
    for segment in &record.segments {
        validate_layer_file_names(&segment.files)?;
        ensure_valid_index!(
            segment.files.has_digests(),
            "regex generation {} overlay segment {} is missing artifact digests",
            record.generation,
            segment.generation
        );
        ensure_valid_index!(
            segment.generation > 0
                && segment.generation <= record.generation
                && previous_generation.is_none_or(|previous| segment.generation > previous)
                && segment_generations.insert(segment.generation),
            "regex generation {} has duplicate, non-increasing, or future overlay generation {}",
            record.generation,
            segment.generation
        );
        for name in [
            segment.files.lookup.as_str(),
            segment.files.postings.as_str(),
            segment.files.docs.as_str(),
        ] {
            ensure_valid_index!(
                artifact_names.insert(name),
                "regex generation {} references duplicate artifact '{name}'",
                record.generation
            );
        }
        previous_generation = Some(segment.generation);
    }
    ensure_valid_index!(
        record
            .overlay_state
            .owners
            .keys()
            .all(|path| record.overlay_state.shadowed_paths.contains(path)),
        "regex generation {} has an overlay owner that is not shadowed",
        record.generation
    );
    ensure_valid_index!(
        record.overlay_state.deleted_paths.iter().all(|path| record
            .overlay_state
            .shadowed_paths
            .contains(path)
            && !record.overlay_state.owners.contains_key(path)),
        "regex generation {} has inconsistent tombstones",
        record.generation
    );
    ensure_valid_index!(
        record
            .overlay_state
            .owners
            .values()
            .all(|owner| segment_generations.contains(owner)),
        "regex generation {} has an owner for a missing segment",
        record.generation
    );
    ensure_valid_index!(
        record.manifest.overlay_files == record.overlay_state.owners.len(),
        "regex generation {} declares {} overlay files but owns {}",
        record.generation,
        record.manifest.overlay_files,
        record.overlay_state.owners.len()
    );
    if let Some(expected) = record.manifest.overlay_state_digest.as_deref() {
        let actual = overlay_state_digest(&record.overlay_state)?;
        ensure_valid_index!(
            actual == expected,
            "regex generation {} overlay state failed digest validation (expected {expected}, found {actual})",
            record.generation
        );
    }
    Ok(())
}

pub(crate) fn load_recorded_generation(
    root: &Path,
    manifest: RegexIndexManifest,
    record: RegexGenerationRecord,
) -> Result<RegexIndexRuntime> {
    validate_generation_record(&record)?;
    let publication_fingerprint = generation_record_fingerprint(&record)?;
    ensure_valid_index!(
        record.manifest == durable_manifest(&manifest),
        "regex generation {} record does not match its published manifest",
        manifest.generation
    );
    let base = load_layer(root, &record.base)
        .context("failed to load base regex index layer for published generation")?;
    let mut overlays = Vec::with_capacity(record.segments.len());
    for segment in &record.segments {
        let layer = load_layer(root, &segment.files).with_context(|| {
            format!(
                "failed to load regex overlay segment generation {}",
                segment.generation
            )
        })?;
        overlays.push(LoadedOverlaySegment {
            generation: segment.generation,
            layer: Arc::new(layer),
            files: segment.files.clone(),
        });
    }
    let loaded = LoadedIndex {
        base: Arc::new(base),
        base_files: record.base,
        overlays,
        overlay_state: record.overlay_state,
    };
    validate_loaded_overlay_state(&loaded)?;
    validate_manifest_counts(&manifest, &loaded)?;
    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(loaded)),
        publication_fingerprint: Some(publication_fingerprint),
    })
}

pub(crate) fn load_legacy_generation(
    root: &Path,
    mut manifest: RegexIndexManifest,
) -> Result<RegexIndexRuntime> {
    let mut base_files = LayerFiles::legacy_base();
    let base = load_layer(root, &base_files).context("failed to load base regex index layer")?;
    populate_layer_digests(root, &mut base_files)?;
    let mut overlay_files = LayerFiles::legacy_overlay();
    let overlay =
        load_layer(root, &overlay_files).context("failed to load overlay regex index layer")?;
    populate_layer_digests(root, &mut overlay_files)?;
    let mut overlay_state = load_overlay_state(root)?;
    if let Some(expected) = manifest.overlay_state_digest.as_deref() {
        let actual = overlay_state_digest(&overlay_state)?;
        ensure_valid_index!(
            actual == expected,
            "legacy regex overlay state failed digest validation (expected {expected}, found {actual})"
        );
    }
    let mut overlays = Vec::new();
    if !overlay.docs.is_empty() {
        for doc in &overlay.docs {
            overlay_state.shadowed_paths.insert(doc.path.clone());
            if !overlay_state.deleted_paths.contains(&doc.path) {
                overlay_state
                    .owners
                    .insert(doc.path.clone(), manifest.generation);
            }
        }
        overlays.push(LoadedOverlaySegment {
            generation: manifest.generation,
            layer: Arc::new(overlay),
            files: overlay_files,
        });
    }
    manifest.overlay_files = overlay_state.owners.len();
    manifest.overlay_segments = overlays.len();
    let loaded = LoadedIndex {
        base: Arc::new(base),
        base_files,
        overlays,
        overlay_state,
    };
    validate_loaded_overlay_state(&loaded)?;
    let record = generation_record_from_loaded(&manifest, &loaded);
    validate_generation_record(&record)?;
    let publication_fingerprint = generation_record_fingerprint(&record)?;
    Ok(RegexIndexRuntime {
        manifest,
        loaded: Some(Arc::new(loaded)),
        publication_fingerprint: Some(publication_fingerprint),
    })
}

pub(crate) fn validate_loaded_overlay_state(loaded: &LoadedIndex) -> Result<()> {
    let mut newest_document_owner = BTreeMap::<&str, u64>::new();
    for segment in &loaded.overlays {
        for document in &segment.layer.docs {
            newest_document_owner.insert(&document.path, segment.generation);
        }
    }
    for (path, owner) in &loaded.overlay_state.owners {
        ensure_valid_index!(
            loaded.overlay_state.shadowed_paths.contains(path)
                && !loaded.overlay_state.deleted_paths.contains(path),
            "regex overlay owner for '{path}' conflicts with shadow/tombstone state"
        );
        let owner_segment = loaded
            .overlays
            .iter()
            .find(|segment| segment.generation == *owner);
        ensure_valid_index!(
            owner_segment
                .and_then(|segment| segment.layer.doc_ids_by_path.get(path))
                .is_some(),
            "regex overlay owner generation {owner} has no document for '{path}'"
        );
        ensure_valid_index!(
            newest_document_owner.get(path.as_str()).copied() == Some(*owner),
            "regex overlay owner generation {owner} is not the newest document for '{path}'"
        );
    }
    for path in &loaded.overlay_state.deleted_paths {
        ensure_valid_index!(
            loaded.overlay_state.shadowed_paths.contains(path)
                && !loaded.overlay_state.owners.contains_key(path),
            "regex overlay tombstone for '{path}' conflicts with owner/shadow state"
        );
    }
    for segment in &loaded.overlays {
        for doc in &segment.layer.docs {
            ensure_valid_index!(
                loaded.overlay_state.shadowed_paths.contains(&doc.path),
                "regex overlay segment {} contains unshadowed document '{}'",
                segment.generation,
                doc.path
            );
        }
    }
    for (path, owner) in newest_document_owner {
        if loaded.overlay_state.deleted_paths.contains(path) {
            continue;
        }
        ensure_valid_index!(
            loaded.overlay_state.owners.get(path).copied() == Some(owner),
            "regex overlay newest document generation {owner} has no matching owner for '{path}'"
        );
    }
    ensure_valid_index!(
        loaded.overlay_state.shadowed_paths.iter().all(|path| loaded
            .overlay_state
            .owners
            .contains_key(path)
            || loaded.overlay_state.deleted_paths.contains(path)),
        "regex overlay shadow state contains a path without an owner or tombstone"
    );
    Ok(())
}

pub(crate) fn overlay_state_digest(state: &OverlayState) -> Result<String> {
    Ok(artifact_digest(&serde_json::to_vec(state)?))
}

pub(crate) fn validate_manifest_counts(
    manifest: &RegexIndexManifest,
    loaded: &LoadedIndex,
) -> Result<()> {
    let actual_files = loaded.all_indexed_paths(None).len();
    ensure_valid_index!(
        manifest.overlay_files == loaded.overlay_state.owners.len()
            && manifest.overlay_segments == loaded.overlays.len()
            && manifest.total_files == actual_files,
        "regex generation {} manifest counts do not match loaded layers (files {}/{}, overlays {}/{}, segments {}/{})",
        manifest.generation,
        manifest.total_files,
        actual_files,
        manifest.overlay_files,
        loaded.overlay_state.owners.len(),
        manifest.overlay_segments,
        loaded.overlays.len()
    );
    Ok(())
}

pub(crate) fn recover_previous_runtime(
    root: &Path,
    failed_manifest: Option<RegexIndexManifest>,
    error: SearchError,
    verify_workspace: bool,
) -> Result<RegexIndexRuntime> {
    let failed_generation = failed_manifest
        .as_ref()
        .map_or(0, |manifest| manifest.generation);
    if let Ok(previous) = load_manifest_file(&previous_manifest_path(root)) {
        if let Ok(mut runtime) = load_runtime_from_manifest(root, previous, verify_workspace) {
            let reason = format!(
                "recovered generation {} after rejecting generation {}: {error:#}",
                runtime.manifest.generation, failed_generation
            );
            runtime.manifest.stale_reason = Some(reason.clone());
            runtime.manifest.last_error = Some(reason);
            return Ok(runtime);
        }
    }
    let mut manifest = failed_manifest.unwrap_or_default();
    mark_manifest_unloaded(&mut manifest, "corrupt", format!("{error:#}"));
    Ok(RegexIndexRuntime {
        manifest,
        loaded: None,
        publication_fingerprint: None,
    })
}

pub(crate) fn collect_live_overlay_documents(
    root: &Path,
    overlay_state: &mut OverlayState,
) -> Result<Vec<IndexedDocument>> {
    let paths = overlay_state.owners.keys().cloned().collect::<Vec<_>>();
    let mut docs = Vec::with_capacity(paths.len());
    let mut newly_deleted = Vec::new();
    for path in paths {
        match index_document(root, &root.join(&path))? {
            Some(document) => docs.push(document),
            None => newly_deleted.push(path),
        }
    }
    for path in newly_deleted {
        overlay_state.owners.remove(&path);
        overlay_state.deleted_paths.insert(path);
    }
    docs.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(docs)
}
