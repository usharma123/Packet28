use super::*;
use crate::broker::{
    broker_prepare_handoff, broker_task_status, emit_task_event_for_generation,
    ensure_task_record_mut, mark_handoff_consumed,
};
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin};

const CHILD_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const DELEGATED_LAUNCH_GATE_SCRIPT: &str = "printf '%s\n' \
    'packet28 delegated launch gate ready'; \
    if IFS= read -r _; then exec </dev/null; exec \"$@\"; fi";

struct ChildRegistration {
    generation: TaskGenerationToken,
    pid: u32,
}

struct DelegatedLaunchGate {
    writer: ChildStdin,
}

impl DelegatedLaunchGate {
    fn release(mut self) -> Result<()> {
        self.writer
            .write_all(b"launch\n")
            .context("failed to release delegated child launch gate")
    }
}

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        self.generation.complete_child(self.pid);
    }
}

fn signal_process_group(process: OwnedChildProcess, signal: i32) -> Result<()> {
    // SAFETY: `kill` is called with a process-group id created by
    // `CommandExt::process_group(0)` and a valid POSIX signal constant.
    let result = unsafe { libc::kill(-process.process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| {
        format!(
            "failed to signal process group {} for child {}",
            process.process_group, process.pid
        )
    })
}

fn process_group_exists(process: OwnedChildProcess) -> Result<bool> {
    // SAFETY: signal 0 performs a non-mutating existence probe for the owned
    // process group.
    let result = unsafe { libc::kill(-process.process_group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(error).with_context(|| {
        format!(
            "failed to probe process group {} for child {}",
            process.process_group, process.pid
        )
    })
}

pub(crate) fn current_process_group() -> i32 {
    // SAFETY: `getpgrp` has no preconditions and only reads the caller's
    // current process-group id.
    unsafe { libc::getpgrp() }
}

fn wait_for_process_group_exit(process: OwnedChildProcess, timeout: Duration) -> Result<bool> {
    let started = Instant::now();
    loop {
        if !process_group_exists(process)? {
            return Ok(true);
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_remaining_process_group(process: OwnedChildProcess) -> Result<()> {
    if !process_group_exists(process)? {
        return Ok(());
    }
    signal_process_group(process, libc::SIGTERM)?;
    if wait_for_process_group_exit(process, CHILD_TERMINATION_GRACE)? {
        return Ok(());
    }
    signal_process_group(process, libc::SIGKILL)?;
    if wait_for_process_group_exit(process, CHILD_REAP_TIMEOUT)? {
        return Ok(());
    }
    anyhow::bail!(
        "timed out waiting for delegated process group {} to exit",
        process.process_group
    )
}

pub(crate) fn recovered_agent_process_group_exists(pid: u32) -> Result<bool> {
    if pid == 0 {
        anyhow::bail!("recovered agent pid must be greater than zero");
    }
    if pid == std::process::id() {
        anyhow::bail!(
            "refusing to trust recovered agent pid {pid} because it is the current daemon"
        );
    }
    let process_group = i32::try_from(pid)
        .with_context(|| format!("recovered agent pid {pid} does not fit in a process-group id"))?;
    if process_group == current_process_group() {
        anyhow::bail!(
            "refusing to trust recovered agent process group {process_group} because it owns the \
             current daemon"
        );
    }
    process_group_exists(OwnedChildProcess { pid, process_group })
        .with_context(|| format!("failed to inspect recovered agent process group for pid {pid}"))
}

fn terminate_and_reap_child(child: &mut Child, process: OwnedChildProcess) -> Result<()> {
    let _ = signal_process_group(process, libc::SIGTERM);
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return terminate_remaining_process_group(process);
        }
        if started.elapsed() >= CHILD_TERMINATION_GRACE {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let _ = signal_process_group(process, libc::SIGKILL);
    child.wait().with_context(|| {
        format!(
            "failed to reap delegated child process {} after cancellation",
            process.pid
        )
    })?;
    terminate_remaining_process_group(process)
}

pub(crate) fn terminate_generation_processes(generation: &TaskGenerationToken) -> Result<()> {
    terminate_generations_processes(std::slice::from_ref(generation))
}

pub(crate) fn terminate_generations_processes(generations: &[TaskGenerationToken]) -> Result<()> {
    signal_generation_processes(generations, libc::SIGTERM, "terminate");
    if wait_for_generation_children(generations, CHILD_TERMINATION_GRACE) {
        return Ok(());
    }

    signal_generation_processes(generations, libc::SIGKILL, "kill");
    if wait_for_generation_children(generations, CHILD_REAP_TIMEOUT) {
        return Ok(());
    }

    let remaining = generations
        .iter()
        .flat_map(TaskGenerationToken::children)
        .map(|process| process.pid.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!("timed out reaping cancelled task child processes: {remaining}")
}

fn signal_generation_processes(generations: &[TaskGenerationToken], signal: i32, action: &str) {
    for process in generations.iter().flat_map(TaskGenerationToken::children) {
        if let Err(error) = signal_process_group(process, signal) {
            daemon_log(&format!(
                "failed to {action} task child pid={} error={error:#}",
                process.pid
            ));
        }
    }
}

fn wait_for_generation_children(generations: &[TaskGenerationToken], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    for generation in generations {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !generation.wait_for_children(remaining) {
            return false;
        }
    }
    true
}

pub(crate) fn spawn_owned_child_waiter(
    state: Arc<Mutex<DaemonState>>,
    task_id: String,
    generation: TaskGenerationToken,
    process: OwnedChildProcess,
    child: Child,
) -> Result<()> {
    // Ownership invariant: the child is stored outside the closure until the
    // waiter thread is successfully created. If thread creation fails, this
    // function still owns the handle and synchronously terminates and reaps it.
    let pid = child.id();
    debug_assert_eq!(pid, process.pid);
    let shared_child = Arc::new(Mutex::new(Some(child)));
    let child_for_waiter = shared_child.clone();
    let generation_for_waiter = generation.clone();
    let spawn_result = thread::Builder::new()
        .name(format!("packet28-child-waiter-{pid}"))
        .spawn(move || {
            let _registration = ChildRegistration {
                generation: generation_for_waiter.clone(),
                pid,
            };
            let Some(mut child) = child_for_waiter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            else {
                daemon_log(&format!(
                    "delegated child waiter lost ownership of pid={pid}"
                ));
                return;
            };
            let wait_result = child.wait().and_then(|status| {
                terminate_remaining_process_group(process)
                    .map(|()| status)
                    .map_err(|error| std::io::Error::other(error.to_string()))
            });
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
                Err(error) => (
                    None,
                    format!("agent launch failed: {error}"),
                    now_unix(),
                    Some(error.to_string()),
                ),
            };

            if let Ok(mut guard) = state.lock().map_err(lock_err) {
                if guard
                    .task_generations
                    .matches(&task_id, generation_for_waiter.id())
                    && !generation_for_waiter.is_cancelled()
                {
                    let mutable = guard.require_task_mutable(&task_id).is_ok();
                    if let Some(task) = guard.tasks.tasks.get_mut(&task_id).filter(|_| mutable) {
                        if task.latest_agent_pid == Some(pid) {
                            task.latest_agent_completed_at_unix = Some(completed_at_unix);
                            task.latest_agent_exit_code = exit_code;
                            if let Some(error) = error_text {
                                task.last_error = Some(error);
                            }
                            let _ = persist_task(&guard, &task_id);
                        }
                    }
                }
            }
            let _ = emit_task_event_for_generation(
                state,
                &task_id,
                generation_for_waiter.id(),
                "task.agent_launch_completed",
                json!({
                    "summary": summary,
                    "exit_code": exit_code,
                    "completed_at_unix": completed_at_unix,
                }),
            );
        });
    if let Err(error) = spawn_result {
        let cleanup_result = shared_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(|mut child| terminate_and_reap_child(&mut child, process))
            .unwrap_or(Ok(()));
        generation.complete_child(pid);
        cleanup_result?;
        return Err(error).with_context(|| {
            format!("failed to start delegated child waiter thread for pid {pid}")
        });
    }
    Ok(())
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

fn task_agent_dir(root: &Path, task_id: &str) -> Result<PathBuf> {
    let task_id = task_storage_id(task_id)?;
    Ok(task_artifact_dir(root, &task_id).join("agent"))
}

fn task_agent_bootstrap_path(root: &Path, task_id: &str) -> Result<PathBuf> {
    Ok(task_agent_dir(root, task_id)?.join("latest-bootstrap.json"))
}

fn task_agent_handoff_path(root: &Path, task_id: &str) -> Result<PathBuf> {
    Ok(task_agent_dir(root, task_id)?.join("latest-handoff.json"))
}

fn task_agent_launch_log_path(root: &Path, task_id: &str, started_at_unix: u64) -> Result<PathBuf> {
    Ok(task_agent_dir(root, task_id)?.join(format!("launch-{started_at_unix}.log")))
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
        anyhow!("Packet28 returned a ready handoff for task '{task_id}' without context payload")
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
        let _ = mark_handoff_consumed(&state, &task_id, handoff_id)?;
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
    let bootstrap_path = task_agent_bootstrap_path(&root, &request.task_id)?;
    let handoff_path = task_agent_handoff_path(&root, &request.task_id)?;

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
    if request.task_id.trim().is_empty() {
        anyhow::bail!("daemon task launch-agent requires task_id");
    }
    if request.command.is_empty() {
        anyhow::bail!("daemon task launch-agent requires a delegated command after --");
    }
    if request.wait_for_handoff {
        anyhow::bail!(
            "daemon task launch-agent handoff waiting must run on the async orchestration boundary"
        );
    }
    let (root, generation, _launch_lease, _child_launch_lease) = {
        let mut guard = state.lock().map_err(lock_err)?;
        ensure_task_record_mut(&mut guard, &request.task_id)?;
        let generation = guard.task_generations.ensure(&request.task_id)?;
        let launch_lease = generation.acquire_operation().ok_or_else(|| {
            anyhow!(
                "task '{}' was cancelled before delegated agent launch",
                request.task_id
            )
        })?;
        let child_launch_lease = generation.acquire_child_launch().ok_or_else(|| {
            if generation.is_cancelled() {
                anyhow!(
                    "task '{}' was cancelled before delegated agent launch",
                    request.task_id
                )
            } else {
                anyhow!(
                    "task '{}' already has an active delegated agent launch",
                    request.task_id
                )
            }
        })?;
        persist_task(&guard, &request.task_id)?;
        (
            guard.root.clone(),
            generation,
            launch_lease,
            child_launch_lease,
        )
    };
    let bootstrap = task_prepare_launch_bootstrap(state.clone(), &request)?;
    if generation.is_cancelled() {
        anyhow::bail!(
            "task '{}' was cancelled while preparing delegated agent launch",
            request.task_id
        );
    }
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
    let storage_id = task_storage_id(&bootstrap.task_id)?;
    let brief_json_path = task_brief_json_path(&root, &storage_id);
    let brief_md_path = task_brief_markdown_path(&root, &storage_id);
    let state_json_path = task_state_json_path(&root, &storage_id);
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
    let log_path = task_agent_launch_log_path(&root, &bootstrap.task_id, started_at_unix)?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let stdout_log = fs::File::create(&log_path)
        .with_context(|| format!("failed to create '{}'", log_path.display()))?;
    let stderr_log = stdout_log
        .try_clone()
        .with_context(|| format!("failed to clone '{}'", log_path.display()))?;
    let mut child = Command::new("/bin/sh");
    child
        .arg("-c")
        .arg(DELEGATED_LAUNCH_GATE_SCRIPT)
        .arg("packet28-delegated-launch-gate")
        .args(&request.command)
        .stdin(std::process::Stdio::piped())
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
        .env("PACKET28_ROOT", &root)
        .process_group(0);
    let mut child = child
        .spawn()
        .with_context(|| format!("failed to spawn delegated command '{}'", request.command[0]))?;
    let Some(gate_writer) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("delegated child launch gate was not created");
    };
    let launch_gate = DelegatedLaunchGate {
        writer: gate_writer,
    };
    let pid = child.id();
    let process_group = match i32::try_from(pid) {
        Ok(process_group) => process_group,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "delegated child pid {pid} does not fit in a process-group id: {error}"
            ));
        }
    };
    let owned_process = OwnedChildProcess { pid, process_group };
    if !generation.register_child(owned_process) {
        terminate_and_reap_child(&mut child, owned_process)?;
        anyhow::bail!(
            "task '{}' was cancelled while launching delegated agent",
            bootstrap.task_id
        );
    }
    let ownership_result = (|| -> Result<()> {
        let (persistence, revision) = {
            let mut guard = state.lock().map_err(lock_err)?;
            if !guard
                .task_generations
                .matches(&bootstrap.task_id, generation.id())
                || generation.is_cancelled()
            {
                anyhow::bail!(
                    "task '{}' generation changed while launching delegated agent",
                    bootstrap.task_id
                );
            }
            guard.require_task_mutable(&bootstrap.task_id)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(&bootstrap.task_id)
                .ok_or_else(|| {
                    anyhow!(
                        "task '{}' disappeared while launching delegated agent",
                        bootstrap.task_id
                    )
                })?;
            task.latest_agent_pid = Some(pid);
            task.latest_agent_bootstrap_mode = Some(bootstrap.mode.to_string());
            task.latest_agent_log_path = Some(log_path.to_string_lossy().to_string());
            task.latest_agent_started_at_unix = Some(started_at_unix);
            task.latest_agent_completed_at_unix = None;
            task.latest_agent_exit_code = None;
            task.latest_agent_context_version = Some(bootstrap.response.context_version.clone());
            task.latest_agent_handoff_artifact_id = bootstrap.handoff_artifact_id.clone();
            task.latest_agent_handoff_checkpoint_id = bootstrap.handoff_checkpoint_id.clone();
            let revision = mark_task_dirty(&guard, &bootstrap.task_id)?;
            (guard.persistence.clone(), revision)
        };
        persistence.flush_through(revision)
    })();
    if let Err(error) = ownership_result {
        let reap_result = terminate_and_reap_child(&mut child, owned_process);
        generation.complete_child(pid);
        reap_result?;
        return Err(error);
    }
    let started_event = emit_task_event_for_generation(
        state.clone(),
        &bootstrap.task_id,
        generation.id(),
        "task.agent_launch_started",
        json!({
            "summary": format!("spawned delegated agent pid={pid} mode={}", bootstrap.mode),
            "pid": pid,
            "bootstrap_mode": bootstrap.mode,
            "log_path": log_path.to_string_lossy().to_string(),
        }),
    );
    match started_event {
        Ok(true) => {}
        Ok(false) => {
            terminate_and_reap_child(&mut child, owned_process)?;
            generation.complete_child(pid);
            anyhow::bail!(
                "task '{}' generation changed before delegated agent launch was recorded",
                bootstrap.task_id
            );
        }
        Err(error) => {
            let reap_result = terminate_and_reap_child(&mut child, owned_process);
            generation.complete_child(pid);
            reap_result?;
            return Err(error);
        }
    }
    if let Err(error) = launch_gate.release() {
        let reap_result = terminate_and_reap_child(&mut child, owned_process);
        generation.complete_child(pid);
        reap_result?;
        return Err(error);
    }
    if let Err(error) = spawn_owned_child_waiter(
        state.clone(),
        bootstrap.task_id.clone(),
        generation.clone(),
        owned_process,
        child,
    ) {
        if let Ok(mut guard) = state.lock().map_err(lock_err) {
            if guard
                .task_generations
                .matches(&bootstrap.task_id, generation.id())
                && !generation.is_cancelled()
            {
                let mutable = guard.require_task_mutable(&bootstrap.task_id).is_ok();
                if let Some(task) = guard
                    .tasks
                    .tasks
                    .get_mut(&bootstrap.task_id)
                    .filter(|_| mutable)
                {
                    if task.latest_agent_pid == Some(pid) {
                        task.latest_agent_completed_at_unix = Some(now_unix());
                        task.latest_agent_exit_code = None;
                        task.last_error = Some(error.to_string());
                        let _ = persist_task(&guard, &bootstrap.task_id);
                    }
                }
            }
        }
        return Err(error);
    }
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
