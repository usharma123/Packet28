//! Experimental borrowed-content rebuild session for daemon composition.
//!
//! This module is available only with the non-default
//! `shared-repository-scan` feature. It retains regex-specific filtering,
//! derived grams, writer locking, artifact validation, and manifest ownership
//! while allowing a coordinator to lend each file buffer.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::error::{Result, SearchError};
#[cfg(test)]
use crate::generation::rebuild_full_index;
use crate::generation::{
    durable_manifest, load_published_runtime, load_runtime, overlay_state_digest,
    prune_generation_artifacts, publish_manifest, validate_generation_record,
};
use crate::layer::{build_layer, IndexedDocument};
use crate::model::{
    LayerFiles, LoadedIndex, RegexGenerationRecord, RegexIndexManifest, RegexIndexRuntime,
    MAX_INDEXED_FILE_BYTES, REGEX_INDEX_SCHEMA_VERSION,
};
#[cfg(test)]
use crate::paths::{manifest_path, previous_manifest_path};
use crate::postings::build_indexed_grams;
use crate::publication::{
    acquire_writer_lock, capture_manifest_files, ensure_manifest_files_unchanged,
    reserve_generation, restore_owned_manifest_files, save_generation_record,
    seal_generation_record, GenerationWriterLock, ManifestFilesSnapshot,
};
use crate::support::{mtime_secs, now_unix};
use crate::weights::WEIGHT_TABLE_VERSION;
use crate::workspace::{
    authenticate_full_build_workspace, begin_full_build_workspace, GitWorkspaceSnapshot,
};

/// Maximum file size consumed by the regex full-index builder.
pub const MAX_SHARED_SCAN_CONTENT_BYTES: usize = MAX_INDEXED_FILE_BYTES;

/// Content digests used to prove byte-for-byte base-layer parity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexIndexContentDigests {
    /// Digest of the fixed-width lookup table.
    pub lookup: String,
    /// Digest of the postings table.
    pub postings: String,
    /// Digest of the serialized document table.
    pub documents: String,
}

impl RegexIndexRuntime {
    /// Returns the validated base-layer content digests for parity checks.
    pub fn shared_scan_content_digests(&self) -> Option<RegexIndexContentDigests> {
        let loaded = self.loaded.as_ref()?;
        Some(RegexIndexContentDigests {
            lookup: loaded.base_files.lookup_digest.clone(),
            postings: loaded.base_files.postings_digest.clone(),
            documents: loaded.base_files.docs_digest.clone(),
        })
    }

    /// Returns base-layer document paths in document-id order.
    pub fn shared_scan_document_paths(&self) -> Option<Vec<String>> {
        let loaded = self.loaded.as_ref()?;
        Some(
            loaded
                .base
                .docs
                .iter()
                .map(|document| document.path.clone())
                .collect(),
        )
    }
}

/// Returns whether a regex rebuild wants a path yielded by its repository walker.
pub fn wants_path(relative_path: &str) -> bool {
    is_normalized_repository_relative_path(relative_path)
        && !relative_path.starts_with(".git/")
        && !relative_path.starts_with(".packet28/")
        && !relative_path.starts_with("target/")
        && !relative_path.starts_with("node_modules/")
}

fn is_normalized_repository_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path.split('/').all(|component| !component.is_empty())
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Returns whether a discovered regex path needs its content read.
pub fn wants_content(metadata: &fs::Metadata) -> bool {
    metadata.len() <= MAX_INDEXED_FILE_BYTES as u64
}

/// A regex-specific full-rebuild session that borrows one content buffer at a time.
pub struct RegexIndexScanSession {
    root: PathBuf,
    include_tests: bool,
    discovered: BTreeSet<String>,
    docs: Vec<IndexedDocument>,
    writer: GenerationWriterLock,
    previous: Option<RegexIndexManifest>,
    publication_snapshot: ManifestFilesSnapshot,
    generation: u64,
    started_at_unix: u64,
    workspace_before: Option<GitWorkspaceSnapshot>,
}

