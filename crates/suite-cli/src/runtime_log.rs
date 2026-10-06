//! Process-owned diagnostics for background Packet28 CLI services.
//!
//! A service launched in the background with the managed-log flag installs
//! one size-rotated workspace log before its startup work. Diagnostics routed
//! through [`diagnostic`] and panics are recorded there; without a managed
//! log, diagnostics keep their stderr destination.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::path::Path;
use std::sync::OnceLock;

use packet28_daemon_core::storage::now_unix;
use packet28_daemon_protocol::logging::{
    daemon_dir_components, runtime_log_max_bytes, RUNTIME_LOG_BACKUPS,
};
use packet28_daemon_protocol::paths::daemon_dir;
use packet28_state_fs::{LogRotation, RotatingLog, StateDir};

struct ManagedLog {
    label: &'static str,
    log: RotatingLog,
}

static MANAGED_LOG: OnceLock<ManagedLog> = OnceLock::new();

/// Routes this process's diagnostics into `file_name` in the daemon directory.
///
/// Installation is best-effort: when the log cannot be prepared, diagnostics
/// keep their stderr destination and the service continues.
pub(crate) fn install_managed_log(root: &Path, file_name: &str, label: &'static str) {
    if MANAGED_LOG.get().is_some() {
        return;
    }
    let Some(log) = open_managed_log(root, file_name) else {
        return;
    };
    if MANAGED_LOG.set(ManagedLog { label, log }).is_ok() {
        install_panic_hook();
    }
}

fn open_managed_log(root: &Path, file_name: &str) -> Option<RotatingLog> {
    std::fs::create_dir_all(daemon_dir(root)).ok()?;
    let directory = StateDir::open(root, &daemon_dir_components(), false).ok()?;
    let policy = LogRotation::new(runtime_log_max_bytes(), RUNTIME_LOG_BACKUPS);
    RotatingLog::new(directory, file_name, policy).ok()
}

/// Records a diagnostic in the managed log, or writes it to stderr.
pub(crate) fn diagnostic(message: &str) {
    if !record_if_managed(message) {
        eprintln!("{message}");
    }
}

/// Records a diagnostic only when a managed log is installed.
///
/// Returns `false` when the process has no managed log.
pub(crate) fn record_if_managed(message: &str) -> bool {
    let Some(managed) = MANAGED_LOG.get() else {
        return false;
    };
    let _ = managed
        .log
        .append_record(&format!("[{} {}] {message}", managed.label, now_unix()));
    true
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(managed) = MANAGED_LOG.get() {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            let mut record = format!("[{} {}] thread '{name}' {info}", managed.label, now_unix());
            let backtrace = Backtrace::capture();
            if backtrace.status() == BacktraceStatus::Captured {
                record.push_str(&format!("\nstack backtrace:\n{backtrace}"));
            }
            let _ = managed.log.try_append_record(&record);
        }
        previous(info);
    }));
}
