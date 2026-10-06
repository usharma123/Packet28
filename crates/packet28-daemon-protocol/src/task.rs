//! Task lifecycle, launch, watch, and registry payloads.

use super::*;

/// Runtime task state represented on the wire as compatible boolean fields.
///
/// The enum prevents callers from constructing contradictory combinations such
/// as a task that is both cancelled and pending a replan. Serialization keeps
/// the original `running`, `cancel_requested`, and `pending_replan` fields so
/// existing task registries and clients remain compatible. The additive
/// `cancelled` field is emitted only for the terminal state and distinguishes
/// it from an in-progress cancellation; older readers continue to observe
/// `cancel_requested = true`, while existing states keep their exact shape.
///
/// # Examples
///
/// Lifecycle changes go through checked transitions:
///
/// ```
/// use packet28_daemon_protocol::task::TaskLifecycle;
///
/// let mut lifecycle = TaskLifecycle::Idle;
/// lifecycle.start()?;
/// assert!(lifecycle.is_running());
/// assert!(!lifecycle.finish_run()?);
/// assert_eq!(lifecycle, TaskLifecycle::Idle);
/// # Ok::<(), packet28_daemon_protocol::task::TaskLifecycleTransitionError>(())
/// ```
///
/// The former parallel boolean representation cannot be constructed through
/// the public API, so contradictory states are rejected at compile time:
///
/// ```compile_fail
/// use packet28_daemon_protocol::task::TaskLifecycle;
///
/// let _invalid = TaskLifecycle {
///     running: true,
///     cancel_requested: true,
///     pending_replan: true,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskLifecycle {
    /// The task is registered but has no work in flight.
    #[default]
    Idle,
    /// Work is queued because a watch requested a replan.
    ReplanPending,
    /// The stored sequence is currently executing.
    Running,
    /// A queued replan has been durably claimed for execution.
    ///
    /// This state is distinct from [`Self::Running`] so another crash before
    /// completion can restore the still-owned replan instead of treating it as
    /// ordinary interrupted work. Recovery is at-least-once: reducers must
    /// tolerate replay if a process crashes after a reducer side effect but
    /// before the terminal task checkpoint.
    RunningRecoveredReplan,
    /// The sequence is executing and must run again after it completes.
    RunningReplanPending,
    /// Cancellation owns the task and no new work may start.
    Cancelling {
        /// Whether sequence work was active when cancellation began.
        was_running: bool,
    },
    /// Cancellation completed after all owned work and children quiesced.
    Cancelled,
}

