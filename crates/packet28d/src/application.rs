use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{ErrorKind, Read as _};
use std::net::TcpListener;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use context_kernel_core::{Kernel, PersistConfig};
use packet28_daemon_core::retention::recover_task_store_quarantine_and_acquire_daemon_lease;
use packet28_daemon_core::storage::{
    ensure_daemon_dir, ensure_daemon_socket_dir,
    load_task_watch_registry_with_deltas_and_event_tails, now_unix, remove_runtime_files,
    write_runtime_info,
};
use packet28_daemon_core::task_store_lease::acquire_daemon_instance_lease;
use packet28_daemon_protocol::message::{
    DaemonRuntimeInfo, DaemonTransportAuth, DAEMON_TRANSPORT_SECRET_BYTES,
};
use packet28_daemon_protocol::paths::{log_path, ready_path, socket_path, workspace_socket_path};
use packet28_daemon_protocol::task::{TaskLaunchAgentRequest, TaskLifecycle};

use crate::index::{enqueue_initial_index_work, run_index_worker, IndexIngress, IndexWorkReceiver};
use crate::kernel_registry::PersistentKernelRegistry;
use crate::launch::task_launch_agent;
use crate::persistence::PersistenceOwner;
use crate::runtime::{BlockingPool, DaemonRuntimeConfig, ShutdownSignal, StateChangeSignal};
use crate::runtime_files::{load_index_manifest_file, load_index_runtime_files};
use crate::server::{authenticate_tcp_connection, handle_connection};
use crate::state::{
    BackgroundCommand, DaemonState, IndexCommand, TaskGenerationRegistry, WatchEventMsg,
};
use crate::watch::{
    restore_watchers, run_recovered_replan_for_task, run_watch_processor, WatchIngress,
};
use crate::{
    daemon_log, lock_err, preflight_restart_recovery, reconcile_interrupted_task_lifecycles,
    reconcile_task_event_high_waters, resolve_root, TASK_PERSISTENCE_DEBOUNCE_MS,
};