impl RegexIndexScanSession {
    /// Starts a locked regex rebuild from a coordinator-owned discovery plan.
    ///
    /// # Errors
    ///
    /// Returns a typed filesystem or manifest error when the writer lock or
    /// pre-publication state cannot be read, or [`SearchError::IndexNotReady`]
    /// when a Git-backed workspace cannot be attested.
    pub fn begin(root: &Path, include_tests: bool, discovered_paths: &[String]) -> Result<Self> {
        if let Some(path) = discovered_paths
            .iter()
            .find(|path| !is_normalized_repository_relative_path(path))
        {
            return Err(SearchError::InvalidChangedPath { path: path.clone() });
        }
        let writer = acquire_writer_lock(root)?;
        let workspace_before = begin_full_build_workspace(root)?;
        let publication_snapshot = capture_manifest_files(root)?;
        let previous = load_published_runtime(root)
            .ok()
            .flatten()
            .or_else(|| load_runtime(root).ok().filter(RegexIndexRuntime::is_loaded))
            .map(|runtime| durable_manifest(&runtime.manifest));
        let generation = reserve_generation(root, &writer)?;
        Ok(Self {
            root: root.to_path_buf(),
            include_tests,
            discovered: discovered_paths
                .iter()
                .filter(|path| wants_path(path))
                .cloned()
                .collect(),
            docs: Vec::new(),
            writer,
            previous,
            publication_snapshot,
            generation,
            started_at_unix: now_unix(),
            workspace_before,
        })
    }

    /// Returns the regex-specific progress total for this discovery plan.
    pub fn total_files(&self) -> usize {
        self.discovered.len()
    }

    /// Borrows one successfully read regex candidate and derives its grams.
    ///
    /// Empty, NUL-containing, and oversized files retain the existing
    /// full-rebuild omission behavior.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::CorruptIndex`] when `relative_path` was not part
    /// of the immutable discovery plan.
    pub fn ingest(
        &mut self,
        relative_path: &str,
        metadata: &fs::Metadata,
        bytes: &[u8],
    ) -> Result<()> {
        if !self.discovered.contains(relative_path) {
            return Err(SearchError::corrupt(format!(
                "shared regex scan path '{relative_path}' was not in the discovery plan"
            )));
        }
        if !wants_content(metadata) || bytes.is_empty() || bytes.contains(&0) {
            return Ok(());
        }
        self.docs.push(IndexedDocument {
            doc_id: 0,
            path: relative_path.to_string(),
            size: metadata.len(),
            mtime_secs: mtime_secs(metadata),
            fingerprint: blake3::hash(bytes).to_hex().to_string(),
            grams: build_indexed_grams(bytes),
        });
        Ok(())
    }

