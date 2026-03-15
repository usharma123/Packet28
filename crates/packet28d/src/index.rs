use super::*;

pub(crate) fn build_index_status(runtime: &InteractiveIndexRuntime) -> DaemonIndexStatusResponse {
    let dirty_file_count = runtime.manifest.dirty_paths.len();
    let queued_file_count = runtime.manifest.queued_paths.len();
    let ready = runtime.snapshot.is_some()
        && runtime.manifest.status == "ready"
        && runtime.manifest.dirty_paths.is_empty();
    DaemonIndexStatusResponse {
        manifest: runtime.manifest.clone(),
        ready,
        fallback_mode: !ready,
        loaded_generation: runtime
            .snapshot
            .as_ref()
            .map(|_| runtime.manifest.generation),
        dirty_file_count,
        queued_file_count,
    }
}

pub(crate) fn enqueue_index_command(
    state: &Arc<Mutex<DaemonState>>,
    command: IndexCommand,
) -> Result<()> {
    let tx = state.lock().map_err(lock_err)?.index_tx.clone();
    tx.send(command)
        .map_err(|err| anyhow!("failed to queue index work: {err}"))
}

pub(crate) fn enqueue_full_index_rebuild(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    {
        let mut guard = state.lock().map_err(lock_err)?;
        guard.interactive_index.manifest.status = "queued".to_string();
        guard.interactive_index.manifest.last_error = None;
        guard.interactive_index.manifest.queued_paths.clear();
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    }
    enqueue_index_command(state, IndexCommand::RebuildFull)
}

pub(crate) fn enqueue_incremental_index_paths(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
) -> Result<Vec<String>> {
    let normalized = paths
        .iter()
        .map(|path| path.replace('\\', "/"))
        .filter(|path| !path.trim().is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        return Ok(Vec::new());
    }
    {
        let mut guard = state.lock().map_err(lock_err)?;
        for path in &normalized {
            insert_sorted_unique(
                &mut guard.interactive_index.manifest.dirty_paths,
                path.clone(),
            );
            insert_sorted_unique(
                &mut guard.interactive_index.manifest.queued_paths,
                path.clone(),
            );
        }
        if guard.interactive_index.manifest.status == "missing" {
            guard.interactive_index.manifest.status = "queued".to_string();
        }
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    }
    enqueue_index_command(state, IndexCommand::ReindexPaths(normalized.clone()))?;
    Ok(normalized)
}

