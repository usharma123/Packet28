//! Process-owned daemon diagnostics.
//!
//! A foreground daemon writes diagnostics to stderr. A background daemon
//! started with the managed-log flag installs one [`RotatingLog`] for its
//! workspace before startup work begins; every diagnostic and panic is then
//! recorded there and rotated by size while the daemon runs.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::path::Path;
use std::sync::OnceLock;

use packet28_daemon_core::storage::now_unix;
use packet28_daemon_protocol::logging::{
    daemon_dir_components, runtime_log_max_bytes, RUNTIME_LOG_BACKUPS,
};
use packet28_daemon_protocol::paths::{daemon_dir, LOG_FILE_NAME};
use packet28_state_fs::{LogRotation, RotatingLog, StateDir};

static MANAGED_LOG: OnceLock<RotatingLog> = OnceLock::new();

/// Routes diagnostics for this process into the workspace `packet28d.log`.
///
/// Installation is best-effort: when the log directory cannot be prepared,
/// diagnostics keep their stderr destination and the daemon continues.
pub(crate) fn install_managed_log(root: &Path) {
    if MANAGED_LOG.get().is_some() {
        return;
    }
    let Some(log) = open_managed_log(root) else {
        return;
    };
    if MANAGED_LOG.set(log).is_ok() {
        install_panic_hook();
    }
}

fn open_managed_log(root: &Path) -> Option<RotatingLog> {
    std::fs::create_dir_all(daemon_dir(root)).ok()?;
    let directory = StateDir::open(root, &daemon_dir_components(), false).ok()?;
    let policy = LogRotation::new(runtime_log_max_bytes(), RUNTIME_LOG_BACKUPS);
    RotatingLog::new(directory, LOG_FILE_NAME, policy).ok()
}

pub(crate) fn daemon_log(message: &str) {
    let record = format!("[packet28d {}] {message}", now_unix());
    match MANAGED_LOG.get() {
        Some(log) => {
            let _ = log.append_record(&record);
        }
        None => eprintln!("{record}"),
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(log) = MANAGED_LOG.get() {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            let mut record = format!("[packet28d {}] thread '{name}' {info}", now_unix());
            let backtrace = Backtrace::capture();
            if backtrace.status() == BacktraceStatus::Captured {
                record.push_str(&format!("\nstack backtrace:\n{backtrace}"));
            }
            let _ = log.try_append_record(&record);
        }
        previous(info);
    }));
}
