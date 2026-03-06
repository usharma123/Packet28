use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use context_memory_core::{
    ContextStoreListFilter, ContextStorePaging, ContextStorePruneRequest, PacketCache,
    PersistConfig as MemoryPersistConfig, RecallOptions,
};
use context_kernel_core::{Kernel, KernelRequest, PersistConfig};
use diffy_core::model::CoverageFormat;
use glob::Pattern;
use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};
use packet28_daemon_core::{
    ensure_daemon_dir, load_task_registry, load_watch_registry, now_unix,
    read_socket_message, remove_runtime_files, save_task_registry, save_watch_registry,
    socket_path, write_runtime_info, write_socket_message, CoverCheckRequest, CoverCheckResponse,
    ContextRecallRequest, ContextRecallResponse, ContextStoreGetRequest, ContextStoreGetResponse,
    ContextStoreListRequest, ContextStoreListResponse, ContextStorePruneDaemonRequest,
    ContextStorePruneResponse, ContextStoreStatsRequest, ContextStoreStatsResponse, DaemonRequest,
    DaemonResponse, DaemonRuntimeInfo, DaemonStatus, PacketFetchResponse, TaskRecord,
    TaskRegistry, TaskSubmitSpec, TestMapRequest, TestMapResponse, TestMapSummary,
    TestShardRequest, TestShardResponse, WatchKind, WatchRegistration, WatchRegistry, WatchSpec,
};
use serde_json::json;

#[derive(Parser)]
#[command(name = "packet28d", version, about = "Packet28 local daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon server for one workspace root
    Serve {
        #[arg(long, default_value = ".")]
        root: String,
    },
}

struct WatchEventMsg {
    watch_id: String,
    paths: Vec<PathBuf>,
    error: Option<String>,
}

struct DaemonState {
    root: PathBuf,
    kernel: Arc<Kernel>,
    runtime: DaemonRuntimeInfo,
    tasks: TaskRegistry,
    watches: WatchRegistry,
    watcher_handles: HashMap<String, PollWatcher>,
    shutting_down: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { root } => serve(resolve_root(Path::new(&root))),
    }
}

fn serve(root: PathBuf) -> Result<()> {
    std::env::set_current_dir(&root)
        .with_context(|| format!("failed to set daemon cwd to '{}'", root.display()))?;
    ensure_daemon_dir(&root)?;
    let socket = socket_path(&root);
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket '{}'", socket.display()))?;
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind '{}'", socket.display()))?;
    listener.set_nonblocking(true)?;

    let runtime = DaemonRuntimeInfo {
        pid: std::process::id(),
        started_at_unix: now_unix(),
        socket_path: socket.to_string_lossy().to_string(),
        workspace_root: root.to_string_lossy().to_string(),
    };
    write_runtime_info(&root, &runtime)?;

    let kernel = Arc::new(Kernel::with_v1_reducers_and_persistence(PersistConfig::new(
        root.clone(),
    )));
    let tasks = load_task_registry(&root)?;
    let watches = load_watch_registry(&root)?;
    let state = Arc::new(Mutex::new(DaemonState {
        root: root.clone(),
        kernel,
        runtime,
        tasks,
        watches,
        watcher_handles: HashMap::new(),
        shutting_down: false,
    }));

    let (watch_tx, watch_rx) = mpsc::channel();
    restore_watchers(&state, &watch_tx)?;
    spawn_watch_processor(state.clone(), watch_rx);

    loop {
        if state.lock().map_err(lock_err)?.shutting_down {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let request = match read_socket_message(&mut stream) {
                    Ok(value) => value,
                    Err(err) => {
                        let _ = write_socket_message(
                            &mut stream,
                            &DaemonResponse::Error {
                                message: err.to_string(),
                            },
                        );
                        continue;
                    }
                };
                let response = handle_request(state.clone(), watch_tx.clone(), request);
                let response = match response {
                    Ok(value) => value,
                    Err(err) => DaemonResponse::Error {
                        message: err.to_string(),
                    },
                };
                write_socket_message(&mut stream, &response)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err.into()),
        }
    }

    remove_runtime_files(&root)?;
    Ok(())
}

