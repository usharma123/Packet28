use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use packet28_daemon_core::storage::{load_task_events_from_offset, TaskEventLogRead};
use serde_json::{json, Map, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::transport::McpMessageFraming;
use super::McpSessionState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NotificationDelivery {
    Delivered,
    Backpressured,
}

/// Owns notification cancellation and task completion.
pub(super) struct NotificationTask {
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

impl NotificationTask {
    pub(super) fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub(super) async fn supervise<F, T>(&mut self, operation: F) -> Result<T>
    where
        F: Future<Output = T>,
    {
        tokio::select! {
            output = operation => Ok(output),
            result = self.join() => Err(unexpected_notification_exit(result)),
        }
    }

    pub(super) async fn supervise_result<F, T>(&mut self, operation: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        self.supervise(operation).await?
    }

    pub(super) async fn shutdown(mut self, grace: Duration) -> Result<()> {
        if self.task.is_none() {
            return Ok(());
        }
        self.request_shutdown();
        match tokio::time::timeout(grace, self.join()).await {
            Ok(result) => result,
            Err(_) => {
                self.abort();
                let _ = tokio::time::timeout(grace, self.join()).await;
                Err(anyhow!(
                    "MCP notification task did not stop within {}ms; task aborted",
                    grace.as_millis()
                ))
            }
        }
    }

    pub(super) async fn join(&mut self) -> Result<()> {
        let joined = self
            .task
            .as_mut()
            .ok_or_else(|| anyhow!("MCP notification task was already joined"))?;
        let result = joined.await;
        self.task.take();
        result.map_err(|error| anyhow!("MCP notification task failed: {error}"))?
    }

    fn abort(&mut self) {
        if let Some(task) = self.task.as_mut() {
            task.abort();
        }
    }
}

impl Drop for NotificationTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn unexpected_notification_exit(result: Result<()>) -> anyhow::Error {
    match result {
        Ok(()) => anyhow!("MCP notification task stopped unexpectedly"),
        Err(error) => error,
    }
}

pub(super) fn start_notification_task<Deliver, DeliveryFuture>(
    root: PathBuf,
    session: Arc<Mutex<McpSessionState>>,
    poll_interval: Duration,
    deliver: Deliver,
) -> NotificationTask
where
    Deliver: FnMut(Value, McpMessageFraming) -> DeliveryFuture + Send + 'static,
    DeliveryFuture: Future<Output = Result<NotificationDelivery>> + Send + 'static,
{
    start_notification_task_with_reader(
        root,
        session,
        poll_interval,
        |root, task_id, offset| async move {
            let read_task_id = task_id.clone();
            let pending = tokio::task::spawn_blocking(move || {
                read_notification_events_with_recovery(&root, &read_task_id, offset, || {
                    crate::broker_client::ensure_daemon(&root)
                })
            });
            match tokio::time::timeout(Duration::from_secs(12), pending).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(anyhow!("MCP notification reader task failed: {error}")),
                Err(_) => Err(anyhow!(
                    "MCP notification recovery readiness timed out for task {task_id:?}"
                )),
            }
        },
        deliver,
    )
}

fn read_notification_events_with_recovery(
    root: &Path,
    task_id: &str,
    offset: u64,
    ensure_ready: impl FnOnce() -> Result<()>,
) -> Result<Option<TaskEventLogRead>> {
    match load_task_events_from_offset(root, task_id, offset) {
        Ok(read) => Ok(Some(read)),
        Err(error) => {
            // Recovery publishes its checkpoint before readiness. A failed
            // strict read during startup must wait for that authority before
            // deciding whether this identity has a verified successor.
            if ensure_ready().is_ok()
                && crate::task_runtime::resolve_task_continuation(root, task_id)? != task_id
            {
                return Ok(None);
            }
            Err(anyhow!("MCP notification event-log read failed for task {task_id:?} at offset {offset}: {error}"))
        }
    }
}

fn start_notification_task_with_reader<Read, ReadFuture, Deliver, DeliveryFuture>(
    root: PathBuf,
    session: Arc<Mutex<McpSessionState>>,
    poll_interval: Duration,
    read: Read,
    deliver: Deliver,
) -> NotificationTask
where
    Read: FnMut(PathBuf, String, u64) -> ReadFuture + Send + 'static,
    ReadFuture: Future<Output = Result<Option<TaskEventLogRead>>> + Send + 'static,
    Deliver: FnMut(Value, McpMessageFraming) -> DeliveryFuture + Send + 'static,
    DeliveryFuture: Future<Output = Result<NotificationDelivery>> + Send + 'static,
{
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let task = tokio::spawn(run_notification_loop(
        root,
        session,
        poll_interval,
        shutdown_receiver,
        read,
        deliver,
    ));
    NotificationTask {
        shutdown,
        task: Some(task),
    }
}

async fn run_notification_loop<Read, ReadFuture, Deliver, DeliveryFuture>(
    root: PathBuf,
    session: Arc<Mutex<McpSessionState>>,
    poll_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut read: Read,
    mut deliver: Deliver,
) -> Result<()>
where
    Read: FnMut(PathBuf, String, u64) -> ReadFuture,
    ReadFuture: Future<Output = Result<Option<TaskEventLogRead>>>,
    Deliver: FnMut(Value, McpMessageFraming) -> DeliveryFuture,
    DeliveryFuture: Future<Output = Result<NotificationDelivery>>,
{
    let mut interval = tokio::time::interval(poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    result = run_notification_pass(&root, &session, &mut read, &mut deliver) => {
                        result?;
                    }
                }
            }
        }
    }
}

