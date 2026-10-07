use super::*;
use crate::broker::{
    complete_task_cancellation_for_generation, emit_task_event_for_generation,
    refresh_broker_context_for_task, refresh_task_context_summary_for_generation,
    set_context_reason_for_generation,
};
use std::collections::HashSet;

const TASK_CANCELLATION_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WATCH_OVERFLOW_KEYS: usize = 128;
const MAX_WATCH_OVERFLOW_PATHS_PER_KEY: usize = 256;
const MAX_PENDING_WATCH_KEYS: usize = 128;
const MAX_PENDING_WATCH_PATHS: usize = 256;

#[derive(Clone)]
pub(crate) struct WatchIngress {
    sender: tokio::sync::mpsc::Sender<WatchEventMsg>,
    overflowed: Arc<Mutex<WatchOverflowState>>,
    overflow_ready: Arc<tokio::sync::Notify>,
    overflow_key_capacity: usize,
    overflow_path_capacity: usize,
}

#[derive(Default)]
struct WatchOverflowState {
    messages: HashMap<(String, TaskGenerationId), WatchOverflowEvent>,
    global_rescan: bool,
}

struct WatchOverflowEvent {
    watch_id: String,
    generation: TaskGenerationId,
    paths: HashSet<PathBuf>,
    error: Option<String>,
}

struct WatchOverflowBatch {
    messages: Vec<WatchEventMsg>,
    global_rescan: bool,
}

#[derive(Default)]
struct GlobalWatchSweep {
    after_watch_id: Option<String>,
}

pub(crate) struct TaskAdmission {
    pub(crate) task: TaskRecord,
    pub(crate) watches: Vec<WatchRegistration>,
    pub(crate) generation: TaskGenerationId,
    pub(crate) replaced_task: Option<TaskRecord>,
}

#[derive(Clone, Copy)]
enum TaskRunFence {
    Unfenced,
    Initial(TaskGenerationId),
    RecoveredReplan(TaskGenerationId),
}

impl TaskRunFence {
    const fn expected_generation(self) -> Option<TaskGenerationId> {
        match self {
            Self::Unfenced => None,
            Self::Initial(generation) | Self::RecoveredReplan(generation) => Some(generation),
        }
    }

    const fn is_fenced(self) -> bool {
        !matches!(self, Self::Unfenced)
    }
}

impl WatchIngress {
    pub(crate) fn new(capacity: usize) -> (Self, tokio::sync::mpsc::Receiver<WatchEventMsg>) {
        Self::with_overflow_limits(
            capacity,
            capacity.clamp(1, MAX_WATCH_OVERFLOW_KEYS),
            MAX_WATCH_OVERFLOW_PATHS_PER_KEY,
        )
    }

    fn with_overflow_limits(
        capacity: usize,
        overflow_key_capacity: usize,
        overflow_path_capacity: usize,
    ) -> (Self, tokio::sync::mpsc::Receiver<WatchEventMsg>) {
        assert!(capacity > 0, "watch ingress capacity must be nonzero");
        assert!(
            overflow_key_capacity > 0,
            "watch overflow key capacity must be nonzero"
        );
        assert!(
            overflow_path_capacity > 0,
            "watch overflow path capacity must be nonzero"
        );
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (
            Self {
                sender,
                overflowed: Arc::new(Mutex::new(WatchOverflowState::default())),
                overflow_ready: Arc::new(tokio::sync::Notify::new()),
                overflow_key_capacity,
                overflow_path_capacity,
            },
            receiver,
        )
    }

    pub(crate) fn send(&self, message: WatchEventMsg) {
        match self.sender.try_send(message) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(message)) => {
                let key = (message.watch_id.clone(), message.generation);
                let mut overflowed = self
                    .overflowed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if overflowed.global_rescan {
                    drop(overflowed);
                    self.overflow_ready.notify_one();
                    return;
                }
                if !overflowed.messages.contains_key(&key)
                    && overflowed.messages.len() >= self.overflow_key_capacity
                {
                    overflowed.messages.clear();
                    overflowed.global_rescan = true;
                    drop(overflowed);
                    self.overflow_ready.notify_one();
                    return;
                }
                let pending =
                    overflowed
                        .messages
                        .entry(key)
                        .or_insert_with(|| WatchOverflowEvent {
                            watch_id: message.watch_id.clone(),
                            generation: message.generation,
                            paths: HashSet::new(),
                            error: None,
                        });
                if pending.error.is_none() {
                    pending.error = message.error;
                }
                for path in message.paths {
                    if pending.paths.len() >= self.overflow_path_capacity {
                        break;
                    }
                    pending.paths.insert(path);
                }
                drop(overflowed);
                self.overflow_ready.notify_one();
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn drain_overflowed(&self) -> WatchOverflowBatch {
        let mut overflowed = self
            .overflowed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let global_rescan = std::mem::take(&mut overflowed.global_rescan);
        let mut messages = std::mem::take(&mut overflowed.messages)
            .into_values()
            .map(|message| {
                let mut paths = message.paths.into_iter().collect::<Vec<_>>();
                paths.sort();
                WatchEventMsg {
                    watch_id: message.watch_id,
                    generation: message.generation,
                    paths,
                    error: message.error,
                    overflowed: true,
                }
            })
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| left.watch_id.cmp(&right.watch_id));
        WatchOverflowBatch {
            messages,
            global_rescan,
        }
    }

    #[cfg(test)]
    fn overflow_len(&self) -> usize {
        self.overflowed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .messages
            .len()
    }

    #[cfg(test)]
    fn global_rescan_pending(&self) -> bool {
        self.overflowed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .global_rescan
    }
}

