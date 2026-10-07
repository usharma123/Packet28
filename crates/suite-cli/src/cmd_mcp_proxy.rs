use super::*;
use crate::cmd_mcp::proxy_catalog::{
    ensure_upstream_resource_catalog_loaded, ensure_upstream_tools_loaded, forward_name_for_tool,
    invalidate_resource_catalog, owner_for_tool, route_for_resource, ProxyCatalog,
};
use crate::cmd_mcp::proxy_resource::ResourceRoute;
use crate::cmd_mcp::proxy_resource_paging::{resource_catalog_continuation, ResourceListKind};
use crate::cmd_mcp::proxy_upstream::{
    proxy_output_channel, spawn_upstream_clients, write_proxy_output, UpstreamClient, UpstreamPool,
};

const MAX_PROXY_INFLIGHT: usize = 64;
const PROXY_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const PROXY_FATAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("packet28-mcp-proxy")
        .build()
        .context("failed to start MCP proxy runtime")?;
    let result = runtime.block_on(serve_proxy_stdio_async(root, config, task_id));
    runtime.shutdown_timeout(MCP_RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

async fn serve_proxy_stdio_async(
    root: PathBuf,
    config: McpProxyConfig,
    task_id: String,
) -> Result<()> {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let session = Arc::new(Mutex::new(McpSessionState::default()));
    if let Ok(mut guard) = session.lock() {
        guard.proxy_task_id = Some(task_id.clone());
    }
    let tracked_root = root.clone();
    let tracked_session = session.clone();
    run_blocking("track proxy task", move || {
        track_task(&tracked_session, &tracked_root, &task_id)
    })
    .await?;
    let (output, output_receiver) = proxy_output_channel();
    let mut writer = Some(tokio::spawn(write_proxy_output(output_receiver)));
    let upstreams =
        match spawn_upstream_clients(&root, &config, output.clone(), session.clone()).await {
            Ok(upstreams) => upstreams,
            Err(error) => {
                drop(output);
                if let Some(writer) = writer {
                    let _ = writer.await;
                }
                return Err(error);
            }
        };
    let notification_output = output.clone();
    let mut notification_task = start_notification_task(
        root.clone(),
        session.clone(),
        MCP_NOTIFICATION_POLL_INTERVAL,
        move |notification, framing| {
            let output = notification_output.clone();
            async move {
                Ok(if output.try_send(notification, framing)? {
                    NotificationDelivery::Delivered
                } else {
                    NotificationDelivery::Backpressured
                })
            }
        },
    );
    let catalog = Arc::new(ProxyCatalog::default());
    let inflight = Arc::new(tokio::sync::Semaphore::new(MAX_PROXY_INFLIGHT));
    let mut requests = tokio::task::JoinSet::new();
    let mut serve_result = Ok(());
    loop {
        while let Some(joined) = requests.try_join_next() {
            if let Err(error) = flatten_proxy_task(joined) {
                serve_result = Err(error);
                break;
            }
        }
        if serve_result.is_err() {
            break;
        }
        let input = async {
            if let Some(writer_task) = writer.as_mut() {
                tokio::select! {
                    message = crate::cmd_mcp::transport::read_message_async(&mut reader) => message,
                    result = writer_task => {
                        writer = None;
                        Err(proxy_writer_error(result))
                    }
                }
            } else {
                Err(anyhow!("MCP stdout writer stopped"))
            }
        };
        let next = match notification_task.supervise_result(input).await {
            Ok(next) => next,
            Err(error) => {
                serve_result = Err(error);
                break;
            }
        };
        let Some((request, framing)) = next else {
            break;
        };
        if let Ok(mut guard) = session.lock() {
            guard.framing = Some(framing);
        }
        if request.is_array() && is_client_response_payload(&request) {
            let response = notification_task
                .supervise_result(dispatch_proxy_payload(
                    &root, &session, &upstreams, &catalog, request,
                ))
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    serve_result = Err(error);
                    break;
                }
            };
            if let Some(response) = response {
                if let Err(error) = notification_task
                    .supervise_result(output.send(response, framing))
                    .await
                {
                    serve_result = Err(error);
                    break;
                }
            }
            continue;
        }

        let method = request.get("method").and_then(Value::as_str);
        let process_inline = !request.is_array()
            && (method.is_none() || request.get("id").is_none() || method == Some("initialize"));
        if process_inline {
            let response = notification_task
                .supervise_result(dispatch_proxy_message(
                    &root, &session, &upstreams, &catalog, request,
                ))
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    serve_result = Err(error);
                    break;
                }
            };
            if let Some(response) = response {
                if let Err(error) = notification_task
                    .supervise_result(output.send(response, framing))
                    .await
                {
                    serve_result = Err(error);
                    break;
                }
            }
            continue;
        }

        // Initialization above remains the ordering barrier. Normal client
        // requests and request batches continue concurrently so the sole stdin
        // reader remains available for upstream reverse-request responses.
        let permit = match inflight.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                if let Err(error) = notification_task
                    .supervise_result(forward_overload_client_responses(&upstreams, &request))
                    .await
                {
                    serve_result = Err(error);
                    break;
                }
                if let Some(response) = proxy_overload_response(&request) {
                    if let Err(error) = notification_task
                        .supervise_result(output.send(response, framing))
                        .await
                    {
                        serve_result = Err(error);
                        break;
                    }
                }
                continue;
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                serve_result = Err(anyhow!("MCP proxy request queue closed"));
                break;
            }
        };
        let root = root.clone();
        let session = session.clone();
        let upstreams = upstreams.clone();
        let catalog = catalog.clone();
        let output = output.clone();
        requests.spawn(async move {
            let _permit = permit;
            let response = if request.is_array() {
                dispatch_proxy_payload(&root, &session, &upstreams, &catalog, request).await?
            } else {
                dispatch_proxy_message(&root, &session, &upstreams, &catalog, request).await?
            };
            if let Some(response) = response {
                output.send(response, framing).await?;
            }
            Ok(())
        });
    }

    notification_task.request_shutdown();
    if serve_result.is_err() {
        requests.abort_all();
    } else {
        let mut drain_error = None;
        let drained = tokio::time::timeout(PROXY_SHUTDOWN_GRACE, async {
            while let Some(joined) = requests.join_next().await {
                if let Err(error) = flatten_proxy_task(joined) {
                    drain_error.get_or_insert(error);
                }
            }
        })
        .await
        .is_ok();
        if !drained {
            requests.abort_all();
            serve_result = Err(anyhow!(
                "MCP proxy requests did not finish within {}ms shutdown grace",
                PROXY_SHUTDOWN_GRACE.as_millis()
            ));
        } else {
            serve_result = drain_error.map_or(Ok(()), Err);
        }
    }

    while let Some(joined) = requests.join_next().await {
        if let Err(error) = joined {
            if !error.is_cancelled() && serve_result.is_ok() {
                serve_result = Err(anyhow!("MCP proxy request task failed: {error}"));
            }
        } else if serve_result.is_ok() {
            serve_result = Err(anyhow!(
                "MCP proxy request completed after shutdown cancellation"
            ));
        }
    }

    if let Err(error) = notification_task
        .shutdown(MCP_NOTIFICATION_SHUTDOWN_GRACE)
        .await
    {
        if serve_result.is_ok() {
            serve_result = Err(error);
        }
    }

    upstreams.request_shutdown();
    let upstream_shutdown = if serve_result.is_err() {
        tokio::time::timeout(PROXY_FATAL_SHUTDOWN_GRACE, upstreams.shutdown())
            .await
            .map_err(|_| {
                anyhow!(
                    "MCP upstreams did not stop within {}ms fatal shutdown grace",
                    PROXY_FATAL_SHUTDOWN_GRACE.as_millis()
                )
            })
    } else {
        upstreams.shutdown().await;
        Ok(())
    };
    if let Err(error) = upstream_shutdown {
        if serve_result.is_ok() {
            serve_result = Err(error);
        }
    }

    drop(upstreams);
    drop(output);
    if let Some(writer) = writer {
        let writer_result = finish_proxy_writer(writer, serve_result.is_err()).await;
        if let Err(error) = writer_result {
            if serve_result.is_ok() {
                serve_result = Err(error);
            }
        }
    }
    serve_result
}

