use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
#[cfg(unix)]
use packet28_daemon_client::transport::{DaemonEndpoint, DaemonStream};
use packet28_daemon_core::storage::read_runtime_info;
#[cfg(unix)]
use packet28_daemon_core::task_store_lease::acquire_daemon_startup_lease;
use packet28_daemon_protocol::{
    commands::{
        CoverCheckRequest, CoverCheckResponse, PacketFetchRequest, PacketFetchResponse,
        SequenceSubmitResponse, TaskSubmitSpec, TestMapRequest, TestMapResponse, TestShardRequest,
        TestShardResponse,
    },
    context_store::{
        ContextRecallRequest, ContextRecallResponse, ContextStoreGetRequest,
        ContextStoreGetResponse, ContextStoreListRequest, ContextStoreListResponse,
        ContextStorePruneDaemonRequest, ContextStorePruneResponse, ContextStoreStatsRequest,
        ContextStoreStatsResponse,
    },
    frame::{read_frame, write_frame},
    message::{ContextResolveRequest, ContextResolveResponse, DaemonRequest, DaemonResponse},
    paths::{log_path, ready_path, resolve_workspace_root, socket_path, workspace_socket_path},
    registry::{DaemonRegistryRequestV1, DaemonRegistryResponseV1, DaemonStatusV1},
};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
    reader: BufReader<DaemonStream>,
    writer: BufWriter<DaemonStream>,
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

