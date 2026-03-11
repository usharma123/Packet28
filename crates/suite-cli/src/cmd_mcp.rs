use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    load_task_events, task_artifact_dir, task_brief_markdown_path, task_state_json_path,
    BrokerAction, BrokerDecomposeRequest, BrokerEstimateContextRequest, BrokerGetContextRequest,
    BrokerResponseMode, BrokerTaskStatusRequest, BrokerTaskStatusResponse, BrokerToolResultKind,
    BrokerValidatePlanRequest, BrokerVerbosity, BrokerWriteOp, BrokerWriteStateBatchRequest,
    BrokerWriteStateBatchResponse, BrokerWriteStateRequest, BrokerWriteStateResponse,
    DaemonRequest, DaemonResponse, TaskRecord,
};
use serde::Deserialize;
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
    latest_context_versions: BTreeMap<String, String>,
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
            latest_context_versions: BTreeMap::new(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpMessageFraming {
    ContentLength,
    NewlineJson,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct McpProxyConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpProxyServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Packet28SearchResponseMode {
    #[default]
    Slim,
    Full,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Packet28SearchArgs {
    task_id: String,
    query: String,
    paths: Vec<String>,
    fixed_string: bool,
    case_sensitive: Option<bool>,
    whole_word: bool,
    context_lines: Option<usize>,
    max_matches_per_file: Option<usize>,
    max_total_matches: Option<usize>,
    response_mode: Packet28SearchResponseMode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Packet28ReadRegionsArgs {
    task_id: String,
    path: String,
    regions: Vec<String>,
    line_start: Option<usize>,
    line_end: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Packet28FetchToolResultArgs {
    task_id: String,
    artifact_id: Option<String>,
    invocation_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Packet28SyncArgs {
    task_id: String,
    task_text: Option<String>,
    action: Option<BrokerAction>,
    budget_tokens: Option<u64>,
    budget_bytes: Option<usize>,
    query: Option<String>,
    focus_paths: Vec<String>,
    focus_symbols: Vec<String>,
    tool_name: Option<String>,
    tool_result_kind: Option<BrokerToolResultKind>,
    include_sections: Vec<String>,
    exclude_sections: Vec<String>,
    verbosity: Option<BrokerVerbosity>,
    response_mode: Option<BrokerResponseMode>,
    include_self_context: bool,
    max_sections: Option<usize>,
    default_max_items_per_section: Option<usize>,
    section_item_limits: BTreeMap<String, usize>,
    persist_artifacts: Option<bool>,
    include_estimate: bool,
    writes: Vec<BrokerWriteStateRequest>,
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

struct UpstreamClient {
    name: String,
    _child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    request_timeout: Duration,
    command_preview: String,
}

const BROKER_SECTION_IDS: &[&str] = &[
    "task_objective",
    "budget_notes",
    "task_memory",
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
const DEFAULT_UPSTREAM_TIMEOUT_MS: u64 = 30_000;

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

fn load_proxy_config(path: &Path) -> Result<McpProxyConfig> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read MCP proxy config '{}'", path.display()))?;
    let config: McpProxyConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid MCP proxy config '{}'", path.display()))?;
    if config.mcp_servers.is_empty() {
        return Err(anyhow!(
            "MCP proxy config '{}' contains no upstream servers",
            path.display()
        ));
    }
    Ok(config)
}

fn serve_proxy_stdio(root: PathBuf, config: McpProxyConfig, task_id: String) -> Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let writer = Arc::new(Mutex::new(io::stdout()));
    let session = Arc::new(Mutex::new(McpSessionState::default()));
    if let Ok(mut guard) = session.lock() {
        guard.proxy_task_id = Some(task_id.clone());
    }
    track_task(&session, &root, &task_id)?;
    start_notification_thread(root.clone(), writer.clone(), session.clone());
    let mut upstreams = spawn_upstream_clients(&root, &config, writer.clone(), session.clone())?;

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
            let _ = handle_proxy_notification(&root, &session, &mut upstreams, method, params);
            continue;
        }

        let response = match handle_proxy_method(
            &root,
            &session,
            &mut upstreams,
            method,
            params,
            id.clone().unwrap(),
        ) {
            Ok(value) => value,
            Err(err) => json!({
                "jsonrpc":"2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": err.to_string()
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

fn spawn_upstream_clients(
    root: &Path,
    config: &McpProxyConfig,
    writer: Arc<Mutex<io::Stdout>>,
    session: Arc<Mutex<McpSessionState>>,
) -> Result<BTreeMap<String, UpstreamClient>> {
    let mut upstreams = BTreeMap::new();
    for (name, server) in &config.mcp_servers {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        } else {
            command.current_dir(root);
        }
        command.envs(server.env.clone());
        let command_preview = render_command_preview(&server.command, &server.args);
        let timeout = Duration::from_millis(
            server
                .timeout_ms
                .unwrap_or(DEFAULT_UPSTREAM_TIMEOUT_MS)
                .max(1),
        );
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start upstream MCP server '{name}'"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("upstream MCP server '{name}' has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("upstream MCP server '{name}' has no stdout"))?;
        let (tx, rx) = mpsc::channel();
        start_upstream_reader_thread(name.clone(), stdout, tx, writer.clone(), session.clone());
        upstreams.insert(
            name.clone(),
            UpstreamClient {
                name: name.clone(),
                _child: child,
                stdin,
                responses: rx,
                request_timeout: timeout,
                command_preview,
            },
        );
    }
    Ok(upstreams)
}

fn render_command_preview(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().map(|arg| {
            if arg.contains(' ') {
                format!("{arg:?}")
            } else {
                arg.clone()
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}

fn start_upstream_reader_thread(
    upstream_name: String,
    stdout: std::process::ChildStdout,
    responses: Sender<Value>,
    writer: Arc<Mutex<io::Stdout>>,
    session: Arc<Mutex<McpSessionState>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let Ok(Some((message, _framing))) = read_message(&mut reader) else {
                break;
            };
            if message.get("id").is_some() {
                if responses.send(message).is_err() {
                    break;
                }
                continue;
            }
            let framing = session
                .lock()
                .ok()
                .and_then(|guard| guard.framing)
                .unwrap_or(McpMessageFraming::ContentLength);
            let mut notification = message;
            if let Some(params) = notification
                .get_mut("params")
                .and_then(Value::as_object_mut)
            {
                params
                    .entry("upstream".to_string())
                    .or_insert_with(|| Value::String(upstream_name.clone()));
            }
            if let Ok(mut guard) = writer.lock() {
                let _ = write_message(&mut *guard, &notification, framing);
            }
        }
    });
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

fn handle_proxy_notification(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
    method: &str,
    params: Value,
) -> Result<()> {
    handle_notification(root, session, method, params.clone())?;
    let notification = json!({
        "jsonrpc":"2.0",
        "method": method,
        "params": params,
    });
    for upstream in upstreams.values_mut() {
        send_message_to_upstream(upstream, &notification)?;
    }
    Ok(())
}

fn handle_proxy_method(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
    method: &str,
    params: Value,
    id: Value,
) -> Result<Value> {
    match method {
        "initialize" => {
            if let Ok(mut guard) = session.lock() {
                guard.initialized = true;
                guard.upstream_tools_loaded = false;
                guard.upstream_resources_loaded = false;
                guard.upstream_resource_templates_loaded = false;
            }
            for upstream in upstreams.values_mut() {
                let request = json!({
                    "jsonrpc":"2.0",
                    "id": format!("packet28-init-{}", upstream.name),
                    "method":"initialize",
                    "params": params.clone(),
                });
                let response = send_request_to_upstream(upstream, &request)?;
                if response.get("error").is_some() {
                    return Ok(json!({
                        "jsonrpc":"2.0",
                        "id": id,
                        "error": response["error"].clone()
                    }));
                }
            }
            Ok(json!({
                "jsonrpc":"2.0",
                "id": id,
                "result": handle_method(root, session, method, params)?,
            }))
        }
        "tools/list" => {
            let mut result = handle_method(root, session, method, Value::Null)?;
            if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                tools.extend(ensure_upstream_tools_loaded(session, upstreams)?);
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/list" => {
            let mut result = handle_method(root, session, method, Value::Null)?;
            if let Some(resources) = result.get_mut("resources").and_then(Value::as_array_mut) {
                resources.extend(ensure_upstream_resources_loaded(session, upstreams)?);
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/templates/list" => {
            let mut result = handle_method(root, session, method, Value::Null)?;
            if let Some(templates) = result
                .get_mut("resourceTemplates")
                .and_then(Value::as_array_mut)
            {
                templates.extend(ensure_upstream_resource_templates_loaded(
                    session, upstreams,
                )?);
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "prompts/list" => Ok(json!({
            "jsonrpc":"2.0",
            "id":id,
            "result": handle_method(root, session, method, Value::Null)?,
        })),
        "prompts/get" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing prompt name"))?;
            if name.starts_with("packet28.") {
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "result": handle_method(root, session, method, params)?,
                }));
            }
            let upstream = upstreams
                .values_mut()
                .next()
                .ok_or_else(|| anyhow!("no upstream MCP servers configured"))?;
            send_request_to_upstream(
                upstream,
                &json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method":"prompts/get",
                    "params": params,
                }),
            )
        }
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing resource uri"))?;
            if uri.starts_with("packet28://") {
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "result": handle_method(root, session, method, params)?,
                }));
            }
            if owner_for_resource(session, uri).is_none() {
                let _ = ensure_upstream_resources_loaded(session, upstreams)?;
            }
            let owner = owner_for_resource(session, uri)
                .ok_or_else(|| anyhow!("no upstream owns resource '{uri}'"))?;
            let upstream = upstreams
                .get_mut(&owner)
                .ok_or_else(|| anyhow!("missing upstream '{owner}'"))?;
            let response = send_request_to_upstream(
                upstream,
                &json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method":"resources/read",
                    "params": params,
                }),
            )?;
            Ok(response)
        }
        "tools/call" => handle_proxy_tool_call(root, session, upstreams, params, id),
        _ => {
            let upstream = upstreams
                .values_mut()
                .next()
                .ok_or_else(|| anyhow!("no upstream MCP servers configured"))?;
            send_request_to_upstream(
                upstream,
                &json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }),
            )
        }
    }
}