async fn finish_proxy_writer(
    mut writer: tokio::task::JoinHandle<Result<()>>,
    fatal: bool,
) -> Result<()> {
    if !fatal {
        return writer
            .await
            .map_err(|error| anyhow!("MCP stdout writer task failed: {error}"))?;
    }

    match tokio::time::timeout(MCP_NOTIFICATION_SHUTDOWN_GRACE, &mut writer).await {
        Ok(result) => result.map_err(|error| anyhow!("MCP stdout writer task failed: {error}"))?,
        Err(_) => {
            writer.abort();
            let _ = tokio::time::timeout(MCP_NOTIFICATION_SHUTDOWN_GRACE, &mut writer).await;
            Err(anyhow!(
                "MCP stdout writer did not stop within {}ms; task aborted",
                MCP_NOTIFICATION_SHUTDOWN_GRACE.as_millis()
            ))
        }
    }
}

async fn dispatch_proxy_payload(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &Arc<UpstreamPool>,
    catalog: &ProxyCatalog,
    payload: Value,
) -> Result<Option<Value>> {
    let Value::Array(requests) = payload else {
        return dispatch_proxy_message(root, session, upstreams, catalog, payload).await;
    };
    if requests.is_empty() {
        return Ok(Some(mcp_error_response(
            Value::Null,
            -32600,
            "empty JSON-RPC batch",
        )));
    }
    if requests.len() > MAX_MCP_BATCH_MESSAGES {
        return Ok(Some(Value::Array(vec![mcp_error_response(
            Value::Null,
            -32000,
            &format!("JSON-RPC batch member limit exceeded ({MAX_MCP_BATCH_MESSAGES})"),
        )])));
    }
    let mut pending_requests = Vec::with_capacity(requests.len());
    for request in requests {
        if is_client_response_message(&request)
            && upstreams.forward_client_response(&request).await?
        {
            continue;
        }
        pending_requests.push(request);
    }

    let mut responses = Vec::new();
    for request in pending_requests {
        if let Some(response) =
            dispatch_proxy_message(root, session, upstreams, catalog, request).await?
        {
            responses.push(response);
        }
    }
    Ok((!responses.is_empty()).then_some(Value::Array(responses)))
}

