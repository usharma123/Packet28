use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
#[cfg(unix)]
use packet28_daemon_client::runtime_discovery::read_runtime_info_if_present;
#[cfg(unix)]
use packet28_daemon_client::transport::{
    endpoint_accepts_connections, request_status_v1, verify_runtime_workspace,
    workspace_root_matches, DaemonClientError, DaemonEndpoint, DaemonStream,
};
#[cfg(unix)]
use packet28_daemon_core::task_store_lease::{
    acquire_daemon_startup_lease, daemon_instance_lock_path,
};
#[cfg(unix)]
use packet28_daemon_protocol::message::DaemonRuntimeInfo;
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
    logging::{runtime_log_max_bytes, MANAGED_LOG_FLAG, RUNTIME_LOG_BACKUPS},
    message::{ContextResolveRequest, ContextResolveResponse, DaemonRequest, DaemonResponse},
    paths::{
        daemon_dir, log_path, ready_path, resolve_workspace_root, socket_path,
        workspace_socket_path,
    },
    registry::{DaemonRegistryRequestV1, DaemonRegistryResponseV1, DaemonStatusV1},
};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::process::{Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::mpsc;
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
    // A starting daemon accepts connections but answers only once ready, so
    // its status is awaited under the startup-readiness deadline below rather
    // than the unbounded socket timeout of this fast path. Metadata naming
    // another workspace is never contacted; the lease-guarded path below
    // fails closed on it unless this workspace's authority was released.
    if daemon_fast_path_allowed(&root)
        && daemon_status_existing(&root)
            .is_ok_and(|status| workspace_root_matches(&root, &status.workspace_root))
    {
        return Ok(());
    }
    // Discovery, stale-file cleanup, and bootstrap stay inside one startup
    // lease so a concurrent client cannot replace the runtime between the
    // authority probe and cleanup.
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    // An unreachable endpoint does not mean the previous daemon has exited: a
    // stopping daemon withdraws its endpoint before it finishes persistence and
    // cleanup. Leave its runtime files alone and do not spawn a replacement
    // until it releases the instance lease. A daemon that became ready, or is
    // still starting, while this client waited for the lease is reused.
    if wait_for_daemon_authority(&root, DAEMON_BOOTSTRAP_AUTHORITY_TIMEOUT)?
        == DaemonAuthority::Serving
    {
        return Ok(());
    }
    // Authority was released, so metadata naming another workspace is stale
    // and its endpoint is never contacted.
    if daemon_runtime_is_foreign(&root) {
        cleanup_unreachable_runtime_files(&root)?;
    } else {
        let endpoint = daemon_endpoint(&root)?;
        if endpoint_may_have_stale_socket(&endpoint) && connect_daemon_endpoint(&endpoint).is_err()
        {
            cleanup_unreachable_runtime_files(&root)?;
        }
    }
    let daemon = start_daemon(&root)?;
    wait_for_spawned_daemon(&root, &daemon)
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
fn daemon_log_backup_path(log_path: &Path, index: usize) -> PathBuf {
    let mut name = log_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    log_path.with_file_name(name)
}

/// Rotates `log_path` when it has grown past `max_bytes`, keeping up to
/// `max_backups` numbered generations (`packet28d.log.1` .. `.max_backups`).
///
/// This is the start-time fallback for a `packet28d` binary that predates
/// managed logs and therefore inherits an appended log file as stdout/stderr.
/// Such a daemon can still exceed the threshold before its next restart.
/// Rotation is best-effort: any filesystem error is ignored so a rotation
/// problem can never block the daemon from starting.
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

/// Returns whether `binary` accepts the managed-log flag on `serve`.
///
/// A daemon that owns its log rotates it while running; an older binary needs
/// the launcher to supply an appended log file instead.
#[cfg(unix)]
fn daemon_supports_managed_log(binary: &Path) -> bool {
    Command::new(binary)
        .args(["serve", "--help"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(MANAGED_LOG_FLAG)
        })
}

/// A daemon process spawned by this client, reaped by a background thread.
#[cfg(unix)]
struct SpawnedDaemon {
    pid: u32,
    exited: mpsc::Receiver<std::io::Result<ExitStatus>>,
}

#[cfg(unix)]
fn start_daemon(root: &Path) -> Result<SpawnedDaemon> {
    let binary = packet28d_binary()?;
    ensure_executable(&binary)?;
    let mut command = Command::new(&binary);
    command
        .arg("serve")
        .arg("--root")
        .arg(root.as_os_str())
        .stdin(Stdio::null());
    if daemon_supports_managed_log(&binary) {
        // The daemon owns, writes, and rotates its log; it never depends on
        // this launcher or on inherited descriptors staying alive.
        command
            .arg(MANAGED_LOG_FLAG)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else {
        let log_path = log_path(root);
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create daemon log dir '{}'", parent.display())
            })?;
        }
        rotate_daemon_log_if_needed(&log_path, runtime_log_max_bytes(), RUNTIME_LOG_BACKUPS);
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
    }
    let mut child = command.spawn().context("failed to spawn packet28d")?;
    let pid = child.id();
    let (sender, exited) = mpsc::channel();
    thread::Builder::new()
        .name(format!("packet28d-reaper-{pid}"))
        .spawn(move || {
            let _ = sender.send(child.wait());
        })
        .context("failed to start packet28d child reaper")?;
    Ok(SpawnedDaemon { pid, exited })
}

