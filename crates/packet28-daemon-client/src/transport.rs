//! Authenticated client connections to the endpoint published by `packet28d`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use packet28_daemon_protocol::frame::{read_frame, write_frame, FrameError};
use packet28_daemon_protocol::message::{DaemonResponse, DaemonRuntimeInfo, DaemonTransportAuth};
use packet28_daemon_protocol::paths::{runtime_path, socket_path};
use packet28_daemon_protocol::registry::{
    DaemonRegistryRequestV1, DaemonRegistryResponseV1, DaemonStatusV1,
};
use thiserror::Error;

use crate::runtime_discovery::{read_runtime_info_if_present, RuntimeDiscoveryError};

/// A daemon endpoint selected from authenticated runtime discovery.
#[derive(Clone)]
pub struct DaemonEndpoint {
    address: String,
    transport_auth: Option<DaemonTransportAuth>,
}

impl DaemonEndpoint {
    /// Returns the Unix path or `tcp://` address published for the daemon.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Selects the endpoint published in authenticated runtime metadata.
    ///
    /// `runtime` must come from [`crate::runtime_discovery`]; an empty
    /// endpoint selects the conventional Unix socket for `root`.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonClientError::LegacyUnauthenticatedTcp`] when the
    /// runtime advertises TCP without a per-instance capability.
    pub fn from_runtime(
        root: &Path,
        runtime: &DaemonRuntimeInfo,
    ) -> Result<DaemonEndpoint, DaemonClientError> {
        if runtime.socket_path.is_empty() {
            return Ok(default_endpoint(root));
        }
        if runtime.socket_path.starts_with("tcp://") && runtime.transport_auth.is_none() {
            return Err(DaemonClientError::LegacyUnauthenticatedTcp {
                endpoint: runtime.socket_path.clone(),
            });
        }
        Ok(DaemonEndpoint {
            address: runtime.socket_path.clone(),
            transport_auth: runtime.transport_auth.clone(),
        })
    }
}

impl std::fmt::Debug for DaemonEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonEndpoint")
            .field("address", &self.address)
            .field("transport_auth", &self.transport_auth)
            .finish()
    }
}

/// A connected daemon byte stream.
#[derive(Debug)]
pub enum DaemonStream {
    /// Mutually authenticated Unix-domain socket.
    Unix(UnixStream),
    /// Capability-authenticated loopback TCP socket.
    Tcp(TcpStream),
}

impl DaemonStream {
    /// Bounds each subsequent socket read and write by `timeout`.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, including for a zero `timeout`.
    pub fn set_io_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))
            }
            Self::Tcp(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))
            }
        }
    }

    /// Clones the underlying socket handle.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error from cloning the connected socket.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }
}

impl Read for DaemonStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for DaemonStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// Failure to discover or authenticate a daemon connection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DaemonClientError {
    /// Runtime discovery metadata or its namespace could not be authenticated.
    #[error(transparent)]
    Discovery(#[from] RuntimeDiscoveryError),
    /// A legacy TCP endpoint has no per-instance capability.
    #[error(
        "refusing legacy unauthenticated daemon TCP endpoint '{endpoint}'; stop that daemon with \
         its matching Packet28 version and start it again"
    )]
    LegacyUnauthenticatedTcp {
        /// Rejected endpoint.
        endpoint: String,
    },
    /// A socket operation failed.
    #[error("{operation} '{endpoint}': {source}")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Endpoint being accessed.
        endpoint: String,
        /// Operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A framed authentication operation failed.
    #[error("{operation} '{endpoint}': {source}")]
    Frame {
        /// Operation that failed.
        operation: &'static str,
        /// Endpoint being accessed.
        endpoint: String,
        /// Framing error.
        #[source]
        source: FrameError,
    },
    /// The daemon explicitly rejected the TCP capability.
    #[error("daemon TCP authentication at '{endpoint}' was rejected: {message}")]
    AuthenticationRejected {
        /// Rejected endpoint.
        endpoint: String,
        /// Daemon response.
        message: String,
    },
    /// The daemon returned a non-authentication response to the prelude.
    #[error("unexpected daemon authentication response from '{endpoint}': {response:?}")]
    UnexpectedAuthenticationResponse {
        /// Endpoint that returned the response.
        endpoint: String,
        /// Unexpected response.
        response: Box<DaemonResponse>,
    },
    /// A bounded request reached its deadline before it completed.
    #[error("deadline elapsed while {operation} '{endpoint}'")]
    DeadlineElapsed {
        /// Operation that could not start or finish in time.
        operation: &'static str,
        /// Endpoint being accessed.
        endpoint: String,
    },
    /// Authenticated runtime metadata names another workspace.
    #[error(
        "daemon runtime metadata '{runtime}' names workspace '{published}' (pid {pid}), not the \
         requested workspace '{requested}'; refusing to use another workspace's daemon and \
         leaving its files in place"
    )]
    ForeignWorkspace {
        /// Runtime metadata path for the requested workspace.
        runtime: String,
        /// Requested, normalized workspace root.
        requested: String,
        /// Workspace root the metadata names.
        published: String,
        /// Daemon pid the metadata names.
        pid: u32,
    },
    /// The daemon answered a status request with an error.
    #[error("daemon at '{endpoint}' rejected the status request: {message}")]
    StatusRejected {
        /// Endpoint that answered.
        endpoint: String,
        /// Daemon error message.
        message: String,
    },
    /// The daemon answered a status request with another response.
    #[error("unexpected daemon status response from '{endpoint}': {response:?}")]
    UnexpectedStatusResponse {
        /// Endpoint that answered.
        endpoint: String,
        /// Unexpected response.
        response: Box<DaemonRegistryResponseV1>,
    },
}

