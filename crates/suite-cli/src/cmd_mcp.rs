use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    load_task_events, task_brief_markdown_path, task_state_json_path, BrokerDecomposeRequest,
    BrokerEstimateContextRequest, BrokerGetContextRequest, BrokerValidatePlanRequest,
    BrokerWriteStateRequest, DaemonRequest, DaemonResponse,
};
use serde_json::{json, Map, Value};

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommands,
}

#[derive(Subcommand)]
pub enum McpCommands {
    /// Serve Packet28 as an MCP stdio server
    Serve(McpServeArgs),
}

#[derive(Args, Clone)]
pub struct McpServeArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
}

#[derive(Default)]
struct McpSessionState {
    initialized: bool,
    shutdown: bool,
    tracked_tasks: BTreeMap<String, u64>,
    framing: Option<McpMessageFraming>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpMessageFraming {
    ContentLength,
    NewlineJson,
}

const BROKER_SECTION_IDS: &[&str] = &[
    "task_objective",
    "active_decisions",
    "open_questions",
    "resolved_questions",
    "current_focus",
    "checkpoint_deltas",
    "repo_map",
    "relevant_context",
    "recommended_actions",
    "progress",
];

pub fn run(args: McpArgs) -> Result<i32> {
    match args.command {
        McpCommands::Serve(args) => run_serve(args),
    }
}

fn run_serve(args: McpServeArgs) -> Result<i32> {
    let root = crate::broker_client::resolve_root(&args.root);
    crate::broker_client::ensure_daemon(&root)?;
    serve_stdio(root)?;
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

fn read_message(reader: &mut impl BufRead) -> Result<Option<(Value, McpMessageFraming)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            let value = serde_json::from_str(trimmed)?;
            return Ok(Some((value, McpMessageFraming::NewlineJson)));
        }
        return read_header_framed_message(reader, trimmed);
    }
}

fn read_header_framed_message(
    reader: &mut impl BufRead,
    first_line: &str,
) -> Result<Option<(Value, McpMessageFraming)>> {
    let mut content_length = None::<usize>;
    parse_header_line(first_line, &mut content_length)?;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            parse_header(name, value, &mut content_length)?;
        }
    }

    let content_length =
        content_length.ok_or_else(|| anyhow!("missing Content-Length header in MCP request"))?;
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(Some((
        serde_json::from_slice(&body)?,
        McpMessageFraming::ContentLength,
    )))
}

fn parse_header_line(line: &str, content_length: &mut Option<usize>) -> Result<()> {
    let Some((name, value)) = line.split_once(':') else {
        return Err(anyhow!("missing Content-Length header in MCP request"));
    };
    parse_header(name, value, content_length)
}

fn parse_header(name: &str, value: &str, content_length: &mut Option<usize>) -> Result<()> {
    if name.eq_ignore_ascii_case("content-length") {
        *content_length = Some(value.trim().parse::<usize>()?);
    }
    Ok(())
}

