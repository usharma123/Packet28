//! Typed failures returned by daemon persistence and compatibility APIs.

use std::io;
use std::path::{Path, PathBuf};

use packet28_daemon_protocol::frame::FrameError;
use thiserror::Error;

/// Failure produced by reusable `packet28-daemon-core` operations.
///
/// Filesystem, codec, and framing variants retain their concrete source in the
/// standard [`std::error::Error::source`] chain. Policy and validation
/// variants expose their rejected values directly. The enum is non-exhaustive
/// so future storage backends can add precise failure modes without forcing
/// downstream exhaustive matches.
///
/// # Examples
///
/// ```
/// use std::error::Error as _;
/// use std::io::Cursor;
///
/// use packet28_daemon_core::{read_socket_message, DaemonCoreError, DaemonRequest};
///
/// let mut empty_frame = Cursor::new(0_u64.to_be_bytes());
/// let error = read_socket_message::<_, DaemonRequest>(&mut empty_frame).unwrap_err();
///
/// assert!(matches!(error, DaemonCoreError::Frame { .. }));
/// assert!(error.source().is_some());
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DaemonCoreError {
    /// A filesystem or file-lock operation failed.
    #[error("{operation} {}: {source}", path.display())]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Path being accessed.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: io::Error,
    },

    /// Persisted daemon JSON could not be encoded or decoded.
    #[error("{operation} {}: {source}", path.display())]
    Json {
        /// Encoding or decoding operation that failed.
        operation: &'static str,
        /// Persisted JSON path.
        path: PathBuf,
        /// Typed JSON codec failure.
        #[source]
        source: serde_json::Error,
    },

    /// A task registry would exceed the supported serialized-size bound.
    #[error(
        "task registry {} is {encoded_bytes} bytes; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    TaskRegistryTooLarge {
        /// Registry path that was not replaced.
        path: PathBuf,
        /// Encoded size of the rejected registry.
        encoded_bytes: u64,
        /// Maximum supported encoded size.
        max_bytes: u64,
    },

    /// A watch registry would exceed the supported serialized-size bound.
    #[error(
        "watch registry {} is {encoded_bytes} bytes; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    WatchRegistryTooLarge {
        /// Registry path that was not replaced or decoded.
        path: PathBuf,
        /// Encoded size of the rejected registry.
        encoded_bytes: u64,
        /// Maximum supported encoded size.
        max_bytes: u64,
    },

    /// A registry record cannot fit the bounded crash-recovery journal.
    #[error(
        "task registry {} requires a {journal_bytes}-byte retention journal; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    TaskRegistryRetentionEnvelopeTooLarge {
        /// Registry path that was not replaced.
        path: PathBuf,
        /// Encoded size of the rejected maximum candidate journal.
        journal_bytes: u64,
        /// Maximum supported encoded journal size.
        max_bytes: u64,
    },

    /// An active-task record would exceed the supported serialized-size bound.
    #[error(
        "active-task record {} is {encoded_bytes} bytes; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    ActiveTaskRecordTooLarge {
        /// Active-task path that was not replaced or decoded.
        path: PathBuf,
        /// Encoded size of the rejected record.
        encoded_bytes: u64,
        /// Maximum supported encoded size.
        max_bytes: u64,
    },

    /// An active-task record violates an invariant required by all writers.
    #[error("invalid active-task record {}: {message}", path.display())]
    InvalidActiveTaskRecord {
        /// Active-task path that was rejected.
        path: PathBuf,
        /// Stable explanation of the invalid record.
        message: String,
    },

    /// A task registry violates an invariant required by all supported writers.
    #[error("invalid task registry {}: {message}", path.display())]
    InvalidTaskRegistry {
        /// Registry path that was rejected.
        path: PathBuf,
        /// Stable explanation of the invalid registry shape.
        message: String,
    },

    /// Task and watch registries do not describe one durable checkpoint.
    #[error(
        "task/watch registry checkpoint generations disagree under {}: task={task_generation:?}, watch={watch_generation:?}",
        root.display()
    )]
    RegistryCheckpointGenerationMismatch {
        /// Workspace root containing the mismatched registry documents.
        root: PathBuf,
        /// Generation stored in the task registry root, if present.
        task_generation: Option<u64>,
        /// Generation stored in the watch registry root, if present.
        watch_generation: Option<u64>,
    },

    /// A standalone registry writer attempted to mutate paired authority.
    #[error(
        "standalone {registry} registry write rejected under paired checkpoint authority at {}: task={task_generation:?}, watch={watch_generation:?}",
        root.display()
    )]
    RegistryCheckpointRequired {
        /// Workspace root containing paired registry authority.
        root: Box<PathBuf>,
        /// Registry half whose standalone writer was rejected.
        registry: &'static str,
        /// Generation stored in the task registry root, if present.
        task_generation: Option<u64>,
        /// Generation stored in the watch registry root, if present.
        watch_generation: Option<u64>,
    },

    /// Task and watch records violate their cross-registry relationship.
    #[error("invalid task/watch registry checkpoint under {}: {message}", root.display())]
    InvalidTaskWatchRegistry {
        /// Workspace root containing the rejected checkpoint.
        root: PathBuf,
        /// Stable explanation of the invalid relationship.
        message: String,
    },

    /// A proposed registry delta violates an identifier or batch invariant.
    #[error("invalid task/watch registry delta under {}: {message}", root.display())]
    InvalidRegistryDeltaBatch {
        /// Workspace root whose delta was rejected.
        root: PathBuf,
        /// Stable explanation of the invalid delta.
        message: String,
    },

    /// A complete registry-delta WAL artifact violates durable integrity.
    #[error("invalid task/watch registry delta WAL {}: {message}", path.display())]
    InvalidRegistryDeltaWal {
        /// WAL path whose bytes were rejected.
        path: PathBuf,
        /// Stable explanation of malformed data, checksum, or revision failure.
        message: String,
    },

    /// A registry-delta frame would exceed the supported serialized-size bound.
    #[error(
        "task/watch registry delta frame {} is {encoded_bytes} bytes; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    RegistryDeltaFrameTooLarge {
        /// WAL path that was not appended.
        path: PathBuf,
        /// Encoded payload size of the rejected frame.
        encoded_bytes: u64,
        /// Maximum supported encoded payload size.
        max_bytes: u64,
    },

    /// The registry-delta WAL would exceed its bounded storage envelope.
    #[error(
        "task/watch registry delta WAL {} would be {encoded_bytes} bytes; maximum supported size is {max_bytes} bytes",
        path.display()
    )]
    RegistryDeltaWalTooLarge {
        /// WAL path that was not extended.
        path: PathBuf,
        /// Resulting WAL size of the rejected append.
        encoded_bytes: u64,
        /// Maximum supported WAL size.
        max_bytes: u64,
    },

    /// A registry-delta append does not continue the durable revision stream.
    #[error(
        "task/watch registry delta revision mismatch at {}: expected first revision {expected_first}, received {actual_first}..={actual_last}",
        path.display()
    )]
    RegistryDeltaRevisionMismatch {
        /// WAL path whose revision stream was not changed.
        path: PathBuf,
        /// Required first revision for the next frame.
        expected_first: u64,
        /// First revision supplied by the caller.
        actual_first: u64,
        /// Last revision supplied by the caller.
        actual_last: u64,
    },

    /// The monotonic task/watch checkpoint generation cannot advance.
    #[error(
        "task/watch registry checkpoint generation is exhausted under {}: task={task_generation:?}, watch={watch_generation:?}",
        root.display()
    )]
    RegistryCheckpointGenerationExhausted {
        /// Workspace root containing the exhausted checkpoint.
        root: PathBuf,
        /// Generation stored in the task registry root, if present.
        task_generation: Option<u64>,
        /// Generation stored in the watch registry root, if present.
        watch_generation: Option<u64>,
    },

    /// A task identifier cannot be represented safely by task storage paths.
    #[error("invalid task storage identifier for {}: {message}", path.display())]
    InvalidTaskStorageIdentifier {
        /// Storage path that was not created or changed.
        path: PathBuf,
        /// Stable explanation of the invalid identifier.
        message: String,
    },

    /// A complete task-event frame violates durable log integrity.
    #[error("invalid task event frame {}: {message}", path.display())]
    InvalidTaskEventFrame {
        /// Event-log path whose frame was rejected.
        path: PathBuf,
        /// Stable explanation of malformed data, identity, or sequence failure.
        message: String,
    },

    /// A task's event history was quarantined and its continuation moved to a
    /// linked successor; the superseded identity accepts no further events.
    #[error(
        "task {task_id:?} was superseded by {successor_task_id:?} after its event history was quarantined"
    )]
    TaskSuperseded {
        /// Fenced task identifier that was addressed.
        task_id: String,
        /// Linked task that continues the work.
        successor_task_id: String,
    },

    /// A task record is an archived tombstone and cannot be continued.
    #[error(
        "task {task_id:?} is an archived record tombstone; its original record is preserved by {archive_file:?}"
    )]
    TaskArchived {
        /// Fenced task identifier that was addressed.
        task_id: String,
        /// Digest-named archive file relative to the workspace state directory.
        archive_file: String,
    },

    /// A durable mutation completed before its storage authority was lost.
    ///
    /// The mutation must not be retried blindly: its bytes may already be
    /// durable even though Packet28 could not prove that the canonical
    /// filename and lock still named the authenticated objects at return.
    #[error(
        "{operation} may have committed at {} before storage authority was lost: {source}",
        path.display()
    )]
    StorageMutationAuthorityLost {
        /// Durable mutation whose completion could not be authenticated.
        operation: &'static str,
        /// Canonical path whose final binding became uncertain.
        path: PathBuf,
        /// Attachment or descriptor/name authentication failure.
        #[source]
        source: io::Error,
    },

    /// Authority JSON exceeded a pre-materialization resource budget.
    #[error(
        "{authority} JSON {} exceeds the {resource} budget: observed {observed}, maximum {max}"
        ,
        path.display()
    )]
    AuthorityJsonLimitExceeded {
        /// Authority file that was rejected without materializing it.
        path: PathBuf,
        /// Stable authority kind, such as `task registry`.
        authority: &'static str,
        /// Stable resource name, such as `value nodes`.
        resource: &'static str,
        /// First count that exceeded the budget.
        observed: u64,
        /// Maximum accepted count.
        max: u64,
    },

    /// A legacy daemon-core socket frame could not be encoded or decoded.
    #[error("{operation} daemon socket frame: {source}")]
    Frame {
        /// Framing operation that failed.
        operation: &'static str,
        /// Typed protocol framing failure.
        #[source]
        source: FrameError,
    },

    /// A retention request did not specify a usable bound.
    #[error("invalid task-retention policy: {message}")]
    InvalidRetentionPolicy {
        /// Explanation of the rejected policy.
        message: &'static str,
    },

    /// The Packet28 state root was not a real directory contained by the workspace.
    #[error(
        "unsafe Packet28 state root {} for workspace {}: {reason}",
        state_root.display(),
        workspace_root.display()
    )]
    UnsafeStateRoot {
        /// Canonical workspace root.
        workspace_root: PathBuf,
        /// State path that failed validation.
        state_root: PathBuf,
        /// Failed containment or file-type invariant.
        reason: &'static str,
    },

    /// Cleanup was requested while the daemon may still own task state.
    #[error("task retention cannot apply while daemon owns task storage at {}", path.display())]
    RetentionBlockedByDaemon {
        /// Lifecycle lock or readiness marker requiring the daemon to stop.
        path: PathBuf,
    },

    /// Another daemon process already owns this workspace.
    #[error("another Packet28 daemon already owns {}", path.display())]
    DaemonInstanceAlreadyRunning {
        /// Persistent instance-lock path owned by the running daemon.
        path: PathBuf,
    },

    /// A candidate changed after inspection and was not removed.
    #[error("task-retention candidate changed during cleanup: {}", path.display())]
    RetentionCandidateChanged {
        /// Candidate path whose identity changed.
        path: PathBuf,
    },

    /// Applying retention is unsupported on this platform.
    #[error("task-retention deletion is unsupported on this platform; use dry-run inspection")]
    RetentionApplyUnsupported,
}