    /// Builds and validates immutable artifacts without publishing the manifest.
    ///
    /// # Errors
    ///
    /// Returns typed I/O, encoding, or corruption failures from base-layer
    /// construction and generation-record validation. A workspace change or
    /// borrowed-content mismatch returns [`SearchError::IndexNotReady`].
    pub fn prepare(mut self) -> Result<PreparedRegexIndexRuntime> {
        self.docs.sort_by(|left, right| left.path.cmp(&right.path));
        for (index, document) in self.docs.iter_mut().enumerate() {
            document.doc_id = u32::try_from(index)?;
        }
        let mut manifest = RegexIndexManifest {
            schema_version: REGEX_INDEX_SCHEMA_VERSION,
            weight_table_version: WEIGHT_TABLE_VERSION,
            generation: self.generation,
            include_tests: self.include_tests,
            status: "ready".to_string(),
            last_build_started_at_unix: Some(self.started_at_unix),
            ..RegexIndexManifest::default()
        };
        let mut base_files = LayerFiles::base(self.generation);
        let base_layer = build_layer(&self.root, &self.docs, &mut base_files)?;
        manifest.total_files = self.docs.len();
        manifest.indexed_files = self.docs.len();
        let reported_paths = self.discovered.iter().cloned().collect::<Vec<_>>();
        let fingerprints = self
            .docs
            .iter()
            .map(|doc| (doc.path.clone(), doc.fingerprint.clone()))
            .collect();
        let overlay_state = authenticate_full_build_workspace(
            &self.root,
            self.workspace_before.as_ref(),
            &reported_paths,
            &fingerprints,
            &mut manifest,
        )?;
        manifest.overlay_state_digest = Some(overlay_state_digest(&overlay_state)?);
        manifest.last_build_completed_at_unix = Some(now_unix());
        let mut record = RegexGenerationRecord {
            schema_version: REGEX_INDEX_SCHEMA_VERSION,
            generation: self.generation,
            manifest: manifest.clone(),
            base: base_files.clone(),
            segments: Vec::new(),
            overlay_state: overlay_state.clone(),
        };
        let publication_fingerprint = seal_generation_record(&mut manifest, &mut record)?;
        validate_generation_record(&record)?;
        save_generation_record(&self.root, &record)?;
        let runtime = RegexIndexRuntime {
            manifest,
            loaded: Some(Arc::new(LoadedIndex {
                base: Arc::new(base_layer),
                base_files,
                overlays: Vec::new(),
                overlay_state,
            })),
            publication_fingerprint: Some(publication_fingerprint),
        };
        Ok(PreparedRegexIndexRuntime {
            root: self.root,
            _writer: self.writer,
            previous: self.previous,
            publication_snapshot: self.publication_snapshot,
            published_snapshot: None,
            runtime,
            published: false,
            committed: false,
        })
    }
}

/// A validated regex generation awaiting manifest publication.
///
/// Dropping a published but uncommitted handle performs a best-effort rollback
/// to the exact manifest bytes captured before the scan began.
pub struct PreparedRegexIndexRuntime {
    root: PathBuf,
    _writer: GenerationWriterLock,
    previous: Option<RegexIndexManifest>,
    publication_snapshot: ManifestFilesSnapshot,
    published_snapshot: Option<ManifestFilesSnapshot>,
    runtime: RegexIndexRuntime,
    published: bool,
    committed: bool,
}

impl PreparedRegexIndexRuntime {
    /// Publishes this validated generation while retaining rollback ownership.
    ///
    /// # Errors
    ///
    /// Returns a typed manifest error or rejects a duplicate publication.
    pub fn publish(&mut self) -> Result<()> {
        self.publish_with(|root, writer, expected, previous, current| {
            publish_manifest(root, writer, expected, previous, current).map(|_| ())
        })
    }

    fn publish_with<F>(&mut self, publish: F) -> Result<()>
    where
        F: FnOnce(
            &Path,
            &GenerationWriterLock,
            &ManifestFilesSnapshot,
            Option<&RegexIndexManifest>,
            &RegexIndexManifest,
        ) -> Result<()>,
    {
        self.publish_with_observer(publish, capture_manifest_files)
    }