pub(crate) fn register_task_and_watches(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    spec: TaskSubmitSpec,
) -> Result<TaskAdmission> {
    let root = {
        let guard = state.lock().map_err(lock_err)?;
        guard.root.clone()
    };
    let spec = normalize_task_submit_spec(&root, spec)?;

    let replaces_existing_task = {
        let guard = state.lock().map_err(lock_err)?;
        guard.tasks.tasks.contains_key(&spec.task_id)
    };
    if replaces_existing_task {
        let _ = cancel_task(state.clone(), &spec.task_id)?;
    }

    let mut registrations = Vec::new();
    let mut replaced_task = None;
    let generation = {
        let mut guard = state.lock().map_err(lock_err)?;
        guard.require_task_mutable(&spec.task_id)?;
        if let Some(existing) = guard.tasks.tasks.get(&spec.task_id) {
            if !existing.lifecycle.is_cancelled() {
                anyhow::bail!(
                    "task '{}' was concurrently replaced during registration",
                    spec.task_id
                );
            }
            replaced_task = Some(existing.clone());
        }
        let generation = guard.task_generations.create(&spec.task_id)?.id();
        guard.tasks.tasks.remove(&spec.task_id);
        let watch_ids = spec
            .watches
            .iter()
            .map(|watch: &WatchSpec| {
                let mut watch = watch.clone();
                watch.task_id = spec.task_id.clone();
                if watch.root.trim().is_empty() {
                    watch.root = guard.root.to_string_lossy().to_string();
                }
                let registration = WatchRegistration {
                    watch_id: watch_id_for(&watch),
                    spec: watch,
                    active: true,
                    last_event_at_unix: None,
                    last_error: None,
                };
                guard.watches.watches.push(registration.clone());
                registrations.push(registration.clone());
                registration.watch_id
            })
            .collect::<Vec<_>>();
        let task = TaskRecord {
            task_id: spec.task_id.clone(),
            watch_ids,
            sequence_present: true,
            sequence: Some(spec.sequence.clone()),
            ..TaskRecord::default()
        };
        guard.tasks.tasks.insert(spec.task_id.clone(), task.clone());
        let mut delta = RegistryDelta::default().upsert_task(task);
        for registration in registrations.iter().cloned() {
            delta = delta.upsert_watch(registration);
        }
        guard.persistence.stage(delta)?;
        generation
    };

    let admission = (|| -> Result<TaskRecord> {
        for registration in &registrations {
            install_watch(
                state.clone(),
                watch_tx.clone(),
                registration.watch_id.clone(),
            )?;
        }

        let guard = state.lock().map_err(lock_err)?;
        if !guard.task_generations.matches(&spec.task_id, generation) {
            anyhow::bail!(
                "task '{}' generation changed during registration",
                spec.task_id
            );
        }
        guard
            .tasks
            .tasks
            .get(&spec.task_id)
            .cloned()
            .ok_or_else(|| anyhow!("task disappeared after registration"))
    })();
    let task = match admission {
        Ok(task) => task,
        Err(admission_error) => {
            if let Err(rollback_error) = rollback_failed_task_admission(
                state.clone(),
                &spec.task_id,
                generation,
                replaced_task,
            ) {
                state.lock().map_err(lock_err)?.shutdown.request();
                return Err(rollback_error.context(format!(
                    "task '{}' admission failed ({admission_error:#}) and rollback also failed",
                    spec.task_id
                )));
            }
            return Err(admission_error);
        }
    };
    Ok(TaskAdmission {
        task,
        watches: registrations,
        generation,
        replaced_task,
    })
}

pub(crate) fn run_sequence_for_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<context_kernel_core::KernelSequenceResponse> {
    run_sequence_for_task_with_generation_fence(state, task_id, TaskRunFence::Unfenced)?
        .ok_or_else(|| anyhow!("unfenced task run was not admitted"))
}

pub(crate) fn run_initial_sequence_for_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: TaskGenerationId,
) -> Result<context_kernel_core::KernelSequenceResponse> {
    let Some(response) = run_sequence_for_task_with_generation_fence(
        state,
        task_id,
        TaskRunFence::Initial(expected_generation),
    )?
    else {
        return Err(context_kernel_core::KernelError::SequenceCancelled {
            task_id: Some(task_id.to_string()),
        }
        .into());
    };
    Ok(response)
}

pub(crate) fn run_recovered_replan_for_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: TaskGenerationId,
) -> Result<bool> {
    Ok(run_sequence_for_task_with_generation_fence(
        state,
        task_id,
        TaskRunFence::RecoveredReplan(expected_generation),
    )?
    .is_some())
}

fn run_sequence_for_task_with_generation_fence(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    fence: TaskRunFence,
) -> Result<Option<context_kernel_core::KernelSequenceResponse>> {
    run_sequence_for_task_with_executor_fence(
        state,
        task_id,
        fence,
        |kernel, sequence, observer| kernel.execute_sequence_with_observer(sequence, observer),
    )
}

#[cfg(test)]
pub(crate) fn run_sequence_for_task_with_executor<F>(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: Option<TaskGenerationId>,
    execute_sequence: F,
) -> Result<Option<context_kernel_core::KernelSequenceResponse>>
where
    F: Fn(
        &Kernel,
        context_kernel_core::KernelSequenceRequest,
        &mut dyn SequenceObserver,
    ) -> std::result::Result<
        context_kernel_core::KernelSequenceResponse,
        context_kernel_core::KernelError,
    >,
{
    let fence = expected_generation.map_or(TaskRunFence::Unfenced, TaskRunFence::RecoveredReplan);
    run_sequence_for_task_with_executor_fence(state, task_id, fence, execute_sequence)
}