fn refresh_upstream_tools(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    let native_tool_names = native_tool_names();
    let mut discovered = BTreeMap::<String, Vec<(String, Value)>>::new();
    let mut tool_owners = BTreeMap::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-tools-refresh-{}", upstream.name),
                "method":"tools/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("tools"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    discovered
                        .entry(name.to_string())
                        .or_default()
                        .push((upstream.name.clone(), item.clone()));
                }
            }
        }
    }

    let mut tool_forward_names = BTreeMap::new();
    let mut rendered_tools = Vec::new();
    for (name, entries) in discovered {
        let needs_namespace = entries.len() > 1 || native_tool_names.contains_key(&name);
        for (owner, item) in entries {
            let alias = if needs_namespace {
                namespaced_tool_name(&owner, &name)
            } else {
                name.clone()
            };
            tool_owners.insert(alias.clone(), owner.clone());
            tool_forward_names.insert(alias.clone(), name.clone());
            rendered_tools.push(annotated_tool_item(item, &alias, &owner, needs_namespace));
        }
    }
    rendered_tools.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });

    if let Ok(mut guard) = session.lock() {
        guard.tool_owners = tool_owners;
        guard.tool_forward_names = tool_forward_names;
        guard.upstream_tools_cache = rendered_tools.clone();
        guard.upstream_tools_loaded = true;
    }
    Ok(rendered_tools)
}

fn refresh_upstream_resources(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    let mut resource_owners = BTreeMap::new();
    let mut rendered_resources = Vec::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-resources-refresh-{}", upstream.name),
                "method":"resources/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("resources"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                    resource_owners.insert(uri.to_string(), upstream.name.clone());
                    rendered_resources.push(item.clone());
                }
            }
        }
    }
    rendered_resources.sort_by(|left, right| {
        left.get("uri")
            .and_then(Value::as_str)
            .cmp(&right.get("uri").and_then(Value::as_str))
    });
    if let Ok(mut guard) = session.lock() {
        guard.resource_owners = resource_owners;
        guard.upstream_resources_cache = rendered_resources.clone();
        guard.upstream_resources_loaded = true;
    }
    Ok(rendered_resources)
}

fn owner_for_tool(session: &Arc<Mutex<McpSessionState>>, tool_name: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.tool_owners.get(tool_name).cloned())
}

fn forward_name_for_tool(session: &Arc<Mutex<McpSessionState>>, tool_name: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.tool_forward_names.get(tool_name).cloned())
}

fn owner_for_resource(session: &Arc<Mutex<McpSessionState>>, uri: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.resource_owners.get(uri).cloned())
}

fn ensure_upstream_tools_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_tools_loaded {
            return Ok(guard.upstream_tools_cache.clone());
        }
    }
    refresh_upstream_tools(session, upstreams)
}

fn ensure_upstream_resources_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_resources_loaded {
            return Ok(guard.upstream_resources_cache.clone());
        }
    }
    refresh_upstream_resources(session, upstreams)
}