/// Runs one Packet28 daemon instance for `root` until shutdown completes.
///
/// The nearest ancestor containing `.git` becomes the workspace root. This
/// function changes the process working directory, acquires the workspace's
/// daemon and task-store leases, binds its configured transport, and blocks
/// while the owned runtime serves requests. Call it at most once per process.
///
/// Shutdown withdraws readiness, cancels active generations, joins runtime
/// owners, flushes kernel and task persistence, removes runtime files, and only
/// then releases the lifecycle leases.
///
/// # Errors
///
/// Returns an error when root resolution, recovery, lease acquisition,
/// transport startup, request orchestration, persistence shutdown, or
/// runtime-file cleanup cannot complete safely. Corrupt or conflicted durable
/// state fails closed before readiness is published.
pub fn serve(root: PathBuf) -> Result<()> {
    let root = resolve_root(&root);

    // Leave the spawning hook's process group/session so the host tearing
    // down a finished hook cannot take the daemon with it.
    packet28_daemon_protocol::process::detach_from_parent_session();

    std::env::set_current_dir(&root)
        .with_context(|| format!("failed to set daemon cwd to '{}'", root.display()))?;
    let daemon_instance_lease = acquire_daemon_instance_lease(&root)?;
    remove_stale_ready_marker(&root)?;
    let (recovery, task_store_lease) =
        recover_task_store_quarantine_and_acquire_daemon_lease(&root, &daemon_instance_lease)?;
    if recovery.restored_precommit_groups > 0
        || recovery.completed_committed_groups > 0
        || !recovery.issues.is_empty()
    {
        daemon_log(&format!(
            "task-store recovery restored={} completed={} issues={}",
            recovery.restored_precommit_groups,
            recovery.completed_committed_groups,
            recovery.issues.len()
        ));
    }
    ensure_daemon_dir(&root)?;
    let config = DaemonRuntimeConfig::from_env()?;
    let daemon_log_path = log_path(&root);
    let listener = bind_daemon_listener(&root)?;
    let runtime_files = RuntimeFileGuard::new(&root);
    let registry_instance_id = generate_daemon_registry_instance_id()?;

    let runtime = DaemonRuntimeInfo {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at_unix: now_unix(),
        ready_at_unix: None,
        socket_path: listener.endpoint(),
        workspace_root: root.to_string_lossy().to_string(),
        log_path: daemon_log_path.to_string_lossy().to_string(),
        transport_auth: listener.transport_auth(),
    };
    write_runtime_info(&root, &runtime)?;
    daemon_log(&format!(
        "starting packet28d pid={} root={} log={}",
        runtime.pid,
        root.display(),
        daemon_log_path.display()
    ));

    let kernel = Arc::new(
        Kernel::try_with_v1_reducers_and_persistence(PersistConfig::new(root.clone()))
            .with_context(|| {
                format!(
                    "failed to open primary persistent kernel for '{}'",
                    root.display()
                )
            })?,
    );
    let kernel_registry = Arc::new(PersistentKernelRegistry::new(
        &root,
        kernel.clone(),
        config.max_persistent_roots,
    )?);
    let (loaded_registry, event_tails) =
        load_task_watch_registry_with_deltas_and_event_tails(&root)?;
    let checkpoint_revision = loaded_registry.checkpoint_revision;
    let replayed_revision = loaded_registry.replayed_revision;
    let durable_tasks = loaded_registry.tasks;
    let durable_watches = loaded_registry.watches;
    // Reconciliation is a new delta. Keep the replayed image intact until the
    // persistence owner has authenticated it and assumed revision ownership.
    let mut tasks = durable_tasks.clone();
    let mut watches = durable_watches.clone();
    let preflight_healed = preflight_restart_recovery(&mut tasks)?;
    let event_high_water_changes = reconcile_task_event_high_waters(&mut tasks, &event_tails)?;
    let restart_reconciliation =
        reconcile_interrupted_task_lifecycles(&mut tasks, &mut watches, now_unix())?;
    let (persistence_owner, persistence) = PersistenceOwner::start_owned(
        root.clone(),
        task_store_lease.clone(),
        Duration::from_millis(TASK_PERSISTENCE_DEBOUNCE_MS),
        durable_tasks,
        durable_watches,
        checkpoint_revision,
        replayed_revision,
    )?;
    if restart_reconciliation.changed_tasks > 0 {
        daemon_log(&format!(
            "reconciled {} interrupted task lifecycle(s) after restart",
            restart_reconciliation.changed_tasks
        ));
    }
    if !restart_reconciliation.replan_task_ids.is_empty() {
        daemon_log(&format!(
            "restoring {} durable queued task replan(s) after restart",
            restart_reconciliation.replan_task_ids.len()
        ));
    }
    let mut startup_delta = crate::persistence::RegistryDelta::default();
    for task_id in event_high_water_changes
        .iter()
        .chain(&restart_reconciliation.changed_task_ids)
        .chain(&preflight_healed)
    {
        let task = tasks
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("startup reconciliation lost changed task '{task_id}'"))?;
        startup_delta = startup_delta.upsert_task(task);
    }
    for watch_id in &restart_reconciliation.removed_watch_ids {
        startup_delta = startup_delta.remove_watch(watch_id.clone());
    }
    persistence.stage_and_flush(startup_delta)?;
    persistence.checkpoint_current()?;
    let manifest = load_index_manifest_file(&root);
    let interactive_index = load_index_runtime_files(&root, manifest);
    let (index_tx, index_rx) = IndexIngress::new();
    let (background_tx, background_rx) =
        tokio::sync::mpsc::channel(config.background_queue_capacity);
    let shutdown = ShutdownSignal::new();
    let state = Arc::new(Mutex::new(DaemonState {
        root: root.clone(),
        kernel,
        kernel_registry,
        runtime,
        tasks,
        task_generations: TaskGenerationRegistry::default(),
        agent_snapshots: BTreeMap::new(),
        watches,
        registry_instance_id,
        registry_page_index: None,
        watcher_handles: HashMap::new(),
        subscribers: HashMap::new(),
        source_file_cache: BTreeMap::new(),
        interactive_index,
        index_tx,
        background_tx,
        persistence,
        #[cfg(test)]
        _persistence_owner: None,
        shutdown: shutdown.clone(),
        changes: StateChangeSignal::new(),
        shutting_down: false,
    }));
    let recovered_replans =
        prepare_recovered_replans(&state, restart_reconciliation.replan_task_ids)?;

    let (watch_tx, watch_rx) = WatchIngress::new(config.watch_queue_capacity);
    restore_watchers(&state, &watch_tx)?;
    enqueue_initial_index_work(&state)?;

    let blocking_pool = BlockingPool::with_lifecycle_leases(
        config.max_blocking_operations,
        daemon_instance_lease.clone(),
        task_store_lease.clone(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("packet28d-runtime")
        .build()
        .context("failed to create packet28d Tokio runtime")?;
    mark_ready(&state)?;
    let runtime_outcome = runtime.block_on(run_daemon_runtime(DaemonRuntimeInputs {
        listener,
        state: state.clone(),
        watch_tx,
        watch_rx,
        background_rx,
        index_rx,
        blocking_pool,
        daemon_instance_lease: daemon_instance_lease.clone(),
        task_store_lease: task_store_lease.clone(),
        recovered_replans,
        config,
    }));
    shutdown.request();
    let shutdown_deadline = runtime_outcome.deadline;
    let mut lifecycle_result = runtime_outcome.result;
    record_runtime_result(
        &mut lifecycle_result,
        shutdown_persistent_kernels(&state, remaining_until(shutdown_deadline)),
    );
    record_runtime_result(
        &mut lifecycle_result,
        persistence_owner
            .shutdown(remaining_until(shutdown_deadline))
            .map(|_| ())
            .context("failed to shut down daemon task persistence"),
    );
    runtime.shutdown_timeout(remaining_until(shutdown_deadline));

    daemon_log("shutting down packet28d");
    let cleanup_result = runtime_files.cleanup();
    // The supervisor has joined uncancellable index work and waited for every
    // admitted blocking mutation before the task-store lease can be released.
    // Runtime-file cleanup is part of the same lifecycle ownership window.
    drop(task_store_lease);
    drop(daemon_instance_lease);
    record_runtime_result(&mut lifecycle_result, cleanup_result);
    lifecycle_result
}

struct RuntimeFileGuard<'a> {
    root: &'a Path,
    armed: bool,
}