fn handle_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    request: DaemonRequest,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Execute { request } => {
            let kernel = state.lock().map_err(lock_err)?.kernel.clone();
            let response = kernel.execute(request)?;
            Ok(DaemonResponse::Execute { response })
        }
        DaemonRequest::ExecuteSequence { spec } => {
            let (task, watches) = register_task_and_watches(state.clone(), watch_tx, spec)?;
            let response = run_sequence_for_task(state.clone(), &task.task_id)?;
            let task = state
                .lock()
                .map_err(lock_err)?
                .tasks
                .tasks
                .get(&task.task_id)
                .cloned()
                .unwrap_or(task);
            Ok(DaemonResponse::ExecuteSequence {
                response,
                task,
                watches,
            })
        }
        DaemonRequest::Status => {
            let guard = state.lock().map_err(lock_err)?;
            let status = build_status(&guard)?;
            Ok(DaemonResponse::Status { status })
        }
        DaemonRequest::Stop => {
            state.lock().map_err(lock_err)?.shutting_down = true;
            Ok(DaemonResponse::Ack {
                message: "stopping".to_string(),
            })
        }
        DaemonRequest::TaskStatus { task_id } => {
            let task = state
                .lock()
                .map_err(lock_err)?
                .tasks
                .tasks
                .get(&task_id)
                .cloned();
            Ok(DaemonResponse::TaskStatus { task })
        }
        DaemonRequest::TaskCancel { task_id } => {
            let removed = cancel_task(state.clone(), &task_id)?;
            Ok(DaemonResponse::TaskCancel {
                task: removed.0,
                removed_watch_ids: removed.1,
            })
        }
        DaemonRequest::WatchList { task_id } => {
            let state = state.lock().map_err(lock_err)?;
            let watches = state
                .watches
                .watches
                .iter()
                .filter(|watch| {
                    task_id
                        .as_ref()
                        .map(|task_id| watch.spec.task_id == *task_id)
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            Ok(DaemonResponse::WatchList { watches })
        }
        DaemonRequest::WatchRemove { watch_id } => {
            let removed = remove_watch(state, &watch_id)?;
            Ok(DaemonResponse::WatchRemove { removed })
        }
        DaemonRequest::PacketFetch { request } => {
            let root = resolve_root(Path::new(&request.root));
            let value = suite_packet_core::read_packet_artifact(&root, &request.handle)
                .map_err(|source| anyhow!(source.to_string()))?;
            let wrapper = serde_json::from_value(value)
                .map_err(|source| anyhow!("invalid packet artifact: {source}"))?;
            Ok(DaemonResponse::PacketFetch {
                response: PacketFetchResponse { wrapper },
            })
        }
        DaemonRequest::CoverCheck { request } => {
            let response = run_cover_check(request)?;
            Ok(DaemonResponse::CoverCheck { response })
        }
        DaemonRequest::TestShard { request } => {
            let response = run_test_shard(request)?;
            Ok(DaemonResponse::TestShard { response })
        }
        DaemonRequest::TestMap { request } => {
            let response = run_test_map(request)?;
            Ok(DaemonResponse::TestMap { response })
        }
        DaemonRequest::ContextStoreList { request } => {
            let response = run_context_store_list(request)?;
            Ok(DaemonResponse::ContextStoreList { response })
        }
        DaemonRequest::ContextStoreGet { request } => {
            let response = run_context_store_get(request)?;
            Ok(DaemonResponse::ContextStoreGet { response })
        }
        DaemonRequest::ContextStorePrune { request } => {
            let response = run_context_store_prune(request)?;
            Ok(DaemonResponse::ContextStorePrune { response })
        }
        DaemonRequest::ContextStoreStats { request } => {
            let response = run_context_store_stats(request)?;
            Ok(DaemonResponse::ContextStoreStats { response })
        }
        DaemonRequest::ContextRecall { request } => {
            let response = run_context_recall(request)?;
            Ok(DaemonResponse::ContextRecall { response })
        }
    }
}

fn build_status(state: &DaemonState) -> Result<DaemonStatus> {
    Ok(DaemonStatus {
        pid: state.runtime.pid,
        socket_path: state.runtime.socket_path.clone(),
        workspace_root: state.runtime.workspace_root.clone(),
        started_at_unix: state.runtime.started_at_unix,
        uptime_secs: now_unix().saturating_sub(state.runtime.started_at_unix),
        tasks: state.tasks.tasks.values().cloned().collect(),
        watches: state.watches.watches.clone(),
    })
}

