use super::*;

pub(crate) fn default_index_manifest(root: &Path) -> DaemonIndexManifest {
    DaemonIndexManifest {
        schema_version: INTERACTIVE_INDEX_SCHEMA_VERSION,
        root: root.to_string_lossy().to_string(),
        generation: 0,
        include_tests: true,
        status: "missing".to_string(),
        dirty_paths: Vec::new(),
        queued_paths: Vec::new(),
        total_files: 0,
        indexed_files: 0,
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

pub(crate) fn load_index_snapshot_file(
    root: &Path,
    manifest: &DaemonIndexManifest,
) -> Option<Arc<mapy_core::RepoIndexSnapshot>> {
    if manifest.status == "missing" || manifest.generation == 0 {
        return None;
    }
    let raw = fs::read(index_snapshot_path(root)).ok()?;
    let snapshot = bincode::deserialize::<mapy_core::RepoIndexSnapshot>(&raw).ok()?;
    if snapshot.version == 0 {
        return None;
    }
    Some(Arc::new(snapshot))
}

pub(crate) fn save_index_snapshot_file(
    root: &Path,
    snapshot: &mapy_core::RepoIndexSnapshot,
) -> Result<()> {
    fs::create_dir_all(index_dir(root))
        .with_context(|| format!("failed to create index dir '{}'", index_dir(root).display()))?;
    let encoded = bincode::serialize(snapshot)?;
    fs::write(index_snapshot_path(root), encoded).with_context(|| {
        format!(
            "failed to write index snapshot '{}'",
            index_snapshot_path(root).display()
        )
    })?;
    Ok(())
}

pub(crate) fn clear_index_files(root: &Path) -> Result<()> {
    for path in [index_manifest_path(root), index_snapshot_path(root)] {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove '{}'", path.display()))?;
        }
    }
    Ok(())
}