fn ensure_upstream_resource_templates_loaded(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<Vec<Value>> {
    if let Ok(guard) = session.lock() {
        if guard.upstream_resource_templates_loaded {
            return Ok(guard.upstream_resource_templates_cache.clone());
        }
    }
    let mut templates = Vec::new();
    for upstream in upstreams.values_mut() {
        let response = send_request_to_upstream(
            upstream,
            &json!({
                "jsonrpc":"2.0",
                "id": format!("packet28-templates-list-{}", upstream.name),
                "method":"resources/templates/list"
            }),
        )?;
        if let Some(items) = response
            .get("result")
            .and_then(|value| value.get("resourceTemplates"))
            .and_then(Value::as_array)
        {
            templates.extend(items.iter().cloned());
        }
    }
    if let Ok(mut guard) = session.lock() {
        guard.upstream_resource_templates_cache = templates.clone();
        guard.upstream_resource_templates_loaded = true;
    }
    Ok(templates)
}

fn namespaced_tool_name(owner: &str, name: &str) -> String {
    let prefix = format!("{owner}.");
    if name.starts_with(&prefix) {
        name.to_string()
    } else {
        format!("{owner}.{name}")
    }
}

fn annotated_tool_item(mut item: Value, alias: &str, owner: &str, namespaced: bool) -> Value {
    if let Some(obj) = item.as_object_mut() {
        obj.insert("name".to_string(), Value::String(alias.to_string()));
        if namespaced {
            let description = obj
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Upstream MCP tool");
            obj.insert(
                "description".to_string(),
                Value::String(format!("{description} [via {owner}]")),
            );
        }
    }
    item
}

fn native_tool_names() -> BTreeMap<String, ()> {
    [
        "packet28.get_context",
        "packet28.estimate_context",
        "packet28.sync",
        "packet28.search",
        "packet28.fetch_tool_result",
        "packet28.read_regions",
        "packet28.write_state",
        "packet28.validate_plan",
        "packet28.decompose",
        "packet28.task_status",
        "packet28.capabilities",
    ]
    .into_iter()
    .map(|name| (name.to_string(), ()))
    .collect()
}

fn next_proxy_invocation(session: &Arc<Mutex<McpSessionState>>) -> Result<(String, u64, String)> {
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    let task_id = guard
        .proxy_task_id
        .clone()
        .ok_or_else(|| anyhow!("proxy task_id is not initialized"))?;
    guard.next_invocation_seq = guard.next_invocation_seq.saturating_add(1).max(1);
    let sequence = guard.next_invocation_seq;
    Ok((task_id, sequence, format!("tool-invocation-{sequence}")))
}

fn send_message_to_upstream(upstream: &mut UpstreamClient, request: &Value) -> Result<()> {
    let body = serde_json::to_vec(request)?;
    write!(upstream.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    upstream.stdin.write_all(&body)?;
    upstream.stdin.flush()?;
    Ok(())
}

fn send_request_to_upstream(upstream: &mut UpstreamClient, request: &Value) -> Result<Value> {
    send_message_to_upstream(upstream, request)?;
    upstream
        .responses
        .recv_timeout(upstream.request_timeout)
        .map_err(|_| {
            anyhow!(
                "timed out waiting for upstream '{}' after {}ms while running `{}`",
                upstream.name,
                upstream.request_timeout.as_millis(),
                upstream.command_preview
            )
        })
}

fn handle_proxy_tool_call(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
    params: Value,
    id: Value,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    if name.starts_with("packet28.") {
        let result = handle_tool_call(root, session, params)?;
        return Ok(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }

    if owner_for_tool(session, name).is_none() {
        let _ = ensure_upstream_tools_loaded(session, upstreams)?;
    }
    let owner =
        owner_for_tool(session, name).ok_or_else(|| anyhow!("no upstream owns tool '{name}'"))?;
    let upstream_tool_name = forward_name_for_tool(session, name)
        .ok_or_else(|| anyhow!("no upstream mapping found for tool '{name}'"))?;
    let upstream = upstreams
        .get_mut(&owner)
        .ok_or_else(|| anyhow!("missing upstream '{owner}'"))?;

    let operation_kind = classify_tool_operation(name, &arguments);
    let request_summary = summarize_json_value(&arguments, 160);
    let request_fingerprint = blake3::hash(serde_json::to_string(&arguments)?.as_bytes())
        .to_hex()
        .to_string();
    let request_paths = extract_paths(root, &arguments);
    let request_symbols = extract_symbols(&arguments);
    let search_query = extract_named_string(&arguments, &["query", "q", "pattern", "search_query"]);
    let command = extract_named_string(&arguments, &["cmd", "command"]);
    let (task_id, sequence, invocation_id) = next_proxy_invocation(session)?;

    track_task(session, root, &task_id)?;
    write_auto_capture_state(
        root,
        BrokerWriteStateRequest {
            task_id: task_id.clone(),
            op: Some(BrokerWriteOp::ToolInvocationStarted),
            invocation_id: Some(invocation_id.clone()),
            tool_name: Some(name.to_string()),
            server_name: Some(owner.clone()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary.clone()),
            request_fingerprint: Some(request_fingerprint.clone()),
            sequence: Some(sequence),
            paths: request_paths.clone(),
            symbols: request_symbols.clone(),
            ..BrokerWriteStateRequest::default()
        },
    )?;

    let started_at = Instant::now();
    let response = send_request_to_upstream(
        upstream,
        &json!({
            "jsonrpc":"2.0",
            "id": id,
            "method":"tools/call",
            "params": {
                "name": upstream_tool_name,
                "arguments": arguments.clone(),
            }
        }),
    )?;
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    let empty = Value::Null;
    let response_result = response.get("result").unwrap_or(&empty);
    let response_paths = extract_paths(root, response_result);
    let response_symbols = extract_symbols(response_result);
    let mut paths = request_paths;
    paths.extend(response_paths);
    paths.sort();
    paths.dedup();
    let mut symbols = request_symbols;
    symbols.extend(response_symbols);
    symbols.sort();
    symbols.dedup();

    if let Some(error) = response.get("error") {
        let error_class = error
            .get("code")
            .and_then(Value::as_i64)
            .map(|code| format!("code:{code}"))
            .or_else(|| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(classify_error_message)
            });
        let error_message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream tool call failed")
            .to_string();
        let artifact_id =
            store_tool_artifact(root, &task_id, &invocation_id, "failure", error).ok();
        write_auto_capture_state(
            root,
            BrokerWriteStateRequest {
                task_id: task_id.clone(),
                op: Some(BrokerWriteOp::ToolInvocationFailed),
                invocation_id: Some(invocation_id.clone()),
                tool_name: Some(name.to_string()),
                server_name: Some(owner.clone()),
                operation_kind: Some(operation_kind),
                request_summary: Some(request_summary),
                request_fingerprint: Some(request_fingerprint),
                error_class,
                error_message: Some(error_message.clone()),
                retryable: Some(is_retryable_error(&error_message)),
                sequence: Some(sequence),
                duration_ms: Some(duration_ms),
                paths: paths.clone(),
                symbols: symbols.clone(),
                ..BrokerWriteStateRequest::default()
            },
        )?;
        if let Some(artifact_id) = artifact_id {
            write_auto_capture_state(
                root,
                BrokerWriteStateRequest {
                    task_id: task_id.clone(),
                    op: Some(BrokerWriteOp::EvidenceCaptured),
                    artifact_id: Some(artifact_id),
                    note: Some(format!("failure output for {}", name)),
                    ..BrokerWriteStateRequest::default()
                },
            )?;
        }
        if !paths.is_empty() || !symbols.is_empty() {
            write_auto_capture_state(
                root,
                BrokerWriteStateRequest {
                    task_id,
                    op: Some(BrokerWriteOp::FocusInferred),
                    note: Some(format!("inferred from failed {}", name)),
                    paths,
                    symbols,
                    ..BrokerWriteStateRequest::default()
                },
            )?;
        }
        return Ok(response);
    }

    let result = response.get("result").cloned().unwrap_or(Value::Null);
    let result_summary = summarize_json_value(&result, 200);
    let artifact_id = maybe_store_result_artifact(
        root,
        &task_id,
        &invocation_id,
        &result,
        !paths.is_empty() || !symbols.is_empty(),
    )?;

    write_auto_capture_state(
        root,
        BrokerWriteStateRequest {
            task_id: task_id.clone(),
            op: Some(BrokerWriteOp::ToolInvocationCompleted),
            invocation_id: Some(invocation_id.clone()),
            tool_name: Some(name.to_string()),
            server_name: Some(owner.clone()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary),
            result_summary: Some(result_summary),
            request_fingerprint: Some(request_fingerprint),
            search_query,
            command,
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            artifact_id: artifact_id.clone(),
            paths: paths.clone(),
            symbols: symbols.clone(),
            ..BrokerWriteStateRequest::default()
        },
    )?;
    if !paths.is_empty() || !symbols.is_empty() {
        write_auto_capture_state(
            root,
            BrokerWriteStateRequest {
                task_id: task_id.clone(),
                op: Some(BrokerWriteOp::FocusInferred),
                note: Some(format!("inferred from {}", name)),
                paths,
                symbols,
                ..BrokerWriteStateRequest::default()
            },
        )?;
    }
    if let Some(artifact_id) = artifact_id {
        write_auto_capture_state(
            root,
            BrokerWriteStateRequest {
                task_id,
                op: Some(BrokerWriteOp::EvidenceCaptured),
                artifact_id: Some(artifact_id),
                note: Some(format!("captured from {}", name)),
                ..BrokerWriteStateRequest::default()
            },
        )?;
    }

    Ok(response)
}

fn write_auto_capture_state(root: &Path, request: BrokerWriteStateRequest) -> Result<()> {
    crate::broker_client::write_state(root, request).map(|_| ())
}

fn write_auto_capture_state_batch_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    requests: Vec<BrokerWriteStateRequest>,
) -> Result<()> {
    broker_write_state_batch_via_session(root, session, requests).map(|_| ())
}

fn classify_tool_operation(name: &str, arguments: &Value) -> suite_packet_core::ToolOperationKind {
    let lower_name = name.to_ascii_lowercase();
    let command = extract_named_string(arguments, &["cmd", "command"])
        .unwrap_or_default()
        .to_ascii_lowercase();
    let query = extract_named_string(arguments, &["query", "q", "pattern", "search_query"])
        .unwrap_or_default()
        .to_ascii_lowercase();
    if lower_name.contains("search")
        || lower_name.contains("grep")
        || lower_name.contains("find")
        || !query.is_empty()
    {
        suite_packet_core::ToolOperationKind::Search
    } else if lower_name.contains("read")
        || lower_name.contains("open")
        || lower_name.contains("view")
        || lower_name.contains("cat")
    {
        suite_packet_core::ToolOperationKind::Read
    } else if lower_name.contains("edit")
        || lower_name.contains("write")
        || lower_name.contains("patch")
        || lower_name.contains("replace")
    {
        suite_packet_core::ToolOperationKind::Edit
    } else if lower_name.contains("test")
        || command.contains(" test")
        || command.starts_with("test ")
        || command.contains("pytest")
    {
        suite_packet_core::ToolOperationKind::Test
    } else if lower_name.contains("build")
        || command.contains("cargo build")
        || command.contains("npm run build")
    {
        suite_packet_core::ToolOperationKind::Build
    } else if lower_name.contains("diff") || command.contains("git diff") {
        suite_packet_core::ToolOperationKind::Diff
    } else if lower_name.contains("git") || command.starts_with("git ") {
        suite_packet_core::ToolOperationKind::Git
    } else if lower_name.contains("fetch")
        || lower_name.contains("http")
        || lower_name.contains("request")
    {
        suite_packet_core::ToolOperationKind::Fetch
    } else {
        suite_packet_core::ToolOperationKind::Generic
    }
}

fn summarize_json_value(value: &Value, limit: usize) -> String {
    let rendered = match value {
        Value::Null => "null".to_string(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unserializable>".to_string()),
    };
    if rendered.len() <= limit {
        rendered
    } else {
        format!("{}...", &rendered[..limit])
    }
}

fn extract_named_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key) {
                    if let Some(text) = value.as_str().filter(|text| !text.trim().is_empty()) {
                        return Some(text.to_string());
                    }
                }
            }
            map.values()
                .find_map(|child| extract_named_string(child, keys))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| extract_named_string(child, keys)),
        _ => None,
    }
}

