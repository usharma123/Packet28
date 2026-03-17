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

#[allow(dead_code)]
#[path = "cmd_mcp_native.rs"]
mod native_tools;
#[path = "cmd_mcp_prompt_resource.rs"]
mod prompt_resource;
#[path = "cmd_mcp_proxy.rs"]
mod proxy;
#[path = "cmd_mcp_proxy_catalog.rs"]
mod proxy_catalog;
#[path = "cmd_mcp_proxy_upstream.rs"]
mod proxy_upstream;
#[allow(dead_code)]
#[path = "cmd_mcp_support.rs"]
mod support;
#[path = "cmd_mcp_transport.rs"]
mod transport;

use crate::cmd_mcp::native_tools::{
    handle_packet28_fetch_context, handle_packet28_prepare_handoff,
    handle_packet28_write_intention, Packet28FetchContextArgs, Packet28PrepareHandoffArgs,
    Packet28WriteIntentionArgs,
};
use crate::cmd_mcp::prompt_resource::{
    handle_prompt_get, handle_resource_read, handle_resources_list, prompt_descriptors,
    resolve_current_task_id,
};
use crate::cmd_mcp::proxy::{load_proxy_config, serve_proxy_stdio};
use crate::cmd_mcp::support::{
    broker_task_status_via_session, classify_error_message, extract_named_string, extract_paths,
    extract_symbols, is_retryable_error, maybe_store_result_artifact, resolve_session_task_id,
    store_tool_artifact, summarize_json_value, track_task,
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
                    "name": "packet28.fetch_context",
                    "description": "Fetch a stored Packet28 broker context by context_version or artifact_id. Use response_mode='slim' to omit heavy sections.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"},
                            "artifact_id": {"type":"string"},
                            "context_version": {"type":"string"},
                            "response_mode": {"type":"string","enum":["slim","full"]}
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
                    "name": "packet28.write_intention",
                    "description": "Persist the current task objective and worker intent into Packet28.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["text"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "text": {"type":"string"},
                            "note": {"type":"string"},
                            "step_id": {"type":"string"},
                            "question_id": {"type":"string"},
                            "paths": {"type":"array","items":{"type":"string"}},
                            "symbols": {"type":"array","items":{"type":"string"}}
                        }
                    }
                },
                {
                    "name": "packet28.task_status",
                    "description": "Return current Packet28 task status and handoff state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"}
                        }
                    }
                },
                {
                    "name": "packet28.capabilities",
                    "description": "Describe the active Packet28 hooks-first runtime contract.",
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
        "packet28.write_intention" => {
            let mut request: Packet28WriteIntentionArgs = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                Some(request.text.as_str()),
                "packet28.write_intention",
            )?;
            track_task(session, root, &request.task_id)?;
            crate::task_runtime::store_active_task(
                root,
                &packet28_daemon_core::ActiveTaskRecord {
                    task_id: request.task_id.clone(),
                    session_id: None,
                    updated_at_unix: packet28_daemon_core::now_unix(),
                },
            )?;
            handle_packet28_write_intention(root, request)?
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
    // Keep this payload minimal — it is injected into every MCP init and
    // counts against the agent's context budget.  Only include fields the
    // agent needs to *decide what to call*; omit anything derivable from
    // tool schemas or MCP protocol defaults.
    json!({
        "response_modes": ["slim", "full"],
        "hooks_first": true,
        "push_notification": "notifications/packet28.context_updated",
        "task_id_optional_after_first": true,
        "relaunch": "daemon_managed",
        "supersession": "replace"
    })
}

fn summarize_tool_payload(name: &str, payload: &Value) -> String {
    match name {
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
        "packet28.task_status" => "Packet28 task status.".to_string(),
        "packet28.capabilities" => "Packet28 broker capabilities.".to_string(),
        _ => "Packet28 response.".to_string(),
    }
}
