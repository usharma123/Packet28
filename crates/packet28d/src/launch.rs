use super::*;

pub(crate) fn task_await_handoff(
    state: Arc<Mutex<DaemonState>>,
    request: TaskAwaitHandoffRequest,
) -> Result<TaskAwaitHandoffResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("daemon task await-handoff requires task_id");
    }
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(300_000));
    let poll = Duration::from_millis(request.poll_ms.unwrap_or(250).max(10));
    let started = Instant::now();
    let mut polls = 0_u64;
    loop {
        polls = polls.saturating_add(1);
        let status = broker_task_status(
            state.clone(),
            BrokerTaskStatusRequest {
                task_id: request.task_id.clone(),
            },
        )?;
        let is_newer_than_after = request
            .after_context_version
            .as_ref()
            .is_none_or(|after| status.latest_context_version.as_deref() != Some(after.as_str()));
        if status.handoff_ready && is_newer_than_after {
            return Ok(TaskAwaitHandoffResponse {
                task_status: status,
                waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                polls,
            });
        }
        if started.elapsed() >= timeout {
            let waiting_for_newer_context =
                request.after_context_version.as_ref().is_some_and(|after| {
                    status.latest_context_version.as_deref() == Some(after.as_str())
                });
            let reason = if waiting_for_newer_context {
                request
                    .after_context_version
                    .as_ref()
                    .map(|after| {
                        format!("newer handoff than context version '{after}' did not become ready")
                    })
                    .unwrap_or_else(|| "handoff did not become ready".to_string())
            } else {
                status
                    .handoff_reason
                    .unwrap_or_else(|| "handoff did not become ready".to_string())
            };
            anyhow::bail!(
                "timed out waiting for Packet28 handoff for task '{}': {}",
                request.task_id,
                reason
            );
        }
        thread::sleep(poll);
    }
}

pub(crate) struct TaskLaunchBootstrap {
    pub(crate) mode: &'static str,
    pub(crate) task_id: String,
    pub(crate) response: BrokerGetContextResponse,
    pub(crate) bootstrap_path: PathBuf,
    pub(crate) handoff_path: Option<String>,
    pub(crate) handoff_id: Option<String>,
    pub(crate) handoff_artifact_id: Option<String>,
    pub(crate) handoff_checkpoint_id: Option<String>,
    pub(crate) handoff_reason: Option<String>,
}

fn task_agent_dir(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join("agent")
}

fn task_agent_bootstrap_path(root: &Path, task_id: &str) -> PathBuf {
    task_agent_dir(root, task_id).join("latest-bootstrap.json")
}

fn task_agent_handoff_path(root: &Path, task_id: &str) -> PathBuf {
    task_agent_dir(root, task_id).join("latest-handoff.json")
}

fn task_agent_launch_log_path(root: &Path, task_id: &str, started_at_unix: u64) -> PathBuf {
    task_agent_dir(root, task_id).join(format!("launch-{}.log", started_at_unix))
}

fn task_prepare_handoff_bootstrap(
    state: Arc<Mutex<DaemonState>>,
    task_id: String,
    query: Option<String>,
    bootstrap_path: &Path,
    handoff_path: &Path,
) -> Result<TaskLaunchBootstrap> {
    let handoff = broker_prepare_handoff(
        state.clone(),
        BrokerPrepareHandoffRequest {
            task_id: task_id.clone(),
            query,
            response_mode: Some(BrokerResponseMode::Full),
            include_debug_memory: false,
        },
    )?;
    if !handoff.handoff_ready {
        anyhow::bail!(
            "Packet28 handoff is not ready for task '{}': {}",
            task_id,
            handoff.handoff_reason
        );
    }
    let response = handoff.context.ok_or_else(|| {
        anyhow!(
            "Packet28 returned a ready handoff for task '{}' without context payload",
            task_id
        )
    })?;
    if let Some(parent) = handoff_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    fs::write(handoff_path, serde_json::to_vec(&response)?)
        .with_context(|| format!("failed to write '{}'", handoff_path.display()))?;
    let handoff_id = handoff
        .handoff
        .as_ref()
        .map(|handoff| handoff.handoff_id.clone());
    if let Some(handoff_id) = handoff_id.as_deref() {
        let _ = crate::broker_handoff::mark_handoff_consumed(&state, &task_id, handoff_id)?;
    }
    Ok(TaskLaunchBootstrap {
        mode: "handoff",
        task_id,
        response,
        bootstrap_path: bootstrap_path.to_path_buf(),
        handoff_path: Some(handoff_path.to_string_lossy().to_string()),
        handoff_id,
        handoff_artifact_id: handoff.latest_handoff_artifact_id,
        handoff_checkpoint_id: handoff.latest_handoff_checkpoint_id,
        handoff_reason: Some(handoff.handoff_reason),
    })
}