    fn publish_with_observer<F, O>(&mut self, publish: F, mut observe: O) -> Result<()>
    where
        F: FnOnce(
            &Path,
            &GenerationWriterLock,
            &ManifestFilesSnapshot,
            Option<&RegexIndexManifest>,
            &RegexIndexManifest,
        ) -> Result<()>,
        O: FnMut(&Path) -> Result<ManifestFilesSnapshot>,
    {
        if self.published {
            return Err(SearchError::corrupt(
                "prepared regex generation was already published",
            ));
        }
        ensure_manifest_files_unchanged(&self.root, &self.publication_snapshot)?;
        let published_snapshot = ManifestFilesSnapshot {
            current: Some(serde_json::to_vec_pretty(&self.runtime.manifest)?),
            previous: self
                .previous
                .as_ref()
                .filter(|manifest| manifest.generation > 0)
                .map(durable_manifest)
                .map(|manifest| serde_json::to_vec_pretty(&manifest))
                .transpose()?
                .or_else(|| self.publication_snapshot.previous.clone()),
        };
        match publish(
            &self.root,
            &self._writer,
            &self.publication_snapshot,
            self.previous.as_ref(),
            &self.runtime.manifest,
        ) {
            Ok(()) => {
                self.published = true;
                self.published_snapshot = Some(published_snapshot.clone());
                match observe(&self.root) {
                    Ok(actual) if actual == published_snapshot => Ok(()),
                    Ok(_) => {
                        self.relinquish_publication();
                        Err(SearchError::corrupt(
                            "regex index manifests changed after publication",
                        ))
                    }
                    Err(error) => Err(error),
                }
            }
            Err(publication) => {
                self.handle_failed_publication(publication, published_snapshot, observe(&self.root))
            }
        }
    }

    fn handle_failed_publication(
        &mut self,
        publication: SearchError,
        published_snapshot: ManifestFilesSnapshot,
        observed: Result<ManifestFilesSnapshot>,
    ) -> Result<()> {
        match observed {
            Ok(actual) if actual == self.publication_snapshot => Err(publication),
            Ok(actual)
                if snapshot_uses_only_owned_or_target(
                    &actual,
                    &published_snapshot,
                    &self.publication_snapshot,
                ) =>
            {
                self.published = true;
                self.published_snapshot = Some(published_snapshot);
                self.restore_after_publication_error(publication)
            }
            Ok(_) => Err(publication),
            Err(observation) => {
                self.published = true;
                self.published_snapshot = Some(published_snapshot);
                Err(SearchError::FailureProvenance {
                    build: Box::new(publication),
                    persistence: Box::new(
                        observation.context("failed to inspect regex publication outcome"),
                    ),
                })
            }
        }
    }

    fn restore_after_publication_error(&mut self, publication: SearchError) -> Result<()> {
        let published_snapshot = self.published_snapshot.as_ref().ok_or_else(|| {
            SearchError::corrupt("failed regex publication has no rollback fingerprint")
        })?;
        match restore_owned_manifest_files(
            &self.root,
            published_snapshot,
            &self.publication_snapshot,
        ) {
            Ok(()) => {
                self.relinquish_publication();
                Err(publication)
            }
            Err(rollback) => Err(SearchError::FailureProvenance {
                build: Box::new(publication),
                persistence: Box::new(
                    rollback.context("failed to restore pre-publication regex manifests"),
                ),
            }),
        }
    }

    fn relinquish_publication(&mut self) {
        self.published = false;
        self.published_snapshot = None;
    }

    /// Restores both regex manifest files to their pre-publication bytes.
    ///
    /// # Errors
    ///
    /// Returns a typed filesystem error when either manifest cannot be restored.
    pub fn rollback(&mut self) -> Result<()> {
        if !self.published {
            return Ok(());
        }
        let published_snapshot = self.published_snapshot.as_ref().ok_or_else(|| {
            SearchError::corrupt("published regex generation has no rollback fingerprint")
        })?;
        restore_owned_manifest_files(&self.root, published_snapshot, &self.publication_snapshot)?;
        self.published = false;
        self.published_snapshot = None;
        Ok(())
    }

    /// Returns metadata for the validated generation without publishing it.
    pub fn manifest(&self) -> &RegexIndexManifest {
        &self.runtime.manifest
    }

    /// Returns the validated base-layer digests before publication.
    pub fn content_digests(&self) -> Option<RegexIndexContentDigests> {
        self.runtime.shared_scan_content_digests()
    }

