use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    load_task_events, task_artifact_dir, task_brief_markdown_path, task_state_json_path,
    task_version_json_path, BrokerPrepareHandoffRequest, BrokerResponseMode,
    BrokerTaskStatusRequest, BrokerTaskStatusResponse, BrokerWriteOp, BrokerWriteStateBatchRequest,
    BrokerWriteStateBatchResponse, BrokerWriteStateRequest, BrokerWriteStateResponse,
    DaemonRequest, DaemonResponse, TaskRecord,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[path = "cmd_mcp_native.rs"]
mod native_tools;
#[path = "cmd_mcp_prompt_resource.rs"]
mod prompt_resource;
#[path = "cmd_mcp_proxy.rs"]
mod proxy;
#[path = "cmd_mcp_support.rs"]
mod support;
#[path = "cmd_mcp_transport.rs"]
mod transport;

use crate::cmd_mcp::native_tools::{
    handle_packet28_fetch_context, handle_packet28_fetch_tool_result,
    handle_packet28_prepare_handoff, handle_packet28_read_regions, handle_packet28_search,
    Packet28FetchContextArgs, Packet28FetchToolResultArgs, Packet28PrepareHandoffArgs,
    Packet28ReadRegionsArgs, Packet28SearchArgs,
};
use crate::cmd_mcp::prompt_resource::{
    handle_prompt_get, handle_resource_read, handle_resources_list, prompt_descriptors,
    resolve_current_task_id,
};
use crate::cmd_mcp::proxy::{load_proxy_config, serve_proxy_stdio};
use crate::cmd_mcp::support::{
    broker_task_status_via_session, broker_write_state_via_session, classify_error_message,
    extract_named_string, extract_paths, extract_symbols, is_retryable_error,
    load_tool_result_artifact, maybe_store_result_artifact, next_task_invocation,
    resolve_session_task_id, store_result_artifact, store_tool_artifact, summarize_json_value,
    track_task, write_auto_capture_state_batch_via_session,
};
use crate::cmd_mcp::transport::{
    read_message, render_command_preview, write_message, McpMessageFraming,
};

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommands,
}

#[derive(Subcommand)]
pub enum McpCommands {
    /// Serve Packet28 as an MCP stdio server
    Serve(McpServeArgs),
    /// Proxy one or more upstream MCP servers and auto-capture tool activity
    Proxy(McpProxyArgs),
}

#[derive(Args, Clone)]
pub struct McpServeArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
}

#[derive(Args, Clone)]
pub struct McpProxyArgs {
    #[arg(long, default_value = ".")]
    pub root: String,

    #[arg(long, default_value = ".mcp.json")]
    pub upstream_config: String,

    #[arg(long)]
    pub task_id: Option<String>,
}

struct McpSessionState {
    initialized: bool,
    shutdown: bool,
    tracked_tasks: BTreeMap<String, u64>,
    current_task_id: Option<String>,
    framing: Option<McpMessageFraming>,
    tool_owners: BTreeMap<String, String>,
    tool_forward_names: BTreeMap<String, String>,
    upstream_tools_cache: Vec<Value>,
    upstream_tools_loaded: bool,
    resource_owners: BTreeMap<String, String>,
    upstream_resources_cache: Vec<Value>,
    upstream_resources_loaded: bool,
    upstream_resource_templates_cache: Vec<Value>,
    upstream_resource_templates_loaded: bool,
    proxy_task_id: Option<String>,
    next_invocation_seq: u64,
    #[cfg(unix)]
    daemon_client: Option<crate::cmd_daemon::PersistentDaemonClient>,
}