fn extract_paths(root: &Path, value: &Value) -> Vec<String> {
    let mut paths = BTreeMap::<String, ()>::new();
    collect_named_paths(root, None, value, &mut paths);
    paths.into_keys().collect()
}

fn collect_named_paths(
    root: &Path,
    current_key: Option<&str>,
    value: &Value,
    paths: &mut BTreeMap<String, ()>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_named_paths(root, Some(key), child, paths);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_named_paths(root, current_key, child, paths);
            }
        }
        Value::String(text) => {
            let key = current_key.unwrap_or_default().to_ascii_lowercase();
            let looks_pathish = key.contains("path")
                || key.contains("file")
                || key.contains("uri")
                || text.contains('/')
                || text.ends_with(".rs")
                || text.ends_with(".ts")
                || text.ends_with(".tsx")
                || text.ends_with(".js")
                || text.ends_with(".jsx")
                || text.ends_with(".json")
                || text.ends_with(".md")
                || text.ends_with(".py")
                || text.ends_with(".java");
            if looks_pathish {
                let normalized = normalize_capture_path(root, text);
                if !normalized.is_empty() {
                    paths.insert(normalized, ());
                }
            }
        }
        _ => {}
    }
}

fn normalize_capture_path(root: &Path, text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return String::new();
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(root) {
            return stripped.to_string_lossy().to_string();
        }
    }
    trimmed
        .trim_start_matches("./")
        .trim_start_matches('/')
        .replace('\\', "/")
}

fn extract_symbols(value: &Value) -> Vec<String> {
    let mut symbols = BTreeMap::<String, ()>::new();
    collect_symbols(None, value, &mut symbols);
    symbols.into_keys().collect()
}

fn collect_symbols(current_key: Option<&str>, value: &Value, symbols: &mut BTreeMap<String, ()>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_symbols(Some(key), child, symbols);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_symbols(current_key, child, symbols);
            }
        }
        Value::String(text) => {
            let key = current_key.unwrap_or_default().to_ascii_lowercase();
            if key.contains("symbol") || key.contains("function") || key.contains("method") {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    symbols.insert(trimmed.to_string(), ());
                }
            }
        }
        _ => {}
    }
}

fn classify_error_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timeout") {
        "timeout".to_string()
    } else if lower.contains("not found") {
        "not_found".to_string()
    } else if lower.contains("permission") || lower.contains("denied") {
        "permission".to_string()
    } else {
        "generic".to_string()
    }
}

fn is_retryable_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("temporar")
        || lower.contains("unavailable")
        || lower.contains("try again")
}

fn maybe_store_result_artifact(
    root: &Path,
    task_id: &str,
    invocation_id: &str,
    result: &Value,
    material_scope_change: bool,
) -> Result<Option<String>> {
    let bytes = serde_json::to_vec(result)?;
    if !material_scope_change && bytes.len() < 1536 {
        return Ok(None);
    }
    Ok(Some(store_tool_artifact(
        root,
        task_id,
        invocation_id,
        "result",
        result,
    )?))
}

fn store_result_artifact(
    root: &Path,
    task_id: &str,
    invocation_id: &str,
    result: &Value,
) -> Result<String> {
    store_tool_artifact(root, task_id, invocation_id, "result", result)
}

fn store_tool_artifact(
    root: &Path,
    task_id: &str,
    invocation_id: &str,
    suffix: &str,
    payload: &Value,
) -> Result<String> {
    let dir = task_artifact_dir(root, task_id).join("tool-evidence");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create tool evidence dir '{}'", dir.display()))?;
    let artifact_id = format!("{invocation_id}-{suffix}.json");
    let path = dir.join(&artifact_id);
    fs::write(&path, serde_json::to_vec_pretty(payload)?)
        .with_context(|| format!("failed to write tool evidence '{}'", path.display()))?;
    Ok(artifact_id)
}

fn load_tool_result_artifact(
    root: &Path,
    task_id: &str,
    artifact_id: Option<&str>,
    invocation_id: Option<&str>,
) -> Result<(String, Value)> {
    let selected_artifact_id = match (artifact_id, invocation_id) {
        (Some(artifact_id), _) if !artifact_id.trim().is_empty() => artifact_id.trim().to_string(),
        (None, Some(invocation_id)) if !invocation_id.trim().is_empty() => {
            format!("{}-result.json", invocation_id.trim())
        }
        _ => {
            return Err(anyhow!(
                "packet28.fetch_tool_result requires artifact_id or invocation_id"
            ));
        }
    };
    let path = task_artifact_dir(root, task_id)
        .join("tool-evidence")
        .join(&selected_artifact_id);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read tool evidence '{}'", path.display()))?;
    let value = serde_json::from_str(&text)
        .with_context(|| format!("invalid tool evidence JSON '{}'", path.display()))?;
    Ok((selected_artifact_id, value))
}

fn prompt_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "packet28.start_task",
            "description": "Start a new Packet28-scoped task with the recommended broker flow.",
            "arguments": [
                {
                    "name": "task",
                    "description": "Natural-language task description to start.",
                    "required": true
                },
                {
                    "name": "task_id",
                    "description": "Optional explicit Packet28 task identifier.",
                    "required": false
                }
            ]
        }),
        json!({
            "name": "packet28.continue_task",
            "description": "Continue the current or a specific Packet28 task with the latest known context.",
            "arguments": [
                {
                    "name": "task_id",
                    "description": "Optional Packet28 task identifier. Defaults to the current task.",
                    "required": false
                }
            ]
        }),
        json!({
            "name": "packet28.summarize_current_context",
            "description": "Summarize the latest persisted Packet28 brief for the current or specified task.",
            "arguments": [
                {
                    "name": "task_id",
                    "description": "Optional Packet28 task identifier. Defaults to the current task.",
                    "required": false
                }
            ]
        }),
    ]
}

