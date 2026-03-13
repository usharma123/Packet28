use super::*;

pub(crate) fn prompt_descriptors() -> Vec<Value> {
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

pub(crate) fn handle_prompt_get(
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
- Use slim in-turn reducers such as `packet28.search` and `packet28.read_regions`.\n\
- Persist important reads, edits, decisions, checkpoints, and intent changes with `packet28.write_state`.\n\
- Keep one mutable Packet28 context block and replace older briefs when a newer brief supersedes them.\n\
- If Packet28 is fronting upstream MCP tools via proxy, prefer those proxied tools so activity is auto-captured into the next brief.\n\
- During the active turn, prefer slim Packet28 packets and fetch full artifacts only on demand.\n\
- For long-running work, record the current objective with `packet28.write_state(op=\"intention\", ...)`, save a checkpoint, then call `packet28.prepare_handoff` before launching a fresh worker.\n\
- Use `packet28.fetch_context` only when you explicitly need to inspect a stored handoff/context artifact.\n\
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
- Use slim reducers such as `packet28.search` and `packet28.read_regions` inside the active turn.\n\
- Persist state changes with `packet28.write_state` and fetch full artifacts only when you need detail.\n\
- If you are about to hand work to a fresh worker, write the latest intention, save a checkpoint, and call `packet28.prepare_handoff`.\n\
- Use `packet28.fetch_context` only to inspect a stored handoff/context artifact.\n\
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
                "No persisted brief is available yet. Produce reducer activity or prepare a handoff first."
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

pub(crate) fn resolve_requested_or_current_task_id(
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

pub(crate) fn resolve_current_task_id(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
) -> Result<String> {
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

pub(crate) fn daemon_status(root: &Path) -> Result<packet28_daemon_core::DaemonStatus> {
    match crate::cmd_daemon::send_request(root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => Ok(status),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub(crate) fn select_current_task(tasks: &[TaskRecord]) -> Option<&TaskRecord> {
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

pub(crate) fn handle_resources_list(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
) -> Result<Value> {
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

pub(crate) fn handle_resource_read(
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
    if status.handoff_ready {
        let _ = crate::broker_client::prepare_handoff(
            root,
            BrokerPrepareHandoffRequest {
                task_id: task_id.to_string(),
                query: None,
                response_mode: Some(packet28_daemon_core::BrokerResponseMode::Full),
            },
        )?;
    }
    Ok(())
}