pub fn execute_sequence(root: &Path, spec: TaskSubmitSpec) -> Result<SequenceSubmitResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ExecuteSequence { spec })? {
        DaemonResponse::ExecuteSequence {
            response,
            task,
            watches,
        } => Ok(SequenceSubmitResponse {
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

pub fn execute_context_resolve(
    root: &Path,
    request: ContextResolveRequest,
) -> Result<ContextResolveResponse> {
    match send_request(root, &DaemonRequest::ContextResolve { request })? {
        DaemonResponse::ContextResolve { response } => Ok(response),
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
    after_seq: Option<u64>,
) -> Result<(DaemonStream, usize)> {
    let endpoint = daemon_endpoint(root)?;
    let stream = connect_daemon_endpoint(&endpoint)?;
    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut reader = BufReader::new(stream.try_clone()?);
    write_frame(
        &mut writer,
        &DaemonRequest::TaskSubscribe {
            task_id: task_id.to_string(),
            replay_last,
            after_seq,
        },
    )?;
    match read_frame(&mut reader)? {
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
pub(crate) fn send_request_without_start(
    root: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse> {
    let root = normalize_daemon_root(root);
    send_request_existing_daemon(&root, request)
}

#[cfg(not(unix))]
pub(crate) fn send_request_without_start(
    _root: &Path,
    _request: &DaemonRequest,
) -> Result<DaemonResponse> {
    daemon_not_supported()
}

#[cfg(unix)]
impl PersistentDaemonClient {
    pub fn connect(root: &Path) -> Result<Self> {
        let root = normalize_daemon_root(root);
        ensure_daemon(&root)?;
        let endpoint = daemon_endpoint(&root)?;
        let stream = connect_daemon_endpoint(&endpoint)?;
        let reader_stream = stream.try_clone()?;
        Ok(Self {
            root,
            reader: BufReader::new(reader_stream),
            writer: BufWriter::new(stream),
        })
    }

    pub fn send_request(&mut self, request: &DaemonRequest) -> Result<DaemonResponse> {
        write_frame(&mut self.writer, request)?;
        Ok(read_frame(&mut self.reader)?)
    }

    pub fn send_registry_request(
        &mut self,
        request: &DaemonRegistryRequestV1,
    ) -> Result<DaemonRegistryResponseV1> {
        write_frame(&mut self.writer, request)?;
        Ok(read_frame(&mut self.reader)?)
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
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    if daemon_status_existing(&root).is_ok() {
        return Ok(());
    }
    let endpoint = daemon_endpoint(&root)?;
    if endpoint_may_have_stale_socket(&endpoint) && connect_daemon_endpoint(&endpoint).is_err() {
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

/// Default size ceiling for `packet28d.log` before it is rotated on the next
/// daemon start. Kept intentionally modest so a crash-loop cannot balloon the
/// active log into the gigabytes observed in long-lived workspaces.
#[cfg(unix)]
const DAEMON_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Number of rotated `packet28d.log.N` generations retained. Total on-disk log
/// footprint is bounded by roughly `(DAEMON_LOG_MAX_BACKUPS + 1) * max_bytes`.
#[cfg(unix)]
const DAEMON_LOG_MAX_BACKUPS: usize = 3;

/// Environment override for the rotation threshold, in bytes. A non-positive or
/// unparsable value falls back to [`DAEMON_LOG_MAX_BYTES`].
#[cfg(unix)]
const DAEMON_LOG_MAX_BYTES_ENV: &str = "PACKET28_DAEMON_LOG_MAX_BYTES";

#[cfg(unix)]
fn daemon_log_max_bytes() -> u64 {
    std::env::var(DAEMON_LOG_MAX_BYTES_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DAEMON_LOG_MAX_BYTES)
}

#[cfg(unix)]
fn daemon_log_backup_path(log_path: &Path, index: usize) -> PathBuf {
    let mut name = log_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    log_path.with_file_name(name)
}

/// Rotates `log_path` when it has grown past `max_bytes`, keeping up to
/// `max_backups` numbered generations (`packet28d.log.1` .. `.max_backups`).
///
/// This runs on every daemon (re)start. A healthy daemon logs sparsely, but a
/// crash-loop restarts repeatedly and each restart previously reopened the same
/// file in append mode, which is how a multi-gigabyte `packet28d.log` was
/// observed. Rotation is best-effort: any filesystem error is ignored so a
/// rotation problem can never block the daemon from starting.
#[cfg(unix)]
fn rotate_daemon_log_if_needed(log_path: &Path, max_bytes: u64, max_backups: usize) {
    if max_bytes == 0 || max_backups == 0 {
        return;
    }
    let Ok(metadata) = std::fs::metadata(log_path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() < max_bytes {
        return;
    }
    // Drop the oldest generation, shift the remaining backups up by one, then
    // move the active log into the first backup slot so the daemon starts fresh.
    let _ = std::fs::remove_file(daemon_log_backup_path(log_path, max_backups));
    for index in (1..max_backups).rev() {
        let from = daemon_log_backup_path(log_path, index);
        let to = daemon_log_backup_path(log_path, index + 1);
        let _ = std::fs::rename(&from, &to);
    }
    let _ = std::fs::rename(log_path, daemon_log_backup_path(log_path, 1));
}

#[cfg(unix)]
fn start_daemon(root: &Path) -> Result<()> {
    let binary = packet28d_binary()?;
    ensure_executable(&binary)?;
    let root_arg = root.to_string_lossy().to_string();
    let log_path = log_path(root);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create daemon log dir '{}'", parent.display()))?;
    }
    rotate_daemon_log_if_needed(&log_path, daemon_log_max_bytes(), DAEMON_LOG_MAX_BACKUPS);
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
    let mut child = Command::new(binary)
        .arg("serve")
        .arg("--root")
        .arg(root_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn packet28d")?;
    let pid = child.id();
    thread::Builder::new()
        .name(format!("packet28d-reaper-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        })
        .context("failed to start packet28d child reaper")?;
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

/// Stop the workspace daemon if it is running and wait for its socket to go
/// away. Returns `Ok(true)` when a daemon was reachable and asked to stop.
#[cfg(unix)]
pub(crate) fn stop_daemon_and_wait(root: &Path) -> Result<bool> {
    let root = normalize_daemon_root(root);
    let was_running = daemon_status_existing(&root).is_ok();
    stop_daemon_if_running(&root)?;
    wait_for_daemon_shutdown(&root, Duration::from_secs(5))?;
    cleanup_unreachable_runtime_files(&root)?;
    Ok(was_running)
}

#[cfg(not(unix))]
pub(crate) fn stop_daemon_and_wait(_root: &Path) -> Result<bool> {
    Ok(false)
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
    let endpoint = daemon_endpoint(root)?;
    let stream = connect_daemon_endpoint(&endpoint)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(&mut writer, request)?;
    Ok(read_frame(&mut reader)?)
}

#[cfg(unix)]
fn send_registry_request_existing_daemon(
    root: &Path,
    request: &DaemonRegistryRequestV1,
) -> Result<DaemonRegistryResponseV1> {
    let endpoint = daemon_endpoint(root)?;
    let stream = connect_daemon_endpoint(&endpoint)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(&mut writer, request)?;
    Ok(read_frame(&mut reader)?)
}

#[cfg(unix)]
fn daemon_status_existing(root: &Path) -> Result<DaemonStatusV1> {
    match send_registry_request_existing_daemon(root, &DaemonRegistryRequestV1::Status) {
        Ok(DaemonRegistryResponseV1::Status { status }) => Ok(*status),
        Ok(DaemonRegistryResponseV1::Error { message })
            if daemon_error_indicates_protocol_mismatch(&message) =>
        {
            legacy_daemon_status_existing(root)
        }
        Ok(DaemonRegistryResponseV1::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!(
            "unexpected daemon registry status response: {other:?}"
        )),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn legacy_daemon_status_existing(root: &Path) -> Result<DaemonStatusV1> {
    match send_request_existing_daemon(root, &DaemonRequest::Status) {
        Ok(DaemonResponse::Status { status }) => Ok(DaemonStatusV1::from_legacy(status)),
        Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!(
            "unexpected legacy daemon status response: {other:?}"
        )),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
pub(crate) fn daemon_status_v1(root: &Path) -> Result<DaemonStatusV1> {
    let root = normalize_daemon_root(root);
    ensure_daemon(&root)?;
    daemon_status_existing(&root)
}

#[cfg(not(unix))]
pub(crate) fn daemon_status_v1(_root: &Path) -> Result<DaemonStatusV1> {
    daemon_not_supported()
}

#[cfg(unix)]
fn stop_daemon_if_running(root: &Path) -> Result<()> {
    let endpoint = daemon_endpoint(root)?;
    if !endpoint_may_have_stale_socket(&endpoint) {
        return Ok(());
    }
    match send_request_existing_daemon(root, &DaemonRequest::Stop) {
        Ok(_) => Ok(()),
        Err(err) => {
            if connect_daemon_endpoint(&endpoint).is_ok() {
                Err(err)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn cleanup_unreachable_runtime_files(root: &Path) -> Result<()> {
    for path in [
        socket_path(root),
        workspace_socket_path(root),
        ready_path(root),
    ] {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| {
                format!("failed to remove stale runtime file '{}'", path.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_daemon_shutdown(root: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let endpoint = daemon_endpoint(root)?;
        if !endpoint_may_have_stale_socket(&endpoint) || connect_daemon_endpoint(&endpoint).is_err()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(anyhow!(
        "packet28d did not stop; socket still reachable at '{}'",
        daemon_endpoint(root)?.address()
    ))
}

#[cfg(unix)]
fn daemon_endpoint(root: &Path) -> Result<DaemonEndpoint> {
    Ok(packet28_daemon_client::transport::discover_endpoint(root)?)
}

#[cfg(unix)]
fn endpoint_may_have_stale_socket(endpoint: &DaemonEndpoint) -> bool {
    packet28_daemon_client::transport::endpoint_may_have_stale_socket(endpoint)
}

#[cfg(unix)]
fn connect_daemon_endpoint(endpoint: &DaemonEndpoint) -> Result<DaemonStream> {
    Ok(packet28_daemon_client::transport::connect_endpoint(
        endpoint,
        DAEMON_SOCKET_TIMEOUT,
    )?)
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

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect packet28d binary '{}'", path.display()))?;
    let mode = metadata.permissions().mode();
    if mode & 0o111 != 0 {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(mode | 0o755);
    std::fs::set_permissions(path, permissions).with_context(|| {
        format!(
            "packet28d binary '{}' is not executable and could not be repaired",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use packet28_daemon_protocol::paths::runtime_path;
    #[cfg(unix)]
    use std::io::Write as _;

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

    #[cfg(unix)]
    #[test]
    fn legacy_tcp_runtime_without_owner_capability_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        packet28_daemon_core::storage::write_runtime_info(
            root.path(),
            &packet28_daemon_protocol::message::DaemonRuntimeInfo {
                socket_path: "tcp://127.0.0.1:4242".to_string(),
                ..packet28_daemon_protocol::message::DaemonRuntimeInfo::default()
            },
        )
        .unwrap();

        let error = daemon_endpoint(root.path())
            .expect_err("legacy unauthenticated TCP discovery unexpectedly succeeded");

        assert!(error
            .to_string()
            .contains("refusing legacy unauthenticated daemon TCP endpoint"));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_symlink_is_not_treated_as_missing() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let runtime = runtime_path(root.path());
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        symlink(root.path().join("missing-runtime-target"), &runtime).unwrap();

        let error = daemon_endpoint(root.path()).expect_err(
            "unauthenticated runtime symlink unexpectedly fell back to a Unix endpoint",
        );

        assert!(error
            .to_string()
            .contains("failed to read authenticated daemon runtime metadata"));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_rotates_only_past_threshold_and_shifts_backups() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("packet28d.log");

        // A small log is left untouched.
        std::fs::write(&log, b"small").unwrap();
        rotate_daemon_log_if_needed(&log, 1024, 3);
        assert!(log.exists());
        assert!(!daemon_log_backup_path(&log, 1).exists());

        // Crossing the threshold moves the active log into `.1`.
        std::fs::write(&log, vec![b'x'; 2048]).unwrap();
        rotate_daemon_log_if_needed(&log, 1024, 3);
        assert!(!log.exists(), "active log should be rotated away");
        assert_eq!(
            std::fs::read(daemon_log_backup_path(&log, 1))
                .unwrap()
                .len(),
            2048
        );

        // A second rotation shifts `.1` -> `.2` and installs the new `.1`.
        std::fs::write(&log, vec![b'y'; 2048]).unwrap();
        rotate_daemon_log_if_needed(&log, 1024, 3);
        assert_eq!(
            std::fs::read(daemon_log_backup_path(&log, 1)).unwrap(),
            vec![b'y'; 2048]
        );
        assert_eq!(
            std::fs::read(daemon_log_backup_path(&log, 2)).unwrap(),
            vec![b'x'; 2048]
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_rotation_bounds_backup_count() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("packet28d.log");
        for _ in 0..5 {
            std::fs::write(&log, vec![b'z'; 2048]).unwrap();
            rotate_daemon_log_if_needed(&log, 1024, 2);
        }
        assert!(daemon_log_backup_path(&log, 1).exists());
        assert!(daemon_log_backup_path(&log, 2).exists());
        assert!(
            !daemon_log_backup_path(&log, 3).exists(),
            "backups beyond max_backups must be pruned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_max_bytes_env_override_is_respected() {
        let default = super::DAEMON_LOG_MAX_BYTES;
        std::env::set_var(super::DAEMON_LOG_MAX_BYTES_ENV, "4096");
        assert_eq!(daemon_log_max_bytes(), 4096);
        std::env::set_var(super::DAEMON_LOG_MAX_BYTES_ENV, "not-a-number");
        assert_eq!(daemon_log_max_bytes(), default);
        std::env::remove_var(super::DAEMON_LOG_MAX_BYTES_ENV);
        assert_eq!(daemon_log_max_bytes(), default);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_repairs_packaged_daemon_mode() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("packet28d");
        std::fs::File::create(&daemon)
            .unwrap()
            .write_all(b"#!/bin/sh\n")
            .unwrap();
        let mut permissions = std::fs::metadata(&daemon).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&daemon, permissions).unwrap();

        ensure_executable(&daemon).unwrap();

        let mode = std::fs::metadata(&daemon).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0);
    }
}
