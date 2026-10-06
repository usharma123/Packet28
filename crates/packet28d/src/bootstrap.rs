//! Lease-guarded daemon bootstrap for clients that do not link daemon authority.
//!
//! `p28` may not depend on `packet28-daemon-core`, so it cannot take the
//! startup lease or probe the instance lease itself. `packet28d start` performs
//! discovery, stale-file cleanup, and spawn under the same leases as the
//! Packet28 CLI, so a stopping daemon's runtime files are never removed and no
//! losing replacement is spawned while it still owns the workspace.
//!
//! Bootstrap distinguishes two phases with separate fixed deadlines:
//!
//! - Authority: a daemon owns the instance lease but is neither serving nor
//!   identifiably starting, for example because it is stopping. It gets
//!   [`BOOTSTRAP_AUTHORITY_TIMEOUT`] to release authority.
//! - Startup readiness: a daemon this call spawned, or an existing daemon whose
//!   authenticated runtime metadata and live endpoint identify a startup that
//!   has not yet published readiness. It gets [`BOOTSTRAP_STARTUP_TIMEOUT`] to
//!   answer bounded status with the same identity.
//!
//! Published runtime metadata must name the requested workspace before its
//! endpoint is used, and status must answer with that workspace and the
//! published pid; stale or copied metadata for another workspace fails closed.
//!
//! Neither deadline covers startup-lease acquisition, which blocks behind
//! another bootstrap or an explicit stop, so the call as a whole has no single
//! wall-clock bound. A timed-out caller leaves a starting daemon running.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use packet28_daemon_client::runtime_discovery::read_runtime_info_if_present;
use packet28_daemon_client::transport::{
    endpoint_accepts_connections, request_status_v1, verify_runtime_workspace,
    workspace_root_matches, DaemonEndpoint,
};
use packet28_daemon_core::task_store_lease::{
    acquire_daemon_startup_lease, daemon_instance_lock_path, daemon_instance_released,
};
use packet28_daemon_protocol::message::DaemonRuntimeInfo;
use packet28_daemon_protocol::paths::{log_path, ready_path, socket_path, workspace_socket_path};
use packet28_daemon_protocol::registry::DaemonStatusV1;

use crate::resolve_root;

/// Bound for a daemon that is stopping or held offline to release workspace
/// authority. It matches the client bootstrap bound that predates authority
/// waiting; explicit stop and restart use the longer shutdown grace instead.
const BOOTSTRAP_AUTHORITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound for a starting daemon to answer status after it was spawned or
/// identified. It matches the 30 s connected status budget clients had before
/// bootstrap moved into `packet28d`; seeded 5,000-task debug startup measured
/// about 20 s.
const BOOTSTRAP_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

const BOOTSTRAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Starts the daemon for `root` unless one is already serving it.
///
/// Holds the startup lease from discovery until the selected daemon answers
/// status. An existing daemon that is still starting is reused rather than
/// treated as a stopping owner. Runtime files are removed and a daemon is
/// spawned only after the instance lease is released.
///
/// # Errors
///
/// Returns a lease, runtime-metadata, or endpoint integrity error, an
/// authority or startup-readiness timeout, a stale-file cleanup or spawn
/// error, or an error when the spawned daemon exits before it is ready.
pub fn start(root: PathBuf) -> Result<()> {
    let root = resolve_root(&root);
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    if wait_for_authority(&root)? == Authority::Serving {
        return Ok(());
    }
    remove_stale_endpoint_files(&root)?;
    let mut daemon = spawn_daemon(&root)?;
    wait_for_spawned_daemon(&root, &mut daemon)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    Serving,
    Released,
}

/// What the current instance-lease owner's published state shows.
enum Owner {
    Serving,
    Starting(Box<StartupCandidate>),
    Unavailable,
}

/// A daemon identified by authenticated runtime metadata that has not yet
/// published readiness.
struct StartupCandidate {
    runtime: DaemonRuntimeInfo,
    endpoint: DaemonEndpoint,
}

/// Waits until the owner of the workspace serves it or releases authority.
///
/// An owner identified as starting moves to the startup-readiness phase with
/// its own deadline; any other owner must answer status or release authority
/// before the authority deadline.
fn wait_for_authority(root: &Path) -> Result<Authority> {
    let started = Instant::now();
    let deadline = started + BOOTSTRAP_AUTHORITY_TIMEOUT;
    loop {
        if instance_released(root)? {
            return Ok(Authority::Released);
        }
        match observe_owner(root, deadline)? {
            Owner::Serving => return Ok(Authority::Serving),
            Owner::Starting(candidate) => return wait_for_existing_startup(root, &candidate),
            Owner::Unavailable => {}
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "packet28d did not release workspace authority '{}' within {} ms (authority \
                 phase, elapsed {} ms); its runtime files were left in place (log: {})",
                daemon_instance_lock_path(root).display(),
                BOOTSTRAP_AUTHORITY_TIMEOUT.as_millis(),
                started.elapsed().as_millis(),
                log_path(root).display()
            ));
        }
        thread::sleep(BOOTSTRAP_POLL_INTERVAL);
    }
}

