use super::*;
use crate::broker::insert_sorted_unique;
use crate::runtime_files::{
    clear_index_files, clear_regex_index_files, complete_index_clear_revision,
    index_clear_is_complete, index_clear_is_pending, pending_index_clear,
    persist_index_clear_pending, record_index_work_after_clear,
};
#[cfg(all(test, unix))]
use crate::runtime_files::{
    clear_index_files_with_binding_hook_for_test,
    clear_index_files_with_generation_fence_hook_for_test,
    index_clear_is_pending_with_final_binding_hook_for_test,
    open_index_clear_parent_with_sync_for_test,
};
#[cfg(test)]
use crate::runtime_files::{
    complete_index_clear, complete_index_clear_with_sync_for_test,
    complete_index_clear_with_transition_hook_for_test,
    index_clear_is_pending_with_read_hook_for_test, index_clear_requires_rebuild,
    index_clear_temporary_path_for_test, persist_index_clear_pending_with_nonce_for_test,
    persist_index_clear_pending_with_parent_hook_for_test,
};

const INDEX_WAKE_QUEUE_CAPACITY: usize = 1;
const MAX_PENDING_INDEX_PATHS: usize = 4_096;
const MAX_INDEX_PATH_INPUTS: usize = MAX_PENDING_INDEX_PATHS * 2;
const MAX_INDEX_PATH_BYTES: usize = 4_096;
const MAX_INDEX_PATH_INPUT_BYTES: usize = 1024 * 1024;
const INDEX_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const INDEX_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct IndexIngress {
    pending: Arc<Mutex<PendingIndexWork>>,
    wake: std::sync::mpsc::SyncSender<()>,
}

pub(crate) struct IndexWorkReceiver {
    pending: Arc<Mutex<PendingIndexWork>>,
    wake: std::sync::mpsc::Receiver<()>,
}

#[derive(Default)]
struct PendingIndexWork {
    next_epoch: u64,
    clear_epoch: Option<u64>,
    clear_revision: Option<u64>,
    full_rebuild_epoch: Option<u64>,
    paths: BTreeMap<String, u64>,
    shutdown_epoch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexWorkBatch {
    clear_epoch: Option<u64>,
    clear_revision: Option<u64>,
    full_rebuild_epoch: Option<u64>,
    paths: BTreeMap<String, u64>,
    shutdown_epoch: Option<u64>,
    epoch: u64,
}

impl IndexWorkBatch {
    fn follow_up_after(&self, epoch: u64) -> IndexFollowUp {
        IndexFollowUp {
            clear: self
                .clear_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
            full_rebuild: self
                .full_rebuild_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
            paths: self
                .paths
                .iter()
                .filter_map(|(path, command_epoch)| {
                    (*command_epoch > epoch).then_some(path.clone())
                })
                .collect(),
            shutdown: self
                .shutdown_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
        }
    }

    fn paths_after(&self, epoch: u64) -> Vec<String> {
        self.paths
            .iter()
            .filter_map(|(path, command_epoch)| (*command_epoch > epoch).then_some(path.clone()))
            .collect()
    }

    fn merge_newer(mut self, newer: Self) -> Self {
        if newer.clear_epoch.is_some() {
            self.clear_epoch = newer.clear_epoch;
            self.clear_revision = newer.clear_revision;
            self.full_rebuild_epoch = newer.full_rebuild_epoch;
            self.paths = newer.paths;
        } else {
            if let Some(full_epoch) = newer.full_rebuild_epoch {
                self.full_rebuild_epoch = Some(full_epoch);
                self.paths.retain(|_, path_epoch| *path_epoch > full_epoch);
            }
            for (path, epoch) in newer.paths {
                self.paths.insert(path, epoch);
            }
        }
        if newer.shutdown_epoch.is_some() {
            self.shutdown_epoch = newer.shutdown_epoch;
        }
        self.epoch = self.epoch.max(newer.epoch);
        self
    }

    fn promote_retry_to_full(mut self) -> Self {
        self.full_rebuild_epoch = Some(self.epoch);
        self.paths.clear();
        self
    }
}

#[derive(Debug, Clone, Default)]
struct IndexFollowUp {
    clear: bool,
    full_rebuild: bool,
    paths: BTreeSet<String>,
    shutdown: bool,
}

impl IndexFollowUp {
    fn merge(&mut self, newer: Self) {
        self.clear |= newer.clear;
        self.full_rebuild |= newer.full_rebuild;
        self.paths.extend(newer.paths);
        self.shutdown |= newer.shutdown;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexBatchStatus {
    Complete,
    Retry(IndexWorkBatch),
}

#[derive(Debug)]
struct IndexRetryBackoff {
    consecutive_failures: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl Default for IndexRetryBackoff {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            initial_delay: INDEX_RETRY_INITIAL_DELAY,
            max_delay: INDEX_RETRY_MAX_DELAY,
        }
    }
}

impl IndexRetryBackoff {
    #[cfg(test)]
    fn with_delays(initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            consecutive_failures: 0,
            initial_delay,
            max_delay,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let shift = self.consecutive_failures.min(31);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.initial_delay
            .saturating_mul(1_u32 << shift)
            .min(self.max_delay)
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

#[derive(Debug)]
pub(crate) struct DaemonIndexSearchNotReady {
    reason: String,
}

impl std::fmt::Display for DaemonIndexSearchNotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "indexed search is not ready: {}", self.reason)
    }
}

impl std::error::Error for DaemonIndexSearchNotReady {}

#[derive(Debug)]
pub(crate) struct IndexQueueOutcome {
    pub(crate) full: bool,
    pub(crate) queued_paths: Vec<String>,
}

impl IndexIngress {
    pub(crate) fn new() -> (Self, IndexWorkReceiver) {
        let (wake, receiver) = std::sync::mpsc::sync_channel(INDEX_WAKE_QUEUE_CAPACITY);
        let pending = Arc::new(Mutex::new(PendingIndexWork::default()));
        (
            Self {
                pending: pending.clone(),
                wake,
            },
            IndexWorkReceiver {
                pending,
                wake: receiver,
            },
        )
    }

    pub(crate) fn send(&self, command: IndexCommand) -> Result<()> {
        self.send_with_clear_revision(command, None)
    }