impl<'a> RuntimeFileGuard<'a> {
    fn new(root: &'a Path) -> Self {
        Self { root, armed: true }
    }

    fn cleanup(mut self) -> Result<()> {
        remove_runtime_files(self.root)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for RuntimeFileGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = remove_runtime_files(self.root) {
                daemon_log(&format!(
                    "failed to clean runtime files after startup failure: {error}"
                ));
            }
        }
    }
}

fn remove_stale_ready_marker(root: &Path) -> Result<()> {
    let path = ready_path(root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove stale readiness marker '{}'",
                path.display()
            )
        }),
    }
}

fn prepare_recovered_replans(
    state: &Arc<Mutex<DaemonState>>,
    task_ids: Vec<String>,
) -> Result<Vec<BackgroundCommand>> {
    let mut guard = state.lock().map_err(lock_err)?;
    let mut commands = Vec::with_capacity(task_ids.len());
    for task_id in task_ids {
        let task = guard
            .tasks
            .tasks
            .get(&task_id)
            .ok_or_else(|| anyhow!("startup replan task '{task_id}' disappeared"))?;
        if task.lifecycle != TaskLifecycle::ReplanPending {
            anyhow::bail!(
                "startup replan task '{task_id}' changed lifecycle before runtime admission"
            );
        }
        if !task.sequence_present || task.sequence.is_none() {
            anyhow::bail!(
                "startup replan task '{task_id}' has no stored sequence and cannot be recovered"
            );
        }
        let generation = guard.task_generations.ensure(&task_id)?.id();
        commands.push(BackgroundCommand::RunRecoveredReplan {
            task_id,
            generation,
        });
    }
    Ok(commands)
}

pub(crate) fn shutdown_persistent_kernels(
    state: &Arc<Mutex<DaemonState>>,
    timeout: Duration,
) -> Result<()> {
    let registry = state.lock().map_err(lock_err)?.kernel_registry.clone();
    let kernels = registry.kernels()?;
    let started = Instant::now();
    let mut result = Ok(());
    let kernel_count = kernels.len();
    for (index, kernel) in kernels.into_iter().enumerate() {
        let remaining = timeout.saturating_sub(started.elapsed());
        let roots_remaining = u32::try_from(kernel_count.saturating_sub(index)).unwrap_or(u32::MAX);
        let root_budget = remaining / roots_remaining.max(1);
        record_runtime_result(
            &mut result,
            kernel
                .shutdown_cache_persistence(root_budget)
                .map(|_| ())
                .context("failed to shut down daemon cache persistence"),
        );
    }
    result
}

struct DaemonRuntimeInputs {
    listener: DaemonListener,
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    watch_rx: tokio::sync::mpsc::Receiver<WatchEventMsg>,
    background_rx: tokio::sync::mpsc::Receiver<BackgroundCommand>,
    index_rx: IndexWorkReceiver,
    blocking_pool: BlockingPool,
    daemon_instance_lease: packet28_daemon_core::task_store_lease::TaskStoreLease,
    task_store_lease: packet28_daemon_core::task_store_lease::TaskStoreLease,
    recovered_replans: Vec<BackgroundCommand>,
    config: DaemonRuntimeConfig,
}

pub(crate) struct DaemonRuntimeOutcome {
    pub(crate) result: Result<()>,
    pub(crate) deadline: Instant,
}