/// Bound for a starting daemon to answer status after it was spawned or
/// identified. It matches the connected status budget of
/// [`DAEMON_SOCKET_TIMEOUT`] that bootstrap effectively allowed before startup
/// readiness was separated from authority release; seeded 5,000-task debug
/// startup measured about 20 s.
#[cfg(unix)]
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Waits for the daemon this client spawned to answer status with its own
/// identity.
///
/// The instance lease is never probed here: a probe could briefly hold it
/// while the child tries to acquire it. On timeout the daemon is left running
/// to finish startup.
#[cfg(unix)]
fn wait_for_spawned_daemon(root: &Path, daemon: &SpawnedDaemon) -> Result<()> {
    let started = Instant::now();
    let deadline = started + DAEMON_STARTUP_TIMEOUT;
    let mut last_error = None;
    loop {
        match daemon.exited.try_recv() {
            Ok(status) => {
                let status = status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|error| format!("an unobservable status ({error})"));
                return Err(anyhow!(
                    "packet28d pid {} exited with {status} before becoming ready (startup \
                     readiness phase, elapsed {} ms; log: {})",
                    daemon.pid,
                    started.elapsed().as_millis(),
                    log_path(root).display()
                ));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(anyhow!("packet28d pid {} reaper stopped", daemon.pid))
            }
        }
        // Until the child publishes its own metadata, runtime files may belong
        // to the daemon that released authority; they are never trusted for
        // the child.
        match read_runtime_info_if_present(root) {
            Ok(Some(runtime)) if runtime.pid == daemon.pid => {
                verify_runtime_workspace(root, &runtime)?;
                let endpoint = DaemonEndpoint::from_runtime(root, &runtime)?;
                match request_status_v1(&endpoint, deadline) {
                    Ok(status) if same_daemon(root, &status, &runtime) => return Ok(()),
                    Ok(status) => {
                        last_error = Some(identity_mismatch(root, &status, &runtime));
                    }
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(daemon_startup_timeout(
                root, daemon.pid, started, last_error,
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reports whether authenticated runtime metadata names another workspace.
#[cfg(unix)]
fn daemon_runtime_is_foreign(root: &Path) -> bool {
    matches!(
        read_runtime_info_if_present(root),
        Ok(Some(runtime)) if !workspace_root_matches(root, &runtime.workspace_root)
    )
}

/// Reports whether the status fast path may contact the published endpoint:
/// not when authenticated runtime metadata names a daemon that has not
/// published readiness, or another workspace. Any read failure is left to the
/// lease-guarded path, which fails closed on it.
#[cfg(unix)]
fn daemon_fast_path_allowed(root: &Path) -> bool {
    match read_runtime_info_if_present(root) {
        Ok(Some(runtime)) => {
            runtime.ready_at_unix.is_some() && workspace_root_matches(root, &runtime.workspace_root)
        }
        Ok(None) | Err(_) => true,
    }
}

/// A daemon identified by authenticated runtime metadata that has not yet
/// published readiness.
#[cfg(unix)]
struct StartupCandidate {
    runtime: DaemonRuntimeInfo,
    endpoint: DaemonEndpoint,
}

/// What the current instance-lease owner's published state shows.
#[cfg(unix)]
enum DaemonOwner {
    Serving,
    Starting(Box<StartupCandidate>),
    Unavailable,
}

/// Reads runtime metadata while another daemon may own the workspace.
///
/// A stopping owner removes its metadata, and a read that races the removal
/// fails. The removal cannot fail a second read, so one retry tells it apart
/// from unauthentic or malformed metadata, which still fails closed.
#[cfg(unix)]
fn read_owner_runtime(root: &Path) -> Result<Option<DaemonRuntimeInfo>> {
    read_runtime_info_if_present(root)
        .or_else(|_| read_runtime_info_if_present(root))
        .context("failed to read packet28d runtime metadata while the daemon owns the workspace")
}

/// Classifies the instance-lease owner from authenticated runtime metadata.
///
/// Runtime metadata only selects what to wait for; it never authorizes
/// cleanup. Unauthentic or malformed metadata, or metadata naming another
/// workspace, fails closed before its endpoint is used.
#[cfg(unix)]
fn observe_daemon_owner(root: &Path, deadline: Instant) -> Result<DaemonOwner> {
    let Some(runtime) = read_owner_runtime(root)? else {
        return Ok(DaemonOwner::Unavailable);
    };
    verify_runtime_workspace(root, &runtime)?;
    let endpoint = DaemonEndpoint::from_runtime(root, &runtime)?;
    if runtime.ready_at_unix.is_none() {
        // A daemon binds its listener and publishes runtime metadata before it
        // loads durable state, then accepts requests once ready.
        if endpoint_accepts_connections(&endpoint)? {
            return Ok(DaemonOwner::Starting(Box::new(StartupCandidate {
                runtime,
                endpoint,
            })));
        }
        return Ok(DaemonOwner::Unavailable);
    }
    match request_status_v1(&endpoint, deadline) {
        Ok(status) if same_daemon(root, &status, &runtime) => Ok(DaemonOwner::Serving),
        // An older daemon answers the authenticated V1 request with a
        // protocol error; callers reach it through legacy requests.
        Err(DaemonClientError::StatusRejected { message, .. })
            if daemon_error_indicates_protocol_mismatch(&message) =>
        {
            Ok(DaemonOwner::Serving)
        }
        // A stopping daemon withdraws its endpoint before it releases
        // authority; keep waiting within the authority deadline.
        Ok(_) | Err(_) => Ok(DaemonOwner::Unavailable),
    }
}

/// Waits for an existing starting daemon to answer status with its identity.
///
/// Returns [`DaemonAuthority::Released`] if it releases the instance lease
/// first. On timeout the daemon is left running to finish startup.
#[cfg(unix)]
fn wait_for_existing_daemon_startup(
    root: &Path,
    candidate: &StartupCandidate,
) -> Result<DaemonAuthority> {
    let started = Instant::now();
    let deadline = started + DAEMON_STARTUP_TIMEOUT;
    loop {
        let last_error = match request_status_v1(&candidate.endpoint, deadline) {
            Ok(status) if same_daemon(root, &status, &candidate.runtime) => {
                return Ok(DaemonAuthority::Serving)
            }
            Ok(status) => identity_mismatch(root, &status, &candidate.runtime),
            Err(error) => error.to_string(),
        };
        if daemon_instance_released(root)? {
            return Ok(DaemonAuthority::Released);
        }
        if Instant::now() >= deadline {
            return Err(daemon_startup_timeout(
                root,
                candidate.runtime.pid,
                started,
                Some(last_error),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether status comes from the published daemon serving `root`.
#[cfg(unix)]
fn same_daemon(root: &Path, status: &DaemonStatusV1, runtime: &DaemonRuntimeInfo) -> bool {
    status.pid == runtime.pid
        && status.workspace_root == runtime.workspace_root
        && workspace_root_matches(root, &status.workspace_root)
}

#[cfg(unix)]
fn identity_mismatch(root: &Path, status: &DaemonStatusV1, runtime: &DaemonRuntimeInfo) -> String {
    format!(
        "status identity pid {} root '{}' does not match runtime pid {} root '{}' for \
         workspace '{}'",
        status.pid,
        status.workspace_root,
        runtime.pid,
        runtime.workspace_root,
        root.display()
    )
}

#[cfg(unix)]
fn daemon_startup_timeout(
    root: &Path,
    pid: u32,
    started: Instant,
    last_error: Option<String>,
) -> anyhow::Error {
    anyhow!(
        "packet28d pid {pid} did not become ready within {} ms (startup readiness phase, \
         elapsed {} ms); it was left running to finish startup (log: {}; last probe: {})",
        DAEMON_STARTUP_TIMEOUT.as_millis(),
        started.elapsed().as_millis(),
        log_path(root).display(),
        last_error.as_deref().unwrap_or("none")
    )
}

/// Bound for bootstrap to wait for a daemon that is stopping or held offline to
/// release workspace authority. It matches the client bootstrap bound that
/// predates authority waiting, so hooks and MCP clients keep their worst-case
/// latency; only explicit stop and restart use [`DAEMON_STOP_TIMEOUT`]. A
/// bootstrap that times out neither removes runtime files nor spawns.
#[cfg(unix)]
const DAEMON_BOOTSTRAP_AUTHORITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Default bound for a stopping daemon to release workspace authority. It
/// exceeds the daemon's default shutdown grace so normal persistence and
/// cleanup complete before a client gives up.
#[cfg(unix)]
const DAEMON_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Environment override for [`DAEMON_STOP_TIMEOUT`], in milliseconds. A
/// non-positive or unparsable value falls back to the default.
#[cfg(unix)]
const DAEMON_STOP_TIMEOUT_ENV: &str = "PACKET28_DAEMON_STOP_TIMEOUT_MS";

#[cfg(unix)]
fn daemon_stop_timeout() -> Duration {
    std::env::var(DAEMON_STOP_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DAEMON_STOP_TIMEOUT)
}

/// Stops the workspace daemon and waits until it releases workspace authority.
///
/// Returns the daemon's stop acknowledgement when one was reachable. The
/// startup lease is held from the stop request through stale-file cleanup, so
/// a concurrent client starts its replacement only after the stopping daemon
/// has released its instance lease and finished cleanup.
///
/// # Errors
///
/// Returns the daemon's stop error, an instance-lock integrity or I/O error,
/// or a timeout when the daemon keeps owning the workspace. A timed-out stop
/// leaves the live daemon's runtime files in place.
#[cfg(unix)]
pub(crate) fn stop_daemon_and_wait(root: &Path) -> Result<Option<String>> {
    let root = normalize_daemon_root(root);
    match std::fs::symlink_metadata(daemon_dir(&root)) {
        Ok(_) => {}
        // Without daemon state no daemon owns this workspace's lease or
        // runtime files. Do not create state merely to stop.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let acknowledgement = request_daemon_stop(&root)?;
            wait_for_daemon_shutdown(&root, daemon_stop_timeout())?;
            return Ok(acknowledgement);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect daemon state directory '{}'",
                    daemon_dir(&root).display()
                )
            })
        }
    }
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    let acknowledgement = request_daemon_stop(&root)?;
    wait_for_daemon_shutdown(&root, daemon_stop_timeout())?;
    cleanup_unreachable_runtime_files(&root)?;
    Ok(acknowledgement)
}

#[cfg(not(unix))]
pub(crate) fn stop_daemon_and_wait(_root: &Path) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(unix)]
pub(crate) fn restart_daemon(root: &Path) -> Result<()> {
    let root = normalize_daemon_root(root);
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    request_daemon_stop(&root)?;
    wait_for_daemon_shutdown(&root, daemon_stop_timeout())?;
    cleanup_unreachable_runtime_files(&root)?;
    let daemon = start_daemon(&root)?;
    wait_for_spawned_daemon(&root, &daemon)
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

/// Asks the daemon for a workspace to stop when its existing endpoint is
/// reachable, returning its acknowledgement.
///
/// # Errors
///
/// Returns a daemon error or unexpected response, or the connection or
/// stop-request error if the daemon endpoint remains reachable after the stop
/// request fails.
#[cfg(unix)]
fn request_daemon_stop(root: &Path) -> Result<Option<String>> {
    let endpoint = daemon_endpoint(root)?;
    if !endpoint_may_have_stale_socket(&endpoint) {
        return Ok(None);
    }
    match send_request_existing_daemon(root, &DaemonRequest::Stop) {
        Ok(DaemonResponse::Ack { message }) => Ok(Some(message)),
        Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!("unexpected daemon response: {other:?}")),
        Err(err) => {
            if connect_daemon_endpoint(&endpoint).is_ok() {
                Err(err)
            } else {
                Ok(None)
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

/// Waits until the daemon endpoint is unreachable and no daemon owns the
/// workspace instance lease.
///
/// The endpoint closes before shutdown persistence and runtime-file cleanup
/// finish; only the instance lease release marks the end of daemon authority.
/// Runtime metadata is read only after that release, because the stopping
/// daemon unlinks it during cleanup.
#[cfg(unix)]
fn wait_for_daemon_shutdown(root: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_instance_released(root)? {
            // A daemon without an instance lease may still serve the endpoint.
            let endpoint = daemon_endpoint(root)?;
            if !endpoint_may_have_stale_socket(&endpoint)
                || connect_daemon_endpoint(&endpoint).is_err()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "packet28d did not stop; socket still reachable at '{}'",
                    endpoint.address()
                ));
            }
        } else if Instant::now() >= deadline {
            return Err(daemon_authority_timeout(root, timeout));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonAuthority {
    Serving,
    Released,
}

/// Waits until the owner of the workspace serves it or releases authority.
///
/// Each status probe is bounded by the remaining authority time. An owner
/// identified as starting moves to the startup-readiness phase with its own
/// deadline of [`DAEMON_STARTUP_TIMEOUT`].
#[cfg(unix)]
fn wait_for_daemon_authority(root: &Path, timeout: Duration) -> Result<DaemonAuthority> {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_instance_released(root)? {
            return Ok(DaemonAuthority::Released);
        }
        match observe_daemon_owner(root, deadline)? {
            DaemonOwner::Serving => return Ok(DaemonAuthority::Serving),
            DaemonOwner::Starting(candidate) => {
                return wait_for_existing_daemon_startup(root, &candidate)
            }
            DaemonOwner::Unavailable => {}
        }
        if Instant::now() >= deadline {
            return Err(daemon_authority_timeout(root, timeout));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Probes the authenticated daemon instance lease without blocking.
///
/// Integrity and I/O failures are reported, never treated as a stopped daemon.
#[cfg(unix)]
fn daemon_instance_released(root: &Path) -> Result<bool> {
    packet28_daemon_core::task_store_lease::daemon_instance_released(root).with_context(|| {
        format!(
            "failed to probe packet28d instance authority '{}'",
            daemon_instance_lock_path(root).display()
        )
    })
}

#[cfg(unix)]
fn daemon_authority_timeout(root: &Path, timeout: Duration) -> anyhow::Error {
    anyhow!(
        "packet28d did not release workspace authority '{}' within {} ms; its runtime files \
         were left in place (log: {})",
        daemon_instance_lock_path(root).display(),
        timeout.as_millis(),
        log_path(root).display()
    )
}

/// Discovers the endpoint of the daemon published for `root`.
///
/// Metadata naming another workspace fails closed, so no request for `root`,
/// including Stop, reaches another workspace's daemon.
#[cfg(unix)]
fn daemon_endpoint(root: &Path) -> Result<DaemonEndpoint> {
    let Some(runtime) = read_runtime_info_if_present(root)? else {
        return Ok(packet28_daemon_client::transport::discover_endpoint(root)?);
    };
    let endpoint = DaemonEndpoint::from_runtime(root, &runtime)?;
    verify_runtime_workspace(root, &runtime)?;
    Ok(endpoint)
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