    pub(crate) fn request_shutdown(&self, publish_shutdown: impl FnOnce()) -> Result<()> {
        let mut pending = self.pending.lock().map_err(lock_err)?;
        let result = if pending.shutdown_epoch.is_some() {
            Ok(())
        } else if let Some(epoch) = pending.next_epoch.checked_add(1) {
            pending.next_epoch = epoch;
            pending.shutdown_epoch = Some(epoch);
            match self.wake.try_send(()) {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => Ok(()),
                Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                    pending.shutdown_epoch = None;
                    Err(anyhow!("index worker is not running"))
                }
            }
        } else {
            Err(anyhow!("index ingress epoch exhausted"))
        };
        // Keep batch consumption behind this lock until the daemon's shutdown
        // intent is visible. Only an accepted shutdown may be repeated safely.
        publish_shutdown();
        result
    }

    fn send_clear(&self, revision: u64) -> Result<()> {
        self.send_with_clear_revision(IndexCommand::Clear, Some(revision))
    }

    fn send_with_clear_revision(
        &self,
        command: IndexCommand,
        clear_revision: Option<u64>,
    ) -> Result<()> {
        if matches!(command, IndexCommand::Shutdown) {
            return self.request_shutdown(|| {});
        }
        {
            let mut pending = self.pending.lock().map_err(lock_err)?;
            if pending.shutdown_epoch.is_some() && !matches!(command, IndexCommand::Shutdown) {
                anyhow::bail!("index worker is shutting down");
            }
            pending.next_epoch = pending
                .next_epoch
                .checked_add(1)
                .ok_or_else(|| anyhow!("index ingress epoch exhausted"))?;
            let epoch = pending.next_epoch;
            match command {
                IndexCommand::Clear => {
                    pending.clear_epoch = Some(epoch);
                    pending.clear_revision = clear_revision;
                    pending.full_rebuild_epoch = None;
                    pending.paths.clear();
                }
                IndexCommand::RebuildFull => {
                    pending.full_rebuild_epoch = Some(epoch);
                    pending.paths.clear();
                }
                IndexCommand::ReindexPaths(paths) => {
                    for path in paths {
                        if path.len() > MAX_INDEX_PATH_BYTES {
                            pending.full_rebuild_epoch = Some(epoch);
                            pending.paths.clear();
                            break;
                        }
                        pending.paths.insert(path, epoch);
                        if pending.paths.len() > MAX_PENDING_INDEX_PATHS {
                            pending.full_rebuild_epoch = Some(epoch);
                            pending.paths.clear();
                            break;
                        }
                    }
                }
                IndexCommand::Shutdown => {
                    pending.shutdown_epoch = Some(epoch);
                }
            }
        }
        match self.wake.try_send(()) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                anyhow::bail!("index worker is not running")
            }
        }
    }

    #[cfg(test)]
    fn pending_counts(&self) -> (usize, bool) {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (pending.paths.len(), pending.full_rebuild_epoch.is_some())
    }

    fn follow_up_after(&self, epoch: u64) -> Result<IndexFollowUp> {
        let pending = self.pending.lock().map_err(lock_err)?;
        Ok(IndexFollowUp {
            clear: pending
                .clear_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
            full_rebuild: pending
                .full_rebuild_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
            paths: pending
                .paths
                .iter()
                .filter_map(|(path, command_epoch)| {
                    (*command_epoch > epoch).then_some(path.clone())
                })
                .collect(),
            shutdown: pending
                .shutdown_epoch
                .is_some_and(|command_epoch| command_epoch > epoch),
        })
    }
}

impl IndexWorkReceiver {
    fn recv_debounced(&self) -> Result<IndexWorkBatch> {
        self.wake
            .recv()
            .map_err(|_| anyhow!("index ingress disconnected"))?;
        self.take_debounced()
    }

    fn recv_debounced_timeout(&self, timeout: Duration) -> Result<Option<IndexWorkBatch>> {
        match self.wake.recv_timeout(timeout) {
            Ok(()) => self.take_debounced().map(Some),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("index ingress disconnected"))
            }
        }
    }

    fn take_debounced(&self) -> Result<IndexWorkBatch> {
        loop {
            match self
                .wake
                .recv_timeout(Duration::from_millis(INDEX_BATCH_DEBOUNCE_MS))
            {
                Ok(()) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut pending = self.pending.lock().map_err(lock_err)?;
        let epoch = pending.next_epoch;
        Ok(IndexWorkBatch {
            clear_epoch: pending.clear_epoch.take(),
            clear_revision: pending.clear_revision.take(),
            full_rebuild_epoch: pending.full_rebuild_epoch.take(),
            paths: std::mem::take(&mut pending.paths),
            shutdown_epoch: pending.shutdown_epoch,
            epoch,
        })
    }

    #[cfg(test)]
    pub(crate) fn discard_until_shutdown(self) {
        while let Ok(batch) = self.recv_debounced() {
            if batch.shutdown_epoch.is_some() {
                break;
            }
        }
    }
}

pub(crate) fn build_index_status(runtime: &InteractiveIndexRuntime) -> DaemonIndexStatusResponse {
    let mut manifest = runtime.manifest.clone();
    if let Some(regex_runtime) = runtime.regex_runtime.as_ref() {
        apply_regex_manifest_status(&mut manifest, regex_runtime);
    }
    let dirty_file_count = runtime.manifest.dirty_paths.len();
    let queued_file_count = runtime.manifest.queued_paths.len();
    let ready = runtime.repo_is_current()
        && runtime.regex_is_current()
        && manifest.status == DaemonIndexState::Ready
        && manifest.dirty_paths.is_empty()
        && manifest.queued_paths.is_empty();
    DaemonIndexStatusResponse {
        manifest,
        ready,
        fallback_mode: !ready,
        loaded_generation: runtime
            .regex_runtime
            .as_ref()
            .and_then(|runtime| runtime.is_loaded().then_some(runtime.manifest.generation)),
        dirty_file_count,
        queued_file_count,
    }
}

pub(crate) fn enqueue_full_index_rebuild(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    record_index_work_after_clear(&guard.root)?;
    queue_full_index_rebuild_manifest_locked(&mut guard)?;
    guard.index_tx.send(IndexCommand::RebuildFull)
}

fn queue_full_index_rebuild_manifest_locked(guard: &mut DaemonState) -> Result<()> {
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Queued)?;
    guard.interactive_index.manifest.total_files = 0;
    guard.interactive_index.manifest.indexed_files = 0;
    guard.interactive_index.manifest.regex_status = Some("queued".to_string());
    guard.interactive_index.manifest.regex_total_files = 0;
    guard.interactive_index.manifest.regex_indexed_files = 0;
    guard.interactive_index.manifest.last_error = None;
    guard.interactive_index.manifest.regex_stale_reason = None;
    guard.interactive_index.manifest.queued_paths.clear();
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

pub(crate) fn enqueue_index_clear(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let revision = persist_index_clear_pending(&guard.root)?;
    queue_index_clear_locked(&mut guard, revision)
}

pub(crate) fn enqueue_persisted_index_clear(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let Some((revision, rebuild_after_clear)) = pending_index_clear(&guard.root) else {
        anyhow::bail!("persisted index clear is not pending");
    };
    queue_index_clear_locked(&mut guard, revision)?;
    if rebuild_after_clear {
        guard.index_tx.send(IndexCommand::RebuildFull)?;
    }
    Ok(())
}

pub(crate) fn enqueue_initial_index_work(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let (clear_pending, clear_complete, should_rebuild, external_regex_only) = {
        let guard = state.lock().map_err(lock_err)?;
        (
            index_clear_is_pending(&guard.root),
            index_clear_is_complete(&guard.root),
            guard.interactive_index.needs_rebuild(),
            guard.interactive_index.manifest.status == DaemonIndexState::Missing
                && guard.interactive_index.manifest.dirty_paths.is_empty()
                && guard.interactive_index.manifest.queued_paths.is_empty()
                && guard.interactive_index.regex_is_current()
                && !guard.interactive_index.repo_is_current(),
        )
    };
    if clear_pending {
        enqueue_persisted_index_clear(state)
    } else if !clear_complete && external_regex_only {
        rebuild_external_regex_generation_before_ready(state)
    } else if !clear_complete && should_rebuild {
        enqueue_full_index_rebuild(state)
    } else {
        Ok(())
    }
}

fn rebuild_external_regex_generation_before_ready(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    {
        let mut guard = state.lock().map_err(lock_err)?;
        record_index_work_after_clear(&guard.root)?;
        queue_full_index_rebuild_manifest_locked(&mut guard)?;
    }
    perform_full_index_rebuild(state, None, None)
        .context("failed to hydrate daemon indexes from an external regex generation")
}

fn queue_index_clear_locked(guard: &mut DaemonState, revision: u64) -> Result<()> {
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Queued)?;
    guard.interactive_index.manifest.dirty_paths.clear();
    guard.interactive_index.manifest.queued_paths.clear();
    guard.interactive_index.manifest.regex_status = Some("clear_pending".to_string());
    guard.interactive_index.manifest.regex_stale_reason = Some(if revision == 0 {
        "persisted index clear is pending".to_string()
    } else {
        format!("index clear revision {revision} is pending")
    });
    guard.interactive_index.manifest.last_error = None;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    guard.index_tx.send_clear(revision)
}