fn register_task_and_watches(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    mut spec: TaskSubmitSpec,
) -> Result<(TaskRecord, Vec<WatchRegistration>)> {
    if spec.task_id.trim().is_empty() {
        anyhow::bail!("task_id cannot be empty");
    }
    spec.sequence.reactive.enabled = true;
    spec.sequence.reactive.task_id = Some(spec.task_id.clone());

    let removed_watch_ids = {
        let guard = state.lock().map_err(lock_err)?;
        guard
            .tasks
            .tasks
            .get(&spec.task_id)
            .map(|task| task.watch_ids.clone())
            .unwrap_or_default()
    };
    for watch_id in removed_watch_ids {
        let _ = remove_watch(state.clone(), &watch_id)?;
    }

    let mut registrations = Vec::new();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let watch_ids = spec
            .watches
            .iter()
            .map(|watch| {
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
            running: false,
            cancel_requested: false,
            pending_replan: false,
            last_request_id: None,
            last_started_at_unix: None,
            last_completed_at_unix: None,
            last_replan_at_unix: None,
            last_error: None,
            watch_ids,
            sequence_present: true,
            sequence: Some(spec.sequence.clone()),
            last_sequence_metadata: None,
        };
        guard.tasks.tasks.insert(spec.task_id.clone(), task.clone());
        persist_state(&guard)?;
    }

    for registration in &registrations {
        install_watch(state.clone(), watch_tx.clone(), registration.watch_id.clone())?;
    }

    let task = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&spec.task_id)
        .cloned()
        .ok_or_else(|| anyhow!("task disappeared after registration"))?;
    Ok((task, registrations))
}

fn run_sequence_for_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<context_kernel_core::KernelSequenceResponse> {
    loop {
        let (kernel, sequence) = {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            let sequence = task
                .sequence
                .clone()
                .ok_or_else(|| anyhow!("task '{}' has no stored sequence", task_id))?;
            task.running = true;
            task.pending_replan = false;
            task.last_started_at_unix = Some(now_unix());
            task.last_error = None;
            persist_state(&guard)?;
            (guard.kernel.clone(), sequence)
        };

        let result = kernel.execute_sequence(sequence);

        let rerun = {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            task.running = false;
            task.last_completed_at_unix = Some(now_unix());
            match &result {
                Ok(response) => {
                    task.last_request_id = Some(response.request_id);
                    task.last_sequence_metadata = Some(response.metadata.clone());
                    task.last_error = None;
                }
                Err(err) => {
                    task.last_error = Some(err.to_string());
                }
            }
            let rerun = task.pending_replan && !task.cancel_requested;
            if rerun {
                task.last_replan_at_unix = Some(now_unix());
            }
            persist_state(&guard)?;
            rerun
        };

        match result {
            Ok(response) if rerun => continue,
            Ok(response) => return Ok(response),
            Err(err) => return Err(err.into()),
        }
    }
}

fn cancel_task(state: Arc<Mutex<DaemonState>>, task_id: &str) -> Result<(Option<TaskRecord>, Vec<String>)> {
    let watch_ids = {
        let mut guard = state.lock().map_err(lock_err)?;
        let Some(task) = guard.tasks.tasks.get_mut(task_id) else {
            return Ok((None, Vec::new()));
        };
        task.cancel_requested = true;
        task.watch_ids.clone()
    };
    for watch_id in &watch_ids {
        let _ = remove_watch(state.clone(), watch_id)?;
    }
    let mut guard = state.lock().map_err(lock_err)?;
    let removed = guard.tasks.tasks.remove(task_id);
    persist_state(&guard)?;
    Ok((removed, watch_ids))
}

fn remove_watch(state: Arc<Mutex<DaemonState>>, watch_id: &str) -> Result<Option<WatchRegistration>> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard.watcher_handles.remove(watch_id);
    let removed = if let Some(index) = guard
        .watches
        .watches
        .iter()
        .position(|watch| watch.watch_id == watch_id)
    {
        Some(guard.watches.watches.remove(index))
    } else {
        None
    };
    for task in guard.tasks.tasks.values_mut() {
        task.watch_ids.retain(|candidate| candidate != watch_id);
    }
    persist_state(&guard)?;
    Ok(removed)
}

