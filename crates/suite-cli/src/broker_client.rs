use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use blake3::Hasher;
use packet28_daemon_core::{
    BrokerAction, BrokerDecomposeRequest, BrokerDecomposeResponse, BrokerEstimateContextRequest,
    BrokerEstimateContextResponse, BrokerGetContextRequest, BrokerGetContextResponse,
    BrokerTaskStatusRequest, BrokerTaskStatusResponse, BrokerValidatePlanRequest,
    BrokerValidatePlanResponse, BrokerWriteStateRequest, BrokerWriteStateResponse, DaemonRequest,
    DaemonResponse,
};

pub const DEFAULT_BROKER_BUDGET_TOKENS: u64 = 5_000;
pub const DEFAULT_BROKER_BUDGET_BYTES: usize = 32_000;

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

pub fn get_context(
    root: &Path,
    mut request: BrokerGetContextRequest,
) -> Result<BrokerGetContextResponse> {
    if request.task_id.trim().is_empty() {
        return Err(anyhow!("broker get_context requires task_id"));
    }
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(DEFAULT_BROKER_BUDGET_TOKENS);
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(DEFAULT_BROKER_BUDGET_BYTES);
    }
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerGetContext { request })? {
        DaemonResponse::BrokerGetContext { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn estimate_context(
    root: &Path,
    mut request: BrokerEstimateContextRequest,
) -> Result<BrokerEstimateContextResponse> {
    if request.task_id.trim().is_empty() {
        return Err(anyhow!("broker estimate_context requires task_id"));
    }
    if request.action.is_none() {
        request.action = Some(BrokerAction::Plan);
    }
    if request.budget_tokens.is_none() {
        request.budget_tokens = Some(DEFAULT_BROKER_BUDGET_TOKENS);
    }
    if request.budget_bytes.is_none() {
        request.budget_bytes = Some(DEFAULT_BROKER_BUDGET_BYTES);
    }
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerEstimateContext { request })?
    {
        DaemonResponse::BrokerEstimateContext { response } => Ok(response),
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

pub fn validate_plan(
    root: &Path,
    request: BrokerValidatePlanRequest,
) -> Result<BrokerValidatePlanResponse> {
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerValidatePlan { request })? {
        DaemonResponse::BrokerValidatePlan { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn decompose(root: &Path, request: BrokerDecomposeRequest) -> Result<BrokerDecomposeResponse> {
    ensure_daemon(root)?;
    match crate::cmd_daemon::send_request(root, &DaemonRequest::BrokerDecompose { request })? {
        DaemonResponse::BrokerDecompose { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
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