impl DaemonRuntimeOutcome {
    fn with_new_deadline(result: Result<()>, grace: Duration) -> Self {
        Self {
            result,
            deadline: Instant::now() + grace,
        }
    }
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn run_daemon_runtime(inputs: DaemonRuntimeInputs) -> DaemonRuntimeOutcome {
    let DaemonRuntimeInputs {
        listener,
        state,
        watch_tx,
        watch_rx,
        background_rx,
        index_rx,
        blocking_pool,
        daemon_instance_lease,
        task_store_lease,
        recovered_replans,
        config,
    } = inputs;
    let shutdown = match state.lock().map_err(lock_err) {
        Ok(guard) => guard.shutdown.clone(),
        Err(error) => {
            return DaemonRuntimeOutcome::with_new_deadline(Err(error), config.shutdown_grace)
        }
    };
    let watch_task = tokio::spawn(run_watch_processor(
        state.clone(),
        watch_tx.clone(),
        watch_rx,
        blocking_pool.clone(),
    ));
    let background_task = tokio::spawn(run_background_tasks(
        state.clone(),
        background_rx,
        blocking_pool.clone(),
        recovered_replans,
    ));
    let index_state = state.clone();
    let index_task = tokio::task::spawn_blocking(move || {
        let _daemon_instance_lease = daemon_instance_lease;
        let _task_store_lease = task_store_lease;
        run_index_worker(index_state, index_rx)
    });
    let transport_task = tokio::spawn(run_transport(
        listener,
        state.clone(),
        watch_tx,
        blocking_pool.clone(),
        config.clone(),
    ));

    supervise_daemon_tasks(
        state,
        shutdown,
        blocking_pool,
        config.shutdown_grace,
        DaemonRuntimeTasks {
            transport: transport_task,
            watch: watch_task,
            background: background_task,
            index: index_task,
        },
    )
    .await
}

pub(crate) struct DaemonRuntimeTasks {
    pub(crate) transport: tokio::task::JoinHandle<Result<()>>,
    pub(crate) watch: tokio::task::JoinHandle<Result<()>>,
    pub(crate) background: tokio::task::JoinHandle<Result<()>>,
    pub(crate) index: tokio::task::JoinHandle<Result<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DaemonRuntimeTask {
    Transport,
    Watch,
    Background,
    Index,
}

impl DaemonRuntimeTask {
    const fn name(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Watch => "watch processor",
            Self::Background => "background processor",
            Self::Index => "index worker",
        }
    }
}

pub(crate) async fn supervise_daemon_tasks(
    state: Arc<Mutex<DaemonState>>,
    shutdown: ShutdownSignal,
    blocking_pool: BlockingPool,
    grace: Duration,
    tasks: DaemonRuntimeTasks,
) -> DaemonRuntimeOutcome {
    let DaemonRuntimeTasks {
        mut transport,
        mut watch,
        mut background,
        mut index,
    } = tasks;
    let mut shutdown_receiver = shutdown.subscribe();
    let trigger = tokio::select! {
        biased;
        () = wait_for_shutdown_request(&mut shutdown_receiver) => DaemonRuntimeTrigger::Shutdown,
        result = &mut transport => {
            DaemonRuntimeTrigger::TaskExit(DaemonRuntimeTask::Transport, result)
        }
        result = &mut watch => {
            DaemonRuntimeTrigger::TaskExit(DaemonRuntimeTask::Watch, result)
        }
        result = &mut background => {
            DaemonRuntimeTrigger::TaskExit(DaemonRuntimeTask::Background, result)
        }
        result = &mut index => {
            DaemonRuntimeTrigger::TaskExit(DaemonRuntimeTask::Index, result)
        }
    };
    let deadline = Instant::now() + grace;
    let (first_task, mut result) = match trigger {
        DaemonRuntimeTrigger::Shutdown => (None, Ok(())),
        DaemonRuntimeTrigger::TaskExit(task, exit) => {
            (Some(task), classify_first_runtime_exit(task, exit))
        }
    };

    shutdown.request();
    blocking_pool.request_shutdown();
    let shutdown_start = begin_daemon_shutdown(&state);
    record_runtime_result(&mut result, shutdown_start.result);

    if first_task != Some(DaemonRuntimeTask::Transport) {
        record_runtime_result(
            &mut result,
            join_abortable_runtime_task(
                DaemonRuntimeTask::Transport.name(),
                deadline,
                &mut transport,
            )
            .await,
        );
    }
    if first_task != Some(DaemonRuntimeTask::Watch) {
        record_runtime_result(
            &mut result,
            join_abortable_runtime_task(DaemonRuntimeTask::Watch.name(), deadline, &mut watch)
                .await,
        );
    }
    if first_task != Some(DaemonRuntimeTask::Background) {
        record_runtime_result(
            &mut result,
            join_abortable_runtime_task(
                DaemonRuntimeTask::Background.name(),
                deadline,
                &mut background,
            )
            .await,
        );
    }
    if first_task != Some(DaemonRuntimeTask::Index) {
        record_runtime_result(
            &mut result,
            join_blocking_runtime_task(DaemonRuntimeTask::Index.name(), deadline, &mut index).await,
        );
    }
    if tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        blocking_pool.wait_for_idle(),
    )
    .await
    .is_err()
    {
        record_runtime_result(
            &mut result,
            Err(anyhow!(
                "{} blocking operation(s) exceeded shutdown grace and remain lease-owned",
                blocking_pool.active_operations()
            )),
        );
    }
    DaemonRuntimeOutcome { result, deadline }
}

enum DaemonRuntimeTrigger {
    Shutdown,
    TaskExit(
        DaemonRuntimeTask,
        std::result::Result<Result<()>, tokio::task::JoinError>,
    ),
}

async fn wait_for_shutdown_request(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

struct DaemonShutdownStart {
    result: Result<()>,
}

fn begin_daemon_shutdown(state: &Arc<Mutex<DaemonState>>) -> DaemonShutdownStart {
    let (root, cancelled_generations, mut result) = {
        let mut guard = match state.lock().map_err(lock_err) {
            Ok(guard) => guard,
            Err(error) => {
                return DaemonShutdownStart { result: Err(error) };
            }
        };
        guard.shutting_down = true;
        let cancelled_generations = guard.task_generations.request_cancel_all();
        guard.watcher_handles.clear();
        (
            guard.root.clone(),
            cancelled_generations,
            guard.index_tx.send(IndexCommand::Shutdown),
        )
    };
    if !cancelled_generations.is_empty() {
        daemon_log(&format!(
            "requested cancellation for {} active task generation(s)",
            cancelled_generations.len()
        ));
    }
    match fs::remove_file(ready_path(&root)) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            record_runtime_result(
                &mut result,
                Err(error).with_context(|| {
                    format!("failed to withdraw readiness for '{}'", root.display())
                }),
            );
        }
    }
    record_runtime_result(
        &mut result,
        crate::launch::terminate_generations_processes(&cancelled_generations)
            .context("failed to terminate delegated task processes during daemon shutdown"),
    );
    DaemonShutdownStart { result }
}