impl Default for McpSessionState {
    fn default() -> Self {
        Self {
            initialized: false,
            shutdown: false,
            tracked_tasks: BTreeMap::new(),
            current_task_id: None,
            framing: None,
            tool_owners: BTreeMap::new(),
            tool_forward_names: BTreeMap::new(),
            upstream_tools_cache: Vec::new(),
            upstream_tools_loaded: false,
            resource_owners: BTreeMap::new(),
            upstream_resources_cache: Vec::new(),
            upstream_resources_loaded: false,
            upstream_resource_templates_cache: Vec::new(),
            upstream_resource_templates_loaded: false,
            proxy_task_id: None,
            next_invocation_seq: 0,
            #[cfg(unix)]
            daemon_client: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct McpProxyConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpProxyServerConfig>,
}

impl Default for McpProxyConfig {
    fn default() -> Self {
        Self {
            mcp_servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(default)]
struct McpProxyServerConfig {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
}

const BROKER_SECTION_IDS: &[&str] = &[
    "task_objective",
    "budget_notes",
    "task_memory",
    "agent_intention",
    "checkpoint_context",
    "active_decisions",
    "open_questions",
    "resolved_questions",
    "current_focus",
    "discovered_scope",
    "recent_tool_activity",
    "tool_failures",
    "evidence_cache",
    "checkpoint_deltas",
    "search_evidence",
    "code_evidence",
    "relevant_context",
    "recommended_actions",
    "progress",
];
pub fn run(args: McpArgs) -> Result<i32> {
    match args.command {
        McpCommands::Serve(args) => run_serve(args),
        McpCommands::Proxy(args) => run_proxy(args),
    }
}

fn run_serve(args: McpServeArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    crate::broker_client::ensure_daemon(&root)?;
    serve_stdio(root)?;
    Ok(0)
}

fn run_proxy(args: McpProxyArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    crate::broker_client::ensure_daemon(&root)?;
    let config_path = crate::cmd_common::resolve_path_from_cwd(
        &args.upstream_config,
        &crate::cmd_common::caller_cwd()?,
    );
    let config = load_proxy_config(Path::new(&config_path))?;
    serve_proxy_stdio(
        root,
        config,
        args.task_id
            .unwrap_or_else(|| crate::broker_client::derive_task_id("packet28-mcp-proxy-session")),
    )?;
    Ok(0)
}

fn serve_stdio(root: PathBuf) -> Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let writer = Arc::new(Mutex::new(io::stdout()));
    let session = Arc::new(Mutex::new(McpSessionState::default()));
    start_notification_thread(root.clone(), writer.clone(), session.clone());

    loop {
        let Some((request, framing)) = read_message(&mut reader)? else {
            break;
        };
        if let Ok(mut guard) = session.lock() {
            guard.framing = Some(framing);
        }
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing method"))?;
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let id = request.get("id").cloned();

        if id.is_none() {
            let _ = handle_notification(&root, &session, method, params);
            continue;
        }

        let response = match handle_method(&root, &session, method, params) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(err) => json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{
                    "code":-32000,
                    "message":err.to_string()
                }
            }),
        };
        let mut guard = writer
            .lock()
            .map_err(|_| anyhow!("failed to lock MCP stdout"))?;
        write_message(&mut *guard, &response, framing)?;
    }

    if let Ok(mut guard) = session.lock() {
        guard.shutdown = true;
    }
    Ok(())
}

