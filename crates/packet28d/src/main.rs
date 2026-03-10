use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, ErrorKind};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use context_kernel_core::{
    normalize_sequence_request, Kernel, KernelFailure, KernelRequest, KernelResponse,
    KernelStepRequest, PersistConfig, SequenceObserver,
};
use context_memory_core::{
    ContextStoreListFilter, ContextStorePaging, ContextStorePruneRequest, PacketCache,
    PersistConfig as MemoryPersistConfig, RecallOptions,
};
use diffy_core::model::CoverageFormat;
use glob::Pattern;
use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};
use packet28_daemon_core::{
    append_task_event, ensure_daemon_dir, load_task_events, load_task_registry,
    load_watch_registry, log_path, now_unix, read_socket_message, ready_path, remove_runtime_files,
    save_task_registry, save_watch_registry, socket_path, task_brief_json_path,
    task_brief_markdown_path, task_event_log_path, task_state_json_path, task_version_json_path,
    write_runtime_info, write_socket_message, BrokerAction, BrokerDecision, BrokerDecomposeIntent,
    BrokerDecomposeRequest, BrokerDecomposeResponse, BrokerDecomposedStep, BrokerDeltaResponse,
    BrokerEstimateContextRequest, BrokerEstimateContextResponse, BrokerEvictionCandidate,
    BrokerGetContextRequest, BrokerGetContextResponse, BrokerPacketRef, BrokerPlanStep,
    BrokerPlanViolation, BrokerQuestion, BrokerRecommendedAction, BrokerResolvedQuestion,
    BrokerResponseMode, BrokerSection, BrokerSectionEstimate, BrokerSourceKind,
    BrokerSupersessionMode, BrokerTaskStatusRequest, BrokerTaskStatusResponse,
    BrokerToolResultKind, BrokerValidatePlanRequest, BrokerValidatePlanResponse, BrokerVerbosity,
    BrokerWriteOp, BrokerWriteStateRequest, BrokerWriteStateResponse, ContextRecallRequest,
    ContextRecallResponse, ContextStoreGetRequest, ContextStoreGetResponse,
    ContextStoreListRequest, ContextStoreListResponse, ContextStorePruneDaemonRequest,
    ContextStorePruneResponse, ContextStoreStatsRequest, ContextStoreStatsResponse,
    CoverCheckRequest, CoverCheckResponse, DaemonEvent, DaemonEventFrame, DaemonRequest,
    DaemonResponse, DaemonRuntimeInfo, DaemonStatus, PacketFetchResponse, TaskRecord, TaskRegistry,
    TaskSubmitSpec, TestMapRequest, TestMapResponse, TestMapSummary, TestShardRequest,
    TestShardResponse, WatchKind, WatchRegistration, WatchRegistry, WatchSpec,
};
use serde_json::{json, Value};

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

struct PendingWatchEvent {
    watch_id: String,
    paths: Vec<PathBuf>,
    error: Option<String>,
    due_at: Instant,
}

struct DaemonState {
    root: PathBuf,
    kernel: Arc<Kernel>,
    runtime: DaemonRuntimeInfo,
    tasks: TaskRegistry,
    watches: WatchRegistry,
    watcher_handles: HashMap<String, PollWatcher>,
    subscribers: HashMap<String, Vec<Sender<DaemonEventFrame>>>,
    shutting_down: bool,
}

struct TaskSequenceObserver {
    state: Arc<Mutex<DaemonState>>,
    task_id: String,
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

const DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS: u64 = 5_000;
const DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES: usize = 32_000;

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
    let daemon_log_path = log_path(&root);
    let socket = socket_path(&root);
    let listener = bind_listener(&socket)?;

    let runtime = DaemonRuntimeInfo {
        pid: std::process::id(),
        started_at_unix: now_unix(),
        ready_at_unix: None,
        socket_path: socket.to_string_lossy().to_string(),
        workspace_root: root.to_string_lossy().to_string(),
        log_path: daemon_log_path.to_string_lossy().to_string(),
    };
    write_runtime_info(&root, &runtime)?;
    daemon_log(&format!(
        "starting packet28d pid={} root={} log={}",
        runtime.pid,
        root.display(),
        daemon_log_path.display()
    ));

    let kernel = Arc::new(Kernel::with_v1_reducers_and_persistence(
        PersistConfig::new(root.clone()),
    ));
    let tasks = load_task_registry(&root)?;
    let watches = load_watch_registry(&root)?;
    let state = Arc::new(Mutex::new(DaemonState {
        root: root.clone(),
        kernel,
        runtime,
        tasks,
        watches,
        watcher_handles: HashMap::new(),
        subscribers: HashMap::new(),
        shutting_down: false,
    }));

    let (watch_tx, watch_rx) = mpsc::channel();
    restore_watchers(&state, &watch_tx)?;
    spawn_watch_processor(state.clone(), watch_rx);
    mark_ready(&state)?;

    loop {
        if state.lock().map_err(lock_err)?.shutting_down {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                let watch_tx = watch_tx.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_connection(state, watch_tx, stream) {
                        daemon_log(&format!("request handling failed: {err}"));
                    }
                });
            }
            Err(err) => {
                daemon_log(&format!("listener accept failed: {err}"));
                return Err(err.into());
            }
        }
    }

    daemon_log("shutting down packet28d");
    remove_runtime_files(&root)?;
    Ok(())
}

fn handle_connection(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    stream: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let request = match read_socket_message(&mut reader) {
        Ok(value) => value,
        Err(err) => {
            let response = DaemonResponse::Error {
                message: err.to_string(),
            };
            write_socket_response(&mut writer, &response)?;
            return Ok(());
        }
    };
    if let DaemonRequest::TaskSubscribe {
        task_id,
        replay_last,
    } = request
    {
        return handle_task_subscribe(state, &mut writer, task_id, replay_last);
    }
    let response = match handle_request(state, watch_tx, request) {
        Ok(value) => value,
        Err(err) => {
            daemon_log(&format!("daemon request failed: {err}"));
            DaemonResponse::Error {
                message: err.to_string(),
            }
        }
    };
    write_socket_response(&mut writer, &response)?;
    Ok(())
}

fn handle_task_subscribe(
    state: Arc<Mutex<DaemonState>>,
    writer: &mut BufWriter<UnixStream>,
    task_id: String,
    replay_last: usize,
) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let replay = load_task_events(&root, &task_id)?;
    let replay = if replay_last == 0 || replay_last >= replay.len() {
        replay
    } else {
        replay[replay.len().saturating_sub(replay_last)..].to_vec()
    };
    write_socket_response(
        writer,
        &DaemonResponse::TaskSubscribeAck {
            task_id: task_id.clone(),
            replayed: replay.len(),
        },
    )?;
    for frame in replay {
        match write_socket_message(writer, &frame) {
            Ok(()) => {}
            Err(err) if is_benign_disconnect_error(&err) => return Ok(()),
            Err(err) => return Err(err),
        }
    }

    let (tx, rx) = mpsc::channel();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        guard
            .subscribers
            .entry(task_id.clone())
            .or_default()
            .push(tx);
    }

    while let Ok(frame) = rx.recv() {
        match write_socket_message(writer, &frame) {
            Ok(()) => {}
            Err(err) if is_benign_disconnect_error(&err) => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn write_socket_response(
    writer: &mut BufWriter<UnixStream>,
    response: &DaemonResponse,
) -> Result<()> {
    match write_socket_message(writer, response) {
        Ok(()) => Ok(()),
        Err(err) if is_benign_disconnect_error(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_benign_disconnect_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| {
                matches!(
                    io_err.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
                )
            })
    })
}

fn handle_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    request: DaemonRequest,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Execute { request } => {
            let kernel = kernel_for_request(&state, &request)?;
            let response = kernel.execute(request)?;
            Ok(DaemonResponse::Execute { response })
        }
        DaemonRequest::ExecuteSequence { spec } => {
            let (task, watches) = register_task_and_watches(state.clone(), watch_tx, spec)?;
            let response = match run_sequence_for_task(state.clone(), &task.task_id) {
                Ok(response) => response,
                Err(err) => {
                    daemon_log(&format!(
                        "initial task run failed task_id={} error={err}",
                        task.task_id
                    ));
                    let _ = cancel_task(state.clone(), &task.task_id);
                    return Err(err);
                }
            };
            if let Some(failure) = response
                .step_results
                .iter()
                .find_map(|step| step.failure.as_ref())
            {
                let message = failure.message.clone();
                daemon_log(&format!(
                    "initial task run failed task_id={} error={message}",
                    task.task_id
                ));
                let _ = cancel_task(state.clone(), &task.task_id);
                return Err(anyhow!(message));
            }
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
            let root = {
                let mut guard = state.lock().map_err(lock_err)?;
                guard.shutting_down = true;
                guard.root.clone()
            };
            wake_listener(&root);
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
        DaemonRequest::TaskSubscribe { .. } => {
            Err(anyhow!("task subscribe is handled as a streaming request"))
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
        DaemonRequest::BrokerGetContext { request } => {
            let response = broker_get_context(state, request)?;
            Ok(DaemonResponse::BrokerGetContext { response })
        }
        DaemonRequest::BrokerEstimateContext { request } => {
            let response = broker_estimate_context(state, request)?;
            Ok(DaemonResponse::BrokerEstimateContext { response })
        }
        DaemonRequest::BrokerValidatePlan { request } => {
            let response = broker_validate_plan(state, request)?;
            Ok(DaemonResponse::BrokerValidatePlan { response })
        }
        DaemonRequest::BrokerDecompose { request } => {
            let response = broker_decompose(state, request)?;
            Ok(DaemonResponse::BrokerDecompose { response })
        }
        DaemonRequest::BrokerWriteState { request } => {
            let response = broker_write_state(state, request)?;
            Ok(DaemonResponse::BrokerWriteState { response })
        }
        DaemonRequest::BrokerTaskStatus { request } => {
            let response = broker_task_status(state, request)?;
            Ok(DaemonResponse::BrokerTaskStatus { response })
        }
    }
}

fn kernel_for_request(state: &Arc<Mutex<DaemonState>>, request: &KernelRequest) -> Result<Kernel> {
    if let Some(root) = persist_root_override(&request.target, &request.policy_context) {
        return Ok(Kernel::with_v1_reducers_and_persistence(
            PersistConfig::new(resolve_root(Path::new(&root))),
        ));
    }

    Ok(Kernel::with_v1_reducers_and_persistence(
        PersistConfig::new(state.lock().map_err(lock_err)?.root.clone()),
    ))
}

fn persist_root_override(target: &str, policy_context: &Value) -> Option<String> {
    if !matches!(target, "agenty.state.write" | "agenty.state.snapshot") {
        return None;
    }

    policy_context
        .get("persist_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(ToOwned::to_owned)
}

fn build_status(state: &DaemonState) -> Result<DaemonStatus> {
    Ok(DaemonStatus {
        pid: state.runtime.pid,
        socket_path: state.runtime.socket_path.clone(),
        workspace_root: state.runtime.workspace_root.clone(),
        started_at_unix: state.runtime.started_at_unix,
        ready_at_unix: state.runtime.ready_at_unix,
        log_path: state.runtime.log_path.clone(),
        uptime_secs: now_unix().saturating_sub(state.runtime.started_at_unix),
        tasks: state.tasks.tasks.values().cloned().collect(),
        watches: state.watches.watches.clone(),
    })
}

fn emit_task_event(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    kind: &str,
    data: Value,
) -> Result<()> {
    let (root, frame, subscribers) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = guard
            .tasks
            .tasks
            .entry(task_id.to_string())
            .or_insert_with(|| TaskRecord {
                task_id: task_id.to_string(),
                ..TaskRecord::default()
            });
        task.last_event_seq = task.last_event_seq.saturating_add(1);
        let frame = DaemonEventFrame {
            seq: task.last_event_seq,
            task_id: task_id.to_string(),
            event: DaemonEvent {
                kind: kind.to_string(),
                occurred_at_unix: now_unix(),
                data,
            },
        };
        let subscribers = guard.subscribers.get(task_id).cloned().unwrap_or_default();
        (guard.root.clone(), frame, subscribers)
    };
    append_task_event(&root, &frame)?;
    let mut still_open = Vec::new();
    for subscriber in subscribers {
        if subscriber.send(frame.clone()).is_ok() {
            still_open.push(subscriber);
        }
    }
    let mut guard = state.lock().map_err(lock_err)?;
    if still_open.is_empty() {
        guard.subscribers.remove(task_id);
    } else {
        guard.subscribers.insert(task_id.to_string(), still_open);
    }
    persist_state(&guard)?;
    Ok(())
}

fn refresh_task_context_summary(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<Option<Value>> {
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let response = match kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": task_id,
            "budget_tokens": DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS,
            "budget_bytes": DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES,
            "scope": "task_first",
        }),
        ..KernelRequest::default()
    }) {
        Ok(response) => response,
        Err(err) => {
            daemon_log(&format!(
                "context manage refresh failed task_id={task_id}: {err}"
            ));
            return Ok(None);
        }
    };
    let Some(packet) = response.output_packets.first() else {
        return Ok(None);
    };
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!(source.to_string()))?;
    let summary = json!({
        "working_set_tokens": envelope.payload.budget.working_set_tokens,
        "evictable_tokens": envelope.payload.budget.evictable_tokens,
        "changed_paths_since_checkpoint": envelope.payload.changed_paths_since_checkpoint.len(),
        "changed_symbols_since_checkpoint": envelope.payload.changed_symbols_since_checkpoint.len(),
    });
    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(task) = guard.tasks.tasks.get_mut(task_id) {
        task.last_context_refresh_at_unix = Some(now_unix());
        task.working_set_est_tokens = envelope.payload.budget.working_set_tokens;
        task.evictable_est_tokens = envelope.payload.budget.evictable_tokens;
        task.changed_since_checkpoint_paths = envelope.payload.changed_paths_since_checkpoint.len();
        task.changed_since_checkpoint_symbols =
            envelope.payload.changed_symbols_since_checkpoint.len();
    }
    persist_state(&guard)?;
    Ok(Some(summary))
}

fn broker_default_budget_tokens() -> u64 {
    DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS
}

fn broker_default_budget_bytes() -> usize {
    DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES
}

fn ensure_task_record_mut<'a>(tasks: &'a mut TaskRegistry, task_id: &str) -> &'a mut TaskRecord {
    tasks
        .tasks
        .entry(task_id.to_string())
        .or_insert_with(|| TaskRecord {
            task_id: task_id.to_string(),
            ..TaskRecord::default()
        })
}

fn next_context_version(current: Option<&str>) -> String {
    current
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1)
        .to_string()
}

fn ensure_context_version(task: &mut TaskRecord) -> String {
    let version = task
        .latest_context_version
        .clone()
        .unwrap_or_else(|| next_context_version(None));
    task.latest_context_version = Some(version.clone());
    version
}

fn bump_context_version(state: &Arc<Mutex<DaemonState>>, task_id: &str) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, task_id);
    let version = next_context_version(task.latest_context_version.as_deref());
    task.latest_context_version = Some(version.clone());
    persist_state(&guard)?;
    Ok(version)
}

fn set_context_reason(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, task_id);
    task.latest_context_reason = Some(reason.into());
    persist_state(&guard)?;
    Ok(())
}

fn current_context_version(state: &Arc<Mutex<DaemonState>>, task_id: &str) -> Result<String> {
    let mut guard = state.lock().map_err(lock_err)?;
    let version = ensure_context_version(ensure_task_record_mut(&mut guard.tasks, task_id));
    persist_state(&guard)?;
    Ok(version)
}

fn update_broker_link_state(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerWriteStateRequest,
) -> Result<()> {
    let mut guard = state.lock().map_err(lock_err)?;
    let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
    match request.op.unwrap_or(BrokerWriteOp::FileRead) {
        BrokerWriteOp::QuestionOpen => {
            if let (Some(question_id), Some(text)) = (&request.question_id, &request.text) {
                task.question_texts
                    .insert(question_id.clone(), text.clone());
                task.resolved_questions.remove(question_id);
            }
        }
        BrokerWriteOp::QuestionResolve => {
            if let Some(question_id) = &request.question_id {
                task.question_texts
                    .entry(question_id.clone())
                    .or_insert_with(|| "resolved question".to_string());
                if let Some(decision_id) = &request.resolution_decision_id {
                    task.resolved_questions
                        .insert(question_id.clone(), decision_id.clone());
                    task.linked_decisions
                        .insert(decision_id.clone(), question_id.clone());
                } else {
                    task.resolved_questions
                        .entry(question_id.clone())
                        .or_insert_with(String::new);
                }
            }
        }
        BrokerWriteOp::DecisionAdd => {
            if let (Some(decision_id), Some(question_id)) =
                (&request.decision_id, &request.resolves_question_id)
            {
                task.linked_decisions
                    .insert(decision_id.clone(), question_id.clone());
                task.resolved_questions
                    .insert(question_id.clone(), decision_id.clone());
            }
        }
        BrokerWriteOp::DecisionSupersede => {
            if let Some(decision_id) = &request.decision_id {
                task.linked_decisions.remove(decision_id);
                task.resolved_questions
                    .retain(|_, linked_decision_id| linked_decision_id != decision_id);
            }
        }
        _ => {}
    }
    persist_state(&guard)?;
    Ok(())
}