fn restore_watchers(state: &Arc<Mutex<DaemonState>>, watch_tx: &Sender<WatchEventMsg>) -> Result<()> {
    let watch_ids = state
        .lock()
        .map_err(lock_err)?
        .watches
        .watches
        .iter()
        .map(|watch| watch.watch_id.clone())
        .collect::<Vec<_>>();
    for watch_id in watch_ids {
        install_watch(state.clone(), watch_tx.clone(), watch_id)?;
    }
    Ok(())
}

fn install_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    watch_id: String,
) -> Result<()> {
    let spec = {
        let guard = state.lock().map_err(lock_err)?;
        guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == watch_id)
            .map(|watch| watch.spec.clone())
            .ok_or_else(|| anyhow!("unknown watch '{watch_id}'"))?
    };

    let callback_watch_id = watch_id.clone();
    let mut watcher = PollWatcher::new(
        move |result: notify::Result<Event>| match result {
            Ok(event) => {
                let _ = watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    paths: event.paths,
                    error: None,
                });
            }
            Err(err) => {
                let _ = watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    paths: Vec::new(),
                    error: Some(err.to_string()),
                });
            }
        },
        Config::default().with_poll_interval(Duration::from_millis(spec.debounce_ms.unwrap_or(250))),
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
    if let Some(watch) = guard
        .watches
        .watches
        .iter_mut()
        .find(|watch| watch.watch_id == watch_id)
    {
        watch.active = true;
        watch.last_error = None;
    }
    guard.watcher_handles.insert(watch_id, watcher);
    persist_state(&guard)?;
    Ok(())
}

fn spawn_watch_processor(state: Arc<Mutex<DaemonState>>, watch_rx: Receiver<WatchEventMsg>) {
    thread::spawn(move || {
        while let Ok(message) = watch_rx.recv() {
            if state.lock().map_err(lock_err).map(|guard| guard.shutting_down).unwrap_or(false) {
                break;
            }
            if let Err(err) = process_watch_event(state.clone(), message) {
                let _ = err;
            }
        }
    });
}

fn process_watch_event(state: Arc<Mutex<DaemonState>>, message: WatchEventMsg) -> Result<()> {
    let (watch, kernel, sequence_present, task_id) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let watch_index = guard
            .watches
            .watches
            .iter()
            .position(|watch| watch.watch_id == message.watch_id)
            .ok_or_else(|| anyhow!("unknown watch '{}'", message.watch_id))?;
        if let Some(err) = &message.error {
            guard.watches.watches[watch_index].last_error = Some(err.clone());
            persist_state(&guard)?;
            return Ok(());
        }
        guard.watches.watches[watch_index].last_event_at_unix = Some(now_unix());
        let watch = guard.watches.watches[watch_index].clone();
        let task_id = watch.spec.task_id.clone();
        let sequence_present = guard
            .tasks
            .tasks
            .get(&task_id)
            .map(|task| task.sequence_present && !task.cancel_requested)
            .unwrap_or(false);
        let kernel = guard.kernel.clone();
        persist_state(&guard)?;
        (watch, kernel, sequence_present, task_id)
    };

    let paths = normalize_watch_paths(&watch.spec, &message.paths)?;
    if paths.is_empty() {
        return Ok(());
    }

    let event = match watch.spec.kind {
        WatchKind::Git => json!({
            "task_id": task_id,
            "event_id": format!("{}-{}", watch.watch_id, now_unix()),
            "occurred_at_unix": now_unix(),
            "actor": "packet28d.watch",
            "kind": "focus_set",
            "paths": paths,
            "symbols": [],
            "data": {
                "type": "focus_set",
                "note": "git_watch",
            }
        }),
        WatchKind::File | WatchKind::TestReport => json!({
            "task_id": task_id,
            "event_id": format!("{}-{}", watch.watch_id, now_unix()),
            "occurred_at_unix": now_unix(),
            "actor": "packet28d.watch",
            "kind": "file_edited",
            "paths": paths,
            "symbols": [],
            "data": {
                "type": "file_edited",
                "regions": [],
            }
        }),
    };
    kernel.execute(KernelRequest {
        target: "agenty.state.write".to_string(),
        reducer_input: event,
        ..KernelRequest::default()
    })?;

    if sequence_present {
        let mut spawn_replan = false;
        {
            let mut guard = state.lock().map_err(lock_err)?;
            if let Some(task) = guard.tasks.tasks.get_mut(&task_id) {
                if task.running {
                    task.pending_replan = true;
                } else {
                    spawn_replan = true;
                }
            }
            persist_state(&guard)?;
        }
        if spawn_replan {
            let state_clone = state.clone();
            thread::spawn(move || {
                let _ = run_sequence_for_task(state_clone, &task_id);
            });
        }
    }

    Ok(())
}