fn run_sequence_for_task_with_executor_fence<F>(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    fence: TaskRunFence,
    execute_sequence: F,
) -> Result<Option<context_kernel_core::KernelSequenceResponse>>
where
    F: Fn(
        &Kernel,
        context_kernel_core::KernelSequenceRequest,
        &mut dyn SequenceObserver,
    ) -> std::result::Result<
        context_kernel_core::KernelSequenceResponse,
        context_kernel_core::KernelError,
    >,
{
    loop {
        let fence_pending_replan = {
            let guard = state.lock().map_err(lock_err)?;
            if guard.shutting_down {
                if fence.is_fenced() {
                    return Ok(None);
                }
                anyhow::bail!("daemon is shutting down");
            }
            if !task_run_fence_matches(&guard, task_id, fence) {
                return Ok(None);
            }
            let task = guard
                .tasks
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            if task.lifecycle == TaskLifecycle::ReplanPending {
                if let Err(error) = persist_task(&guard, task_id) {
                    guard.shutdown.request();
                    return Err(error.context("failed to persist queued task replan"));
                }
                true
            } else {
                false
            }
        };
        if fence_pending_replan {
            if let Err(error) = flush_persistence(&state) {
                state.lock().map_err(lock_err)?.shutdown.request();
                return Err(error.context("failed to durably fence queued task replan"));
            }
        }

        let (kernel, sequence, generation, _sequence_lease, durable_replan_claim) = {
            let mut guard = state.lock().map_err(lock_err)?;
            if guard.shutting_down {
                if fence.is_fenced() {
                    return Ok(None);
                }
                anyhow::bail!("daemon is shutting down");
            }
            if !task_run_fence_matches(&guard, task_id, fence) {
                return Ok(None);
            }
            let (sequence, running_lifecycle, durable_replan_claim) = {
                let task = guard
                    .tasks
                    .tasks
                    .get(task_id)
                    .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
                let mut running_lifecycle = task.lifecycle;
                let durable_replan_claim = running_lifecycle == TaskLifecycle::ReplanPending;
                if durable_replan_claim {
                    running_lifecycle.start_durable_replan()?;
                } else {
                    running_lifecycle.start()?;
                }
                let sequence = task
                    .sequence
                    .clone()
                    .ok_or_else(|| anyhow!("task '{task_id}' has no stored sequence"))?;
                (sequence, running_lifecycle, durable_replan_claim)
            };
            let generation = match fence.expected_generation() {
                Some(expected_generation) => {
                    let Some(generation) = guard.task_generations.current(task_id) else {
                        return Ok(None);
                    };
                    if generation.id() != expected_generation {
                        return Ok(None);
                    }
                    generation
                }
                None => guard.task_generations.ensure(task_id)?,
            };
            let Some(sequence_lease) = generation.acquire_operation() else {
                if fence.is_fenced() {
                    return Ok(None);
                }
                return Err(context_kernel_core::KernelError::SequenceCancelled {
                    task_id: Some(task_id.to_string()),
                }
                .into());
            };
            guard.require_task_mutable(task_id)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("task '{task_id}' disappeared before sequence start"))?;
            task.lifecycle = running_lifecycle;
            task.last_started_at_unix = Some(now_unix());
            task.last_error = None;
            if let Err(error) = persist_task(&guard, task_id) {
                if durable_replan_claim {
                    guard.shutdown.request();
                }
                return Err(error.context("failed to persist claimed task lifecycle"));
            }
            (
                guard.kernel.clone(),
                sequence,
                generation,
                sequence_lease,
                durable_replan_claim,
            )
        };
        if durable_replan_claim {
            // Every queued replan claim must be durable before reducer
            // execution. A crash can then distinguish it from ordinary
            // Running residue and restore the still-owned replan.
            if let Err(error) = flush_persistence(&state) {
                state.lock().map_err(lock_err)?.shutdown.request();
                return Err(error.context("failed to durably publish claimed task replan"));
            }
            let guard = state.lock().map_err(lock_err)?;
            let claim_is_current = !guard.shutting_down
                && !generation.is_cancelled()
                && guard.task_generations.matches(task_id, generation.id())
                && guard.tasks.tasks.get(task_id).is_some_and(|task| {
                    matches!(
                        task.lifecycle,
                        TaskLifecycle::RunningRecoveredReplan | TaskLifecycle::RunningReplanPending
                    )
                });
            if !claim_is_current {
                return Ok(None);
            }
        }
        let _ = emit_task_event_for_generation(
            state.clone(),
            task_id,
            generation.id(),
            "task_started",
            json!({"task_id": task_id, "step_count": sequence.steps.len()}),
        );

        let mut observer = TaskSequenceObserver {
            state: state.clone(),
            task_id: task_id.to_string(),
            generation: generation.clone(),
        };
        let result = execute_sequence(&kernel, sequence, &mut observer);

        let rerun = {
            let mut guard = state.lock().map_err(lock_err)?;
            if !guard.task_generations.matches(task_id, generation.id())
                || generation.is_cancelled()
            {
                return Err(context_kernel_core::KernelError::SequenceCancelled {
                    task_id: Some(task_id.to_string()),
                }
                .into());
            }
            guard.require_task_mutable(task_id)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            task.last_completed_at_unix = Some(now_unix());
            match &result {
                Ok(response) => {
                    task.last_request_id = Some(response.request_id);
                    task.last_sequence_metadata = Some(response.metadata.clone());
                    task.last_error = None;
                }
                Err(err) => {
                    task.last_error = Some(err.to_string());
                    daemon_log(&format!("task run failed task_id={task_id} error={err}"));
                }
            }
            let rerun = task.lifecycle.finish_run()?;
            if rerun {
                task.last_replan_at_unix = Some(now_unix());
            }
            if let Err(error) = persist_task(&guard, task_id) {
                if rerun {
                    // The active invocation can no longer publish ownership of
                    // its queued rerun. Force process-level recovery instead
                    // of leaving ReplanPending live without an executor.
                    guard.shutdown.request();
                }
                return Err(error.context("failed to persist completed task lifecycle"));
            }
            rerun
        };

        let postprocessing = (|| -> Result<()> {
            if result.is_ok() && !generation.is_cancelled() {
                let mut summary = refresh_task_context_summary_for_generation(
                    state.clone(),
                    task_id,
                    generation.id(),
                )?
                .unwrap_or_else(|| json!({}));
                if generation.is_cancelled() {
                    return Err(context_kernel_core::KernelError::SequenceCancelled {
                        task_id: Some(task_id.to_string()),
                    }
                    .into());
                }
                let _ = set_context_reason_for_generation(
                    &state,
                    task_id,
                    generation.id(),
                    "replan_applied",
                )?;
                if !generation.is_cancelled() {
                    if let Some(response) = refresh_broker_context_for_task(&state, task_id, None)?
                    {
                        let storage_id = task_storage_id(task_id)?;
                        let root = state.lock().map_err(lock_err)?.root.clone();
                        let brief_path = task_brief_markdown_path(&root, &storage_id);
                        if let Some(object) = summary.as_object_mut() {
                            object.insert(
                                "changed_section_ids".to_string(),
                                Value::Array(
                                    response
                                        .delta
                                        .changed_sections
                                        .iter()
                                        .map(|section| Value::String(section.id.clone()))
                                        .collect(),
                                ),
                            );
                            object.insert(
                                "removed_section_ids".to_string(),
                                Value::Array(
                                    response
                                        .delta
                                        .removed_section_ids
                                        .iter()
                                        .map(|id| Value::String(id.clone()))
                                        .collect(),
                                ),
                            );
                            object.insert(
                                "reason".to_string(),
                                Value::String("replan_applied".to_string()),
                            );
                            object.insert(
                                "context_version".to_string(),
                                Value::String(response.context_version),
                            );
                            object.insert(
                                "brief_path".to_string(),
                                Value::String(brief_path.to_string_lossy().to_string()),
                            );
                        }
                    }
                }
                let _ = emit_task_event_for_generation(
                    state.clone(),
                    task_id,
                    generation.id(),
                    "context_updated",
                    summary,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = postprocessing {
            if rerun && !generation.is_cancelled() {
                daemon_log(&format!(
                    "task run postprocessing failed before owned rerun task_id={task_id}: \
                     {error:#}"
                ));
                continue;
            } else {
                return Err(error);
            }
        }

        match result {
            Ok(_) if rerun => {
                if generation.is_cancelled() {
                    return Err(context_kernel_core::KernelError::SequenceCancelled {
                        task_id: Some(task_id.to_string()),
                    }
                    .into());
                }
                continue;
            }
            Ok(response) => {
                let emitted = emit_task_event_for_generation(
                    state,
                    task_id,
                    generation.id(),
                    "task_completed",
                    json!({"task_id": task_id, "request_id": response.request_id}),
                )?;
                if !emitted {
                    return Err(context_kernel_core::KernelError::SequenceCancelled {
                        task_id: Some(task_id.to_string()),
                    }
                    .into());
                }
                return Ok(Some(response));
            }
            Err(err) if rerun => {
                let emitted = emit_task_event_for_generation(
                    state.clone(),
                    task_id,
                    generation.id(),
                    "task_failed",
                    json!({"task_id": task_id, "error": err.to_string()}),
                );
                match emitted {
                    Ok(true) if !generation.is_cancelled() => {}
                    Ok(_) => {
                        return Err(context_kernel_core::KernelError::SequenceCancelled {
                            task_id: Some(task_id.to_string()),
                        }
                        .into());
                    }
                    Err(error) if !generation.is_cancelled() => {
                        daemon_log(&format!(
                            "failed to persist task_failed before owned rerun task_id={task_id}: \
                             {error:#}"
                        ));
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }
            Err(err) => {
                let _ = emit_task_event_for_generation(
                    state,
                    task_id,
                    generation.id(),
                    "task_failed",
                    json!({"task_id": task_id, "error": err.to_string()}),
                )?;
                return Err(err.into());
            }
        }
    }
}

fn task_run_fence_matches(state: &DaemonState, task_id: &str, fence: TaskRunFence) -> bool {
    let Some(expected_generation) = fence.expected_generation() else {
        return true;
    };
    let lifecycle_matches = state
        .tasks
        .tasks
        .get(task_id)
        .is_some_and(|task| match fence {
            TaskRunFence::Unfenced => true,
            TaskRunFence::Initial(_) => {
                matches!(
                    task.lifecycle,
                    TaskLifecycle::Idle | TaskLifecycle::ReplanPending
                )
            }
            TaskRunFence::RecoveredReplan(_) => task.lifecycle == TaskLifecycle::ReplanPending,
        });
    lifecycle_matches && state.task_generations.matches(task_id, expected_generation)
}

pub(crate) fn cancel_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<(Option<TaskRecord>, Vec<String>)> {
    let (generation, watch_ids) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let Some(existing) = guard.tasks.tasks.get(task_id) else {
            return Ok((None, Vec::new()));
        };
        if existing.lifecycle.is_cancelled() || existing.archived.is_some() {
            return Ok((Some(existing.clone()), Vec::new()));
        }
        guard.require_task_mutable(task_id)?;
        let generation = guard.task_generations.ensure(task_id)?;
        generation.request_cancel();
        guard.require_task_mutable(task_id)?;
        let task = guard
            .tasks
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow!("task '{task_id}' disappeared before cancellation"))?;
        task.lifecycle.request_cancel();
        let watch_ids = task.watch_ids.clone();
        persist_task(&guard, task_id)?;
        (generation, watch_ids)
    };
    for watch_id in &watch_ids {
        let _ = remove_watch(state.clone(), watch_id)?;
    }
    crate::launch::terminate_generation_processes(&generation)?;
    if !generation.wait_until_idle(TASK_CANCELLATION_QUIESCE_TIMEOUT) {
        anyhow::bail!("timed out waiting for cancelled task '{task_id}' generation to become idle");
    }
    let terminal =
        complete_task_cancellation_for_generation(state, task_id, generation.id(), &watch_ids)?;
    Ok((terminal, watch_ids))
}

pub(crate) fn rollback_failed_task_admission(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: TaskGenerationId,
    replaced_task: Option<TaskRecord>,
) -> Result<()> {
    rollback_failed_task_admission_inner(state, task_id, expected_generation, replaced_task, false)
}

pub(crate) fn rollback_failed_initial_task_admission(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: TaskGenerationId,
    replaced_task: Option<TaskRecord>,
) -> Result<()> {
    let remove_fresh_storage = replaced_task.is_none();
    rollback_failed_task_admission_inner(
        state,
        task_id,
        expected_generation,
        replaced_task,
        remove_fresh_storage,
    )
}

fn rollback_failed_task_admission_inner(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    expected_generation: TaskGenerationId,
    replaced_task: Option<TaskRecord>,
    remove_fresh_storage: bool,
) -> Result<()> {
    let (generation, persistence) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let Some(generation) = guard.task_generations.current(task_id) else {
            return Ok(());
        };
        if generation.id() != expected_generation {
            return Ok(());
        }
        generation.request_cancel();
        let watch_ids = guard
            .watches
            .watches
            .iter()
            .filter(|watch| watch.spec.task_id == task_id)
            .map(|watch| watch.watch_id.clone())
            .collect::<Vec<_>>();
        for watch_id in watch_ids {
            guard.watcher_handles.remove(&watch_id);
        }
        (generation, guard.persistence.clone())
    };

    crate::launch::terminate_generation_processes(&generation)?;
    if !generation.wait_until_idle(TASK_CANCELLATION_QUIESCE_TIMEOUT) {
        anyhow::bail!("timed out waiting for failed task '{task_id}' generation to become idle");
    }

    let _event_guard = persistence.event_guard();
    if remove_fresh_storage && persistence.task_is_durably_admitted(task_id) {
        {
            let guard = state.lock().map_err(lock_err)?;
            let Some(current_generation) = guard.task_generations.current(task_id) else {
                return Ok(());
            };
            if current_generation.id() != expected_generation {
                return Ok(());
            }
            if guard
                .tasks
                .tasks
                .get(task_id)
                .is_some_and(|task| task.lifecycle.is_cancelling() || task.lifecycle.is_cancelled())
            {
                return Ok(());
            }
        }
        persistence
            .cleanup_failed_initial_task_storage(task_id)
            .with_context(|| {
                format!("failed to remove storage for failed initial task '{task_id}'")
            })?;
    }
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let Some(current_generation) = guard.task_generations.current(task_id) else {
            return Ok(());
        };
        if current_generation.id() != expected_generation {
            return Ok(());
        }
        if guard
            .tasks
            .tasks
            .get(task_id)
            .is_some_and(|task| task.lifecycle.is_cancelling() || task.lifecycle.is_cancelled())
        {
            return Ok(());
        }

        let watch_ids = guard
            .watches
            .watches
            .iter()
            .filter(|watch| watch.spec.task_id == task_id)
            .map(|watch| watch.watch_id.clone())
            .collect::<Vec<_>>();
        for watch_id in &watch_ids {
            guard.watcher_handles.remove(watch_id);
        }
        guard
            .watches
            .watches
            .retain(|watch| watch.spec.task_id != task_id);
        let failed_high_water = guard
            .tasks
            .tasks
            .remove(task_id)
            .map(|task| task.last_event_seq)
            .unwrap_or_default();
        let mut delta = RegistryDelta::default();
        if let Some(mut replaced_task) = replaced_task {
            replaced_task.last_event_seq = replaced_task.last_event_seq.max(failed_high_water);
            guard
                .tasks
                .tasks
                .insert(task_id.to_string(), replaced_task.clone());
            delta = delta.upsert_task(replaced_task);
        } else {
            delta = delta.remove_task(task_id);
        }
        for watch_id in watch_ids {
            delta = delta.remove_watch(watch_id);
        }
        guard.subscribers.remove(task_id);
        guard
            .task_generations
            .remove_if_current(task_id, expected_generation);
        guard.persistence.stage(delta)?;
        guard.changes.notify();
    }
    persistence
        .flush()
        .with_context(|| format!("failed to persist rollback for task '{task_id}'"))?;
    Ok(())
}

pub(crate) fn remove_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_id: &str,
) -> Result<Option<WatchRegistration>> {
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(task_id) = guard
        .watches
        .watches
        .iter()
        .find(|watch| watch.watch_id == watch_id)
        .map(|watch| watch.spec.task_id.clone())
    {
        guard.require_task_mutable(&task_id)?;
    }
    guard.watcher_handles.remove(watch_id);
    let removed = guard
        .watches
        .watches
        .iter()
        .position(|watch| watch.watch_id == watch_id)
        .map(|index| guard.watches.watches.remove(index));
    let Some(removed_watch) = removed.as_ref() else {
        return Ok(None);
    };
    let task_id = removed_watch.spec.task_id.clone();
    let mut delta = RegistryDelta::default().remove_watch(watch_id);
    if let Some(task) = guard.tasks.tasks.get_mut(&task_id) {
        task.watch_ids.retain(|candidate| candidate != watch_id);
        delta = delta.upsert_task(task.clone());
    }
    guard.persistence.stage(delta)?;
    Ok(removed)
}