impl TaskLifecycle {
    /// Returns whether sequence work is currently active.
    pub const fn is_running(self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::RunningRecoveredReplan
                | Self::RunningReplanPending
                | Self::Cancelling { was_running: true }
        )
    }

    /// Returns whether cancellation has been requested.
    pub const fn is_cancelling(self) -> bool {
        matches!(self, Self::Cancelling { .. })
    }

    /// Returns whether cancellation completed and the record is terminal.
    pub const fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Returns whether another sequence run is pending.
    pub const fn has_pending_replan(self) -> bool {
        matches!(self, Self::ReplanPending | Self::RunningReplanPending)
    }

    /// Starts one sequence generation.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLifecycleTransitionError`] when work is already running or
    /// cancellation owns the task.
    pub fn start(&mut self) -> Result<(), TaskLifecycleTransitionError> {
        match *self {
            Self::Idle | Self::ReplanPending => {
                *self = Self::Running;
                Ok(())
            }
            from => Err(TaskLifecycleTransitionError {
                from,
                action: TaskLifecycleAction::Start,
            }),
        }
    }

    /// Claims one queued replan while retaining durable restart provenance.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLifecycleTransitionError`] unless a replan is pending.
    pub fn start_durable_replan(&mut self) -> Result<(), TaskLifecycleTransitionError> {
        match *self {
            Self::ReplanPending => {
                *self = Self::RunningRecoveredReplan;
                Ok(())
            }
            from => Err(TaskLifecycleTransitionError {
                from,
                action: TaskLifecycleAction::StartDurableReplan,
            }),
        }
    }

    /// Requests a replan without starting duplicate work.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLifecycleTransitionError`] when cancellation owns the
    /// task.
    pub fn request_replan(&mut self) -> Result<bool, TaskLifecycleTransitionError> {
        let should_start = match *self {
            Self::Idle => {
                *self = Self::ReplanPending;
                true
            }
            Self::Running => {
                *self = Self::RunningReplanPending;
                false
            }
            Self::RunningRecoveredReplan => {
                *self = Self::RunningReplanPending;
                false
            }
            Self::ReplanPending | Self::RunningReplanPending => false,
            Self::Cancelling { .. } | Self::Cancelled => {
                return Err(TaskLifecycleTransitionError {
                    from: *self,
                    action: TaskLifecycleAction::RequestReplan,
                });
            }
        };
        Ok(should_start)
    }

    /// Completes the active generation and reports whether a rerun is pending.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLifecycleTransitionError`] when no generation is running.
    pub fn finish_run(&mut self) -> Result<bool, TaskLifecycleTransitionError> {
        match *self {
            Self::Running => {
                *self = Self::Idle;
                Ok(false)
            }
            Self::RunningRecoveredReplan => {
                *self = Self::Idle;
                Ok(false)
            }
            Self::RunningReplanPending => {
                *self = Self::ReplanPending;
                Ok(true)
            }
            from => Err(TaskLifecycleTransitionError {
                from,
                action: TaskLifecycleAction::FinishRun,
            }),
        }
    }

    /// Moves the task into its terminal cancellation transition.
    ///
    /// Returns `true` only for the first cancellation request.
    pub fn request_cancel(&mut self) -> bool {
        if self.is_cancelling() || self.is_cancelled() {
            return false;
        }
        *self = Self::Cancelling {
            was_running: self.is_running(),
        };
        true
    }

    /// Completes cancellation after all owned work has quiesced.
    ///
    /// # Errors
    ///
    /// Returns [`TaskLifecycleTransitionError`] unless cancellation is in
    /// progress.
    pub fn complete_cancel(&mut self) -> Result<(), TaskLifecycleTransitionError> {
        match *self {
            Self::Cancelling { .. } => {
                *self = Self::Cancelled;
                Ok(())
            }
            from => Err(TaskLifecycleTransitionError {
                from,
                action: TaskLifecycleAction::CompleteCancel,
            }),
        }
    }

    const fn legacy_flags(self) -> TaskLifecycleWire {
        TaskLifecycleWire {
            running: self.is_running(),
            cancel_requested: self.is_cancelling() || self.is_cancelled(),
            // Preserve a queued-work projection for readers that predate the
            // additive recovery marker. Such readers recover this state as
            // RunningReplanPending instead of dropping it as ordinary Running.
            pending_replan: self.has_pending_replan()
                || matches!(self, Self::RunningRecoveredReplan),
            cancelled: self.is_cancelled(),
            recovered_replan: matches!(self, Self::RunningRecoveredReplan),
        }
    }

    fn from_legacy_flags(flags: TaskLifecycleWire) -> Result<Self, &'static str> {
        if flags.recovered_replan {
            if flags.running && !flags.cancel_requested && flags.pending_replan && !flags.cancelled
            {
                return Ok(Self::RunningRecoveredReplan);
            }
            return Err(
                "recovered_replan requires running=true, cancel_requested=false, \
                 pending_replan=true, and cancelled=false",
            );
        }
        if flags.cancelled {
            return Ok(Self::Cancelled);
        }
        Ok(
            match (flags.running, flags.cancel_requested, flags.pending_replan) {
                (running, true, _) => Self::Cancelling {
                    was_running: running,
                },
                (true, false, true) => Self::RunningReplanPending,
                (true, false, false) => Self::Running,
                (false, false, true) => Self::ReplanPending,
                (false, false, false) => Self::Idle,
            },
        )
    }
}