fn classify_first_runtime_exit(
    task: DaemonRuntimeTask,
    exit: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    classify_runtime_join(task.name(), exit)?;
    anyhow::bail!("{} exited before daemon shutdown", task.name())
}

fn classify_runtime_join(
    name: &str,
    joined: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    joined
        .map_err(|error| anyhow!("{name} failed to join: {error}"))?
        .with_context(|| format!("{name} failed"))
}

fn record_runtime_result(primary: &mut Result<()>, candidate: Result<()>) {
    let Err(error) = candidate else {
        return;
    };
    if primary.is_ok() {
        *primary = Err(error);
    } else {
        daemon_log(&format!("additional daemon shutdown failure: {error:#}"));
    }
}

async fn join_abortable_runtime_task(
    name: &str,
    deadline: Instant,
    task: &mut tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), &mut *task).await {
        Ok(joined) => classify_runtime_join(name, joined),
        Err(_) => {
            daemon_log(&format!(
                "{name} exceeded shutdown grace; aborting async owner"
            ));
            task.abort();
            if let Err(error) = task.await {
                if !error.is_cancelled() {
                    daemon_log(&format!("{name} failed while being reaped: {error}"));
                }
            }
            anyhow::bail!("{name} exceeded shutdown grace")
        }
    }
}

async fn join_blocking_runtime_task(
    name: &str,
    deadline: Instant,
    task: &mut tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), &mut *task).await {
        Ok(joined) => classify_runtime_join(name, joined),
        Err(_) => {
            daemon_log(&format!(
                "{name} exceeded shutdown grace; detaching lease-owned blocking work"
            ));
            task.abort();
            anyhow::bail!("{name} exceeded shutdown grace and remains lease-owned")
        }
    }
}

async fn run_background_tasks(
    state: Arc<Mutex<DaemonState>>,
    mut receiver: tokio::sync::mpsc::Receiver<BackgroundCommand>,
    blocking_pool: BlockingPool,
    recovered_replans: Vec<BackgroundCommand>,
) -> Result<()> {
    let mut shutdown = state.lock().map_err(lock_err)?.shutdown.subscribe();
    let max_pending = blocking_pool.max_operations();
    let now = tokio::time::Instant::now();
    let mut pending = recovered_replans
        .into_iter()
        .map(|command| (now, command))
        .collect::<VecDeque<_>>();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        let next_ready = pending.front().map(|(ready_at, _)| *ready_at);
        let command_is_ready =
            next_ready.is_some_and(|ready_at| ready_at <= tokio::time::Instant::now());
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Some(Ok(Ok(()))) | None => {}
                    Some(Ok(Err(error))) => return Err(error),
                    Some(Err(error)) => {
                        if error.is_cancelled() {
                            continue;
                        }
                        return Err(anyhow!("background task failed to join: {error}"));
                    }
                }
            }
            admission = blocking_pool.admit(),
                if command_is_ready && tasks.len() < max_pending => {
                let admission = admission?;
                let Some((_, command)) = pending.pop_front() else {
                    drop(admission);
                    continue;
                };
                let task_state = state.clone();
                tasks.spawn(async move {
                    match command {
                        BackgroundCommand::RelaunchAgent { task_id, command } => {
                            let log_task_id = task_id.clone();
                            let result = admission
                                .run_cancellable(move |_| {
                                    task_launch_agent(
                                        task_state,
                                        TaskLaunchAgentRequest {
                                            task_id,
                                            task: None,
                                            wait_for_handoff: false,
                                            handoff_timeout_ms: None,
                                            handoff_poll_ms: None,
                                            command,
                                        },
                                    )
                                })
                                .await;
                            match result {
                                Ok(launched) => daemon_log(&format!(
                                    "auto-relaunched agent pid={} task={log_task_id}",
                                    launched.pid
                                )),
                                Err(error) => daemon_log(&format!(
                                    "auto-relaunch failed for task {log_task_id}: {error:#}"
                                )),
                            }
                            Ok(())
                        }
                        BackgroundCommand::RunRecoveredReplan {
                            task_id,
                            generation,
                        } => {
                            let log_task_id = task_id.clone();
                            let recovery_state = task_state.clone();
                            let result = admission
                                .run_cancellable(move |cancellation| {
                                    if cancellation.is_cancelled() {
                                        return Ok(false);
                                    }
                                    run_recovered_replan_for_task(
                                        task_state,
                                        &task_id,
                                        generation,
                                    )
                                })
                                .await;
                            match result {
                                Ok(true) => daemon_log(&format!(
                                    "completed recovered queued replan task={log_task_id}"
                                )),
                                Ok(false) => daemon_log(&format!(
                                    "skipped stale recovered queued replan task={log_task_id}"
                                )),
                                Err(error) => {
                                    let recovery_work_is_ownerless = {
                                        let guard = recovery_state.lock().map_err(lock_err)?;
                                        guard
                                            .tasks
                                            .tasks
                                            .get(&log_task_id)
                                            .is_some_and(|task| {
                                                matches!(
                                                    task.lifecycle,
                                                    TaskLifecycle::ReplanPending
                                                        | TaskLifecycle::RunningRecoveredReplan
                                                        | TaskLifecycle::RunningReplanPending
                                                )
                                            })
                                            && guard
                                                .task_generations
                                                .matches(&log_task_id, generation)
                                    };
                                    if recovery_work_is_ownerless {
                                        return Err(error.context(format!(
                                            "recovered queued replan for task '{log_task_id}' \
                                             failed while durable work remained ownerless"
                                        )));
                                    }
                                    daemon_log(&format!(
                                        "recovered queued replan failed for task {log_task_id}: \
                                         {error:#}"
                                    ));
                                }
                            }
                            Ok(())
                        }
                    }
                });
            }
            command = receiver.recv(), if pending.len() < max_pending => {
                let Some(command) = command else {
                    break;
                };
                pending.push_back((
                    tokio::time::Instant::now() + Duration::from_millis(500),
                    command,
                ));
            }
            () = sleep_until_optional(next_ready), if next_ready.is_some() && !command_is_ready => {}
        }
    }
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(anyhow!("background task failed to join: {error}")),
        }
    }
    Ok(())
}

