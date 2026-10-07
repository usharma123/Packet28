use super::*;
use crate::broker::emit_task_event_for_generation;
use packet28_daemon_protocol::registry::RegistryRevisionV1;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Condvar;

pub(crate) struct WatchEventMsg {
    pub(crate) watch_id: String,
    pub(crate) generation: TaskGenerationId,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) error: Option<String>,
    pub(crate) overflowed: bool,
}

pub(crate) struct PendingWatchEvent {
    pub(crate) watch_id: String,
    pub(crate) generation: TaskGenerationId,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) error: Option<String>,
    pub(crate) overflowed: bool,
    pub(crate) due_at: tokio::time::Instant,
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "retained for test-only parity coverage of the superseded direct-evidence cache"
    )
)]
pub(crate) struct CachedSourceFile {
    pub(crate) size: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct InteractiveIndexRuntime {
    pub(crate) manifest: DaemonIndexManifest,
    pub(crate) repo_runtime: Option<mapy_core::RepoIndexRuntime>,
    pub(crate) regex_runtime: Option<packet28_search_core::RegexIndexRuntime>,
}

impl InteractiveIndexRuntime {
    pub(crate) fn repo_is_current(&self) -> bool {
        self.repo_runtime.as_ref().is_some_and(|runtime| {
            runtime.is_loaded()
                && runtime.manifest.status == "ready"
                && runtime.manifest.recovered_from_generation.is_none()
                && runtime.manifest.last_error.is_none()
        })
    }

    pub(crate) fn regex_is_current(&self) -> bool {
        self.regex_runtime.as_ref().is_some_and(|runtime| {
            runtime.is_loaded()
                && runtime.manifest.status == "ready"
                && runtime.manifest.stale_reason.is_none()
                && runtime.manifest.last_error.is_none()
        })
    }

    pub(crate) fn repo_is_query_ready(&self) -> bool {
        self.repo_is_current()
            && self.manifest.status == DaemonIndexState::Ready
            && self.manifest.dirty_paths.is_empty()
            && self.manifest.queued_paths.is_empty()
    }

    pub(crate) fn regex_is_query_ready(&self) -> bool {
        self.regex_is_current()
            && self.manifest.status == DaemonIndexState::Ready
            && self.manifest.dirty_paths.is_empty()
            && self.manifest.queued_paths.is_empty()
    }