impl DaemonCoreError {
    pub(crate) fn io(operation: &'static str, path: impl AsRef<Path>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    pub(crate) fn json(
        operation: &'static str,
        path: impl AsRef<Path>,
        source: serde_json::Error,
    ) -> Self {
        Self::Json {
            operation,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    pub(crate) fn frame(operation: &'static str, source: FrameError) -> Self {
        Self::Frame { operation, source }
    }

    /// Returns recovery guidance for this category of failure.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Io { .. } => "Check that the reported path exists and is readable and writable.",
            Self::Json { .. } => {
                "Repair or regenerate the reported daemon state file before retrying."
            }
            Self::TaskRegistryTooLarge { .. } => {
                "Reduce completed task history before saving the task registry."
            }
            Self::WatchRegistryTooLarge { .. } => {
                "Reduce persisted watch history before saving the watch registry."
            }
            Self::TaskRegistryRetentionEnvelopeTooLarge { .. } => {
                "Reduce the largest task record before saving the task registry."
            }
            Self::ActiveTaskRecordTooLarge { .. } => {
                "Reduce active-task metadata before saving the record."
            }
            Self::InvalidActiveTaskRecord { .. } => {
                "Provide a non-empty active task identifier."
            }
            Self::InvalidTaskRegistry { .. } => {
                "Make each non-empty task map key match its embedded task identifier."
            }
            Self::RegistryCheckpointGenerationMismatch { .. } => {
                "Restore task and watch registries from the same checkpoint before restarting packet28d."
            }
            Self::RegistryCheckpointRequired { .. } => {
                "Write task and watch registries through one paired checkpoint."
            }
            Self::InvalidTaskWatchRegistry { .. } => {
                "Repair task and watch ownership references before retrying."
            }
            Self::InvalidRegistryDeltaBatch { .. } => {
                "Correct the delta identifiers and overlapping mutations before retrying."
            }
            Self::InvalidRegistryDeltaWal { .. } => {
                "Restore the registry delta WAL from a valid checkpoint or investigate storage corruption before restarting packet28d."
            }
            Self::RegistryDeltaFrameTooLarge { .. } => {
                "Split the registry mutation into smaller atomic batches."
            }
            Self::RegistryDeltaWalTooLarge { .. } => {
                "Publish a task/watch checkpoint before appending more registry deltas."
            }
            Self::RegistryDeltaRevisionMismatch { .. } => {
                "Reload durable registry authority and retry with the next contiguous revision."
            }
            Self::RegistryCheckpointGenerationExhausted { .. } => {
                "Archive and reinitialize daemon task/watch registry state before retrying."
            }
            Self::InvalidTaskStorageIdentifier { .. } => {
                "Use a non-empty task identifier whose derived storage key is portable and unambiguous."
            }
            Self::InvalidTaskEventFrame { .. } => {
                "Repair or restore the event log so every complete frame is valid, task-bound, and sequence-contiguous."
            }
            Self::TaskSuperseded { .. } => {
                "Continue with the successor task; the superseded task keeps its quarantined history for inspection."
            }
            Self::TaskArchived { .. } => {
                "Start a new task; inspect the archived original with `Packet28 daemon storage show-archived-record`."
            }
            Self::StorageMutationAuthorityLost { .. } => {
                "Do not retry blindly; inspect the canonical file and registry under an authenticated lock first."
            }
            Self::AuthorityJsonLimitExceeded { .. } => {
                "Reduce the authority document's nesting, entry count, or decoded string content."
            }
            Self::Frame { .. } => {
                "Verify that the peer uses the same Packet28 daemon protocol version."
            }
            Self::InvalidRetentionPolicy { .. } => {
                "Specify --max-age-seconds, --max-bytes, or both."
            }
            Self::UnsafeStateRoot { .. } => {
                "Replace symlinked or non-directory Packet28 state with a real workspace-local directory."
            }
            Self::RetentionBlockedByDaemon { .. } => {
                "Stop packet28d before applying task retention."
            }
            Self::DaemonInstanceAlreadyRunning { .. } => {
                "Use the running daemon or stop it before starting another instance."
            }
            Self::RetentionCandidateChanged { .. } => {
                "Re-run the dry-run inspection before retrying cleanup."
            }
            Self::RetentionApplyUnsupported => {
                "Run retention in dry-run mode and remove data manually on this platform."
            }
        }
    }
}

/// Result returned by fallible `packet28-daemon-core` operations.
pub type Result<T> = std::result::Result<T, DaemonCoreError>;