/// Discovers the authoritative endpoint for `root`.
///
/// A missing authenticated runtime publication uses the conventional Unix
/// endpoint for compatibility. Any present but unauthentic discovery state
/// fails closed.
///
/// # Errors
///
/// Returns [`DaemonClientError::Discovery`] when published state cannot be
/// authenticated or decoded, and
/// [`DaemonClientError::LegacyUnauthenticatedTcp`] when an older runtime
/// advertises TCP without a per-instance capability.
pub fn discover_endpoint(root: &Path) -> Result<DaemonEndpoint, DaemonClientError> {
    let Some(runtime) = read_runtime_info_if_present(root)? else {
        return Ok(default_endpoint(root));
    };
    DaemonEndpoint::from_runtime(root, &runtime)
}

/// Reports whether a published workspace root names `root`.
///
/// `root` is the caller's normalized workspace root. Identical spellings
/// match; otherwise both paths must resolve to the same canonical directory,
/// so a symlinked spelling of one workspace matches but another workspace's
/// root or an empty value never does.
pub fn workspace_root_matches(root: &Path, published: &str) -> bool {
    if published.is_empty() {
        return false;
    }
    let published = Path::new(published);
    if published == root {
        return true;
    }
    match (published.canonicalize(), root.canonicalize()) {
        (Ok(published), Ok(root)) => published == root,
        _ => false,
    }
}

/// Verifies that authenticated runtime metadata belongs to `root`.
///
/// Runtime discovery authenticates where metadata lives and who owns it, not
/// the workspace it names. Callers verify this before using the published
/// endpoint, so stale or copied metadata never directs requests for `root` to
/// another workspace's daemon.
///
/// # Errors
///
/// Returns [`DaemonClientError::ForeignWorkspace`] when the metadata names
/// another workspace.
pub fn verify_runtime_workspace(
    root: &Path,
    runtime: &DaemonRuntimeInfo,
) -> Result<(), DaemonClientError> {
    if workspace_root_matches(root, &runtime.workspace_root) {
        return Ok(());
    }
    Err(DaemonClientError::ForeignWorkspace {
        runtime: runtime_path(root).to_string_lossy().to_string(),
        requested: root.to_string_lossy().to_string(),
        published: runtime.workspace_root.clone(),
        pid: runtime.pid,
    })
}

/// Returns whether a discovered endpoint can leave a stale socket artifact.
pub fn endpoint_may_have_stale_socket(endpoint: &DaemonEndpoint) -> bool {
    endpoint.address.starts_with("tcp://") || Path::new(&endpoint.address).exists()
}

/// Discovers and connects to the authoritative daemon endpoint.
///
/// Unix server credentials or the TCP capability prelude are authenticated
/// before this function returns, so callers cannot send a request frame to an
/// unauthenticated peer.
///
/// # Errors
///
/// Returns [`DaemonClientError`] when discovery, connection setup, Unix peer
/// verification, or the TCP authentication prelude fails.
pub fn connect(root: &Path, timeout: Duration) -> Result<DaemonStream, DaemonClientError> {
    let endpoint = discover_endpoint(root)?;
    connect_endpoint(&endpoint, timeout)
}

/// Connects to a previously authenticated discovery result.
///
/// # Errors
///
/// Returns [`DaemonClientError`] when the socket cannot be connected or
/// configured, the Unix peer has the wrong effective user, or the TCP
/// capability exchange fails.
pub fn connect_endpoint(
    endpoint: &DaemonEndpoint,
    timeout: Duration,
) -> Result<DaemonStream, DaemonClientError> {
    if let Some(address) = endpoint.address.strip_prefix("tcp://") {
        return connect_tcp(address, endpoint, timeout);
    }
    connect_unix(Path::new(&endpoint.address), timeout)
}