fn start_notification_thread(
    root: PathBuf,
    writer: Arc<Mutex<io::Stdout>>,
    session: Arc<Mutex<McpSessionState>>,
) {
    thread::spawn(move || loop {
        let (initialized, shutdown, tracked_tasks, framing) = match session.lock() {
            Ok(guard) => (
                guard.initialized,
                guard.shutdown,
                guard.tracked_tasks.clone(),
                guard.framing,
            ),
            Err(_) => return,
        };
        if shutdown {
            return;
        }
        if !initialized || framing.is_none() {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let framing = framing.unwrap_or(McpMessageFraming::ContentLength);

        for (task_id, last_seen_seq) in tracked_tasks {
            let frames = match load_task_events(&root, &task_id) {
                Ok(frames) => frames,
                Err(_) => continue,
            };
            let mut newest_seq = last_seen_seq;
            for frame in frames.into_iter().filter(|frame| frame.seq > last_seen_seq) {
                newest_seq = newest_seq.max(frame.seq);
                if frame.event.kind != "context_updated" {
                    continue;
                }
                let mut params = match frame.event.data {
                    Value::Object(map) => map,
                    other => {
                        let mut map = Map::new();
                        map.insert("data".to_string(), other);
                        map
                    }
                };
                params.insert("task_id".to_string(), Value::String(task_id.clone()));
                params.insert(
                    "context_version".to_string(),
                    params
                        .get("context_version")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                params.insert("event_seq".to_string(), Value::Number(frame.seq.into()));
                let notification = json!({
                    "jsonrpc":"2.0",
                    "method":"notifications/packet28.context_updated",
                    "params": Value::Object(params),
                });
                if let Ok(mut guard) = writer.lock() {
                    let _ = write_message(&mut *guard, &notification, framing);
                }
            }
            if newest_seq > last_seen_seq {
                if let Ok(mut guard) = session.lock() {
                    if let Some(current) = guard.tracked_tasks.get_mut(&task_id) {
                        *current = newest_seq;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    });
}

fn handle_notification(
    _root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    method: &str,
    _params: Value,
) -> Result<()> {
    if method == "notifications/initialized" {
        let mut guard = session
            .lock()
            .map_err(|_| anyhow!("failed to lock MCP session"))?;
        guard.initialized = true;
        return Ok(());
    }
    Ok(())
}

fn handle_method(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    method: &str,
    params: Value,
) -> Result<Value> {
    match method {
        "initialize" => {
            if let Ok(mut guard) = session.lock() {
                guard.initialized = true;
            }
            Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {},
                    "resources": {},
                    "prompts": {}
                },
                "serverInfo": {
                    "name": "Packet28",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }))
        }
        "tools/list" => Ok(json!({
            "tools": [
                {
                    "name": "packet28.search",
                    "description": "Search repository files under the Packet28 root with reducer-backed grouped results and auto-capture the result into broker state. Returns only compact_preview, match_count, and artifact_id by default; fetch full details later by artifact or invocation id.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["query"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "query": {"type":"string"},
                            "paths": {"type":"array","items":{"type":"string"}},
                            "fixed_string": {"type":"boolean"},
                            "case_sensitive": {"type":"boolean"},
                            "whole_word": {"type":"boolean"},
                            "context_lines": {"type":"number"},
                            "max_matches_per_file": {"type":"number"},
                            "max_total_matches": {"type":"number"},
                            "response_mode": {"type":"string","enum":["slim","full"]}
                        }
                    }
                },
                {
                    "name": "packet28.fetch_tool_result",
                    "description": "Fetch a previously stored full native Packet28 tool result by artifact_id or invocation_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"},
                            "artifact_id": {"type":"string"},
                            "invocation_id": {"type":"string"}
                        }
                    }
                },
                {
                    "name": "packet28.fetch_context",
                    "description": "Fetch a previously stored full Packet28 broker context by context_version or artifact_id.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"},
                            "artifact_id": {"type":"string"},
                            "context_version": {"type":"string"}
                        }
                    }
                },
                {
                    "name": "packet28.prepare_handoff",
                    "description": "Prepare a compact Packet28 handoff packet for bootstrapping a fresh worker after a checkpoint.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"},
                            "query": {"type":"string"},
                            "response_mode": {"type":"string","enum":["slim","full"]}
                        }
                    }
                },
                {
                    "name": "packet28.read_regions",
                    "description": "Read file content under the Packet28 root using explicit region hints and auto-capture the result into broker state.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["path"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "path": {"type":"string"},
                            "regions": {"type":"array","items":{"type":"string"}},
                            "line_start": {"type":"number"},
                            "line_end": {"type":"number"}
                        }
                    }
                },
                {
                    "name": "packet28.write_state",
                    "description": "Write one structured agent-state update into Packet28.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["op"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "op": {"type":"string","enum":["focus_set","focus_clear","file_read","file_edit","intention","checkpoint_save","decision_add","decision_supersede","step_complete","question_open","question_resolve","tool_invocation_started","tool_invocation_completed","tool_invocation_failed","tool_result","focus_inferred","evidence_captured"]},
                            "paths": {"type":"array","items":{"type":"string"}},
                            "symbols": {"type":"array","items":{"type":"string"}},
                            "note": {"type":"string"},
                            "decision_id": {"type":"string"},
                            "question_id": {"type":"string"},
                            "checkpoint_id": {"type":"string"},
                            "step_id": {"type":"string"},
                            "text": {"type":"string"},
                            "regions": {"type":"array","items":{"type":"string"}},
                            "resolves_question_id": {"type":"string"},
                            "resolution_decision_id": {"type":"string"},
                            "invocation_id": {"type":"string"},
                            "tool_name": {"type":"string"},
                            "server_name": {"type":"string"},
                            "operation_kind": {"type":"string","enum":["search","read","edit","build","test","diff","git","fetch","generic"]},
                            "request_summary": {"type":"string"},
                            "result_summary": {"type":"string"},
                            "request_fingerprint": {"type":"string"},
                            "search_query": {"type":"string"},
                            "command": {"type":"string"},
                            "sequence": {"type":"number"},
                            "duration_ms": {"type":"number"},
                            "error_class": {"type":"string"},
                            "error_message": {"type":"string"},
                            "retryable": {"type":"boolean"},
                            "artifact_id": {"type":"string"},
                            "refresh_context": {"type":"boolean"}
                        }
                    }
                },
                {
                    "name": "packet28.task_status",
                    "description": "Return current Packet28 task status and broker artifact paths.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"}
                        }
                    }
                },
                {
                    "name": "packet28.capabilities",
                    "description": "Describe Packet28 broker capabilities and supported section/filter modes.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        })),
        "prompts/list" => Ok(json!({
            "prompts": prompt_descriptors(),
        })),
        "prompts/get" => handle_prompt_get(root, session, params),
        "tools/call" => handle_tool_call(root, session, params),
        "resources/list" => handle_resources_list(root, session),
        "resources/templates/list" => Ok(json!({
            "resourceTemplates": [
                {
                    "uriTemplate": "packet28://task/{task_id}/brief",
                    "name": "Packet28 task brief",
                    "description": "Latest brokered brief for a task."
                },
                {
                    "uriTemplate": "packet28://task/{task_id}/events",
                    "name": "Packet28 task events",
                    "description": "Event stream replay for a task."
                },
                {
                    "uriTemplate": "packet28://task/{task_id}/state",
                    "name": "Packet28 task state",
                    "description": "Current task state metadata for a task."
                },
                {
                    "uriTemplate": "packet28://current/{artifact}",
                    "name": "Packet28 current task artifact",
                    "description": "Current task aliases for task, brief, events, and state."
                }
            ]
        })),
        "resources/read" => handle_resource_read(root, session, params),
        _ => Err(anyhow!("unsupported MCP method '{method}'")),
    }
}