    /// Finalizes a successfully paired publication and releases its writer lock.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::CorruptIndex`] if [`Self::publish`] has not succeeded.
    pub fn commit(mut self) -> Result<RegexIndexRuntime> {
        if !self.published {
            return Err(SearchError::corrupt(
                "prepared regex generation must be published before commit",
            ));
        }
        let published_snapshot = self.published_snapshot.as_ref().ok_or_else(|| {
            SearchError::corrupt("published regex generation has no commit fingerprint")
        })?;
        ensure_manifest_files_unchanged(&self.root, published_snapshot)?;
        let _ = prune_generation_artifacts(&self.root, &self._writer);
        self.committed = true;
        Ok(self.runtime.clone())
    }
}

fn snapshot_uses_only_owned_or_target(
    actual: &ManifestFilesSnapshot,
    owned: &ManifestFilesSnapshot,
    target: &ManifestFilesSnapshot,
) -> bool {
    (actual.current == owned.current || actual.current == target.current)
        && (actual.previous == owned.previous || actual.previous == target.previous)
}

impl Drop for PreparedRegexIndexRuntime {
    fn drop(&mut self) {
        if self.published && !self.committed {
            let _ = self.rollback();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare_shared(root: &Path, paths: &[String]) -> PreparedRegexIndexRuntime {
        let mut session = RegexIndexScanSession::begin(root, true, paths).unwrap();
        for relative_path in paths {
            let path = root.join(relative_path);
            let metadata = fs::metadata(&path).unwrap();
            if wants_content(&metadata) {
                let bytes = fs::read(path).unwrap();
                session.ingest(relative_path, &metadata, &bytes).unwrap();
            }
        }
        session.prepare().unwrap()
    }

    #[test]
    fn borrowed_scan_matches_standard_layer_bytes_and_rolls_back_until_commit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            b"pub fn packet_search_literal() {}\n",
        )
        .unwrap();
        fs::write(root.join("docs/readme.md"), b"packet search literal\n").unwrap();
        let paths = vec!["docs/readme.md".to_string(), "src/lib.rs".to_string()];

        let standard = rebuild_full_index(root, true).unwrap();
        let expected_digests = standard.shared_scan_content_digests().unwrap();
        let mut prepared = prepare_shared(root, &paths);

        assert_eq!(prepared.content_digests().unwrap(), expected_digests);
        assert_eq!(
            load_runtime(root).unwrap().manifest.generation,
            standard.manifest.generation
        );
        prepared.publish().unwrap();
        assert!(load_runtime(root).unwrap().manifest.generation > standard.manifest.generation);
        prepared.rollback().unwrap();
        assert_eq!(
            load_runtime(root).unwrap().manifest.generation,
            standard.manifest.generation
        );

        prepared.publish().unwrap();
        let shared = prepared.commit().unwrap();
        assert_eq!(
            shared.shared_scan_content_digests().unwrap(),
            expected_digests
        );
    }

    #[test]
    fn dropping_a_published_generation_restores_the_previous_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Searchable;\n").unwrap();
        let paths = vec!["src/lib.rs".to_string()];
        let standard = rebuild_full_index(root, true).unwrap();

        let mut prepared = prepare_shared(root, &paths);
        prepared.publish().unwrap();
        drop(prepared);