/// Classifies the instance-lease owner from authenticated runtime metadata.
///
/// Runtime metadata only selects what to wait for; it never authorizes
/// cleanup. Unauthentic or malformed metadata, or metadata naming another
/// workspace, fails closed before its endpoint is used.
fn observe_owner(root: &Path, deadline: Instant) -> Result<Owner> {
    let Some(runtime) = read_runtime_info_if_present(root)
        .context("failed to read packet28d runtime metadata while the daemon owns the workspace")?
    else {
        // The owner has not published runtime metadata yet, or has removed it
        // during shutdown cleanup.
        return Ok(Owner::Unavailable);
    };
    verify_runtime_workspace(root, &runtime)?;
    let endpoint = DaemonEndpoint::from_runtime(root, &runtime)?;
    if runtime.ready_at_unix.is_none() {
        // A daemon binds its listener and publishes runtime metadata before it
        // loads durable state, then accepts requests once ready.
        if endpoint_accepts_connections(&endpoint)? {
            return Ok(Owner::Starting(Box::new(StartupCandidate {
                runtime,
                endpoint,
            })));
        }
        return Ok(Owner::Unavailable);
    }
    match request_status_v1(&endpoint, deadline) {
        Ok(status) if same_daemon(root, &status, &runtime) => Ok(Owner::Serving),
        // A stopping daemon withdraws its endpoint before it releases
        // authority; keep waiting within the authority deadline.
        Ok(_) | Err(_) => Ok(Owner::Unavailable),
    }
}

/// Waits for an existing starting daemon to answer status with its identity.
///
/// Returns [`Authority::Released`] if it releases the instance lease first.
/// On timeout the daemon is left running to finish startup.
fn wait_for_existing_startup(root: &Path, candidate: &StartupCandidate) -> Result<Authority> {
    let started = Instant::now();
    let deadline = started + BOOTSTRAP_STARTUP_TIMEOUT;
    loop {
        let last_error = match request_status_v1(&candidate.endpoint, deadline) {
            Ok(status) if same_daemon(root, &status, &candidate.runtime) => {
                return Ok(Authority::Serving)
            }
            Ok(status) => identity_mismatch(root, &status, &candidate.runtime),
            Err(error) => error.to_string(),
        };
        if instance_released(root)? {
            return Ok(Authority::Released);
        }
        if Instant::now() >= deadline {
            return Err(startup_timeout(
                root,
                candidate.runtime.pid,
                started,
                Some(last_error),
            ));
        }
        thread::sleep(BOOTSTRAP_POLL_INTERVAL);
    }
}

/// Waits for the daemon this call spawned to answer status with its identity.
///
/// The instance lease is never probed here: a probe could briefly hold it
/// while the child tries to acquire it. On timeout the child is left running
/// to finish startup.
fn wait_for_spawned_daemon(root: &Path, daemon: &mut Child) -> Result<()> {
    let pid = daemon.id();
    let started = Instant::now();
    let deadline = started + BOOTSTRAP_STARTUP_TIMEOUT;
    let mut last_error = None;
    loop {
        if let Some(status) = daemon.try_wait().context("failed to poll packet28d")? {
            return Err(anyhow!(
                "packet28d pid {pid} exited with {status} before becoming ready (startup \
                 readiness phase, elapsed {} ms; log: {})",
                started.elapsed().as_millis(),
                log_path(root).display()
            ));
        }
        // Until the child publishes its own metadata, runtime files may belong
        // to the daemon that released authority; they are never trusted for
        // the child.
        match read_runtime_info_if_present(root) {
            Ok(Some(runtime)) if runtime.pid == pid => {
                verify_runtime_workspace(root, &runtime)?;
                let endpoint = DaemonEndpoint::from_runtime(root, &runtime)?;
                match request_status_v1(&endpoint, deadline) {
                    Ok(status) if same_daemon(root, &status, &runtime) => return Ok(()),
                    Ok(status) => last_error = Some(identity_mismatch(root, &status, &runtime)),
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(startup_timeout(root, pid, started, last_error));
        }
        thread::sleep(BOOTSTRAP_POLL_INTERVAL);
    }
}

/// Whether status comes from the published daemon serving `root`.
fn same_daemon(root: &Path, status: &DaemonStatusV1, runtime: &DaemonRuntimeInfo) -> bool {
    status.pid == runtime.pid
        && status.workspace_root == runtime.workspace_root
        && workspace_root_matches(root, &status.workspace_root)
}

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

fn startup_timeout(
    root: &Path,
    pid: u32,
    started: Instant,
    last_error: Option<String>,
) -> anyhow::Error {
    anyhow!(
        "packet28d pid {pid} did not become ready within {} ms (startup readiness phase, \
         elapsed {} ms); it was left running to finish startup (log: {}; last probe: {})",
        BOOTSTRAP_STARTUP_TIMEOUT.as_millis(),
        started.elapsed().as_millis(),
        log_path(root).display(),
        last_error.as_deref().unwrap_or("none")
    )
}

fn instance_released(root: &Path) -> Result<bool> {
    daemon_instance_released(root).with_context(|| {
        format!(
            "failed to probe packet28d instance authority '{}'",
            daemon_instance_lock_path(root).display()
        )
    })
}

/// Removes endpoint files left by a daemon that no longer owns the workspace.
fn remove_stale_endpoint_files(root: &Path) -> Result<()> {
    for path in [
        socket_path(root),
        workspace_socket_path(root),
        ready_path(root),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove stale runtime file '{}'", path.display())
                })
            }
        }
    }
    Ok(())
}

fn spawn_daemon(root: &Path) -> Result<Child> {
    let binary = std::env::current_exe().context("failed to resolve packet28d executable")?;
    let log_path = log_path(root);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create daemon log dir '{}'", parent.display()))?;
    }
    let open_log = || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))
    };
    Command::new(binary)
        .arg("serve")
        .arg("--root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(open_log()?))
        .stderr(Stdio::from(open_log()?))
        .spawn()
        .context("failed to spawn packet28d")
}
