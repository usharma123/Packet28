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
    if lower.contains("timeout") || lower.contains("timed out") {
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
        || lower.contains("timed out")
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
    let task_id = TaskStorageId::try_from(task_id)?;
    let handle = artifact_io::ArtifactHandle::from_invocation(invocation_id, suffix)?;
    let bytes = artifact_io::encode_json_artifact(payload)?;
    let _writer_lease =
        packet28_daemon_core::task_store_lease::acquire_task_store_writer_lease(root)?;
    artifact_io::write_task_artifact(
        root,
        &task_id,
        artifact_io::ArtifactLocation::ToolEvidence,
        &handle,
        &bytes,
    )?;
    Ok(handle.as_str().to_owned())
}

/// Search only authenticated predecessor namespaces. A present artifact in
/// the requested namespace wins; conflicting inherited handles need an owner.
pub(crate) fn read_lineage_artifact(
    root: &Path,
    task_id: &TaskStorageId,
    locations: &[(artifact_io::ArtifactLocation, artifact_io::ArtifactHandle)],
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    let mut found = None;
    for (index, owner) in crate::task_runtime::artifact_task_lineage(root, task_id.as_str())?
        .into_iter()
        .enumerate()
    {
        let owner = TaskStorageId::try_from(owner.as_str())?;
        for (location, handle) in locations {
            if let Some(artifact) =
                artifact_io::read_task_artifact(root, &owner, *location, handle)?
            {
                if index == 0 {
                    return Ok(Some(artifact));
                }
                if found.is_some() {
                    return Err(anyhow!(
                        "artifact handle has multiple recovery owners; supply the original task_id"
                    ));
                }
                found = Some(artifact);
                break;
            }
        }
    }
    Ok(found)
}

pub(crate) fn load_tool_result_artifact(
    root: &Path,
    task_id: &str,
    artifact_id: Option<&str>,
    invocation_id: Option<&str>,
) -> Result<(String, Value)> {
    let task_id = TaskStorageId::try_from(task_id)?;
    let selected_handle = if let Some(artifact_id) = artifact_id {
        artifact_io::ArtifactHandle::try_from(artifact_id)?
    } else if let Some(invocation_id) = invocation_id {
        artifact_io::ArtifactHandle::from_invocation(invocation_id, "result")?
    } else {
        return Err(anyhow!(
            "packet28.fetch_tool_result requires artifact_id or invocation_id"
        ));
    };
    let selected_artifact_id = selected_handle.as_str().to_owned();
    let hook_handle = selected_handle.json_file_name()?;
    let artifact = read_lineage_artifact(
        root,
        &task_id,
        &[
            (
                artifact_io::ArtifactLocation::ToolEvidence,
                selected_handle.clone(),
            ),
            (artifact_io::ArtifactLocation::HookArtifacts, hook_handle),
        ],
    )?;
    let (path, bytes) = artifact.ok_or_else(|| {
        anyhow!(
            "failed to resolve stored artifact handle {:?}",
            selected_handle.as_str()
        )
    })?;
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid artifact JSON '{}'", path.display()))?;
    Ok((selected_artifact_id, value))
}

pub(crate) fn load_raw_output_artifact(
    root: &Path,
    task_id: &str,
    handle: &str,
) -> Result<(String, String)> {
    let task_id = TaskStorageId::try_from(task_id)?;
    let handle = artifact_io::ArtifactHandle::try_from(handle)?;
    let artifact = read_lineage_artifact(
        root,
        &task_id,
        &[
            (artifact_io::ArtifactLocation::TaskRoot, handle.clone()),
            (artifact_io::ArtifactLocation::HookSpool, handle.clone()),
            (artifact_io::ArtifactLocation::HookArtifacts, handle.clone()),
            (artifact_io::ArtifactLocation::ToolEvidence, handle.clone()),
        ],
    )?;
    let (path, bytes) = artifact.ok_or_else(|| {
        anyhow!(
            "failed to resolve raw artifact handle {:?}",
            handle.as_str()
        )
    })?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("raw artifact '{}' is not UTF-8", path.display()))?;
    Ok((path.display().to_string(), text))
}