/// Reports whether a live listener accepts connections at `endpoint`.
///
/// A daemon binds its listener before loading durable state but serves
/// requests only once ready, so this is liveness evidence for a daemon that is
/// still starting, not readiness. A Unix peer must have the client's effective
/// user. A TCP listener cannot answer the capability prelude before it is
/// ready, so its connection is closed without sending a request; callers rely
/// on the owner-private runtime metadata that published the capability.
///
/// # Errors
///
/// Returns [`DaemonClientError`] for a Unix peer owned by another user, a
/// legacy TCP endpoint without a capability, or a connection failure other
/// than an absent or refusing listener.
pub fn endpoint_accepts_connections(endpoint: &DaemonEndpoint) -> Result<bool, DaemonClientError> {
    let refused = |error: &std::io::Error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    };
    if let Some(address) = endpoint.address.strip_prefix("tcp://") {
        if endpoint.transport_auth.is_none() {
            return Err(DaemonClientError::LegacyUnauthenticatedTcp {
                endpoint: endpoint.address.clone(),
            });
        }
        return match TcpStream::connect(address) {
            Ok(_) => Ok(true),
            Err(error) if refused(&error) => Ok(false),
            Err(source) => Err(DaemonClientError::Io {
                operation: "failed to connect to daemon endpoint",
                endpoint: endpoint.address.clone(),
                source,
            }),
        };
    }
    match UnixStream::connect(&endpoint.address) {
        Ok(stream) => {
            verify_unix_server_peer(&stream, effective_uid()).map_err(|source| {
                DaemonClientError::Io {
                    operation: "failed to authenticate daemon peer",
                    endpoint: endpoint.address.clone(),
                    source,
                }
            })?;
            Ok(true)
        }
        Err(error) if refused(&error) => Ok(false),
        Err(source) => Err(DaemonClientError::Io {
            operation: "failed to connect to",
            endpoint: endpoint.address.clone(),
            source,
        }),
    }
}

/// Requests bounded V1 status from an authenticated endpoint.
///
/// Every socket read and write, including the TCP capability exchange, is
/// bounded by the time remaining before `deadline`, recomputed before each
/// operating-system call. Partial progress never extends the deadline, and a
/// response completed after it is rejected. Establishing a local connection
/// is not separately bounded.
///
/// # Errors
///
/// Returns [`DaemonClientError::DeadlineElapsed`] when the deadline passes
/// before the response is complete, a connection, authentication, or framing
/// error, or the daemon's rejection or unexpected response.
pub fn request_status_v1(
    endpoint: &DaemonEndpoint,
    deadline: Instant,
) -> Result<DaemonStatusV1, DaemonClientError> {
    let mut stream = connect_endpoint_until(endpoint, deadline)?;
    match exchange_status(&mut stream, endpoint, deadline)? {
        DaemonRegistryResponseV1::Status { status } => Ok(*status),
        DaemonRegistryResponseV1::Error { message } => Err(DaemonClientError::StatusRejected {
            endpoint: endpoint.address.clone(),
            message,
        }),
        response => Err(DaemonClientError::UnexpectedStatusResponse {
            endpoint: endpoint.address.clone(),
            response: Box::new(response),
        }),
    }
}

/// Sends a status request and reads its response within `deadline`.
fn exchange_status<S: IoTimeouts + Read + Write>(
    socket: &mut S,
    endpoint: &DaemonEndpoint,
    deadline: Instant,
) -> Result<DaemonRegistryResponseV1, DaemonClientError> {
    let mut io = DeadlineIo::new(socket, deadline);
    write_frame(&mut io, &DaemonRegistryRequestV1::Status).map_err(|source| {
        deadline_or(
            DaemonClientError::Frame {
                operation: "failed to write status request to",
                endpoint: endpoint.address.clone(),
                source,
            },
            endpoint,
            deadline,
            "writing status request to",
        )
    })?;
    let response = read_frame(&mut io).map_err(|source| {
        deadline_or(
            DaemonClientError::Frame {
                operation: "failed to read status response from",
                endpoint: endpoint.address.clone(),
                source,
            },
            endpoint,
            deadline,
            "reading status response from",
        )
    })?;
    // A response completed after the deadline is not accepted.
    ensure_before(endpoint, deadline, "reading status response from")?;
    Ok(response)
}

/// Connects and authenticates within `deadline`.
fn connect_endpoint_until(
    endpoint: &DaemonEndpoint,
    deadline: Instant,
) -> Result<DaemonStream, DaemonClientError> {
    let remaining = ensure_before(endpoint, deadline, "connecting to")?;
    let Some(address) = endpoint.address.strip_prefix("tcp://") else {
        return connect_unix(Path::new(&endpoint.address), remaining);
    };
    let auth = tcp_auth(endpoint)?;
    let mut stream = open_tcp(address, endpoint, remaining)?;
    authenticate_tcp(&mut DeadlineIo::new(&mut stream, deadline), auth, endpoint)
        .map_err(|error| deadline_or(error, endpoint, deadline, "authenticating with"))?;
    ensure_before(endpoint, deadline, "authenticating with")?;
    Ok(DaemonStream::Tcp(stream))
}