pub(crate) fn enqueue_incremental_index_paths(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
) -> Result<IndexQueueOutcome> {
    enqueue_incremental_index_paths_after_root_snapshot(state, paths, |_| Ok(()))
}

fn enqueue_incremental_index_paths_after_root_snapshot(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
    after_root_snapshot: impl FnOnce(&Path) -> Result<()>,
) -> Result<IndexQueueOutcome> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    after_root_snapshot(&root)?;
    let (normalized, input_requires_full, includes_root) = normalize_index_paths(&root, paths)?;
    if normalized.is_empty() && !input_requires_full && !includes_root {
        return Ok(IndexQueueOutcome {
            full: false,
            queued_paths: Vec::new(),
        });
    }
    let mut guard = state.lock().map_err(lock_err)?;
    record_index_work_after_clear(&guard.root)?;
    let additional_paths = normalized
        .iter()
        .filter(|path| {
            guard
                .interactive_index
                .manifest
                .queued_paths
                .binary_search(path)
                .is_err()
        })
        .count();
    let promote_to_full = input_requires_full
        || includes_root
        || guard
            .interactive_index
            .manifest
            .queued_paths
            .len()
            .saturating_add(additional_paths)
            > MAX_PENDING_INDEX_PATHS;
    if promote_to_full {
        guard
            .interactive_index
            .manifest
            .status
            .transition_to(DaemonIndexState::Queued)?;
        guard.interactive_index.manifest.total_files = 0;
        guard.interactive_index.manifest.indexed_files = 0;
        guard.interactive_index.manifest.regex_status = Some("queued".to_string());
        guard.interactive_index.manifest.regex_total_files = 0;
        guard.interactive_index.manifest.regex_indexed_files = 0;
        guard.interactive_index.manifest.last_error = None;
        guard.interactive_index.manifest.regex_stale_reason = None;
        guard.interactive_index.manifest.dirty_paths.clear();
        guard.interactive_index.manifest.queued_paths.clear();
    } else {
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
        if guard.interactive_index.manifest.status == DaemonIndexState::Missing {
            guard
                .interactive_index
                .manifest
                .status
                .transition_to(DaemonIndexState::Queued)?;
        }
    }
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    if promote_to_full {
        guard.index_tx.send(IndexCommand::RebuildFull)?;
        Ok(IndexQueueOutcome {
            full: true,
            queued_paths: Vec::new(),
        })
    } else {
        guard
            .index_tx
            .send(IndexCommand::ReindexPaths(normalized.clone()))?;
        Ok(IndexQueueOutcome {
            full: false,
            queued_paths: normalized,
        })
    }
}

fn normalize_index_paths(root: &Path, paths: &[String]) -> Result<(Vec<String>, bool, bool)> {
    if paths.len() > MAX_INDEX_PATH_INPUTS {
        return Ok((Vec::new(), true, false));
    }
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to canonicalize workspace root '{}'", root.display()))?;
    let mut normalized = BTreeSet::new();
    let mut requires_full = false;
    let mut includes_root = false;
    let mut inspected_bytes = 0usize;
    for path in paths {
        inspected_bytes = inspected_bytes.saturating_add(path.len());
        if inspected_bytes > MAX_INDEX_PATH_INPUT_BYTES {
            requires_full = true;
            break;
        }
        if path.len() > MAX_INDEX_PATH_BYTES {
            requires_full = true;
            break;
        }
        let Some(path) = normalize_index_path(&canonical_root, path)? else {
            includes_root |= !path.is_empty();
            continue;
        };
        if path.len() > MAX_INDEX_PATH_BYTES {
            requires_full = true;
            break;
        }
        normalized.insert(path);
        if normalized.len() > MAX_PENDING_INDEX_PATHS {
            requires_full = true;
            break;
        }
    }
    Ok((
        normalized.into_iter().collect(),
        requires_full,
        includes_root,
    ))
}