fn task_prepare_launch_bootstrap(
    state: Arc<Mutex<DaemonState>>,
    request: &TaskLaunchAgentRequest,
) -> Result<TaskLaunchBootstrap> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("daemon task launch-agent requires task_id");
    }
    if request.command.is_empty() {
        anyhow::bail!("daemon task launch-agent requires a delegated command after --");
    }
    let root = state.lock().map_err(lock_err)?.root.clone();
    let bootstrap_path = task_agent_bootstrap_path(&root, &request.task_id);
    let handoff_path = task_agent_handoff_path(&root, &request.task_id);
    let after_context_version = if request.wait_for_handoff {
        load_task_record(&state, &request.task_id).and_then(|task| {
            task.latest_agent_bootstrap_mode
                .as_deref()
                .filter(|mode| *mode == "handoff")
                .and(task.latest_agent_context_version.clone())
        })
    } else {
        None
    };
    if request.wait_for_handoff {
        let _ = task_await_handoff(
            state.clone(),
            TaskAwaitHandoffRequest {
                task_id: request.task_id.clone(),
                timeout_ms: request.handoff_timeout_ms,
                poll_ms: request.handoff_poll_ms,
                after_context_version,
            },
        )?;
    }

    let status = broker_task_status(
        state.clone(),
        BrokerTaskStatusRequest {
            task_id: request.task_id.clone(),
        },
    )?;
    if !status.handoff_ready {
        anyhow::bail!(
            "Packet28 handoff is not ready for task '{}': {}",
            request.task_id,
            status
                .handoff_reason
                .unwrap_or_else(|| "checkpointed handoff required before relaunch".to_string())
        );
    }
    task_prepare_handoff_bootstrap(
        state,
        request.task_id.clone(),
        request.task.clone(),
        &bootstrap_path,
        &handoff_path,
    )
}