fn load_agent_snapshot_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<suite_packet_core::AgentSnapshotPayload> {
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let response = kernel.execute(KernelRequest {
        target: "agenty.state.snapshot".to_string(),
        reducer_input: json!({ "task_id": task_id }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no agent snapshot packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid agent snapshot packet: {source}"))?;
    Ok(envelope.payload)
}

fn load_context_manage_for_task(
    kernel: &Arc<context_kernel_core::Kernel>,
    request: &BrokerGetContextRequest,
    focus_paths: &[String],
    focus_symbols: &[String],
) -> Result<suite_packet_core::ContextManagePayload> {
    let response = kernel.execute(KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": request.task_id,
            "query": request.query,
            "budget_tokens": request.budget_tokens.unwrap_or_else(broker_default_budget_tokens),
            "budget_bytes": request.budget_bytes.unwrap_or_else(broker_default_budget_bytes),
            "scope": "task_first",
            "focus_paths": focus_paths,
            "focus_symbols": focus_symbols,
        }),
        policy_context: json!({
            "task_id": request.task_id,
        }),
        ..KernelRequest::default()
    })?;
    let packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no context manage packet"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(packet.body.clone())
            .map_err(|source| anyhow!("invalid context manage packet: {source}"))?;
    Ok(envelope.payload)
}

fn load_repo_map_for_task(
    request: &BrokerGetContextRequest,
    focus_paths: &[String],
    focus_symbols: &[String],
    root: &Path,
) -> Result<Option<suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>>> {
    let action = request.action.unwrap_or(BrokerAction::Plan);
    if !matches!(
        action,
        BrokerAction::Plan | BrokerAction::Inspect | BrokerAction::Edit | BrokerAction::Summarize
    ) {
        return Ok(None);
    }

    Ok(Some(build_repo_map_envelope(
        root,
        focus_paths,
        focus_symbols,
        12,
        24,
    )?))
}

fn build_repo_map_envelope(
    root: &Path,
    focus_paths: &[String],
    focus_symbols: &[String],
    max_files: usize,
    max_symbols: usize,
) -> Result<suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>> {
    mapy_core::build_repo_map(mapy_core::RepoMapRequest {
        repo_root: root.to_string_lossy().to_string(),
        focus_paths: focus_paths.to_vec(),
        focus_symbols: focus_symbols.to_vec(),
        max_files,
        max_symbols,
        include_tests: true,
    })
    .map_err(|source| anyhow!(source.to_string()))
}

fn load_cached_coverage(root: &Path) -> Result<Option<suite_packet_core::CoverageData>> {
    let path = root.join(".covy").join("state").join("latest.bin");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cached coverage state '{}'", path.display()))?;
    let coverage = suite_foundation_core::cache::deserialize_coverage(&bytes)
        .map_err(|source| anyhow!(source.to_string()))?;
    Ok(Some(coverage))
}

fn load_cached_testmap(root: &Path) -> Result<Option<suite_packet_core::TestMapIndex>> {
    let path = root.join(".covy").join("state").join("testmap.bin");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(testy_core::pipeline_testmap::load_testmap(&path)?))
}

fn normalize_plan_steps(steps: &[BrokerPlanStep]) -> Vec<BrokerPlanStep> {
    steps
        .iter()
        .enumerate()
        .map(|(idx, step)| {
            let mut normalized = step.clone();
            if normalized.id.trim().is_empty() {
                normalized.id = format!("step-{}", idx + 1);
            } else {
                normalized.id = normalized.id.trim().to_string();
            }
            normalized.action = normalized.action.trim().to_ascii_lowercase();
            normalized.description = normalized
                .description
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            normalized.paths = merged_unique(&[], &step.paths);
            normalized.symbols = merged_unique(&[], &step.symbols);
            normalized.depends_on = merged_unique(&[], &step.depends_on);
            normalized
        })
        .collect()
}

fn is_edit_like_action(action: &str) -> bool {
    matches!(
        action,
        "edit"
            | "file_edit"
            | "rename"
            | "extract"
            | "split_file"
            | "merge_files"
            | "restructure_module"
    )
}

fn is_test_like_step(step: &BrokerPlanStep) -> bool {
    step.action.contains("test")
        || step
            .description
            .as_deref()
            .is_some_and(|text| text.to_ascii_lowercase().contains("test"))
        || step.paths.iter().any(|path| {
            let lower = path.to_ascii_lowercase();
            lower.contains("test") || lower.contains("/spec") || lower.ends_with("_test.rs")
        })
}

fn estimate_plan_step_tokens(step: &BrokerPlanStep) -> u64 {
    let mut estimate = 48_u64;
    estimate = estimate.saturating_add((step.paths.len() as u64) * 40);
    estimate = estimate.saturating_add((step.symbols.len() as u64) * 24);
    estimate = estimate.saturating_add((step.depends_on.len() as u64) * 8);
    if let Some(description) = &step.description {
        estimate = estimate.saturating_add(estimate_text_cost(description).0);
    }
    estimate.max(64)
}

fn tokenize_task_text(task_text: &str) -> Vec<String> {
    task_text
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Debug, Clone, Default)]
struct QueryFocus {
    raw_query: Option<String>,
    text_tokens: Vec<String>,
    full_symbol_terms: Vec<String>,
    symbol_terms: Vec<String>,
    path_terms: Vec<String>,
}

fn derive_query_focus(query: Option<&str>) -> QueryFocus {
    let raw_query = query
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let Some(raw_query) = raw_query else {
        return QueryFocus::default();
    };

    let text_tokens = tokenize_task_text(&raw_query);
    let mut full_symbol_terms = Vec::new();
    let mut symbol_terms = Vec::new();
    let mut path_terms = Vec::new();
    let mut seen_full = HashSet::new();
    let mut seen_symbols = HashSet::new();
    let mut seen_paths = HashSet::new();

    for raw_part in raw_query.split_whitespace() {
        let cleaned = trim_query_fragment(raw_part);
        if cleaned.is_empty() {
            continue;
        }
        if looks_like_query_path(&cleaned) && seen_paths.insert(cleaned.to_ascii_lowercase()) {
            path_terms.push(cleaned.clone());
        }
        if looks_like_symbol_term(&cleaned) {
            if seen_full.insert(cleaned.to_ascii_lowercase()) {
                full_symbol_terms.push(cleaned.clone());
            }
            for piece in expand_identifier_pieces(&cleaned) {
                let lowered = piece.to_ascii_lowercase();
                if piece.len() >= 3 && seen_symbols.insert(lowered) {
                    symbol_terms.push(piece);
                }
            }
        }
    }

    for token in &text_tokens {
        if token.len() >= 3 && seen_symbols.insert(token.to_ascii_lowercase()) {
            symbol_terms.push(token.clone());
        }
    }

    QueryFocus {
        raw_query: Some(raw_query),
        text_tokens,
        full_symbol_terms,
        symbol_terms,
        path_terms,
    }
}

fn trim_query_fragment(fragment: &str) -> String {
    fragment
        .trim_matches(|ch: char| {
            !ch.is_ascii_alphanumeric()
                && !matches!(ch, '_' | '-' | '.' | '/' | '\\' | ':' )
        })
        .trim_end_matches("()")
        .to_string()
}

fn looks_like_query_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".json", ".md", ".py", ".java", ".go", ".kt",
        ]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn looks_like_symbol_term(value: &str) -> bool {
    value.contains('.')
        || value.contains("::")
        || value.contains('_')
        || value
            .chars()
            .any(|ch| ch.is_ascii_uppercase())
        || value
            .chars()
            .any(|ch| ch.is_ascii_alphabetic())
}

fn expand_identifier_pieces(value: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    for part in value
        .replace("::", ".")
        .replace(['/', '\\', '.', '_', '-'], " ")
        .split_whitespace()
    {
        if !part.is_empty() {
            pieces.push(part.to_string());
        }
        pieces.extend(split_camel_identifier(part));
    }
    pieces
        .into_iter()
        .filter(|piece| !piece.trim().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn split_camel_identifier(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars = value.chars().collect::<Vec<_>>();
    for (idx, ch) in chars.iter().enumerate() {
        let prev = idx.checked_sub(1).and_then(|pos| chars.get(pos));
        let next = chars.get(idx + 1);
        let boundary = prev.is_some_and(|prev_ch| {
            (prev_ch.is_ascii_lowercase() && ch.is_ascii_uppercase())
                || (prev_ch.is_ascii_alphabetic()
                    && ch.is_ascii_digit())
                || (prev_ch.is_ascii_digit()
                    && ch.is_ascii_alphabetic())
                || (prev_ch.is_ascii_uppercase()
                    && ch.is_ascii_uppercase()
                    && next.is_some_and(|next_ch| next_ch.is_ascii_lowercase()))
        });
        if boundary && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        current.push(*ch);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn scope_group(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some((prefix, _)) = normalized.split_once("/src/") {
        return prefix.to_string();
    }
    Path::new(&normalized)
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn parent_dir(path: &str) -> String {
    Path::new(path)
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn role_file_weight(path: &str) -> usize {
    let Some(file_name) = Path::new(path).file_name().and_then(|value| value.to_str()) else {
        return 0;
    };
    if file_name.starts_with("cmd_") || file_name == "report.rs" {
        3
    } else if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        2
    } else {
        0
    }
}

fn expand_scope_paths(
    task_text: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    primary_paths: &[String],
    explicit_symbols: &[String],
    max_paths: usize,
) -> Vec<String> {
    if primary_paths.is_empty() {
        return Vec::new();
    }

    let primary_set = primary_paths
        .iter()
        .map(|path| path.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let primary_scopes = primary_paths
        .iter()
        .map(|path| scope_group(path))
        .collect::<HashSet<_>>();
    let primary_dirs = primary_paths
        .iter()
        .map(|path| parent_dir(path))
        .collect::<HashSet<_>>();
    let task_tokens = tokenize_task_text(task_text);
    let explicit_symbols = explicit_symbols
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    let mut edge_counts = HashMap::<String, usize>::new();
    for edge in &rich_map.edges {
        let from_is_primary = primary_set.contains(&edge.from.to_ascii_lowercase());
        let to_is_primary = primary_set.contains(&edge.to.to_ascii_lowercase());
        if from_is_primary && !to_is_primary {
            *edge_counts.entry(edge.to.clone()).or_insert(0) += 1;
        }
        if to_is_primary && !from_is_primary {
            *edge_counts.entry(edge.from.clone()).or_insert(0) += 1;
        }
    }

    let mut symbol_hits = HashMap::<String, usize>::new();
    for symbol in &rich_map.symbols_ranked {
        let symbol_name = symbol.name.to_ascii_lowercase();
        if task_tokens
            .iter()
            .any(|token| symbol_name.contains(token.as_str()))
            || explicit_symbols
                .iter()
                .any(|token| symbol_name.contains(token.as_str()))
        {
            *symbol_hits.entry(symbol.file.clone()).or_insert(0) += 1;
        }
    }

    let mut scored = rich_map
        .files_ranked
        .iter()
        .map(|file| {
            let lower_path = file.path.to_ascii_lowercase();
            let scope = scope_group(&file.path);
            let dir = parent_dir(&file.path);
            let path_token_hits = task_tokens
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();
            let explicit_symbol_hits = explicit_symbols
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();

            let mut score = 0usize;
            if primary_set.contains(&lower_path) {
                score += 1000;
            }
            score += edge_counts.get(&file.path).copied().unwrap_or(0) * 220;
            if primary_scopes.contains(&scope) {
                score += 120;
            }
            if primary_dirs.contains(&dir) {
                score += 60;
            }
            score += (path_token_hits + explicit_symbol_hits) * 35;
            score += symbol_hits.get(&file.path).copied().unwrap_or(0) * 30;
            score += role_file_weight(&file.path)
                * if primary_scopes.contains(&scope) {
                    25
                } else {
                    10
                };

            (score, file.score, file.path.clone())
        })
        .filter(|(score, _, _)| *score > 0)
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .map(|(_, _, path)| path)
        .take(max_paths.max(primary_paths.len()))
        .collect()
}

fn derive_broker_focus_symbols(
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
) -> Vec<String> {
    let query_focus = derive_query_focus(request.query.as_deref());
    let explicit = merged_unique(&snapshot.focus_symbols, &request.focus_symbols);
    merged_unique(&explicit, &query_focus.symbol_terms)
}

fn derive_broker_focus_paths(
    root: &Path,
    objective: Option<&str>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    request: &BrokerGetContextRequest,
    max_paths: usize,
) -> Result<Vec<String>> {
    let query_focus = derive_query_focus(objective.or(request.query.as_deref()));
    let explicit_paths = merged_unique(
        &merged_unique(&snapshot.focus_paths, &request.focus_paths),
        &query_focus.path_terms,
    );
    let explicit_symbols = derive_broker_focus_symbols(snapshot, request);
    if explicit_paths.is_empty() && explicit_symbols.is_empty() && objective.is_none() {
        return Ok(Vec::new());
    }

    let wide_map = mapy_core::expand_repo_map_payload(&build_repo_map_envelope(
        root,
        &explicit_paths,
        &explicit_symbols,
        64,
        128,
    )?);
    let primary_paths = infer_scope_paths(
        objective.unwrap_or_default(),
        &wide_map,
        &explicit_paths,
        &explicit_symbols,
    );
    let expanded = expand_scope_paths(
        objective.unwrap_or_default(),
        &wide_map,
        &primary_paths,
        &explicit_symbols,
        max_paths,
    );
    Ok(merged_unique(&explicit_paths, &expanded))
}

fn infer_scope_paths(
    task_text: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    explicit_paths: &[String],
    explicit_symbols: &[String],
) -> Vec<String> {
    if !explicit_paths.is_empty() {
        return merged_unique(&[], explicit_paths);
    }

    let tokens = tokenize_task_text(task_text);
    let explicit_symbol_set = explicit_symbols
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut scored = rich_map
        .files_ranked
        .iter()
        .map(|file| {
            let lower_path = file.path.to_ascii_lowercase();
            let token_matches = tokens
                .iter()
                .filter(|token| lower_path.contains(token.as_str()))
                .count();
            let symbol_matches = rich_map
                .symbols_ranked
                .iter()
                .filter(|symbol| {
                    symbol.file == file.path
                        && explicit_symbol_set.contains(&symbol.name.to_ascii_lowercase())
                })
                .count();
            let score = token_matches + symbol_matches;
            (score, file.score, file.path.clone())
        })
        .filter(|(score, _, _)| *score > 0)
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .map(|(_, _, path)| path)
        .take(6)
        .collect()
}

fn truncate_evidence_line(line: &str, max_len: usize) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= max_len {
        trimmed.to_string()
    } else {
        let shortened = trimmed
            .chars()
            .take(max_len.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
}

#[derive(Debug, Clone, Default)]
struct CodeEvidenceMatch {
    line_no: usize,
    reason: &'static str,
    matched_symbol: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CodeEvidenceSummary {
    rendered_lines: Vec<String>,
    first_match_line: Option<usize>,
    primary_match_symbol: Option<String>,
}

fn looks_like_signature(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        return false;
    }
    let prefixes = [
        "pub fn ",
        "fn ",
        "pub struct ",
        "struct ",
        "pub enum ",
        "enum ",
        "pub trait ",
        "trait ",
        "impl ",
        "pub mod ",
        "mod ",
        "class ",
        "interface ",
        "export function ",
        "export class ",
        "def ",
    ];
    prefixes.iter().any(|prefix| trimmed.starts_with(prefix))
        || (trimmed.contains(" fn ")
            || trimmed.contains(" class ")
            || trimmed.contains(" interface "))
}

fn looks_like_low_signal_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("package ")
}

fn match_query_focus_line(line: &str, query_focus: &QueryFocus) -> Option<CodeEvidenceMatch> {
    let lower = line.to_ascii_lowercase();
    if let Some(symbol) = query_focus
        .symbol_terms
        .iter()
        .find(|symbol| lower.contains(&symbol.to_ascii_lowercase()))
    {
        return Some(CodeEvidenceMatch {
            line_no: 0,
            reason: "exact_symbol_match",
            matched_symbol: Some(symbol.clone()),
        });
    }
    if let Some(symbol) = query_focus
        .full_symbol_terms
        .iter()
        .find(|symbol| lower.contains(&symbol.to_ascii_lowercase()))
    {
        return Some(CodeEvidenceMatch {
            line_no: 0,
            reason: "full_symbol_match",
            matched_symbol: Some(symbol.clone()),
        });
    }
    if looks_like_signature(line) {
        return Some(CodeEvidenceMatch {
            line_no: 0,
            reason: "signature_match",
            matched_symbol: None,
        });
    }
    None
}

fn collapse_evidence_windows(matches: &[CodeEvidenceMatch], total_lines: usize) -> Vec<(usize, usize)> {
    let mut windows = matches
        .iter()
        .map(|matched| {
            let start = matched.line_no.saturating_sub(1).max(1);
            let end = (matched.line_no + 1).min(total_lines.max(1));
            (start, end)
        })
        .collect::<Vec<_>>();
    windows.sort_unstable();
    let mut collapsed: Vec<(usize, usize)> = Vec::new();
    for (start, end) in windows {
        if let Some((_, current_end)) = collapsed.last_mut() {
            if start <= *current_end + 1 {
                *current_end = (*current_end).max(end);
                continue;
            }
        }
        collapsed.push((start, end));
    }
    collapsed
}

fn extract_code_evidence(
    root: &Path,
    relative_path: &str,
    query_focus: &QueryFocus,
    max_windows: usize,
    max_lines: usize,
) -> CodeEvidenceSummary {
    let Ok(contents) = fs::read_to_string(root.join(relative_path)) else {
        return CodeEvidenceSummary::default();
    };
    let lines = contents.lines().collect::<Vec<_>>();
    let mut matches = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let mut matched = match_query_focus_line(line, query_focus);
        if let Some(current) = matched.as_mut() {
            current.line_no = idx + 1;
        } else if !query_focus.symbol_terms.is_empty() || !query_focus.full_symbol_terms.is_empty() {
            continue;
        }
        if looks_like_low_signal_line(line)
            && matched
                .as_ref()
                .is_none_or(|current| !matches!(current.reason, "exact_symbol_match" | "full_symbol_match"))
        {
            continue;
        }
        if let Some(matched) = matched {
            matches.push(matched);
        }
    }

    if matches.is_empty() {
        for (idx, line) in lines.iter().enumerate() {
            if looks_like_low_signal_line(line) {
                continue;
            }
            matches.push(CodeEvidenceMatch {
                line_no: idx + 1,
                reason: "fallback",
                matched_symbol: None,
            });
            break;
        }
    }

    let primary_match_symbol = matches
        .iter()
        .find_map(|matched| matched.matched_symbol.clone());
    let windows = collapse_evidence_windows(&matches, lines.len())
        .into_iter()
        .take(max_windows)
        .collect::<Vec<_>>();
    let mut rendered_lines = Vec::new();
    for (start, end) in windows {
        for line_no in start..=end {
            let Some(line) = lines.get(line_no - 1) else {
                continue;
            };
            if looks_like_low_signal_line(line)
                && !matches
                    .iter()
                    .any(|matched| matched.line_no == line_no)
            {
                continue;
            }
            rendered_lines.push(format!(
                "- {relative_path}:{} {}",
                line_no,
                truncate_evidence_line(line, 120)
            ));
            if rendered_lines.len() >= max_lines {
                return CodeEvidenceSummary {
                    first_match_line: matches.first().map(|matched| matched.line_no),
                    primary_match_symbol,
                    rendered_lines,
                };
            }
        }
    }

    CodeEvidenceSummary {
        rendered_lines,
        first_match_line: matches.first().map(|matched| matched.line_no),
        primary_match_symbol,
    }
}

fn find_candidate_test_paths(
    path: &str,
    rich_map: &mapy_core::RepoMapPayloadRich,
    testmap: Option<&suite_packet_core::TestMapIndex>,
) -> Vec<String> {
    let lower = path.to_ascii_lowercase();
    let mut candidates = HashMap::<String, usize>::new();
    for file in &rich_map.files_ranked {
        let file_lower = file.path.to_ascii_lowercase();
        if !(file_lower.contains("test") || file_lower.contains("/spec")) {
            continue;
        }
        let score = if file_lower.contains(lower.as_str()) {
            3
        } else if Path::new(&file.path)
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|stem| lower.contains(&stem.to_ascii_lowercase()))
        {
            2
        } else {
            1
        };
        candidates.insert(file.path.clone(), score);
    }
    if let Some(testmap) = testmap {
        if let Some(mapped) = testmap.file_to_tests.get(path) {
            for test_id in mapped {
                candidates.entry(test_id.clone()).or_insert(4);
            }
        }
    }
    let mut ranked = candidates.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(path, _)| path).take(3).collect()
}

fn coverage_gap_for_path(coverage: Option<&suite_packet_core::CoverageData>, path: &str) -> bool {
    let Some(coverage) = coverage else {
        return false;
    };
    coverage
        .files
        .get(path)
        .and_then(|file| file.line_coverage_pct())
        .map(|pct| pct < 80.0)
        .unwrap_or(true)
}

fn current_deleted_paths(root: &Path) -> HashSet<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                return None;
            }
            let status = &trimmed[..2];
            if !status.contains('D') {
                return None;
            }
            Some(trimmed[3..].trim().to_string())
        })
        .collect()
}