fn normalize_index_path(canonical_root: &Path, path: &str) -> Result<Option<String>> {
    if path.is_empty() {
        return Ok(None);
    }
    if path.trim() != path || path.contains('\n') || (cfg!(unix) && path.contains('\\')) {
        anyhow::bail!("index path '{path}' cannot be represented by the incremental index engines");
    }
    let candidate = Path::new(path);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        canonical_root.join(candidate)
    };
    let resolved = canonicalize_allow_missing(&absolute)?;
    let relative = resolved
        .strip_prefix(canonical_root)
        .map_err(|_| {
            anyhow!(
                "index path '{}' escapes workspace root '{}'",
                path,
                canonical_root.display()
            )
        })?
        .to_path_buf();
    if relative.as_os_str().is_empty() {
        return Ok(None);
    }
    let relative = relative
        .to_str()
        .ok_or_else(|| anyhow!("index path '{path}' resolves to a non-UTF-8 workspace path"))?;
    Ok(Some(if cfg!(windows) {
        relative.replace('\\', "/")
    } else {
        relative.to_string()
    }))
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut missing = Vec::<std::ffi::OsString>::new();
    let mut resolved_is_directory = true;
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if missing.pop().is_none() {
                    if !resolved_is_directory {
                        anyhow::bail!(
                            "index path '{}' traverses through a non-directory",
                            path.display()
                        );
                    }
                    resolved.pop();
                    resolved_is_directory = true;
                }
            }
            std::path::Component::Prefix(prefix) => {
                resolved.push(prefix.as_os_str());
                resolved_is_directory = true;
            }
            std::path::Component::RootDir => {
                resolved.push(std::path::MAIN_SEPARATOR.to_string());
                resolved_is_directory = true;
            }
            std::path::Component::Normal(component) if missing.is_empty() => {
                let candidate = resolved.join(component);
                match fs::symlink_metadata(&candidate) {
                    Ok(_) => {
                        resolved = fs::canonicalize(&candidate).with_context(|| {
                            format!("failed to resolve index path '{}'", path.display())
                        })?;
                        resolved_is_directory = resolved
                            .metadata()
                            .with_context(|| {
                                format!(
                                    "failed to inspect resolved index path '{}'",
                                    resolved.display()
                                )
                            })?
                            .is_dir();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing.push(component.to_os_string());
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect index path '{}'", candidate.display())
                        });
                    }
                }
            }
            std::path::Component::Normal(component) => {
                missing.push(component.to_os_string());
            }
        }
    }
    resolved.extend(missing);
    Ok(resolved)
}

pub(crate) fn run_index_worker(
    state: Arc<Mutex<DaemonState>>,
    index_rx: IndexWorkReceiver,
) -> Result<()> {
    run_index_worker_with_processor(state, index_rx, process_index_batch_with_recovery)
}

fn run_index_worker_with_processor(
    state: Arc<Mutex<DaemonState>>,
    index_rx: IndexWorkReceiver,
    process: impl FnMut(
        &Arc<Mutex<DaemonState>>,
        &IndexWorkBatch,
        Option<&crate::runtime::ShutdownSignal>,
    ) -> Result<IndexBatchStatus>,
) -> Result<()> {
    run_index_worker_with_processor_and_backoff(
        state,
        index_rx,
        IndexRetryBackoff::default(),
        process,
        |_| {},
    )
}

fn run_index_worker_with_processor_and_backoff(
    state: Arc<Mutex<DaemonState>>,
    index_rx: IndexWorkReceiver,
    mut retry_backoff: IndexRetryBackoff,
    mut process: impl FnMut(
        &Arc<Mutex<DaemonState>>,
        &IndexWorkBatch,
        Option<&crate::runtime::ShutdownSignal>,
    ) -> Result<IndexBatchStatus>,
    mut observe_retry_delay: impl FnMut(Duration),
) -> Result<()> {
    let shutdown = state.lock().map_err(lock_err)?.shutdown.clone();
    let mut next_batch = None;
    loop {
        let batch = match next_batch.take() {
            Some(batch) => batch,
            None => index_rx.recv_debounced()?,
        };
        if batch.shutdown_epoch.is_some() || shutdown.is_requested() {
            if let Some(clear_epoch) = batch.clear_epoch {
                perform_index_clear_for_batch(
                    &state,
                    clear_epoch,
                    batch.clear_revision,
                    batch.follow_up_after(clear_epoch),
                )
                .context("index clear failed during daemon shutdown")?;
            }
            return Ok(());
        }
        match process(&state, &batch, Some(&shutdown))? {
            IndexBatchStatus::Complete => retry_backoff.reset(),
            IndexBatchStatus::Retry(retry) => {
                let delay = retry_backoff.next_delay();
                observe_retry_delay(delay);
                daemon_log(&format!(
                    "index retry attempt {} delayed for {}ms",
                    retry_backoff.consecutive_failures,
                    delay.as_millis()
                ));
                match index_rx.recv_debounced_timeout(delay)? {
                    Some(explicit) => {
                        next_batch = Some(retry.merge_newer(explicit));
                    }
                    None => next_batch = Some(retry),
                }
            }
        }
    }
}

fn process_index_batch_with_recovery(
    state: &Arc<Mutex<DaemonState>>,
    batch: &IndexWorkBatch,
    shutdown: Option<&crate::runtime::ShutdownSignal>,
) -> Result<IndexBatchStatus> {
    if let Some(clear_epoch) = batch.clear_epoch {
        if let Err(error) = perform_index_clear_for_batch(
            state,
            clear_epoch,
            batch.clear_revision,
            batch.follow_up_after(clear_epoch),
        ) {
            daemon_log(&format!("index clear failed and will be retried: {error}"));
            reconcile_failed_index_clear(state, &error)?;
            return Ok(IndexBatchStatus::Retry(batch.clone()));
        }
    }
    let full_epoch = batch.full_rebuild_epoch;
    if let Some(full_epoch) = full_epoch {
        if let Err(error) = perform_full_index_rebuild_for_batch(
            state,
            shutdown,
            full_epoch,
            batch.follow_up_after(full_epoch),
        ) {
            daemon_log(&format!(
                "full index rebuild failed and will be retried: {error}"
            ));
            reconcile_failed_index_publication(state, &error)?;
            if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
                return Ok(IndexBatchStatus::Complete);
            }
            return Ok(IndexBatchStatus::Retry(batch.clone()));
        }
    }
    let paths = full_epoch.map_or_else(
        || batch.paths.keys().cloned().collect::<Vec<_>>(),
        |epoch| batch.paths_after(epoch),
    );
    if !paths.is_empty() {
        let path_epoch = paths
            .iter()
            .filter_map(|path| batch.paths.get(path))
            .copied()
            .max()
            .unwrap_or(batch.epoch);
        if let Err(error) = perform_incremental_index_update_for_batch(
            state,
            &paths,
            shutdown,
            path_epoch,
            batch.follow_up_after(path_epoch),
        ) {
            daemon_log(&format!(
                "incremental index update failed; forcing a reconciled full rebuild: {error}"
            ));
            reconcile_failed_index_publication(state, &error)?;
            if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
                return Ok(IndexBatchStatus::Complete);
            }
            return Ok(IndexBatchStatus::Retry(
                batch.clone().promote_retry_to_full(),
            ));
        }
    }
    Ok(IndexBatchStatus::Complete)
}

