use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::{
    log_path, read_runtime_info, read_socket_message, ready_path, resolve_workspace_root,
    socket_path, write_socket_message, ContextRecallRequest, ContextRecallResponse,
    ContextStoreGetRequest, ContextStoreGetResponse, ContextStoreListRequest,
    ContextStoreListResponse, ContextStorePruneDaemonRequest, ContextStorePruneResponse,
    ContextStoreStatsRequest, ContextStoreStatsResponse, CoverCheckRequest, CoverCheckResponse,
    DaemonRequest, DaemonResponse, PacketFetchRequest, PacketFetchResponse, TaskSubmitSpec,
    TestMapRequest, TestMapResponse, TestShardRequest, TestShardResponse,
};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
const DAEMON_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(unix)]
pub struct PersistentDaemonClient {
    root: PathBuf,
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

pub fn via_daemon_env_enabled() -> bool {
    crate::cmd_common::parse_daemon_env_flag(std::env::var("PACKET28_VIA_DAEMON").ok().as_deref())
}

pub fn daemon_root_env() -> Option<String> {
    std::env::var("PACKET28_DAEMON_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn daemon_workspace_root(explicit_root: Option<&str>) -> Result<PathBuf> {
    let start = if let Some(root) = explicit_root {
        PathBuf::from(root)
    } else if let Some(root) = daemon_root_env() {
        PathBuf::from(root)
    } else {
        std::env::current_dir().context("failed to resolve current directory")?
    };
    Ok(resolve_workspace_root(&start))
}

fn normalize_daemon_root(root: &Path) -> PathBuf {
    resolve_workspace_root(root)
}

#[cfg(not(unix))]
pub(crate) fn daemon_not_supported<T>() -> Result<T> {
    Err(anyhow!(
        "packet28 daemon commands are only supported on Unix targets"
    ))
}

pub fn execute_kernel_request(
    root: &Path,
    request: context_kernel_core::KernelRequest,
) -> Result<context_kernel_core::KernelResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::Execute { request })? {
        DaemonResponse::Execute { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn send_kernel_request(
    root: &Path,
    request: context_kernel_core::KernelRequest,
) -> Result<context_kernel_core::KernelResponse> {
    execute_kernel_request(root, request)
}

pub fn execute_sequence(
    root: &Path,
    spec: TaskSubmitSpec,
) -> Result<packet28_daemon_core::SequenceSubmitResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ExecuteSequence { spec })? {
        DaemonResponse::ExecuteSequence {
            response,
            task,
            watches,
        } => Ok(packet28_daemon_core::SequenceSubmitResponse {
            task_id: task.task_id,
            watch_ids: watches.iter().map(|watch| watch.watch_id.clone()).collect(),
            response,
        }),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_cover_check(root: &Path, request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::CoverCheck { request })? {
        DaemonResponse::CoverCheck { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_packet_fetch(
    root: &Path,
    request: PacketFetchRequest,
) -> Result<PacketFetchResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::PacketFetch { request })? {
        DaemonResponse::PacketFetch { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn send_cover_check(root: &Path, request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    execute_cover_check(root, request)
}

pub fn send_packet_fetch(root: &Path, request: PacketFetchRequest) -> Result<PacketFetchResponse> {
    execute_packet_fetch(root, request)
}

pub fn execute_test_shard(root: &Path, request: TestShardRequest) -> Result<TestShardResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::TestShard { request })? {
        DaemonResponse::TestShard { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_test_map(root: &Path, request: TestMapRequest) -> Result<TestMapResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::TestMap { request })? {
        DaemonResponse::TestMap { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_list(
    root: &Path,
    request: ContextStoreListRequest,
) -> Result<ContextStoreListResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreList { request })? {
        DaemonResponse::ContextStoreList { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_get(
    root: &Path,
    request: ContextStoreGetRequest,
) -> Result<ContextStoreGetResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreGet { request })? {
        DaemonResponse::ContextStoreGet { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_prune(
    root: &Path,
    request: ContextStorePruneDaemonRequest,
) -> Result<ContextStorePruneResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStorePrune { request })? {
        DaemonResponse::ContextStorePrune { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_stats(
    root: &Path,
    request: ContextStoreStatsRequest,
) -> Result<ContextStoreStatsResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreStats { request })? {
        DaemonResponse::ContextStoreStats { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_recall(
    root: &Path,
    request: ContextRecallRequest,
) -> Result<ContextRecallResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextRecall { request })? {
        DaemonResponse::ContextRecall { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(unix)]
pub fn send_request(root: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let root = normalize_daemon_root(root);
    ensure_daemon(&root)?;
    let response = send_request_existing_daemon(&root, request)?;
    if daemon_response_indicates_protocol_mismatch(&response) {
        restart_daemon(&root)?;
        return send_request_existing_daemon(&root, request);
    }
    Ok(response)
}

#[cfg(unix)]
pub(crate) fn subscribe_task(
    root: &Path,
    task_id: &str,
    replay_last: usize,
) -> Result<(UnixStream, usize)> {
    let socket = socket_path(root);
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to daemon socket '{}'", socket.display()))?;
    stream
        .set_read_timeout(None)
        .context("failed to configure subscribe read timeout")?;
    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut reader = BufReader::new(stream.try_clone()?);
    write_socket_message(
        &mut writer,
        &DaemonRequest::TaskSubscribe {
            task_id: task_id.to_string(),
            replay_last,
        },
    )?;
    match read_socket_message(&mut reader)? {
        DaemonResponse::TaskSubscribeAck { replayed, .. } => Ok((stream, replayed)),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
pub fn send_request(_root: &Path, _request: &DaemonRequest) -> Result<DaemonResponse> {
    daemon_not_supported()
}

#[cfg(unix)]
impl PersistentDaemonClient {
    pub fn connect(root: &Path) -> Result<Self> {
        let root = normalize_daemon_root(root);
        ensure_daemon(&root)?;
        let socket = socket_path(&root);
        let stream = connect_daemon_socket(&socket)?;
        let reader_stream = stream.try_clone()?;
        Ok(Self {
            root,
            reader: BufReader::new(reader_stream),
            writer: BufWriter::new(stream),
        })
    }

    pub fn send_request(&mut self, request: &DaemonRequest) -> Result<DaemonResponse> {
        write_socket_message(&mut self.writer, request)?;
        read_socket_message(&mut self.reader)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(unix)]
pub(crate) fn ensure_daemon(root: &Path) -> Result<()> {
    let root = normalize_daemon_root(root);
    if daemon_status_existing(&root).is_ok() {
        return Ok(());
    }
    if socket_path(&root).exists() && connect_daemon_socket(&socket_path(&root)).is_err() {
        cleanup_unreachable_runtime_files(&root)?;
    }
    start_daemon(&root)?;
    wait_for_daemon(&root, Duration::from_secs(10))
}

#[cfg(not(unix))]
pub(crate) fn ensure_daemon(_root: &Path) -> Result<()> {
    daemon_not_supported()
}

pub(crate) fn resolve_root_arg(root: &str) -> PathBuf {
    let cwd = PathBuf::from(root);
    resolve_workspace_root(&cwd)
}

#[cfg(unix)]
fn start_daemon(root: &Path) -> Result<()> {
    let binary = packet28d_binary()?;
    let root_arg = root.to_string_lossy().to_string();
    let log_path = log_path(root);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create daemon log dir '{}'", parent.display()))?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
    Command::new(binary)
        .arg("serve")
        .arg("--root")
        .arg(root_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn packet28d")?;
    Ok(())
}

#[cfg(unix)]
fn wait_for_daemon(root: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if daemon_status_existing(root).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Ok(runtime) = read_runtime_info(root) {
        return Err(anyhow!(
            "packet28d did not become ready; runtime file exists for pid {} at {} (log: {})",
            runtime.pid,
            runtime.socket_path,
            runtime.log_path
        ));
    }
    Err(anyhow!("packet28d did not become ready"))
}

#[cfg(unix)]
pub(crate) fn restart_daemon(root: &Path) -> Result<()> {
    let root = normalize_daemon_root(root);
    stop_daemon_if_running(&root)?;
    wait_for_daemon_shutdown(&root, Duration::from_secs(5))?;
    cleanup_unreachable_runtime_files(&root)?;
    start_daemon(&root)?;
    wait_for_daemon(&root, Duration::from_secs(10))
}

#[cfg(unix)]
fn daemon_response_indicates_protocol_mismatch(response: &DaemonResponse) -> bool {
    matches!(
        response,
        DaemonResponse::Error { message } if daemon_error_indicates_protocol_mismatch(message)
    )
}

#[cfg(unix)]
fn daemon_error_indicates_protocol_mismatch(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unknown variant") && lower.contains("expected one of")
}

#[cfg(unix)]
fn send_request_existing_daemon(root: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let socket = socket_path(root);
    let stream = connect_daemon_socket(&socket)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_socket_message(&mut writer, request)?;
    read_socket_message(&mut reader)
}

#[cfg(unix)]
fn daemon_status_existing(root: &Path) -> Result<packet28_daemon_core::DaemonStatus> {
    match send_request_existing_daemon(root, &DaemonRequest::Status) {
        Ok(DaemonResponse::Status { status }) => Ok(status),
        Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!("unexpected daemon status response: {other:?}")),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn connect_daemon_socket(socket: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to '{}'", socket.display()))?;
    stream
        .set_read_timeout(Some(DAEMON_SOCKET_TIMEOUT))
        .with_context(|| {
            format!(
                "failed to configure read timeout for '{}'",
                socket.display()
            )
        })?;
    stream
        .set_write_timeout(Some(DAEMON_SOCKET_TIMEOUT))
        .with_context(|| {
            format!(
                "failed to configure write timeout for '{}'",
                socket.display()
            )
        })?;
    Ok(stream)
}

#[cfg(unix)]
fn stop_daemon_if_running(root: &Path) -> Result<()> {
    let socket = socket_path(root);
    if !socket.exists() {
        return Ok(());
    }
    match send_request_existing_daemon(root, &DaemonRequest::Stop) {
        Ok(_) => Ok(()),
        Err(err) => {
            if socket.exists() && UnixStream::connect(&socket).is_ok() {
                Err(err)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn cleanup_unreachable_runtime_files(root: &Path) -> Result<()> {
    for path in [socket_path(root), ready_path(root)] {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale runtime file '{}'", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_daemon_shutdown(root: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let socket = socket_path(root);
        if !socket.exists() || UnixStream::connect(&socket).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(anyhow!(
        "packet28d did not stop; socket still reachable at '{}'",
        socket_path(root).display()
    ))
}

#[cfg(unix)]
fn packet28d_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_packet28d") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let candidate = current
        .parent()
        .ok_or_else(|| anyhow!("missing executable parent"))?
        .join("packet28d");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(anyhow!(
        "could not locate packet28d next to '{}'",
        current.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mismatch_errors_are_detected() {
        assert!(daemon_error_indicates_protocol_mismatch(
            "unknown variant `hook_ingest`, expected one of `execute`, `status` at line 1 column 21"
        ));
    }

    #[test]
    fn normal_daemon_errors_do_not_trigger_protocol_restart() {
        let response = DaemonResponse::Error {
            message: "prepare_handoff did not return a ready handoff".to_string(),
        };
        assert!(!daemon_response_indicates_protocol_mismatch(&response));
    }
}