fn normalize_watch_paths(spec: &WatchSpec, raw_paths: &[PathBuf]) -> Result<Vec<String>> {
    let root = resolve_root(Path::new(&spec.root));
    let paths = match spec.kind {
        WatchKind::Git => git_changed_paths(&root)?,
        WatchKind::File | WatchKind::TestReport => raw_paths
            .iter()
            .filter_map(|path| path.strip_prefix(&root).ok().map(|path| path.to_path_buf()))
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect(),
    };
    let includes = spec
        .include_globs
        .iter()
        .filter_map(|glob| Pattern::new(glob).ok())
        .collect::<Vec<_>>();
    let excludes = spec
        .exclude_globs
        .iter()
        .filter_map(|glob| Pattern::new(glob).ok())
        .collect::<Vec<_>>();

    let mut filtered = Vec::new();
    for path in paths {
        let include_ok = includes.is_empty() || includes.iter().any(|glob| glob.matches(&path));
        let exclude_hit = excludes.iter().any(|glob| glob.matches(&path));
        if include_ok && !exclude_hit && !filtered.iter().any(|candidate| candidate == &path) {
            filtered.push(path);
        }
    }
    Ok(filtered)
}

fn git_changed_paths(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=no")
        .current_dir(root)
        .output()
        .context("failed to run git status")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in stdout.lines() {
        if line.len() > 3 {
            let path = line[3..].trim().replace('\\', "/");
            if !path.is_empty() && !paths.iter().any(|candidate| candidate == &path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

fn watch_paths(spec: &WatchSpec) -> Vec<PathBuf> {
    let root = resolve_root(Path::new(&spec.root));
    if spec.paths.is_empty() {
        return vec![root];
    }
    spec.paths
        .iter()
        .map(|path| {
            let candidate = PathBuf::from(path);
            if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            }
        })
        .collect()
}

fn watch_id_for(spec: &WatchSpec) -> String {
    let mut seed = format!("{:?}:{}:{}", spec.kind, spec.task_id, spec.root);
    for path in &spec.paths {
        seed.push(':');
        seed.push_str(path);
    }
    let hash = blake3::hash(seed.as_bytes()).to_hex();
    format!("watch-{}", &hash[..12])
}

fn persist_state(state: &DaemonState) -> Result<()> {
    save_watch_registry(&state.root, &state.watches)?;
    save_task_registry(&state.root, &state.tasks)?;
    Ok(())
}

fn resolve_root(path: &Path) -> PathBuf {
    let mut current = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    loop {
        if current.join(".git").exists() {
            return current;
        }
        if !current.pop() {
            return path.to_path_buf();
        }
    }
}

fn lock_err<T>(err: std::sync::PoisonError<T>) -> anyhow::Error {
    anyhow!("daemon state lock poisoned: {err}")
}

fn run_cover_check(request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    let config = suite_foundation_core::CovyConfig::load(Path::new(&request.config_path))
        .unwrap_or_default();
    let base = request.base.as_deref().unwrap_or(&config.diff.base);
    let head = request.head.as_deref().unwrap_or(&config.diff.head);
    let issue_gate = suite_foundation_core::config::IssueGateConfig {
        max_new_errors: request.max_new_errors.or(config.gate.issues.max_new_errors),
        max_new_warnings: request
            .max_new_warnings
            .or(config.gate.issues.max_new_warnings),
        max_new_issues: config.gate.issues.max_new_issues,
    };
    let gate_config = suite_foundation_core::config::GateConfig {
        fail_under_total: request.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: request
            .fail_under_changed
            .or(config.gate.fail_under_changed),
        fail_under_new: request.fail_under_new.or(config.gate.fail_under_new),
        issues: issue_gate,
    };
    let coverage_format = parse_format(&request.format)?;
    let source_root = request.source_root.as_ref().map(PathBuf::from);
    let strip_prefixes: Vec<String> = request
        .strip_prefix
        .iter()
        .cloned()
        .chain(config.ingest.strip_prefixes.iter().cloned())
        .collect();

    let mut coverage_paths = request.coverage.clone();
    coverage_paths.extend(request.paths.clone());
    let pipeline_request = diffy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: coverage_paths,
            format: coverage_format,
            stdin: false,
            input_state_path: request.input.clone(),
            default_input_state_path: Some(".covy/state/latest.bin".to_string()),
            strip_prefixes,
            reject_paths_with_input: true,
            no_inputs_error:
                "No coverage files specified. Provide file paths, use --stdin, or run `covy ingest` first."
                    .to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: request.issues.clone(),
            issues_state_path: request.issues_state.clone(),
            no_issues_state: request.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };
    let output = diffy_core::pipeline::run_analysis(
        pipeline_request,
        &diffy_core::pipeline::PipelineIngestAdapters {
            ingest_coverage_auto: |path| covy_ingest::ingest_path(path).map_err(Into::into),
            ingest_coverage_with_format: |path, format| {
                covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
            },
            ingest_coverage_stdin: |_format| anyhow::bail!("stdin is not supported through packet28d"),
            ingest_diagnostics: |path| covy_ingest::ingest_diagnostics_path(path).map_err(Into::into),
        },
    )?;

    let gate_json = serde_json::to_value(&output.gate_result).unwrap_or_default();
    let gate_json_bytes = serde_json::to_vec(&gate_json).unwrap_or_default();
    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();
    let envelope = suite_packet_core::EnvelopeV1 {
        version: "1".to_string(),
        tool: "covy".to_string(),
        kind: "coverage_gate".to_string(),
        hash: String::new(),
        summary: format!(
            "passed={} changed={:?} total={:?} new={:?}",
            output.gate_result.passed,
            output.gate_result.changed_coverage_pct,
            output.gate_result.total_coverage_pct,
            output.gate_result.new_file_coverage_pct
        ),
        files: changed_paths
            .iter()
            .map(|path: &String| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(0.75),
                source: Some("cover.check".to_string()),
            })
            .collect(),
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: suite_packet_core::BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((gate_json_bytes.len() / 4) as u64),
            payload_est_bytes: Some(gate_json_bytes.len()),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: now_unix(),
        },
        payload: gate_json,
    }
    .with_canonical_hash_and_real_budget();

    Ok(CoverCheckResponse {
        exit_code: if output.gate_result.passed { 0 } else { 1 },
        packet_type: suite_packet_core::PACKET_TYPE_COVER_CHECK.to_string(),
        envelope: serde_json::from_value(serde_json::to_value(envelope)?)?,
    })
}