async fn sleep_until_optional(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    }
}

pub(crate) async fn run_transport(
    listener: DaemonListener,
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    blocking_pool: BlockingPool,
    config: DaemonRuntimeConfig,
) -> Result<()> {
    let listener = listener.into_async()?;
    let permits = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let authentication_permits = Arc::new(tokio::sync::Semaphore::new(
        config.max_pending_tcp_authentications,
    ));
    let mut shutdown = state.lock().map_err(lock_err)?.shutdown.subscribe();
    let mut connections = tokio::task::JoinSet::new();

    loop {
        while let Some(joined) = connections.try_join_next() {
            log_connection_join(Some(joined));
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                log_connection_join(joined);
            }
            accepted = listener.accept() => {
                let accepted = accepted.context("daemon listener accept failed")?;
                let stream = match accepted {
                    DaemonAcceptOutcome::Accepted(stream) => stream,
                    DaemonAcceptOutcome::Rejected { reason } => {
                        daemon_log(&format!("connection rejected before request dispatch: {reason}"));
                        continue;
                    }
                };
                let connection_state = state.clone();
                let connection_watch_tx = watch_tx.clone();
                let connection_pool = blocking_pool.clone();
                let connection_config = config.clone();
                match stream {
                    DaemonAcceptedStream::Unix(stream) => {
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            daemon_log(&format!(
                                "connection rejected: active connection cap {} reached",
                                config.max_connections
                            ));
                            continue;
                        };
                        connections.spawn(async move {
                            let _permit = permit;
                            handle_connection(
                                connection_state,
                                connection_watch_tx,
                                stream,
                                connection_config,
                                connection_pool,
                            )
                            .await
                        });
                    }
                    DaemonAcceptedStream::Tcp { mut stream, auth } => {
                        let Ok(authentication_permit) =
                            authentication_permits.clone().try_acquire_owned()
                        else {
                            daemon_log(&format!(
                                "TCP connection rejected: pending authentication cap {} reached",
                                config.max_pending_tcp_authentications
                            ));
                            continue;
                        };
                        let connection_permits = permits.clone();
                        connections.spawn(async move {
                            let mut connection_shutdown = connection_state
                                .lock()
                                .map_err(lock_err)?
                                .shutdown
                                .subscribe();
                            let authenticated = tokio::select! {
                                biased;
                                changed = connection_shutdown.changed() => {
                                    if changed.is_err() || *connection_shutdown.borrow() {
                                        return Ok(());
                                    }
                                    false
                                }
                                authenticated = tokio::time::timeout(
                                    connection_config.transport_auth_timeout,
                                    authenticate_tcp_connection(
                                        &mut stream,
                                        &auth,
                                        &connection_config,
                                        &connection_pool,
                                    ),
                                ) => match authenticated {
                                    Ok(authenticated) => authenticated?,
                                    Err(_) => false,
                                },
                            };
                            drop(authentication_permit);
                            if !authenticated {
                                return Ok(());
                            }
                            let Ok(permit) = connection_permits.try_acquire_owned() else {
                                daemon_log(&format!(
                                    "authenticated connection rejected: active connection cap {} reached",
                                    connection_config.max_connections
                                ));
                                return Ok(());
                            };
                            let _permit = permit;
                            handle_connection(
                                connection_state,
                                connection_watch_tx,
                                stream,
                                connection_config,
                                connection_pool,
                            )
                            .await
                        });
                    }
                }
            }
        }
    }
    drop(listener);

    while let Some(joined) = connections.join_next().await {
        log_connection_join(Some(joined));
    }
    Ok(())
}

