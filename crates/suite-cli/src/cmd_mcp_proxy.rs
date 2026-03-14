use super::*;
use crate::cmd_mcp::proxy_upstream::{
    send_message_to_upstream, send_request_to_upstream, spawn_upstream_clients, UpstreamClient,
};

pub(crate) fn load_proxy_config(path: &Path) -> Result<McpProxyConfig> {
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

pub(crate) fn serve_proxy_stdio(
    root: PathBuf,
    config: McpProxyConfig,
    task_id: String,
) -> Result<()> {
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
        "packet28.search",
        "packet28.fetch_tool_result",
        "packet28.fetch_context",
        "packet28.prepare_handoff",
        "packet28.read_regions",
        "packet28.write_state",
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