fn is_client_response_payload(payload: &Value) -> bool {
    payload.as_array().is_some_and(|messages| {
        !messages.is_empty() && messages.iter().all(is_client_response_message)
    })
}

fn is_client_response_message(message: &Value) -> bool {
    message
        .as_object()
        .is_some_and(|object| object.contains_key("id") && !object.contains_key("method"))
}

fn proxy_overload_response(request: &Value) -> Option<Value> {
    let overload = |id| mcp_error_response(id, -32000, "MCP proxy inflight limit reached");
    let Some(requests) = request.as_array() else {
        return request.get("id").cloned().map(overload);
    };
    let responses = requests
        .iter()
        .filter_map(|request| {
            request
                .get("method")
                .and_then(Value::as_str)
                .and(request.get("id"))
                .cloned()
                .map(overload)
        })
        .collect::<Vec<_>>();
    (!responses.is_empty()).then_some(Value::Array(responses))
}

async fn forward_overload_client_responses(
    upstreams: &UpstreamPool,
    request: &Value,
) -> Result<()> {
    let Some(messages) = request.as_array() else {
        return Ok(());
    };
    for message in messages
        .iter()
        .filter(|message| is_client_response_message(message))
    {
        let _ = upstreams.forward_client_response(message).await?;
    }
    Ok(())
}

async fn dispatch_proxy_message(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &Arc<UpstreamPool>,
    catalog: &ProxyCatalog,
    request: Value,
) -> Result<Option<Value>> {
    let Some(object) = request.as_object() else {
        return Ok(Some(mcp_error_response(
            Value::Null,
            -32600,
            "JSON-RPC request must be an object",
        )));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        if id.is_some() && upstreams.forward_client_response(&request).await? {
            return Ok(None);
        }
        return Ok(Some(mcp_error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "missing method",
        )));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        let _ = handle_proxy_notification(root, session, upstreams, method, params).await;
        return Ok(None);
    };
    Ok(Some(
        match handle_proxy_method(
            root,
            session,
            upstreams,
            catalog,
            method,
            params,
            id.clone(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => json!({
                "jsonrpc":"2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": error.to_string()
                }
            }),
        },
    ))
}

fn proxy_writer_error(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow!("MCP stdout writer stopped unexpectedly"),
        Ok(Err(error)) => error,
        Err(error) => anyhow!("MCP stdout writer task failed: {error}"),
    }
}

fn flatten_proxy_task(
    joined: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    joined.map_err(|error| anyhow!("MCP proxy request task failed: {error}"))?
}