fn run_test_shard(request: TestShardRequest) -> Result<TestShardResponse> {
    if request.schema {
        return Ok(TestShardResponse {
            schema: Some(testy_core::command_shard::SHARD_PLAN_SCHEMA_EXAMPLES.to_string()),
            plan: None,
        });
    }

    let plan = testy_core::command_shard::run_shard_plan_command(
        testy_core::command_shard::ShardPlanArgs {
            shards: request.shards,
            tasks_json: request.tasks_json,
            tier: request.tier,
            include_tag: request.include_tag,
            exclude_tag: request.exclude_tag,
            tests_file: request.tests_file,
            impact_json: request.impact_json,
            timings: request.timings,
            unknown_test_seconds: request.unknown_test_seconds,
            algorithm: parse_shard_algorithm(request.algorithm.as_deref())?,
            write_files: request.write_files,
        },
        &request.config_path,
    )?;

    Ok(TestShardResponse {
        schema: None,
        plan: Some(plan),
    })
}

fn run_test_map(request: TestMapRequest) -> Result<TestMapResponse> {
    if request.schema {
        return Ok(TestMapResponse {
            schema: Some(testy_core::pipeline_testmap::TESTMAP_MANIFEST_SCHEMA_EXAMPLE.to_string()),
            warnings: Vec::new(),
            summary: None,
        });
    }

    let adapters = testy_core::pipeline_testmap::TestMapAdapters {
        ingest_coverage: |path| covy_ingest::ingest_path(path).map_err(Into::into),
    };
    let output = testy_core::command_testmap::run_testmap_build(
        testy_core::command_testmap::TestmapBuildArgs {
            manifest: request.manifest,
            output: request.output,
            timings_output: request.timings_output,
        },
        &adapters,
    )?;

    Ok(TestMapResponse {
        schema: None,
        warnings: output.warnings,
        summary: Some(TestMapSummary {
            manifest_files: output.summary.manifest_files,
            records: output.summary.records,
            tests: output.summary.tests,
            files: output.summary.files,
            output_testmap_path: output.summary.output_testmap_path,
            output_timings_path: output.summary.output_timings_path,
        }),
    })
}