/// Returns the time left before `deadline`, or a deadline error.
fn ensure_before(
    endpoint: &DaemonEndpoint,
    deadline: Instant,
    operation: &'static str,
) -> Result<Duration, DaemonClientError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(DaemonClientError::DeadlineElapsed {
            operation,
            endpoint: endpoint.address.clone(),
        });
    }
    Ok(remaining)
}

/// Reports `error` as a deadline error once the deadline has passed, since a
/// deadline-bounded socket then fails with a timeout or truncated frame.
fn deadline_or(
    error: DaemonClientError,
    endpoint: &DaemonEndpoint,
    deadline: Instant,
    operation: &'static str,
) -> DaemonClientError {
    if Instant::now() >= deadline {
        return DaemonClientError::DeadlineElapsed {
            operation,
            endpoint: endpoint.address.clone(),
        };
    }
    error
}

/// A socket whose individual reads and writes can be time-bounded.
trait IoTimeouts {
    fn set_io_timeouts(&self, timeout: Duration) -> std::io::Result<()>;
}

impl IoTimeouts for TcpStream {
    fn set_io_timeouts(&self, timeout: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(timeout))?;
        self.set_write_timeout(Some(timeout))
    }
}

impl IoTimeouts for DaemonStream {
    fn set_io_timeouts(&self, timeout: Duration) -> std::io::Result<()> {
        self.set_io_timeout(timeout)
    }
}

/// Bounds every read and write on a socket by one absolute deadline.
///
/// Socket timeouts apply per system call, so a peer sending one byte at a
/// time would otherwise restart them indefinitely. The remaining time is
/// recomputed and armed before every call instead.
struct DeadlineIo<'a, S> {
    socket: &'a mut S,
    deadline: Instant,
}

impl<'a, S: IoTimeouts> DeadlineIo<'a, S> {
    fn new(socket: &'a mut S, deadline: Instant) -> Self {
        Self { socket, deadline }
    }

    /// Arms the socket timeouts with the remaining time.
    fn arm(&self) -> std::io::Result<()> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request deadline elapsed",
            ));
        }
        match self.socket.set_io_timeouts(remaining) {
            // macOS rejects socket options once the peer has closed the
            // connection. Calls then return buffered data, EOF, or an error
            // without blocking, and the timeout armed earlier still applies.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            result => result,
        }
    }
}

/// Reports whether a socket call stopped at its armed timeout.
fn socket_timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

impl<S: IoTimeouts + Read> Read for DeadlineIo<'_, S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.arm()?;
            match self.socket.read(buffer) {
                // A timeout firing early re-arms with whatever time is left.
                Err(error) if socket_timed_out(&error) => continue,
                result => return result,
            }
        }
    }
}

impl<S: IoTimeouts + Write> Write for DeadlineIo<'_, S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        loop {
            self.arm()?;
            match self.socket.write(buffer) {
                Err(error) if socket_timed_out(&error) => continue,
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.arm()?;
        self.socket.flush()
    }
}

fn default_endpoint(root: &Path) -> DaemonEndpoint {
    DaemonEndpoint {
        address: socket_path(root).to_string_lossy().to_string(),
        transport_auth: None,
    }
}

fn connect_unix(path: &Path, timeout: Duration) -> Result<DaemonStream, DaemonClientError> {
    let endpoint = path.to_string_lossy().to_string();
    let stream = UnixStream::connect(path).map_err(|source| DaemonClientError::Io {
        operation: "failed to connect to",
        endpoint: endpoint.clone(),
        source,
    })?;
    verify_unix_server_peer(&stream, effective_uid()).map_err(|source| DaemonClientError::Io {
        operation: "failed to authenticate daemon peer",
        endpoint: endpoint.clone(),
        source,
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| DaemonClientError::Io {
            operation: "failed to configure read timeout for",
            endpoint: endpoint.clone(),
            source,
        })?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|source| DaemonClientError::Io {
            operation: "failed to configure write timeout for",
            endpoint,
            source,
        })?;
    Ok(DaemonStream::Unix(stream))
}

fn connect_tcp(
    address: &str,
    endpoint: &DaemonEndpoint,
    timeout: Duration,
) -> Result<DaemonStream, DaemonClientError> {
    let auth = tcp_auth(endpoint)?;
    let mut stream = open_tcp(address, endpoint, timeout)?;
    authenticate_tcp(&mut stream, auth, endpoint)?;
    Ok(DaemonStream::Tcp(stream))
}

fn tcp_auth(endpoint: &DaemonEndpoint) -> Result<&DaemonTransportAuth, DaemonClientError> {
    endpoint
        .transport_auth
        .as_ref()
        .ok_or_else(|| DaemonClientError::LegacyUnauthenticatedTcp {
            endpoint: endpoint.address.clone(),
        })
}