fn write_message(writer: &mut impl Write, value: &Value, framing: McpMessageFraming) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    match framing {
        McpMessageFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
            writer.write_all(&body)?;
        }
        McpMessageFraming::NewlineJson => {
            writer.write_all(&body)?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
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
                    "resources": {}
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
                    "name": "packet28.get_context",
                    "description": "Get action-specific Packet28 context for a task.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id", "action"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "action": {"type":"string","enum":["plan","inspect","choose_tool","interpret","edit","summarize"]},
                            "budget_tokens": {"type":"number"},
                            "budget_bytes": {"type":"number"},
                            "since_version": {"type":"string"},
                            "focus_paths": {"type":"array","items":{"type":"string"}},
                            "focus_symbols": {"type":"array","items":{"type":"string"}},
                            "tool_name": {"type":"string"},
                            "tool_result_kind": {"type":"string","enum":["build","stack","test","diff","generic"]},
                            "query": {"type":"string"},
                            "include_sections": {"type":"array","items":{"type":"string"}},
                            "exclude_sections": {"type":"array","items":{"type":"string"}},
                            "verbosity": {"type":"string","enum":["compact","standard","rich"]},
                            "response_mode": {"type":"string","enum":["full","delta","auto"]},
                            "include_self_context": {"type":"boolean"},
                            "max_sections": {"type":"number"},
                            "default_max_items_per_section": {"type":"number"},
                            "section_item_limits": {"type":"object","additionalProperties":{"type":"number"}}
                        }
                    }
                },
                {
                    "name": "packet28.estimate_context",
                    "description": "Preview the cost and selected sections for a broker context request without fetching the full brief.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id", "action"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "action": {"type":"string","enum":["plan","inspect","choose_tool","interpret","edit","summarize"]},
                            "budget_tokens": {"type":"number"},
                            "budget_bytes": {"type":"number"},
                            "since_version": {"type":"string"},
                            "focus_paths": {"type":"array","items":{"type":"string"}},
                            "focus_symbols": {"type":"array","items":{"type":"string"}},
                            "tool_name": {"type":"string"},
                            "tool_result_kind": {"type":"string","enum":["build","stack","test","diff","generic"]},
                            "query": {"type":"string"},
                            "include_sections": {"type":"array","items":{"type":"string"}},
                            "exclude_sections": {"type":"array","items":{"type":"string"}},
                            "verbosity": {"type":"string","enum":["compact","standard","rich"]},
                            "response_mode": {"type":"string","enum":["full","delta","auto"]},
                            "include_self_context": {"type":"boolean"},
                            "max_sections": {"type":"number"},
                            "default_max_items_per_section": {"type":"number"},
                            "section_item_limits": {"type":"object","additionalProperties":{"type":"number"}}
                        }
                    }
                },
                {
                    "name": "packet28.write_state",
                    "description": "Write one structured agent-state update into Packet28.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id", "op"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "op": {"type":"string","enum":["focus_set","focus_clear","file_read","file_edit","checkpoint_save","decision_add","decision_supersede","step_complete","question_open","question_resolve"]},
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
                            "resolution_decision_id": {"type":"string"}
                        }
                    }
                },
                {
                    "name": "packet28.validate_plan",
                    "description": "Validate a structured agent plan against current repo, task, and broker state.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id", "steps"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "steps": {
                                "type":"array",
                                "items": {
                                    "type":"object",
                                    "required":["id","action"],
                                    "properties": {
                                        "id": {"type":"string"},
                                        "action": {"type":"string"},
                                        "description": {"type":"string"},
                                        "paths": {"type":"array","items":{"type":"string"}},
                                        "symbols": {"type":"array","items":{"type":"string"}},
                                        "depends_on": {"type":"array","items":{"type":"string"}}
                                    }
                                }
                            },
                            "budget_tokens": {"type":"number"},
                            "require_read_before_edit": {"type":"boolean"},
                            "require_test_gate": {"type":"boolean"}
                        }
                    }
                },
                {
                    "name": "packet28.decompose",
                    "description": "Experimental: deterministically decompose a constrained refactor intent into ordered plan steps.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id", "task_text", "intent"],
                        "properties": {
                            "task_id": {"type":"string"},
                            "task_text": {"type":"string"},
                            "intent": {"type":"string","enum":["rename","extract","split_file","merge_files","restructure_module"]},
                            "scope_paths": {"type":"array","items":{"type":"string"}},
                            "scope_symbols": {"type":"array","items":{"type":"string"}},
                            "max_steps": {"type":"number"}
                        }
                    }
                },
                {
                    "name": "packet28.task_status",
                    "description": "Return current Packet28 task status and broker artifact paths.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id"],
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
        "tools/call" => handle_tool_call(root, session, params),
        "resources/list" => handle_resources_list(root),
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
        "packet28.get_context" => {
            let request: BrokerGetContextRequest = serde_json::from_value(arguments)?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(crate::broker_client::get_context(root, request)?)?
        }
        "packet28.estimate_context" => {
            let request: BrokerEstimateContextRequest = serde_json::from_value(arguments)?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(crate::broker_client::estimate_context(root, request)?)?
        }
        "packet28.write_state" => {
            let request: BrokerWriteStateRequest = serde_json::from_value(arguments)?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(crate::broker_client::write_state(root, request)?)?
        }
        "packet28.validate_plan" => {
            let request: BrokerValidatePlanRequest = serde_json::from_value(arguments)?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(crate::broker_client::validate_plan(root, request)?)?
        }
        "packet28.decompose" => {
            let request: BrokerDecomposeRequest = serde_json::from_value(arguments)?;
            track_task(session, root, &request.task_id)?;
            serde_json::to_value(crate::broker_client::decompose(root, request)?)?
        }
        "packet28.task_status" => {
            let task_id = arguments
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("packet28.task_status requires task_id"))?;
            track_task(session, root, task_id)?;
            serde_json::to_value(crate::broker_client::task_status(root, task_id)?)?
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
        "actions": ["plan", "inspect", "choose_tool", "interpret", "edit", "summarize"],
        "section_ids": BROKER_SECTION_IDS,
        "verbosity_modes": ["compact", "standard", "rich"],
        "response_modes": ["full", "delta", "auto"],
        "tools": ["packet28.get_context", "packet28.estimate_context", "packet28.validate_plan", "packet28.decompose", "packet28.write_state", "packet28.task_status", "packet28.capabilities"],
        "tool_result_kinds": ["build", "stack", "test", "diff", "generic"],
        "push_notifications": {
            "supported": true,
            "method": "notifications/packet28.context_updated"
        },
        "filters": {
            "include_sections": true,
            "exclude_sections": true,
            "include_self_context_default": false
        },
        "section_limits": {
            "explicit_limits_supported": true,
            "deprecated_verbosity_alias": true
        },
        "estimate_context": true,
        "planning_tools": {
            "validate_plan": true,
            "decompose": true,
            "decompose_requires_intent": true,
            "decompose_experimental": true,
            "decompose_scope": "constrained_refactors_only"
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
        "packet28.get_context" => payload
            .get("brief")
            .and_then(Value::as_str)
            .filter(|brief| !brief.trim().is_empty())
            .map(|brief| brief.to_string())
            .unwrap_or_else(|| "Packet28 returned no rendered brief.".to_string()),
        "packet28.estimate_context" => {
            let est_tokens = payload
                .get("est_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let sections = payload
                .get("selected_section_ids")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            format!("Packet28 context estimate with {sections} section(s), ~{est_tokens} tokens.")
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
        "packet28.validate_plan" => {
            let valid = payload
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let violations = payload
                .get("violations")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            format!("Packet28 plan validation valid={valid} violations={violations}.")
        }
        "packet28.decompose" => {
            let steps = payload
                .get("steps")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            format!("Packet28 decomposition returned {steps} step(s).")
        }
        "packet28.task_status" => "Packet28 task status.".to_string(),
        "packet28.capabilities" => "Packet28 broker capabilities.".to_string(),
        _ => "Packet28 response.".to_string(),
    }
}

fn track_task(session: &Arc<Mutex<McpSessionState>>, root: &Path, task_id: &str) -> Result<()> {
    let latest_seq = load_task_events(root, task_id)?
        .last()
        .map(|frame| frame.seq)
        .unwrap_or(0);
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    guard
        .tracked_tasks
        .entry(task_id.to_string())
        .or_insert(latest_seq);
    Ok(())
}

fn handle_resources_list(root: &Path) -> Result<Value> {
    let status = match crate::cmd_daemon::send_request(root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => status,
        DaemonResponse::Error { message } => return Err(anyhow!(message)),
        other => return Err(anyhow!("unexpected daemon response: {other:?}")),
    };
    let mut resources = Vec::new();
    for task in status.tasks {
        resources.push(json!({
            "uri": format!("packet28://task/{}/brief", task.task_id),
            "name": format!("Packet28 brief {}", task.task_id),
            "description": "Latest broker brief",
            "mimeType": "text/markdown"
        }));
        resources.push(json!({
            "uri": format!("packet28://task/{}/events", task.task_id),
            "name": format!("Packet28 events {}", task.task_id),
            "description": "Task event replay",
            "mimeType": "application/json"
        }));
        resources.push(json!({
            "uri": format!("packet28://task/{}/state", task.task_id),
            "name": format!("Packet28 state {}", task.task_id),
            "description": "Task broker metadata",
            "mimeType": "application/json"
        }));
    }
    Ok(json!({ "resources": resources }))
}

fn handle_resource_read(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    params: Value,
) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing resource uri"))?;
    let task_id = uri
        .strip_prefix("packet28://task/")
        .and_then(|rest| rest.split('/').next())
        .filter(|task_id| !task_id.is_empty())
        .ok_or_else(|| anyhow!("invalid Packet28 resource URI"))?;
    track_task(session, root, task_id)?;
    if uri.ends_with("/brief") {
        let path = task_brief_markdown_path(root, task_id);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        return Ok(json!({
            "contents": [
                {
                    "uri": uri,
                    "mimeType": "text/markdown",
                    "text": text
                }
            ]
        }));
    }
    if uri.ends_with("/events") {
        let frames = load_task_events(root, task_id)?;
        return Ok(json!({
            "contents": [
                {
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&frames)?
                }
            ]
        }));
    }
    if uri.ends_with("/state") {
        let path = task_state_json_path(root, task_id);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        return Ok(json!({
            "contents": [
                {
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text
                }
            ]
        }));
    }
    Err(anyhow!("unsupported Packet28 resource URI '{uri}'"))
}
