use super::*;

pub(crate) fn write_auto_capture_state_batch_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    requests: Vec<BrokerWriteStateRequest>,
) -> Result<()> {
    broker_write_state_batch_via_session(root, session, requests).map(|_| ())
}

pub(crate) fn summarize_json_value(value: &Value, limit: usize) -> String {
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

pub(crate) fn extract_named_string(value: &Value, keys: &[&str]) -> Option<String> {
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

pub(crate) fn extract_paths(root: &Path, value: &Value) -> Vec<String> {
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

pub(crate) fn extract_symbols(value: &Value) -> Vec<String> {
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

pub(crate) fn classify_error_message(message: &str) -> String {
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

pub(crate) fn is_retryable_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("temporar")
        || lower.contains("unavailable")
        || lower.contains("try again")
}

pub(crate) fn maybe_store_result_artifact(
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

pub(crate) fn store_result_artifact(
    root: &Path,
    task_id: &str,
    invocation_id: &str,
    result: &Value,
) -> Result<String> {
    store_tool_artifact(root, task_id, invocation_id, "result", result)
}

pub(crate) fn store_tool_artifact(
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

pub(crate) fn load_tool_result_artifact(
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
    let tool_path = task_artifact_dir(root, task_id)
        .join("tool-evidence")
        .join(&selected_artifact_id);
    let hook_path = task_artifact_dir(root, task_id)
        .join("hook-artifacts")
        .join(format!("{selected_artifact_id}.json"));
    let path = if tool_path.exists() {
        tool_path
    } else {
        hook_path
    };
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read stored artifact '{}'", path.display()))?;
    let value = serde_json::from_str(&text)
        .with_context(|| format!("invalid artifact JSON '{}'", path.display()))?;
    Ok((selected_artifact_id, value))
}

pub(crate) fn load_raw_output_artifact(
    root: &Path,
    task_id: &str,
    handle: &str,
) -> Result<(String, String)> {
    let trimmed = handle.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("packet28.fetch_raw_output requires handle"));
    }
    let direct = PathBuf::from(trimmed);
    let task_root = task_artifact_dir(root, task_id);
    let candidates = [
        direct.clone(),
        task_root.join(trimmed),
        task_root.join("hook-spool").join(trimmed),
        task_root.join("hook-artifacts").join(trimmed),
        task_root.join("tool-evidence").join(trimmed),
    ];
    let path = candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| anyhow!("failed to resolve raw artifact handle '{trimmed}'"))?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read raw artifact '{}'", path.display()))?;
    Ok((path.display().to_string(), text))
}

pub(crate) fn track_task(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    task_id: &str,
) -> Result<()> {
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

pub(crate) fn resolve_session_task_id(
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
    } else if let Some(task) = crate::task_runtime::load_active_task(root) {
        task.task_id
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

pub(crate) fn broker_write_state_via_session(
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

pub(crate) fn broker_write_state_batch_via_session(
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

pub(crate) fn broker_task_status_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    task_id: &str,
) -> Result<BrokerTaskStatusResponse> {
    let task_id = resolve_session_task_id(session, root, task_id, None, "packet28.task_status")?;
    let mut response = match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::BrokerTaskStatus {
            request: BrokerTaskStatusRequest {
                task_id: task_id.clone(),
            },
        },
    )? {
        DaemonResponse::BrokerTaskStatus { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }?;
    let supports_push = session.lock().ok().is_some_and(|guard| {
        guard.initialized && guard.framing.is_some() && guard.tracked_tasks.contains_key(&task_id)
    });
    response.supports_push = supports_push;
    Ok(response)
}

pub(crate) fn packet28_search_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: packet28_reducer_core::SearchRequest,
) -> Result<packet28_reducer_core::SearchResult> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::Packet28Search { request },
    )? {
        DaemonResponse::Packet28Search { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub(crate) fn next_task_invocation(
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