fn handle_prompt_get(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    params: Value,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing prompt name"))?;
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match name {
        "packet28.start_task" => {
            let task = prompt_argument(&arguments, "task")
                .ok_or_else(|| anyhow!("packet28.start_task requires task"))?;
            let task_id = prompt_argument(&arguments, "task_id")
                .unwrap_or_else(|| crate::broker_client::derive_task_id(&task));
            let prompt = format!(
                "Start Packet28 task `{task_id}` for: {task}\n\n\
Use Packet28 as the primary context broker for this task.\n\
- If the next step is cheap or you are budget-constrained, call `packet28.estimate_context` first.\n\
- Then call `packet28.get_context` with `task_id=\"{task_id}\"`, `action=\"plan\"`, `query={task:?}`, and `response_mode=\"auto\"`.\n\
- Keep one mutable Packet28 context block and replace older briefs when a newer brief supersedes them.\n\
- If Packet28 is fronting upstream MCP tools via proxy, prefer those proxied tools so activity is auto-captured into the next brief.\n\
- After important reads, edits, decisions, or checkpoints, call `packet28.write_state`.\n\
- If Packet28 is unavailable, fall back to direct reads and commands."
            );
            Ok(prompt_response("Start a new Packet28 task.", prompt))
        }
        "packet28.continue_task" => {
            let task_id = resolve_requested_or_current_task_id(
                root,
                session,
                prompt_argument(&arguments, "task_id").as_deref(),
            )?;
            let status = broker_task_status_via_session(root, session, &task_id)?;
            let brief_excerpt = read_brief_excerpt(root, &task_id)
                .unwrap_or_else(|| "No persisted brief is available yet.".to_string());
            let prompt = format!(
                "Continue Packet28 task `{task_id}`.\n\n\
Latest known status:\n\
- context version: {}\n\
- reason: {}\n\
- supports push notifications: {}\n\n\
Recommended flow:\n\
- Read `packet28://current/brief` or `packet28://task/{task_id}/brief` to review the latest rendered brief.\n\
- Call `packet28.get_context` with `task_id=\"{task_id}\"`, the action that matches your next step, `since_version` set to the latest context version when available, and `response_mode=\"auto\"`.\n\
- If you use Packet28 proxy mode, prefer proxied upstream tools so tool activity is captured automatically.\n\n\
Latest brief excerpt:\n{}",
                status
                    .latest_context_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                status
                    .latest_context_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                status.supports_push,
                brief_excerpt,
            );
            Ok(prompt_response(
                "Continue the current Packet28 task.",
                prompt,
            ))
        }
        "packet28.summarize_current_context" => {
            let task_id = resolve_requested_or_current_task_id(
                root,
                session,
                prompt_argument(&arguments, "task_id").as_deref(),
            )?;
            let brief = read_brief_excerpt(root, &task_id).unwrap_or_else(|| {
                "No persisted brief is available yet. Call `packet28.get_context` first."
                    .to_string()
            });
            let prompt = format!(
                "Summarize the current Packet28 context for task `{task_id}`. Focus on active decisions, discovered scope, recent tool activity, and the next recommended actions.\n\n\
Current brief:\n{brief}"
            );
            Ok(prompt_response(
                "Summarize the latest Packet28 context.",
                prompt,
            ))
        }
        _ => Err(anyhow!("unsupported prompt '{name}'")),
    }
}

fn prompt_response(description: &str, text: String) -> Value {
    json!({
        "description": description,
        "messages": [
            {
                "role": "user",
                "content": {
                    "type": "text",
                    "text": text
                }
            }
        ]
    })
}

fn prompt_argument(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_requested_or_current_task_id(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    requested_task_id: Option<&str>,
) -> Result<String> {
    if let Some(task_id) = requested_task_id.filter(|value| !value.trim().is_empty()) {
        track_task(session, root, task_id)?;
        return Ok(task_id.trim().to_string());
    }
    resolve_current_task_id(root, session)
}

fn resolve_current_task_id(root: &Path, session: &Arc<Mutex<McpSessionState>>) -> Result<String> {
    if let Ok(guard) = session.lock() {
        if let Some(task_id) = guard.current_task_id.clone() {
            return Ok(task_id);
        }
    }
    let status = daemon_status(root)?;
    let current = select_current_task(&status.tasks)
        .map(|task| task.task_id.clone())
        .ok_or_else(|| anyhow!("no Packet28 task is available for current-task resources"))?;
    track_task(session, root, &current)?;
    Ok(current)
}

fn daemon_status(root: &Path) -> Result<packet28_daemon_core::DaemonStatus> {
    match crate::cmd_daemon::send_request(root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => Ok(status),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn select_current_task(tasks: &[TaskRecord]) -> Option<&TaskRecord> {
    tasks.iter().max_by_key(|task| task_recency_key(task))
}

fn task_recency_key(task: &TaskRecord) -> (u8, u64, u64, u64, u64, u64) {
    (
        u8::from(task.running),
        task.last_context_refresh_at_unix.unwrap_or(0),
        task.latest_brief_generated_at_unix.unwrap_or(0),
        task.last_completed_at_unix.unwrap_or(0),
        task.last_started_at_unix.unwrap_or(0),
        task.last_event_seq,
    )
}

fn read_brief_excerpt(root: &Path, task_id: &str) -> Option<String> {
    let path = task_brief_markdown_path(root, task_id);
    let text = fs::read_to_string(path).ok()?;
    Some(truncate_for_prompt(&text, 4_000))
}

fn truncate_for_prompt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_string()
    } else {
        format!("{}...", &text[..limit])
    }
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
                    "name": "packet28.get_context",
                    "description": "Get action-specific Packet28 context for a task.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["action"],
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
                            "section_item_limits": {"type":"object","additionalProperties":{"type":"number"}},
                            "persist_artifacts": {"type":"boolean"}
                        }
                    }
                },
                {
                    "name": "packet28.estimate_context",
                    "description": "Preview the cost and selected sections for a broker context request without fetching the full brief.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["action"],
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
                            "section_item_limits": {"type":"object","additionalProperties":{"type":"number"}},
                            "persist_artifacts": {"type":"boolean"}
                        }
                    }
                },
                {
                    "name": "packet28.search",
                    "description": "Search repository files under the Packet28 root with reducer-backed grouped results and auto-capture the result into broker state. Returns a slim payload by default and can fetch full details later by artifact or invocation id.",
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
                    "name": "packet28.sync",
                    "description": "High-level Packet28 turn sync: resolve the current task, apply optional state writes, auto-use session since_version, optionally estimate, then fetch broker context.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type":"string"},
                            "task_text": {"type":"string"},
                            "action": {"type":"string","enum":["plan","inspect","choose_tool","interpret","edit","summarize"]},
                            "budget_tokens": {"type":"number"},
                            "budget_bytes": {"type":"number"},
                            "query": {"type":"string"},
                            "focus_paths": {"type":"array","items":{"type":"string"}},
                            "focus_symbols": {"type":"array","items":{"type":"string"}},
                            "tool_name": {"type":"string"},
                            "tool_result_kind": {"type":"string","enum":["build","stack","test","diff","generic"]},
                            "include_sections": {"type":"array","items":{"type":"string"}},
                            "exclude_sections": {"type":"array","items":{"type":"string"}},
                            "verbosity": {"type":"string","enum":["compact","standard","rich"]},
                            "response_mode": {"type":"string","enum":["full","delta","auto"]},
                            "include_self_context": {"type":"boolean"},
                            "max_sections": {"type":"number"},
                            "default_max_items_per_section": {"type":"number"},
                            "section_item_limits": {"type":"object","additionalProperties":{"type":"number"}},
                            "persist_artifacts": {"type":"boolean"},
                            "include_estimate": {"type":"boolean"},
                            "writes": {"type":"array","items":{"type":"object"}}
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
                            "op": {"type":"string","enum":["focus_set","focus_clear","file_read","file_edit","checkpoint_save","decision_add","decision_supersede","step_complete","question_open","question_resolve","tool_invocation_started","tool_invocation_completed","tool_invocation_failed","tool_result","focus_inferred","evidence_captured"]},
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
                    "name": "packet28.validate_plan",
                    "description": "Validate a structured agent plan against current repo, task, and broker state.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["steps"],
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
                        "required": ["task_text", "intent"],
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
        "packet28.get_context" => {
            let mut request: BrokerGetContextRequest = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.query.as_deref(),
                "packet28.get_context",
            )?;
            if request.persist_artifacts.is_none() {
                request.persist_artifacts = Some(false);
            }
            let task_id = request.task_id.clone();
            let mut payload =
                serde_json::to_value(broker_get_context_via_session(root, session, request)?)?;
            if payload.get("task_id").is_none() {
                payload["task_id"] = json!(task_id);
            }
            payload
        }
        "packet28.estimate_context" => {
            let mut request: BrokerEstimateContextRequest = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                request.query.as_deref(),
                "packet28.estimate_context",
            )?;
            let task_id = request.task_id.clone();
            let mut payload =
                serde_json::to_value(broker_estimate_context_via_session(root, session, request)?)?;
            if payload.get("task_id").is_none() {
                payload["task_id"] = json!(task_id);
            }
            payload
        }
        "packet28.search" => {
            let mut request: Packet28SearchArgs = serde_json::from_value(arguments)?;
            request.task_id =
                resolve_session_task_id(session, root, &request.task_id, None, "packet28.search")?;
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
            handle_packet28_fetch_tool_result(root, request)?
        }
        "packet28.sync" => {
            let request: Packet28SyncArgs = serde_json::from_value(arguments)?;
            handle_packet28_sync(root, session, request)?
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
            serde_json::to_value(broker_write_state_via_session(root, session, request)?)?
        }
        "packet28.validate_plan" => {
            let mut request: BrokerValidatePlanRequest = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                None,
                "packet28.validate_plan",
            )?;
            serde_json::to_value(broker_validate_plan_via_session(root, session, request)?)?
        }
        "packet28.decompose" => {
            let mut request: BrokerDecomposeRequest = serde_json::from_value(arguments)?;
            request.task_id = resolve_session_task_id(
                session,
                root,
                &request.task_id,
                Some(&request.task_text),
                "packet28.decompose",
            )?;
            serde_json::to_value(broker_decompose_via_session(root, session, request)?)?
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
        "actions": ["plan", "inspect", "choose_tool", "interpret", "edit", "summarize"],
        "section_ids": BROKER_SECTION_IDS,
        "verbosity_modes": ["compact", "standard", "rich"],
        "response_modes": ["full", "delta", "auto"],
        "tools": ["packet28.get_context", "packet28.estimate_context", "packet28.search", "packet28.fetch_tool_result", "packet28.sync", "packet28.read_regions", "packet28.validate_plan", "packet28.decompose", "packet28.write_state", "packet28.task_status", "packet28.capabilities"],
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
            "detail_fetch_tool": "packet28.fetch_tool_result"
        },
        "sync": {
            "supported": true,
            "tool": "packet28.sync",
            "manages_since_version": true,
            "supports_write_batch": true,
            "supports_estimate": true
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
        "packet28.search" => {
            let matches = payload
                .get("match_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let files = payload
                .get("paths")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            format!("Packet28 search found {matches} match(es) across {files} file(s).")
        }
        "packet28.fetch_tool_result" => {
            let artifact_id = payload
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Packet28 fetched tool result artifact {artifact_id}.")
        }
        "packet28.sync" => payload
            .get("context")
            .and_then(|context| context.get("brief"))
            .and_then(Value::as_str)
            .filter(|brief| !brief.trim().is_empty())
            .map(|brief| brief.to_string())
            .unwrap_or_else(|| "Packet28 sync completed.".to_string()),
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
    guard.current_task_id = Some(task_id.to_string());
    Ok(())
}

fn session_current_task_id(session: &Arc<Mutex<McpSessionState>>) -> Option<String> {
    session.lock().ok().and_then(|guard| {
        guard
            .current_task_id
            .clone()
            .or_else(|| guard.proxy_task_id.clone())
    })
}

fn session_context_version(session: &Arc<Mutex<McpSessionState>>, task_id: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.latest_context_versions.get(task_id).cloned())
}