fn handle_tool_call(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    params: Value,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let payload = match name {
        "packet28.search" => {
            let mut request: Packet28SearchArgs = serde_json::from_value(arguments)?;
            request.task_id =
                resolve_session_task_id(session, root, &request.task_id, None, "packet28.search")?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_search(root, session, request)?
        }
        "packet28.fetch_tool_result" => {
            let mut request: Packet28FetchToolResultArgs = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.fetch_tool_result",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_fetch_tool_result(root, request)?
        }
        "packet28.fetch_context" => {
            let mut request: Packet28FetchContextArgs = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.fetch_context",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_fetch_context(root, request)?
        }
        "packet28.prepare_handoff" => {
            let mut request: Packet28PrepareHandoffArgs = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.prepare_handoff",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_prepare_handoff(root, request)?
        }
        "packet28.read_regions" => {
            let mut request: Packet28ReadRegionsArgs = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.read_regions",
            )?;
            track_task(session, root, &request.task_id)?;
            handle_packet28_read_regions(root, session, request)?
        }
        "packet28.write_state" => {
            let mut request: BrokerWriteStateRequest = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.write_state",
            )?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(broker_write_state_via_session(root, session, request)?)?
        }
        "packet28.task_status" => {
            let task_id = resolve_session_task_id(
                session,
                root,
                arguments
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                None,
                "packet28.task_status",
            )?;
            track_task(session, root, &task_id)?;
            serde_json::to_value(broker_task_status_via_session(root, session, &task_id)?)?
        }
        "packet28.capabilities" => capabilities_payload(),
        _ => return Err(anyhow!("unsupported tool '{name}'")),
    };
    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": summarize_tool_payload(name, &payload)
            }
        ],
        "structuredContent": payload
    }))
}