fn merged_unique(current: &[String], requested: &[String]) -> Vec<String> {
    let mut values = std::collections::BTreeSet::new();
    for value in current {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            values.insert(trimmed.to_string());
        }
    }
    for value in requested {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            values.insert(trimmed.to_string());
        }
    }
    values.into_iter().collect()
}

fn broker_objective(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
) -> Option<String> {
    if let Some(query) = request
        .query
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return Some(query.to_string());
    }
    let guard = state.lock().ok()?;
    guard
        .tasks
        .tasks
        .get(&request.task_id)
        .and_then(|task| task.latest_broker_request.as_ref())
        .and_then(|previous| previous.query.as_ref())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn broker_request_response_mode(request: &BrokerGetContextRequest) -> BrokerResponseMode {
    request.response_mode.unwrap_or(BrokerResponseMode::Full)
}

#[derive(Debug, Clone)]
struct BrokerEffectiveLimits {
    max_sections: usize,
    default_max_items_per_section: usize,
    section_item_limits: BTreeMap<String, usize>,
}

fn estimate_text_cost(text: &str) -> (u64, u64) {
    let est_bytes = text.len() as u64;
    let est_tokens = est_bytes.saturating_add(3) / 4;
    (est_tokens.max(1), est_bytes)
}

fn packet_source_kind(packet: &suite_packet_core::ContextManagePacketRef) -> BrokerSourceKind {
    if packet.target.starts_with("agenty.state.") {
        BrokerSourceKind::SelfAuthored
    } else if packet.target.starts_with("contextq.")
        || packet.target.starts_with("mapy.")
        || packet.target.starts_with("context.")
    {
        BrokerSourceKind::Derived
    } else {
        BrokerSourceKind::External
    }
}

fn section_ids_for_action(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &[
            "task_objective",
            "active_decisions",
            "open_questions",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "repo_map",
            "code_evidence",
            "relevant_context",
            "recommended_actions",
        ],
        BrokerAction::Inspect => &[
            "task_objective",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "repo_map",
            "code_evidence",
            "relevant_context",
            "checkpoint_deltas",
            "active_decisions",
            "open_questions",
        ],
        BrokerAction::ChooseTool => &[
            "task_objective",
            "recent_tool_activity",
            "tool_failures",
            "discovered_scope",
            "recommended_actions",
            "relevant_context",
            "open_questions",
            "active_decisions",
        ],
        BrokerAction::Interpret => &[
            "task_objective",
            "recent_tool_activity",
            "tool_failures",
            "code_evidence",
            "recommended_actions",
            "relevant_context",
            "active_decisions",
            "open_questions",
            "resolved_questions",
        ],
        BrokerAction::Edit => &[
            "task_objective",
            "current_focus",
            "discovered_scope",
            "recent_tool_activity",
            "tool_failures",
            "evidence_cache",
            "checkpoint_deltas",
            "active_decisions",
            "repo_map",
            "code_evidence",
            "relevant_context",
            "resolved_questions",
        ],
        BrokerAction::Summarize => &[
            "task_objective",
            "progress",
            "recent_tool_activity",
            "tool_failures",
            "active_decisions",
            "resolved_questions",
            "open_questions",
            "checkpoint_deltas",
        ],
    }
}

