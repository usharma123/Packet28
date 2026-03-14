use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use blake3::Hasher;
use packet28_daemon_core::{
    BrokerPrepareHandoffRequest, BrokerPrepareHandoffResponse, BrokerTaskStatusRequest,
    BrokerTaskStatusResponse, BrokerWriteOp, BrokerWriteStateRequest, BrokerWriteStateResponse,
    DaemonRequest, DaemonResponse, HookIngestRequest, HookIngestResponse, TaskAwaitHandoffRequest,
    TaskAwaitHandoffResponse,
};

pub fn resolve_root(root: &str) -> PathBuf {
    crate::cmd_daemon::resolve_root_arg(root)
}

pub fn ensure_daemon(root: &Path) -> Result<()> {
    crate::cmd_daemon::ensure_daemon(root)
}

pub fn derive_task_id(task: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(task.trim().as_bytes());
    format!("task-{}", &hasher.finalize().to_hex().to_string()[..12])
}

pub fn prepare_handoff(
    root: &Path,
    request: BrokerPrepareHandoffRequest,
) -> Result<BrokerPrepareHandoffResponse> {
    if request.task_id.trim().is_empty() {
        return Err(anyhow!("broker prepare_handoff requires task_id"));
    }
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerPrepareHandoff { request })? {
        DaemonResponse::BrokerPrepareHandoff { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn write_state(
    root: &Path,
    request: BrokerWriteStateRequest,
) -> Result<BrokerWriteStateResponse> {
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerWriteState { request })? {
        DaemonResponse::BrokerWriteState { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn write_intention(
    root: &Path,
    mut request: BrokerWriteStateRequest,
) -> Result<BrokerWriteStateResponse> {
    request.op = Some(BrokerWriteOp::Intention);
    write_state(root, request)
}

pub fn task_status(root: &Path, task_id: &str) -> Result<BrokerTaskStatusResponse> {
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(
        root,
        &DaemonRequest::BrokerTaskStatus {
            request: BrokerTaskStatusRequest {
                task_id: task_id.to_string(),
            },
        },
    )? {
        DaemonResponse::BrokerTaskStatus { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn hook_ingest(root: &Path, request: HookIngestRequest) -> Result<HookIngestResponse> {
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::HookIngest { request })? {
        DaemonResponse::HookIngest { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn await_handoff(
    root: &Path,
    request: TaskAwaitHandoffRequest,
) -> Result<TaskAwaitHandoffResponse> {
    if request.task_id.trim().is_empty() {
        return Err(anyhow!("daemon task await-handoff requires task_id"));
    }
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::TaskAwaitHandoff { request })? {
        DaemonResponse::TaskAwaitHandoff { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}