fn remember_task_context_version(
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
    context_version: &str,
) -> Result<()> {
    if context_version.trim().is_empty() {
        return Ok(());
    }
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    guard.current_task_id = Some(task_id.to_string());
    guard
        .latest_context_versions
        .insert(task_id.to_string(), context_version.to_string());
    Ok(())
}

fn resolve_session_task_id(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    explicit_task_id: &str,
    derive_hint: Option<&str>,
    tool_name: &str,
) -> Result<String> {
    let task_id = if !explicit_task_id.trim().is_empty() {
        explicit_task_id.trim().to_string()
    } else if let Some(task_id) = session_current_task_id(session) {
        task_id
    } else if let Ok(task_id) = resolve_current_task_id(root, session) {
        task_id
    } else if let Some(hint) = derive_hint.filter(|hint| !hint.trim().is_empty()) {
        crate::broker_client::derive_task_id(hint)
    } else {
        return Err(anyhow!(
            "{tool_name} requires task_id or an active Packet28 session task"
        ));
    };
    track_task(session, root, &task_id)?;
    Ok(task_id)
}

#[cfg(unix)]
fn send_daemon_request_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: &DaemonRequest,
) -> Result<DaemonResponse> {
    crate::cmd_daemon::ensure_daemon(root)?;
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    if guard.daemon_client.is_none() {
        guard.daemon_client = Some(crate::cmd_daemon::PersistentDaemonClient::connect(root)?);
    }
    let first_attempt = guard
        .daemon_client
        .as_mut()
        .ok_or_else(|| anyhow!("failed to initialize persistent daemon client"))?
        .send_request(request);
    match first_attempt {
        Ok(response) => Ok(response),
        Err(_) => {
            guard.daemon_client = Some(crate::cmd_daemon::PersistentDaemonClient::connect(root)?);
            guard
                .daemon_client
                .as_mut()
                .ok_or_else(|| anyhow!("failed to reinitialize persistent daemon client"))?
                .send_request(request)
        }
    }
}

#[cfg(not(unix))]
fn send_daemon_request_via_session(
    root: &Path,
    _session: &Arc<Mutex<McpSessionState>>,
    request: &DaemonRequest,
) -> Result<DaemonResponse> {
    crate::cmd_daemon::send_request(root, request)
}