pub(crate) fn task_launch_agent(
    state: Arc<Mutex<DaemonState>>,
    request: TaskLaunchAgentRequest,
) -> Result<TaskLaunchAgentResponse> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let bootstrap = task_prepare_launch_bootstrap(state.clone(), &request)?;
    fs::write(
        &bootstrap.bootstrap_path,
        serde_json::to_vec(&bootstrap.response)?,
    )
    .with_context(|| {
        format!(
            "failed to persist bootstrap payload to '{}'",
            bootstrap.bootstrap_path.display()
        )
    })?;
    let brief_json_path = task_brief_json_path(&root, &bootstrap.task_id);
    let brief_md_path = task_brief_markdown_path(&root, &bootstrap.task_id);
    let state_json_path = task_state_json_path(&root, &bootstrap.task_id);
    let proxy_config = std::env::var_os("PACKET28_MCP_UPSTREAM_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            let candidate = root.join(".mcp.proxy.json");
            candidate.exists().then_some(candidate)
        });
    let proxy_command = proxy_config.as_ref().map(|config| {
        format!(
            "Packet28 mcp proxy --root {} --upstream-config {} --task-id {}",
            root.display(),
            config.display(),
            bootstrap.task_id
        )
    });
    let mcp_command = proxy_command
        .clone()
        .unwrap_or_else(|| format!("Packet28 mcp serve --root {}", root.display()));
    let started_at_unix = now_unix();
    let log_path = task_agent_launch_log_path(&root, &bootstrap.task_id, started_at_unix);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let stdout_log = fs::File::create(&log_path)
        .with_context(|| format!("failed to create '{}'", log_path.display()))?;
    let stderr_log = stdout_log
        .try_clone()
        .with_context(|| format!("failed to clone '{}'", log_path.display()))?;

    let mut child = Command::new(&request.command[0]);
    child
        .args(&request.command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout_log))
        .stderr(std::process::Stdio::from(stderr_log))
        .env("PACKET28_BOOTSTRAP_MODE", bootstrap.mode)
        .env("PACKET28_BOOTSTRAP_PATH", &bootstrap.bootstrap_path)
        .env("PACKET28_TASK_ID", &bootstrap.task_id)
        .env(
            "PACKET28_BROKER_CONTEXT_VERSION",
            &bootstrap.response.context_version,
        )
        .env(
            "PACKET28_BROKER_BUDGET_REMAINING_TOKENS",
            bootstrap.response.budget_remaining_tokens.to_string(),
        )
        .env("PACKET28_BROKER_BRIEF_PATH", &brief_md_path)
        .env("PACKET28_BROKER_BRIEF_JSON_PATH", &brief_json_path)
        .env("PACKET28_BROKER_STATE_PATH", &state_json_path)
        .env("PACKET28_BROKER_SUPPORTS_PUSH", "1")
        .env(
            "PACKET28_BROKER_PREPARE_HANDOFF_TOOL",
            "packet28.prepare_handoff",
        )
        .env("PACKET28_BROKER_WINDOW_MODE", "replace")
        .env("PACKET28_BROKER_SUPERSESSION", "1")
        .env("PACKET28_BROKER_SECTION_CACHE_KEY", "sections_by_id")
        .env("PACKET28_BROKER_REPLACE_PACKET28_CONTEXT", "1")
        .env(
            "PACKET28_HANDOFF_PATH",
            bootstrap.handoff_path.clone().unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_ID",
            bootstrap.handoff_id.clone().unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_ARTIFACT_ID",
            bootstrap.handoff_artifact_id.clone().unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_CHECKPOINT_ID",
            bootstrap.handoff_checkpoint_id.clone().unwrap_or_default(),
        )
        .env(
            "PACKET28_HANDOFF_REASON",
            bootstrap.handoff_reason.clone().unwrap_or_default(),
        )
        .env(
            "PACKET28_MCP_NOTIFICATION_METHOD",
            "notifications/packet28.context_updated",
        )
        .env("PACKET28_MCP_COMMAND", mcp_command)
        .env("PACKET28_MCP_PROXY_TASK_ID", &bootstrap.task_id)
        .env(
            "PACKET28_MCP_PROXY_COMMAND",
            proxy_command.unwrap_or_default(),
        )
        .env("PACKET28_ROOT", &root);
    let mut child = child
        .spawn()
        .with_context(|| format!("failed to spawn delegated command '{}'", request.command[0]))?;
    let pid = child.id();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let task = ensure_task_record_mut(&mut guard.tasks, &bootstrap.task_id);
        task.latest_agent_pid = Some(pid);
        task.latest_agent_bootstrap_mode = Some(bootstrap.mode.to_string());
        task.latest_agent_log_path = Some(log_path.to_string_lossy().to_string());
        task.latest_agent_started_at_unix = Some(started_at_unix);
        task.latest_agent_completed_at_unix = None;
        task.latest_agent_exit_code = None;
        task.latest_agent_context_version = Some(bootstrap.response.context_version.clone());
        task.latest_agent_handoff_artifact_id = bootstrap.handoff_artifact_id.clone();
        task.latest_agent_handoff_checkpoint_id = bootstrap.handoff_checkpoint_id.clone();
        persist_state(&guard)?;
    }
    let task_id = bootstrap.task_id.clone();
    let state_for_wait = state.clone();
    thread::spawn(move || {
        let wait_result = child.wait();
        let (exit_code, summary, completed_at_unix, error_text) = match wait_result {
            Ok(status) => (
                status.code(),
                format!(
                    "agent launch completed exit_code={}",
                    status.code().unwrap_or(-1)
                ),
                now_unix(),
                None,
            ),
            Err(err) => (
                None,
                format!("agent launch failed: {err}"),
                now_unix(),
                Some(err.to_string()),
            ),
        };
        if let Ok(mut guard) = state_for_wait.lock().map_err(lock_err) {
            let task = ensure_task_record_mut(&mut guard.tasks, &task_id);
            task.latest_agent_completed_at_unix = Some(completed_at_unix);
            task.latest_agent_exit_code = exit_code;
            if let Some(err) = error_text.clone() {
                task.last_error = Some(err);
            }
            let _ = persist_state(&guard);
        }
        let _ = emit_task_event(
            state_for_wait,
            &task_id,
            "task.agent_launch_completed",
            json!({
                "summary": summary,
                "exit_code": exit_code,
                "completed_at_unix": completed_at_unix,
            }),
        );
    });
    let _ = emit_task_event(
        state.clone(),
        &bootstrap.task_id,
        "task.agent_launch_started",
        json!({
            "summary": format!("spawned delegated agent pid={pid} mode={}", bootstrap.mode),
            "pid": pid,
            "bootstrap_mode": bootstrap.mode,
            "log_path": log_path.to_string_lossy().to_string(),
        }),
    );
    Ok(TaskLaunchAgentResponse {
        task_id: bootstrap.task_id,
        pid,
        bootstrap_mode: bootstrap.mode.to_string(),
        bootstrap_path: bootstrap.bootstrap_path.to_string_lossy().to_string(),
        log_path: log_path.to_string_lossy().to_string(),
        handoff_id: bootstrap.handoff_id,
        handoff_artifact_id: bootstrap.handoff_artifact_id,
        handoff_checkpoint_id: bootstrap.handoff_checkpoint_id,
        started_at_unix,
    })
}
