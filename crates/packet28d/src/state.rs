use super::*;

pub(crate) struct WatchEventMsg {
    pub(crate) watch_id: String,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) error: Option<String>,
}

pub(crate) struct PendingWatchEvent {
    pub(crate) watch_id: String,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) error: Option<String>,
    pub(crate) due_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CachedSourceFile {
    pub(crate) size: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct InteractiveIndexRuntime {
    pub(crate) manifest: DaemonIndexManifest,
    pub(crate) snapshot: Option<Arc<mapy_core::RepoIndexSnapshot>>,
}

pub(crate) enum IndexCommand {
    RebuildFull,
    ReindexPaths(Vec<String>),
    Clear,
    Shutdown,
}

pub(crate) struct DaemonState {
    pub(crate) root: PathBuf,
    pub(crate) kernel: Arc<Kernel>,
    pub(crate) runtime: DaemonRuntimeInfo,
    pub(crate) tasks: TaskRegistry,
    pub(crate) agent_snapshots: BTreeMap<String, suite_packet_core::AgentSnapshotPayload>,
    pub(crate) watches: WatchRegistry,
    pub(crate) watcher_handles: HashMap<String, PollWatcher>,
    pub(crate) subscribers: HashMap<String, Vec<Sender<DaemonEventFrame>>>,
    pub(crate) source_file_cache: BTreeMap<String, CachedSourceFile>,
    pub(crate) interactive_index: InteractiveIndexRuntime,
    pub(crate) index_tx: Sender<IndexCommand>,
    pub(crate) shutting_down: bool,
}

pub(crate) struct TaskSequenceObserver {
    pub(crate) state: Arc<Mutex<DaemonState>>,
    pub(crate) task_id: String,
}

impl SequenceObserver for TaskSequenceObserver {
    fn on_step_started(&mut self, position: usize, step: &KernelStepRequest) {
        let _ = emit_task_event(
            self.state.clone(),
            &self.task_id,
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
        let _ = emit_task_event(
            self.state.clone(),
            &self.task_id,
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
        let _ = emit_task_event(
            self.state.clone(),
            &self.task_id,
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
        let _ = emit_task_event(
            self.state.clone(),
            &self.task_id,
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