pub(crate) fn restore_watchers(
    state: &Arc<Mutex<DaemonState>>,
    watch_tx: &WatchIngress,
) -> Result<()> {
    let watch_ids = state
        .lock()
        .map_err(lock_err)?
        .watches
        .watches
        .iter()
        .map(|watch| watch.watch_id.clone())
        .collect::<Vec<_>>();
    for watch_id in watch_ids {
        if let Err(err) = install_watch(state.clone(), watch_tx.clone(), watch_id.clone()) {
            daemon_log(&format!("failed to restore watch {watch_id}: {err}"));
        }
    }
    Ok(())
}

pub(crate) fn install_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    watch_id: String,
) -> Result<()> {
    let (spec, generation) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let spec = guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == watch_id)
            .map(|watch| watch.spec.clone())
            .ok_or_else(|| anyhow!("unknown watch '{watch_id}'"))?;
        if !guard.tasks.tasks.contains_key(&spec.task_id) {
            anyhow::bail!(
                "watch '{}' belongs to unknown task '{}'",
                watch_id,
                spec.task_id
            );
        }
        let generation = guard.task_generations.ensure(&spec.task_id)?.id();
        (spec, generation)
    };

    let callback_watch_id = watch_id.clone();
    let mut watcher = PollWatcher::new(
        move |result: notify::Result<Event>| match result {
            Ok(event) => {
                watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    generation,
                    paths: event.paths,
                    error: None,
                    overflowed: false,
                });
            }
            Err(err) => {
                watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    generation,
                    paths: Vec::new(),
                    error: Some(err.to_string()),
                    overflowed: false,
                });
            }
        },
        Config::default()
            .with_poll_interval(Duration::from_millis(spec.debounce_ms.unwrap_or(250))),
    )?;

    let paths = watch_paths(&spec);
    for path in &paths {
        let mode = if matches!(spec.kind, WatchKind::Git | WatchKind::File) {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(path, mode)?;
    }

    let mut guard = state.lock().map_err(lock_err)?;
    let Some(current_generation) = guard.task_generations.current(&spec.task_id) else {
        anyhow::bail!(
            "task '{}' generation disappeared during watch installation",
            spec.task_id
        );
    };
    if current_generation.id() != generation || current_generation.is_cancelled() {
        anyhow::bail!(
            "task '{}' generation changed during watch installation",
            spec.task_id
        );
    }
    let watch = guard
        .watches
        .watches
        .iter_mut()
        .find(|watch| watch.watch_id == watch_id)
        .ok_or_else(|| anyhow!("watch '{watch_id}' was removed during installation"))?;
    watch.active = true;
    watch.last_error = None;
    guard.watcher_handles.insert(watch_id.clone(), watcher);
    persist_watch(&guard, &watch_id)?;
    daemon_log(&format!(
        "installed watch watch_id={watch_id} task_id={} kind={:?}",
        spec.task_id, spec.kind
    ));
    Ok(())
}

