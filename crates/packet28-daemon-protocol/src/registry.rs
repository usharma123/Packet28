//! Additive, versioned registry status and pagination protocol.
//!
//! These messages intentionally live outside [`crate::message::DaemonRequest`]
//! and [`crate::message::DaemonResponse`]. Those enums are frozen for the 0.2
//! compatibility line, while new registry capabilities use versioned wire
//! tags that can evolve through a later protocol version.
//!
//! # Examples
//!
//! A V1 page request is an additive wire message, not a legacy
//! [`crate::DaemonRequest`] variant:
//!
//! ```
//! use packet28_daemon_protocol::registry::{
//!     DaemonRegistryRequestV1, TaskListPageRequestV1,
//! };
//! use packet28_daemon_protocol::DaemonRequest;
//!
//! let request = DaemonRegistryRequestV1::TaskListPage {
//!     request: TaskListPageRequestV1::default(),
//! };
//! let wire = serde_json::to_value(request)?;
//!
//! assert_eq!(wire["type"], "task_list_page_v1");
//! assert!(serde_json::from_value::<DaemonRequest>(wire).is_err());
//! # Ok::<(), serde_json::Error>(())
//! ```

use serde::{Deserialize, Serialize};

use crate::index::DaemonIndexStatusResponse;
use crate::message::DaemonStatus;
use crate::task::{TaskRecord, WatchRegistration};

/// Default number of registry records requested per page.
pub const DEFAULT_REGISTRY_PAGE_LIMIT: usize = 128;
/// Maximum number of registry records accepted in one page request.
pub const MAX_REGISTRY_PAGE_LIMIT: usize = 256;
/// Maximum compact JSON size of one paginated registry record.
///
/// This is lower than the frame limit so a valid page can always include its
/// cursor and snapshot metadata.
pub const MAX_REGISTRY_PAGE_ITEM_BYTES: usize = 1024 * 1024;
/// Maximum compact JSON size of one registry page response.
pub const MAX_REGISTRY_PAGE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum compact JSON size of a liveness-oriented registry status response.
pub const MAX_DAEMON_STATUS_V1_RESPONSE_BYTES: usize = 1024 * 1024;

/// Registry revision fenced to one daemon instance.
///
/// The instance identifier prevents a revision counter that resets after a
/// daemon restart from accepting a cursor issued by the previous process.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
#[serde(default, deny_unknown_fields)]
pub struct RegistryRevisionV1 {
    /// Opaque identifier generated once for the serving daemon process.
    pub instance_id: String,
    /// Monotonic in-process registry mutation revision.
    pub revision: u64,
}

impl std::fmt::Display for RegistryRevisionV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.instance_id, self.revision)
    }
}

/// Additive registry requests supported by the V1 extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRegistryRequestV1 {
    /// Requests bounded daemon liveness metadata and registry counts.
    #[serde(rename = "registry_status_v1")]
    Status,
    /// Requests one task-registry page.
    #[serde(rename = "task_list_page_v1")]
    TaskListPage {
        /// Cursor, snapshot, and page-size parameters.
        request: TaskListPageRequestV1,
    },
    /// Requests one watch-registry page.
    #[serde(rename = "watch_list_page_v1")]
    WatchListPage {
        /// Filter, cursor, snapshot, and page-size parameters.
        request: WatchListPageRequestV1,
    },
}

/// Additive registry responses supported by the V1 extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRegistryResponseV1 {
    /// Bounded daemon liveness metadata and registry counts.
    #[serde(rename = "registry_status_v1")]
    Status {
        /// Versioned status payload.
        status: Box<DaemonStatusV1>,
    },
    /// One task-registry page.
    #[serde(rename = "task_list_page_v1")]
    TaskListPage {
        /// Versioned page payload.
        page: TaskListPageV1,
    },
    /// One watch-registry page.
    #[serde(rename = "watch_list_page_v1")]
    WatchListPage {
        /// Versioned page payload.
        page: WatchListPageV1,
    },
    /// A bounded request or compatibility error.
    #[serde(rename = "error")]
    Error {
        /// Human-readable failure detail.
        message: String,
    },
}

/// Bounded daemon status with explicit registry semantics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonStatusV1 {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Packet28 daemon version.
    pub version: String,
    /// Active daemon endpoint.
    pub socket_path: String,
    /// Canonical workspace root.
    pub workspace_root: String,
    /// Daemon start time.
    pub started_at_unix: u64,
    /// Readiness publication time.
    pub ready_at_unix: Option<u64>,
    /// Daemon log path.
    pub log_path: String,
    /// Process uptime.
    pub uptime_secs: u64,
    /// Total number of task records.
    pub task_count: usize,
    /// Total number of watch records.
    pub watch_count: usize,
    /// Instance-fenced revision for V1 registry pages, or `None` for a
    /// normalized legacy status returned by a pre-extension daemon.
    pub registry_revision: Option<RegistryRevisionV1>,
    /// Whether large index details were omitted to preserve the status bound.
    pub index_truncated: bool,
    /// Current index status when it fits the response bound.
    pub index: Option<DaemonIndexStatusResponse>,
}