fn log_connection_join(joined: Option<std::result::Result<Result<()>, tokio::task::JoinError>>) {
    match joined {
        Some(Ok(Err(error))) if !is_benign_connection_error(&error) => {
            daemon_log(&format!("request handling failed: {error}"));
        }
        Some(Ok(Err(_))) => {}
        Some(Err(error)) if !error.is_cancelled() => {
            daemon_log(&format!("connection task failed to join: {error}"));
        }
        Some(Ok(Ok(()))) | Some(Err(_)) | None => {}
    }
}

fn is_benign_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
                )
            })
    })
}

fn mark_ready(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let (root, runtime) = {
        let mut guard = state.lock().map_err(lock_err)?;
        guard.runtime.ready_at_unix = Some(now_unix());
        (guard.root.clone(), guard.runtime.clone())
    };
    write_runtime_info(&root, &runtime)?;
    fs::write(
        ready_path(&root),
        format!("{}\n", runtime.ready_at_unix.unwrap_or_default()),
    )
    .with_context(|| format!("failed to write ready file for '{}'", root.display()))?;
    daemon_log(&format!(
        "daemon ready root={} socket={}",
        root.display(),
        runtime.socket_path
    ));
    Ok(())
}

fn cleanup_socket_before_bind(socket: &Path) -> Result<()> {
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => {
                anyhow::bail!(
                    "packet28d is already running for '{}'; refusing to replace a live socket",
                    socket.display()
                );
            }
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                ) =>
            {
                daemon_log(&format!(
                    "removing stale socket '{}' after probe failure: {}",
                    socket.display(),
                    err
                ));
                fs::remove_file(socket).with_context(|| {
                    format!("failed to remove stale socket '{}'", socket.display())
                })?;
            }
            Err(err) => {
                daemon_log(&format!(
                    "removing unreachable socket '{}' after probe failure: {}",
                    socket.display(),
                    err
                ));
                fs::remove_file(socket).with_context(|| {
                    format!(
                        "failed to remove unreachable socket '{}' after probe failure",
                        socket.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

pub(crate) enum DaemonListener {
    Unix {
        endpoint: PathBuf,
        listener: UnixListener,
        owner_uid: u32,
    },
    Tcp {
        endpoint: String,
        listener: TcpListener,
        auth: DaemonTransportAuth,
    },
}

enum DaemonAcceptedStream {
    Unix(tokio::net::UnixStream),
    Tcp {
        stream: tokio::net::TcpStream,
        auth: DaemonTransportAuth,
    },
}

enum DaemonAcceptOutcome {
    Accepted(DaemonAcceptedStream),
    Rejected { reason: String },
}

enum AsyncDaemonListener {
    Unix {
        listener: tokio::net::UnixListener,
        owner_uid: u32,
    },
    Tcp {
        listener: tokio::net::TcpListener,
        auth: DaemonTransportAuth,
    },
}

impl DaemonListener {
    pub(crate) fn endpoint(&self) -> String {
        match self {
            DaemonListener::Unix { endpoint, .. } => endpoint.to_string_lossy().to_string(),
            DaemonListener::Tcp { endpoint, .. } => endpoint.clone(),
        }
    }

    pub(crate) fn transport_auth(&self) -> Option<DaemonTransportAuth> {
        match self {
            Self::Unix { .. } => None,
            Self::Tcp { auth, .. } => Some(auth.clone()),
        }
    }

    fn into_async(self) -> Result<AsyncDaemonListener> {
        match self {
            DaemonListener::Unix {
                listener,
                owner_uid,
                ..
            } => {
                listener.set_nonblocking(true)?;
                Ok(AsyncDaemonListener::Unix {
                    listener: tokio::net::UnixListener::from_std(listener)?,
                    owner_uid,
                })
            }
            DaemonListener::Tcp { listener, auth, .. } => {
                listener.set_nonblocking(true)?;
                Ok(AsyncDaemonListener::Tcp {
                    listener: tokio::net::TcpListener::from_std(listener)?,
                    auth,
                })
            }
        }
    }
}

impl AsyncDaemonListener {
    async fn accept(&self) -> std::io::Result<DaemonAcceptOutcome> {
        match self {
            Self::Unix {
                listener,
                owner_uid,
            } => {
                let (stream, _) = listener.accept().await?;
                let credentials = match stream.peer_cred() {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        return Ok(DaemonAcceptOutcome::Rejected {
                            reason: format!(
                                "Unix peer credentials could not be authenticated: {error}"
                            ),
                        });
                    }
                };
                if credentials.uid() != *owner_uid {
                    return Ok(DaemonAcceptOutcome::Rejected {
                        reason: format!(
                            "Unix peer uid {} does not own daemon uid {owner_uid}",
                            credentials.uid()
                        ),
                    });
                }
                Ok(DaemonAcceptOutcome::Accepted(DaemonAcceptedStream::Unix(
                    stream,
                )))
            }
            Self::Tcp { listener, auth } => {
                let (stream, _) = listener.accept().await?;
                Ok(DaemonAcceptOutcome::Accepted(DaemonAcceptedStream::Tcp {
                    stream,
                    auth: auth.clone(),
                }))
            }
        }
    }
}

fn bind_daemon_listener(root: &Path) -> Result<DaemonListener> {
    if std::env::var("PACKET28D_FORCE_TCP")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
    {
        return bind_tcp_listener("PACKET28D_FORCE_TCP requested TCP daemon transport");
    }
    let primary = socket_path(root);
    if let Err(error) = ensure_daemon_socket_dir(root) {
        daemon_log(&format!(
            "falling back to workspace socket after private temp socket directory authentication failed: {error:#}"
        ));
        return bind_workspace_listener_or_tcp(root, &primary, &error.to_string());
    }
    bind_preferred_daemon_listener(root, &primary)
}

pub(crate) fn bind_preferred_daemon_listener(
    root: &Path,
    primary: &Path,
) -> Result<DaemonListener> {
    match bind_unix_listener(primary) {
        Ok(listener) => Ok(listener),
        Err(primary_error) if io_error_is_permission_denied(&primary_error) => {
            bind_workspace_listener_or_tcp(root, primary, &primary_error.to_string())
        }
        Err(error) => Err(error),
    }
}

fn bind_workspace_listener_or_tcp(
    root: &Path,
    primary: &Path,
    primary_error: &str,
) -> Result<DaemonListener> {
    let fallback = workspace_socket_path(root);
    daemon_log(&format!(
        "falling back to workspace socket '{}' after temp socket '{}' failed: {primary_error}",
        fallback.display(),
        primary.display()
    ));
    match bind_unix_listener(&fallback) {
        Ok(listener) => Ok(listener),
        Err(fallback_error) => {
            daemon_log(&format!(
                "falling back to TCP after workspace socket bind failed: {fallback_error:#}"
            ));
            bind_tcp_listener(&format!(
                "Unix sockets '{}' and '{}' were unavailable",
                primary.display(),
                fallback.display()
            ))
        }
    }
}

pub(crate) fn bind_unix_listener(endpoint: &Path) -> Result<DaemonListener> {
    cleanup_socket_before_bind(endpoint)?;
    let listener = UnixListener::bind(endpoint)
        .with_context(|| format!("failed to bind '{}'", endpoint.display()))?;
    if let Err(error) = authenticate_bound_unix_socket(endpoint) {
        drop(listener);
        let _ = fs::remove_file(endpoint);
        return Err(error);
    }
    Ok(DaemonListener::Unix {
        endpoint: endpoint.to_path_buf(),
        listener,
        owner_uid: effective_uid(),
    })
}

fn authenticate_bound_unix_socket(socket: &Path) -> Result<()> {
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure Unix daemon socket '{}'", socket.display()))?;
    let metadata = fs::symlink_metadata(socket).with_context(|| {
        format!(
            "failed to authenticate Unix daemon socket '{}'",
            socket.display()
        )
    })?;
    let effective_uid = effective_uid();
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.file_type().is_socket() || metadata.uid() != effective_uid || mode != 0o600 {
        anyhow::bail!(
            "Unix daemon socket '{}' failed owner/mode authentication: \
             socket={} uid={} mode={mode:o}; expected uid={effective_uid} mode=600",
            socket.display(),
            metadata.file_type().is_socket(),
            metadata.uid()
        );
    }
    Ok(())
}

pub(crate) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not retain pointers.
    unsafe { libc::geteuid() }
}

fn io_error_is_permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1)
                || error.to_string().contains("Operation not permitted")
        })
    })
}

