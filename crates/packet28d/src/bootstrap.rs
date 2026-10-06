//! Lease-guarded daemon bootstrap for clients that do not link daemon authority.
//!
//! `p28` may not depend on `packet28-daemon-core`, so it cannot take the
//! startup lease or probe the instance lease itself. `packet28d start` performs
//! discovery, stale-file cleanup, and spawn under the same leases as the
//! Packet28 CLI, so a stopping daemon's runtime files are never removed and no
//! losing replacement is spawned while it still owns the workspace.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::task_store_lease::{
    acquire_daemon_startup_lease, daemon_instance_lock_path, daemon_instance_released,
};
use packet28_daemon_protocol::paths::{log_path, ready_path, socket_path, workspace_socket_path};

use crate::resolve_root;

/// Bound for a daemon that is stopping or held offline to release workspace
/// authority. It matches the client bootstrap bound that predates authority
/// waiting, so search and hook latency is unchanged; explicit stop and restart
/// use the longer shutdown grace instead.
const BOOTSTRAP_AUTHORITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound for the spawned daemon to publish readiness.
const BOOTSTRAP_READY_TIMEOUT: Duration = Duration::from_secs(10);

const BOOTSTRAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Starts the daemon for `root` unless one is already serving it.
///
/// Holds the startup lease from discovery until the spawned daemon is ready.
/// A daemon that still owns the instance lease without readiness is stopping
/// or held offline; bootstrap waits for it to release authority and, on
/// timeout, fails without removing runtime files or spawning.
///
/// # Errors
///
/// Returns a lease integrity or I/O error, an authority timeout, a stale-file
/// cleanup or spawn error, or an error when the daemon exits or does not
/// become ready in time.
pub fn start(root: PathBuf) -> Result<()> {
    let root = resolve_root(&root);
    let _startup_lease = acquire_daemon_startup_lease(&root)?;
    if wait_for_authority(&root)? == Authority::Serving {
        return Ok(());
    }
    remove_stale_endpoint_files(&root)?;
    let mut daemon = spawn_daemon(&root)?;
    wait_for_ready(&root, &mut daemon)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    Serving,
    Released,
}

/// Waits until a ready daemon owns the workspace or no daemon owns it.
///
/// A serving daemon withdraws readiness before it starts shutting down, so a
/// held instance lease with a readiness marker is a live daemon.
fn wait_for_authority(root: &Path) -> Result<Authority> {
    let deadline = Instant::now() + BOOTSTRAP_AUTHORITY_TIMEOUT;
    loop {
        let released = daemon_instance_released(root).with_context(|| {
            format!(
                "failed to probe packet28d instance authority '{}'",
                daemon_instance_lock_path(root).display()
            )
        })?;
        if released {
            return Ok(Authority::Released);
        }
        if ready_path(root).exists() {
            return Ok(Authority::Serving);
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "packet28d did not release workspace authority '{}' within {} ms; its runtime \
                 files were left in place (log: {})",
                daemon_instance_lock_path(root).display(),
                BOOTSTRAP_AUTHORITY_TIMEOUT.as_millis(),
                log_path(root).display()
            ));
        }
        thread::sleep(BOOTSTRAP_POLL_INTERVAL);
    }
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

/// Waits for the spawned daemon to publish readiness. A daemon still starting
/// after the deadline is left running so it can finish recovery.
fn wait_for_ready(root: &Path, daemon: &mut Child) -> Result<()> {
    let deadline = Instant::now() + BOOTSTRAP_READY_TIMEOUT;
    loop {
        if ready_path(root).exists() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait().context("failed to poll packet28d")? {
            return Err(anyhow!(
                "packet28d exited with {status} before becoming ready (log: {})",
                log_path(root).display()
            ));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "packet28d did not become ready within {} ms (log: {})",
                BOOTSTRAP_READY_TIMEOUT.as_millis(),
                log_path(root).display()
            ));
        }
        thread::sleep(BOOTSTRAP_POLL_INTERVAL);
    }
}