async fn handle_proxy_notification(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &Arc<UpstreamPool>,
    method: &str,
    params: Value,
) -> Result<()> {
    handle_notification(root, session, method, params.clone())?;
    let notification = json!({
        "jsonrpc":"2.0",
        "method": method,
        "params": params,
    });
    for upstream in upstreams.values() {
        upstream.send_message(&notification).await?;
    }
    Ok(())
}

async fn handle_proxy_method(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &Arc<UpstreamPool>,
    catalog: &ProxyCatalog,
    method: &str,
    params: Value,
    id: Value,
) -> Result<Value> {
    match method {
        "initialize" => {
            if let Ok(mut guard) = session.lock() {
                guard.initialized = true;
                guard.upstream_tools_loaded = false;
            }
            invalidate_resource_catalog(session)?;
            let mut resource_list_changed = false;
            for upstream in upstreams.values() {
                let request = json!({
                    "jsonrpc":"2.0",
                    "id": format!("packet28-init-{}", upstream.name),
                    "method":"initialize",
                    "params": params.clone(),
                });
                let response = upstream.send_request(&request).await?;
                if response.get("error").is_some() {
                    return Ok(json!({
                        "jsonrpc":"2.0",
                        "id": id,
                        "error": response["error"].clone()
                    }));
                }
                resource_list_changed |=
                    response["result"]["capabilities"]["resources"]["listChanged"].as_bool()
                        == Some(true);
            }
            let mut result = handle_local_method(root, session, method, params).await?;
            if resource_list_changed {
                result["capabilities"]["resources"]["listChanged"] = Value::Bool(true);
            }
            Ok(json!({
                "jsonrpc":"2.0",
                "id": id,
                "result": result,
            }))
        }
        "tools/list" => {
            let mut result = handle_local_method(root, session, method, Value::Null).await?;
            if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                tools.extend(ensure_upstream_tools_loaded(session, upstreams, catalog).await?);
            }
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/list" => {
            if resource_catalog_continuation(&params)? {
                let result =
                    catalog.page_resources(ResourceListKind::Resources, &params, Vec::new())?;
                return Ok(json!({"jsonrpc":"2.0","id":id,"result":result}));
            }
            let local = handle_local_method(root, session, method, Value::Null).await?;
            let mut resources = local
                .get("resources")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| anyhow!("local resources/list result is missing resources"))?;
            let upstream =
                ensure_upstream_resource_catalog_loaded(session, upstreams, catalog).await?;
            resources.extend(upstream.resources);
            let result = catalog.page_resources(ResourceListKind::Resources, &params, resources)?;
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "resources/templates/list" => {
            if resource_catalog_continuation(&params)? {
                let result =
                    catalog.page_resources(ResourceListKind::Templates, &params, Vec::new())?;
                return Ok(json!({"jsonrpc":"2.0","id":id,"result":result}));
            }
            let local = handle_local_method(root, session, method, Value::Null).await?;
            let mut templates = local
                .get("resourceTemplates")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    anyhow!("local resources/templates/list result is missing resourceTemplates")
                })?;
            let upstream =
                ensure_upstream_resource_catalog_loaded(session, upstreams, catalog).await?;
            templates.extend(upstream.templates);
            let result = catalog.page_resources(ResourceListKind::Templates, &params, templates)?;
            Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }
        "prompts/list" => Ok(json!({
            "jsonrpc":"2.0",
            "id":id,
            "result": handle_local_method(root, session, method, Value::Null).await?,
        })),
        "resources/subscribe" | "resources/unsubscribe" => Err(anyhow!(
            "MCP proxy does not advertise resource subscriptions; method '{method}' is unsupported"
        )),
        "prompts/get" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing prompt name"))?;
            if name.starts_with("packet28.") {
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "result": handle_local_method(root, session, method, params).await?,
                }));
            }
            let upstream = upstreams
                .values()
                .next()
                .ok_or_else(|| anyhow!("no upstream MCP servers configured"))?;
            upstream
                .send_request(&json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method":"prompts/get",
                    "params": params,
                }))
                .await
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
                    "result": handle_local_method(root, session, method, params).await?,
                }));
            }
            let _ = ensure_upstream_resource_catalog_loaded(session, upstreams, catalog).await?;
            let owner = match route_for_resource(session, uri)? {
                ResourceRoute::Missing => {
                    return Err(anyhow!("no upstream owns resource '{uri}'"));
                }
                ResourceRoute::Unique(owner) => owner,
                ResourceRoute::Ambiguous(owners) => {
                    return Err(anyhow!(
                        "resource '{uri}' is advertised by multiple upstreams: {}",
                        owners.join(", ")
                    ));
                }
            };
            let upstream = upstreams
                .get(&owner)
                .ok_or_else(|| anyhow!("missing upstream '{owner}'"))?;
            let response = upstream
                .send_request(&json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method":"resources/read",
                    "params": params,
                }))
                .await?;
            Ok(response)
        }
        "tools/call" => handle_proxy_tool_call(root, session, upstreams, catalog, params, id).await,
        _ => {
            let upstream = upstreams
                .values()
                .next()
                .ok_or_else(|| anyhow!("no upstream MCP servers configured"))?;
            upstream
                .send_request(&json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }))
                .await
        }
    }
}