pub(crate) fn bind_tcp_listener(reason: &str) -> Result<DaemonListener> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .with_context(|| format!("failed to bind TCP fallback after {reason}"))?;
    let endpoint = format!("tcp://{}", listener.local_addr()?);
    let auth = generate_daemon_transport_auth()?;
    daemon_log(&format!(
        "using TCP daemon endpoint '{endpoint}' ({reason})"
    ));
    Ok(DaemonListener::Tcp {
        endpoint,
        listener,
        auth,
    })
}

fn generate_daemon_transport_auth() -> Result<DaemonTransportAuth> {
    Ok(DaemonTransportAuth::from_secret_bytes(
        read_daemon_random_bytes("TCP capability")?,
    ))
}

fn generate_daemon_registry_instance_id() -> Result<String> {
    let random = read_daemon_random_bytes("registry instance identifier")?;
    Ok(blake3::hash(&random).to_hex().to_string())
}

fn read_daemon_random_bytes(purpose: &str) -> Result<[u8; DAEMON_TRANSPORT_SECRET_BYTES]> {
    let mut secret = [0_u8; DAEMON_TRANSPORT_SECRET_BYTES];
    fs::File::open("/dev/urandom")
        .with_context(|| {
            format!("failed to open operating-system random source for daemon {purpose}")
        })?
        .read_exact(&mut secret)
        .with_context(|| {
            format!("failed to read daemon {purpose} from operating-system random source")
        })?;
    Ok(secret)
}