fn open_tcp(
    address: &str,
    endpoint: &DaemonEndpoint,
    timeout: Duration,
) -> Result<TcpStream, DaemonClientError> {
    let stream = TcpStream::connect(address).map_err(|source| DaemonClientError::Io {
        operation: "failed to connect to daemon endpoint",
        endpoint: endpoint.address.clone(),
        source,
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| DaemonClientError::Io {
            operation: "failed to configure read timeout for",
            endpoint: endpoint.address.clone(),
            source,
        })?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|source| DaemonClientError::Io {
            operation: "failed to configure write timeout for",
            endpoint: endpoint.address.clone(),
            source,
        })?;
    Ok(stream)
}

/// Performs the TCP capability prelude before any request is sent.
fn authenticate_tcp<S: Read + Write>(
    stream: &mut S,
    auth: &DaemonTransportAuth,
    endpoint: &DaemonEndpoint,
) -> Result<(), DaemonClientError> {
    write_frame(stream, auth).map_err(|source| DaemonClientError::Frame {
        operation: "failed to write authentication prelude to",
        endpoint: endpoint.address.clone(),
        source,
    })?;
    match read_frame(stream).map_err(|source| DaemonClientError::Frame {
        operation: "failed to read authentication response from",
        endpoint: endpoint.address.clone(),
        source,
    })? {
        DaemonResponse::Ack { message } if message == "authenticated" => Ok(()),
        DaemonResponse::Error { message } => Err(DaemonClientError::AuthenticationRejected {
            endpoint: endpoint.address.clone(),
            message,
        }),
        response => Err(DaemonClientError::UnexpectedAuthenticationResponse {
            endpoint: endpoint.address.clone(),
            response: Box::new(response),
        }),
    }
}