async fn handle_local_method(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    method: &str,
    params: Value,
) -> Result<Value> {
    let root = root.to_path_buf();
    let session = session.clone();
    let method = method.to_string();
    run_blocking("local MCP method", move || {
        handle_method(&root, &session, &method, params)
    })
    .await
}

async fn run_blocking<T, F>(operation: &'static str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .with_context(|| format!("{operation} worker failed"))?
}

fn next_proxy_invocation(session: &Arc<Mutex<McpSessionState>>) -> Result<(String, u64, String)> {
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    let task_id = guard
        .proxy_task_id
        .clone()
        .ok_or_else(|| anyhow!("proxy task_id is not initialized"))?;
    let (sequence, invocation_id) = guard.invocations.allocate()?;
    Ok((task_id, sequence, invocation_id))
}

async fn handle_proxy_tool_call(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    upstreams: &Arc<UpstreamPool>,
    catalog: &ProxyCatalog,
    params: Value,
    id: Value,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    if name.starts_with("packet28.") || name.starts_with("packet28_") {
        let root = root.to_path_buf();
        let session = session.clone();
        let result = run_blocking("native MCP tool", move || {
            handle_tool_call(&root, &session, params)
        })
        .await?;
        return Ok(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }

    if owner_for_tool(session, name).is_none() {
        let _ = ensure_upstream_tools_loaded(session, upstreams, catalog).await?;
    }
    let owner =
        owner_for_tool(session, name).ok_or_else(|| anyhow!("no upstream owns tool '{name}'"))?;
    let upstream_tool_name = forward_name_for_tool(session, name)
        .ok_or_else(|| anyhow!("no upstream mapping found for tool '{name}'"))?;
    let upstream = upstreams
        .get(&owner)
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

    let capture_root = root.to_path_buf();
    let capture_session = session.clone();
    let capture_task_id = task_id.clone();
    let capture_invocation_id = invocation_id.clone();
    let capture_name = name.to_string();
    let capture_owner = owner.clone();
    let capture_request_summary = request_summary.clone();
    let capture_request_fingerprint = request_fingerprint.clone();
    let capture_request_paths = request_paths.clone();
    let capture_request_symbols = request_symbols.clone();
    run_blocking("record upstream MCP invocation", move || {
        track_task(&capture_session, &capture_root, &capture_task_id)?;
        write_auto_capture_state(
            &capture_root,
            BrokerWriteStateRequest {
                task_id: capture_task_id,
                op: Some(BrokerWriteOp::ToolInvocationStarted),
                invocation_id: Some(capture_invocation_id),
                tool_name: Some(capture_name),
                server_name: Some(capture_owner),
                operation_kind: Some(operation_kind),
                request_summary: Some(capture_request_summary),
                request_fingerprint: Some(capture_request_fingerprint),
                sequence: Some(sequence),
                paths: capture_request_paths,
                symbols: capture_request_symbols,
                ..BrokerWriteStateRequest::default()
            },
        )
    })
    .await?;

    let started_at = Instant::now();
    let response = upstream
        .send_request(&json!({
            "jsonrpc":"2.0",
            "id": id,
            "method":"tools/call",
            "params": {
                "name": upstream_tool_name,
                "arguments": arguments.clone(),
            }
        }))
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let error_message = error.to_string();
            let capture_root = root.to_path_buf();
            let capture_task_id = task_id.clone();
            let capture_invocation_id = invocation_id.clone();
            let capture_name = name.to_string();
            let capture_owner = owner.clone();
            let capture_request_summary = request_summary.clone();
            let capture_request_fingerprint = request_fingerprint.clone();
            let capture_error_class = classify_error_message(&error_message);
            let capture_error_message = error_message.clone();
            let capture_paths = request_paths.clone();
            let capture_symbols = request_symbols.clone();
            let record_result = run_blocking(
                "record failed upstream MCP transport invocation",
                move || {
                    write_auto_capture_state(
                        &capture_root,
                        BrokerWriteStateRequest {
                            task_id: capture_task_id,
                            op: Some(BrokerWriteOp::ToolInvocationFailed),
                            invocation_id: Some(capture_invocation_id),
                            tool_name: Some(capture_name),
                            server_name: Some(capture_owner),
                            operation_kind: Some(operation_kind),
                            request_summary: Some(capture_request_summary),
                            request_fingerprint: Some(capture_request_fingerprint),
                            error_class: Some(capture_error_class),
                            error_message: Some(capture_error_message.clone()),
                            retryable: Some(is_retryable_error(&capture_error_message)),
                            sequence: Some(sequence),
                            duration_ms: Some(duration_ms),
                            paths: capture_paths,
                            symbols: capture_symbols,
                            ..BrokerWriteStateRequest::default()
                        },
                    )
                },
            )
            .await;
            if let Err(record_error) = record_result {
                return Err(anyhow!(
                    "{error:#}; additionally failed to record terminal invocation state: \
                     {record_error:#}"
                ));
            }
            return Err(error);
        }
    };
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
        let capture_root = root.to_path_buf();
        let capture_task_id = task_id;
        let capture_invocation_id = invocation_id;
        let capture_name = name.to_string();
        let capture_owner = owner;
        let capture_error = error.clone();
        run_blocking("record failed upstream MCP invocation", move || {
            let artifact_id = store_tool_artifact(
                &capture_root,
                &capture_task_id,
                &capture_invocation_id,
                "failure",
                &capture_error,
            )
            .ok();
            write_auto_capture_state(
                &capture_root,
                BrokerWriteStateRequest {
                    task_id: capture_task_id.clone(),
                    op: Some(BrokerWriteOp::ToolInvocationFailed),
                    invocation_id: Some(capture_invocation_id),
                    tool_name: Some(capture_name.clone()),
                    server_name: Some(capture_owner),
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
                    &capture_root,
                    BrokerWriteStateRequest {
                        task_id: capture_task_id.clone(),
                        op: Some(BrokerWriteOp::EvidenceCaptured),
                        artifact_id: Some(artifact_id),
                        note: Some(format!("failure output for {capture_name}")),
                        ..BrokerWriteStateRequest::default()
                    },
                )?;
            }
            if !paths.is_empty() || !symbols.is_empty() {
                write_auto_capture_state(
                    &capture_root,
                    BrokerWriteStateRequest {
                        task_id: capture_task_id,
                        op: Some(BrokerWriteOp::FocusInferred),
                        note: Some(format!("inferred from failed {capture_name}")),
                        paths,
                        symbols,
                        ..BrokerWriteStateRequest::default()
                    },
                )?;
            }
            Ok(())
        })
        .await?;
        return Ok(response);
    }

    let result = response.get("result").cloned().unwrap_or(Value::Null);
    let rewrite_response = should_compact_proxy_tool(upstream, name, operation_kind)
        && response.get("result").is_some();
    let result_summary = if rewrite_response {
        summarize_json_value(&result, 280)
    } else {
        summarize_json_value(&result, 200)
    };
    let capture_root = root.to_path_buf();
    let capture_name = name.to_string();
    let capture_owner = owner.clone();
    let capture_result_summary = result_summary.clone();
    let artifact_id = run_blocking("record completed upstream MCP invocation", move || {
        let artifact_id = if rewrite_response {
            Some(store_tool_artifact(
                &capture_root,
                &task_id,
                &invocation_id,
                "result",
                &result,
            )?)
        } else {
            maybe_store_result_artifact(
                &capture_root,
                &task_id,
                &invocation_id,
                &result,
                !paths.is_empty() || !symbols.is_empty(),
            )?
        };
        write_auto_capture_state(
            &capture_root,
            BrokerWriteStateRequest {
                task_id: task_id.clone(),
                op: Some(BrokerWriteOp::ToolInvocationCompleted),
                invocation_id: Some(invocation_id),
                tool_name: Some(capture_name.clone()),
                server_name: Some(capture_owner),
                operation_kind: Some(operation_kind),
                request_summary: Some(request_summary),
                result_summary: Some(capture_result_summary),
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
                &capture_root,
                BrokerWriteStateRequest {
                    task_id: task_id.clone(),
                    op: Some(BrokerWriteOp::FocusInferred),
                    note: Some(format!("inferred from {capture_name}")),
                    paths,
                    symbols,
                    ..BrokerWriteStateRequest::default()
                },
            )?;
        }
        if let Some(artifact_id) = artifact_id.as_ref() {
            write_auto_capture_state(
                &capture_root,
                BrokerWriteStateRequest {
                    task_id,
                    op: Some(BrokerWriteOp::EvidenceCaptured),
                    artifact_id: Some(artifact_id.clone()),
                    note: Some(format!("captured from {capture_name}")),
                    ..BrokerWriteStateRequest::default()
                },
            )?;
        }
        Ok(artifact_id)
    })
    .await?;
    let response_artifact_id = artifact_id;

    if rewrite_response {
        let compact_preview = result_summary.clone();
        return Ok(json!({
            "jsonrpc":"2.0",
            "id": id,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": compact_preview
                    }
                ],
                "structuredContent": {
                    "original_tool": name,
                    "upstream": owner,
                    "artifact_id": response_artifact_id,
                    "compact_preview": result_summary,
                    "response_mode": "slim"
                }
            }
        }));
    }

    Ok(response)
}