    pub(crate) fn needs_rebuild(&self) -> bool {
        !self.repo_is_current()
            || !self.regex_is_current()
            || self.manifest.status != DaemonIndexState::Ready
            || !self.manifest.dirty_paths.is_empty()
            || !self.manifest.queued_paths.is_empty()
    }
}

pub(crate) enum IndexCommand {
    RebuildFull,
    ReindexPaths(Vec<String>),
    Clear,
    Shutdown,
}

pub(crate) enum BackgroundCommand {
    RelaunchAgent {
        task_id: String,
        command: Vec<String>,
    },
    RunRecoveredReplan {
        task_id: String,
        generation: TaskGenerationId,
    },
}

pub(crate) struct TaskSubscriber {
    pub(crate) id: u64,
    pub(crate) sender: tokio::sync::mpsc::Sender<DaemonEventFrame>,
}

#[derive(Debug, Default)]
pub(crate) struct RegistryPageIndex {
    pub(crate) revision: RegistryRevisionV1,
    pub(crate) watch_positions: BTreeMap<String, usize>,
    pub(crate) watch_ids_by_task: BTreeMap<String, BTreeSet<String>>,
}

pub(crate) struct DaemonState {
    pub(crate) root: PathBuf,
    pub(crate) kernel: Arc<Kernel>,
    pub(crate) kernel_registry: Arc<crate::kernel_registry::PersistentKernelRegistry>,
    pub(crate) runtime: DaemonRuntimeInfo,
    pub(crate) tasks: TaskRegistry,
    pub(crate) task_generations: TaskGenerationRegistry,
    pub(crate) agent_snapshots: BTreeMap<String, suite_packet_core::AgentSnapshotPayload>,
    pub(crate) watches: WatchRegistry,
    pub(crate) registry_instance_id: String,
    pub(crate) registry_page_index: Option<RegistryPageIndex>,
    pub(crate) watcher_handles: HashMap<String, PollWatcher>,
    pub(crate) subscribers: HashMap<String, Vec<TaskSubscriber>>,
    pub(crate) source_file_cache: BTreeMap<String, CachedSourceFile>,
    pub(crate) interactive_index: InteractiveIndexRuntime,
    pub(crate) index_tx: crate::index::IndexIngress,
    /// Serializes index request admission; see `index::with_index_admission`.
    pub(crate) index_admission: Arc<std::sync::Mutex<()>>,
    pub(crate) background_tx: tokio::sync::mpsc::Sender<BackgroundCommand>,
    pub(crate) persistence: crate::persistence::PersistenceHandle,
    #[cfg(test)]
    pub(crate) _persistence_owner: Option<crate::persistence::PersistenceOwner>,
    pub(crate) shutdown: crate::runtime::ShutdownSignal,
    pub(crate) changes: crate::runtime::StateChangeSignal,
    pub(crate) shutting_down: bool,
    pub(crate) task_maintenance: crate::task_maintenance::TaskMaintenance,
    pub(crate) record_sizes: crate::task_maintenance::RecordSizeIndex,
}

impl DaemonState {
    pub(crate) fn registry_revision(&self) -> RegistryRevisionV1 {
        RegistryRevisionV1 {
            instance_id: self.registry_instance_id.clone(),
            revision: self.persistence.latest_revision(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TaskGenerationId(u64);

#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnedChildProcess {
    pub(crate) pid: u32,
    pub(crate) process_group: i32,
}

#[derive(Debug, Default)]
struct TaskGenerationActivityState {
    active_operations: usize,
    child_launch_in_progress: bool,
    children: HashMap<u32, OwnedChildProcess>,
}

#[derive(Debug, Default)]
struct TaskGenerationActivity {
    state: Mutex<TaskGenerationActivityState>,
    changed: Condvar,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskGenerationToken {
    id: TaskGenerationId,
    cancelled: Arc<AtomicBool>,
    activity: Arc<TaskGenerationActivity>,
}

impl TaskGenerationToken {
    pub(crate) fn id(&self) -> TaskGenerationId {
        self.id
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Acquire)
    }

    pub(crate) fn request_cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::Release);
        self.activity.changed.notify_all();
    }

    pub(crate) fn acquire_operation(&self) -> Option<TaskActivityLease> {
        if self.is_cancelled() {
            return None;
        }
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_cancelled() {
            return None;
        }
        activity.active_operations = activity.active_operations.saturating_add(1);
        Some(TaskActivityLease {
            activity: self.activity.clone(),
        })
    }

    pub(crate) fn acquire_child_launch(&self) -> Option<TaskChildLaunchLease> {
        if self.is_cancelled() {
            return None;
        }
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_cancelled() || activity.child_launch_in_progress || !activity.children.is_empty()
        {
            return None;
        }
        activity.child_launch_in_progress = true;
        Some(TaskChildLaunchLease {
            activity: self.activity.clone(),
        })
    }

    pub(crate) fn register_child(&self, child: OwnedChildProcess) -> bool {
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_cancelled() {
            return false;
        }
        activity.children.insert(child.pid, child);
        true
    }

    pub(crate) fn complete_child(&self, pid: u32) {
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        activity.children.remove(&pid);
        self.activity.changed.notify_all();
    }

    pub(crate) fn children(&self) -> Vec<OwnedChildProcess> {
        self.activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .children
            .values()
            .copied()
            .collect()
    }

    /// Returns whether no operation, child launch, or child process is active.
    pub(crate) fn is_idle(&self) -> bool {
        let activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        activity.active_operations == 0
            && !activity.child_launch_in_progress
            && activity.children.is_empty()
    }

    pub(crate) fn wait_for_children(&self, timeout: Duration) -> bool {
        self.wait_for_activity(timeout, |activity| activity.children.is_empty())
    }

    pub(crate) fn wait_until_idle(&self, timeout: Duration) -> bool {
        self.wait_for_activity(timeout, |activity| {
            activity.active_operations == 0 && activity.children.is_empty()
        })
    }

    fn wait_for_activity(
        &self,
        timeout: Duration,
        is_complete: impl Fn(&TaskGenerationActivityState) -> bool,
    ) -> bool {
        let started = Instant::now();
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !is_complete(&activity) {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return false;
            };
            let (next, wait_result) = self
                .activity
                .changed
                .wait_timeout(activity, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            activity = next;
            if wait_result.timed_out() && !is_complete(&activity) {
                return false;
            }
        }
        true
    }
}

pub(crate) struct TaskActivityLease {
    activity: Arc<TaskGenerationActivity>,
}

pub(crate) struct TaskChildLaunchLease {
    activity: Arc<TaskGenerationActivity>,
}

impl Drop for TaskChildLaunchLease {
    fn drop(&mut self) {
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        activity.child_launch_in_progress = false;
        self.activity.changed.notify_all();
    }
}

impl Drop for TaskActivityLease {
    fn drop(&mut self) {
        let mut activity = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        activity.active_operations = activity.active_operations.saturating_sub(1);
        self.activity.changed.notify_all();
    }
}

#[derive(Debug, Default)]
pub(crate) struct TaskGenerationRegistry {
    next_id: u64,
    active: HashMap<String, TaskGenerationToken>,
}

impl TaskGenerationRegistry {
    pub(crate) fn create(&mut self, task_id: &str) -> Result<TaskGenerationToken> {
        if self.active.contains_key(task_id) {
            anyhow::bail!("task '{task_id}' already has an active generation");
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task generation id exhausted"))?;
        let token = TaskGenerationToken {
            id: TaskGenerationId(self.next_id),
            cancelled: Arc::new(AtomicBool::new(false)),
            activity: Arc::new(TaskGenerationActivity::default()),
        };
        self.active.insert(task_id.to_string(), token.clone());
        Ok(token)
    }

    pub(crate) fn ensure(&mut self, task_id: &str) -> Result<TaskGenerationToken> {
        if let Some(token) = self.active.get(task_id) {
            return Ok(token.clone());
        }
        self.create(task_id)
    }

    pub(crate) fn current(&self, task_id: &str) -> Option<TaskGenerationToken> {
        self.active.get(task_id).cloned()
    }

    pub(crate) fn matches(&self, task_id: &str, generation: TaskGenerationId) -> bool {
        self.active
            .get(task_id)
            .is_some_and(|token| token.id == generation)
    }

    pub(crate) fn remove_if_current(
        &mut self,
        task_id: &str,
        generation: TaskGenerationId,
    ) -> bool {
        if !self.matches(task_id, generation) {
            return false;
        }
        self.active.remove(task_id);
        true
    }

    pub(crate) fn request_cancel_all(&self) -> Vec<TaskGenerationToken> {
        let generations = self.active.values().cloned().collect::<Vec<_>>();
        for token in &generations {
            token.request_cancel();
        }
        generations
    }
}

pub(crate) struct TaskSequenceObserver {
    pub(crate) state: Arc<Mutex<DaemonState>>,
    pub(crate) task_id: String,
    pub(crate) generation: TaskGenerationToken,
}

impl SequenceObserver for TaskSequenceObserver {
    fn should_cancel(&self) -> bool {
        self.generation.is_cancelled()
    }

