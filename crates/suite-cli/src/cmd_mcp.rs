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
    BrokerDecomposeRequest, BrokerEstimateContextRequest, BrokerGetContextRequest,
    BrokerValidatePlanRequest, BrokerWriteOp, BrokerWriteStateRequest, DaemonRequest,
    DaemonResponse,
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

#[derive(Default)]
struct McpSessionState {
    initialized: bool,
    shutdown: bool,
    tracked_tasks: BTreeMap<String, u64>,
    framing: Option<McpMessageFraming>,
    tool_owners: BTreeMap<String, String>,
    resource_owners: BTreeMap<String, String>,
    proxy_task_id: Option<String>,
    next_invocation_seq: u64,
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
}

struct UpstreamClient {
    name: String,
    _child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
}

const BROKER_SECTION_IDS: &[&str] = &[
    "task_objective",
    "active_decisions",
    "open_questions",
    "resolved_questions",
    "current_focus",
    "discovered_scope",
    "recent_tool_activity",
    "tool_failures",
    "evidence_cache",
    "checkpoint_deltas",
    "repo_map",
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
            },
        );
    }
    Ok(upstreams)
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
            refresh_upstream_tools(session, upstreams)?;
            if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                for upstream in upstreams.values_mut() {
                    let response = send_request_to_upstream(
                        upstream,
                        &json!({
                            "jsonrpc":"2.0",
                            "id": format!("packet28-tools-list-{}", upstream.name),
                            "method":"tools/list"
                        }),
                    )?;
                    if let Some(items) = response
                        .get("result")
                        .and_then(|value| value.get("tools"))
                        .and_then(Value::as_array)
                    {
                        tools.extend(items.iter().cloned());
                    }
                }
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/list" => {
            let mut result = handle_method(root, session, method, Value::Null)?;
            refresh_upstream_resources(session, upstreams)?;
            if let Some(resources) = result.get_mut("resources").and_then(Value::as_array_mut) {
                for upstream in upstreams.values_mut() {
                    let response = send_request_to_upstream(
                        upstream,
                        &json!({
                            "jsonrpc":"2.0",
                            "id": format!("packet28-resources-list-{}", upstream.name),
                            "method":"resources/list"
                        }),
                    )?;
                    if let Some(items) = response
                        .get("result")
                        .and_then(|value| value.get("resources"))
                        .and_then(Value::as_array)
                    {
                        resources.extend(items.iter().cloned());
                    }
                }
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/templates/list" => {
            let mut result = handle_method(root, session, method, Value::Null)?;
            if let Some(templates) = result
                .get_mut("resourceTemplates")
                .and_then(Value::as_array_mut)
            {
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
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
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
                refresh_upstream_resources(session, upstreams)?;
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
) -> Result<()> {
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
                    tool_owners.insert(name.to_string(), upstream.name.clone());
                }
            }
        }
    }
    if let Ok(mut guard) = session.lock() {
        guard.tool_owners = tool_owners;
    }
    Ok(())
}

fn refresh_upstream_resources(
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &mut BTreeMap<String, UpstreamClient>,
) -> Result<()> {
    let mut resource_owners = BTreeMap::new();
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
                }
            }
        }
    }
    if let Ok(mut guard) = session.lock() {
        guard.resource_owners = resource_owners;
    }
    Ok(())
}

fn owner_for_tool(session: &Arc<Mutex<McpSessionState>>, tool_name: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.tool_owners.get(tool_name).cloned())
}

fn owner_for_resource(session: &Arc<Mutex<McpSessionState>>, uri: &str) -> Option<String> {
    session
        .lock()
        .ok()
        .and_then(|guard| guard.resource_owners.get(uri).cloned())
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
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| anyhow!("timed out waiting for upstream '{}'", upstream.name))
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
        refresh_upstream_tools(session, upstreams)?;
    }
    let owner =
        owner_for_tool(session, name).ok_or_else(|| anyhow!("no upstream owns tool '{name}'"))?;
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
                "name": name,
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
    trimmed.to_string()
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
                            "artifact_id": {"type":"string"}
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