fn should_compact_proxy_tool(
    upstream: &UpstreamClient,
    tool_name: &str,
    operation_kind: suite_packet_core::ToolOperationKind,
) -> bool {
    matches!(
        operation_kind,
        suite_packet_core::ToolOperationKind::Read | suite_packet_core::ToolOperationKind::Search
    ) && upstream
        .compact_tools
        .iter()
        .any(|entry| entry == tool_name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn fatal_cleanup_aborts_a_blocked_stdout_writer_after_the_grace() {
        let writer = tokio::spawn(std::future::pending::<Result<()>>());
        let mut finish = Box::pin(finish_proxy_writer(writer, true));
        tokio::task::yield_now().await;

        tokio::time::advance(MCP_NOTIFICATION_SHUTDOWN_GRACE).await;
        let error = finish.as_mut().await.unwrap_err();

        assert!(error.to_string().contains("task aborted"));
    }

    #[test]
    fn proxy_overload_preserves_each_request_id_and_skips_non_requests() {
        let response = proxy_overload_response(&json!([
            {"jsonrpc":"2.0","id":"first","method":"tools/list"},
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":7,"method":"prompts/list"},
            {"jsonrpc":"2.0","id":"client-response","result":{}}
        ]))
        .unwrap();
        let responses = response.as_array().unwrap();
        assert_eq!(
            (
                responses.len(),
                responses[0]["id"].clone(),
                responses[1]["id"].clone(),
                responses[0]["error"]["code"].clone(),
                responses[1]["error"]["code"].clone(),
            ),
            (2, json!("first"), json!(7), json!(-32000), json!(-32000))
        );
        assert!(proxy_overload_response(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .is_none());
        assert!(proxy_overload_response(&json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":"client-response","result":{}}
        ]))
        .is_none());
    }

    #[tokio::test]
    async fn proxy_batch_limit_plus_one_is_bounded_and_next_request_remains_responsive() {
        let root = tempfile::tempdir().unwrap();
        let session = Arc::new(Mutex::new(McpSessionState::default()));
        let config = McpProxyConfig::default();
        let (output, _receiver) = proxy_output_channel();
        let upstreams = spawn_upstream_clients(root.path(), &config, output, session.clone())
            .await
            .unwrap();
        let oversized = Value::Array(vec![
            json!({"jsonrpc":"2.0","method":"notifications/initialized"});
            MAX_MCP_BATCH_MESSAGES + 1
        ]);

        let rejection = dispatch_proxy_payload(
            root.path(),
            &session,
            &upstreams,
            &ProxyCatalog::default(),
            oversized,
        )
        .await
        .unwrap()
        .unwrap();
        let responses = rejection.as_array().unwrap();
        assert_eq!(
            (
                responses.len(),
                responses[0]["id"].clone(),
                responses[0]["error"]["code"].clone(),
            ),
            (1, Value::Null, json!(-32000))
        );

        let next = dispatch_proxy_payload(
            root.path(),
            &session,
            &upstreams,
            &ProxyCatalog::default(),
            json!({"jsonrpc":"2.0","id":"next","method":"prompts/list"}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(next["id"], "next");
    }
}