fn default_limits_for_action(action: BrokerAction) -> BrokerEffectiveLimits {
    let mut section_item_limits = BTreeMap::new();
    match action {
        BrokerAction::Plan => {
            section_item_limits.insert("active_decisions".to_string(), 8);
            section_item_limits.insert("open_questions".to_string(), 8);
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("repo_map".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 6);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Inspect => {
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("repo_map".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::ChooseTool => {
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("discovered_scope".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("open_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 6,
                default_max_items_per_section: 5,
                section_item_limits,
            }
        }
        BrokerAction::Interpret => {
            section_item_limits.insert("recent_tool_activity".to_string(), 4);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("recommended_actions".to_string(), 4);
            section_item_limits.insert("relevant_context".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 4);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 4,
                section_item_limits,
            }
        }
        BrokerAction::Edit => {
            section_item_limits.insert("current_focus".to_string(), 8);
            section_item_limits.insert("discovered_scope".to_string(), 8);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("evidence_cache".to_string(), 4);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            section_item_limits.insert("repo_map".to_string(), 8);
            section_item_limits.insert("code_evidence".to_string(), 6);
            section_item_limits.insert("relevant_context".to_string(), 5);
            BrokerEffectiveLimits {
                max_sections: 8,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
        BrokerAction::Summarize => {
            section_item_limits.insert("progress".to_string(), 3);
            section_item_limits.insert("recent_tool_activity".to_string(), 6);
            section_item_limits.insert("tool_failures".to_string(), 4);
            section_item_limits.insert("resolved_questions".to_string(), 6);
            section_item_limits.insert("checkpoint_deltas".to_string(), 8);
            BrokerEffectiveLimits {
                max_sections: 7,
                default_max_items_per_section: 8,
                section_item_limits,
            }
        }
    }
}

fn legacy_verbosity_limits(
    action: BrokerAction,
    verbosity: BrokerVerbosity,
) -> BrokerEffectiveLimits {
    let mut limits = default_limits_for_action(action);
    match verbosity {
        BrokerVerbosity::Compact => {
            limits.max_sections = limits.max_sections.min(4);
            limits.default_max_items_per_section = 3;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).min(3);
            }
        }
        BrokerVerbosity::Standard => {}
        BrokerVerbosity::Rich => {
            limits.max_sections = limits.max_sections.max(8);
            limits.default_max_items_per_section = 12;
            for value in limits.section_item_limits.values_mut() {
                *value = (*value).max(10);
            }
        }
    }
    limits
}

fn resolve_effective_limits(
    action: BrokerAction,
    verbosity: Option<BrokerVerbosity>,
    max_sections: Option<usize>,
    default_max_items_per_section: Option<usize>,
    section_item_limits: &BTreeMap<String, usize>,
) -> BrokerEffectiveLimits {
    let has_explicit_limits = max_sections.is_some()
        || default_max_items_per_section.is_some()
        || !section_item_limits.is_empty();
    let mut limits = if has_explicit_limits {
        default_limits_for_action(action)
    } else {
        legacy_verbosity_limits(action, verbosity.unwrap_or(BrokerVerbosity::Standard))
    };
    if let Some(value) = max_sections.filter(|value| *value > 0) {
        limits.max_sections = value;
    }
    if let Some(value) = default_max_items_per_section.filter(|value| *value > 0) {
        limits.default_max_items_per_section = value;
    }
    for (section_id, limit) in section_item_limits {
        if *limit > 0 {
            limits
                .section_item_limits
                .insert(section_id.clone(), *limit);
        }
    }
    limits
}

fn section_item_limit(limits: &BrokerEffectiveLimits, section_id: &str) -> usize {
    limits
        .section_item_limits
        .get(section_id)
        .copied()
        .unwrap_or(limits.default_max_items_per_section)
}

fn truncate_lines(lines: Vec<String>, max_lines: usize) -> String {
    lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn filter_requested_section_ids(
    action: BrokerAction,
    include_sections: &[String],
    exclude_sections: &[String],
) -> HashSet<String> {
    let mut allowed = section_ids_for_action(action)
        .iter()
        .map(|value| (*value).to_string())
        .collect::<HashSet<_>>();
    if !include_sections.is_empty() {
        allowed = include_sections
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .collect();
    }
    for excluded in exclude_sections {
        allowed.remove(excluded.trim());
    }
    allowed
}

fn load_task_record(state: &Arc<Mutex<DaemonState>>, task_id: &str) -> Option<TaskRecord> {
    state.lock().ok()?.tasks.tasks.get(task_id).cloned()
}

fn build_resolved_questions(
    task: Option<&TaskRecord>,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> Vec<BrokerResolvedQuestion> {
    let Some(task) = task else {
        return Vec::new();
    };
    let active_decisions = snapshot
        .active_decisions
        .iter()
        .map(|decision| (decision.id.as_str(), decision.text.as_str()))
        .collect::<BTreeMap<_, _>>();
    task.resolved_questions
        .iter()
        .map(|(question_id, decision_id)| BrokerResolvedQuestion {
            id: question_id.clone(),
            text: task
                .question_texts
                .get(question_id)
                .cloned()
                .unwrap_or_else(|| "resolved question".to_string()),
            resolved_by_decision_id: (!decision_id.trim().is_empty()).then(|| decision_id.clone()),
            resolution_text: (!decision_id.trim().is_empty())
                .then(|| {
                    active_decisions
                        .get(decision_id.as_str())
                        .map(|value| (*value).to_string())
                })
                .flatten(),
        })
        .collect()
}

fn score_to_string(score: f64) -> String {
    format!("{score:.2}")
}

fn render_repo_map_reason(
    path: &str,
    query_focus: &QueryFocus,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    edges: &[mapy_core::RepoEdgeRich],
    evidence: Option<&CodeEvidenceSummary>,
) -> String {
    let lower_path = path.to_ascii_lowercase();
    if let Some(symbol) = evidence
        .and_then(|summary| summary.primary_match_symbol.as_ref())
        .filter(|symbol| !symbol.trim().is_empty())
    {
        return format!("contains {symbol}");
    }
    if let Some(token) = query_focus
        .path_terms
        .iter()
        .find(|token| lower_path.contains(&token.to_ascii_lowercase()))
        .or_else(|| {
            query_focus
                .text_tokens
                .iter()
                .find(|token| lower_path.contains(token.as_str()))
        })
    {
        if lower_path.contains("test") || lower_path.contains("/spec") {
            return format!("likely test for {token}");
        }
        return format!("matches query token {token}");
    }
    if snapshot.focus_paths.iter().any(|focus| focus == path) {
        return "explicit focus path".to_string();
    }
    if snapshot
        .read_paths_by_tool
        .iter()
        .any(|summary| summary.paths.iter().any(|candidate| candidate == path))
    {
        return "read via tool".to_string();
    }
    if snapshot
        .edited_paths_by_tool
        .iter()
        .any(|summary| summary.paths.iter().any(|candidate| candidate == path))
    {
        return "edited via tool".to_string();
    }
    if edges.iter().any(|edge| {
        (edge.from == path && snapshot.focus_paths.iter().any(|focus| focus == &edge.to))
            || (edge.to == path && snapshot.focus_paths.iter().any(|focus| focus == &edge.from))
    }) {
        return "connected by import edge to focused file".to_string();
    }
    "high repo-map relevance".to_string()
}

fn shrink_section_to_budget(
    section: &BrokerSection,
    remaining_tokens: u64,
    remaining_bytes: u64,
) -> Option<BrokerSection> {
    if remaining_tokens == 0 || remaining_bytes == 0 {
        return None;
    }
    let lines = section.body.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    for line_count in (1..=lines.len()).rev() {
        let candidate_body = lines[..line_count].join("\n");
        let (est_tokens, est_bytes) = estimate_text_cost(&candidate_body);
        if est_tokens <= remaining_tokens && est_bytes <= remaining_bytes {
            let mut candidate = section.clone();
            candidate.body = candidate_body;
            return Some(candidate);
        }
    }
    let mut candidate = section.clone();
    let max_chars = remaining_bytes.min((remaining_tokens.saturating_mul(4)).max(1)) as usize;
    let truncated = section
        .body
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    if truncated.is_empty() {
        return None;
    }
    candidate.body = format!("{truncated}...");
    Some(candidate)
}

fn action_critical_section_ids(action: BrokerAction) -> &'static [&'static str] {
    match action {
        BrokerAction::Plan => &["repo_map", "relevant_context", "recommended_actions"],
        BrokerAction::Inspect => &["code_evidence", "repo_map", "relevant_context"],
        BrokerAction::ChooseTool => &["recent_tool_activity", "tool_failures", "recommended_actions"],
        BrokerAction::Interpret => &["recent_tool_activity", "tool_failures", "code_evidence"],
        BrokerAction::Edit => &["code_evidence", "current_focus", "checkpoint_deltas", "evidence_cache"],
        BrokerAction::Summarize => &["progress", "recent_tool_activity", "tool_failures"],
    }
}

fn prune_sections_for_budget(
    action: BrokerAction,
    sections: Vec<BrokerSection>,
    budget_tokens: u64,
    budget_bytes: u64,
    max_sections: usize,
) -> (Vec<BrokerSection>, Vec<BrokerEvictionCandidate>) {
    if sections.is_empty() {
        return (sections, Vec::new());
    }

    let critical_ids = action_critical_section_ids(action)
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut selected = Vec::new();
    let mut pruned = Vec::new();
    let mut used_tokens = 0_u64;
    let mut used_bytes = 0_u64;
    let min_remaining_tokens_for_optional = ((budget_tokens as f64) * 0.2).ceil() as u64;
    let min_remaining_bytes_for_optional = ((budget_bytes as f64) * 0.2).ceil() as u64;

    let consider = |section: BrokerSection,
                    must_keep: bool,
                    selected: &mut Vec<BrokerSection>,
                    pruned: &mut Vec<BrokerEvictionCandidate>,
                    used_tokens: &mut u64,
                    used_bytes: &mut u64| {
        let (est_tokens, est_bytes) = estimate_text_cost(&section.body);
        if est_tokens + *used_tokens <= budget_tokens && est_bytes + *used_bytes <= budget_bytes {
            *used_tokens = (*used_tokens).saturating_add(est_tokens);
            *used_bytes = (*used_bytes).saturating_add(est_bytes);
            selected.push(section);
            return;
        }
        let remaining_tokens = budget_tokens.saturating_sub(*used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(*used_bytes);
        if must_keep {
            if let Some(shrunk) =
                shrink_section_to_budget(&section, remaining_tokens, remaining_bytes)
            {
                let (shrunk_tokens, shrunk_bytes) = estimate_text_cost(&shrunk.body);
                *used_tokens = (*used_tokens).saturating_add(shrunk_tokens);
                *used_bytes = (*used_bytes).saturating_add(shrunk_bytes);
                selected.push(shrunk);
                return;
            }
        }
        pruned.push(BrokerEvictionCandidate {
            section_id: section.id.clone(),
            reason: "budget_pruned".to_string(),
            est_tokens,
        });
    };

    let mut objective = sections
        .iter()
        .find(|section| section.id == "task_objective")
        .cloned();
    if let Some(objective) = objective.take() {
        consider(objective, true, &mut selected, &mut pruned, &mut used_tokens, &mut used_bytes);
    }

    for section_id in action_critical_section_ids(action) {
        if let Some(section) = sections.iter().find(|section| section.id == *section_id).cloned() {
            consider(section, true, &mut selected, &mut pruned, &mut used_tokens, &mut used_bytes);
        }
    }

    for section in sections {
        if section.id == "task_objective" || critical_ids.contains(section.id.as_str()) {
            continue;
        }
        let remaining_tokens = budget_tokens.saturating_sub(used_tokens);
        let remaining_bytes = budget_bytes.saturating_sub(used_bytes);
        if remaining_tokens < min_remaining_tokens_for_optional
            || remaining_bytes < min_remaining_bytes_for_optional
        {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
            continue;
        }
        consider(section, false, &mut selected, &mut pruned, &mut used_tokens, &mut used_bytes);
    }

    if selected.len() > max_sections {
        for section in selected.drain(max_sections..) {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            pruned.push(BrokerEvictionCandidate {
                section_id: section.id,
                reason: "budget_pruned".to_string(),
                est_tokens,
            });
        }
    }

    (selected, pruned)
}

fn build_broker_sections(
    root: &Path,
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
    manage: &suite_packet_core::ContextManagePayload,
    repo_map: Option<&suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>>,
) -> Vec<BrokerSection> {
    let action = request.action.unwrap_or(BrokerAction::Plan);
    let effective_limits = resolve_effective_limits(
        action,
        request.verbosity,
        request.max_sections,
        request.default_max_items_per_section,
        &request.section_item_limits,
    );
    let allowed_sections =
        filter_requested_section_ids(action, &request.include_sections, &request.exclude_sections);
    let task = load_task_record(state, &request.task_id);
    let resolved_questions = build_resolved_questions(task.as_ref(), snapshot);
    let query_focus = derive_query_focus(broker_objective(state, request).as_deref());
    let mut sections = Vec::new();

    if let Some(objective) = query_focus.raw_query.clone() {
        sections.push(BrokerSection {
            id: "task_objective".to_string(),
            title: "Task Objective".to_string(),
            body: objective,
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.active_decisions.is_empty() {
        sections.push(BrokerSection {
            id: "active_decisions".to_string(),
            title: "Active Decisions".to_string(),
            body: truncate_lines(
                snapshot
                    .active_decisions
                    .iter()
                    .map(|decision| {
                        let suffix = task
                            .as_ref()
                            .and_then(|task| task.linked_decisions.get(&decision.id))
                            .map(|question_id| format!(" (answers {question_id})"))
                            .unwrap_or_default();
                        format!("- {}: {}{}", decision.id, decision.text, suffix)
                    })
                    .collect(),
                section_item_limit(&effective_limits, "active_decisions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.open_questions.is_empty() {
        sections.push(BrokerSection {
            id: "open_questions".to_string(),
            title: "Open Questions".to_string(),
            body: truncate_lines(
                snapshot
                    .open_questions
                    .iter()
                    .map(|question| format!("- {}: {}", question.id, question.text))
                    .collect(),
                section_item_limit(&effective_limits, "open_questions"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !resolved_questions.is_empty() {
        sections.push(BrokerSection {
            id: "resolved_questions".to_string(),
            title: "Resolved Questions".to_string(),
            body: truncate_lines(
                resolved_questions
                    .iter()
                    .map(|question| {
                        match (&question.resolved_by_decision_id, &question.resolution_text) {
                            (Some(decision_id), Some(text)) => {
                                format!(
                                    "- {}: {} -> {} ({})",
                                    question.id, question.text, decision_id, text
                                )
                            }
                            (Some(decision_id), None) => {
                                format!("- {}: {} -> {}", question.id, question.text, decision_id)
                            }
                            _ => format!("- {}: {}", question.id, question.text),
                        }
                    })
                    .collect(),
                section_item_limit(&effective_limits, "resolved_questions"),
            ),
            priority: if matches!(action, BrokerAction::Interpret | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    let focus_lines = merged_unique(&snapshot.focus_paths, &request.focus_paths)
        .into_iter()
        .map(|path| format!("- path: {path}"))
        .chain(
            merged_unique(&snapshot.focus_symbols, &request.focus_symbols)
                .into_iter()
                .map(|symbol| format!("- symbol: {symbol}")),
        )
        .collect::<Vec<_>>();
    if !focus_lines.is_empty() {
        sections.push(BrokerSection {
            id: "current_focus".to_string(),
            title: "Current Focus".to_string(),
            body: truncate_lines(
                focus_lines,
                section_item_limit(&effective_limits, "current_focus"),
            ),
            priority: if matches!(action, BrokerAction::Inspect | BrokerAction::Edit) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::SelfAuthored,
        });
    }

    let discovered_scope_lines = snapshot
        .read_paths_by_tool
        .iter()
        .flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- read via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        })
        .chain(snapshot.edited_paths_by_tool.iter().flat_map(|summary| {
            summary
                .paths
                .iter()
                .map(|path| format!("- edited via {}: {}", summary.tool_name, path))
                .collect::<Vec<_>>()
        }))
        .chain(
            snapshot
                .focus_symbols
                .iter()
                .map(|symbol| format!("- symbol: {symbol}")),
        )
        .collect::<Vec<_>>();
    if !discovered_scope_lines.is_empty() {
        sections.push(BrokerSection {
            id: "discovered_scope".to_string(),
            title: "Discovered Scope".to_string(),
            body: truncate_lines(
                discovered_scope_lines,
                section_item_limit(&effective_limits, "discovered_scope"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Plan
                    | BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Edit
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.recent_tool_invocations.is_empty() {
        let lines = snapshot
            .recent_tool_invocations
            .iter()
            .rev()
            .map(|invocation| {
                let request = invocation
                    .request_summary
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("no request summary");
                let result = invocation
                    .result_summary
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("no result summary");
                format!(
                    "- #{} {} [{}] {} -> {}",
                    invocation.sequence,
                    invocation.tool_name,
                    serde_json::to_string(&invocation.operation_kind)
                        .unwrap_or_else(|_| "\"generic\"".to_string())
                        .trim_matches('"'),
                    request,
                    result
                )
            })
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "recent_tool_activity".to_string(),
            title: "Recent Tool Activity".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "recent_tool_activity"),
            ),
            priority: if matches!(
                action,
                BrokerAction::Inspect
                    | BrokerAction::ChooseTool
                    | BrokerAction::Interpret
                    | BrokerAction::Edit
                    | BrokerAction::Summarize
            ) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.tool_failures.is_empty() {
        let lines = snapshot
            .tool_failures
            .iter()
            .rev()
            .map(|failure| {
                format!(
                    "- #{} {} [{}] {}",
                    failure.sequence,
                    failure.tool_name,
                    serde_json::to_string(&failure.operation_kind)
                        .unwrap_or_else(|_| "\"generic\"".to_string())
                        .trim_matches('"'),
                    failure
                        .error_message
                        .as_deref()
                        .or(failure.error_class.as_deref())
                        .unwrap_or("tool failed")
                )
            })
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "tool_failures".to_string(),
            title: "Tool Failures".to_string(),
            body: truncate_lines(
                lines,
                section_item_limit(&effective_limits, "tool_failures"),
            ),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.evidence_artifact_ids.is_empty() {
        sections.push(BrokerSection {
            id: "evidence_cache".to_string(),
            title: "Evidence Cache".to_string(),
            body: truncate_lines(
                snapshot
                    .evidence_artifact_ids
                    .iter()
                    .map(|artifact_id| format!("- artifact: {artifact_id}"))
                    .collect(),
                section_item_limit(&effective_limits, "evidence_cache"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if !snapshot.changed_paths_since_checkpoint.is_empty()
        || !snapshot.changed_symbols_since_checkpoint.is_empty()
    {
        let body = snapshot
            .changed_paths_since_checkpoint
            .iter()
            .map(|path| format!("- changed path: {path}"))
            .chain(
                snapshot
                    .changed_symbols_since_checkpoint
                    .iter()
                    .map(|symbol| format!("- changed symbol: {symbol}")),
            )
            .collect::<Vec<_>>();
        sections.push(BrokerSection {
            id: "checkpoint_deltas".to_string(),
            title: "Checkpoint Deltas".to_string(),
            body: truncate_lines(
                body,
                section_item_limit(&effective_limits, "checkpoint_deltas"),
            ),
            priority: if matches!(action, BrokerAction::Edit | BrokerAction::Summarize) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if let Some(repo_map) = repo_map {
        let rich_repo_map = mapy_core::expand_repo_map_payload(repo_map);
        let evidence_by_file = rich_repo_map
            .files_ranked
            .iter()
            .take(5)
            .map(|file| {
                (
                    file.path.clone(),
                    extract_code_evidence(
                        root,
                        &file.path,
                        &query_focus,
                        3,
                        section_item_limit(&effective_limits, "code_evidence").min(15),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let lines = rich_repo_map
            .files_ranked
            .iter()
            .filter_map(|file| {
                let evidence = evidence_by_file.get(&file.path);
                let line_hint = evidence
                    .and_then(|summary| {
                        summary
                            .primary_match_symbol
                            .as_ref()
                            .and(summary.first_match_line)
                    })
                    .map(|line| format!(":{line}"))
                    .unwrap_or_default();
                Some(format!(
                    "- {}{} [score={}] — {}",
                    file.path,
                    line_hint,
                    score_to_string(file.score),
                    render_repo_map_reason(
                        &file.path,
                        &query_focus,
                        snapshot,
                        &rich_repo_map.edges,
                        evidence,
                    )
                ))
            })
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            sections.push(BrokerSection {
                id: "repo_map".to_string(),
                title: "Relevant Files".to_string(),
                body: truncate_lines(lines, section_item_limit(&effective_limits, "repo_map")),
                priority: if matches!(action, BrokerAction::Plan | BrokerAction::Inspect) {
                    1
                } else {
                    2
                },
                source_kind: BrokerSourceKind::Derived,
            });
        }

        let evidence_lines = rich_repo_map
            .files_ranked
            .iter()
            .take(5)
            .flat_map(|file| {
                evidence_by_file
                    .get(&file.path)
                    .map(|summary| summary.rendered_lines.clone())
                    .unwrap_or_default()
            })
            .take(15)
            .collect::<Vec<_>>();
        if !evidence_lines.is_empty() {
            sections.push(BrokerSection {
                id: "code_evidence".to_string(),
                title: "Code Evidence".to_string(),
                body: evidence_lines.join("\n"),
                priority: if matches!(
                    action,
                    BrokerAction::Inspect | BrokerAction::Interpret | BrokerAction::Edit
                ) {
                    1
                } else {
                    2
                },
                source_kind: BrokerSourceKind::Derived,
            });
        }
    }

    if !manage.working_set.is_empty() || !manage.recommended_packets.is_empty() {
        let packets = if !manage.working_set.is_empty() {
            &manage.working_set
        } else {
            &manage.recommended_packets
        };
        let visible_packets = packets
            .iter()
            .filter(|packet| {
                request.include_self_context
                    || packet_source_kind(packet) != BrokerSourceKind::SelfAuthored
            })
            .map(|packet| {
                let summary = packet.summary.as_deref().unwrap_or("no summary");
                format!("- {} [{}] {}", packet.target, packet.cache_key, summary)
            })
            .collect::<Vec<_>>();
        if !visible_packets.is_empty() {
            sections.push(BrokerSection {
                id: "relevant_context".to_string(),
                title: "Relevant Context".to_string(),
                body: truncate_lines(
                    visible_packets,
                    section_item_limit(&effective_limits, "relevant_context"),
                ),
                priority: if matches!(
                    action,
                    BrokerAction::Plan | BrokerAction::Interpret | BrokerAction::ChooseTool
                ) {
                    1
                } else {
                    2
                },
                source_kind: BrokerSourceKind::External,
            });
        }
    }

    if !manage.recommended_actions.is_empty() {
        let title = match request
            .tool_result_kind
            .unwrap_or(BrokerToolResultKind::Generic)
        {
            BrokerToolResultKind::Build => "Build Guidance",
            BrokerToolResultKind::Stack => "Stack Guidance",
            BrokerToolResultKind::Test => "Test Guidance",
            BrokerToolResultKind::Diff => "Diff Guidance",
            BrokerToolResultKind::Generic => "Recommended Actions",
        };
        sections.push(BrokerSection {
            id: "recommended_actions".to_string(),
            title: title.to_string(),
            body: truncate_lines(
                manage
                    .recommended_actions
                    .iter()
                    .map(|action| format!("- {}: {}", action.kind, action.summary))
                    .collect(),
                section_item_limit(&effective_limits, "recommended_actions"),
            ),
            priority: if matches!(action, BrokerAction::ChooseTool | BrokerAction::Interpret) {
                1
            } else {
                2
            },
            source_kind: BrokerSourceKind::Derived,
        });
    }

    if matches!(action, BrokerAction::Summarize) {
        let progress = vec![
            format!("- completed steps: {}", snapshot.completed_steps.len()),
            format!("- files read: {}", snapshot.files_read.len()),
            format!("- files edited: {}", snapshot.files_edited.len()),
        ];
        sections.push(BrokerSection {
            id: "progress".to_string(),
            title: "Progress".to_string(),
            body: progress.join("\n"),
            priority: 1,
            source_kind: BrokerSourceKind::Derived,
        });
    }

    sections.retain(|section| allowed_sections.contains(&section.id));
    sections
}

fn render_brief(task_id: &str, context_version: &str, sections: &[BrokerSection]) -> String {
    let mut blocks = vec![format!(
        "[Packet28 Context v{context_version} — current Packet28 context for task {task_id}; supersedes all prior Packet28 context for this task]"
    )];
    blocks.extend(
        sections
            .iter()
            .map(|section| format!("## {}\n{}", section.title, section.body)),
    );
    blocks.join("\n\n")
}

fn load_versioned_broker_response(
    root: &Path,
    task_id: &str,
    context_version: &str,
) -> Result<Option<BrokerGetContextResponse>> {
    let path = task_version_json_path(root, task_id, context_version);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read versioned broker response '{}'",
            path.display()
        )
    })?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn build_delta(
    current: &[BrokerSection],
    previous: Option<&BrokerGetContextResponse>,
) -> BrokerDeltaResponse {
    let Some(previous) = previous else {
        return BrokerDeltaResponse {
            changed_sections: current.to_vec(),
            removed_section_ids: Vec::new(),
            unchanged_section_ids: Vec::new(),
            full_refresh_required: true,
        };
    };
    let current_by_id = current
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();
    let previous_by_id = previous
        .sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<BTreeMap<_, _>>();

    let mut changed_sections = Vec::new();
    let mut unchanged_section_ids = Vec::new();
    for section in current {
        match previous_by_id.get(section.id.as_str()) {
            Some(old) if *old == section => unchanged_section_ids.push(section.id.clone()),
            _ => changed_sections.push(section.clone()),
        }
    }
    let removed_section_ids = previous
        .sections
        .iter()
        .filter(|section| !current_by_id.contains_key(section.id.as_str()))
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    BrokerDeltaResponse {
        changed_sections,
        removed_section_ids,
        unchanged_section_ids,
        full_refresh_required: false,
    }
}

fn build_section_estimates(
    sections: &[BrokerSection],
    changed_ids: &HashSet<String>,
) -> Vec<BrokerSectionEstimate> {
    sections
        .iter()
        .map(|section| {
            let (est_tokens, est_bytes) = estimate_text_cost(&section.body);
            BrokerSectionEstimate {
                id: section.id.clone(),
                est_tokens,
                est_bytes,
                source_kind: section.source_kind,
                changed: changed_ids.contains(&section.id),
            }
        })
        .collect()
}

fn build_eviction_candidates(sections: &[BrokerSection]) -> Vec<BrokerEvictionCandidate> {
    sections
        .iter()
        .filter(|section| {
            matches!(
                section.id.as_str(),
                "relevant_context" | "repo_map" | "checkpoint_deltas" | "recommended_actions"
            )
        })
        .map(|section| {
            let (est_tokens, _) = estimate_text_cost(&section.body);
            let reason = match section.id.as_str() {
                "relevant_context" => "refreshable evidence".to_string(),
                "repo_map" => "stable repo anchors".to_string(),
                "checkpoint_deltas" => "checkpoint state can be recomputed".to_string(),
                "recommended_actions" => "guidance can be regenerated".to_string(),
                _ => "refreshable section".to_string(),
            };
            BrokerEvictionCandidate {
                section_id: section.id.clone(),
                reason,
                est_tokens,
            }
        })
        .collect()
}

fn should_use_delta_view(
    request: &BrokerGetContextRequest,
    delta: &BrokerDeltaResponse,
    full_sections_len: usize,
) -> bool {
    match broker_request_response_mode(request) {
        BrokerResponseMode::Full => false,
        BrokerResponseMode::Delta => request.since_version.is_some(),
        BrokerResponseMode::Auto => {
            request.since_version.is_some()
                && !delta.full_refresh_required
                && !delta.changed_sections.is_empty()
                && delta.changed_sections.len() < full_sections_len
        }
    }
}

fn write_broker_artifacts(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    response: &BrokerGetContextResponse,
) -> Result<String> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let brief_md_path = task_brief_markdown_path(&root, task_id);
    let brief_json_path = task_brief_json_path(&root, task_id);
    let state_json_path = task_state_json_path(&root, task_id);
    let version_json_path = task_version_json_path(&root, task_id, &response.context_version);
    if let Some(parent) = brief_md_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create task artifact dir '{}'", parent.display())
        })?;
    }
    if let Some(parent) = version_json_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create versioned broker artifact dir '{}'",
                parent.display()
            )
        })?;
    }
    fs::write(&brief_md_path, &response.brief)
        .with_context(|| format!("failed to write '{}'", brief_md_path.display()))?;
    fs::write(&brief_json_path, serde_json::to_vec_pretty(response)?)
        .with_context(|| format!("failed to write '{}'", brief_json_path.display()))?;
    fs::write(&version_json_path, serde_json::to_vec_pretty(response)?)
        .with_context(|| format!("failed to write '{}'", version_json_path.display()))?;

    let hash = blake3::hash(serde_json::to_string(response)?.as_bytes())
        .to_hex()
        .to_string();
    let generated_at = now_unix();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, task_id);
        task.latest_brief_path = Some(brief_md_path.to_string_lossy().to_string());
        task.latest_brief_hash = Some(hash.clone());
        task.latest_brief_generated_at_unix = Some(generated_at);
        task.last_context_refresh_at_unix = Some(generated_at);
        let state_value = json!({
            "task_id": task_id,
            "context_version": task.latest_context_version,
            "latest_brief_path": task.latest_brief_path,
            "latest_brief_hash": task.latest_brief_hash,
            "latest_brief_generated_at_unix": task.latest_brief_generated_at_unix,
            "latest_context_reason": task.latest_context_reason,
            "brief_json_path": brief_json_path.to_string_lossy().to_string(),
            "event_path": task_event_log_path(&root, task_id).to_string_lossy().to_string(),
            "supports_push": true,
        });
        persist_state(&guard)?;
        fs::write(&state_json_path, serde_json::to_vec_pretty(&state_value)?)
            .with_context(|| format!("failed to write '{}'", state_json_path.display()))?;
    }
    Ok(hash)
}

fn compute_broker_response(
    state: &Arc<Mutex<DaemonState>>,
    request: &BrokerGetContextRequest,
) -> Result<BrokerGetContextResponse> {
    let snapshot = load_agent_snapshot_for_task(state, &request.task_id)?;
    let task = load_task_record(state, &request.task_id);
    let root = state.lock().map_err(lock_err)?.root.clone();
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    let objective = broker_objective(state, request);
    let focus_symbols = derive_broker_focus_symbols(&snapshot, request);
    let focus_paths =
        derive_broker_focus_paths(&root, objective.as_deref(), &snapshot, request, 8)?;
    let manage = load_context_manage_for_task(&kernel, request, &focus_paths, &focus_symbols)?;
    let repo_map = load_repo_map_for_task(request, &focus_paths, &focus_symbols, &root)?;
    let version = current_context_version(state, &request.task_id)?;
    let action = request.action.unwrap_or(BrokerAction::Plan);
    let effective_limits = resolve_effective_limits(
        action,
        request.verbosity,
        request.max_sections,
        request.default_max_items_per_section,
        &request.section_item_limits,
    );
    let full_sections =
        build_broker_sections(&root, state, request, &snapshot, &manage, repo_map.as_ref());
    let budget_tokens = request
        .budget_tokens
        .unwrap_or_else(broker_default_budget_tokens);
    let budget_bytes = request
        .budget_bytes
        .unwrap_or_else(broker_default_budget_bytes);
    let (selected_sections, budget_pruned_evictions) = prune_sections_for_budget(
        action,
        full_sections.clone(),
        budget_tokens,
        budget_bytes as u64,
        effective_limits.max_sections,
    );
    let previous_response = match request.since_version.as_deref() {
        Some(since_version) if since_version != version => {
            load_versioned_broker_response(&root, &request.task_id, since_version)?
        }
        _ => None,
    };
    let delta = build_delta(&selected_sections, previous_response.as_ref());
    let changed_ids = delta
        .changed_sections
        .iter()
        .map(|section| section.id.clone())
        .collect::<HashSet<_>>();
    let use_delta_view = should_use_delta_view(request, &delta, selected_sections.len());
    let sections = if use_delta_view {
        delta.changed_sections.clone()
    } else {
        selected_sections.clone()
    };
    let brief = render_brief(&request.task_id, &version, &sections);
    let (est_tokens, est_bytes) = estimate_text_cost(&brief);
    let resolved_questions = build_resolved_questions(task.as_ref(), &snapshot);
    let discovered_paths = merged_unique(
        &snapshot.focus_paths,
        &snapshot
            .read_paths_by_tool
            .iter()
            .flat_map(|summary| summary.paths.iter().cloned())
            .chain(
                snapshot
                    .edited_paths_by_tool
                    .iter()
                    .flat_map(|summary| summary.paths.iter().cloned()),
            )
            .collect::<Vec<_>>(),
    );
    let discovered_symbols = merged_unique(&snapshot.focus_symbols, &[]);
    let mut eviction_candidates = build_eviction_candidates(&selected_sections);
    eviction_candidates.extend(budget_pruned_evictions);
    eviction_candidates.sort_by(|a, b| {
        a.section_id
            .cmp(&b.section_id)
            .then_with(|| a.reason.cmp(&b.reason))
    });
    eviction_candidates.dedup_by(|a, b| a.section_id == b.section_id && a.reason == b.reason);
    Ok(BrokerGetContextResponse {
        stale: request
            .since_version
            .as_deref()
            .is_some_and(|since| since != version),
        invalidates_since_version: request
            .since_version
            .as_deref()
            .is_some_and(|since| since != version),
        context_version: version.clone(),
        brief,
        supersedes_prior_context: true,
        supersession_mode: BrokerSupersessionMode::Replace,
        superseded_before_version: version.clone(),
        sections: sections.clone(),
        est_tokens,
        est_bytes,
        budget_remaining_tokens: budget_tokens.saturating_sub(est_tokens),
        budget_remaining_bytes: (budget_bytes as u64).saturating_sub(est_bytes),
        section_estimates: build_section_estimates(&sections, &changed_ids),
        eviction_candidates,
        delta,
        working_set: manage
            .working_set
            .iter()
            .map(|packet| BrokerPacketRef {
                cache_key: packet.cache_key.clone(),
                target: packet.target.clone(),
                score: packet.score,
                summary: packet.summary.clone(),
                packet_types: packet.packet_types.clone(),
                est_tokens: packet.est_tokens,
                est_bytes: packet.est_bytes,
            })
            .collect(),
        recommended_actions: manage
            .recommended_actions
            .iter()
            .map(|action| BrokerRecommendedAction {
                kind: action.kind.clone(),
                summary: action.summary.clone(),
                related_paths: action.related_paths.clone(),
                related_symbols: action.related_symbols.clone(),
            })
            .collect(),
        active_decisions: snapshot
            .active_decisions
            .iter()
            .map(|decision| BrokerDecision {
                id: decision.id.clone(),
                text: decision.text.clone(),
                resolves_question_id: task
                    .as_ref()
                    .and_then(|task| task.linked_decisions.get(&decision.id))
                    .cloned(),
            })
            .collect(),
        open_questions: snapshot
            .open_questions
            .iter()
            .map(|question| BrokerQuestion {
                id: question.id.clone(),
                text: question.text.clone(),
            })
            .collect(),
        resolved_questions,
        changed_paths_since_checkpoint: snapshot.changed_paths_since_checkpoint.clone(),
        changed_symbols_since_checkpoint: snapshot.changed_symbols_since_checkpoint.clone(),
        recent_tool_invocations: snapshot.recent_tool_invocations.clone(),
        tool_failures: snapshot.tool_failures.clone(),
        discovered_paths,
        discovered_symbols,
        evidence_artifact_ids: snapshot.evidence_artifact_ids.clone(),
        effective_max_sections: effective_limits.max_sections,
        effective_default_max_items_per_section: effective_limits.default_max_items_per_section,
        effective_section_item_limits: effective_limits.section_item_limits,
    })
}

fn estimate_request_to_get_request(
    request: &BrokerEstimateContextRequest,
) -> BrokerGetContextRequest {
    BrokerGetContextRequest {
        task_id: request.task_id.clone(),
        action: request.action,
        budget_tokens: request.budget_tokens,
        budget_bytes: request.budget_bytes,
        since_version: request.since_version.clone(),
        focus_paths: request.focus_paths.clone(),
        focus_symbols: request.focus_symbols.clone(),
        tool_name: request.tool_name.clone(),
        tool_result_kind: request.tool_result_kind,
        query: request.query.clone(),
        include_sections: request.include_sections.clone(),
        exclude_sections: request.exclude_sections.clone(),
        verbosity: request.verbosity,
        response_mode: request.response_mode,
        include_self_context: request.include_self_context,
        max_sections: request.max_sections,
        default_max_items_per_section: request.default_max_items_per_section,
        section_item_limits: request.section_item_limits.clone(),
    }
}

fn refresh_broker_context_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<Option<BrokerGetContextResponse>> {
    let request = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(task_id)
        .and_then(|task| task.latest_broker_request.clone());
    let Some(mut request) = request else {
        return Ok(None);
    };
    request.since_version = None;
    request.response_mode = Some(BrokerResponseMode::Full);
    let response = compute_broker_response(state, &request)?;
    write_broker_artifacts(state, task_id, &response)?;
    Ok(Some(response))
}

fn broker_get_context(
    state: Arc<Mutex<DaemonState>>,
    mut request: BrokerGetContextRequest,
) -> Result<BrokerGetContextResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker get_context requires task_id");
    }
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(broker_default_budget_tokens());
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(broker_default_budget_bytes());
    }
    if request.verbosity.is_none() {
        request.verbosity = Some(BrokerVerbosity::Standard);
    }
    if request.response_mode.is_none() {
        request.response_mode = Some(BrokerResponseMode::Full);
    }
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, &request.task_id);
        ensure_context_version(task);
        let mut session_request = request.clone();
        session_request.since_version = None;
        session_request.response_mode = Some(BrokerResponseMode::Full);
        task.latest_broker_request = Some(session_request);
        persist_state(&guard)?;
    }
    let _ = set_context_reason(&state, &request.task_id, "get_context");
    let response = compute_broker_response(&state, &request)?;
    write_broker_artifacts(&state, &request.task_id, &response)?;
    Ok(response)
}

fn broker_estimate_context(
    state: Arc<Mutex<DaemonState>>,
    mut request: BrokerEstimateContextRequest,
) -> Result<BrokerEstimateContextResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker estimate_context requires task_id");
    }
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(broker_default_budget_tokens());
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(broker_default_budget_bytes());
    }
    let get_request = estimate_request_to_get_request(&request);
    let response = compute_broker_response(&state, &get_request)?;
    Ok(BrokerEstimateContextResponse {
        context_version: response.context_version.clone(),
        selected_section_ids: response
            .sections
            .iter()
            .map(|section| section.id.clone())
            .collect(),
        est_tokens: response.est_tokens,
        est_bytes: response.est_bytes,
        budget_remaining_tokens: response.budget_remaining_tokens,
        budget_remaining_bytes: response.budget_remaining_bytes,
        section_estimates: response.section_estimates,
        eviction_candidates: response.eviction_candidates,
        would_use_delta: should_use_delta_view(
            &get_request,
            &response.delta,
            response.delta.changed_sections.len() + response.delta.unchanged_section_ids.len(),
        ),
        would_include_brief: !response.sections.is_empty(),
        effective_max_sections: response.effective_max_sections,
        effective_default_max_items_per_section: response.effective_default_max_items_per_section,
        effective_section_item_limits: response.effective_section_item_limits,
    })
}

fn broker_validate_plan(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerValidatePlanRequest,
) -> Result<BrokerValidatePlanResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker validate_plan requires task_id");
    }
    let root = state.lock().map_err(lock_err)?.root.clone();
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let normalized_steps = normalize_plan_steps(&request.steps);
    let coverage = load_cached_coverage(&root)?;
    let _testmap = load_cached_testmap(&root)?;
    let focus_paths = normalized_steps
        .iter()
        .flat_map(|step| step.paths.iter().cloned())
        .collect::<Vec<_>>();
    let focus_symbols = normalized_steps
        .iter()
        .flat_map(|step| step.symbols.iter().cloned())
        .collect::<Vec<_>>();
    let repo_map = mapy_core::expand_repo_map_payload(&build_repo_map_envelope(
        &root,
        &focus_paths,
        &focus_symbols,
        48,
        96,
    )?);
    let deleted_files = current_deleted_paths(&root);
    let completed_steps = snapshot
        .completed_steps
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let files_read = snapshot.files_read.iter().cloned().collect::<HashSet<_>>();
    let step_index = normalized_steps
        .iter()
        .enumerate()
        .map(|(idx, step)| (step.id.clone(), idx))
        .collect::<HashMap<_, _>>();
    let mut touched_paths = HashMap::<String, usize>::new();
    for (idx, step) in normalized_steps.iter().enumerate() {
        for path in &step.paths {
            touched_paths.entry(path.clone()).or_insert(idx);
        }
    }

    let mut violations = Vec::new();
    let mut warnings = Vec::new();

    for step in &normalized_steps {
        for path in &step.paths {
            if !root.join(path).exists() {
                let rule = if deleted_files.contains(path) {
                    "deleted_path"
                } else {
                    "unknown_path"
                };
                let message = if deleted_files.contains(path) {
                    format!("step targets '{path}', which is deleted in the current diff")
                } else {
                    format!("step targets '{path}', which does not exist in the current workspace")
                };
                violations.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: rule.to_string(),
                    severity: "error".to_string(),
                    message,
                    related_paths: vec![path.clone()],
                    related_symbols: Vec::new(),
                });
            }
        }

        for dependency in &step.depends_on {
            if completed_steps.contains(dependency) {
                warnings.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: "redundant_dependency".to_string(),
                    severity: "warning".to_string(),
                    message: format!(
                        "step depends on '{dependency}', but that step is already completed"
                    ),
                    related_paths: step.paths.clone(),
                    related_symbols: step.symbols.clone(),
                });
            } else if !step_index.contains_key(dependency) {
                violations.push(BrokerPlanViolation {
                    step_id: step.id.clone(),
                    rule: "missing_dependency".to_string(),
                    severity: "error".to_string(),
                    message: format!("step depends on unknown step '{dependency}'"),
                    related_paths: step.paths.clone(),
                    related_symbols: step.symbols.clone(),
                });
            }
        }

        if request.require_read_before_edit.unwrap_or(true) && is_edit_like_action(&step.action) {
            for path in &step.paths {
                if !files_read.contains(path) {
                    violations.push(BrokerPlanViolation {
                        step_id: step.id.clone(),
                        rule: "read_before_edit".to_string(),
                        severity: "error".to_string(),
                        message: format!(
                            "step edits '{path}' before the agent has recorded a file_read for it"
                        ),
                        related_paths: vec![path.clone()],
                        related_symbols: step.symbols.clone(),
                    });
                }
            }
        }
    }

    for edge in &repo_map.edges {
        let Some(importer_idx) = touched_paths.get(&edge.from).copied() else {
            continue;
        };
        let Some(imported_idx) = touched_paths.get(&edge.to).copied() else {
            continue;
        };
        if importer_idx < imported_idx {
            let importer_step = &normalized_steps[importer_idx];
            let imported_step = &normalized_steps[imported_idx];
            let importer_depends = importer_step
                .depends_on
                .iter()
                .any(|id| id == &imported_step.id);
            let imported_depends = imported_step
                .depends_on
                .iter()
                .any(|id| id == &importer_step.id);
            if !importer_depends && !imported_depends {
                violations.push(BrokerPlanViolation {
                    step_id: importer_step.id.clone(),
                    rule: "dependency_order".to_string(),
                    severity: "error".to_string(),
                    message: format!(
                        "step touches '{}' before its dependency '{}'; add a dependency or reorder the plan",
                        edge.from, edge.to
                    ),
                    related_paths: vec![edge.from.clone(), edge.to.clone()],
                    related_symbols: Vec::new(),
                });
            }
        }
    }

    if request.require_test_gate.unwrap_or(true) {
        for (idx, step) in normalized_steps.iter().enumerate() {
            if !is_edit_like_action(&step.action) {
                continue;
            }
            for path in &step.paths {
                if !coverage_gap_for_path(coverage.as_ref(), path) {
                    continue;
                }
                let has_following_test_gate =
                    normalized_steps.iter().skip(idx + 1).any(is_test_like_step);
                if !has_following_test_gate {
                    violations.push(BrokerPlanViolation {
                        step_id: step.id.clone(),
                        rule: "missing_test_gate".to_string(),
                        severity: "error".to_string(),
                        message: format!(
                            "step edits uncovered path '{path}' without a later test-focused step"
                        ),
                        related_paths: vec![path.clone()],
                        related_symbols: step.symbols.clone(),
                    });
                }
            }
        }
    }

    let est_plan_tokens = normalized_steps
        .iter()
        .map(estimate_plan_step_tokens)
        .sum::<u64>();
    if let Some(budget_tokens) = request.budget_tokens {
        if est_plan_tokens > budget_tokens {
            violations.push(BrokerPlanViolation {
                step_id: "plan".to_string(),
                rule: "budget_exceeded".to_string(),
                severity: "error".to_string(),
                message: format!(
                    "normalized plan is estimated at ~{est_plan_tokens} tokens, over the requested budget of {budget_tokens}"
                ),
                related_paths: normalized_steps
                    .iter()
                    .flat_map(|step| step.paths.iter().cloned())
                    .collect(),
                related_symbols: Vec::new(),
            });
        }
    }

    Ok(BrokerValidatePlanResponse {
        valid: violations.is_empty(),
        violations,
        warnings,
        normalized_steps,
        est_plan_tokens: Some(est_plan_tokens),
    })
}

fn broker_decompose(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerDecomposeRequest,
) -> Result<BrokerDecomposeResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("broker decompose requires task_id");
    }
    if request.task_text.trim().is_empty() {
        anyhow::bail!("broker decompose requires task_text");
    }
    let Some(intent) = request.intent else {
        return Ok(BrokerDecomposeResponse {
            steps: Vec::new(),
            assumptions: Vec::new(),
            unresolved: vec!["intent is required for deterministic decomposition".to_string()],
            selected_scope_paths: Vec::new(),
        });
    };

    let root = state.lock().map_err(lock_err)?.root.clone();
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    let repo_map = build_repo_map_envelope(
        &root,
        &merged_unique(&snapshot.focus_paths, &request.scope_paths),
        &merged_unique(&snapshot.focus_symbols, &request.scope_symbols),
        64,
        128,
    )?;
    let rich_map = mapy_core::expand_repo_map_payload(&repo_map);
    let coverage = load_cached_coverage(&root)?;
    let testmap = load_cached_testmap(&root)?;
    let primary_scope_paths = infer_scope_paths(
        &request.task_text,
        &rich_map,
        &request.scope_paths,
        &request.scope_symbols,
    );
    let selected_scope_paths = expand_scope_paths(
        &request.task_text,
        &rich_map,
        &primary_scope_paths,
        &request.scope_symbols,
        8,
    );
    if selected_scope_paths.is_empty() {
        return Ok(BrokerDecomposeResponse {
            steps: Vec::new(),
            assumptions: vec![format!(
                "intent locked to {:?} for deterministic decomposition",
                intent
            )],
            unresolved: vec![
                "unable to resolve scope paths from task text; supply scope_paths or scope_symbols"
                    .to_string(),
            ],
            selected_scope_paths,
        });
    }

    let max_steps = request.max_steps.unwrap_or(8).max(1);
    let edge_map = rich_map
        .edges
        .iter()
        .filter(|edge| {
            selected_scope_paths.contains(&edge.from) && selected_scope_paths.contains(&edge.to)
        })
        .fold(BTreeMap::<String, Vec<String>>::new(), |mut acc, edge| {
            acc.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
            acc
        });
    let mut ordered_paths = selected_scope_paths.clone();
    ordered_paths.sort_by_key(|path| edge_map.get(path).map(|deps| deps.len()).unwrap_or(0));

    let mut steps = Vec::new();
    let mut path_to_step = BTreeMap::<String, String>::new();
    let action = match intent {
        BrokerDecomposeIntent::Rename => "rename",
        BrokerDecomposeIntent::Extract => "extract",
        BrokerDecomposeIntent::SplitFile => "split_file",
        BrokerDecomposeIntent::MergeFiles => "merge_files",
        BrokerDecomposeIntent::RestructureModule => "restructure_module",
    };

    for (idx, path) in ordered_paths.iter().enumerate() {
        if steps.len() >= max_steps {
            break;
        }
        let step_id = format!("step-{}", idx + 1);
        let depends_on = edge_map
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|dependency| path_to_step.get(dependency).cloned())
            .collect::<Vec<_>>();
        let related_symbols = rich_map
            .symbols_ranked
            .iter()
            .filter(|symbol| symbol.file == *path)
            .take(3)
            .map(|symbol| symbol.name.clone())
            .collect::<Vec<_>>();
        let description = match intent {
            BrokerDecomposeIntent::Rename => format!("Rename identifiers and references in {path}"),
            BrokerDecomposeIntent::Extract => {
                format!("Extract focused logic from {path} into a smaller unit")
            }
            BrokerDecomposeIntent::SplitFile => {
                format!("Split {path} into smaller responsibility-focused files")
            }
            BrokerDecomposeIntent::MergeFiles => {
                format!("Merge related logic centered on {path}")
            }
            BrokerDecomposeIntent::RestructureModule => {
                format!("Restructure module boundaries around {path}")
            }
        };
        let coverage_gap = coverage_gap_for_path(coverage.as_ref(), path);
        let step = BrokerDecomposedStep {
            id: step_id.clone(),
            action: action.to_string(),
            description,
            paths: vec![path.clone()],
            symbols: related_symbols.clone(),
            depends_on,
            coverage_gap,
            est_tokens: 120 + (related_symbols.len() as u64 * 24),
        };
        path_to_step.insert(path.clone(), step_id);
        steps.push(step);
    }

    let mut test_targets = Vec::new();
    for step in &steps {
        if !step.coverage_gap {
            continue;
        }
        for path in &step.paths {
            test_targets.extend(find_candidate_test_paths(path, &rich_map, testmap.as_ref()));
        }
    }
    let test_targets = merged_unique(&[], &test_targets);
    if !test_targets.is_empty() && steps.len() < max_steps {
        let depends_on = steps.iter().map(|step| step.id.clone()).collect::<Vec<_>>();
        steps.push(BrokerDecomposedStep {
            id: format!("step-{}", steps.len() + 1),
            action: "add_tests".to_string(),
            description: "Add or update tests to cover the decomposed scope".to_string(),
            paths: test_targets,
            symbols: Vec::new(),
            depends_on,
            coverage_gap: false,
            est_tokens: 160,
        });
    }

    Ok(BrokerDecomposeResponse {
        steps,
        assumptions: vec![format!(
            "intent constrained to {}",
            action.replace('_', " ")
        )],
        unresolved: Vec::new(),
        selected_scope_paths,
    })
}

fn event_id_for_write(request: &BrokerWriteStateRequest) -> String {
    let payload = serde_json::to_string(request).unwrap_or_else(|_| request.task_id.clone());
    let hash = blake3::hash(payload.as_bytes()).to_hex().to_string();
    format!("broker-{}", &hash[..16])
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn derived_tool_invocation_id(request: &BrokerWriteStateRequest) -> String {
    request
        .invocation_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| event_id_for_write(request))
}

fn derived_tool_sequence(request: &BrokerWriteStateRequest) -> u64 {
    request.sequence.unwrap_or_else(now_unix_millis)
}

fn material_write_is_noop(
    request: &BrokerWriteStateRequest,
    snapshot: &suite_packet_core::AgentSnapshotPayload,
) -> bool {
    let op = request.op.unwrap_or(BrokerWriteOp::FileRead);
    match op {
        BrokerWriteOp::FocusSet => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::FocusClear => {
            if request.paths.is_empty() && request.symbols.is_empty() {
                snapshot.focus_paths.is_empty() && snapshot.focus_symbols.is_empty()
            } else {
                request
                    .paths
                    .iter()
                    .all(|path| !snapshot.focus_paths.iter().any(|existing| existing == path))
                    && request.symbols.iter().all(|symbol| {
                        !snapshot
                            .focus_symbols
                            .iter()
                            .any(|existing| existing == symbol)
                    })
            }
        }
        BrokerWriteOp::FileRead => request
            .paths
            .iter()
            .all(|path| snapshot.files_read.iter().any(|existing| existing == path)),
        BrokerWriteOp::FileEdit => {
            request.paths.iter().all(|path| {
                snapshot
                    .files_edited
                    .iter()
                    .any(|existing| existing == path)
                    && snapshot
                        .changed_paths_since_checkpoint
                        .iter()
                        .any(|existing| existing == path)
            }) && request.symbols.iter().all(|symbol| {
                snapshot
                    .changed_symbols_since_checkpoint
                    .iter()
                    .any(|existing| existing == symbol)
            })
        }
        BrokerWriteOp::CheckpointSave => request
            .checkpoint_id
            .as_ref()
            .zip(snapshot.latest_checkpoint_id.as_ref())
            .is_some_and(|(next, current)| next == current),
        BrokerWriteOp::DecisionAdd => request
            .decision_id
            .as_ref()
            .zip(request.text.as_ref())
            .is_some_and(|(decision_id, text)| {
                let exists = snapshot
                    .active_decisions
                    .iter()
                    .any(|decision| decision.id == *decision_id && decision.text == *text);
                let question_already_closed =
                    request
                        .resolves_question_id
                        .as_ref()
                        .is_none_or(|question_id| {
                            !snapshot
                                .open_questions
                                .iter()
                                .any(|question| question.id == *question_id)
                        });
                exists && question_already_closed
            }),
        BrokerWriteOp::DecisionSupersede => {
            request.decision_id.as_ref().is_some_and(|decision_id| {
                !snapshot
                    .active_decisions
                    .iter()
                    .any(|decision| decision.id == *decision_id)
            })
        }
        BrokerWriteOp::StepComplete => request.step_id.as_ref().is_some_and(|step_id| {
            snapshot
                .completed_steps
                .iter()
                .any(|existing| existing == step_id)
        }),
        BrokerWriteOp::QuestionOpen => request
            .question_id
            .as_ref()
            .zip(request.text.as_ref())
            .is_some_and(|(question_id, text)| {
                snapshot
                    .open_questions
                    .iter()
                    .any(|question| question.id == *question_id && question.text == *text)
            }),
        BrokerWriteOp::QuestionResolve => request.question_id.as_ref().is_some_and(|question_id| {
            !snapshot
                .open_questions
                .iter()
                .any(|question| question.id == *question_id)
        }),
        BrokerWriteOp::ToolInvocationStarted => false,
        BrokerWriteOp::ToolInvocationCompleted => {
            request.invocation_id.as_ref().is_some_and(|id| {
                snapshot
                    .recent_tool_invocations
                    .iter()
                    .any(|invocation| invocation.invocation_id == *id)
            })
        }
        BrokerWriteOp::ToolResult => {
            let derived = derived_tool_invocation_id(request);
            snapshot
                .recent_tool_invocations
                .iter()
                .any(|invocation| invocation.invocation_id == derived)
        }
        BrokerWriteOp::ToolInvocationFailed => request.invocation_id.as_ref().is_some_and(|id| {
            snapshot
                .tool_failures
                .iter()
                .any(|failure| failure.invocation_id == *id)
        }),
        BrokerWriteOp::FocusInferred => {
            request
                .paths
                .iter()
                .all(|path| snapshot.focus_paths.iter().any(|existing| existing == path))
                && request.symbols.iter().all(|symbol| {
                    snapshot
                        .focus_symbols
                        .iter()
                        .any(|existing| existing == symbol)
                })
        }
        BrokerWriteOp::EvidenceCaptured => {
            request.artifact_id.as_ref().is_some_and(|artifact_id| {
                snapshot
                    .evidence_artifact_ids
                    .iter()
                    .any(|existing| existing == artifact_id)
            })
        }
    }
}

fn broker_write_to_event(
    request: &BrokerWriteStateRequest,
) -> Result<suite_packet_core::AgentStateEventPayload> {
    let task_id = request.task_id.trim();
    if task_id.is_empty() {
        anyhow::bail!("broker write_state requires task_id");
    }
    let op = request.op.unwrap_or(BrokerWriteOp::FileRead);
    let (kind, data) = match op {
        BrokerWriteOp::FocusSet => (
            suite_packet_core::AgentStateEventKind::FocusSet,
            suite_packet_core::AgentStateEventData::FocusSet {
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::FocusClear => (
            suite_packet_core::AgentStateEventKind::FocusCleared,
            suite_packet_core::AgentStateEventData::FocusCleared {
                clear_all: request.paths.is_empty() && request.symbols.is_empty(),
            },
        ),
        BrokerWriteOp::FileRead => (
            suite_packet_core::AgentStateEventKind::FileRead,
            suite_packet_core::AgentStateEventData::FileRead {},
        ),
        BrokerWriteOp::FileEdit => (
            suite_packet_core::AgentStateEventKind::FileEdited,
            suite_packet_core::AgentStateEventData::FileEdited {
                regions: request.regions.clone(),
            },
        ),
        BrokerWriteOp::CheckpointSave => (
            suite_packet_core::AgentStateEventKind::CheckpointSaved,
            suite_packet_core::AgentStateEventData::CheckpointSaved {
                checkpoint_id: request
                    .checkpoint_id
                    .clone()
                    .ok_or_else(|| anyhow!("checkpoint_save requires checkpoint_id"))?,
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::DecisionAdd => (
            suite_packet_core::AgentStateEventKind::DecisionAdded,
            suite_packet_core::AgentStateEventData::DecisionAdded {
                decision_id: request
                    .decision_id
                    .clone()
                    .ok_or_else(|| anyhow!("decision_add requires decision_id"))?,
                text: request
                    .text
                    .clone()
                    .ok_or_else(|| anyhow!("decision_add requires text"))?,
                supersedes: None,
            },
        ),
        BrokerWriteOp::DecisionSupersede => (
            suite_packet_core::AgentStateEventKind::DecisionSuperseded,
            suite_packet_core::AgentStateEventData::DecisionSuperseded {
                decision_id: request
                    .decision_id
                    .clone()
                    .ok_or_else(|| anyhow!("decision_supersede requires decision_id"))?,
                superseded_by: request.note.clone(),
            },
        ),
        BrokerWriteOp::StepComplete => (
            suite_packet_core::AgentStateEventKind::StepCompleted,
            suite_packet_core::AgentStateEventData::StepCompleted {
                step_id: request
                    .step_id
                    .clone()
                    .ok_or_else(|| anyhow!("step_complete requires step_id"))?,
            },
        ),
        BrokerWriteOp::QuestionOpen => (
            suite_packet_core::AgentStateEventKind::QuestionOpened,
            suite_packet_core::AgentStateEventData::QuestionOpened {
                question_id: request
                    .question_id
                    .clone()
                    .ok_or_else(|| anyhow!("question_open requires question_id"))?,
                text: request
                    .text
                    .clone()
                    .ok_or_else(|| anyhow!("question_open requires text"))?,
            },
        ),
        BrokerWriteOp::QuestionResolve => (
            suite_packet_core::AgentStateEventKind::QuestionResolved,
            suite_packet_core::AgentStateEventData::QuestionResolved {
                question_id: request
                    .question_id
                    .clone()
                    .ok_or_else(|| anyhow!("question_resolve requires question_id"))?,
            },
        ),
        BrokerWriteOp::ToolInvocationStarted => (
            suite_packet_core::AgentStateEventKind::ToolInvocationStarted,
            suite_packet_core::AgentStateEventData::ToolInvocationStarted {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_started requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_started requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_started requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
            },
        ),
        BrokerWriteOp::ToolInvocationCompleted => (
            suite_packet_core::AgentStateEventKind::ToolInvocationCompleted,
            suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_completed requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                result_summary: request.result_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                search_query: request.search_query.clone(),
                command: request.command.clone(),
                artifact_id: request.artifact_id.clone(),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::ToolResult => (
            suite_packet_core::AgentStateEventKind::ToolInvocationCompleted,
            suite_packet_core::AgentStateEventData::ToolInvocationCompleted {
                invocation_id: derived_tool_invocation_id(request),
                sequence: derived_tool_sequence(request),
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_result requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                result_summary: request.result_summary.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                search_query: request.search_query.clone(),
                command: request.command.clone(),
                artifact_id: request.artifact_id.clone(),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::ToolInvocationFailed => (
            suite_packet_core::AgentStateEventKind::ToolInvocationFailed,
            suite_packet_core::AgentStateEventData::ToolInvocationFailed {
                invocation_id: request
                    .invocation_id
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires invocation_id"))?,
                sequence: request
                    .sequence
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires sequence"))?,
                tool_name: request
                    .tool_name
                    .clone()
                    .ok_or_else(|| anyhow!("tool_invocation_failed requires tool_name"))?,
                server_name: request.server_name.clone(),
                operation_kind: request.operation_kind.unwrap_or_default(),
                request_summary: request.request_summary.clone(),
                error_class: request.error_class.clone(),
                error_message: request.error_message.clone(),
                request_fingerprint: request.request_fingerprint.clone(),
                retryable: request.retryable.unwrap_or(false),
                duration_ms: request.duration_ms,
            },
        ),
        BrokerWriteOp::FocusInferred => (
            suite_packet_core::AgentStateEventKind::FocusInferred,
            suite_packet_core::AgentStateEventData::FocusInferred {
                note: request.note.clone(),
            },
        ),
        BrokerWriteOp::EvidenceCaptured => (
            suite_packet_core::AgentStateEventKind::EvidenceCaptured,
            suite_packet_core::AgentStateEventData::EvidenceCaptured {
                artifact_id: request
                    .artifact_id
                    .clone()
                    .ok_or_else(|| anyhow!("evidence_captured requires artifact_id"))?,
                summary: request.note.clone(),
            },
        ),
    };
    Ok(suite_packet_core::AgentStateEventPayload {
        task_id: request.task_id.clone(),
        event_id: event_id_for_write(request),
        occurred_at_unix: now_unix_millis(),
        actor: "packet28.broker".to_string(),
        kind,
        paths: request.paths.clone(),
        symbols: request.symbols.clone(),
        data,
    })
}

fn broker_write_state(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerWriteStateRequest,
) -> Result<BrokerWriteStateResponse> {
    let snapshot = load_agent_snapshot_for_task(&state, &request.task_id)?;
    if material_write_is_noop(&request, &snapshot) {
        update_broker_link_state(&state, &request)?;
        let version = current_context_version(&state, &request.task_id)?;
        return Ok(BrokerWriteStateResponse {
            event_id: event_id_for_write(&request),
            context_version: version,
            accepted: true,
        });
    }

    let event = broker_write_to_event(&request)?;
    let kernel = state.lock().map_err(lock_err)?.kernel.clone();
    kernel.execute(KernelRequest {
        target: "agenty.state.write".to_string(),
        reducer_input: serde_json::to_value(&event)?,
        policy_context: json!({
            "task_id": request.task_id,
        }),
        ..KernelRequest::default()
    })?;
    if matches!(request.op, Some(BrokerWriteOp::ToolResult)) {
        if !request.paths.is_empty() || !request.symbols.is_empty() {
            let focus_event = suite_packet_core::AgentStateEventPayload {
                task_id: request.task_id.clone(),
                event_id: format!("{}-focus", event.event_id),
                occurred_at_unix: now_unix_millis(),
                actor: "packet28.broker".to_string(),
                kind: suite_packet_core::AgentStateEventKind::FocusInferred,
                paths: request.paths.clone(),
                symbols: request.symbols.clone(),
                data: suite_packet_core::AgentStateEventData::FocusInferred {
                    note: Some(format!(
                        "inferred from {}",
                        request
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool_result".to_string())
                    )),
                },
            };
            kernel.execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: serde_json::to_value(&focus_event)?,
                policy_context: json!({
                    "task_id": request.task_id,
                }),
                ..KernelRequest::default()
            })?;
        }
        if let Some(artifact_id) = request
            .artifact_id
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            let evidence_event = suite_packet_core::AgentStateEventPayload {
                task_id: request.task_id.clone(),
                event_id: format!("{}-evidence", event.event_id),
                occurred_at_unix: now_unix_millis(),
                actor: "packet28.broker".to_string(),
                kind: suite_packet_core::AgentStateEventKind::EvidenceCaptured,
                paths: Vec::new(),
                symbols: Vec::new(),
                data: suite_packet_core::AgentStateEventData::EvidenceCaptured {
                    artifact_id,
                    summary: Some(format!(
                        "captured from {}",
                        request
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool_result".to_string())
                    )),
                },
            };
            kernel.execute(KernelRequest {
                target: "agenty.state.write".to_string(),
                reducer_input: serde_json::to_value(&evidence_event)?,
                policy_context: json!({
                    "task_id": request.task_id,
                }),
                ..KernelRequest::default()
            })?;
        }
    }
    if let Some(question_id) = &request.resolves_question_id {
        let question_resolved_event = suite_packet_core::AgentStateEventPayload {
            task_id: request.task_id.clone(),
            event_id: format!("{}-resolve", event.event_id),
            occurred_at_unix: now_unix_millis(),
            actor: "packet28.broker".to_string(),
            kind: suite_packet_core::AgentStateEventKind::QuestionResolved,
            paths: Vec::new(),
            symbols: Vec::new(),
            data: suite_packet_core::AgentStateEventData::QuestionResolved {
                question_id: question_id.clone(),
            },
        };
        kernel.execute(KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: serde_json::to_value(&question_resolved_event)?,
            policy_context: json!({
                "task_id": request.task_id,
            }),
            ..KernelRequest::default()
        })?;
    }
    update_broker_link_state(&state, &request)?;
    let reason = format!(
        "state_write:{}",
        serde_json::to_string(&request.op.unwrap_or(BrokerWriteOp::FileRead))?.trim_matches('"')
    );
    let _ = set_context_reason(&state, &request.task_id, reason);

    let version = bump_context_version(&state, &request.task_id)?;
    if let Some(response) = refresh_broker_context_for_task(&state, &request.task_id)? {
        let changed_section_ids = response
            .delta
            .changed_sections
            .iter()
            .map(|section| section.id.clone())
            .collect::<Vec<_>>();
        let _ = emit_task_event(
            state.clone(),
            &request.task_id,
            "context_updated",
            json!({
                "context_version": response.context_version,
                "changed_section_ids": changed_section_ids,
                "removed_section_ids": response.delta.removed_section_ids,
                "reason": state.lock().ok()
                    .and_then(|guard| guard.tasks.tasks.get(&request.task_id).and_then(|task| task.latest_context_reason.clone()))
                    .unwrap_or_else(|| "state_write".to_string()),
                "summary": response
                    .sections
                    .first()
                    .map(|section| section.title.clone())
                    .unwrap_or_else(|| "broker refresh".to_string()),
            }),
        );
    }

    Ok(BrokerWriteStateResponse {
        event_id: event.event_id,
        context_version: version,
        accepted: true,
    })
}

fn broker_task_status(
    state: Arc<Mutex<DaemonState>>,
    request: BrokerTaskStatusRequest,
) -> Result<BrokerTaskStatusResponse> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let task = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&request.task_id)
        .cloned();
    Ok(BrokerTaskStatusResponse {
        latest_context_version: task
            .as_ref()
            .and_then(|task| task.latest_context_version.clone()),
        last_refresh_at_unix: task
            .as_ref()
            .and_then(|task| task.last_context_refresh_at_unix),
        latest_context_reason: task
            .as_ref()
            .and_then(|task| task.latest_context_reason.clone()),
        supports_push: true,
        task,
        brief_path: task_brief_markdown_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_brief_markdown_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
        state_path: task_state_json_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_state_json_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
        event_path: task_event_log_path(&root, &request.task_id)
            .exists()
            .then(|| {
                task_event_log_path(&root, &request.task_id)
                    .to_string_lossy()
                    .to_string()
            }),
    })
}

fn register_task_and_watches(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    spec: TaskSubmitSpec,
) -> Result<(TaskRecord, Vec<WatchRegistration>)> {
    let root = {
        let guard = state.lock().map_err(lock_err)?;
        guard.root.clone()
    };
    let spec = normalize_task_submit_spec(&root, spec)?;

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
            last_event_seq: 0,
            last_context_refresh_at_unix: None,
            working_set_est_tokens: 0,
            evictable_est_tokens: 0,
            changed_since_checkpoint_paths: 0,
            changed_since_checkpoint_symbols: 0,
            latest_context_version: None,
            latest_brief_path: None,
            latest_brief_hash: None,
            latest_brief_generated_at_unix: None,
            latest_context_reason: None,
            latest_broker_request: None,
            linked_decisions: BTreeMap::new(),
            resolved_questions: BTreeMap::new(),
            question_texts: BTreeMap::new(),
        };
        guard.tasks.tasks.insert(spec.task_id.clone(), task.clone());
    }

    let mut installed_watch_ids: Vec<String> = Vec::new();
    for registration in &registrations {
        if let Err(err) = install_watch(
            state.clone(),
            watch_tx.clone(),
            registration.watch_id.clone(),
        ) {
            let _ = remove_watch(state.clone(), &registration.watch_id);
            for watch_id in &installed_watch_ids {
                let _ = remove_watch(state.clone(), watch_id);
            }
            let mut guard = state.lock().map_err(lock_err)?;
            guard.tasks.tasks.remove(&spec.task_id);
            guard.watches.watches.retain(|watch| {
                !registrations
                    .iter()
                    .any(|candidate| candidate.watch_id == watch.watch_id)
            });
            persist_state(&guard)?;
            return Err(err);
        }
        installed_watch_ids.push(registration.watch_id.clone());
    }

    {
        let guard = state.lock().map_err(lock_err)?;
        persist_state(&guard)?;
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
        let _ = emit_task_event(
            state.clone(),
            task_id,
            "task_started",
            json!({"task_id": task_id, "step_count": sequence.steps.len()}),
        );

        let mut observer = TaskSequenceObserver {
            state: state.clone(),
            task_id: task_id.to_string(),
        };
        let result = kernel.execute_sequence_with_observer(sequence, &mut observer);

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
                    daemon_log(&format!("task run failed task_id={} error={err}", task_id));
                }
            }
            let rerun = task.pending_replan && !task.cancel_requested;
            if rerun {
                task.last_replan_at_unix = Some(now_unix());
            }
            persist_state(&guard)?;
            rerun
        };

        if let Ok(_response) = &result {
            let mut summary =
                refresh_task_context_summary(state.clone(), task_id)?.unwrap_or_else(|| json!({}));
            let _ = set_context_reason(&state, task_id, "replan_applied");
            if let Some(response) = refresh_broker_context_for_task(&state, task_id)? {
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
                        Value::String(response.context_version.clone()),
                    );
                    object.insert(
                        "brief_path".to_string(),
                        Value::String(
                            task_brief_markdown_path(
                                &state.lock().map_err(lock_err)?.root.clone(),
                                task_id,
                            )
                            .to_string_lossy()
                            .to_string(),
                        ),
                    );
                }
            }
            let _ = emit_task_event(state.clone(), task_id, "context_updated", summary);
        }

        match result {
            Ok(response) if rerun => {
                continue;
            }
            Ok(response) => {
                let _ = emit_task_event(
                    state.clone(),
                    task_id,
                    "task_completed",
                    json!({"task_id": task_id, "request_id": response.request_id}),
                );
                return Ok(response);
            }
            Err(err) => {
                let _ = emit_task_event(
                    state.clone(),
                    task_id,
                    "task_failed",
                    json!({"task_id": task_id, "error": err.to_string()}),
                );
                return Err(err.into());
            }
        }
    }
}