fn reconcile_failed_index_publication(
    state: &Arc<Mutex<DaemonState>>,
    error: &anyhow::Error,
) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let repo_runtime = mapy_core::load_repo_index_runtime(&root)
        .ok()
        .filter(mapy_core::RepoIndexRuntime::is_loaded);
    let regex_runtime = packet28_search_core::load_runtime(&root)
        .ok()
        .filter(packet28_search_core::RegexIndexRuntime::is_loaded);
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(repo_runtime) = repo_runtime {
        guard.interactive_index.repo_runtime = Some(repo_runtime);
    }
    if let Some(regex_runtime) = regex_runtime {
        guard.interactive_index.regex_runtime = Some(regex_runtime);
    }
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Queued)?;
    let recovery_reason = format!("index publication failed; queued full retry: {error}");
    guard.interactive_index.manifest.last_error = Some(recovery_reason.clone());
    guard.interactive_index.manifest.regex_stale_reason = Some(recovery_reason);
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

fn reconcile_failed_index_clear(
    state: &Arc<Mutex<DaemonState>>,
    error: &anyhow::Error,
) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let repo_runtime = mapy_core::load_repo_index_runtime(&root)
        .ok()
        .filter(mapy_core::RepoIndexRuntime::is_loaded);
    let regex_runtime = packet28_search_core::load_runtime(&root)
        .ok()
        .filter(packet28_search_core::RegexIndexRuntime::is_loaded);
    let mut guard = state.lock().map_err(lock_err)?;
    guard.interactive_index.repo_runtime = repo_runtime;
    guard.interactive_index.regex_runtime = regex_runtime;
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Queued)?;
    let reason = format!("index clear remains pending after partial failure: {error}");
    guard.interactive_index.manifest.last_error = Some(reason.clone());
    guard.interactive_index.manifest.regex_status = Some("clear_pending".to_string());
    guard.interactive_index.manifest.regex_stale_reason = Some(reason);
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

fn perform_full_index_rebuild(
    state: &Arc<Mutex<DaemonState>>,
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
) -> Result<()> {
    perform_full_index_rebuild_with_hooks(
        state,
        shutdown,
        batch_epoch,
        IndexFollowUp::default(),
        || Ok(()),
        || Ok(()),
    )
}

fn perform_full_index_rebuild_for_batch(
    state: &Arc<Mutex<DaemonState>>,
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: u64,
    follow_up: IndexFollowUp,
) -> Result<()> {
    perform_full_index_rebuild_with_hooks(
        state,
        shutdown,
        Some(batch_epoch),
        follow_up,
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(test)]
fn perform_full_index_rebuild_after_start(
    state: &Arc<Mutex<DaemonState>>,
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
    after_start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    perform_full_index_rebuild_with_hooks(
        state,
        shutdown,
        batch_epoch,
        IndexFollowUp::default(),
        after_start,
        || Ok(()),
    )
}

fn perform_full_index_rebuild_with_hooks(
    state: &Arc<Mutex<DaemonState>>,
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
    mut batch_follow_up: IndexFollowUp,
    after_start: impl FnOnce() -> Result<()>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return Ok(());
    }
    let root = {
        let mut guard = state.lock().map_err(lock_err)?;
        guard
            .interactive_index
            .manifest
            .status
            .transition_to(DaemonIndexState::Building)?;
        guard.interactive_index.manifest.total_files = 0;
        guard.interactive_index.manifest.indexed_files = 0;
        guard.interactive_index.manifest.regex_status = Some("queued".to_string());
        guard.interactive_index.manifest.regex_total_files = 0;
        guard.interactive_index.manifest.regex_indexed_files = 0;
        guard.interactive_index.manifest.last_build_started_at_unix = Some(now_unix());
        guard.interactive_index.manifest.last_error = None;
        guard.interactive_index.manifest.regex_stale_reason = None;
        guard.interactive_index.manifest.queued_paths.clear();
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        guard.root.clone()
    };
    after_start()?;
    #[cfg(feature = "shared-repository-scan")]
    let (repo_runtime, regex_runtime) = {
        mark_regex_build_started(state)?;
        let mut last_repo_progress = None::<(usize, std::time::Instant)>;
        let mut last_regex_progress = None::<(usize, std::time::Instant)>;
        let shared = match crate::shared_repository_scan::rebuild_full_indexes_with_shared_scan(
            &root,
            true,
            || shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested),
            |progress| match progress.engine {
                crate::shared_repository_scan::SharedScanEngine::Map => {
                    if should_persist_progress(
                        progress.completed,
                        progress.total,
                        &mut last_repo_progress,
                    ) {
                        let _ =
                            update_repo_build_progress(state, progress.completed, progress.total);
                    }
                }
                crate::shared_repository_scan::SharedScanEngine::Regex => {
                    if should_persist_progress(
                        progress.completed,
                        progress.total,
                        &mut last_regex_progress,
                    ) {
                        let _ = update_regex_build_progress(
                            state,
                            "building",
                            progress.completed,
                            progress.total,
                        );
                    }
                }
            },
        ) {
            Ok(shared) => shared,
            Err(crate::shared_repository_scan::SharedScanError::Cancelled) => {
                return requeue_interrupted_index_build(state);
            }
            Err(error) => return Err(anyhow!("failed to build shared indexes: {error}")),
        };
        daemon_log(&format!(
            "shared index scan walk_passes={} walked_entries={} metadata_calls={} reads={} bytes_read={} peak_buffer_bytes={} ignored_walk_errors={}",
            shared.telemetry.walk_passes,
            shared.telemetry.walked_entries,
            shared.telemetry.content_metadata_calls,
            shared.telemetry.successful_read_calls,
            shared.telemetry.bytes_read,
            shared.telemetry.peak_retained_content_bytes,
            shared.telemetry.ignored_walk_errors,
        ));
        (shared.repo, shared.regex)
    };
    #[cfg(not(feature = "shared-repository-scan"))]
    let mut last_repo_progress = None::<(usize, std::time::Instant)>;
    #[cfg(not(feature = "shared-repository-scan"))]
    let repo_runtime =
        mapy_core::rebuild_repo_index_runtime_with_progress(&root, true, |indexed, total| {
            if should_persist_progress(indexed, total, &mut last_repo_progress) {
                let _ = update_repo_build_progress(state, indexed, total);
            }
        })
        .map_err(|err| anyhow!("failed to build repo index: {err}"))?;
    #[cfg(not(feature = "shared-repository-scan"))]
    if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return requeue_interrupted_index_build(state);
    }
    #[cfg(not(feature = "shared-repository-scan"))]
    update_repo_build_progress(
        state,
        repo_runtime.manifest.total_files,
        repo_runtime.manifest.total_files,
    )?;
    #[cfg(not(feature = "shared-repository-scan"))]
    mark_regex_build_started(state)?;
    #[cfg(not(feature = "shared-repository-scan"))]
    let mut last_regex_progress = None::<(usize, std::time::Instant)>;
    #[cfg(not(feature = "shared-repository-scan"))]
    let regex_runtime =
        packet28_search_core::rebuild_full_index_with_progress(&root, true, |indexed, total| {
            if should_persist_progress(indexed, total, &mut last_regex_progress) {
                let _ = update_regex_build_progress(state, "building", indexed, total);
            }
        })
        .map_err(|err| anyhow!("failed to build regex search index: {err}"))?;
    if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return requeue_interrupted_index_build(state);
    }
    before_commit()?;
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(epoch) = batch_epoch {
        batch_follow_up.merge(guard.index_tx.follow_up_after(epoch)?);
    }
    if batch_follow_up.clear || batch_follow_up.shutdown {
        guard
            .interactive_index
            .manifest
            .status
            .transition_to(DaemonIndexState::Queued)?;
        guard.interactive_index.manifest.regex_status = Some(if batch_follow_up.clear {
            "clear_pending".to_string()
        } else {
            "queued".to_string()
        });
        guard.interactive_index.manifest.regex_stale_reason = Some(if batch_follow_up.clear {
            "newer index clear superseded this publication".to_string()
        } else {
            "daemon shutdown superseded this publication".to_string()
        });
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        return Ok(());
    }
    guard.interactive_index.repo_runtime = Some(repo_runtime.clone());
    guard.interactive_index.regex_runtime = Some(regex_runtime.clone());
    guard.interactive_index.manifest.generation = guard
        .interactive_index
        .manifest
        .generation
        .saturating_add(1);
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(if batch_follow_up.full_rebuild {
            DaemonIndexState::Queued
        } else {
            DaemonIndexState::Ready
        })?;
    guard.interactive_index.manifest.dirty_paths = batch_follow_up.paths.iter().cloned().collect();
    guard.interactive_index.manifest.queued_paths = batch_follow_up.paths.into_iter().collect();
    guard.interactive_index.manifest.total_files = repo_runtime.manifest.total_files;
    guard.interactive_index.manifest.indexed_files = repo_runtime.manifest.total_files;
    apply_regex_manifest_status(&mut guard.interactive_index.manifest, &regex_runtime);
    if batch_follow_up.full_rebuild {
        guard.interactive_index.manifest.regex_status = Some("queued".to_string());
    }
    guard
        .interactive_index
        .manifest
        .last_build_completed_at_unix = Some(now_unix());
    guard.interactive_index.manifest.last_error = None;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    Ok(())
}