pub(crate) async fn run_watch_processor(
    state: Arc<Mutex<DaemonState>>,
    ingress: WatchIngress,
    mut watch_rx: tokio::sync::mpsc::Receiver<WatchEventMsg>,
    blocking_pool: crate::runtime::BlockingPool,
) -> Result<()> {
    let mut shutdown = state.lock().map_err(lock_err)?.shutdown.subscribe();
    let mut pending = HashMap::<(String, TaskGenerationId), PendingWatchEvent>::new();
    let mut global_sweep = None;
    let mut workers = tokio::task::JoinSet::<Result<()>>::new();
    let max_workers = blocking_pool.max_operations();
    loop {
        let overflowed = ingress.drain_overflowed();
        for message in overflowed.messages {
            if !merge_watch_event(state.clone(), &mut pending, message)? {
                global_sweep.get_or_insert_with(GlobalWatchSweep::default);
            }
        }
        if overflowed.global_rescan {
            global_sweep = Some(GlobalWatchSweep::default());
        }
        fill_global_watch_sweep(state.clone(), &mut pending, &mut global_sweep)?;

        let next_deadline = next_watch_deadline(&pending);
        let event_is_due =
            next_deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now());
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            joined = workers.join_next(), if !workers.is_empty() => {
                match joined {
                    Some(Err(error)) if !error.is_cancelled() => {
                        return Err(anyhow!("watch worker failed to join: {error}"));
                    }
                    Some(Ok(Err(error))) => {
                        return Err(error.context("watch event processing failed"));
                    }
                    Some(Ok(Ok(()))) | Some(Err(_)) | None => {}
                }
            }
            admission = blocking_pool.admit(),
                if event_is_due && workers.len() < max_workers => {
                let admission = admission?;
                let Some(message) = take_due_watch_events(&mut pending, 1).pop() else {
                    drop(admission);
                    continue;
                };
                let worker_state = state.clone();
                workers.spawn(async move {
                    admission
                        .run_cancellable(move |cancellation| {
                            process_watch_event(worker_state, message, &cancellation)
                        })
                        .await
                });
            }
            message = watch_rx.recv() => {
                let Some(message) = message else {
                    break;
                };
                if !merge_watch_event(state.clone(), &mut pending, message)? {
                    global_sweep.get_or_insert_with(GlobalWatchSweep::default);
                }
            }
            () = ingress.overflow_ready.notified() => {}
            () = sleep_until_optional(next_deadline),
                if next_deadline.is_some() && !event_is_due => {}
        }
    }

    workers.abort_all();
    while workers.join_next().await.is_some() {}
    Ok(())
}