impl DaemonStatusV1 {
    /// Normalizes an exhaustive legacy status without inventing truncation or
    /// revision metadata.
    pub fn from_legacy(status: DaemonStatus) -> Self {
        Self {
            pid: status.pid,
            version: status.version,
            socket_path: status.socket_path,
            workspace_root: status.workspace_root,
            started_at_unix: status.started_at_unix,
            ready_at_unix: status.ready_at_unix,
            log_path: status.log_path,
            uptime_secs: status.uptime_secs,
            task_count: status.tasks.len(),
            watch_count: status.watches.len(),
            registry_revision: None,
            index_truncated: false,
            index: status.index,
        }
    }
}

/// Cursor request for a lexicographically ordered task-registry page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskListPageRequestV1 {
    /// Instance-fenced revision returned by the first page, or `None` for the
    /// first page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_revision: Option<RegistryRevisionV1>,
    /// Exclusive task identifier cursor from the preceding response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_task_id: Option<String>,
    /// Requested record count, in `1..=`[`MAX_REGISTRY_PAGE_LIMIT`].
    pub limit: usize,
}

impl Default for TaskListPageRequestV1 {
    fn default() -> Self {
        Self {
            snapshot_revision: None,
            after_task_id: None,
            limit: DEFAULT_REGISTRY_PAGE_LIMIT,
        }
    }
}

/// A task record that a page could not carry because it exceeds the per-record
/// pagination bound. Listing skips it (instead of failing) and reports it here
/// so callers can still enumerate the store and target it for repair/retention.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct OversizedTaskRecordV1 {
    /// Identifier of the omitted task.
    pub task_id: String,
    /// Compact-JSON size of the omitted record, in bytes.
    pub encoded_bytes: u64,
}

/// One bounded, lexicographically ordered task-registry page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskListPageV1 {
    /// Instance-fenced revision that must be echoed by the next request.
    pub snapshot_revision: RegistryRevisionV1,
    /// Records in task-identifier order.
    pub tasks: Vec<TaskRecord>,
    /// Exclusive cursor for the next page, or `None` at the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_task_id: Option<String>,
    /// Total number of task records at [`Self::snapshot_revision`].
    pub total: usize,
    /// Records skipped on this page because they exceed the per-record bound.
    /// Skipping keeps listing available so one oversized record cannot poison
    /// the whole enumeration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_oversized: Vec<OversizedTaskRecordV1>,
}

/// Cursor request for a lexicographically ordered watch-registry page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchListPageRequestV1 {
    /// Instance-fenced revision returned by the first page, or `None` for the
    /// first page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_revision: Option<RegistryRevisionV1>,
    /// Optional task filter applied before cursoring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Exclusive watch identifier cursor from the preceding response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_watch_id: Option<String>,
    /// Requested record count, in `1..=`[`MAX_REGISTRY_PAGE_LIMIT`].
    pub limit: usize,
}

impl Default for WatchListPageRequestV1 {
    fn default() -> Self {
        Self {
            snapshot_revision: None,
            task_id: None,
            after_watch_id: None,
            limit: DEFAULT_REGISTRY_PAGE_LIMIT,
        }
    }
}

/// One bounded, lexicographically ordered watch-registry page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchListPageV1 {
    /// Instance-fenced revision that must be echoed by the next request.
    pub snapshot_revision: RegistryRevisionV1,
    /// Records in watch-identifier order after applying the task filter.
    pub watches: Vec<WatchRegistration>,
    /// Exclusive cursor for the next page, or `None` at the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_watch_id: Option<String>,
    /// Total matching records at [`Self::snapshot_revision`].
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_status_normalization_derives_exhaustive_counts() {
        let status = DaemonStatusV1::from_legacy(DaemonStatus {
            tasks: vec![TaskRecord {
                task_id: "legacy-task".to_string(),
                ..TaskRecord::default()
            }],
            watches: vec![WatchRegistration {
                watch_id: "legacy-watch".to_string(),
                ..WatchRegistration::default()
            }],
            ..DaemonStatus::default()
        });

        assert_eq!(
            (
                status.task_count,
                status.watch_count,
                status.registry_revision
            ),
            (1, 1, None)
        );
    }

    #[test]
    fn page_requests_default_to_the_bounded_page_size() {
        let task_request: DaemonRegistryRequestV1 = serde_json::from_value(serde_json::json!({
            "type": "task_list_page_v1",
            "request": {}
        }))
        .unwrap();
        let watch_request: DaemonRegistryRequestV1 = serde_json::from_value(serde_json::json!({
            "type": "watch_list_page_v1",
            "request": { "task_id": "task-a" }
        }))
        .unwrap();

        assert!(matches!(
            task_request,
            DaemonRegistryRequestV1::TaskListPage {
                request: TaskListPageRequestV1 {
                    limit: DEFAULT_REGISTRY_PAGE_LIMIT,
                    ..
                }
            }
        ));
        assert!(matches!(
            watch_request,
            DaemonRegistryRequestV1::WatchListPage {
                request: WatchListPageRequestV1 {
                    limit: DEFAULT_REGISTRY_PAGE_LIMIT,
                    ..
                }
            }
        ));
    }
}