#[cfg(test)]
fn perform_incremental_index_update(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
) -> Result<()> {
    perform_incremental_index_update_with_hooks(
        state,
        paths,
        shutdown,
        batch_epoch,
        IndexFollowUp::default(),
        || Ok(()),
        || Ok(()),
    )
}

fn perform_incremental_index_update_for_batch(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: u64,
    follow_up: IndexFollowUp,
) -> Result<()> {
    perform_incremental_index_update_with_hooks(
        state,
        paths,
        shutdown,
        Some(batch_epoch),
        follow_up,
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(test)]
fn perform_incremental_index_update_after_start(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
    after_start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    perform_incremental_index_update_with_hooks(
        state,
        paths,
        shutdown,
        batch_epoch,
        IndexFollowUp::default(),
        after_start,
        || Ok(()),
    )
}

fn perform_incremental_index_update_with_hooks(
    state: &Arc<Mutex<DaemonState>>,
    paths: &[String],
    shutdown: Option<&crate::runtime::ShutdownSignal>,
    batch_epoch: Option<u64>,
    mut batch_follow_up: IndexFollowUp,
    after_start: impl FnOnce() -> Result<()>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if paths.is_empty() || shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return Ok(());
    }
    let (root, repo_runtime_opt, regex_runtime_opt) = {
        let mut guard = state.lock().map_err(lock_err)?;
        if !guard.interactive_index.repo_is_current() || !guard.interactive_index.regex_is_current()
        {
            drop(guard);
            return perform_full_index_rebuild_with_hooks(
                state,
                shutdown,
                batch_epoch,
                batch_follow_up,
                || Ok(()),
                before_commit,
            );
        }
        guard
            .interactive_index
            .manifest
            .status
            .transition_to(DaemonIndexState::Building)?;
        guard.interactive_index.manifest.last_build_started_at_unix = Some(now_unix());
        guard.interactive_index.manifest.regex_stale_reason = None;
        for path in paths {
            insert_sorted_unique(
                &mut guard.interactive_index.manifest.dirty_paths,
                path.clone(),
            );
        }
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        (
            guard.root.clone(),
            guard.interactive_index.repo_runtime.clone(),
            guard.interactive_index.regex_runtime.clone(),
        )
    };
    after_start()?;
    let (Some(repo_runtime), Some(regex_runtime)) = (repo_runtime_opt, regex_runtime_opt) else {
        return perform_full_index_rebuild_with_hooks(
            state,
            shutdown,
            batch_epoch,
            batch_follow_up,
            || Ok(()),
            before_commit,
        );
    };
    let (repo_runtime, _summary) =
        mapy_core::update_repo_index_runtime(&root, &repo_runtime, paths, true)
            .map_err(|err| anyhow!("failed to update repo index: {err}"))?;
    if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return requeue_interrupted_index_build(state);
    }
    let regex_runtime =
        packet28_search_core::update_overlay_index(&root, Some(&regex_runtime), paths)
            .map_err(|err| anyhow!("failed to update regex search overlay: {err}"))?;
    if shutdown.is_some_and(crate::runtime::ShutdownSignal::is_requested) {
        return requeue_interrupted_index_build(state);
    }
    before_commit()?;
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(epoch) = batch_epoch {
        batch_follow_up.merge(guard.index_tx.follow_up_after(epoch)?);
    }
    if batch_follow_up.clear || batch_follow_up.shutdown {
        guard
            .interactive_index
            .manifest
            .status
            .transition_to(DaemonIndexState::Queued)?;
        guard.interactive_index.manifest.regex_status = Some(if batch_follow_up.clear {
            "clear_pending".to_string()
        } else {
            "queued".to_string()
        });
        guard.interactive_index.manifest.regex_stale_reason = Some(if batch_follow_up.clear {
            "newer index clear superseded this incremental publication".to_string()
        } else {
            "daemon shutdown superseded this incremental publication".to_string()
        });
        save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
        return Ok(());
    }
    guard.interactive_index.repo_runtime = Some(repo_runtime.clone());
    guard.interactive_index.regex_runtime = Some(regex_runtime.clone());
    guard.interactive_index.manifest.generation = guard
        .interactive_index
        .manifest
        .generation
        .saturating_add(1);
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(if batch_follow_up.full_rebuild {
            DaemonIndexState::Queued
        } else {
            DaemonIndexState::Ready
        })?;
    for path in paths {
        if batch_follow_up.paths.contains(path) {
            continue;
        }
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
    guard.interactive_index.manifest.total_files = repo_runtime.manifest.total_files;
    guard.interactive_index.manifest.indexed_files = repo_runtime.manifest.total_files;
    apply_regex_manifest_status(&mut guard.interactive_index.manifest, &regex_runtime);
    if batch_follow_up.full_rebuild {
        guard.interactive_index.manifest.regex_status = Some("queued".to_string());
    }
    guard
        .interactive_index
        .manifest
        .last_build_completed_at_unix = Some(now_unix());
    guard.interactive_index.manifest.last_error = None;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)?;
    Ok(())
}