impl Serialize for TaskLifecycle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let flags = self.legacy_flags();
        let extra_fields = usize::from(flags.cancelled) + usize::from(flags.recovered_replan);
        let mut map = serializer.serialize_map(Some(3 + extra_fields))?;
        map.serialize_entry("running", &flags.running)?;
        map.serialize_entry("cancel_requested", &flags.cancel_requested)?;
        map.serialize_entry("pending_replan", &flags.pending_replan)?;
        if flags.cancelled {
            map.serialize_entry("cancelled", &true)?;
        }
        if flags.recovered_replan {
            map.serialize_entry("recovered_replan", &true)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TaskLifecycle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let flags = TaskLifecycleWire::deserialize(deserializer)?;
        Self::from_legacy_flags(flags).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(default)]
struct TaskLifecycleWire {
    running: bool,
    cancel_requested: bool,
    pending_replan: bool,
    cancelled: bool,
    recovered_replan: bool,
}

/// Task lifecycle operation rejected by the current runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot {action} while task lifecycle is {from:?}")]
pub struct TaskLifecycleTransitionError {
    /// State in which the operation was attempted.
    pub from: TaskLifecycle,
    /// Operation that was rejected.
    pub action: TaskLifecycleAction,
}

/// Operations that can change a [`TaskLifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLifecycleAction {
    /// Start one sequence generation.
    Start,
    /// Claim a durable replan generation.
    StartDurableReplan,
    /// Queue a replan.
    RequestReplan,
    /// Complete the active sequence generation.
    FinishRun,
    /// Complete a cancellation after owned work quiesces.
    CompleteCancel,
}

