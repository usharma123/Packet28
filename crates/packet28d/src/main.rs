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

mod broker_context;
mod broker_handoff;
mod broker_limits;
mod broker_ops;
mod broker_render;
mod broker_search;
mod broker_search_plan;
mod broker_snapshot;
mod broker_support;
mod commands;
mod hooks;
mod index;
mod launch;
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
use crate::broker_limits::*;
use crate::broker_ops::{broker_task_status, broker_write_state, broker_write_state_batch};
use crate::broker_render::*;
use crate::broker_search::*;
use crate::broker_search_plan::*;
use crate::broker_snapshot::*;
use crate::broker_support::*;
use crate::commands::{
    run_context_recall, run_context_store_get, run_context_store_list, run_context_store_prune,
    run_context_store_stats, run_cover_check, run_test_map, run_test_shard,
};
use crate::hooks::hook_ingest;
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
    cancel_task, register_task_and_watches, remove_watch, restore_watchers, run_sequence_for_task,
    spawn_watch_processor,
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
        eprintln!("error: {err:#}");
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
            Err(err) if matches!(err.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
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
mod tests;
