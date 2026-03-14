use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};
use packet28_daemon_core::{
    append_task_event, ensure_daemon_dir, index_dir, index_manifest_path, index_snapshot_path,
    load_task_events, load_task_registry, load_watch_registry, log_path, now_unix,
    read_socket_message, ready_path, remove_runtime_files, save_task_registry, save_watch_registry,
    socket_path, task_artifact_dir, task_brief_json_path, task_brief_markdown_path,
    task_event_log_path, task_state_json_path, task_version_json_path, write_runtime_info,
    write_socket_message, BrokerAction, BrokerDecision, BrokerDecomposeIntent,
    BrokerDecomposeRequest, BrokerDecomposeResponse, BrokerDecomposedStep, BrokerDeltaResponse,
    BrokerEstimateContextRequest, BrokerEstimateContextResponse, BrokerEvictionCandidate,
    BrokerGetContextRequest, BrokerGetContextResponse, BrokerPacketRef, BrokerPlanStep,
    BrokerPlanViolation, BrokerPrepareHandoffRequest, BrokerPrepareHandoffResponse, BrokerQuestion,
    BrokerRecommendedAction, BrokerResolvedQuestion, BrokerResponseMode, BrokerSection,
    BrokerSectionEstimate, BrokerSourceKind, BrokerSupersessionMode, BrokerTaskStatusRequest,
    BrokerTaskStatusResponse, BrokerToolResultKind, BrokerValidatePlanRequest,
    BrokerValidatePlanResponse, BrokerVerbosity, BrokerWriteOp, BrokerWriteStateBatchRequest,
    BrokerWriteStateBatchResponse, BrokerWriteStateRequest, BrokerWriteStateResponse,
    ContextRecallRequest, ContextRecallResponse, ContextStoreGetRequest, ContextStoreGetResponse,
    ContextStoreListRequest, ContextStoreListResponse, ContextStorePruneDaemonRequest,
    ContextStorePruneResponse, ContextStoreStatsRequest, ContextStoreStatsResponse,
    CoverCheckRequest, CoverCheckResponse, DaemonEvent, DaemonEventFrame, DaemonIndexClearResponse,
    DaemonIndexManifest, DaemonIndexRebuildRequest, DaemonIndexRebuildResponse,
    DaemonIndexStatusResponse, DaemonRequest, DaemonResponse, DaemonRuntimeInfo, DaemonStatus,
    PacketFetchResponse, TaskAwaitHandoffRequest, TaskAwaitHandoffResponse, TaskLaunchAgentRequest,
    TaskLaunchAgentResponse, TaskRecord, TaskRegistry, TaskSubmitSpec, TestMapRequest,
    TestMapResponse, TestMapSummary, TestShardRequest, TestShardResponse, WatchKind,
    WatchRegistration, WatchRegistry, WatchSpec,
};
use serde_json::{json, Value};

mod launch;
mod broker_handoff;
mod broker_ops;
mod broker_context;
mod broker_limits;
mod broker_render;
mod broker_search;
mod broker_search_plan;
mod broker_snapshot;
mod commands;
mod index;
mod planning;
mod runtime_files;
mod server;
mod state;
mod watch;

use crate::broker_context::{
    broker_decompose, broker_estimate_context, broker_get_context, broker_validate_plan,
    refresh_broker_context_for_task,
};
use crate::broker_handoff::{broker_prepare_handoff, compute_handoff_state};
use crate::commands::{
    run_context_recall, run_context_store_get, run_context_store_list, run_context_store_prune,
    run_context_store_stats, run_cover_check, run_test_map, run_test_shard,
};
use crate::broker_limits::*;
use crate::broker_render::*;
use crate::broker_search::*;
use crate::broker_search_plan::*;
use crate::broker_snapshot::*;
use crate::broker_ops::{broker_task_status, broker_write_state, broker_write_state_batch};
use crate::index::{
    build_index_status, daemon_index_clear, daemon_index_rebuild, daemon_index_status,
    enqueue_full_index_rebuild, enqueue_incremental_index_paths, spawn_index_worker,
};
use crate::launch::{task_await_handoff, task_launch_agent};
use crate::planning::*;
use crate::runtime_files::{
    clear_index_files, default_index_manifest, load_index_manifest_file, load_index_snapshot_file,
    save_index_manifest_file, save_index_snapshot_file,
};
use crate::server::handle_connection;
use crate::state::{
    CachedSourceFile, DaemonState, IndexCommand, InteractiveIndexRuntime, PendingWatchEvent,
    TaskSequenceObserver, WatchEventMsg,
};
use crate::watch::{
    cancel_task, register_task_and_watches, remove_watch, restore_watchers,
    run_sequence_for_task, spawn_watch_processor,
};

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