async fn resolve_notification_task(root: &Path, task_id: &str) -> Result<String> {
    let root = root.to_path_buf();
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::task_runtime::resolve_task_continuation(&root, &task_id)
    })
    .await
    .map_err(|error| anyhow!("MCP recovery resolution task failed: {error}"))?
}

async fn run_notification_pass<Read, ReadFuture, Deliver, DeliveryFuture>(
    root: &Path,
    session: &Arc<Mutex<McpSessionState>>,
    read: &mut Read,
    deliver: &mut Deliver,
) -> Result<()>
where
    Read: FnMut(PathBuf, String, u64) -> ReadFuture,
    ReadFuture: Future<Output = Result<Option<TaskEventLogRead>>>,
    Deliver: FnMut(Value, McpMessageFraming) -> DeliveryFuture,
    DeliveryFuture: Future<Output = Result<NotificationDelivery>>,
{
    let (initialized, tracked_tasks, framing) = match session.lock() {
        Ok(guard) => (
            guard.initialized,
            guard.tracked_tasks.clone(),
            guard.framing,
        ),
        Err(_) => return Err(anyhow!("MCP notification session lock is poisoned")),
    };
    let Some(framing) = framing.filter(|_| initialized) else {
        return Ok(());
    };

    for (original_task_id, original_last_seen_seq) in tracked_tasks {
        let task_id = resolve_notification_task(root, &original_task_id).await?;
        let (last_seen_seq, previous_offset) = if task_id != original_task_id {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "notifications/packet28.task_recovered",
                "params": {
                    "predecessor_task_id": original_task_id,
                    "successor_task_id": task_id,
                    "prior_event_seq": original_last_seen_seq,
                },
            });
            match deliver(notification, framing).await? {
                NotificationDelivery::Backpressured => continue,
                NotificationDelivery::Delivered => {}
            }
            let mut guard = session
                .lock()
                .map_err(|_| anyhow!("MCP notification session lock is poisoned"))?;
            guard.tracked_tasks.remove(&original_task_id);
            guard.tracked_task_offsets.remove(&original_task_id);
            // Independent identity, independent cursor. If already tracked,
            // preserve its delivered cursor rather than replaying duplicates.
            let seq = *guard.tracked_tasks.entry(task_id.clone()).or_insert(0);
            let offset = *guard
                .tracked_task_offsets
                .entry(task_id.clone())
                .or_insert(0);
            if guard.current_task_id.as_deref() == Some(original_task_id.as_str()) {
                guard.current_task_id = Some(task_id.clone());
            }
            if guard.proxy_task_id.as_deref() == Some(original_task_id.as_str()) {
                guard.proxy_task_id = Some(task_id.clone());
            }
            (seq, offset)
        } else {
            // Another predecessor may already have transferred tracking to
            // this task during this pass; always use the current cursor.
            let guard = session
                .lock()
                .map_err(|_| anyhow!("MCP notification session lock is poisoned"))?;
            let Some(seq) = guard.tracked_tasks.get(&task_id).copied() else {
                continue;
            };
            (
                seq,
                guard
                    .tracked_task_offsets
                    .get(&task_id)
                    .copied()
                    .unwrap_or(0),
            )
        };
        let read = match read(root.to_path_buf(), task_id.clone(), previous_offset).await {
            Ok(Some(read)) => read,
            Ok(None) => continue,
            Err(error) => {
                // Startup may have checkpointed a verified recovery while
                // this read was in flight. Retry only that durable transition;
                // all other read/integrity failures retain their fatal behavior.
                if resolve_notification_task(root, &task_id).await? != task_id {
                    continue;
                }
                return Err(error);
            }
        };
        let mut newest_delivered_seq = last_seen_seq;
        let mut backpressured = false;
        for frame in read
            .events
            .into_iter()
            .filter(|frame| frame.seq > last_seen_seq)
        {
            if frame.event.kind != "context_updated" {
                newest_delivered_seq = newest_delivered_seq.max(frame.seq);
                continue;
            }
            let mut params = match frame.event.data {
                Value::Object(map) => map,
                other => {
                    let mut map = Map::new();
                    map.insert("data".to_string(), other);
                    map
                }
            };
            params.insert("task_id".to_string(), Value::String(task_id.clone()));
            params.insert(
                "context_version".to_string(),
                params
                    .get("context_version")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            params.insert("event_seq".to_string(), Value::Number(frame.seq.into()));
            let notification = json!({
                "jsonrpc":"2.0",
                "method":"notifications/packet28.context_updated",
                "params": Value::Object(params),
            });
            match deliver(notification, framing).await {
                Ok(NotificationDelivery::Delivered) => {}
                Ok(NotificationDelivery::Backpressured) => {
                    backpressured = true;
                    break;
                }
                Err(error) => {
                    return Err(anyhow!(
                        "MCP notification delivery failed for task {task_id:?}: {error}"
                    ));
                }
            }
            newest_delivered_seq = newest_delivered_seq.max(frame.seq);
        }
        if newest_delivered_seq > last_seen_seq
            || (!backpressured && read.next_offset != previous_offset)
        {
            let Ok(mut guard) = session.lock() else {
                return Err(anyhow!("MCP notification session lock is poisoned"));
            };
            if let Some(current) = guard.tracked_tasks.get_mut(&task_id) {
                *current = newest_delivered_seq;
            }
            if !backpressured {
                guard.tracked_task_offsets.insert(task_id, read.next_offset);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use packet28_daemon_core::storage::save_task_registry;
    use packet28_daemon_protocol::message::{DaemonEvent, DaemonEventFrame};
    use packet28_daemon_protocol::paths::{task_event_log_path, TaskStorageId};
    use packet28_daemon_protocol::task::{TaskRecord, TaskRegistry};
    use tempfile::TempDir;

    use super::*;
    use crate::cmd_mcp::proxy_upstream::proxy_output_channel;

    const TASK_ID: &str = "notification-task";

    fn fixture(
        context_events: u64,
        initialized: bool,
    ) -> (TempDir, Arc<Mutex<McpSessionState>>, u64) {
        let root = TempDir::new().unwrap();
        let mut registry = TaskRegistry::default();
        registry.tasks.insert(
            TASK_ID.to_string(),
            TaskRecord {
                task_id: TASK_ID.to_string(),
                ..TaskRecord::default()
            },
        );
        save_task_registry(root.path(), &registry).unwrap();

        let task_id = TaskStorageId::try_from(TASK_ID).unwrap();
        let event_path = task_event_log_path(root.path(), &task_id);
        std::fs::create_dir_all(event_path.parent().unwrap()).unwrap();
        let events = (1..=context_events)
            .map(|seq| {
                serde_json::to_string(&DaemonEventFrame {
                    seq,
                    task_id: TASK_ID.to_string(),
                    event: DaemonEvent {
                        kind: "context_updated".to_string(),
                        occurred_at_unix: seq,
                        data: json!({"context_version": format!("ctx-{seq}")}),
                    },
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let events = if events.is_empty() {
            String::new()
        } else {
            format!("{events}\n")
        };
        std::fs::write(&event_path, events).unwrap();
        let event_log_len = std::fs::metadata(event_path).unwrap().len();

        let session = Arc::new(Mutex::new(McpSessionState {
            initialized,
            tracked_tasks: BTreeMap::from([(TASK_ID.to_string(), 0)]),
            tracked_task_offsets: BTreeMap::from([(TASK_ID.to_string(), 0)]),
            framing: Some(McpMessageFraming::NewlineJson),
            ..McpSessionState::default()
        }));
        (root, session, event_log_len)
    }

    fn start_deterministic_notification_task<Deliver, DeliveryFuture>(
        root: PathBuf,
        session: Arc<Mutex<McpSessionState>>,
        deliver: Deliver,
    ) -> NotificationTask
    where
        Deliver: FnMut(Value, McpMessageFraming) -> DeliveryFuture + Send + 'static,
        DeliveryFuture: Future<Output = Result<NotificationDelivery>> + Send + 'static,
    {
        start_notification_task_with_reader(
            root,
            session,
            super::super::MCP_NOTIFICATION_POLL_INTERVAL,
            |root, task_id, offset| async move {
                load_task_events_from_offset(&root, &task_id, offset)
                    .map(Some)
                    .map_err(anyhow::Error::from)
            },
            deliver,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn idle_session_delivers_first_notification_within_one_interval_after_initialize() {
        let (root, session, _) = fixture(1, false);
        let (output, mut receiver) = proxy_output_channel();
        let notification_output = output.clone();
        let task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session.clone(),
            move |notification, framing| {
                let output = notification_output.clone();
                async move {
                    Ok(if output.try_send(notification, framing)? {
                        NotificationDelivery::Delivered
                    } else {
                        NotificationDelivery::Backpressured
                    })
                }
            },
        );
        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());

        session.lock().unwrap().initialized = true;
        tokio::time::advance(
            super::super::MCP_NOTIFICATION_POLL_INTERVAL - Duration::from_millis(1),
        )
        .await;
        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());

        tokio::time::advance(Duration::from_millis(1)).await;
        let message = receiver.recv().await.unwrap();
        assert_eq!(message.value["params"]["event_seq"], 1);
        task.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn empty_event_log_remains_a_transient_no_event_read() {
        let (root, session, _) = fixture(0, true);
        let mut task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session.clone(),
            |_notification, _framing| async {
                panic!("empty event log must not deliver a notification")
            },
        );

        tokio::task::yield_now().await;

        assert_eq!(session.lock().unwrap().tracked_tasks[TASK_ID], 0);
        assert_eq!(session.lock().unwrap().tracked_task_offsets[TASK_ID], 0);
        task.request_shutdown();
        task.join().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_join_remains_owned_when_the_wait_is_cancelled() {
        let (root, session, _) = fixture(0, true);
        let mut task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session,
            |_notification, _framing| async {
                panic!("empty event log must not deliver a notification")
            },
        );

        let mut supervised = Box::pin(task.supervise(std::future::pending::<()>()));
        tokio::select! {
            biased;
            result = &mut supervised => {
                panic!("notification supervision stopped before cancellation: {result:?}");
            }
            _ = tokio::task::yield_now() => {}
        }
        drop(supervised);

        task.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_propagates_notification_reader_errors() {
        let (root, session, _) = fixture(0, true);
        let mut task = start_notification_task_with_reader(
            root.path().to_path_buf(),
            session,
            super::super::MCP_NOTIFICATION_POLL_INTERVAL,
            |_root, _task_id, _offset| async {
                Err::<Option<TaskEventLogRead>, _>(anyhow!("reader exploded"))
            },
            |_notification, _framing| async {
                panic!("failed notification read must not attempt delivery")
            },
        );

        let error = task
            .supervise(std::future::pending::<()>())
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "reader exploded");
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_propagates_notification_delivery_errors() {
        let (root, session, _) = fixture(1, true);
        let mut task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session.clone(),
            |_notification, _framing| async {
                Err::<NotificationDelivery, _>(anyhow!("stdout exploded"))
            },
        );

        let error = task
            .supervise(std::future::pending::<()>())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("MCP notification delivery failed"));
        assert_eq!(session.lock().unwrap().tracked_tasks[TASK_ID], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_reports_notification_task_panics() {
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        let mut task = NotificationTask {
            shutdown,
            task: Some(tokio::spawn(async {
                panic!("notification panic fixture");
                #[expect(unreachable_code, reason = "panic drives the task failure")]
                Ok(())
            })),
        };

        let error = task
            .supervise(std::future::pending::<()>())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("MCP notification task failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_rejects_an_unexpected_clean_poller_exit() {
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        let mut task = NotificationTask {
            shutdown,
            task: Some(tokio::spawn(async { Ok(()) })),
        };

        let error = task
            .supervise(std::future::pending::<()>())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "MCP notification task stopped unexpectedly"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_shutdown_aborts_a_task_that_ignores_its_signal() {
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        let task = NotificationTask {
            shutdown,
            task: Some(tokio::spawn(std::future::pending::<Result<()>>())),
        };
        let mut shutdown = Box::pin(task.shutdown(Duration::from_secs(1)));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(1)).await;
        let error = shutdown.as_mut().await.unwrap_err();

        assert!(error.to_string().contains("task aborted"));
    }

    #[tokio::test(start_paused = true)]
    async fn durable_event_corruption_stops_notifications_without_advancing_the_cursor() {
        use std::io::Write as _;

        let (root, session, _) = fixture(1, true);
        let task_id = TaskStorageId::try_from(TASK_ID).unwrap();
        let event_path = task_event_log_path(root.path(), &task_id);
        let mut events = std::fs::OpenOptions::new()
            .append(true)
            .open(event_path)
            .unwrap();
        events.write_all(b"{not-json}\n").unwrap();
        events.sync_all().unwrap();
        let mut task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session.clone(),
            |_notification, _framing| async {
                panic!("corrupt event log must not deliver a notification")
            },
        );

        let error = task.join().await.unwrap_err();

        assert!(error.to_string().contains("invalid task event frame"));
        assert_eq!(session.lock().unwrap().tracked_tasks[TASK_ID], 0);
        assert_eq!(session.lock().unwrap().tracked_task_offsets[TASK_ID], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_output_replays_only_undelivered_notification_and_then_advances_offset() {
        let event_count = super::super::proxy_upstream::MAX_PROXY_OUTPUT_MESSAGES as u64 + 1;
        let (root, session, event_log_len) = fixture(event_count, true);
        let (output, mut receiver) = proxy_output_channel();
        let notification_output = output.clone();
        let mut read = |root: PathBuf, task_id: String, offset| async move {
            load_task_events_from_offset(&root, &task_id, offset)
                .map(Some)
                .map_err(anyhow::Error::from)
        };
        let mut deliver = move |notification, framing| {
            let output = notification_output.clone();
            async move {
                Ok(if output.try_send(notification, framing)? {
                    NotificationDelivery::Delivered
                } else {
                    NotificationDelivery::Backpressured
                })
            }
        };
        // Await each pass explicitly: registry resolution runs on a blocking
        // worker and cannot be synchronized by one executor yield.
        run_notification_pass(root.path(), &session, &mut read, &mut deliver)
            .await
            .unwrap();

        let first_pass_seq = session.lock().unwrap().tracked_tasks[TASK_ID];
        assert_eq!(
            first_pass_seq,
            super::super::proxy_upstream::MAX_PROXY_OUTPUT_MESSAGES as u64
        );
        assert_eq!(session.lock().unwrap().tracked_task_offsets[TASK_ID], 0);
        let first_pass = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|message| message.value["params"]["event_seq"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            first_pass,
            (1..=super::super::proxy_upstream::MAX_PROXY_OUTPUT_MESSAGES as u64)
                .collect::<Vec<_>>()
        );

        run_notification_pass(root.path(), &session, &mut read, &mut deliver)
            .await
            .unwrap();
        let replay = receiver.try_recv().unwrap();
        assert_eq!(replay.value["params"]["event_seq"], event_count);
        assert_eq!(session.lock().unwrap().tracked_tasks[TASK_ID], event_count);
        assert_eq!(
            session.lock().unwrap().tracked_task_offsets[TASK_ID],
            event_log_len
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_signal_joins_without_waiting_for_next_poll_interval() {
        let (root, session, _) = fixture(1, false);
        let started_at = tokio::time::Instant::now();
        let task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session,
            |_notification, _framing| async { Ok(NotificationDelivery::Delivered) },
        );
        tokio::task::yield_now().await;

        task.shutdown(Duration::from_secs(1)).await.unwrap();

        assert_eq!(tokio::time::Instant::now(), started_at);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_a_backpressured_delivery_and_joins_immediately() {
        let (root, session, _) = fixture(1, true);
        let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
        let mut entered_sender = Some(entered_sender);
        let started_at = tokio::time::Instant::now();
        let task = start_deterministic_notification_task(
            root.path().to_path_buf(),
            session,
            move |_notification, _framing| {
                let entered_sender = entered_sender.take();
                async move {
                    if let Some(entered_sender) = entered_sender {
                        let _ = entered_sender.send(());
                    }
                    std::future::pending().await
                }
            },
        );
        entered_receiver.await.unwrap();

        task.shutdown(Duration::from_secs(1)).await.unwrap();

        assert_eq!(tokio::time::Instant::now(), started_at);
    }

    #[test]
    fn strict_read_waits_for_recovery_authority_and_keeps_unchanged_identity_fatal() {
        use packet28_daemon_protocol::task::TaskHistoryRecovery;
        let (root, _, _) = fixture(0, true);
        let path = task_event_log_path(root.path(), &TaskStorageId::try_from(TASK_ID).unwrap());
        std::fs::write(path, b"{damaged history}\n").unwrap();
        let unchanged = read_notification_events_with_recovery(root.path(), TASK_ID, 0, || Ok(()));
        assert!(unchanged
            .unwrap_err()
            .to_string()
            .contains("event-log read failed"));
        let recovered = read_notification_events_with_recovery(root.path(), TASK_ID, 0, || {
            // The old checkpoint is still authoritative when readiness starts.
            assert_eq!(
                crate::task_runtime::resolve_task_continuation(root.path(), TASK_ID)?,
                TASK_ID
            );
            let link = TaskHistoryRecovery {
                predecessor_task_id: TASK_ID.to_string(),
                successor_task_id: "successor".to_string(),
                ..TaskHistoryRecovery::default()
            };
            let mut registry = TaskRegistry::default();
            registry.tasks.insert(
                TASK_ID.to_string(),
                TaskRecord {
                    task_id: TASK_ID.to_string(),
                    superseded_by: Some(link.clone()),
                    ..TaskRecord::default()
                },
            );
            registry.tasks.insert(
                "successor".to_string(),
                TaskRecord {
                    task_id: "successor".to_string(),
                    recovered_from: Some(link),
                    ..TaskRecord::default()
                },
            );
            save_task_registry(root.path(), &registry)?;
            Ok(())
        })
        .unwrap();
        assert!(recovered.is_none());
    }

    #[tokio::test]
    async fn idle_notification_session_follows_recovery_with_independent_cursor_and_backpressure() {
        use packet28_daemon_protocol::task::TaskHistoryRecovery;
        let (root, session, _) = fixture(0, true);
        session
            .lock()
            .unwrap()
            .tracked_tasks
            .insert(TASK_ID.to_string(), 100);
        session.lock().unwrap().current_task_id = Some(TASK_ID.to_string());
        let link = TaskHistoryRecovery {
            predecessor_task_id: TASK_ID.to_string(),
            successor_task_id: "successor".to_string(),
            ..TaskHistoryRecovery::default()
        };
        let mut registry = TaskRegistry::default();
        registry.tasks.insert(
            TASK_ID.to_string(),
            TaskRecord {
                task_id: TASK_ID.to_string(),
                superseded_by: Some(link.clone()),
                ..TaskRecord::default()
            },
        );
        registry.tasks.insert(
            "successor".to_string(),
            TaskRecord {
                task_id: "successor".to_string(),
                recovered_from: Some(link),
                ..TaskRecord::default()
            },
        );
        save_task_registry(root.path(), &registry).unwrap();
        let mut read = |_: PathBuf, task: String, offset: u64| async move {
            assert_eq!(task, "successor");
            assert_eq!(offset, 0);
            Ok(Some(TaskEventLogRead {
                events: vec![DaemonEventFrame {
                    seq: 1,
                    task_id: task,
                    event: DaemonEvent {
                        kind: "context_updated".to_string(),
                        occurred_at_unix: 1,
                        data: json!({"context_version":"ctx-1"}),
                    },
                }],
                next_offset: 42,
            }))
        };
        let mut blocked =
            |_: Value, _: McpMessageFraming| async { Ok(NotificationDelivery::Backpressured) };
        run_notification_pass(root.path(), &session, &mut read, &mut blocked)
            .await
            .unwrap();
        assert_eq!(session.lock().unwrap().tracked_tasks[TASK_ID], 100);
        let mut messages = Vec::new();
        let mut calls = 0;
        let mut deliver = |message: Value, _: McpMessageFraming| {
            calls += 1;
            messages.push(message);
            let result = if calls == 1 {
                NotificationDelivery::Delivered
            } else {
                NotificationDelivery::Backpressured
            };
            async move { Ok(result) }
        };
        run_notification_pass(root.path(), &session, &mut read, &mut deliver)
            .await
            .unwrap();
        assert_eq!(
            messages[0]["method"],
            "notifications/packet28.task_recovered"
        );
        assert_eq!(session.lock().unwrap().tracked_tasks["successor"], 0);
        assert_eq!(session.lock().unwrap().tracked_task_offsets["successor"], 0);
        let mut delivered = Vec::new();
        let mut deliver = |message: Value, _: McpMessageFraming| {
            delivered.push(message);
            async { Ok(NotificationDelivery::Delivered) }
        };
        run_notification_pass(root.path(), &session, &mut read, &mut deliver)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["params"]["event_seq"], 1);
        let guard = session.lock().unwrap();
        assert!(!guard.tracked_tasks.contains_key(TASK_ID));
        assert_eq!(guard.tracked_tasks["successor"], 1);
        assert_eq!(guard.tracked_task_offsets["successor"], 42);
        assert_eq!(guard.current_task_id.as_deref(), Some("successor"));
    }

    #[test]
    fn notification_sources_have_no_unowned_os_thread_polling() {
        let forbidden = [
            ["std", "::thread"].concat(),
            ["thread", "::spawn"].concat(),
            ["thread", "::sleep"].concat(),
        ];
        for (name, source) in [
            ("cmd_mcp.rs", include_str!("cmd_mcp.rs")),
            ("cmd_mcp_proxy.rs", include_str!("cmd_mcp_proxy.rs")),
            (
                "cmd_mcp_notifications.rs",
                include_str!("cmd_mcp_notifications.rs"),
            ),
        ] {
            for pattern in &forbidden {
                assert!(
                    !source.contains(pattern),
                    "{name} contains unowned polling primitive {pattern}"
                );
            }
        }
    }
}