fn broker_get_context_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    mut request: BrokerGetContextRequest,
) -> Result<packet28_daemon_core::BrokerGetContextResponse> {
    if request.task_id.trim().is_empty() {
        request.task_id = resolve_session_task_id(
            session,
            root,
            "",
            request.query.as_deref(),
            "packet28.get_context",
        )?;
    }
    if request.action.is_none() {
        request.action = Some(packet28_daemon_core::BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(crate::broker_client::DEFAULT_BROKER_BUDGET_TOKENS);
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(crate::broker_client::DEFAULT_BROKER_BUDGET_BYTES);
    }
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerGetContext {
            request: request.clone(),
        },
    )? {
        DaemonResponse::BrokerGetContext { response } => {
            remember_task_context_version(session, &request.task_id, &response.context_version)?;
            Ok(response)
        }
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_estimate_context_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    mut request: BrokerEstimateContextRequest,
) -> Result<packet28_daemon_core::BrokerEstimateContextResponse> {
    if request.task_id.trim().is_empty() {
        request.task_id = resolve_session_task_id(
            session,
            root,
            "",
            request.query.as_deref(),
            "packet28.estimate_context",
        )?;
    }
    if request.action.is_none() {
        request.action = Some(packet28_daemon_core::BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(crate::broker_client::DEFAULT_BROKER_BUDGET_TOKENS);
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(crate::broker_client::DEFAULT_BROKER_BUDGET_BYTES);
    }
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerEstimateContext {
            request: request.clone(),
        },
    )? {
        DaemonResponse::BrokerEstimateContext { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_write_state_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: BrokerWriteStateRequest,
) -> Result<BrokerWriteStateResponse> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerWriteState { request },
    )? {
        DaemonResponse::BrokerWriteState { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_write_state_batch_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    requests: Vec<BrokerWriteStateRequest>,
) -> Result<BrokerWriteStateBatchResponse> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerWriteStateBatch {
            request: BrokerWriteStateBatchRequest { requests },
        },
    )? {
        DaemonResponse::BrokerWriteStateBatch { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_validate_plan_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: BrokerValidatePlanRequest,
) -> Result<packet28_daemon_core::BrokerValidatePlanResponse> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerValidatePlan { request },
    )? {
        DaemonResponse::BrokerValidatePlan { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_decompose_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: BrokerDecomposeRequest,
) -> Result<packet28_daemon_core::BrokerDecomposeResponse> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerDecompose { request },
    )? {
        DaemonResponse::BrokerDecompose { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn broker_task_status_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
) -> Result<BrokerTaskStatusResponse> {
    let task_id = resolve_session_task_id(session, root, task_id, None, "packet28.task_status")?;
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerTaskStatus {
            request: BrokerTaskStatusRequest { task_id },
        },
    )? {
        DaemonResponse::BrokerTaskStatus { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn next_task_invocation(
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
) -> Result<(u64, String)> {
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    guard.next_invocation_seq = guard.next_invocation_seq.saturating_add(1).max(1);
    let sequence = guard.next_invocation_seq;
    let _ = task_id;
    Ok((sequence, format!("tool-invocation-{sequence}")))
}

fn json_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
        .collect()
}

fn search_request_summary(args: &Packet28SearchArgs) -> String {
    if args.paths.is_empty() {
        format!(
            "search '{}' across repo ({:?})",
            args.query, args.response_mode
        )
    } else {
        format!(
            "search '{}' in {} path(s) ({:?})",
            args.query,
            args.paths.len(),
            args.response_mode
        )
    }
}

fn read_regions_request_summary(args: &Packet28ReadRegionsArgs, path: &str) -> String {
    if !args.regions.is_empty() {
        format!(
            "read_regions {path} using {} region hint(s)",
            args.regions.len()
        )
    } else if args.line_start.is_some() || args.line_end.is_some() {
        format!(
            "read_regions {path} lines {}-{}",
            args.line_start.unwrap_or(1),
            args.line_end.unwrap_or(args.line_start.unwrap_or(1))
        )
    } else {
        format!("read_regions {path}")
    }
}

fn write_native_tool_result(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
    invocation_id: &str,
    sequence: u64,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    request_summary: String,
    result_summary: String,
    search_query: Option<String>,
    command: Option<String>,
    paths: Vec<String>,
    regions: Vec<String>,
    symbols: Vec<String>,
    artifact_id: Option<String>,
    duration_ms: u64,
) -> Result<()> {
    write_auto_capture_state_batch_via_session(
        root,
        session,
        vec![BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolResult),
            invocation_id: Some(invocation_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary),
            result_summary: Some(result_summary),
            search_query,
            command,
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            paths,
            regions,
            symbols,
            artifact_id,
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        }],
    )
}

fn write_native_tool_failure(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
    invocation_id: &str,
    sequence: u64,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
    request_summary: String,
    error_message: String,
    command: Option<String>,
    duration_ms: u64,
) -> Result<()> {
    write_auto_capture_state_batch_via_session(
        root,
        session,
        vec![BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolInvocationFailed),
            invocation_id: Some(invocation_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            operation_kind: Some(operation_kind),
            request_summary: Some(request_summary),
            error_class: Some(classify_error_message(&error_message)),
            error_message: Some(error_message.clone()),
            retryable: Some(is_retryable_error(&error_message)),
            sequence: Some(sequence),
            duration_ms: Some(duration_ms),
            command,
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        }],
    )
}

fn handle_packet28_search(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28SearchArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.search requires task_id"));
    }
    let query = args.query.trim();
    if query.is_empty() {
        return Err(anyhow!("packet28.search requires query"));
    }
    let (sequence, invocation_id) = next_task_invocation(session, task_id)?;
    let request_summary = search_request_summary(&args);

    let started_at = Instant::now();
    let search_result = match packet28_reducer_core::search(
        root,
        &packet28_reducer_core::SearchRequest {
            query: query.to_string(),
            requested_paths: args.paths.clone(),
            fixed_string: args.fixed_string,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
            context_lines: args.context_lines,
            max_matches_per_file: args.max_matches_per_file,
            max_total_matches: args.max_total_matches,
        },
    ) {
        Ok(result) => result,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            write_native_tool_failure(
                root,
                session,
                task_id,
                &invocation_id,
                sequence,
                "packet28.search",
                suite_packet_core::ToolOperationKind::Search,
                request_summary,
                error.to_string(),
                None,
                duration_ms,
            )?;
            return Err(error);
        }
    };
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let groups = search_result
        .groups
        .iter()
        .map(|group| {
            json!({
                "path": group.path,
                "match_count": group.match_count,
                "displayed_match_count": group.displayed_match_count,
                "truncated": group.truncated,
                "matches": group.matches,
            })
        })
        .collect::<Vec<_>>();
    let result_summary = if search_result.match_count == 0 {
        if !args.paths.is_empty() && search_result.resolved_paths.is_empty() {
            format!(
                "No search paths resolved for '{}' ({} requested path(s) missing)",
                query,
                args.paths.len()
            )
        } else if search_result.resolved_paths.is_empty() {
            format!("No matches for '{}' across repo", query)
        } else {
            format!(
                "No matches for '{}' in {} path(s)",
                query,
                search_result.resolved_paths.len()
            )
        }
    } else {
        search_result
            .compact_preview
            .lines()
            .next()
            .unwrap_or("Search completed")
            .to_string()
    };
    let requested_paths = search_result.requested_paths.clone();
    let resolved_paths = search_result.resolved_paths.clone();
    let paths = search_result.paths.clone();
    let regions = search_result.regions.clone();
    let symbols = search_result.symbols.clone();
    let compact_preview = search_result.compact_preview.clone();
    let diagnostics = search_result.diagnostics.clone();
    let full_payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "query": query,
        "match_count": search_result.match_count,
        "returned_match_count": search_result.returned_match_count,
        "truncated": search_result.truncated,
        "requested_paths": requested_paths,
        "resolved_paths": resolved_paths,
        "paths": paths.clone(),
        "regions": regions.clone(),
        "symbols": symbols.clone(),
        "groups": groups,
        "compact_preview": compact_preview,
        "diagnostics": diagnostics,
        "response_mode": "full",
    });
    let artifact_id = Some(store_result_artifact(
        root,
        task_id,
        full_payload["invocation_id"].as_str().unwrap_or_default(),
        &full_payload,
    )?);
    let payload = match args.response_mode {
        Packet28SearchResponseMode::Full => {
            let mut payload = full_payload;
            payload["artifact_id"] = json!(artifact_id.clone());
            payload
        }
        Packet28SearchResponseMode::Slim => json!({
            "task_id": task_id,
            "invocation_id": invocation_id,
            "sequence": sequence,
            "query": query,
            "match_count": search_result.match_count,
            "returned_match_count": search_result.returned_match_count,
            "truncated": search_result.truncated,
            "paths": full_payload["paths"].clone(),
            "regions": full_payload["regions"].clone(),
            "compact_preview": full_payload["compact_preview"].clone(),
            "diagnostics": full_payload["diagnostics"].clone(),
            "artifact_id": artifact_id.clone(),
            "response_mode": "slim",
        }),
    };
    write_native_tool_result(
        root,
        session,
        task_id,
        payload["invocation_id"].as_str().unwrap_or_default(),
        sequence,
        "packet28.search",
        suite_packet_core::ToolOperationKind::Search,
        request_summary,
        result_summary,
        Some(query.to_string()),
        None,
        search_result.paths.clone(),
        search_result.regions.clone(),
        search_result.symbols.clone(),
        artifact_id,
        duration_ms,
    )?;
    Ok(payload)
}

fn handle_packet28_fetch_tool_result(
    root: &Path,
    args: Packet28FetchToolResultArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.fetch_tool_result requires task_id"));
    }
    let (artifact_id, mut payload) = load_tool_result_artifact(
        root,
        task_id,
        args.artifact_id.as_deref(),
        args.invocation_id.as_deref(),
    )?;
    if payload.get("artifact_id").is_none() {
        payload["artifact_id"] = json!(artifact_id.clone());
    }
    if payload.get("response_mode").is_none() {
        payload["response_mode"] = json!("full");
    }
    Ok(payload)
}