const DEFAULT_CONTEXT_MANAGE_BUDGET_TOKENS: u64 = 5_000;
const DEFAULT_CONTEXT_MANAGE_BUDGET_BYTES: usize = 32_000;
const INTERACTIVE_INDEX_SCHEMA_VERSION: u32 = 2;
const INDEX_BATCH_DEBOUNCE_MS: u64 = 150;

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
    let manifest = load_index_manifest_file(&root);
    let snapshot = load_index_snapshot_file(&root, &manifest);
    let (index_tx, index_rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(DaemonState {
        root: root.clone(),
        kernel,
        runtime,
        tasks,
        agent_snapshots: BTreeMap::new(),
        watches,
        watcher_handles: HashMap::new(),
        subscribers: HashMap::new(),
        source_file_cache: BTreeMap::new(),
        interactive_index: InteractiveIndexRuntime { manifest, snapshot },
        index_tx,
        shutting_down: false,
    }));

    let (watch_tx, watch_rx) = mpsc::channel();
    restore_watchers(&state, &watch_tx)?;
    spawn_watch_processor(state.clone(), watch_rx);
    spawn_index_worker(state.clone(), index_rx);
    {
        let should_queue = {
            let guard = state.lock().map_err(lock_err)?;
            guard.interactive_index.snapshot.is_none()
                || guard.interactive_index.manifest.status != "ready"
        };
        if should_queue {
            let _ = enqueue_full_index_rebuild(&state);
        }
    }
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
        index: Some(build_index_status(&state.interactive_index)),
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
    let mut changed = false;
    match request.op.unwrap_or(BrokerWriteOp::FileRead) {
        BrokerWriteOp::QuestionOpen => {
            if let (Some(question_id), Some(text)) = (&request.question_id, &request.text) {
                task.question_texts
                    .insert(question_id.clone(), text.clone());
                task.resolved_questions.remove(question_id);
                changed = true;
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
                changed = true;
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
                changed = true;
            }
        }
        BrokerWriteOp::DecisionSupersede => {
            if let Some(decision_id) = &request.decision_id {
                task.linked_decisions.remove(decision_id);
                task.resolved_questions
                    .retain(|_, linked_decision_id| linked_decision_id != decision_id);
                changed = true;
            }
        }
        _ => {}
    }
    if changed {
        persist_state(&guard)?;
    }
    Ok(())
}