impl std::fmt::Display for TaskLifecycleAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Start => "start task work",
            Self::StartDurableReplan => "start durable task replan",
            Self::RequestReplan => "request a task replan",
            Self::FinishRun => "finish task work",
            Self::CompleteCancel => "complete task cancellation",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskAwaitHandoffRequest {
    pub task_id: String,
    pub timeout_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub after_context_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskAwaitHandoffResponse {
    pub task_status: BrokerTaskStatusResponse,
    pub waited_ms: u64,
    pub polls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskMarkHandoffConsumedRequest {
    pub task_id: String,
    pub handoff_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskMarkHandoffConsumedResponse {
    pub handoff: Option<BrokerHandoffDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskLaunchAgentRequest {
    pub task_id: String,
    pub task: Option<String>,
    pub wait_for_handoff: bool,
    pub handoff_timeout_ms: Option<u64>,
    pub handoff_poll_ms: Option<u64>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskLaunchAgentResponse {
    pub task_id: String,
    pub pid: u32,
    pub bootstrap_mode: String,
    pub bootstrap_path: String,
    pub log_path: String,
    pub handoff_id: Option<String>,
    pub handoff_artifact_id: Option<String>,
    pub handoff_checkpoint_id: Option<String>,
    pub started_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskRecord {
    pub task_id: String,
    #[serde(flatten)]
    pub lifecycle: TaskLifecycle,
    pub last_request_id: Option<u64>,
    pub last_started_at_unix: Option<u64>,
    pub last_completed_at_unix: Option<u64>,
    pub last_replan_at_unix: Option<u64>,
    pub last_error: Option<String>,
    pub watch_ids: Vec<String>,
    pub sequence_present: bool,
    pub sequence: Option<KernelSequenceRequest>,
    pub last_sequence_metadata: Option<Value>,
    pub last_event_seq: u64,
    pub last_context_refresh_at_unix: Option<u64>,
    pub working_set_est_tokens: u64,
    pub evictable_est_tokens: u64,
    pub changed_since_checkpoint_paths: usize,
    pub changed_since_checkpoint_symbols: usize,
    pub latest_context_version: Option<String>,
    pub latest_brief_path: Option<String>,
    pub latest_brief_hash: Option<String>,
    pub latest_brief_generated_at_unix: Option<u64>,
    pub latest_context_reason: Option<String>,
    pub latest_handoff_id: Option<String>,
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_handoff_generated_at_unix: Option<u64>,
    pub latest_handoff_checkpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoffs: Vec<BrokerHandoffDescriptor>,
    pub latest_agent_pid: Option<u32>,
    pub latest_agent_bootstrap_mode: Option<String>,
    pub latest_agent_log_path: Option<String>,
    pub latest_agent_started_at_unix: Option<u64>,
    pub latest_agent_completed_at_unix: Option<u64>,
    pub latest_agent_exit_code: Option<i32>,
    pub latest_agent_context_version: Option<String>,
    pub latest_agent_handoff_artifact_id: Option<String>,
    pub latest_agent_handoff_checkpoint_id: Option<String>,
    pub latest_hook_session_id: Option<String>,
    pub latest_hook_event_at_unix: Option<u64>,
    pub latest_hook_boundary_at_unix: Option<u64>,
    pub latest_hook_boundary_kind: Option<String>,
    /// Session that last received a nonempty bootstrap brief, independent of hook activity.
    pub latest_hook_bootstrap_session_id: Option<String>,
    /// Namespace that owns the delivered brief.
    pub latest_hook_bootstrap_owner_task_id: Option<String>,
    pub latest_hook_bootstrap_artifact_id: Option<String>,
    pub latest_hook_bootstrap_context_version: Option<String>,
    pub latest_hook_bootstrap_at_unix: Option<u64>,
    pub hook_window_est_tokens: u64,
    pub hook_window_est_bytes: u64,
    pub hook_soft_threshold_tokens: u64,
    pub hook_threshold_exceeded: bool,
    pub latest_hook_handoff_reason: Option<String>,
    pub latest_hook_command_id: Option<String>,
    pub latest_hook_command_kind: Option<String>,
    pub latest_hook_progress_at_unix: Option<u64>,
    pub hook_git_epoch: u64,
    pub hook_fs_epoch: u64,
    pub hook_rust_epoch: u64,
    pub hook_reducer_cache: BTreeMap<String, HookReducerCacheEntry>,
    pub latest_broker_request: Option<BrokerGetContextRequest>,
    pub linked_decisions: BTreeMap<String, String>,
    pub resolved_questions: BTreeMap<String, String>,
    pub question_texts: BTreeMap<String, String>,
    /// Present on a task whose event history failed integrity validation.
    ///
    /// The record is terminal and fenced: it keeps its identity and evidence,
    /// but no further events may be appended under it. Continuation belongs to
    /// [`TaskHistoryRecovery::successor_task_id`]. Absent fields keep the
    /// legacy encoding unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<TaskHistoryRecovery>,
    /// Present on the new task created to continue a superseded task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_from: Option<TaskHistoryRecovery>,
    /// Present on a compact tombstone whose complete original record was moved
    /// to an immutable, digest-authenticated archive.
    ///
    /// The record keeps its identity, lifecycle, event high-water, watch
    /// relationships, and history links. It is terminal and fenced: no
    /// mutation, event append, or continuation may target it. Absent fields
    /// keep the legacy encoding unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<TaskRecordArchive>,
}

/// Current schema of [`TaskRecordArchive`] pointers.
pub const TASK_RECORD_ARCHIVE_SCHEMA_VERSION: u32 = 1;

/// Durable pointer from a compact tombstone to its archived original record.
///
/// The pointer lives in the authenticated task registry, so the digest and
/// byte length authenticate the archive file. `archive_file` is display
/// metadata relative to the workspace state directory, not path authority.
///
/// # Examples
///
/// ```
/// use packet28_daemon_protocol::task::{TaskRecord, TaskRecordArchive};
///
/// let tombstone = TaskRecord {
///     task_id: "task-big".to_string(),
///     archived: Some(TaskRecordArchive {
///         digest: "blake3:00".to_string(),
///         original_encoded_bytes: 2_000_000,
///         ..TaskRecordArchive::default()
///     }),
///     ..TaskRecord::default()
/// };
/// assert!(tombstone.archived.is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskRecordArchive {
    /// Pointer schema version.
    pub schema_version: u32,
    /// `blake3:<hex>` digest of the exact archived compact-JSON record bytes.
    pub digest: String,
    /// Exact byte length of the archived record.
    pub original_encoded_bytes: u64,
    /// Compact-JSON size of the tombstone written in place of the original.
    pub tombstone_encoded_bytes: u64,
    /// Archive commit time.
    pub archived_at_unix: u64,
    /// Why the record was archived.
    pub reason: String,
    /// Archive file relative to the workspace `.packet28` directory.
    pub archive_file: String,
    /// Top-level fields omitted from the tombstone, with their original
    /// compact-JSON value sizes. Every omitted value remains in the archive.
    pub omitted_fields: BTreeMap<String, u64>,
    /// Command that retrieves and verifies the complete original record.
    pub inspect_command: String,
}

/// Durable provenance linking a damaged task identity to its continuation.
///
/// The same link is stored on both records, so either one still names the
/// other and the preserved evidence after the counterpart is removed by
/// ordinary retention.
///
/// # Examples
///
/// ```
/// use packet28_daemon_protocol::task::{TaskHistoryRecovery, TaskRecord};
///
/// let link = TaskHistoryRecovery {
///     predecessor_task_id: "task-a".to_string(),
///     successor_task_id: "task-a-recovered-1".to_string(),
///     ..TaskHistoryRecovery::default()
/// };
/// let damaged = TaskRecord {
///     task_id: "task-a".to_string(),
///     superseded_by: Some(link),
///     ..TaskRecord::default()
/// };
/// assert_eq!(
///     damaged.superseded_by.as_ref().map(|link| link.successor_task_id.as_str()),
///     Some("task-a-recovered-1")
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskHistoryRecovery {
    /// Task whose event history was quarantined.
    pub predecessor_task_id: String,
    /// Task that continues the work under a fresh event sequence.
    pub successor_task_id: String,
    /// Integrity failure that triggered recovery.
    pub reason: String,
    /// Registry high-water of the predecessor before recovery.
    pub prior_last_event_seq: u64,
    /// Recovery time.
    pub recovered_at_unix: u64,
    /// Exact file name in the task-event directory reserved for the
    /// quarantined log before it is moved there. This is display metadata,
    /// not path authority.
    pub quarantined_event_log: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchRegistration {
    pub watch_id: String,
    pub spec: WatchSpec,
    pub active: bool,
    pub last_event_at_unix: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchRegistry {
    pub watches: Vec<WatchRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskRegistry {
    pub tasks: BTreeMap<String, TaskRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_history_links_are_omitted_from_legacy_records() {
        let legacy = serde_json::to_value(TaskRecord {
            task_id: "task".to_string(),
            ..TaskRecord::default()
        })
        .unwrap();
        assert!(legacy.get("superseded_by").is_none());
        assert!(legacy.get("recovered_from").is_none());

        let decoded: TaskRecord = serde_json::from_value(legacy).unwrap();
        assert!(decoded.superseded_by.is_none());
        assert!(decoded.recovered_from.is_none());
    }

    #[test]
    fn task_history_links_round_trip_with_exact_wire_names() {
        let link = TaskHistoryRecovery {
            predecessor_task_id: "task".to_string(),
            successor_task_id: "task-recovered-1".to_string(),
            reason: "invalid task event frame".to_string(),
            prior_last_event_seq: 100,
            recovered_at_unix: 42,
            quarantined_event_log: Some("task.events.jsonl.corrupt-42".to_string()),
        };
        let record = TaskRecord {
            task_id: "task".to_string(),
            superseded_by: Some(link.clone()),
            ..TaskRecord::default()
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            value["superseded_by"],
            serde_json::json!({
                "predecessor_task_id": "task",
                "successor_task_id": "task-recovered-1",
                "reason": "invalid task event frame",
                "prior_last_event_seq": 100,
                "recovered_at_unix": 42,
                "quarantined_event_log": "task.events.jsonl.corrupt-42",
            })
        );
        let decoded: TaskRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.superseded_by, Some(link));
        assert!(decoded.recovered_from.is_none());
    }

    #[test]
    fn start_accepts_only_idle_or_queued_work() {
        for (initial, expected) in [
            (TaskLifecycle::Idle, Ok(TaskLifecycle::Running)),
            (TaskLifecycle::ReplanPending, Ok(TaskLifecycle::Running)),
            (TaskLifecycle::Running, Err(TaskLifecycle::Running)),
            (
                TaskLifecycle::RunningRecoveredReplan,
                Err(TaskLifecycle::RunningRecoveredReplan),
            ),
            (
                TaskLifecycle::RunningReplanPending,
                Err(TaskLifecycle::RunningReplanPending),
            ),
            (
                TaskLifecycle::Cancelling { was_running: false },
                Err(TaskLifecycle::Cancelling { was_running: false }),
            ),
            (TaskLifecycle::Cancelled, Err(TaskLifecycle::Cancelled)),
        ] {
            let mut lifecycle = initial;
            let result = lifecycle.start();
            match expected {
                Ok(expected) => {
                    assert!(result.is_ok(), "start failed from {initial:?}: {result:?}");
                    assert_eq!(lifecycle, expected);
                }
                Err(expected_from) => {
                    assert_eq!(
                        result.unwrap_err(),
                        TaskLifecycleTransitionError {
                            from: expected_from,
                            action: TaskLifecycleAction::Start,
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn request_replan_queues_exactly_one_sequence_owner() {
        for (initial, expected, should_start) in [
            (TaskLifecycle::Idle, TaskLifecycle::ReplanPending, true),
            (
                TaskLifecycle::ReplanPending,
                TaskLifecycle::ReplanPending,
                false,
            ),
            (
                TaskLifecycle::Running,
                TaskLifecycle::RunningReplanPending,
                false,
            ),
            (
                TaskLifecycle::RunningRecoveredReplan,
                TaskLifecycle::RunningReplanPending,
                false,
            ),
            (
                TaskLifecycle::RunningReplanPending,
                TaskLifecycle::RunningReplanPending,
                false,
            ),
        ] {
            let mut lifecycle = initial;
            assert_eq!(lifecycle.request_replan().unwrap(), should_start);
            assert_eq!(lifecycle, expected);
        }
    }

    #[test]
    fn recovered_replan_claim_roundtrips_and_rejects_duplicate_ownership() {
        let mut lifecycle = TaskLifecycle::ReplanPending;
        lifecycle.start_durable_replan().unwrap();
        assert_eq!(lifecycle, TaskLifecycle::RunningRecoveredReplan);
        assert!(lifecycle.is_running());

        let record = TaskRecord {
            task_id: "recovered".to_string(),
            lifecycle,
            ..TaskRecord::default()
        };
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["recovered_replan"], serde_json::json!(true));
        assert_eq!(encoded["running"], serde_json::json!(true));
        assert_eq!(encoded["pending_replan"], serde_json::json!(true));
        assert_eq!(
            serde_json::from_value::<TaskRecord>(encoded.clone())
                .unwrap()
                .lifecycle,
            TaskLifecycle::RunningRecoveredReplan
        );
        let mut legacy_projection = encoded;
        legacy_projection
            .as_object_mut()
            .unwrap()
            .remove("recovered_replan");
        assert_eq!(
            serde_json::from_value::<TaskRecord>(legacy_projection)
                .unwrap()
                .lifecycle,
            TaskLifecycle::RunningReplanPending
        );
        assert_eq!(
            lifecycle.start_durable_replan().unwrap_err(),
            TaskLifecycleTransitionError {
                from: TaskLifecycle::RunningRecoveredReplan,
                action: TaskLifecycleAction::StartDurableReplan,
            }
        );

        assert!(!lifecycle.finish_run().unwrap());
        assert_eq!(lifecycle, TaskLifecycle::Idle);
        let completed = TaskRecord {
            lifecycle,
            ..record.clone()
        };
        let completed = serde_json::to_value(completed).unwrap();
        assert!(completed.get("recovered_replan").is_none());
        assert_eq!(
            serde_json::from_value::<TaskRecord>(completed)
                .unwrap()
                .lifecycle,
            TaskLifecycle::Idle
        );

        let mut cancelled_lifecycle = TaskLifecycle::RunningRecoveredReplan;
        assert!(cancelled_lifecycle.request_cancel());
        let cancelling = serde_json::to_value(TaskRecord {
            lifecycle: cancelled_lifecycle,
            ..record
        })
        .unwrap();
        assert!(cancelling.get("recovered_replan").is_none());
        assert_eq!(
            serde_json::from_value::<TaskRecord>(cancelling)
                .unwrap()
                .lifecycle,
            TaskLifecycle::Cancelling { was_running: true }
        );
    }

    #[test]
    fn malformed_recovered_replan_marker_is_rejected() {
        for malformed in [
            serde_json::json!({
                "task_id": "recovered",
                "running": false,
                "cancel_requested": false,
                "pending_replan": true,
                "recovered_replan": true
            }),
            serde_json::json!({
                "task_id": "recovered",
                "running": true,
                "cancel_requested": true,
                "pending_replan": true,
                "recovered_replan": true
            }),
            serde_json::json!({
                "task_id": "recovered",
                "running": true,
                "cancel_requested": false,
                "pending_replan": false,
                "recovered_replan": true
            }),
            serde_json::json!({
                "task_id": "recovered",
                "running": true,
                "cancel_requested": false,
                "pending_replan": true,
                "cancelled": true,
                "recovered_replan": true
            }),
        ] {
            let error = serde_json::from_value::<TaskRecord>(malformed).unwrap_err();
            assert!(error.to_string().contains("recovered_replan requires"));
        }
    }

    #[test]
    fn request_replan_rejects_cancelling_and_cancelled_tasks() {
        for initial in [
            TaskLifecycle::Cancelling { was_running: true },
            TaskLifecycle::Cancelled,
        ] {
            let mut lifecycle = initial;
            assert_eq!(
                lifecycle.request_replan().unwrap_err(),
                TaskLifecycleTransitionError {
                    from: initial,
                    action: TaskLifecycleAction::RequestReplan,
                }
            );
        }
    }

    #[test]
    fn finish_run_preserves_only_a_pending_rerun() {
        for (initial, expected, rerun) in [
            (TaskLifecycle::Running, TaskLifecycle::Idle, false),
            (
                TaskLifecycle::RunningRecoveredReplan,
                TaskLifecycle::Idle,
                false,
            ),
            (
                TaskLifecycle::RunningReplanPending,
                TaskLifecycle::ReplanPending,
                true,
            ),
        ] {
            let mut lifecycle = initial;
            assert_eq!(lifecycle.finish_run().unwrap(), rerun);
            assert_eq!(lifecycle, expected);
        }
    }

    #[test]
    fn finish_run_rejects_states_without_active_work() {
        for initial in [
            TaskLifecycle::Idle,
            TaskLifecycle::ReplanPending,
            TaskLifecycle::Cancelling { was_running: false },
            TaskLifecycle::Cancelling { was_running: true },
            TaskLifecycle::Cancelled,
        ] {
            let mut lifecycle = initial;
            assert_eq!(
                lifecycle.finish_run().unwrap_err(),
                TaskLifecycleTransitionError {
                    from: initial,
                    action: TaskLifecycleAction::FinishRun,
                }
            );
        }
    }

    #[test]
    fn request_cancel_is_idempotent_and_removes_pending_replan() {
        let mut lifecycle = TaskLifecycle::RunningReplanPending;

        assert!(lifecycle.request_cancel());
        assert_eq!(lifecycle, TaskLifecycle::Cancelling { was_running: true });
        assert!(!lifecycle.request_cancel());
    }

    #[test]
    fn complete_cancel_accepts_only_the_quiescing_transition() {
        for initial in [
            TaskLifecycle::Cancelling { was_running: false },
            TaskLifecycle::Cancelling { was_running: true },
        ] {
            let mut lifecycle = initial;
            lifecycle.complete_cancel().unwrap();
            assert_eq!(lifecycle, TaskLifecycle::Cancelled);
        }

        for initial in [
            TaskLifecycle::Idle,
            TaskLifecycle::ReplanPending,
            TaskLifecycle::Running,
            TaskLifecycle::RunningRecoveredReplan,
            TaskLifecycle::RunningReplanPending,
            TaskLifecycle::Cancelled,
        ] {
            let mut lifecycle = initial;
            assert_eq!(
                lifecycle.complete_cancel().unwrap_err(),
                TaskLifecycleTransitionError {
                    from: initial,
                    action: TaskLifecycleAction::CompleteCancel,
                }
            );
        }
    }

    #[test]
    fn terminal_cancel_is_idempotent() {
        let mut lifecycle = TaskLifecycle::Cancelled;

        assert!(!lifecycle.request_cancel());
        assert!(lifecycle.is_cancelled());
        assert!(!lifecycle.is_cancelling());
    }

    #[test]
    fn task_record_preserves_legacy_lifecycle_wire_fields() {
        let legacy = serde_json::json!({
            "task_id": "task-1",
            "running": true,
            "cancel_requested": false,
            "pending_replan": true
        });
        let record: TaskRecord = serde_json::from_value(legacy).unwrap();

        assert_eq!(record.lifecycle, TaskLifecycle::RunningReplanPending);
        let encoded = serde_json::to_value(record).unwrap();
        assert_eq!(encoded["running"], serde_json::json!(true));
        assert_eq!(encoded["cancel_requested"], serde_json::json!(false));
        assert_eq!(encoded["pending_replan"], serde_json::json!(true));
        assert!(encoded.get("cancelled").is_none());
        assert!(encoded.get("lifecycle").is_none());
    }

    #[test]
    fn every_typed_lifecycle_roundtrips_through_legacy_flags() {
        for lifecycle in [
            TaskLifecycle::Idle,
            TaskLifecycle::ReplanPending,
            TaskLifecycle::Running,
            TaskLifecycle::RunningRecoveredReplan,
            TaskLifecycle::RunningReplanPending,
            TaskLifecycle::Cancelling { was_running: false },
            TaskLifecycle::Cancelling { was_running: true },
            TaskLifecycle::Cancelled,
        ] {
            let record = TaskRecord {
                task_id: "task-1".to_string(),
                lifecycle,
                ..TaskRecord::default()
            };
            let encoded = serde_json::to_vec(&record).unwrap();
            let decoded: TaskRecord = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.lifecycle, lifecycle);
        }
    }

    #[test]
    fn contradictory_legacy_cancel_flags_are_canonicalized() {
        let record: TaskRecord = serde_json::from_value(serde_json::json!({
            "task_id": "task-1",
            "running": false,
            "cancel_requested": true,
            "pending_replan": true
        }))
        .unwrap();

        assert_eq!(
            record.lifecycle,
            TaskLifecycle::Cancelling { was_running: false }
        );
        assert!(!record.lifecycle.has_pending_replan());
    }

    #[test]
    fn additive_cancelled_flag_wins_over_legacy_transition_flags() {
        let record: TaskRecord = serde_json::from_value(serde_json::json!({
            "task_id": "task-1",
            "running": true,
            "cancel_requested": false,
            "pending_replan": true,
            "cancelled": true
        }))
        .unwrap();

        assert_eq!(record.lifecycle, TaskLifecycle::Cancelled);
        let encoded = serde_json::to_value(record).unwrap();
        assert_eq!(encoded["running"], serde_json::json!(false));
        assert_eq!(encoded["cancel_requested"], serde_json::json!(true));
        assert_eq!(encoded["pending_replan"], serde_json::json!(false));
        assert_eq!(encoded["cancelled"], serde_json::json!(true));
    }
}
