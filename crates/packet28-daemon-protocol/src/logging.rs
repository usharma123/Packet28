//! Launcher contract for process-owned background diagnostic logs.
//!
//! A launcher that starts `packet28d serve` or the Claude HTTP hook server in
//! the background passes [`MANAGED_LOG_FLAG`] and detaches the child's stdout
//! and stderr. The child then owns its workspace log file and rotates it by
//! size while it runs, so log growth does not depend on a restart or on the
//! launcher staying alive. Without the flag a process keeps ordinary stderr
//! diagnostics for foreground use.
//!
//! The rotation threshold honours [`RUNTIME_LOG_MAX_BYTES_ENV`], which the
//! child inherits from its launcher. [`RUNTIME_LOG_BACKUPS`] numbered
//! generations (`<log>.1` .. `<log>.3`) are retained beside the active file.

use std::path::{Path, PathBuf};

use crate::paths::daemon_dir;

/// Command-line flag that asks a background child to own its workspace log.
pub const MANAGED_LOG_FLAG: &str = "--managed-log";

/// Environment override for the rotation threshold, in bytes.
///
/// A missing, zero, or unparsable value selects
/// [`DEFAULT_RUNTIME_LOG_MAX_BYTES`].
pub const RUNTIME_LOG_MAX_BYTES_ENV: &str = "PACKET28_DAEMON_LOG_MAX_BYTES";

/// Default size of the active log and of each retained generation.
pub const DEFAULT_RUNTIME_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Number of numbered generations retained beside the active log.
pub const RUNTIME_LOG_BACKUPS: usize = 3;

/// File name of the Claude HTTP hook server log in the daemon directory.
pub const HOOK_HTTP_LOG_FILE_NAME: &str = "packet28-hook-http.log";

/// Returns the configured rotation threshold for this process.
pub fn runtime_log_max_bytes() -> u64 {
    runtime_log_max_bytes_from(std::env::var(RUNTIME_LOG_MAX_BYTES_ENV).ok().as_deref())
}

/// Parses a rotation threshold override, falling back to the default.
pub fn runtime_log_max_bytes_from(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RUNTIME_LOG_MAX_BYTES)
}

/// Returns the Claude HTTP hook server log path for a workspace root.
pub fn hook_http_log_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(HOOK_HTTP_LOG_FILE_NAME)
}

/// Returns the daemon directory as single path components beneath the root.
pub fn daemon_dir_components() -> Vec<&'static str> {
    crate::paths::DAEMON_DIR_NAME.split('/').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_bytes_override_falls_back_for_invalid_values() {
        assert_eq!(runtime_log_max_bytes_from(Some("4096")), 4096);
        assert_eq!(runtime_log_max_bytes_from(Some(" 8192 ")), 8192);
        for invalid in [None, Some("0"), Some("-1"), Some("not-a-number"), Some("")] {
            assert_eq!(
                runtime_log_max_bytes_from(invalid),
                DEFAULT_RUNTIME_LOG_MAX_BYTES,
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn hook_log_and_daemon_components_share_the_daemon_directory() {
        let root = Path::new("/workspace");
        assert_eq!(
            hook_http_log_path(root),
            root.join(".packet28/daemon/packet28-hook-http.log")
        );
        let mut joined = root.to_path_buf();
        for component in daemon_dir_components() {
            joined.push(component);
        }
        assert_eq!(joined, daemon_dir(root));
    }
}