fn cancel_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<(Option<TaskRecord>, Vec<String>)> {
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

fn remove_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_id: &str,
) -> Result<Option<WatchRegistration>> {
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

fn restore_watchers(
    state: &Arc<Mutex<DaemonState>>,
    watch_tx: &Sender<WatchEventMsg>,
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
    if let Some(watch) = guard
        .watches
        .watches
        .iter_mut()
        .find(|watch| watch.watch_id == watch_id)
    {
        watch.active = true;
        watch.last_error = None;
    }
    guard.watcher_handles.insert(watch_id.clone(), watcher);
    persist_state(&guard)?;
    daemon_log(&format!(
        "installed watch watch_id={watch_id} task_id={} kind={:?}",
        spec.task_id, spec.kind
    ));
    Ok(())
}

fn spawn_watch_processor(state: Arc<Mutex<DaemonState>>, watch_rx: Receiver<WatchEventMsg>) {
    thread::spawn(move || {
        let mut pending = HashMap::<String, PendingWatchEvent>::new();
        loop {
            flush_due_watch_events(state.clone(), &mut pending);
            let timeout = next_watch_timeout(&pending).unwrap_or(Duration::from_secs(60));
            match watch_rx.recv_timeout(timeout) {
                Ok(message) => {
                    if state
                        .lock()
                        .map_err(lock_err)
                        .map(|guard| guard.shutting_down)
                        .unwrap_or(false)
                    {
                        break;
                    }
                    merge_watch_event(state.clone(), &mut pending, message);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if state
                        .lock()
                        .map_err(lock_err)
                        .map(|guard| guard.shutting_down)
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        flush_all_watch_events(state, &mut pending);
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
    daemon_log(&format!(
        "watch event watch_id={} task_id={} paths={}",
        watch.watch_id,
        task_id,
        paths.join(",")
    ));
    let _ = emit_task_event(
        state.clone(),
        &task_id,
        "watch_triggered",
        json!({
            "watch_id": watch.watch_id,
            "paths": paths,
            "kind": format!("{:?}", watch.spec.kind),
        }),
    );

    if sequence_present {
        let _ = set_context_reason(&state, &task_id, "watch_triggered");
        let context_version = bump_context_version(&state, &task_id)?;
        let mut spawn_replan = false;
        {
            let mut guard = state.lock().map_err(lock_err)?;
            if let Some(task) = guard.tasks.tasks.get_mut(&task_id) {
                if task.running {
                    task.pending_replan = true;
                } else {
                    task.running = true;
                    spawn_replan = true;
                }
            }
            persist_state(&guard)?;
        }
        let _ = emit_task_event(
            state.clone(),
            &task_id,
            "replan_requested",
            json!({"task_id": task_id, "context_version": context_version}),
        );
        if spawn_replan {
            let state_clone = state.clone();
            daemon_log(&format!("spawning replan task_id={task_id}"));
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
        .arg("-z")
        .arg("--untracked-files=no")
        .current_dir(root)
        .output()
        .context("failed to run git status")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let entries = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0;
    while index < entries.len() {
        let entry = entries[index];
        index += 1;
        if entry.len() <= 3 {
            continue;
        }
        let record = String::from_utf8_lossy(entry);
        let status = &record[..3];
        let mut path = record[3..].trim().to_string();
        let is_rename_or_copy = matches!(status.chars().next(), Some('R' | 'C'))
            || matches!(status.chars().nth(1), Some('R' | 'C'));
        if let Some((_, destination)) = path.rsplit_once("->") {
            path = destination.trim().to_string();
        } else if is_rename_or_copy && index < entries.len() {
            index += 1;
        }
        let path = path.replace('\\', "/");
        if !path.is_empty() && !paths.iter().any(|candidate| candidate == &path) {
            paths.push(path);
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
    let mut paths = spec.paths.clone();
    paths.sort();
    let mut include_globs = spec.include_globs.clone();
    include_globs.sort();
    let mut exclude_globs = spec.exclude_globs.clone();
    exclude_globs.sort();
    let seed = serde_json::to_vec(&json!({
        "kind": spec.kind,
        "task_id": spec.task_id,
        "root": spec.root,
        "paths": paths,
        "include_globs": include_globs,
        "exclude_globs": exclude_globs,
        "debounce_ms": spec.debounce_ms,
    }))
    .expect("watch id seed should serialize");
    let hash = blake3::hash(&seed).to_hex();
    format!("watch-{}", &hash[..12])
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
    pending: &mut HashMap<String, PendingWatchEvent>,
    message: WatchEventMsg,
) {
    let debounce_ms = watch_debounce_ms(&state, &message.watch_id).unwrap_or(250);
    let due_at = Instant::now() + Duration::from_millis(debounce_ms);
    let entry = pending
        .entry(message.watch_id.clone())
        .or_insert_with(|| PendingWatchEvent {
            watch_id: message.watch_id.clone(),
            paths: Vec::new(),
            error: None,
            due_at,
        });
    entry.due_at = due_at;
    if let Some(error) = message.error {
        entry.error = Some(error);
    }
    for path in message.paths {
        if !entry.paths.iter().any(|existing| existing == &path) {
            entry.paths.push(path);
        }
    }
}

fn flush_due_watch_events(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<String, PendingWatchEvent>,
) {
    let now = Instant::now();
    let due = pending
        .iter()
        .filter_map(|(watch_id, entry)| (entry.due_at <= now).then_some(watch_id.clone()))
        .collect::<Vec<_>>();
    for watch_id in due {
        if let Some(entry) = pending.remove(&watch_id) {
            let message = WatchEventMsg {
                watch_id: entry.watch_id,
                paths: entry.paths,
                error: entry.error,
            };
            if let Err(err) = process_watch_event(state.clone(), message) {
                daemon_log(&format!("watch processing failed: {err}"));
            }
        }
    }
}

fn flush_all_watch_events(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<String, PendingWatchEvent>,
) {
    let watch_ids = pending.keys().cloned().collect::<Vec<_>>();
    for watch_id in watch_ids {
        if let Some(entry) = pending.remove(&watch_id) {
            let message = WatchEventMsg {
                watch_id: entry.watch_id,
                paths: entry.paths,
                error: entry.error,
            };
            if let Err(err) = process_watch_event(state.clone(), message) {
                daemon_log(&format!("watch processing failed during flush: {err}"));
            }
        }
    }
}

fn next_watch_timeout(pending: &HashMap<String, PendingWatchEvent>) -> Option<Duration> {
    pending
        .values()
        .map(|entry| entry.due_at)
        .min()
        .map(|due_at| due_at.saturating_duration_since(Instant::now()))
}

fn watch_debounce_ms(state: &Arc<Mutex<DaemonState>>, watch_id: &str) -> Option<u64> {
    let guard = state.lock().ok()?;
    guard
        .watches
        .watches
        .iter()
        .find(|watch| watch.watch_id == watch_id)
        .and_then(|watch| watch.spec.debounce_ms)
}

fn persist_state(state: &DaemonState) -> Result<()> {
    save_watch_registry(&state.root, &state.watches)?;
    save_task_registry(&state.root, &state.tasks)?;
    Ok(())
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

fn wake_listener(root: &Path) {
    let _ = UnixStream::connect(socket_path(root));
}

fn daemon_log(message: &str) {
    eprintln!("[packet28d {}] {message}", now_unix());
}

fn bind_listener(socket: &Path) -> Result<UnixListener> {
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
                fs::remove_file(socket).with_context(|| {
                    format!("failed to remove stale socket '{}'", socket.display())
                })?;
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to probe existing socket '{}'", socket.display())
                });
            }
        }
    }

    UnixListener::bind(socket).with_context(|| format!("failed to bind '{}'", socket.display()))
}

fn resolve_root(path: &Path) -> PathBuf {
    let mut current = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
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
    let config = if request.config_path.trim().is_empty() {
        suite_foundation_core::CovyConfig::default()
    } else {
        suite_foundation_core::CovyConfig::load(Path::new(&request.config_path))?
    };
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
            ingest_coverage_stdin: |_format| {
                anyhow::bail!("stdin is not supported through packet28d")
            },
            ingest_diagnostics: |path| {
                covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
            },
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
    Ok(ContextStoreStatsResponse {
        stats: cache.stats(),
    })
}

fn run_context_recall(request: ContextRecallRequest) -> Result<ContextRecallResponse> {
    let cache = load_cache_root(&request.root);
    let now = now_unix();
    let since_default = now.saturating_sub(86_400);
    let scope = match request.scope.as_deref().unwrap_or_default() {
        "task_first" => context_memory_core::RecallScope::TaskFirst,
        "task_only" => context_memory_core::RecallScope::TaskOnly,
        _ if request.task_id.is_some() => context_memory_core::RecallScope::TaskFirst,
        _ => context_memory_core::RecallScope::Global,
    };
    let hits = cache.recall(
        &request.query,
        &RecallOptions {
            limit: request.limit,
            since_unix: request.since.or(Some(since_default)),
            until_unix: request.until,
            target: request.target,
            task_id: request.task_id,
            scope,
            packet_types: request.packet_types,
            path_filters: request.path_filters,
            symbol_filters: request.symbol_filters,
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
        Some("whale-lpt") => Ok(Some(
            testy_core::command_shard::PlannerAlgorithmArg::WhaleLpt,
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_limits_override_verbosity_alias() {
        let mut section_limits = BTreeMap::new();
        section_limits.insert("relevant_context".to_string(), 2);
        let limits = resolve_effective_limits(
            BrokerAction::Plan,
            Some(BrokerVerbosity::Rich),
            Some(3),
            Some(5),
            &section_limits,
        );
        assert_eq!(limits.max_sections, 3);
        assert_eq!(limits.default_max_items_per_section, 5);
        assert_eq!(limits.section_item_limits["relevant_context"], 2);
    }

    #[test]
    fn omitted_explicit_limits_use_deterministic_action_defaults() {
        let plan_limits =
            resolve_effective_limits(BrokerAction::Plan, None, None, None, &BTreeMap::new());
        let choose_tool_limits =
            resolve_effective_limits(BrokerAction::ChooseTool, None, None, None, &BTreeMap::new());
        assert_eq!(plan_limits.max_sections, 8);
        assert_eq!(plan_limits.default_max_items_per_section, 8);
        assert_eq!(plan_limits.section_item_limits["code_evidence"], 6);
        assert_eq!(choose_tool_limits.max_sections, 6);
        assert_eq!(choose_tool_limits.default_max_items_per_section, 5);
    }

    #[test]
    fn brief_always_starts_with_supersession_header() {
        let brief = render_brief(
            "task-123",
            "7",
            &[BrokerSection {
                id: "task_objective".to_string(),
                title: "Task Objective".to_string(),
                body: "Investigate auth flow".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            }],
        );
        assert!(brief.starts_with("[Packet28 Context v7"));
        assert!(brief.contains("supersedes all prior Packet28 context"));
    }

    #[test]
    fn normalize_plan_steps_trims_and_assigns_missing_ids() {
        let normalized = normalize_plan_steps(&[BrokerPlanStep {
            id: " ".to_string(),
            action: " Edit ".to_string(),
            description: Some(" touch auth ".to_string()),
            paths: vec!["src/auth.rs".to_string(), "src/auth.rs".to_string()],
            symbols: vec![" Login ".to_string()],
            depends_on: vec![" prev ".to_string(), "prev".to_string()],
        }]);
        assert_eq!(normalized[0].id, "step-1");
        assert_eq!(normalized[0].action, "edit");
        assert_eq!(normalized[0].description.as_deref(), Some("touch auth"));
        assert_eq!(normalized[0].paths, vec!["src/auth.rs".to_string()]);
        assert_eq!(normalized[0].symbols, vec!["Login".to_string()]);
        assert_eq!(normalized[0].depends_on, vec!["prev".to_string()]);
    }

    #[test]
    fn infer_scope_paths_prefers_explicit_paths() {
        let inferred = infer_scope_paths(
            "refactor auth module",
            &mapy_core::RepoMapPayloadRich {
                files_ranked: vec![
                    mapy_core::RankedFileRich {
                        path: "src/auth.rs".to_string(),
                        score: 1.0,
                        symbol_count: 1,
                        import_count: 0,
                    },
                    mapy_core::RankedFileRich {
                        path: "src/session.rs".to_string(),
                        score: 0.8,
                        symbol_count: 1,
                        import_count: 0,
                    },
                ],
                ..Default::default()
            },
            &["src/session.rs".to_string()],
            &[],
        );
        assert_eq!(inferred, vec!["src/session.rs".to_string()]);
    }

    #[test]
    fn derive_query_focus_extracts_symbol_terms() {
        let focus = derive_query_focus(Some(
            "What does StringUtils.abbreviate() do in src/main/java/StringUtils.java?",
        ));
        assert!(focus
            .full_symbol_terms
            .contains(&"StringUtils.abbreviate".to_string()));
        assert!(focus.symbol_terms.iter().any(|item| item == "StringUtils"));
        assert!(focus.symbol_terms.iter().any(|item| item == "abbreviate"));
        assert!(focus
            .path_terms
            .iter()
            .any(|item| item.contains("StringUtils.java")));
    }

    #[test]
    fn expand_scope_paths_pulls_adjacent_role_files() {
        let expanded = expand_scope_paths(
            "explain what diffy does",
            &mapy_core::RepoMapPayloadRich {
                files_ranked: vec![
                    mapy_core::RankedFileRich {
                        path: "crates/diffy-core/src/lib.rs".to_string(),
                        score: 1.0,
                        symbol_count: 2,
                        import_count: 1,
                    },
                    mapy_core::RankedFileRich {
                        path: "crates/diffy-core/src/report.rs".to_string(),
                        score: 0.7,
                        symbol_count: 2,
                        import_count: 0,
                    },
                    mapy_core::RankedFileRich {
                        path: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                        score: 0.65,
                        symbol_count: 2,
                        import_count: 1,
                    },
                    mapy_core::RankedFileRich {
                        path: "crates/testy-core/src/lib.rs".to_string(),
                        score: 0.6,
                        symbol_count: 2,
                        import_count: 0,
                    },
                ],
                symbols_ranked: vec![
                    mapy_core::RankedSymbolRich {
                        name: "analyze".to_string(),
                        file: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                        kind: "function".to_string(),
                        score: 0.9,
                    },
                    mapy_core::RankedSymbolRich {
                        name: "render_report".to_string(),
                        file: "crates/diffy-core/src/report.rs".to_string(),
                        kind: "function".to_string(),
                        score: 0.8,
                    },
                ],
                edges: vec![
                    mapy_core::RepoEdgeRich {
                        from: "crates/diffy-cli/src/cmd_analyze.rs".to_string(),
                        to: "crates/diffy-core/src/lib.rs".to_string(),
                        kind: "import".to_string(),
                    },
                    mapy_core::RepoEdgeRich {
                        from: "crates/diffy-core/src/report.rs".to_string(),
                        to: "crates/diffy-core/src/lib.rs".to_string(),
                        kind: "import".to_string(),
                    },
                ],
                ..Default::default()
            },
            &["crates/diffy-core/src/lib.rs".to_string()],
            &["diffy".to_string()],
            6,
        );
        assert!(expanded.contains(&"crates/diffy-core/src/report.rs".to_string()));
        assert!(expanded.contains(&"crates/diffy-cli/src/cmd_analyze.rs".to_string()));
    }

    #[test]
    fn extract_code_evidence_prefers_query_hits_and_context() {
        let root =
            std::env::temp_dir().join(format!("packet28d-code-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/lib.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "use std::fmt;\n\npub struct Diffy;\nimpl Diffy {\n    pub fn analyze() {}\n    pub fn summarize() {}\n}\n",
        )
        .unwrap();

        let evidence = extract_code_evidence(
            &root,
            "src/lib.rs",
            &derive_query_focus(Some("Diffy.analyze")),
            3,
            6,
        );
        assert!(evidence
            .primary_match_symbol
            .as_deref()
            .is_some_and(|value| value == "analyze" || value == "Diffy"));
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("pub fn analyze")));
        assert!(evidence
            .rendered_lines
            .iter()
            .all(|line| !line.contains("use std::fmt")));
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("impl Diffy") || line.contains("pub struct Diffy")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn budget_pruning_drops_optional_sections_before_critical_ones() {
        let sections = vec![
            BrokerSection {
                id: "task_objective".to_string(),
                title: "Task Objective".to_string(),
                body: "Investigate Alpha".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "code_evidence".to_string(),
                title: "Code Evidence".to_string(),
                body: ["- src/alpha.rs:1 fn alpha() {}", "- src/alpha.rs:2 struct Alpha;"]
                    .join("\n"),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "repo_map".to_string(),
                title: "Relevant Files".to_string(),
                body: "- src/alpha.rs [score=0.95] — contains Alpha".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "recent_tool_activity".to_string(),
                title: "Recent Tool Activity".to_string(),
                body: [
                    "- #1 read [read] alpha -> found Alpha",
                    "- #2 grep [search] alpha -> found alpha()",
                ]
                .join("\n"),
                priority: 2,
                source_kind: BrokerSourceKind::Derived,
            },
        ];
        let budget_tokens = estimate_text_cost(&sections[0].body).0
            + estimate_text_cost(&sections[1].body).0
            + estimate_text_cost(&sections[2].body).0
            + 2;
        let budget_bytes = estimate_text_cost(&sections[0].body).1
            + estimate_text_cost(&sections[1].body).1
            + estimate_text_cost(&sections[2].body).1
            + 8;
        let (selected, evicted) = prune_sections_for_budget(
            BrokerAction::Inspect,
            sections,
            budget_tokens,
            budget_bytes,
            8,
        );
        assert!(selected.iter().any(|section| section.id == "code_evidence"));
        assert!(selected.iter().any(|section| section.id == "repo_map"));
        assert!(!selected
            .iter()
            .any(|section| section.id == "recent_tool_activity"));
        assert!(evicted.iter().any(|candidate| {
            candidate.section_id == "recent_tool_activity" && candidate.reason == "budget_pruned"
        }));
    }

    #[test]
    fn budget_pruning_shrinks_critical_sections_before_dropping_them() {
        let code_evidence_body = (1..=8)
            .map(|idx| format!("- src/alpha.rs:{idx} evidence line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let sections = vec![
            BrokerSection {
                id: "task_objective".to_string(),
                title: "Task Objective".to_string(),
                body: "Edit Alpha".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "code_evidence".to_string(),
                title: "Code Evidence".to_string(),
                body: code_evidence_body.clone(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "repo_map".to_string(),
                title: "Relevant Files".to_string(),
                body: "- src/alpha.rs [score=0.95] — contains Alpha".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
        ];
        let objective_cost = estimate_text_cost(&sections[0].body);
        let partial_code_cost = estimate_text_cost(
            &code_evidence_body
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let (selected, _) = prune_sections_for_budget(
            BrokerAction::Inspect,
            sections,
            objective_cost.0 + partial_code_cost.0 + 2,
            objective_cost.1 + partial_code_cost.1 + 8,
            8,
        );
        let code_evidence = selected
            .iter()
            .find(|section| section.id == "code_evidence")
            .expect("code_evidence should be retained");
        assert!(code_evidence.body.len() < code_evidence_body.len());
        assert!(code_evidence.body.contains("src/alpha.rs:1"));
    }
}