fn process_watch_event(
    state: Arc<Mutex<DaemonState>>,
    message: WatchEventMsg,
    cancellation: &crate::runtime::BlockingCancellation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let (task_id, error_message, generation, _activity_lease) = {
        let guard = state.lock().map_err(lock_err)?;
        let Some(registration) = guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == message.watch_id)
        else {
            return Ok(());
        };
        let task_id = registration.spec.task_id.clone();
        let Some(generation) = guard.task_generations.current(&task_id) else {
            return Ok(());
        };
        if generation.id() != message.generation || generation.is_cancelled() {
            return Ok(());
        }
        let Some(activity_lease) = generation.acquire_operation() else {
            return Ok(());
        };
        (task_id, message.error.clone(), generation, activity_lease)
    };

    {
        let mut guard = state.lock().map_err(lock_err)?;
        if !guard.task_generations.matches(&task_id, generation.id()) || generation.is_cancelled() {
            return Ok(());
        }
        if let Some(watch) = guard
            .watches
            .watches
            .iter_mut()
            .find(|watch| watch.watch_id == message.watch_id)
        {
            watch.last_event_at_unix = Some(now_unix());
            watch.last_error = error_message.clone();
        }
        persist_watch(&guard, &message.watch_id)?;
    }

    if let Some(error) = error_message {
        let _ = emit_task_event_for_generation(
            state,
            &task_id,
            generation.id(),
            "watch_error",
            json!({
                "watch_id": message.watch_id,
                "error": error,
            }),
        );
        return Ok(());
    }

    let _ = emit_task_event_for_generation(
        state.clone(),
        &task_id,
        generation.id(),
        "watch_triggered",
        json!({
            "watch_id": message.watch_id,
            "queue_overflowed": message.overflowed,
            "paths": message
                .paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
        }),
    );

    let _ =
        set_context_reason_for_generation(&state, &task_id, generation.id(), "watch_triggered")?;
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let _ = refresh_task_context_summary_for_generation(state.clone(), &task_id, generation.id())?;
    if generation.is_cancelled() || cancellation.is_cancelled() {
        return Ok(());
    }
    let _ = refresh_broker_context_for_task(&state, &task_id, None)?;

    let should_start = {
        let mut guard = state.lock().map_err(lock_err)?;
        if !guard.task_generations.matches(&task_id, generation.id()) || generation.is_cancelled() {
            return Ok(());
        }
        guard.require_task_mutable(&task_id)?;
        let should_start = guard
            .tasks
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?
            .lifecycle
            .request_replan()?;
        persist_task(&guard, &task_id)?;
        should_start
    };
    flush_persistence(&state)
        .with_context(|| format!("failed to durably queue watch replan for task '{task_id}'"))?;

    if should_start && !cancellation.is_cancelled() {
        if let Err(error) = run_sequence_for_task(state.clone(), &task_id) {
            let work_remains_owned = {
                let guard = state.lock().map_err(lock_err)?;
                guard.tasks.tasks.get(&task_id).is_some_and(|task| {
                    task.lifecycle == TaskLifecycle::ReplanPending || task.lifecycle.is_running()
                }) && guard.task_generations.matches(&task_id, generation.id())
                    && !generation.is_cancelled()
            };
            if work_remains_owned {
                return Err(error.context(format!(
                    "task '{task_id}' run failed while durable work remained ownerless"
                )));
            }
            daemon_log(&format!(
                "watch-triggered task run failed after releasing ownership task_id={task_id}: \
                 {error:#}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn watch_paths(spec: &WatchSpec) -> Vec<PathBuf> {
    match spec.kind {
        WatchKind::Git => vec![PathBuf::from(&spec.root).join(".git")],
        WatchKind::File => spec
            .paths
            .iter()
            .map(|path| PathBuf::from(&spec.root).join(path))
            .collect(),
        WatchKind::TestReport => spec
            .paths
            .iter()
            .map(|path| PathBuf::from(&spec.root).join(path))
            .collect(),
    }
}

pub(crate) fn watch_id_for(spec: &WatchSpec) -> String {
    let digest = blake3::hash(
        serde_json::to_string(spec)
            .unwrap_or_else(|_| format!("{spec:?}"))
            .as_bytes(),
    );
    format!("watch-{}", &digest.to_hex()[..16])
}

fn normalize_task_submit_spec(root: &Path, mut spec: TaskSubmitSpec) -> Result<TaskSubmitSpec> {
    if spec.task_id.trim().is_empty() {
        anyhow::bail!("task_id cannot be empty");
    }
    spec.sequence.reactive.enabled = true;
    spec.sequence.reactive.task_id = Some(spec.task_id.clone());
    if spec.sequence.steps.is_empty() {
        anyhow::bail!("sequence must contain at least one step");
    }
    spec.sequence = normalize_sequence_request(spec.sequence).map_err(|source| anyhow!(source))?;

    for watch in &mut spec.watches {
        watch.task_id = spec.task_id.clone();
        if watch.root.trim().is_empty() {
            watch.root = root.to_string_lossy().to_string();
        }
        let watch_root = resolve_root(Path::new(&watch.root));
        if !watch_root.exists() {
            anyhow::bail!("watch root '{}' does not exist", watch_root.display());
        }
        for path in watch_paths(watch) {
            if !path.exists() {
                anyhow::bail!("watch path '{}' does not exist", path.display());
            }
        }
    }

    Ok(spec)
}

fn merge_watch_event(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<(String, TaskGenerationId), PendingWatchEvent>,
    message: WatchEventMsg,
) -> Result<bool> {
    let debounce_ms = {
        let guard = state.lock().map_err(lock_err)?;
        let Some(watch) = guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.active && watch.watch_id == message.watch_id)
        else {
            return Ok(true);
        };
        let Some(generation) = guard.task_generations.current(&watch.spec.task_id) else {
            return Ok(true);
        };
        if generation.id() != message.generation {
            return Ok(true);
        }
        watch.spec.debounce_ms.unwrap_or(250)
    };
    Ok(merge_watch_event_with_debounce(
        pending,
        message,
        debounce_ms,
    ))
}

fn merge_watch_event_with_debounce(
    pending: &mut HashMap<(String, TaskGenerationId), PendingWatchEvent>,
    message: WatchEventMsg,
    debounce_ms: u64,
) -> bool {
    let due_at = tokio::time::Instant::now() + Duration::from_millis(debounce_ms);
    let key = (message.watch_id.clone(), message.generation);
    if !pending.contains_key(&key) && pending.len() >= MAX_PENDING_WATCH_KEYS {
        return false;
    }
    let entry = pending.entry(key).or_insert_with(|| PendingWatchEvent {
        watch_id: message.watch_id.clone(),
        generation: message.generation,
        paths: Vec::new(),
        error: None,
        overflowed: false,
        due_at,
    });
    entry.due_at = due_at;
    entry.overflowed |= message.overflowed;
    if entry.error.is_none() {
        entry.error = message.error.clone();
    }
    for path in message.paths {
        if entry.paths.len() >= MAX_PENDING_WATCH_PATHS {
            entry.overflowed = true;
            break;
        }
        if !entry.paths.iter().any(|existing| existing == &path) {
            entry.paths.push(path);
        }
    }
    true
}

fn take_due_watch_events(
    pending: &mut HashMap<(String, TaskGenerationId), PendingWatchEvent>,
    limit: usize,
) -> Vec<WatchEventMsg> {
    if limit == 0 {
        return Vec::new();
    }
    let now = tokio::time::Instant::now();
    let ready_ids = pending
        .iter()
        .filter(|(_, item)| item.due_at <= now)
        .map(|(key, _)| key.clone())
        .take(limit)
        .collect::<Vec<_>>();
    ready_ids
        .into_iter()
        .filter_map(|key| {
            pending.remove(&key).map(|item| WatchEventMsg {
                watch_id: item.watch_id,
                generation: item.generation,
                paths: item.paths,
                error: item.error,
                overflowed: item.overflowed,
            })
        })
        .collect()
}

fn fill_global_watch_sweep(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<(String, TaskGenerationId), PendingWatchEvent>,
    sweep: &mut Option<GlobalWatchSweep>,
) -> Result<()> {
    let Some(active_sweep) = sweep.as_mut() else {
        return Ok(());
    };
    if pending.len() >= MAX_PENDING_WATCH_KEYS {
        return Ok(());
    }

    let guard = state.lock().map_err(lock_err)?;
    let mut candidates = BTreeMap::new();
    let mut has_later_candidates = false;
    for watch in &guard.watches.watches {
        if !watch.active {
            continue;
        }
        if active_sweep
            .after_watch_id
            .as_ref()
            .is_some_and(|after| watch.watch_id.as_str() <= after.as_str())
        {
            continue;
        }
        let Some(generation) = guard.task_generations.current(&watch.spec.task_id) else {
            continue;
        };
        candidates.insert(
            watch.watch_id.clone(),
            (generation.id(), watch.spec.debounce_ms.unwrap_or(250)),
        );
        if candidates.len() > MAX_PENDING_WATCH_KEYS {
            candidates.pop_last();
            has_later_candidates = true;
        }
    }
    drop(guard);

    let candidate_count = candidates.len();
    let mut processed = 0;
    for (watch_id, (generation, debounce_ms)) in candidates {
        let key = (watch_id.clone(), generation);
        if !pending.contains_key(&key) && pending.len() >= MAX_PENDING_WATCH_KEYS {
            break;
        }
        let merged = merge_watch_event_with_debounce(
            pending,
            WatchEventMsg {
                watch_id: watch_id.clone(),
                generation,
                paths: Vec::new(),
                error: None,
                overflowed: true,
            },
            debounce_ms,
        );
        debug_assert!(merged, "global watch sweep exceeded pending capacity");
        active_sweep.after_watch_id = Some(watch_id);
        processed += 1;
    }
    if processed == candidate_count && !has_later_candidates {
        *sweep = None;
    }
    Ok(())
}

fn next_watch_deadline(
    pending: &HashMap<(String, TaskGenerationId), PendingWatchEvent>,
) -> Option<tokio::time::Instant> {
    pending.values().map(|item| item.due_at).min()
}

async fn sleep_until_optional(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation() -> TaskGenerationId {
        TaskGenerationRegistry::default()
            .create("task-watch")
            .unwrap()
            .id()
    }

    fn message(generation: TaskGenerationId, path: &str) -> WatchEventMsg {
        WatchEventMsg {
            watch_id: "watch-1".to_string(),
            generation,
            paths: vec![PathBuf::from(path)],
            error: None,
            overflowed: false,
        }
    }

    #[test]
    fn bounded_watch_ingress_coalesces_overflow_by_generation() {
        let generation = generation();
        let (ingress, mut receiver) = WatchIngress::new(1);
        ingress.send(message(generation, "src/one.rs"));
        ingress.send(message(generation, "src/two.rs"));
        ingress.send(message(generation, "src/three.rs"));

        assert_eq!(ingress.overflow_len(), 1);
        let queued = receiver.try_recv().unwrap();
        assert_eq!(queued.paths, vec![PathBuf::from("src/one.rs")]);
        let overflowed = ingress.drain_overflowed();
        assert!(!overflowed.global_rescan);
        assert_eq!(overflowed.messages.len(), 1);
        assert!(overflowed.messages[0].overflowed);
        assert_eq!(
            overflowed.messages[0].paths,
            vec![PathBuf::from("src/three.rs"), PathBuf::from("src/two.rs")]
        );
    }

    #[test]
    fn watch_ingress_overflow_storage_has_hard_key_and_path_bounds() {
        let generation = generation();
        let (ingress, _receiver) = WatchIngress::with_overflow_limits(1, 2, 2);
        ingress.send(message(generation, "queued"));

        let overflow_message = |watch_id: &str, paths: &[&str]| WatchEventMsg {
            watch_id: watch_id.to_string(),
            generation,
            paths: paths.iter().map(PathBuf::from).collect(),
            error: None,
            overflowed: false,
        };
        ingress.send(overflow_message("watch-1", &["a", "b", "c"]));
        ingress.send(overflow_message("watch-2", &["d"]));
        assert_eq!(ingress.overflow_len(), 2);
        assert!(!ingress.global_rescan_pending());
        let bounded = ingress.drain_overflowed();
        assert!(!bounded.global_rescan);
        assert_eq!(bounded.messages.len(), 2);
        assert!(bounded
            .messages
            .iter()
            .all(|message| message.paths.len() <= 2));

        ingress.send(overflow_message("watch-1", &["a"]));
        ingress.send(overflow_message("watch-2", &["d"]));
        for index in 3..10_000 {
            let path = format!("src/{index}.rs");
            ingress.send(overflow_message(
                &format!("watch-{index}"),
                &[path.as_str()],
            ));
        }
        assert_eq!(ingress.overflow_len(), 0);
        assert!(ingress.global_rescan_pending());

        let overflowed = ingress.drain_overflowed();
        assert!(overflowed.global_rescan);
        assert!(overflowed.messages.is_empty());
        assert!(!ingress.global_rescan_pending());
    }

    #[test]
    fn pending_watch_paths_are_bounded_and_force_a_rescan() {
        let generation = generation();
        let mut pending = HashMap::new();
        for index in 0..(MAX_PENDING_WATCH_PATHS + 100) {
            merge_watch_event_with_debounce(
                &mut pending,
                message(generation, &format!("src/{index}.rs")),
                250,
            );
        }

        let event = pending.values().next().unwrap();
        assert_eq!(event.paths.len(), MAX_PENDING_WATCH_PATHS);
        assert!(event.overflowed);
    }

    #[test]
    fn pending_watch_keys_are_bounded_and_signal_a_global_sweep() {
        let generation = generation();
        let mut pending = HashMap::new();
        for index in 0..MAX_PENDING_WATCH_KEYS {
            let mut event = message(generation, &format!("src/{index}.rs"));
            event.watch_id = format!("watch-{index}");
            assert!(merge_watch_event_with_debounce(&mut pending, event, 250));
        }

        let mut rejected = message(generation, "src/rejected.rs");
        rejected.watch_id = "watch-rejected".to_string();
        assert!(!merge_watch_event_with_debounce(
            &mut pending,
            rejected,
            250
        ));
        assert_eq!(pending.len(), MAX_PENDING_WATCH_KEYS);
    }

    #[test]
    fn global_watch_sweep_refills_the_bounded_pending_map_incrementally() {
        let state = crate::tests::support::daemon_test_state();
        {
            let mut guard = state.lock().unwrap();
            for index in 0..(MAX_PENDING_WATCH_KEYS + 5) {
                let task_id = format!("task-{index}");
                guard.task_generations.create(&task_id).unwrap();
                guard.watches.watches.push(WatchRegistration {
                    watch_id: format!("watch-{index:04}"),
                    spec: WatchSpec {
                        task_id,
                        debounce_ms: Some(0),
                        ..WatchSpec::default()
                    },
                    active: true,
                    ..WatchRegistration::default()
                });
            }
        }
        let mut pending = HashMap::new();
        let mut sweep = Some(GlobalWatchSweep::default());

        fill_global_watch_sweep(state.clone(), &mut pending, &mut sweep).unwrap();
        assert_eq!(pending.len(), MAX_PENDING_WATCH_KEYS);
        assert_eq!(
            sweep
                .as_ref()
                .and_then(|active| active.after_watch_id.as_deref()),
            Some("watch-0127")
        );

        let drained = take_due_watch_events(&mut pending, MAX_PENDING_WATCH_KEYS);
        assert_eq!(drained.len(), MAX_PENDING_WATCH_KEYS);
        fill_global_watch_sweep(state.clone(), &mut pending, &mut sweep).unwrap();
        assert_eq!(pending.len(), 5);
        assert!(sweep.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_deadline_resets_and_fires_once() {
        let generation = generation();
        let mut pending = HashMap::new();
        merge_watch_event_with_debounce(&mut pending, message(generation, "src/one.rs"), 250);
        tokio::time::advance(Duration::from_millis(200)).await;
        merge_watch_event_with_debounce(&mut pending, message(generation, "src/two.rs"), 250);
        tokio::time::advance(Duration::from_millis(249)).await;
        assert!(take_due_watch_events(&mut pending, 1).is_empty());

        tokio::time::advance(Duration::from_millis(1)).await;
        let due = take_due_watch_events(&mut pending, 1);
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].paths,
            vec![PathBuf::from("src/one.rs"), PathBuf::from("src/two.rs")]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn due_watch_dispatch_respects_worker_admission_capacity() {
        let generation = generation();
        let mut pending = HashMap::new();
        for index in 0..5 {
            let mut event = message(generation, &format!("src/{index}.rs"));
            event.watch_id = format!("watch-{index}");
            merge_watch_event_with_debounce(&mut pending, event, 0);
        }

        assert!(take_due_watch_events(&mut pending, 0).is_empty());
        assert_eq!(pending.len(), 5);
        assert_eq!(take_due_watch_events(&mut pending, 2).len(), 2);
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_pending_debounce_work() {
        let state = crate::tests::support::daemon_test_state();
        let generation = {
            let mut guard = state.lock().unwrap();
            guard.tasks.tasks.insert(
                "task-watch".to_string(),
                TaskRecord {
                    task_id: "task-watch".to_string(),
                    ..TaskRecord::default()
                },
            );
            let generation = guard.task_generations.create("task-watch").unwrap().id();
            guard.watches.watches.push(WatchRegistration {
                watch_id: "watch-1".to_string(),
                spec: WatchSpec {
                    task_id: "task-watch".to_string(),
                    debounce_ms: Some(1_000),
                    ..WatchSpec::default()
                },
                active: true,
                ..WatchRegistration::default()
            });
            generation
        };
        let root = state.lock().unwrap().root.clone();
        let (ingress, receiver) = WatchIngress::new(1);
        let processor = tokio::spawn(run_watch_processor(
            state.clone(),
            ingress.clone(),
            receiver,
            crate::runtime::BlockingPool::new(1),
        ));
        ingress.send(message(generation, "src/one.rs"));
        tokio::task::yield_now().await;
        state.lock().unwrap().shutdown.request();

        processor.await.unwrap().unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(load_task_events(&root, "task-watch").unwrap().is_empty());
    }
}