fn load_agent_snapshot_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<suite_packet_core::AgentSnapshotPayload> {
    if let Some(snapshot) = state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .get(task_id)
        .cloned()
    {
        return Ok(snapshot);
    }
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
    let snapshot = envelope.payload;
    state
        .lock()
        .map_err(lock_err)?
        .agent_snapshots
        .insert(task_id.to_string(), snapshot.clone());
    Ok(snapshot)
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

fn metadata_mtime_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
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

fn request_query_missing(request: &BrokerGetContextRequest) -> bool {
    request
        .query
        .as_ref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
}

fn inherit_broker_request_defaults(
    request: &mut BrokerGetContextRequest,
    previous: Option<&BrokerGetContextRequest>,
) {
    let Some(previous) = previous else {
        return;
    };
    let action_was_explicit = request.action.is_some();

    if request.action.is_none() {
        request.action = previous.action;
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = previous.budget_tokens;
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = previous.budget_bytes;
    }
    if request.focus_paths.is_empty() {
        request.focus_paths = previous.focus_paths.clone();
    }
    if request.focus_symbols.is_empty() {
        request.focus_symbols = previous.focus_symbols.clone();
    }
    if request.tool_name.is_none() {
        request.tool_name = previous.tool_name.clone();
    }
    if request.tool_result_kind.is_none() {
        request.tool_result_kind = previous.tool_result_kind;
    }
    if request_query_missing(request) {
        request.query = previous
            .query
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }
    if !action_was_explicit && request.include_sections.is_empty() {
        request.include_sections = previous.include_sections.clone();
    }
    if !action_was_explicit && request.exclude_sections.is_empty() {
        request.exclude_sections = previous.exclude_sections.clone();
    }
    if request.verbosity.is_none() {
        request.verbosity = previous.verbosity;
    }
    if request.response_mode.is_none() {
        request.response_mode = previous.response_mode;
    }
    if !action_was_explicit && request.max_sections.is_none() {
        request.max_sections = previous.max_sections;
    }
    if !action_was_explicit && request.default_max_items_per_section.is_none() {
        request.default_max_items_per_section = previous.default_max_items_per_section;
    }
    if !action_was_explicit && request.section_item_limits.is_empty() {
        request.section_item_limits = previous.section_item_limits.clone();
    }
    if request.persist_artifacts.is_none() {
        request.persist_artifacts = previous.persist_artifacts;
    }
}

fn broker_request_response_mode(request: &BrokerGetContextRequest) -> BrokerResponseMode {
    request.response_mode.unwrap_or(BrokerResponseMode::Full)
}

fn should_persist_broker_artifacts(request: &BrokerGetContextRequest) -> bool {
    matches!(
        broker_request_response_mode(request),
        BrokerResponseMode::Slim
    ) || request.persist_artifacts.unwrap_or(true)
}

#[derive(Debug, Clone)]
struct BrokerEffectiveLimits {
    max_sections: usize,
    default_max_items_per_section: usize,
    section_item_limits: BTreeMap<String, usize>,
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
        BrokerWriteOp::Intention => snapshot.latest_intention.as_ref().is_some_and(|intention| {
            request
                .text
                .as_ref()
                .is_some_and(|text| text == &intention.text)
                && request.note == intention.note
                && request.step_id == intention.step_id
                && request.question_id == intention.question_id
                && request.paths == intention.paths
                && request.symbols == intention.symbols
        }),
        BrokerWriteOp::CheckpointSave => request
            .checkpoint_id
            .as_ref()
            .zip(snapshot.latest_checkpoint_id.as_ref())
            .is_some_and(|(next, current)| {
                next == current
                    && request.note == snapshot.checkpoint_note
                    && request.paths == snapshot.checkpoint_focus_paths
                    && request.symbols == snapshot.checkpoint_focus_symbols
            }),
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
    fn derive_query_focus_filters_stopwords_but_keeps_symbols() {
        let focus = derive_query_focus(Some(
            "Where is StringUtils.isBlank defined and used across the codebase?",
        ));
        assert!(!focus.text_tokens.iter().any(|item| item == "where"));
        assert!(!focus.text_tokens.iter().any(|item| item == "defined"));
        assert!(!focus.text_tokens.iter().any(|item| item == "used"));
        assert!(focus
            .full_symbol_terms
            .contains(&"StringUtils.isBlank".to_string()));
        assert!(focus
            .symbol_terms
            .iter()
            .any(|item| item.eq_ignore_ascii_case("isBlank")));
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

    fn write_search_fixture(root: &std::path::Path, files: &[(&str, &str)]) {
        let _ = std::fs::remove_dir_all(root);
        for (relative_path, contents) in files {
            let path = root.join(relative_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    fn run_search_execution_for_query(
        root: &std::path::Path,
        query: &str,
        action: BrokerAction,
    ) -> SearchExecution {
        let snapshot = suite_packet_core::AgentSnapshotPayload::default();
        let request = BrokerGetContextRequest {
            task_id: "task-search".to_string(),
            action: Some(action),
            query: Some(query.to_string()),
            ..BrokerGetContextRequest::default()
        };
        let query_focus = derive_query_focus(Some(query));
        build_reducer_search_execution(None, root, &snapshot, &request, &query_focus, action, 8, 8)
    }

    #[test]
    fn exact_symbol_query_returns_definition_first_without_fallback() {
        let root =
            std::env::temp_dir().join(format!("packet28d-search-exact-{}", std::process::id()));
        write_search_fixture(
            &root,
            &[
                (
                    "src/alpha.rs",
                    "pub struct Alpha;\nimpl Alpha { pub fn build() {} }\n",
                ),
                (
                    "src/mentions.rs",
                    "fn helper() { let _ = Alpha::build(); }\n",
                ),
            ],
        );

        let execution =
            run_search_execution_for_query(&root, "Where is Alpha defined?", BrokerAction::Inspect);
        assert!(!execution.used_fallback);
        assert_eq!(
            execution.files.first().map(|file| file.path.as_str()),
            Some("src/alpha.rs")
        );
        assert!(execution.files[0].definition_hits > 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn vague_query_triggers_fallback_only_after_weak_first_pass() {
        let root =
            std::env::temp_dir().join(format!("packet28d-search-fallback-{}", std::process::id()));
        write_search_fixture(
            &root,
            &[
                ("src/alpha.rs", "pub struct AlphaService;\n"),
                (
                    "src/alpha_update.rs",
                    "pub fn update_state_for_alpha_service() {}\n",
                ),
            ],
        );

        let execution = run_search_execution_for_query(
            &root,
            "How is AlphaService.updateState updated?",
            BrokerAction::Inspect,
        );
        assert!(execution.used_fallback);
        assert!(execution
            .files
            .iter()
            .any(|file| file.path == "src/alpha_update.rs"));
        assert!(execution
            .evidence_by_file
            .get("src/alpha_update.rs")
            .is_some_and(|summary| summary
                .rendered_lines
                .iter()
                .any(|line| line.contains("update_state_for_alpha_service"))));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn definition_hits_outrank_bulk_references() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-search-definition-rank-{}",
            std::process::id()
        ));
        write_search_fixture(
            &root,
            &[
                (
                    "src/alpha.rs",
                    "pub struct Alpha;\n",
                ),
                (
                    "src/references.rs",
                    "fn one() { let _ = Alpha; }\nfn two() { let _ = Alpha; }\nfn three() { let _ = Alpha; }\nfn four() { let _ = Alpha; }\n",
                ),
            ],
        );

        let execution = run_search_execution_for_query(&root, "Alpha", BrokerAction::Inspect);
        assert_eq!(
            execution.files.first().map(|file| file.path.as_str()),
            Some("src/alpha.rs")
        );
        assert!(execution.files[0].definition_hits >= execution.files[1].definition_hits);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn broad_generic_tokens_do_not_outrank_exact_symbol_hits() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-search-generic-rank-{}",
            std::process::id()
        ));
        write_search_fixture(
            &root,
            &[
                (
                    "src/request.rs",
                    "pub struct BrokerWriteStateRequest {\n    pub task_id: String,\n}\n",
                ),
                (
                    "src/noise.rs",
                    "pub fn a(task_id: &str) {}\npub fn b(task_id: &str) {}\npub fn c(task_id: &str) {}\npub fn d(task_id: &str) {}\n",
                ),
            ],
        );

        let execution = run_search_execution_for_query(
            &root,
            "How does BrokerWriteStateRequest use task_id?",
            BrokerAction::Inspect,
        );
        assert_eq!(
            execution.files.first().map(|file| file.path.as_str()),
            Some("src/request.rs")
        );
        assert!(execution.files[0].exact_symbol_hits > 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn choose_tool_uses_the_same_staged_search_planner() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-search-choose-tool-{}",
            std::process::id()
        ));
        write_search_fixture(
            &root,
            &[
                ("src/alpha.rs", "pub struct AlphaService;\n"),
                (
                    "src/alpha_update.rs",
                    "pub fn update_state_for_alpha_service() {}\n",
                ),
            ],
        );

        let execution = run_search_execution_for_query(
            &root,
            "How is AlphaService.updateState updated?",
            BrokerAction::ChooseTool,
        );
        assert!(execution.used_fallback);
        assert!(execution
            .files
            .iter()
            .any(|file| file.path == "src/alpha_update.rs"));

        let _ = std::fs::remove_dir_all(&root);
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
            &[],
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
    fn extract_code_evidence_ignores_license_headers_and_prefers_focus_symbols() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-code-evidence-java-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/StringUtils.java");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "/*\n * Licensed to the Apache Software Foundation (ASF)\n */\npackage org.example;\n\npublic class StringUtils {\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

        let mut focus = derive_query_focus(Some(
            "Where is StringUtils.isBlank defined and used across the codebase?",
        ));
        focus.full_symbol_terms.clear();
        focus.symbol_terms.clear();
        let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
        let evidence = extract_code_evidence(&root, "src/StringUtils.java", &focus, &[], 3, 6);
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("isBlank(final CharSequence cs)")));
        assert!(evidence
            .rendered_lines
            .iter()
            .all(|line| !line.contains("Licensed to the Apache")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_code_evidence_prefers_symbol_definitions_over_comment_mentions() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-code-evidence-priority-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/StringUtils.java");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "package org.example;\n\npublic final class StringUtils {\n    /** Mention isBlank in docs before the definition. */\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

        let mut focus = derive_query_focus(Some(
            "Where is StringUtils.isBlank defined and used across the codebase?",
        ));
        focus.full_symbol_terms.clear();
        focus.symbol_terms.clear();
        let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
        let evidence = extract_code_evidence(&root, "src/StringUtils.java", &focus, &[], 1, 3);
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("isBlank(final CharSequence cs)")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_code_evidence_prefers_region_hints_when_present() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-code-evidence-region-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/StringUtils.java");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "package org.example;\n\npublic final class StringUtils {\n    /** Mention isBlank in docs before the definition. */\n    public static String describe() { return \"isBlank docs\"; }\n\n    public static boolean isBlank(final CharSequence cs) {\n        return cs == null || cs.length() == 0;\n    }\n}\n",
        )
        .unwrap();

        let mut focus = derive_query_focus(Some(
            "Where is StringUtils.isBlank defined and used across the codebase?",
        ));
        focus.full_symbol_terms.clear();
        focus.symbol_terms.clear();
        let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
        let provenance = vec![ToolResultProvenance {
            regions: vec!["src/StringUtils.java:7-8".to_string()],
        }];
        let evidence =
            extract_code_evidence(&root, "src/StringUtils.java", &focus, &provenance, 1, 3);
        assert!(evidence.from_region_hint);
        assert_eq!(
            evidence.primary_match_kind,
            Some(EvidenceMatchKind::DefinesSymbol)
        );
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("isBlank(final CharSequence cs)")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_code_evidence_skips_unrelated_signatures_when_symbol_focus_exists() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-code-evidence-unrelated-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/ArrayUtils.java");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "package org.example;\n\npublic class ArrayUtils {\n    public static void shuffle() {}\n}\n",
        )
        .unwrap();

        let mut focus = derive_query_focus(Some(
            "Where is StringUtils.isBlank defined and used across the codebase?",
        ));
        focus.full_symbol_terms.clear();
        focus.symbol_terms.clear();
        let focus = merge_query_focus_with_symbols(focus, &["isBlank".to_string()]);
        let evidence = extract_code_evidence(&root, "src/ArrayUtils.java", &focus, &[], 3, 6);
        assert!(evidence.rendered_lines.is_empty());
        assert!(evidence.primary_match_symbol.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_code_evidence_prefers_method_match_over_class_declaration() {
        let root = std::env::temp_dir().join(format!(
            "packet28d-code-evidence-method-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("src/ArrayUtils.java");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "package org.example;\n\npublic class ArrayUtils {\n    public static void shuffle() {}\n}\n",
        )
        .unwrap();

        let mut focus = derive_query_focus(Some(
            "Add deterministic seeded shuffle overloads to ArrayUtils",
        ));
        focus.full_symbol_terms.clear();
        focus.symbol_terms.clear();
        let focus = merge_query_focus_with_symbols(
            focus,
            &["ArrayUtils".to_string(), "shuffle".to_string()],
        );
        let evidence = extract_code_evidence(&root, "src/ArrayUtils.java", &focus, &[], 3, 6);
        assert!(evidence
            .rendered_lines
            .iter()
            .any(|line| line.contains("public static void shuffle")));
        assert!(evidence
            .rendered_lines
            .iter()
            .all(|line| !line.contains("public class ArrayUtils")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_budget_notes_section_is_empty_without_budget_pruning() {
        let limits =
            resolve_effective_limits(BrokerAction::Inspect, None, None, None, &BTreeMap::new());
        assert!(build_budget_notes_section(&[], &limits).is_none());
        assert!(build_budget_notes_section(
            &[BrokerEvictionCandidate {
                section_id: "search_evidence".to_string(),
                reason: "search evidence can be regenerated".to_string(),
                est_tokens: 12,
            }],
            &limits
        )
        .is_none());
    }

    #[test]
    fn postprocess_selected_sections_adds_budget_notes_and_compacts_tool_activity() {
        let limits =
            resolve_effective_limits(BrokerAction::Inspect, None, None, None, &BTreeMap::new());
        let snapshot = suite_packet_core::AgentSnapshotPayload {
            recent_tool_invocations: vec![suite_packet_core::ToolInvocationSummary {
                invocation_id: "tool-1".to_string(),
                sequence: 7,
                tool_name: "grep".to_string(),
                operation_kind: suite_packet_core::ToolOperationKind::Search,
                request_summary: Some("search for isBlank".to_string()),
                result_summary: Some("Validate.java:806 calls isBlank".to_string()),
                paths: vec!["src/Validate.java".to_string()],
                regions: vec!["src/Validate.java:806-806".to_string()],
                symbols: vec!["isBlank".to_string()],
                duration_ms: Some(12),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sections = vec![
            BrokerSection {
                id: "task_objective".to_string(),
                title: "Task Objective".to_string(),
                body: "Where is StringUtils.isBlank defined and used?".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "recent_tool_activity".to_string(),
                title: "Recent Tool Activity".to_string(),
                body: "- #7 grep [search] search for isBlank -> Validate.java:806 calls isBlank"
                    .to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "code_evidence".to_string(),
                title: "Code Evidence".to_string(),
                body: "- src/Validate.java:806 if (StringUtils.isBlank(chars))".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
        ];
        let pruned = vec![BrokerEvictionCandidate {
            section_id: "search_evidence".to_string(),
            reason: "budget_pruned".to_string(),
            est_tokens: 491,
        }];

        let processed = postprocess_selected_sections(sections, &pruned, &snapshot, &limits);
        let budget_notes = processed
            .iter()
            .find(|section| section.id == "budget_notes")
            .expect("budget notes should be inserted");
        assert!(budget_notes
            .body
            .contains("search_evidence omitted due to budget"));
        assert!(budget_notes.body.contains("491"));
        let tool_activity = processed
            .iter()
            .find(|section| section.id == "recent_tool_activity")
            .expect("tool activity should remain");
        assert!(tool_activity.body.contains("paths=1"));
        assert!(tool_activity.body.contains("regions=1"));
        assert!(tool_activity.body.contains("duration=12ms"));
        assert!(!tool_activity.body.contains("->"));
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
                body: [
                    "- src/alpha.rs:1 fn alpha() {}",
                    "- src/alpha.rs:2 struct Alpha;",
                ]
                .join("\n"),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
            BrokerSection {
                id: "search_evidence".to_string(),
                title: "Relevant Files".to_string(),
                body: "- src/alpha.rs:1 [matches=2] — direct reducer hit for Alpha".to_string(),
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
        let rendered = render_brief("task-a", "v1", &sections[..3]);
        let (budget_tokens, budget_bytes) = estimate_text_cost(&rendered);
        let (selected, evicted) = prune_sections_for_budget(
            BrokerAction::Inspect,
            sections,
            budget_tokens + 2,
            budget_bytes + 8,
            8,
        );
        assert!(selected.iter().any(|section| section.id == "code_evidence"));
        assert!(selected
            .iter()
            .any(|section| section.id == "search_evidence"));
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
                id: "search_evidence".to_string(),
                title: "Relevant Files".to_string(),
                body: "- src/alpha.rs:1 [matches=2] — direct reducer hit for Alpha".to_string(),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
        ];
        let partial_sections = vec![
            sections[0].clone(),
            BrokerSection {
                id: "code_evidence".to_string(),
                title: "Code Evidence".to_string(),
                body: code_evidence_body
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n"),
                priority: 1,
                source_kind: BrokerSourceKind::Derived,
            },
        ];
        let partial_brief = render_brief("task-a", "v1", &partial_sections);
        let (budget_tokens, budget_bytes) = estimate_text_cost(&partial_brief);
        let (selected, _) = prune_sections_for_budget(
            BrokerAction::Inspect,
            sections,
            budget_tokens + 2,
            budget_bytes + 8,
            8,
        );
        let code_evidence = selected
            .iter()
            .find(|section| section.id == "code_evidence")
            .expect("code_evidence should be retained");
        assert!(code_evidence.body.len() < code_evidence_body.len());
        assert!(code_evidence.body.contains("src/alpha.rs:1"));
    }

    #[test]
    fn inherit_broker_request_defaults_reuses_previous_follow_up_shape() {
        let previous = BrokerGetContextRequest {
            task_id: "task-a".to_string(),
            action: Some(BrokerAction::Inspect),
            budget_tokens: Some(700),
            budget_bytes: Some(2800),
            focus_paths: vec!["src/alpha.rs".to_string()],
            focus_symbols: vec!["Alpha".to_string()],
            query: Some("Where is Alpha defined?".to_string()),
            include_sections: vec!["task_objective".to_string(), "code_evidence".to_string()],
            verbosity: Some(BrokerVerbosity::Rich),
            response_mode: Some(BrokerResponseMode::Delta),
            max_sections: Some(5),
            default_max_items_per_section: Some(3),
            section_item_limits: BTreeMap::from([("code_evidence".to_string(), 2)]),
            persist_artifacts: Some(true),
            ..BrokerGetContextRequest::default()
        };
        let mut current = BrokerGetContextRequest {
            task_id: "task-a".to_string(),
            ..BrokerGetContextRequest::default()
        };

        inherit_broker_request_defaults(&mut current, Some(&previous));

        assert_eq!(current.action, Some(BrokerAction::Inspect));
        assert_eq!(current.query.as_deref(), Some("Where is Alpha defined?"));
        assert_eq!(current.focus_paths, vec!["src/alpha.rs"]);
        assert_eq!(current.focus_symbols, vec!["Alpha"]);
        assert_eq!(
            current.include_sections,
            vec!["task_objective".to_string(), "code_evidence".to_string()]
        );
        assert_eq!(current.response_mode, Some(BrokerResponseMode::Delta));
        assert_eq!(current.section_item_limits["code_evidence"], 2);
    }

    #[test]
    fn reducer_search_only_runs_when_evidence_sections_are_allowed() {
        let only_summary = HashSet::from(["task_objective".to_string(), "progress".to_string()]);
        assert!(!should_run_reducer_search(&only_summary));

        let with_search = HashSet::from(["search_evidence".to_string()]);
        assert!(should_run_reducer_search(&with_search));

        let with_code = HashSet::from(["code_evidence".to_string()]);
        assert!(should_run_reducer_search(&with_code));
    }

    #[test]
    fn render_task_memory_lines_surfaces_recent_state() {
        let snapshot = suite_packet_core::AgentSnapshotPayload {
            files_read: vec!["src/alpha.rs".to_string()],
            latest_intention: Some(suite_packet_core::AgentIntention {
                text: "Inspect Alpha before editing".to_string(),
                note: Some("Need a clean handoff breadcrumb".to_string()),
                step_id: Some("investigating".to_string()),
                paths: vec!["src/alpha.rs".to_string()],
                occurred_at_unix: 1,
                ..suite_packet_core::AgentIntention::default()
            }),
            latest_checkpoint_id: Some("cp-1".to_string()),
            checkpoint_note: Some("Validated shuffle scope".to_string()),
            checkpoint_focus_paths: vec!["src/alpha.rs".to_string()],
            checkpoint_focus_symbols: vec!["Alpha".to_string()],
            changed_paths_since_checkpoint: vec!["src/beta.rs".to_string()],
            changed_symbols_since_checkpoint: vec!["Beta".to_string()],
            evidence_artifact_ids: vec!["artifact-1".to_string()],
            recent_tool_invocations: vec![suite_packet_core::ToolInvocationSummary {
                invocation_id: "tool-1".to_string(),
                sequence: 7,
                tool_name: "manual.read".to_string(),
                operation_kind: suite_packet_core::ToolOperationKind::Read,
                request_summary: Some("Read alpha".to_string()),
                result_summary: Some("Found Alpha".to_string()),
                paths: vec!["src/alpha.rs".to_string()],
                symbols: vec!["Alpha".to_string()],
                occurred_at_unix: 1,
                ..suite_packet_core::ToolInvocationSummary::default()
            }],
            ..suite_packet_core::AgentSnapshotPayload::default()
        };

        let rendered = render_task_memory_lines(&snapshot);

        assert!(rendered
            .iter()
            .any(|line| line
                .contains("latest intention [investigating]: Inspect Alpha before editing")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("latest intention note: Need a clean handoff breadcrumb")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("latest tool: manual.read")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("recently read: src/alpha.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("latest checkpoint: cp-1")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("checkpoint note: Validated shuffle scope")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("checkpoint focus path: src/alpha.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("checkpoint focus symbol: Alpha")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("changed since checkpoint: src/beta.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("changed symbol since checkpoint: Beta")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("evidence artifact: artifact-1")));
    }

    #[test]
    fn compute_handoff_state_requires_checkpoint_and_tracks_newer_intentions() {
        let empty_snapshot = suite_packet_core::AgentSnapshotPayload::default();
        let (ready_without_checkpoint, _) = compute_handoff_state(None, &empty_snapshot);
        assert!(!ready_without_checkpoint);

        let snapshot = suite_packet_core::AgentSnapshotPayload {
            latest_checkpoint_id: Some("cp-1".to_string()),
            latest_intention: Some(suite_packet_core::AgentIntention {
                text: "Resume editing beta".to_string(),
                occurred_at_unix: 20,
                ..suite_packet_core::AgentIntention::default()
            }),
            ..suite_packet_core::AgentSnapshotPayload::default()
        };
        let (ready_initial, _) = compute_handoff_state(None, &snapshot);
        assert!(ready_initial);

        let task = TaskRecord {
            task_id: "task-a".to_string(),
            latest_handoff_generated_at_unix: Some(10),
            latest_handoff_checkpoint_id: Some("cp-1".to_string()),
            ..TaskRecord::default()
        };
        let (ready_newer_intention, _) = compute_handoff_state(Some(&task), &snapshot);
        assert!(ready_newer_intention);

        let stale_snapshot = suite_packet_core::AgentSnapshotPayload {
            latest_checkpoint_id: Some("cp-1".to_string()),
            latest_intention: Some(suite_packet_core::AgentIntention {
                text: "Resume editing beta".to_string(),
                occurred_at_unix: 5,
                ..suite_packet_core::AgentIntention::default()
            }),
            ..suite_packet_core::AgentSnapshotPayload::default()
        };
        let (ready_stale, _) = compute_handoff_state(Some(&task), &stale_snapshot);
        assert!(!ready_stale);
    }

    #[test]
    fn checkpoint_context_lines_surface_saved_focus() {
        let snapshot = suite_packet_core::AgentSnapshotPayload {
            latest_checkpoint_id: Some("cp-42".to_string()),
            checkpoint_note: Some("Seeded shuffle plan".to_string()),
            checkpoint_focus_paths: vec![
                "apache/src/main/java/org/apache/commons/lang3/ArrayUtils.java".to_string(),
            ],
            checkpoint_focus_symbols: vec!["shuffle".to_string()],
            ..suite_packet_core::AgentSnapshotPayload::default()
        };

        let rendered = render_checkpoint_context_lines(&snapshot);

        assert!(rendered
            .iter()
            .any(|line| line.contains("checkpoint: cp-42")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("note: Seeded shuffle plan")));
        assert!(rendered.iter().any(|line| line.contains(
            "focus path: apache/src/main/java/org/apache/commons/lang3/ArrayUtils.java"
        )));
        assert!(rendered
            .iter()
            .any(|line| line.contains("focus symbol: shuffle")));
    }
}