fn requeue_interrupted_index_build(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Queued)?;
    guard.interactive_index.manifest.regex_status = Some("queued".to_string());
    guard.interactive_index.manifest.regex_stale_reason =
        Some("daemon shutdown interrupted index publication".to_string());
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

#[cfg(test)]
fn perform_index_clear(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let revision = persist_index_clear_pending(&root)?;
    perform_index_clear_with_hook(
        state,
        None,
        Some(revision),
        IndexFollowUp::default(),
        || Ok(()),
    )
}

fn perform_index_clear_for_batch(
    state: &Arc<Mutex<DaemonState>>,
    clear_epoch: u64,
    clear_revision: Option<u64>,
    follow_up: IndexFollowUp,
) -> Result<()> {
    perform_index_clear_with_hook(state, Some(clear_epoch), clear_revision, follow_up, || {
        Ok(())
    })
}

fn perform_index_clear_with_hook(
    state: &Arc<Mutex<DaemonState>>,
    clear_epoch: Option<u64>,
    clear_revision: Option<u64>,
    mut batch_follow_up: IndexFollowUp,
    after_repo_clear: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    clear_index_files(&root)?;
    after_repo_clear()?;
    clear_regex_index_files(&root)?;
    let clear_revision = clear_revision
        .or_else(|| pending_index_clear(&root).map(|(revision, _)| revision))
        .unwrap_or(1);
    let completed_current_revision = complete_index_clear_revision(&root, clear_revision)?;

    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(epoch) = clear_epoch {
        batch_follow_up.merge(guard.index_tx.follow_up_after(epoch)?);
    }
    let mut manifest = default_index_manifest(&guard.root);
    let has_later_work = !completed_current_revision
        || batch_follow_up.clear
        || batch_follow_up.full_rebuild
        || !batch_follow_up.paths.is_empty();
    if has_later_work {
        manifest.status.transition_to(DaemonIndexState::Queued)?;
        manifest.dirty_paths = batch_follow_up.paths.iter().cloned().collect();
        manifest.queued_paths = batch_follow_up.paths.into_iter().collect();
        manifest.regex_status = Some(if batch_follow_up.clear || !completed_current_revision {
            "clear_pending".to_string()
        } else {
            "queued".to_string()
        });
        manifest.regex_stale_reason = (batch_follow_up.clear || !completed_current_revision)
            .then(|| "newer index clear remains pending".to_string());
    }
    guard.interactive_index = InteractiveIndexRuntime {
        manifest,
        repo_runtime: None,
        regex_runtime: None,
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
    let outcome = if request.full || request.paths.is_empty() {
        enqueue_full_index_rebuild(&state)?;
        IndexQueueOutcome {
            full: true,
            queued_paths: Vec::new(),
        }
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
        full: outcome.full,
        generation: Some(generation),
        queued_paths: outcome.queued_paths,
    })
}

pub(crate) fn daemon_index_clear(
    state: Arc<Mutex<DaemonState>>,
) -> Result<DaemonIndexClearResponse> {
    enqueue_index_clear(&state)?;
    Ok(DaemonIndexClearResponse { cleared: true })
}