    fn on_step_started(&mut self, position: usize, step: &KernelStepRequest) {
        let _ = emit_task_event_for_generation(
            self.state.clone(),
            &self.task_id,
            self.generation.id(),
            "step_started",
            json!({
                "task_id": self.task_id,
                "step_id": step.id,
                "target": step.target,
                "position": position,
            }),
        );
    }

    fn on_step_completed(
        &mut self,
        position: usize,
        step: &KernelStepRequest,
        response: &KernelResponse,
    ) {
        let _ = emit_task_event_for_generation(
            self.state.clone(),
            &self.task_id,
            self.generation.id(),
            "step_completed",
            json!({
                "task_id": self.task_id,
                "step_id": step.id,
                "target": step.target,
                "position": position,
                "request_id": response.request_id,
            }),
        );
    }

    fn on_step_failed(
        &mut self,
        position: usize,
        step: &KernelStepRequest,
        failure: &KernelFailure,
    ) {
        let _ = emit_task_event_for_generation(
            self.state.clone(),
            &self.task_id,
            self.generation.id(),
            "step_failed",
            json!({
                "task_id": self.task_id,
                "step_id": step.id,
                "target": step.target,
                "position": position,
                "failure": failure,
            }),
        );
    }

    fn on_replan_applied(
        &mut self,
        after_step: Option<&str>,
        event_count: usize,
        applied_mutations: &Value,
    ) {
        let _ = emit_task_event_for_generation(
            self.state.clone(),
            &self.task_id,
            self.generation.id(),
            "replan_applied",
            json!({
                "task_id": self.task_id,
                "after_step": after_step,
                "event_count": event_count,
                "mutation_summary": applied_mutations,
            }),
        );
    }
}