        assert_eq!(
            load_runtime(root).unwrap().manifest.generation,
            standard.manifest.generation
        );
    }

    #[test]
    fn failed_publication_restores_both_manifest_files_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Searchable;\n").unwrap();
        let paths = vec!["src/lib.rs".to_string()];
        rebuild_full_index(root, true).unwrap();
        rebuild_full_index(root, true).unwrap();
        let current_path = manifest_path(root);
        let previous_path = previous_manifest_path(root);
        let expected_current = fs::read(&current_path).unwrap();
        let expected_previous = fs::read(&previous_path).unwrap();
        let mut prepared = prepare_shared(root, &paths);

        let error = prepared
            .publish_with(|root, _, _, previous, _| {
                let previous = previous.expect("previous generation");
                fs::write(
                    previous_manifest_path(root),
                    serde_json::to_vec_pretty(&durable_manifest(previous)).unwrap(),
                )
                .unwrap();
                Err(SearchError::corrupt("injected publication failure"))
            })
            .unwrap_err();

        assert!(error.to_string().contains("injected publication failure"));
        assert_eq!(fs::read(current_path).unwrap(), expected_current);
        assert_eq!(fs::read(previous_path).unwrap(), expected_previous);
    }

    #[test]
    fn post_publication_cas_failure_preserves_foreign_manifest_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Shared;\n").unwrap();
        rebuild_full_index(root, true).unwrap();
        let paths = vec![String::from("src/lib.rs")];
        let mut prepared = prepare_shared(root, &paths);
        let foreign = b"{\"foreign\":true}";

        let error = prepared
            .publish_with(|root, writer, expected, previous, current| {
                publish_manifest(root, writer, expected, previous, current).map(|_| ())?;
                fs::write(manifest_path(root), foreign).unwrap();
                Ok(())
            })
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("manifests changed after publication"));
        assert_eq!(fs::read(manifest_path(root)).unwrap(), foreign);
        drop(prepared);
        assert_eq!(fs::read(manifest_path(root)).unwrap(), foreign);
    }

    #[test]
    fn pre_publication_cas_failure_preserves_foreign_manifest_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Shared;\n").unwrap();
        rebuild_full_index(root, true).unwrap();
        let paths = vec![String::from("src/lib.rs")];
        let mut prepared = prepare_shared(root, &paths);
        let foreign = b"{\"foreign\":\"before\"}";

        let error = prepared
            .publish_with(|root, writer, expected, previous, current| {
                fs::write(manifest_path(root), foreign).unwrap();
                publish_manifest(root, writer, expected, previous, current).map(|_| ())
            })
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("manifests changed while the writer lock was held"));
        drop(prepared);
        assert_eq!(fs::read(manifest_path(root)).unwrap(), foreign);
    }

    #[test]
    fn indeterminate_post_publication_read_retains_rollback_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Shared;\n").unwrap();
        let base = rebuild_full_index(root, true).unwrap();
        let paths = vec![String::from("src/lib.rs")];
        let mut prepared = prepare_shared(root, &paths);

        let error = prepared
            .publish_with_observer(
                |root, writer, expected, previous, current| {
                    publish_manifest(root, writer, expected, previous, current).map(|_| ())
                },
                |_| Err(std::io::Error::other("injected manifest read failure").into()),
            )
            .unwrap_err();

        assert!(error.to_string().contains("injected manifest read failure"));
        prepared.rollback().unwrap();
        assert_eq!(
            load_runtime(root).unwrap().manifest.generation,
            base.manifest.generation
        );
    }

    #[test]
    fn rollback_retries_after_current_manifest_was_already_restored() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub struct Shared;\n").unwrap();
        rebuild_full_index(root, true).unwrap();
        let paths = vec![String::from("src/lib.rs")];
        let mut prepared = prepare_shared(root, &paths);
        let before = prepared.publication_snapshot.clone();
        prepared.publish().unwrap();

        fs::write(
            manifest_path(root),
            before.current.as_deref().expect("pre-publication manifest"),
        )
        .unwrap();
        prepared.rollback().unwrap();

        assert_eq!(capture_manifest_files(root).unwrap(), before);
    }

    #[test]
    fn discovery_plan_rejects_paths_outside_the_repository() {
        let directory = tempfile::tempdir().unwrap();
        let error =
            RegexIndexScanSession::begin(directory.path(), true, &["../outside.rs".to_string()])
                .err()
                .unwrap();

        assert!(matches!(error, SearchError::InvalidChangedPath { .. }));
        assert!(!wants_path("../outside.rs"));
        assert!(!wants_path(r"src\\outside.rs"));
    }
}