fn capabilities_payload() -> Value {
    json!({
        "section_ids": BROKER_SECTION_IDS,
        "verbosity_modes": ["compact", "standard", "rich"],
        "response_modes": ["slim", "full"],
        "tools": ["packet28.search", "packet28.fetch_tool_result", "packet28.fetch_context", "packet28.prepare_handoff", "packet28.read_regions", "packet28.write_state", "packet28.task_status", "packet28.capabilities"],
        "prompts": ["packet28.start_task", "packet28.continue_task", "packet28.summarize_current_context"],
        "tool_result_kinds": ["build", "stack", "test", "diff", "generic"],
        "push_notifications": {
            "supported": true,
            "method": "notifications/packet28.context_updated"
        },
        "resources": {
            "current_aliases": ["packet28://current/task", "packet28://current/brief", "packet28://current/events", "packet28://current/state"]
        },
        "filters": {
            "include_sections": true,
            "exclude_sections": true,
            "include_self_context_default": false
        },
        "session": {
            "current_task_default": true,
            "task_id_optional_after_first_task": true
        },
        "search": {
            "response_modes": ["slim", "full"],
            "default_response_mode": "slim",
            "detail_fetch_tool": "packet28.fetch_tool_result",
            "slim_fields": ["compact_preview", "match_count", "artifact_id"]
        },
        "context": {
            "response_modes": ["full"],
            "default_response_mode": "full",
            "detail_fetch_tool": "packet28.fetch_context",
            "use_case": "artifact_inspection_only"
        },
        "handoff": {
            "tool": "packet28.prepare_handoff",
            "default_response_mode": "slim",
            "checkpoint_required": true,
            "detail_fetch_tool": "packet28.fetch_context"
        },
        "section_limits": {
            "explicit_limits_supported": true,
            "deprecated_verbosity_alias": true
        },
        "relaunch": {
            "daemon_managed": true,
            "fresh_worker_recommended": true
        },
        "supersession": {
            "supported": true,
            "mode": "replace",
            "brief_header": true
        },
        "adapter_contract": {
            "window_mode": "replace",
            "local_section_cache": true,
            "delta_patch": true
        },
        "polling_fallback": {
            "supported": true,
            "field": "since_version"
        }
    })
}

fn summarize_tool_payload(name: &str, payload: &Value) -> String {
    match name {
        "packet28.search" => {
            let matches = payload
                .get("match_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(files) = payload
                .get("paths")
                .and_then(Value::as_array)
                .map(|items| items.len())
            {
                format!("Packet28 search found {matches} match(es) across {files} file(s).")
            } else {
                format!("Packet28 search found {matches} match(es).")
            }
        }
        "packet28.fetch_tool_result" => {
            let artifact_id = payload
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched tool result artifact {artifact_id}.")
        }
        "packet28.fetch_context" => {
            let artifact_id = payload
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched broker context artifact {artifact_id}.")
        }
        "packet28.prepare_handoff" => {
            let ready = payload
                .get("handoff_ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reason = payload
                .get("handoff_reason")
                .and_then(Value::as_str)
                .unwrap_or("handoff prepared");
            if ready {
                format!("Packet28 prepared a handoff: {reason}")
            } else {
                format!("Packet28 did not prepare a handoff: {reason}")
            }
        }
        "packet28.read_regions" => {
            let path = payload
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let lines = payload
                .get("lines")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            format!("Packet28 read_regions returned {lines} line(s) from {path}.")
        }
        "packet28.write_state" => {
            let accepted = payload
                .get("accepted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let version = payload
                .get("context_version")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 state write accepted={accepted} context_version={version}.")
        }
        "packet28.task_status" => "Packet28 task status.".to_string(),
        "packet28.capabilities" => "Packet28 broker capabilities.".to_string(),
        _ => "Packet28 response.".to_string(),
    }
}