fn handle_packet28_sync(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28SyncArgs,
) -> Result<Value> {
    let task_id = resolve_session_task_id(
        session,
        root,
        &args.task_id,
        args.task_text.as_deref().or(args.query.as_deref()),
        "packet28.sync",
    )?;
    let used_current_task = args.task_id.trim().is_empty();
    let used_since_version = session_context_version(session, &task_id);

    let write_responses = if args.writes.is_empty() {
        None
    } else {
        let requests = args
            .writes
            .into_iter()
            .map(|mut request| {
                request.task_id = task_id.clone();
                request
            })
            .collect::<Vec<_>>();
        Some(broker_write_state_batch_via_session(
            root, session, requests,
        )?)
    };

    let estimate = if args.include_estimate {
        Some(broker_estimate_context_via_session(
            root,
            session,
            packet28_daemon_core::BrokerEstimateContextRequest {
                task_id: task_id.clone(),
                action: args.action,
                budget_tokens: args.budget_tokens,
                budget_bytes: args.budget_bytes,
                since_version: used_since_version.clone(),
                focus_paths: args.focus_paths.clone(),
                focus_symbols: args.focus_symbols.clone(),
                tool_name: args.tool_name.clone(),
                tool_result_kind: args.tool_result_kind,
                query: args.query.clone(),
                include_sections: args.include_sections.clone(),
                exclude_sections: args.exclude_sections.clone(),
                verbosity: args.verbosity,
                response_mode: args.response_mode,
                include_self_context: args.include_self_context,
                max_sections: args.max_sections,
                default_max_items_per_section: args.default_max_items_per_section,
                section_item_limits: args.section_item_limits.clone(),
                persist_artifacts: args.persist_artifacts,
            },
        )?)
    } else {
        None
    };

    let context = broker_get_context_via_session(
        root,
        session,
        BrokerGetContextRequest {
            task_id: task_id.clone(),
            action: args.action,
            budget_tokens: args.budget_tokens,
            budget_bytes: args.budget_bytes,
            since_version: used_since_version.clone(),
            focus_paths: args.focus_paths,
            focus_symbols: args.focus_symbols,
            tool_name: args.tool_name,
            tool_result_kind: args.tool_result_kind,
            query: args.query,
            include_sections: args.include_sections,
            exclude_sections: args.exclude_sections,
            verbosity: args.verbosity,
            response_mode: Some(args.response_mode.unwrap_or(BrokerResponseMode::Auto)),
            include_self_context: args.include_self_context,
            max_sections: args.max_sections,
            default_max_items_per_section: args.default_max_items_per_section,
            section_item_limits: args.section_item_limits,
            persist_artifacts: args.persist_artifacts.or(Some(false)),
        },
    )?;
    let mut context_payload = serde_json::to_value(context)?;
    if context_payload.get("task_id").is_none() {
        context_payload["task_id"] = json!(task_id.clone());
    }

    Ok(json!({
        "task_id": task_id,
        "current_task_id": session_current_task_id(session),
        "used_current_task": used_current_task,
        "used_since_version": used_since_version,
        "writes_applied": write_responses.as_ref().map(|batch| batch.responses.len()).unwrap_or(0),
        "write_responses": write_responses,
        "estimate": estimate,
        "context": context_payload,
    }))
}

fn handle_packet28_read_regions(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    args: Packet28ReadRegionsArgs,
) -> Result<Value> {
    let task_id = args.task_id.trim();
    if task_id.is_empty() {
        return Err(anyhow!("packet28.read_regions requires task_id"));
    }
    let (sequence, invocation_id) = next_task_invocation(session, task_id)?;
    let request_summary = read_regions_request_summary(&args, &args.path);

    let started_at = Instant::now();
    let read_result = match packet28_reducer_core::read_regions(
        root,
        &packet28_reducer_core::ReadRegionsRequest {
            path: args.path.clone(),
            regions: args.regions.clone(),
            line_start: args.line_start,
            line_end: args.line_end,
        },
    ) {
        Ok(result) => result,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            write_native_tool_failure(
                root,
                session,
                task_id,
                &invocation_id,
                sequence,
                "packet28.read_regions",
                suite_packet_core::ToolOperationKind::Read,
                request_summary,
                error.to_string(),
                None,
                duration_ms,
            )?;
            return Err(error);
        }
    };
    let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let result_summary = format!(
        "Read {} line(s) from {} across {} region(s)",
        read_result.lines.len(),
        read_result.path,
        read_result.regions.len()
    );
    let payload = json!({
        "task_id": task_id,
        "invocation_id": invocation_id,
        "sequence": sequence,
        "path": read_result.path,
        "regions": read_result.regions,
        "symbols": read_result.symbols,
        "lines": read_result.lines,
        "compact_preview": read_result.compact_preview,
    });
    let artifact_id = maybe_store_result_artifact(
        root,
        task_id,
        payload["invocation_id"].as_str().unwrap_or_default(),
        &payload,
        false,
    )?;
    write_native_tool_result(
        root,
        session,
        task_id,
        payload["invocation_id"].as_str().unwrap_or_default(),
        sequence,
        "packet28.read_regions",
        suite_packet_core::ToolOperationKind::Read,
        request_summary,
        result_summary,
        None,
        None,
        vec![payload["path"].as_str().unwrap_or_default().to_string()],
        json_array_strings(&payload, "regions"),
        json_array_strings(&payload, "symbols"),
        artifact_id,
        duration_ms,
    )?;
    Ok(payload)
}

fn handle_resources_list(root: &Path, session: &Arc<Mutex<McpSessionState>>) -> Result<Value> {
    let status = daemon_status(root)?;
    let mut resources = Vec::new();
    let current_task_id = session
        .lock()
        .ok()
        .and_then(|guard| guard.current_task_id.clone())
        .or_else(|| select_current_task(&status.tasks).map(|task| task.task_id.clone()));
    if let Some(current_task_id) = current_task_id {
        if let Ok(mut guard) = session.lock() {
            guard.current_task_id = Some(current_task_id.clone());
        }
        resources.push(json!({
            "uri": "packet28://current/task",
            "name": "Packet28 current task",
            "description": format!("Current task metadata for {}", current_task_id),
            "mimeType": "application/json"
        }));
        resources.push(json!({
            "uri": "packet28://current/brief",
            "name": "Packet28 current brief",
            "description": format!("Latest broker brief for {}", current_task_id),
            "mimeType": "text/markdown"
        }));
        resources.push(json!({
            "uri": "packet28://current/events",
            "name": "Packet28 current events",
            "description": format!("Task event replay for {}", current_task_id),
            "mimeType": "application/json"
        }));
        resources.push(json!({
            "uri": "packet28://current/state",
            "name": "Packet28 current state",
            "description": format!("Task broker metadata for {}", current_task_id),
            "mimeType": "application/json"
        }));
    }
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
    let (task_id, current_alias, kind) = if let Some(kind) = uri.strip_prefix("packet28://current/")
    {
        (resolve_current_task_id(root, session)?, true, kind)
    } else {
        let task_id = uri
            .strip_prefix("packet28://task/")
            .and_then(|rest| rest.split('/').next())
            .filter(|task_id| !task_id.is_empty())
            .ok_or_else(|| anyhow!("invalid Packet28 resource URI"))?;
        let kind = uri
            .strip_prefix(&format!("packet28://task/{task_id}/"))
            .ok_or_else(|| anyhow!("invalid Packet28 resource URI"))?;
        (task_id.to_string(), false, kind)
    };
    track_task(session, root, &task_id)?;
    if current_alias && kind == "task" {
        let status = broker_task_status_via_session(root, session, &task_id)?;
        return Ok(json!({
            "contents": [
                {
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&json!({
                        "task_id": task_id,
                        "status": status,
                    }))?
                }
            ]
        }));
    }
    if kind == "brief" {
        let path = task_brief_markdown_path(root, &task_id);
        materialize_task_artifacts(root, session, &task_id)?;
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
    if kind == "events" {
        let frames = load_task_events(root, &task_id)?;
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
    if kind == "state" {
        let path = task_state_json_path(root, &task_id);
        materialize_task_artifacts(root, session, &task_id)?;
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

fn materialize_task_artifacts(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
) -> Result<()> {
    let status = broker_task_status_via_session(root, session, task_id)?;
    let mut request = status
        .task
        .and_then(|task| task.latest_broker_request)
        .ok_or_else(|| anyhow!("no latest broker request for task '{task_id}'"))?;
    request.persist_artifacts = Some(true);
    request.since_version = None;
    request.response_mode = Some(packet28_daemon_core::BrokerResponseMode::Full);
    let _ = broker_get_context_via_session(root, session, request)?;
    Ok(())
}