fn verify_unix_server_peer(stream: &UnixStream, expected_uid: u32) -> std::io::Result<()> {
    let peer_uid = unix_peer_uid(stream)?;
    if peer_uid == expected_uid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "Unix daemon peer uid {peer_uid} does not match client effective uid \
                 {expected_uid}"
            ),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn unix_peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `length` point to writable storage of the
    // declared sizes, and `stream` owns a live connected Unix socket.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unix peer credential response had an unexpected size",
        ));
    }
    // SAFETY: a successful `getsockopt(SO_PEERCRED)` initialized the complete
    // `ucred` value after the exact returned length was validated.
    Ok(unsafe { credentials.assume_init() }.uid)
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn unix_peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: `uid` and `gid` are valid writable outputs and `stream` owns a
    // live connected Unix socket for the duration of the call.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn unix_peer_uid(_stream: &UnixStream) -> std::io::Result<u32> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unix peer credential verification is unavailable on this platform",
    ))
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and retains no pointers.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    use packet28_daemon_protocol::message::DAEMON_TRANSPORT_SECRET_BYTES;
    use packet28_daemon_protocol::paths::{runtime_path, workspace_socket_path};

    fn write_runtime(root: &Path, runtime: &DaemonRuntimeInfo) {
        let path = runtime_path(root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(runtime).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn connect_uses_authoritative_workspace_unix_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let socket = workspace_socket_path(root.path());
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        write_runtime(
            root.path(),
            &DaemonRuntimeInfo {
                socket_path: socket.to_string_lossy().to_string(),
                ..DaemonRuntimeInfo::default()
            },
        );

        let stream = connect(root.path(), Duration::from_secs(1)).unwrap();
        let _accepted = listener.accept().unwrap();

        assert!(matches!(stream, DaemonStream::Unix(_)));
    }

    #[test]
    fn connect_authenticates_tcp_endpoint_before_returning() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x4d; DAEMON_TRANSPORT_SECRET_BYTES]);
        write_runtime(
            root.path(),
            &DaemonRuntimeInfo {
                socket_path: format!("tcp://{}", listener.local_addr().unwrap()),
                transport_auth: Some(auth.clone()),
                ..DaemonRuntimeInfo::default()
            },
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received: DaemonTransportAuth = read_frame(&mut stream).unwrap();
            assert!(auth.authenticates(&received));
            write_frame(
                &mut stream,
                &DaemonResponse::Ack {
                    message: "authenticated".to_string(),
                },
            )
            .unwrap();
        });

        let stream = connect(root.path(), Duration::from_secs(1)).unwrap();

        assert!(matches!(stream, DaemonStream::Tcp(_)));
        server.join().unwrap();
    }

    #[test]
    fn legacy_tcp_endpoint_without_capability_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        write_runtime(
            root.path(),
            &DaemonRuntimeInfo {
                socket_path: "tcp://127.0.0.1:4242".to_string(),
                ..DaemonRuntimeInfo::default()
            },
        );

        let error = connect(root.path(), Duration::from_secs(1)).unwrap_err();

        assert!(matches!(
            error,
            DaemonClientError::LegacyUnauthenticatedTcp { .. }
        ));
    }

    /// Slack for scheduler and socket-timeout granularity. Assertions only
    /// check that a request ended near its deadline, never how early.
    const DEADLINE_SLACK: Duration = Duration::from_millis(750);

    fn unix_endpoint(socket: &Path) -> DaemonEndpoint {
        DaemonEndpoint {
            address: socket.to_string_lossy().to_string(),
            transport_auth: None,
        }
    }

    #[test]
    fn status_request_to_connected_unserved_unix_listener_ends_at_its_deadline() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("starting.sock");
        // Bound but never accepted, like a daemon still loading durable state.
        let _listener = UnixListener::bind(&socket).unwrap();
        let endpoint = unix_endpoint(&socket);
        assert!(endpoint_accepts_connections(&endpoint).unwrap());

        let budget = Duration::from_millis(400);
        let started = Instant::now();
        let error = request_status_v1(&endpoint, started + budget).unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(error, DaemonClientError::DeadlineElapsed { .. }),
            "unexpected error: {error}"
        );
        assert!(
            elapsed < budget + DEADLINE_SLACK,
            "status request outlived its deadline: {elapsed:?}"
        );
    }

    #[test]
    fn status_request_to_unserved_tcp_listener_ends_at_its_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = DaemonEndpoint {
            address: format!("tcp://{}", listener.local_addr().unwrap()),
            transport_auth: Some(DaemonTransportAuth::from_secret_bytes(
                [0x5a; DAEMON_TRANSPORT_SECRET_BYTES],
            )),
        };
        assert!(endpoint_accepts_connections(&endpoint).unwrap());

        let budget = Duration::from_millis(400);
        let started = Instant::now();
        let error = request_status_v1(&endpoint, started + budget).unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(error, DaemonClientError::DeadlineElapsed { .. }),
            "unexpected error: {error}"
        );
        assert!(
            elapsed < budget + DEADLINE_SLACK,
            "status request outlived its deadline: {elapsed:?}"
        );
    }

    #[test]
    fn status_request_with_elapsed_deadline_does_not_connect() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("never.sock");
        let error = request_status_v1(&unix_endpoint(&socket), Instant::now()).unwrap_err();
        assert!(matches!(error, DaemonClientError::DeadlineElapsed { .. }));
    }

    #[test]
    fn endpoint_liveness_reports_absent_and_refusing_listeners() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            !endpoint_accepts_connections(&unix_endpoint(&root.path().join("absent.sock")))
                .unwrap()
        );
        let closed = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let endpoint = DaemonEndpoint {
            address: format!("tcp://{address}"),
            transport_auth: Some(DaemonTransportAuth::from_secret_bytes(
                [0x5a; DAEMON_TRANSPORT_SECRET_BYTES],
            )),
        };
        assert!(!endpoint_accepts_connections(&endpoint).unwrap());
        let legacy = DaemonEndpoint {
            address: format!("tcp://{address}"),
            transport_auth: None,
        };
        assert!(matches!(
            endpoint_accepts_connections(&legacy),
            Err(DaemonClientError::LegacyUnauthenticatedTcp { .. })
        ));
    }

    #[test]
    fn status_request_returns_the_served_status() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("ready.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: DaemonRegistryRequestV1 = read_frame(&mut stream).unwrap();
            assert!(matches!(request, DaemonRegistryRequestV1::Status));
            write_frame(
                &mut stream,
                &DaemonRegistryResponseV1::Status {
                    status: Box::new(DaemonStatusV1 {
                        pid: 4242,
                        ..DaemonStatusV1::default()
                    }),
                },
            )
            .unwrap();
        });

        let status = request_status_v1(
            &unix_endpoint(&socket),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(status.pid, 4242);
        server.join().unwrap();
    }

    /// Interval between bytes a trickling peer sends.
    const DRIBBLE_INTERVAL: Duration = Duration::from_millis(35);

    fn status_response_wire() -> Vec<u8> {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &DaemonRegistryResponseV1::Status {
                status: Box::new(DaemonStatusV1 {
                    pid: 4242,
                    ..DaemonStatusV1::default()
                }),
            },
        )
        .unwrap();
        wire
    }

    fn ack_wire() -> Vec<u8> {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &DaemonResponse::Ack {
                message: "authenticated".to_string(),
            },
        )
        .unwrap();
        wire
    }

    /// Writes the first `burst` bytes of `wire` at once, then one byte per
    /// `interval` until done, the client hangs up, or `stop` is set. Returns
    /// the number of bytes written.
    fn dribble(
        stream: &mut impl Write,
        wire: &[u8],
        burst: usize,
        interval: Duration,
        stop: &AtomicBool,
    ) -> usize {
        if stream.write_all(&wire[..burst]).is_err() {
            return 0;
        }
        let mut sent = burst;
        for byte in &wire[burst..] {
            if stop.load(Ordering::SeqCst) || stream.write_all(&[*byte]).is_err() {
                break;
            }
            sent += 1;
            thread::sleep(interval);
        }
        sent
    }

    /// Asserts a dribbled response ended at its deadline: not before it, not
    /// meaningfully after it, and after the peer made partial progress.
    fn assert_ended_at_deadline(
        error: &DaemonClientError,
        operation: &str,
        elapsed: Duration,
        budget: Duration,
        sent: usize,
        total: usize,
    ) {
        assert!(
            matches!(
                error,
                DaemonClientError::DeadlineElapsed { operation: actual, .. } if *actual == operation
            ),
            "unexpected error: {error}"
        );
        assert!(
            elapsed >= budget,
            "request ended before its deadline: {elapsed:?}"
        );
        assert!(
            elapsed < budget + DEADLINE_SLACK,
            "partial progress extended the deadline: {elapsed:?}"
        );
        assert!(sent >= 2, "peer made no partial progress: {sent} bytes");
        assert!(
            sent < total,
            "the full response was sent: {sent} of {total} bytes"
        );
    }

    fn dribbled_unix_status(burst: usize) {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("dribble.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let wire = status_response_wire();
        let total = wire.len();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _: DaemonRegistryRequestV1 = read_frame(&mut stream).unwrap();
            dribble(&mut stream, &wire, burst, DRIBBLE_INTERVAL, &server_stop)
        });

        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let error = request_status_v1(&unix_endpoint(&socket), started + budget).unwrap_err();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        let sent = server.join().unwrap();

        assert_ended_at_deadline(
            &error,
            "reading status response from",
            elapsed,
            budget,
            sent,
            total,
        );
    }

    #[test]
    fn dribbled_unix_status_header_does_not_extend_the_deadline() {
        dribbled_unix_status(0);
    }

    #[test]
    fn dribbled_unix_status_body_does_not_extend_the_deadline() {
        dribbled_unix_status(8);
    }

    fn tcp_endpoint(listener: &TcpListener, auth: &DaemonTransportAuth) -> DaemonEndpoint {
        DaemonEndpoint {
            address: format!("tcp://{}", listener.local_addr().unwrap()),
            transport_auth: Some(auth.clone()),
        }
    }

    #[test]
    fn dribbled_tcp_authentication_does_not_extend_the_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x61; DAEMON_TRANSPORT_SECRET_BYTES]);
        let endpoint = tcp_endpoint(&listener, &auth);
        let wire = ack_wire();
        let total = wire.len();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received: DaemonTransportAuth = read_frame(&mut stream).unwrap();
            assert!(auth.authenticates(&received));
            dribble(&mut stream, &wire, 0, DRIBBLE_INTERVAL, &server_stop)
        });

        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let error = request_status_v1(&endpoint, started + budget).unwrap_err();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        let sent = server.join().unwrap();

        assert_ended_at_deadline(&error, "authenticating with", elapsed, budget, sent, total);
    }

    #[test]
    fn dribbled_tcp_status_does_not_extend_the_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x62; DAEMON_TRANSPORT_SECRET_BYTES]);
        let endpoint = tcp_endpoint(&listener, &auth);
        let wire = status_response_wire();
        let total = wire.len();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received: DaemonTransportAuth = read_frame(&mut stream).unwrap();
            assert!(auth.authenticates(&received));
            stream.write_all(&ack_wire()).unwrap();
            let _: DaemonRegistryRequestV1 = read_frame(&mut stream).unwrap();
            dribble(&mut stream, &wire, 0, DRIBBLE_INTERVAL, &server_stop)
        });

        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let error = request_status_v1(&endpoint, started + budget).unwrap_err();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        let sent = server.join().unwrap();

        assert_ended_at_deadline(
            &error,
            "reading status response from",
            elapsed,
            budget,
            sent,
            total,
        );
    }

    #[test]
    fn partial_tcp_responses_completed_before_the_deadline_are_accepted() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let auth = DaemonTransportAuth::from_secret_bytes([0x63; DAEMON_TRANSPORT_SECRET_BYTES]);
        let endpoint = tcp_endpoint(&listener, &auth);
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _: DaemonTransportAuth = read_frame(&mut stream).unwrap();
            let interval = Duration::from_millis(1);
            let ack = ack_wire();
            assert_eq!(
                dribble(&mut stream, &ack, 0, interval, &server_stop),
                ack.len()
            );
            let _: DaemonRegistryRequestV1 = read_frame(&mut stream).unwrap();
            let status = status_response_wire();
            assert_eq!(
                dribble(&mut stream, &status, 0, interval, &server_stop),
                status.len()
            );
        });

        let status = request_status_v1(&endpoint, Instant::now() + Duration::from_secs(30));

        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert_eq!(status.unwrap().pid, 4242);
    }

    #[test]
    fn status_peer_eof_and_oversized_frames_fail_before_the_deadline() {
        let oversized = (packet28_daemon_protocol::frame::MAX_SOCKET_MESSAGE_BYTES as u64 + 1)
            .to_be_bytes()
            .to_vec();
        for (reply, expected) in [(vec![0_u8; 3], "eof"), (oversized, "too large")] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("short.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _: DaemonRegistryRequestV1 = read_frame(&mut stream).unwrap();
                stream.write_all(&reply).unwrap();
            });

            let budget = Duration::from_secs(10);
            let started = Instant::now();
            let error = request_status_v1(&unix_endpoint(&socket), started + budget).unwrap_err();
            server.join().unwrap();

            assert!(
                started.elapsed() < budget / 2,
                "{expected} waited for the deadline"
            );
            match (&error, expected) {
                (
                    DaemonClientError::Frame {
                        source: FrameError::Io(source),
                        ..
                    },
                    "eof",
                ) => assert_eq!(source.kind(), std::io::ErrorKind::UnexpectedEof),
                (
                    DaemonClientError::Frame {
                        source: FrameError::TooLarge { .. },
                        ..
                    },
                    "too large",
                ) => {}
                _ => panic!("unexpected {expected} error: {error}"),
            }
        }
    }

    /// A socket whose final read returns the rest of a complete response
    /// only after `delay`, as when the client is descheduled at the deadline.
    struct LateSocket {
        reply: Cursor<Vec<u8>>,
        delay: Duration,
    }

    impl IoTimeouts for LateSocket {
        fn set_io_timeouts(&self, _timeout: Duration) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for LateSocket {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.reply.get_ref().len() as u64 - self.reply.position();
            if buffer.len() as u64 >= remaining {
                thread::sleep(self.delay);
            }
            self.reply.read(buffer)
        }
    }

    impl Write for LateSocket {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn complete_status_response_after_the_deadline_is_rejected() {
        let endpoint = unix_endpoint(Path::new("late.sock"));
        let mut on_time = LateSocket {
            reply: Cursor::new(status_response_wire()),
            delay: Duration::ZERO,
        };
        let response = exchange_status(
            &mut on_time,
            &endpoint,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(
            response,
            DaemonRegistryResponseV1::Status { status } if status.pid == 4242
        ));

        let mut late = LateSocket {
            reply: Cursor::new(status_response_wire()),
            delay: Duration::from_millis(200),
        };
        let error = exchange_status(
            &mut late,
            &endpoint,
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                DaemonClientError::DeadlineElapsed {
                    operation: "reading status response from",
                    ..
                }
            ),
            "unexpected error: {error}"
        );
        assert_eq!(late.reply.position(), late.reply.get_ref().len() as u64);
    }

    #[test]
    fn runtime_workspace_must_name_the_requested_root() {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let runtime = |workspace_root: &Path| DaemonRuntimeInfo {
            pid: 4242,
            workspace_root: workspace_root.to_string_lossy().to_string(),
            ..DaemonRuntimeInfo::default()
        };

        verify_runtime_workspace(root, &runtime(root)).unwrap();
        // Another spelling of the same directory still names it.
        let alias = other.path().join("alias");
        std::os::unix::fs::symlink(root, &alias).unwrap();
        verify_runtime_workspace(&alias, &runtime(root)).unwrap();
        verify_runtime_workspace(root, &runtime(&alias)).unwrap();
        verify_runtime_workspace(&root.canonicalize().unwrap(), &runtime(root)).unwrap();

        for published in [other.path(), Path::new(""), &root.join("missing")] {
            let error = verify_runtime_workspace(root, &runtime(published)).unwrap_err();
            assert!(
                matches!(
                    &error,
                    DaemonClientError::ForeignWorkspace { pid: 4242, requested, .. }
                        if requested == &root.to_string_lossy()
                ),
                "unexpected error: {error}"
            );
            assert!(error
                .to_string()
                .contains("refusing to use another workspace's daemon"));
        }
    }

    #[test]
    fn runtime_endpoint_selection_rejects_legacy_tcp() {
        let root = tempfile::tempdir().unwrap();
        let runtime = DaemonRuntimeInfo {
            socket_path: "tcp://127.0.0.1:4242".to_string(),
            ..DaemonRuntimeInfo::default()
        };
        assert!(matches!(
            DaemonEndpoint::from_runtime(root.path(), &runtime),
            Err(DaemonClientError::LegacyUnauthenticatedTcp { .. })
        ));
        let empty =
            DaemonEndpoint::from_runtime(root.path(), &DaemonRuntimeInfo::default()).unwrap();
        assert_eq!(empty.address(), socket_path(root.path()).to_string_lossy());
    }

    #[test]
    fn unix_peer_verification_accepts_owner_and_rejects_substituted_uid() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let stream = UnixStream::connect(&socket).unwrap();
        let _accepted = listener.accept().unwrap();

        verify_unix_server_peer(&stream, effective_uid()).unwrap();
        let error = verify_unix_server_peer(&stream, effective_uid() ^ 1).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