pub(crate) fn daemon_packet28_search(
    state: Arc<Mutex<DaemonState>>,
    request: packet28_daemon_protocol::message::Packet28SearchRequest,
) -> Result<packet28_reducer_core::SearchResult> {
    let packet28_daemon_protocol::message::Packet28SearchRequest {
        request,
        force_indexed,
    } = request;
    let (root, runtime, manifest) = {
        let guard = state.lock().map_err(lock_err)?;
        (
            guard.root.clone(),
            guard.interactive_index.regex_runtime.clone(),
            guard.interactive_index.manifest.clone(),
        )
    };
    let request = match normalize_daemon_search_request(&root, &request) {
        Ok(request) => request,
        Err(error) => {
            let reason = format!("requested path scope is not indexable: {error}");
            if force_indexed {
                return Err(DaemonIndexSearchNotReady { reason }.into());
            }
            return live_search_with_reason(&root, &request, reason);
        }
    };
    let daemon_fallback_reason = daemon_manifest_search_fallback_reason(&manifest, &request);
    if let Some(reason) = daemon_fallback_reason {
        if force_indexed {
            return Err(DaemonIndexSearchNotReady { reason }.into());
        }
        return live_search_with_reason(&root, &request, reason);
    }
    let Some(runtime) = runtime else {
        let reason = "regex search index is not ready".to_string();
        if force_indexed {
            return Err(DaemonIndexSearchNotReady { reason }.into());
        }
        return live_search_with_reason(&root, &request, reason);
    };
    match packet28_search_core::guarded_indexed_search(&root, &runtime, &request) {
        Ok(result) => Ok(result),
        Err(packet28_search_core::SearchError::IndexNotReady { reason }) => {
            if force_indexed {
                return Err(DaemonIndexSearchNotReady { reason }.into());
            }
            live_search_with_reason(&root, &request, reason)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn daemon_packet28_search_guard(
    state: Arc<Mutex<DaemonState>>,
    request: packet28_daemon_protocol::message::Packet28SearchRequest,
) -> Result<packet28_daemon_protocol::message::Packet28SearchGuardResponse> {
    let packet28_daemon_protocol::message::Packet28SearchRequest {
        request,
        force_indexed: _,
    } = request;
    let (root, runtime, manifest) = {
        let guard = state.lock().map_err(lock_err)?;
        (
            guard.root.clone(),
            guard.interactive_index.regex_runtime.clone(),
            guard.interactive_index.manifest.clone(),
        )
    };
    let request = match normalize_daemon_search_request(&root, &request) {
        Ok(request) => request,
        Err(error) => {
            return Ok(
                packet28_daemon_protocol::message::Packet28SearchGuardResponse {
                    fallback_reason: Some(format!(
                        "requested path scope is not indexable: {error}"
                    )),
                },
            );
        }
    };
    let daemon_fallback_reason = daemon_manifest_search_fallback_reason(&manifest, &request);
    let fallback_reason = match daemon_fallback_reason {
        Some(reason) => Some(reason),
        None => match runtime {
            Some(runtime) => {
                packet28_search_core::guarded_fallback_reason(&root, &runtime, &request)?
            }
            None => Some("regex search index is not ready".to_string()),
        },
    };
    Ok(packet28_daemon_protocol::message::Packet28SearchGuardResponse { fallback_reason })
}

fn live_search_with_reason(
    root: &Path,
    request: &packet28_reducer_core::SearchRequest,
    reason: String,
) -> Result<packet28_reducer_core::SearchResult> {
    let mut fallback = packet28_reducer_core::search(root, request)?;
    if let Some(engine) = fallback.engine.as_mut() {
        engine.fallback_reason = Some(reason);
    }
    Ok(fallback)
}

fn normalize_daemon_search_request(
    root: &Path,
    request: &packet28_reducer_core::SearchRequest,
) -> Result<packet28_reducer_core::SearchRequest> {
    if request.requested_paths.is_empty() {
        return Ok(request.clone());
    }
    let (requested_paths, exceeded_limit, includes_root) =
        normalize_index_paths(root, &request.requested_paths)?;
    if exceeded_limit {
        anyhow::bail!(
            "requested path count or size exceeds the daemon limit of \
             {MAX_INDEX_PATH_INPUTS} inputs and {MAX_INDEX_PATH_BYTES} bytes per path"
        );
    }
    if requested_paths.is_empty() && !includes_root {
        anyhow::bail!("requested path scope contains no usable paths");
    }
    let mut normalized = request.clone();
    normalized.requested_paths = if includes_root {
        Vec::new()
    } else {
        requested_paths
    };
    Ok(normalized)
}

fn daemon_manifest_search_fallback_reason(
    manifest: &DaemonIndexManifest,
    request: &packet28_reducer_core::SearchRequest,
) -> Option<String> {
    if manifest.status != DaemonIndexState::Ready {
        return Some(format!(
            "daemon index manifest is {:?}; indexed search is not current",
            manifest.status
        ));
    }
    let pending_paths = manifest
        .dirty_paths
        .iter()
        .chain(&manifest.queued_paths)
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if pending_paths.is_empty() {
        return None;
    }
    let pending_paths = pending_paths.into_iter().collect::<Vec<_>>();
    let relevant = request.requested_paths.is_empty()
        || request
            .requested_paths
            .iter()
            .any(|requested| sorted_paths_overlap(&pending_paths, requested));
    relevant.then(|| {
        format!(
            "daemon index has {} queued or dirty path(s) relevant to this search",
            pending_paths.len()
        )
    })
}

fn sorted_paths_overlap(sorted_paths: &[&str], requested: &str) -> bool {
    if requested.is_empty() || requested == "." {
        return true;
    }
    let mut ancestor = requested;
    loop {
        if sorted_paths.binary_search(&ancestor).is_ok() {
            return true;
        }
        let Some(separator) = ancestor.rfind('/') else {
            break;
        };
        ancestor = &ancestor[..separator];
        if ancestor.is_empty() {
            break;
        }
    }
    let descendant_prefix = format!("{requested}/");
    let descendant = sorted_paths.partition_point(|path| *path < descendant_prefix.as_str());
    sorted_paths
        .get(descendant)
        .is_some_and(|path| paths_overlap(requested, path))
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('/');
    let right = right.trim_end_matches('/');
    if left.is_empty() || right.is_empty() || left == "." || right == "." {
        return true;
    }
    left == right
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn apply_regex_manifest_status(
    manifest: &mut DaemonIndexManifest,
    runtime: &packet28_search_core::RegexIndexRuntime,
) {
    manifest.regex_generation = Some(runtime.manifest.generation);
    manifest.regex_status = Some(runtime.manifest.status.clone());
    manifest.regex_total_files = runtime.manifest.total_files;
    manifest.regex_base_commit = runtime.manifest.base_commit.clone();
    manifest.regex_weight_table_version = Some(runtime.manifest.weight_table_version);
    manifest.regex_stale_reason = runtime.manifest.stale_reason.clone();
    manifest.regex_indexed_files = runtime.manifest.indexed_files;
}

fn should_persist_progress(
    indexed: usize,
    total: usize,
    checkpoint: &mut Option<(usize, std::time::Instant)>,
) -> bool {
    let now = std::time::Instant::now();
    let should_emit = match checkpoint {
        None => true,
        Some((last_indexed, last_at)) => {
            indexed == total
                || indexed == 0
                || indexed >= last_indexed.saturating_add(32)
                || now.duration_since(*last_at) >= std::time::Duration::from_millis(250)
        }
    };
    if should_emit {
        *checkpoint = Some((indexed, now));
    }
    should_emit
}

fn update_repo_build_progress(
    state: &Arc<Mutex<DaemonState>>,
    indexed_files: usize,
    total_files: usize,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard
        .interactive_index
        .manifest
        .status
        .transition_to(DaemonIndexState::Building)?;
    guard.interactive_index.manifest.total_files = total_files;
    guard.interactive_index.manifest.indexed_files = indexed_files.min(total_files);
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

fn mark_regex_build_started(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard.interactive_index.manifest.regex_status = Some("building".to_string());
    guard.interactive_index.manifest.regex_total_files = 0;
    guard.interactive_index.manifest.regex_indexed_files = 0;
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

fn update_regex_build_progress(
    state: &Arc<Mutex<DaemonState>>,
    status: &str,
    indexed_files: usize,
    total_files: usize,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard.interactive_index.manifest.regex_status = Some(status.to_string());
    guard.interactive_index.manifest.regex_total_files = total_files;
    guard.interactive_index.manifest.regex_indexed_files = indexed_files.min(total_files);
    save_index_manifest_file(&guard.root, &guard.interactive_index.manifest)
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