fn run_context_store_list(request: ContextStoreListRequest) -> Result<ContextStoreListResponse> {
    let cache = load_cache_root(&request.root);
    let entries = cache.list_entries(
        &ContextStoreListFilter {
            target: request.target,
            contains_query: request.query,
            created_after_unix: request.created_after,
            created_before_unix: request.created_before,
        },
        &ContextStorePaging {
            offset: request.offset,
            limit: request.limit,
        },
    );
    Ok(ContextStoreListResponse { entries })
}

fn run_context_store_get(request: ContextStoreGetRequest) -> Result<ContextStoreGetResponse> {
    let cache = load_cache_root(&request.root);
    Ok(ContextStoreGetResponse {
        entry: cache.get_entry(&request.key),
    })
}

fn run_context_store_prune(
    request: ContextStorePruneDaemonRequest,
) -> Result<ContextStorePruneResponse> {
    let root = std::path::PathBuf::from(&request.root);
    let config = MemoryPersistConfig::new(root.clone());
    let mut cache = PacketCache::load_from_disk(&config);
    let report = cache.prune(ContextStorePruneRequest {
        all: request.all,
        ttl_secs: request.ttl_secs,
    });
    cache
        .save_to_disk(&config)
        .with_context(|| format!("failed to save context store at '{}'", root.display()))?;
    Ok(ContextStorePruneResponse { report })
}

fn run_context_store_stats(request: ContextStoreStatsRequest) -> Result<ContextStoreStatsResponse> {
    let cache = load_cache_root(&request.root);
    Ok(ContextStoreStatsResponse { stats: cache.stats() })
}

fn run_context_recall(request: ContextRecallRequest) -> Result<ContextRecallResponse> {
    let cache = load_cache_root(&request.root);
    let now = now_unix();
    let since_default = now.saturating_sub(86_400);
    let hits = cache.recall(
        &request.query,
        &RecallOptions {
            limit: request.limit,
            since_unix: request.since.or(Some(since_default)),
            until_unix: request.until,
            target: request.target,
        },
    );
    Ok(ContextRecallResponse {
        query: request.query,
        hits,
    })
}

fn load_cache_root(root: &str) -> PacketCache {
    PacketCache::load_from_disk(&MemoryPersistConfig::new(std::path::PathBuf::from(root)))
}

fn parse_shard_algorithm(
    value: Option<&str>,
) -> Result<Option<testy_core::command_shard::PlannerAlgorithmArg>> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some("lpt") => Ok(Some(testy_core::command_shard::PlannerAlgorithmArg::Lpt)),
        Some("whale-lpt") => Ok(Some(testy_core::command_shard::PlannerAlgorithmArg::WhaleLpt)),
        Some(other) => Err(anyhow!(
            "unsupported shard algorithm '{other}'. Expected 'lpt' or 'whale-lpt'"
        )),
    }
}

fn parse_format(value: &str) -> Result<Option<CoverageFormat>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "lcov" => Ok(Some(CoverageFormat::Lcov)),
        "cobertura" => Ok(Some(CoverageFormat::Cobertura)),
        "jacoco" => Ok(Some(CoverageFormat::JaCoCo)),
        "gocov" => Ok(Some(CoverageFormat::GoCov)),
        "llvm-cov" | "llvmcov" => Ok(Some(CoverageFormat::LlvmCov)),
        other => Err(anyhow!("unsupported coverage format '{other}'")),
    }
}