pub(crate) fn track_task(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    task_id: &str,
) -> Result<()> {
    let read = load_task_events_from_offset(root, task_id, 0)?;
    let latest_seq = read.events.last().map(|frame| frame.seq).unwrap_or(0);
    let mut guard = session
        .lock()
        .map_err(|_| anyhow!("failed to lock MCP session"))?;
    guard
        .tracked_tasks
        .entry(task_id.to_string())
        .or_insert(latest_seq);
    guard
        .tracked_task_offsets
        .entry(task_id.to_string())
        .or_insert(read.next_offset);
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

pub(crate) fn resolve_artifact_task_id(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    explicit_task_id: &str,
    _derive_hint: Option<&str>,
    tool_name: &str,
) -> Result<String> {
    let task_id = if !explicit_task_id.is_empty() {
        explicit_task_id.to_string()
    } else if let Some(task_id) = session_current_task_id(session) {
        task_id
    } else if let Some(task) = crate::task_runtime::load_active_task(root)? {
        task.task_id
    } else {
        return Err(anyhow!(
            "{tool_name} requires task_id or an active Packet28 session task"
        ));
    };
    validated_task_storage_id(&task_id)?;
    Ok(task_id)
}

pub(crate) fn resolve_session_task_id(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    explicit_task_id: &str,
    derive_hint: Option<&str>,
    tool_name: &str,
) -> Result<String> {
    resolve_session_task_id_with_startup(
        session,
        root,
        explicit_task_id,
        derive_hint,
        tool_name,
        false,
    )
}

// Daemon-bound tools must let startup persist any recovery link before choosing
// their continuation. Local artifact and analysis tools only read known links.
pub(crate) fn resolve_live_session_task_id(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    explicit_task_id: &str,
    derive_hint: Option<&str>,
    tool_name: &str,
) -> Result<String> {
    resolve_session_task_id_with_startup(
        session,
        root,
        explicit_task_id,
        derive_hint,
        tool_name,
        true,
    )
}

fn resolve_session_task_id_with_startup(
    session: &Arc<Mutex<McpSessionState>>,
    root: &Path,
    explicit_task_id: &str,
    derive_hint: Option<&str>,
    tool_name: &str,
    start_daemon: bool,
) -> Result<String> {
    let task_id = if !explicit_task_id.is_empty() {
        validated_task_storage_id(explicit_task_id)?;
        explicit_task_id.to_string()
    } else if let Some(task_id) = session_current_task_id(session) {
        task_id
    } else if let Some(task) = crate::task_runtime::load_active_task(root)? {
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
    validated_task_storage_id(&task_id)?;
    if start_daemon {
        crate::broker_client::ensure_daemon(root)?;
    }
    let task_id = crate::task_runtime::resolve_task_continuation(root, &task_id)?;
    track_task(session, root, &task_id)?;
    Ok(task_id)
}

#[cfg(unix)]
fn send_daemon_request_via_session(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: &DaemonRequest,
) -> Result<DaemonResponse> {
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
    let task_id =
        resolve_live_session_task_id(session, root, task_id, None, "packet28.task_status")?;
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
    packet28_search_via_session_with_force(root, session, request, false)
}

pub(crate) fn packet28_search_via_session_with_force(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    request: packet28_reducer_core::SearchRequest,
    force_indexed: bool,
) -> Result<packet28_reducer_core::SearchResult> {
    match send_daemon_request_via_session(
        root,
        session,
        &DaemonRequest::Packet28Search {
            request: packet28_daemon_protocol::message::Packet28SearchRequest {
                request,
                force_indexed,
            },
        },
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

#[cfg(test)]
mod recovery_artifact_tests {
    use super::*;
    use packet28_daemon_protocol::task::{TaskHistoryRecovery, TaskRecord, TaskRegistry};

    #[test]
    fn local_resolution_uses_known_recovery_without_starting_daemon() {
        let root = tempfile::tempdir().unwrap();
        let link = TaskHistoryRecovery {
            predecessor_task_id: "old".to_string(),
            successor_task_id: "new".to_string(),
            ..TaskHistoryRecovery::default()
        };
        let mut registry = TaskRegistry::default();
        registry.tasks.insert(
            "old".to_string(),
            TaskRecord {
                task_id: "old".to_string(),
                superseded_by: Some(link.clone()),
                ..TaskRecord::default()
            },
        );
        registry.tasks.insert(
            "new".to_string(),
            TaskRecord {
                task_id: "new".to_string(),
                recovered_from: Some(link),
                ..TaskRecord::default()
            },
        );
        packet28_daemon_core::storage::save_task_registry(root.path(), &registry).unwrap();
        let session = Arc::new(Mutex::new(McpSessionState::default()));
        assert_eq!(
            resolve_session_task_id(
                &session,
                root.path(),
                "old",
                None,
                "packet28.handoff_lint_paths"
            )
            .unwrap(),
            "new"
        );
        assert!(!packet28_daemon_protocol::paths::runtime_path(root.path()).exists());
        assert!(!packet28_daemon_protocol::paths::ready_path(root.path()).exists());
    }

    #[test]
    fn chained_recovery_keeps_result_raw_and_context_owners() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = TaskRegistry::default();
        for task_id in ["old", "mid", "new"] {
            registry.tasks.insert(
                task_id.to_string(),
                TaskRecord {
                    task_id: task_id.to_string(),
                    ..TaskRecord::default()
                },
            );
        }
        for (old, new) in [("old", "mid"), ("mid", "new")] {
            let link = TaskHistoryRecovery {
                predecessor_task_id: old.to_string(),
                successor_task_id: new.to_string(),
                ..TaskHistoryRecovery::default()
            };
            registry.tasks.get_mut(old).unwrap().superseded_by = Some(link.clone());
            registry.tasks.get_mut(new).unwrap().recovered_from = Some(link);
        }
        packet28_daemon_core::storage::save_task_registry(root.path(), &registry).unwrap();
        for owner in ["old", "mid"] {
            let task_id = TaskStorageId::try_from(owner).unwrap();
            for (location, name, bytes) in [
                (
                    artifact_io::ArtifactLocation::ToolEvidence,
                    format!("{owner}-result.json"),
                    br#"{"value":"preserved"}"#.to_vec(),
                ),
                (
                    artifact_io::ArtifactLocation::TaskRoot,
                    format!("{owner}-raw.log"),
                    b"raw bytes".to_vec(),
                ),
                (
                    artifact_io::ArtifactLocation::Versions,
                    format!("{owner}-context.json"),
                    br#"{"brief":"preserved context"}"#.to_vec(),
                ),
            ] {
                artifact_io::write_task_artifact(
                    root.path(),
                    &task_id,
                    location,
                    &artifact_io::ArtifactHandle::try_from(name.as_str()).unwrap(),
                    &bytes,
                )
                .unwrap();
            }
            assert_eq!(
                load_tool_result_artifact(
                    root.path(),
                    "new",
                    Some(&format!("{owner}-result.json")),
                    None
                )
                .unwrap()
                .1["value"],
                "preserved"
            );
            assert_eq!(
                load_raw_output_artifact(root.path(), "new", &format!("{owner}-raw.log"))
                    .unwrap()
                    .1,
                "raw bytes"
            );
            assert!(super::super::read_validated_context_artifact(
                root.path(),
                "new",
                &format!("{owner}-context")
            )
            .unwrap()
            .0
            .to_string_lossy()
            .contains(owner));
        }
        let session = Arc::new(Mutex::new(McpSessionState::default()));
        assert_eq!(
            resolve_artifact_task_id(&session, root.path(), "old", None, "fetch").unwrap(),
            "old"
        );
        assert!(session.lock().unwrap().tracked_tasks.is_empty());
        // Reused names in two predecessor namespaces require an explicit owner.
        for owner in ["old", "mid"] {
            store_tool_artifact(
                root.path(),
                owner,
                "shared",
                "result",
                &json!({"owner":owner}),
            )
            .unwrap();
        }
        assert!(
            load_tool_result_artifact(root.path(), "new", None, Some("shared"))
                .unwrap_err()
                .to_string()
                .contains("multiple recovery owners")
        );
        assert_eq!(
            load_tool_result_artifact(root.path(), "old", None, Some("shared"))
                .unwrap()
                .1["owner"],
            "old"
        );
    }
}