pub(crate) fn spawn_index_worker(state: Arc<Mutex<DaemonState>>, index_rx: Receiver<IndexCommand>) {
    thread::spawn(move || {
        let mut pending_paths = BTreeSet::<String>::new();
        let mut full_rebuild = false;
        loop {
            let message = if full_rebuild || !pending_paths.is_empty() {
                index_rx.recv_timeout(Duration::from_millis(INDEX_BATCH_DEBOUNCE_MS))
            } else {
                match index_rx.recv() {
                    Ok(message) => Ok(message),
                    Err(_) => return,
                }
            };
            match message {
                Ok(IndexCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(IndexCommand::Clear) => {
                    if let Err(err) = perform_index_clear(&state) {
                        daemon_log(&format!("index clear failed: {err}"));
                    }
                    full_rebuild = false;
                    pending_paths.clear();
                }
                Ok(IndexCommand::RebuildFull) => {
                    full_rebuild = true;
                    pending_paths.clear();
                }
                Ok(IndexCommand::ReindexPaths(paths)) => {
                    for path in paths {
                        pending_paths.insert(path);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if full_rebuild {
                if let Err(err) = perform_full_index_rebuild(&state) {
                    daemon_log(&format!("full index rebuild failed: {err}"));
                }
                full_rebuild = false;
                pending_paths.clear();
                continue;
            }
            if !pending_paths.is_empty() {
                let paths = pending_paths.iter().cloned().collect::<Vec<_>>();
                pending_paths.clear();
                if let Err(err) = perform_incremental_index_update(&state, &paths) {
                    daemon_log(&format!("incremental index update failed: {err}"));
                }
            }
        }
    });
}

fn perform_full_index_rebuild(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let root = {
        let mut guard = state.lock().map_err(lock_err)?;
        guard.interactive_index.manifest.status = "building".to_string();
        guard.interactive_index.manifest.last_build_started_at_unix = Some(now_unix());
        guard.interactive_index.manifest.last_error = None;
        guard.interactive_index.manifest.queued_paths.clear();
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        guard.root.clone()
    };
    let snapshot = mapy_core::build_repo_index(&root, true)
        .map_err(|err| anyhow!("failed to build repo index: {err}"))?;
    save_index_snapshot_file(&root, &snapshot)?;
    let mut guard = state.lock().map_err(lock_err)?;
    guard.interactive_index.snapshot = Some(Arc::new(snapshot.clone()));
    guard.interactive_index.manifest.generation = guard
        .interactive_index
        .manifest
        .generation
        .saturating_add(1);
    guard.interactive_index.manifest.status = "ready".to_string();
    guard.interactive_index.manifest.dirty_paths.clear();
    guard.interactive_index.manifest.queued_paths.clear();
    guard.interactive_index.manifest.total_files = snapshot.files.len();
    guard.interactive_index.manifest.indexed_files = snapshot.files.len();
    guard
        .interactive_index
        .manifest
        .last_build_completed_at_unix = Some(now_unix());
    guard.interactive_index.manifest.last_error = None;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    Ok(())
}

fn perform_incremental_index_update(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let (root, snapshot_opt) = {
        let mut guard = state.lock().map_err(lock_err)?;
        if guard.interactive_index.snapshot.is_none() {
            drop(guard);
            return perform_full_index_rebuild(state);
        }
        guard.interactive_index.manifest.status = "building".to_string();
        guard.interactive_index.manifest.last_build_started_at_unix = Some(now_unix());
        for path in paths {
            insert_sorted_unique(
                &mut guard.interactive_index.manifest.dirty_paths,
                path.clone(),
            );
        }
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        (guard.root.clone(), guard.interactive_index.snapshot.clone())
    };
    let Some(snapshot_arc) = snapshot_opt else {
        return perform_full_index_rebuild(state);
    };
    let mut snapshot = (*snapshot_arc).clone();
    let summary = mapy_core::update_repo_index(&root, &mut snapshot, paths, true)
        .map_err(|err| anyhow!("failed to update repo index: {err}"))?;
    save_index_snapshot_file(&root, &snapshot)?;
    let mut guard = state.lock().map_err(lock_err)?;
    guard.interactive_index.snapshot = Some(Arc::new(snapshot.clone()));
    guard.interactive_index.manifest.generation = guard
        .interactive_index
        .manifest
        .generation
        .saturating_add(1);
    guard.interactive_index.manifest.status = "ready".to_string();
    for path in &summary.changed_paths {
        guard
            .interactive_index
            .manifest
            .dirty_paths
            .retain(|candidate| candidate != path);
        guard
            .interactive_index
            .manifest
            .queued_paths
            .retain(|candidate| candidate != path);
    }
    guard.interactive_index.manifest.total_files = snapshot.files.len();
    guard.interactive_index.manifest.indexed_files = snapshot.files.len();
    guard
        .interactive_index
        .manifest
        .last_build_completed_at_unix = Some(now_unix());
    guard.interactive_index.manifest.last_error = None;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    Ok(())
}

fn perform_index_clear(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    clear_index_files(&guard.root)?;
    guard.interactive_index = InteractiveIndexRuntime {
        manifest: default_index_manifest(&guard.root),
        snapshot: None,
    };
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    Ok(())
}

pub(crate) fn daemon_index_status(
    state: Arc<Mutex<DaemonState>>,
) -> Result<DaemonIndexStatusResponse> {
    let guard = state.lock().map_err(lock_err)?;
    Ok(build_index_status(&guard.interactive_index))
}

pub(crate) fn daemon_index_rebuild(
    state: Arc<Mutex<DaemonState>>,
    request: DaemonIndexRebuildRequest,
) -> Result<DaemonIndexRebuildResponse> {
    let queued_paths = if request.full || request.paths.is_empty() {
        enqueue_full_index_rebuild(&state)?;
        Vec::new()
    } else {
        enqueue_incremental_index_paths(&state, &request.paths)?
    };
    let generation = state
        .lock()
        .map_err(lock_err)?
        .interactive_index
        .manifest
        .generation;
    Ok(DaemonIndexRebuildResponse {
        accepted: true,
        full: request.full || request.paths.is_empty(),
        generation: Some(generation),
        queued_paths,
    })
}

pub(crate) fn daemon_index_clear(
    state: Arc<Mutex<DaemonState>>,
) -> Result<DaemonIndexClearResponse> {
    enqueue_index_command(&state, IndexCommand::Clear)?;
    Ok(DaemonIndexClearResponse { cleared: true })
}
