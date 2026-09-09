use super::*;
use crate::broker::{
    broker_decompose, broker_estimate_context, broker_get_context, broker_prepare_handoff,
    broker_task_status, broker_validate_plan, broker_write_state, broker_write_state_batch,
    build_registry_status_v1, build_status, kernel_for_context_root, kernel_for_request,
    mark_handoff_consumed,
};
use crate::instruction_files::resolve_context;
use crate::runtime::{BlockingPool, DaemonRuntimeConfig};
use crate::state::TaskSubscriber;
use crate::watch::WatchIngress;
use packet28_daemon_protocol::frame::{FrameError, MAX_SOCKET_MESSAGE_BYTES};
use packet28_daemon_protocol::message::DaemonTransportAuth;
use packet28_daemon_protocol::registry::{
    DaemonRegistryRequestV1, DaemonRegistryResponseV1, OversizedTaskRecordV1, RegistryRevisionV1,
    TaskListPageRequestV1, TaskListPageV1, WatchListPageRequestV1, WatchListPageV1,
    MAX_REGISTRY_PAGE_ITEM_BYTES, MAX_REGISTRY_PAGE_LIMIT, MAX_REGISTRY_PAGE_RESPONSE_BYTES,
};
use packet28_daemon_protocol::task::{
    TaskMarkHandoffConsumedResponse, TaskRecord, WatchRegistration,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);
const MAX_TRANSPORT_AUTH_FRAME_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IncomingDaemonRequest {
    Registry(DaemonRegistryRequestV1),
    Legacy(Box<DaemonRequest>),
}

#[derive(Debug)]
enum FrameReadError {
    Io(std::io::Error),
    Deadline(&'static str),
    Protocol(FrameError),
}

impl fmt::Display for FrameReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Deadline(phase) => {
                write!(formatter, "daemon frame {phase} read deadline exceeded")
            }
            Self::Protocol(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FrameReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Deadline(_) => None,
        }
    }
}

pub(crate) async fn authenticate_tcp_connection<S>(
    stream: &mut S,
    expected: &DaemonTransportAuth,
    config: &DaemonRuntimeConfig,
    blocking_pool: &BlockingPool,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let candidate =
        match read_frame_bytes_with_limit(stream, config, MAX_TRANSPORT_AUTH_FRAME_BYTES).await {
            Ok(frame) => blocking_pool
                .run_control(move || {
                    serde_json::from_slice::<DaemonTransportAuth>(&frame)
                        .map_err(|_| anyhow!("invalid daemon transport authentication prelude"))
                })
                .await
                .ok(),
            Err(_) => None,
        };
    let Some(candidate) = candidate else {
        write_tcp_auth_rejection(stream, config, blocking_pool).await?;
        return Ok(false);
    };
    if !expected.authenticates(&candidate) {
        write_tcp_auth_rejection(stream, config, blocking_pool).await?;
        return Ok(false);
    }
    write_control_frame(
        stream,
        DaemonResponse::Ack {
            message: "authenticated".to_string(),
        },
        config.frame_write_timeout,
        blocking_pool,
    )
    .await?;
    Ok(true)
}

async fn write_tcp_auth_rejection<S>(
    stream: &mut S,
    config: &DaemonRuntimeConfig,
    blocking_pool: &BlockingPool,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_control_frame(
        stream,
        DaemonResponse::Error {
            message: "daemon transport authentication failed".to_string(),
        },
        config.frame_write_timeout,
        blocking_pool,
    )
    .await
}

pub(crate) async fn handle_connection<S>(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    mut stream: S,
    config: DaemonRuntimeConfig,
    blocking_pool: BlockingPool,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut shutdown = state.lock().map_err(lock_err)?.shutdown.subscribe();
    loop {
        let frame = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            frame = read_frame_bytes(&mut stream, &config) => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) if is_benign_disconnect_error(&error) => return Ok(()),
            Err(error) => {
                let response = DaemonResponse::Error {
                    message: error.to_string(),
                };
                write_async_frame(
                    &mut stream,
                    response,
                    config.frame_write_timeout,
                    &blocking_pool,
                )
                .await?;
                return Ok(());
            }
        };
        let request = match blocking_pool
            .run_control(move || {
                serde_json::from_slice::<IncomingDaemonRequest>(&frame)
                    .map_err(|error| anyhow!("invalid daemon request frame: {error}"))
            })
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let response = DaemonResponse::Error {
                    message: error.to_string(),
                };
                write_async_frame(
                    &mut stream,
                    response,
                    config.frame_write_timeout,
                    &blocking_pool,
                )
                .await?;
                return Ok(());
            }
        };
        let request = match request {
            IncomingDaemonRequest::Registry(request) => {
                let request_state = state.clone();
                let response = blocking_pool
                    .run(move || handle_registry_request_v1(request_state, request))
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        let message = format!("{error:#}");
                        daemon_log(&format!("daemon registry request failed: {message}"));
                        DaemonRegistryResponseV1::Error { message }
                    }
                };
                write_async_frame(
                    &mut stream,
                    response,
                    config.frame_write_timeout,
                    &blocking_pool,
                )
                .await?;
                continue;
            }
            IncomingDaemonRequest::Legacy(request) => *request,
        };
        if matches!(&request, DaemonRequest::Stop) {
            write_control_frame(
                &mut stream,
                DaemonResponse::Ack {
                    message: "stopping".to_string(),
                },
                config.frame_write_timeout,
                &blocking_pool,
            )
            .await?;
            request_daemon_stop(&state, &blocking_pool)?;
            return Ok(());
        }
        if let DaemonRequest::TaskSubscribe {
            task_id,
            replay_last,
            after_seq,
        } = request
        {
            return handle_task_subscribe(
                state,
                &mut stream,
                task_id,
                replay_last,
                after_seq,
                &config,
                &blocking_pool,
            )
            .await;
        }

        let control_response = matches!(&request, DaemonRequest::TaskCancel { .. });
        let response =
            dispatch_request(state.clone(), watch_tx.clone(), request, &blocking_pool).await;
        let response = match response {
            Ok(value) => value,
            Err(error) => {
                let message = format!("{error:#}");
                daemon_log(&format!("daemon request failed: {message}"));
                DaemonResponse::Error { message }
            }
        };
        if control_response {
            write_control_frame(
                &mut stream,
                response,
                config.frame_write_timeout,
                &blocking_pool,
            )
            .await?;
        } else {
            write_async_frame(
                &mut stream,
                response,
                config.frame_write_timeout,
                &blocking_pool,
            )
            .await?;
        }
        if state.lock().map_err(lock_err)?.shutdown.is_requested() {
            return Ok(());
        }
    }
}

fn request_daemon_stop(
    state: &Arc<Mutex<DaemonState>>,
    blocking_pool: &BlockingPool,
) -> Result<()> {
    let (shutdown, index_result) = {
        let mut guard = state.lock().map_err(lock_err)?;
        guard.shutting_down = true;
        let index_result = guard.index_tx.send(IndexCommand::Shutdown);
        (guard.shutdown.clone(), index_result)
    };
    // The acknowledgement is already on the wire. This is the stop
    // linearization point: no later blocking request or child can be admitted.
    blocking_pool.request_shutdown();
    shutdown.request();
    index_result
}

async fn dispatch_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    request: DaemonRequest,
    blocking_pool: &BlockingPool,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::TaskAwaitHandoff { request } => {
            let response = await_task_handoff(state, request, blocking_pool.clone()).await?;
            Ok(DaemonResponse::TaskAwaitHandoff { response })
        }
        DaemonRequest::TaskLaunchAgent { mut request } if request.wait_for_handoff => {
            let wait_request = launch_wait_request(&state, &request)?;
            let _ = await_task_handoff(state.clone(), wait_request, blocking_pool.clone()).await?;
            request.wait_for_handoff = false;
            run_blocking_request(
                state,
                watch_tx,
                DaemonRequest::TaskLaunchAgent { request },
                blocking_pool,
            )
            .await
        }
        request => run_blocking_request(state, watch_tx, request, blocking_pool).await,
    }
}

async fn run_blocking_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    request: DaemonRequest,
    blocking_pool: &BlockingPool,
) -> Result<DaemonResponse> {
    let request_state = state.clone();
    let response = if matches!(&request, DaemonRequest::TaskCancel { .. }) {
        blocking_pool
            .run_cancellation(move || handle_request_and_flush(request_state, watch_tx, request))
            .await?
    } else {
        blocking_pool
            .run(move || handle_request_and_flush(request_state, watch_tx, request))
            .await?
    };
    let changes = state.lock().map_err(lock_err)?.changes.clone();
    changes.notify();
    Ok(response)
}

fn handle_request_and_flush(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    request: DaemonRequest,
) -> Result<DaemonResponse> {
    let request_result = handle_request(state.clone(), watch_tx, request);
    let flush_result = flush_persistence(&state);
    match (request_result, flush_result) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.context("failed to checkpoint daemon request state")),
        (Err(error), Err(flush_error)) => Err(error.context(format!(
            "daemon request also failed to checkpoint state: {flush_error:#}"
        ))),
    }
}

fn launch_wait_request(
    state: &Arc<Mutex<DaemonState>>,
    request: &TaskLaunchAgentRequest,
) -> Result<TaskAwaitHandoffRequest> {
    let after_context_version = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&request.task_id)
        .and_then(|task| {
            task.latest_agent_bootstrap_mode
                .as_deref()
                .filter(|mode| *mode == "handoff")
                .and(task.latest_agent_context_version.clone())
        });
    Ok(TaskAwaitHandoffRequest {
        task_id: request.task_id.clone(),
        timeout_ms: request.handoff_timeout_ms,
        poll_ms: request.handoff_poll_ms,
        after_context_version,
    })
}

async fn await_task_handoff(
    state: Arc<Mutex<DaemonState>>,
    request: TaskAwaitHandoffRequest,
    blocking_pool: BlockingPool,
) -> Result<TaskAwaitHandoffResponse> {
    if request.task_id.trim().is_empty() {
        anyhow::bail!("daemon task await-handoff requires task_id");
    }
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(300_000));
    let poll = Duration::from_millis(request.poll_ms.unwrap_or(250).max(10));
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let (mut changes, mut shutdown) = {
        let guard = state.lock().map_err(lock_err)?;
        (guard.changes.subscribe(), guard.shutdown.subscribe())
    };
    let mut polls = 0_u64;
    loop {
        polls = polls.saturating_add(1);
        let status_state = state.clone();
        let task_id = request.task_id.clone();
        let status = blocking_pool
            .run(move || broker_task_status(status_state, BrokerTaskStatusRequest { task_id }))
            .await?;
        let is_newer_than_after = request
            .after_context_version
            .as_ref()
            .is_none_or(|after| status.latest_context_version.as_deref() != Some(after.as_str()));
        if status.handoff_ready && is_newer_than_after {
            return Ok(TaskAwaitHandoffResponse {
                task_status: status,
                waited_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                polls,
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(handoff_timeout_error(&request, status));
        }
        let next_poll = (tokio::time::Instant::now() + poll).min(deadline);
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    anyhow::bail!(
                        "daemon stopped while waiting for Packet28 handoff for task '{}'",
                        request.task_id
                    );
                }
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    tokio::time::sleep_until(next_poll).await;
                }
            }
            () = tokio::time::sleep_until(next_poll) => {}
        }
    }
}

fn handoff_timeout_error(
    request: &TaskAwaitHandoffRequest,
    status: BrokerTaskStatusResponse,
) -> anyhow::Error {
    let waiting_for_newer_context = request
        .after_context_version
        .as_ref()
        .is_some_and(|after| status.latest_context_version.as_deref() == Some(after.as_str()));
    let reason = if waiting_for_newer_context {
        request
            .after_context_version
            .as_ref()
            .map(|after| {
                format!("newer handoff than context version '{after}' did not become ready")
            })
            .unwrap_or_else(|| "handoff did not become ready".to_string())
    } else {
        status
            .handoff_reason
            .unwrap_or_else(|| "handoff did not become ready".to_string())
    };
    anyhow!(
        "timed out waiting for Packet28 handoff for task '{}': {}",
        request.task_id,
        reason
    )
}

struct SubscriberRegistration {
    state: Arc<Mutex<DaemonState>>,
    task_id: String,
    subscriber_id: u64,
}

impl Drop for SubscriberRegistration {
    fn drop(&mut self) {
        let Ok(mut guard) = self.state.lock() else {
            return;
        };
        if let Some(subscribers) = guard.subscribers.get_mut(&self.task_id) {
            subscribers.retain(|subscriber| subscriber.id != self.subscriber_id);
            if subscribers.is_empty() {
                guard.subscribers.remove(&self.task_id);
            }
        }
    }
}

async fn handle_task_subscribe<S>(
    state: Arc<Mutex<DaemonState>>,
    stream: &mut S,
    task_id: String,
    replay_last: usize,
    after_seq: Option<u64>,
    config: &DaemonRuntimeConfig,
    blocking_pool: &BlockingPool,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let subscriber_id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(config.subscriber_queue_capacity);
    let (root, snapshot_seq, mut shutdown) = {
        let mut guard = state.lock().map_err(lock_err)?;
        let snapshot_seq = guard
            .tasks
            .tasks
            .get(&task_id)
            .map_or(0, |task| task.last_event_seq);
        guard
            .subscribers
            .entry(task_id.clone())
            .or_default()
            .push(TaskSubscriber {
                id: subscriber_id,
                sender,
            });
        (guard.root.clone(), snapshot_seq, guard.shutdown.subscribe())
    };
    let _registration = SubscriberRegistration {
        state,
        task_id: task_id.clone(),
        subscriber_id,
    };
    let replay_task_id = task_id.clone();
    let replay = blocking_pool
        .run(move || Ok(load_task_events(&root, &replay_task_id)?))
        .await?;
    let replay = select_replay_events(
        replay
            .into_iter()
            .filter(|frame| frame.seq <= snapshot_seq)
            .collect(),
        replay_last,
        after_seq,
    );
    write_async_frame(
        stream,
        DaemonResponse::TaskSubscribeAck {
            task_id: task_id.clone(),
            replayed: replay.len(),
            after_seq,
        },
        config.frame_write_timeout,
        blocking_pool,
    )
    .await?;
    for frame in replay {
        write_async_frame(stream, frame, config.frame_write_timeout, blocking_pool).await?;
    }

    loop {
        let frame = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            frame = receiver.recv() => frame,
        };
        let Some(frame) = frame else {
            // A full bounded queue removes the sender. Closing the stream after
            // draining makes lag visible; the client's last sequence is then
            // an exact replay cursor for the next subscription.
            break;
        };
        write_async_frame(stream, frame, config.frame_write_timeout, blocking_pool).await?;
    }
    Ok(())
}

pub(crate) fn select_replay_events(
    events: Vec<DaemonEventFrame>,
    replay_last: usize,
    after_seq: Option<u64>,
) -> Vec<DaemonEventFrame> {
    let replay: Vec<_> = match after_seq {
        Some(after_seq) => events
            .into_iter()
            .filter(|frame| frame.seq > after_seq)
            .collect(),
        None => events,
    };
    if replay_last == 0 || replay_last >= replay.len() {
        replay
    } else {
        replay[replay.len().saturating_sub(replay_last)..].to_vec()
    }
}

async fn read_frame_bytes<R>(
    reader: &mut R,
    config: &DaemonRuntimeConfig,
) -> std::result::Result<Vec<u8>, FrameReadError>
where
    R: AsyncRead + Unpin,
{
    read_frame_bytes_with_limit(reader, config, MAX_SOCKET_MESSAGE_BYTES).await
}

async fn read_frame_bytes_with_limit<R>(
    reader: &mut R,
    config: &DaemonRuntimeConfig,
    max_bytes: usize,
) -> std::result::Result<Vec<u8>, FrameReadError>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0_u8; 8];
    read_exact_with_deadline(
        reader,
        &mut len_bytes,
        config.frame_header_timeout,
        "header",
    )
    .await?;
    let declared = u64::from_be_bytes(len_bytes);
    let len = usize::try_from(declared)
        .map_err(|_| FrameReadError::Protocol(FrameError::LengthOverflow))?;
    if len == 0 {
        return Err(FrameReadError::Protocol(FrameError::Empty));
    }
    if len > max_bytes {
        return Err(FrameReadError::Protocol(FrameError::TooLarge {
            actual: declared,
            limit: max_bytes,
        }));
    }
    let mut body = vec![0_u8; len];
    read_exact_with_deadline(reader, &mut body, config.frame_body_timeout, "body").await?;
    Ok(body)
}

async fn read_exact_with_deadline<R>(
    reader: &mut R,
    bytes: &mut [u8],
    deadline: Duration,
    phase: &'static str,
) -> std::result::Result<(), FrameReadError>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(deadline, reader.read_exact(bytes))
        .await
        .map_err(|_| FrameReadError::Deadline(phase))?
        .map(|_| ())
        .map_err(FrameReadError::Io)
}

async fn write_async_frame<W, T>(
    writer: &mut W,
    value: T,
    deadline: Duration,
    blocking_pool: &BlockingPool,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + Send + 'static,
{
    write_async_frame_with_lane(writer, value, deadline, blocking_pool, false).await
}

async fn write_control_frame<W, T>(
    writer: &mut W,
    value: T,
    deadline: Duration,
    blocking_pool: &BlockingPool,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + Send + 'static,
{
    write_async_frame_with_lane(writer, value, deadline, blocking_pool, true).await
}

async fn write_async_frame_with_lane<W, T>(
    writer: &mut W,
    value: T,
    deadline: Duration,
    blocking_pool: &BlockingPool,
    control: bool,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + Send + 'static,
{
    let encode = move || {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &value)?;
        Ok(encoded)
    };
    let encoded = if control {
        blocking_pool.run_control(encode).await?
    } else {
        blocking_pool.run(encode).await?
    };
    tokio::time::timeout(deadline, async {
        writer.write_all(&encoded).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| anyhow!("daemon frame write deadline exceeded"))??;
    Ok(())
}

fn is_benign_disconnect_error(error: &FrameReadError) -> bool {
    matches!(
        error,
        FrameReadError::Io(io_error)
            if matches!(
                io_error.kind(),
                ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::UnexpectedEof
            )
    )
}

fn handle_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: WatchIngress,
    request: DaemonRequest,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Execute { request } => {
            let kernel = kernel_for_request(&state, &request)?;
            let response = kernel.execute(request)?;
            Ok(DaemonResponse::Execute { response })
        }
        DaemonRequest::ExecuteSequence { spec } => {
            let TaskAdmission {
                task,
                watches,
                generation,
                replaced_task,
            } = register_task_and_watches(state.clone(), watch_tx, spec)?;
            let response =
                match run_initial_sequence_for_task(state.clone(), &task.task_id, generation) {
                    Ok(response) => response,
                    Err(err) => {
                        daemon_log(&format!(
                            "initial task run failed task_id={} error={err}",
                            task.task_id
                        ));
                        return Err(rollback_initial_task_admission(
                            state,
                            &task.task_id,
                            generation,
                            replaced_task,
                            err,
                        ));
                    }
                };
            if let Some(failure) = response
                .step_results
                .iter()
                .find_map(|step| step.failure.as_ref())
            {
                let message = failure.message.clone();
                daemon_log(&format!(
                    "initial task run failed task_id={} error={message}",
                    task.task_id
                ));
                return Err(rollback_initial_task_admission(
                    state,
                    &task.task_id,
                    generation,
                    replaced_task,
                    anyhow!(message),
                ));
            }
            let task = state
                .lock()
                .map_err(lock_err)?
                .tasks
                .tasks
                .get(&task.task_id)
                .cloned()
                .unwrap_or(task);
            Ok(DaemonResponse::ExecuteSequence {
                response,
                task,
                watches,
            })
        }
        DaemonRequest::Status => {
            let guard = state.lock().map_err(lock_err)?;
            let status = build_status(&guard)?;
            Ok(DaemonResponse::Status { status })
        }
        DaemonRequest::Stop => Err(anyhow!(
            "daemon stop must be acknowledged by the async control plane"
        )),
        DaemonRequest::TaskStatus { task_id } => {
            let task = state
                .lock()
                .map_err(lock_err)?
                .tasks
                .tasks
                .get(&task_id)
                .cloned();
            let response = DaemonResponse::TaskStatus { task };
            ensure_legacy_response_fits(&response, "task_status", "task_list_page_v1")?;
            Ok(response)
        }
        DaemonRequest::TaskAwaitHandoff { .. } => Err(anyhow!(
            "task await-handoff must run on the daemon async orchestration boundary"
        )),
        DaemonRequest::TaskMarkHandoffConsumed { request } => {
            let response = TaskMarkHandoffConsumedResponse {
                handoff: mark_handoff_consumed(&state, &request.task_id, &request.handoff_id)?,
            };
            Ok(DaemonResponse::TaskMarkHandoffConsumed { response })
        }
        DaemonRequest::TaskLaunchAgent { request } => {
            let response = task_launch_agent(state, request)?;
            Ok(DaemonResponse::TaskLaunchAgent { response })
        }
        DaemonRequest::TaskCancel { task_id } => {
            let cancellation = cancel_task(state, &task_id)?;
            Ok(DaemonResponse::TaskCancel {
                task: cancellation.0,
                removed_watch_ids: cancellation.1,
            })
        }
        DaemonRequest::TaskSubscribe { .. } => {
            Err(anyhow!("task subscribe is handled as a streaming request"))
        }
        DaemonRequest::WatchList { task_id } => {
            let state = state.lock().map_err(lock_err)?;
            let watches = state
                .watches
                .watches
                .iter()
                .filter(|watch| {
                    task_id
                        .as_ref()
                        .map(|task_id| watch.spec.task_id == *task_id)
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            let response = DaemonResponse::WatchList { watches };
            ensure_legacy_response_fits(&response, "watch_list", "watch_list_page_v1")?;
            Ok(response)
        }
        DaemonRequest::WatchRemove { watch_id } => {
            let removed = remove_watch(state, &watch_id)?;
            Ok(DaemonResponse::WatchRemove { removed })
        }
        DaemonRequest::PacketFetch { request } => {
            let root = resolve_root(Path::new(&request.root));
            let value = suite_packet_core::read_packet_artifact(&root, &request.handle)
                .map_err(|source| anyhow!(source.to_string()))?;
            let wrapper = serde_json::from_value(value)
                .map_err(|source| anyhow!("invalid packet artifact: {source}"))?;
            Ok(DaemonResponse::PacketFetch {
                response: PacketFetchResponse { wrapper },
            })
        }
        DaemonRequest::CoverCheck { request } => {
            let response = run_cover_check(request)?;
            Ok(DaemonResponse::CoverCheck { response })
        }
        DaemonRequest::TestShard { request } => {
            let response = run_test_shard(request)?;
            Ok(DaemonResponse::TestShard { response })
        }
        DaemonRequest::TestMap { request } => {
            let response = run_test_map(request)?;
            Ok(DaemonResponse::TestMap { response })
        }
        DaemonRequest::ContextStoreList { request } => {
            let kernel = kernel_for_context_root(&state, &request.root)?;
            let response = run_context_store_list(&kernel, request)?;
            Ok(DaemonResponse::ContextStoreList { response })
        }
        DaemonRequest::ContextStoreGet { request } => {
            let kernel = kernel_for_context_root(&state, &request.root)?;
            let response = run_context_store_get(&kernel, request)?;
            Ok(DaemonResponse::ContextStoreGet { response })
        }
        DaemonRequest::ContextStorePrune { request } => {
            let kernel = kernel_for_context_root(&state, &request.root)?;
            let response = run_context_store_prune(&kernel, request)?;
            Ok(DaemonResponse::ContextStorePrune { response })
        }
        DaemonRequest::ContextStoreStats { request } => {
            let kernel = kernel_for_context_root(&state, &request.root)?;
            let response = run_context_store_stats(&kernel, request)?;
            Ok(DaemonResponse::ContextStoreStats { response })
        }
        DaemonRequest::ContextRecall { request } => {
            let kernel = kernel_for_context_root(&state, &request.root)?;
            let response = run_context_recall(&kernel, request)?;
            Ok(DaemonResponse::ContextRecall { response })
        }
        DaemonRequest::BrokerGetContext { request } => {
            let response = broker_get_context(state, request)?;
            Ok(DaemonResponse::BrokerGetContext { response })
        }
        DaemonRequest::BrokerEstimateContext { request } => {
            let response = broker_estimate_context(state, request)?;
            Ok(DaemonResponse::BrokerEstimateContext { response })
        }
        DaemonRequest::BrokerPrepareHandoff { request } => {
            let response = broker_prepare_handoff(state, request)?;
            Ok(DaemonResponse::BrokerPrepareHandoff { response })
        }
        DaemonRequest::BrokerValidatePlan { request } => {
            let response = broker_validate_plan(state, request)?;
            Ok(DaemonResponse::BrokerValidatePlan { response })
        }
        DaemonRequest::BrokerDecompose { request } => {
            let response = broker_decompose(state, request)?;
            Ok(DaemonResponse::BrokerDecompose { response })
        }
        DaemonRequest::BrokerWriteState { request } => {
            let response = broker_write_state(state, request)?;
            Ok(DaemonResponse::BrokerWriteState { response })
        }
        DaemonRequest::BrokerWriteStateBatch { request } => {
            let response = broker_write_state_batch(state, request)?;
            Ok(DaemonResponse::BrokerWriteStateBatch { response })
        }
        DaemonRequest::BrokerTaskStatus { request } => {
            let response = broker_task_status(state, request)?;
            Ok(DaemonResponse::BrokerTaskStatus { response })
        }
        DaemonRequest::ContextResolve { request } => {
            let response = resolve_context(state, request)?;
            Ok(DaemonResponse::ContextResolve { response })
        }
        DaemonRequest::InstructionFileResolve { request } => {
            let response = resolve_instruction_file(state, request)?;
            Ok(DaemonResponse::InstructionFileResolve { response })
        }
        DaemonRequest::HookIngest { request } => {
            let response = hook_ingest(state, request)?;
            Ok(DaemonResponse::HookIngest { response })
        }
        DaemonRequest::Packet28Search { request } => {
            let response = daemon_packet28_search(state, request)?;
            Ok(DaemonResponse::Packet28Search { response })
        }
        DaemonRequest::Packet28SearchGuard { request } => {
            let response = crate::index::daemon_packet28_search_guard(state, request)?;
            Ok(DaemonResponse::Packet28SearchGuard { response })
        }
        DaemonRequest::DaemonIndexStatus { request: _ } => {
            let response = daemon_index_status(state)?;
            Ok(DaemonResponse::DaemonIndexStatus { response })
        }
        DaemonRequest::DaemonIndexRebuild { request } => {
            let response = daemon_index_rebuild(state, request)?;
            Ok(DaemonResponse::DaemonIndexRebuild { response })
        }
        DaemonRequest::DaemonIndexClear { request: _ } => {
            let response = daemon_index_clear(state)?;
            Ok(DaemonResponse::DaemonIndexClear { response })
        }
    }
}

const MAX_REGISTRY_PAGE_COLLECTION_BYTES: usize = MAX_REGISTRY_PAGE_RESPONSE_BYTES / 2;

fn handle_registry_request_v1(
    state: Arc<Mutex<DaemonState>>,
    request: DaemonRegistryRequestV1,
) -> Result<DaemonRegistryResponseV1> {
    let mut state = state.lock().map_err(lock_err)?;
    match request {
        DaemonRegistryRequestV1::Status => Ok(DaemonRegistryResponseV1::Status {
            status: Box::new(build_registry_status_v1(&state)?),
        }),
        DaemonRegistryRequestV1::TaskListPage { request } => {
            let revision = state.registry_revision();
            Ok(DaemonRegistryResponseV1::TaskListPage {
                page: build_task_list_page(&state.tasks.tasks, &revision, &request)?,
            })
        }
        DaemonRegistryRequestV1::WatchListPage { request } => {
            let revision = state.registry_revision();
            validate_registry_snapshot("watch", &revision, request.snapshot_revision.as_ref())?;
            ensure_registry_page_index(&mut state, &revision)?;
            let index = state
                .registry_page_index
                .as_ref()
                .ok_or_else(|| anyhow!("watch registry page index is unavailable"))?;
            Ok(DaemonRegistryResponseV1::WatchListPage {
                page: build_watch_list_page(&state.watches.watches, index, &revision, &request)?,
            })
        }
    }
}

fn ensure_registry_page_index(
    state: &mut DaemonState,
    revision: &RegistryRevisionV1,
) -> Result<()> {
    if state
        .registry_page_index
        .as_ref()
        .is_some_and(|index| &index.revision == revision)
    {
        return Ok(());
    }
    let mut index = crate::state::RegistryPageIndex {
        revision: revision.clone(),
        ..crate::state::RegistryPageIndex::default()
    };
    for (position, watch) in state.watches.watches.iter().enumerate() {
        if index
            .watch_positions
            .insert(watch.watch_id.clone(), position)
            .is_some()
        {
            anyhow::bail!(
                "watch registry contains duplicate identifier '{}'",
                watch.watch_id
            );
        }
        index
            .watch_ids_by_task
            .entry(watch.spec.task_id.clone())
            .or_default()
            .insert(watch.watch_id.clone());
    }
    state.registry_page_index = Some(index);
    Ok(())
}

/// Builds a bounded task page while reporting records that cannot fit any page.
fn build_task_list_page(
    tasks: &BTreeMap<String, TaskRecord>,
    revision: &RegistryRevisionV1,
    request: &TaskListPageRequestV1,
) -> Result<TaskListPageV1> {
    validate_registry_page_request(
        "task",
        request.limit,
        request.after_task_id.as_deref(),
        None,
    )?;
    validate_registry_snapshot("task", revision, request.snapshot_revision.as_ref())?;
    if let Some(cursor) = request.after_task_id.as_ref() {
        if !tasks.contains_key(cursor) {
            anyhow::bail!("task page cursor '{cursor}' is not present in the live registry");
        }
    }

    let mut page = TaskListPageV1 {
        snapshot_revision: revision.clone(),
        tasks: Vec::new(),
        next_after_task_id: None,
        total: tasks.len(),
        omitted_oversized: Vec::new(),
    };
    let mut collection_bytes = 0_usize;
    let mut has_more = false;
    let start = request
        .after_task_id
        .as_ref()
        .map_or(Unbounded, |cursor| Excluded(cursor.clone()));
    for (task_id, task) in tasks.range((start, Unbounded)) {
        if page.tasks.len() == request.limit {
            has_more = true;
            break;
        }
        let item_bytes = match encoded_registry_page_item_bytes_checked(task, "task", task_id)? {
            Ok(item_bytes) => item_bytes,
            Err(encoded_bytes) => {
                // Individually oversized: it can never be paginated. Record it
                // and keep going so one poison record cannot brick the whole
                // task listing; retention can still target it directly.
                page.omitted_oversized.push(OversizedTaskRecordV1 {
                    task_id: task_id.clone(),
                    encoded_bytes,
                });
                continue;
            }
        };
        let separator = usize::from(!page.tasks.is_empty());
        let Some(next_bytes) = collection_bytes
            .checked_add(item_bytes)
            .and_then(|bytes| bytes.checked_add(separator))
        else {
            anyhow::bail!("task page byte accounting overflow");
        };
        if next_bytes > MAX_REGISTRY_PAGE_COLLECTION_BYTES {
            if page.tasks.is_empty() {
                // Fits the per-record bound but is larger than the whole
                // collection budget, so it cannot fit any page. Omit it too
                // rather than failing the listing.
                page.omitted_oversized.push(OversizedTaskRecordV1 {
                    task_id: task_id.clone(),
                    encoded_bytes: item_bytes as u64,
                });
                continue;
            }
            has_more = true;
            break;
        }
        collection_bytes = next_bytes;
        page.tasks.push(task.clone());
    }
    if has_more {
        page.next_after_task_id = page.tasks.last().map(|task| task.task_id.clone());
    }
    ensure_registry_page_response_fits(
        &DaemonRegistryResponseV1::TaskListPage { page: page.clone() },
        "task",
    )?;
    Ok(page)
}

fn build_watch_list_page(
    watches: &[WatchRegistration],
    index: &crate::state::RegistryPageIndex,
    revision: &RegistryRevisionV1,
    request: &WatchListPageRequestV1,
) -> Result<WatchListPageV1> {
    validate_registry_page_request(
        "watch",
        request.limit,
        request.after_watch_id.as_deref(),
        request.task_id.as_deref(),
    )?;
    validate_registry_snapshot("watch", revision, request.snapshot_revision.as_ref())?;
    let start = request
        .after_watch_id
        .as_ref()
        .map_or(Unbounded, |cursor| Excluded(cursor.clone()));
    let empty = BTreeSet::new();
    let filtered_ids = request
        .task_id
        .as_ref()
        .map(|task_id| index.watch_ids_by_task.get(task_id).unwrap_or(&empty));
    if let Some(cursor) = request.after_watch_id.as_ref() {
        let contains_cursor = filtered_ids.map_or_else(
            || index.watch_positions.contains_key(cursor),
            |ids| ids.contains(cursor),
        );
        if !contains_cursor {
            anyhow::bail!(
                "watch page cursor '{cursor}' is not present in the filtered live registry"
            );
        }
    }

    match filtered_ids {
        Some(ids) => collect_watch_list_page(
            ids.range((start, Unbounded)),
            ids.len(),
            watches,
            index,
            revision,
            request.limit,
        ),
        None => collect_watch_list_page(
            index
                .watch_positions
                .range((start, Unbounded))
                .map(|(id, _)| id),
            index.watch_positions.len(),
            watches,
            index,
            revision,
            request.limit,
        ),
    }
}

fn collect_watch_list_page<'a>(
    ids: impl Iterator<Item = &'a String>,
    total: usize,
    watches: &[WatchRegistration],
    index: &crate::state::RegistryPageIndex,
    revision: &RegistryRevisionV1,
    limit: usize,
) -> Result<WatchListPageV1> {
    let mut page = WatchListPageV1 {
        snapshot_revision: revision.clone(),
        watches: Vec::new(),
        next_after_watch_id: None,
        total,
    };
    let mut collection_bytes = 0_usize;
    let mut has_more = false;
    for watch_id in ids {
        if page.watches.len() == limit {
            has_more = true;
            break;
        }
        let position = index.watch_positions.get(watch_id).ok_or_else(|| {
            anyhow!("watch page index lost identifier '{watch_id}' at revision {revision}")
        })?;
        let watch = watches.get(*position).ok_or_else(|| {
            anyhow!("watch page index position for '{watch_id}' is out of bounds")
        })?;
        let item_bytes = encoded_registry_page_item_bytes(watch, "watch", watch_id)?;
        let separator = usize::from(!page.watches.is_empty());
        let Some(next_bytes) = collection_bytes
            .checked_add(item_bytes)
            .and_then(|bytes| bytes.checked_add(separator))
        else {
            anyhow::bail!("watch page byte accounting overflow");
        };
        if next_bytes > MAX_REGISTRY_PAGE_COLLECTION_BYTES {
            if page.watches.is_empty() {
                anyhow::bail!(
                    "watch record '{}' cannot fit within the \
                     {MAX_REGISTRY_PAGE_COLLECTION_BYTES}-byte page collection bound",
                    watch.watch_id
                );
            }
            has_more = true;
            break;
        }
        collection_bytes = next_bytes;
        page.watches.push(watch.clone());
    }
    if has_more {
        page.next_after_watch_id = page.watches.last().map(|watch| watch.watch_id.clone());
    }
    ensure_registry_page_response_fits(
        &DaemonRegistryResponseV1::WatchListPage { page: page.clone() },
        "watch",
    )?;
    Ok(page)
}

fn validate_registry_snapshot(
    kind: &str,
    current_revision: &RegistryRevisionV1,
    requested_revision: Option<&RegistryRevisionV1>,
) -> Result<()> {
    if let Some(requested_revision) = requested_revision {
        if requested_revision != current_revision {
            anyhow::bail!(
                "{kind} registry changed during pagination: requested revision \
                 {requested_revision}, current revision {current_revision}; restart pagination"
            );
        }
    }
    Ok(())
}

fn validate_registry_page_request(
    kind: &str,
    limit: usize,
    cursor: Option<&str>,
    filter: Option<&str>,
) -> Result<()> {
    if !(1..=MAX_REGISTRY_PAGE_LIMIT).contains(&limit) {
        anyhow::bail!(
            "{kind} page limit must be between 1 and {MAX_REGISTRY_PAGE_LIMIT}; found {limit}"
        );
    }
    for (name, value) in [("cursor", cursor), ("task filter", filter)] {
        if value.is_some_and(|value| value.len() > MAX_REGISTRY_PAGE_ITEM_BYTES) {
            anyhow::bail!(
                "{kind} page {name} exceeds the {MAX_REGISTRY_PAGE_ITEM_BYTES}-byte request bound"
            );
        }
    }
    Ok(())
}

/// Returns the compact-JSON size of a registry record that fits the item bound.
fn encoded_registry_page_item_bytes(
    item: &impl Serialize,
    kind: &str,
    identifier: &str,
) -> Result<usize> {
    let item_bytes = serde_json::to_vec(item)
        .with_context(|| format!("failed to encode {kind} page record '{identifier}'"))?
        .len();
    if item_bytes > MAX_REGISTRY_PAGE_ITEM_BYTES {
        anyhow::bail!(
            "{kind} record '{identifier}' encodes to {item_bytes} bytes; maximum paginated record \
             size is {MAX_REGISTRY_PAGE_ITEM_BYTES}"
        );
    }
    Ok(item_bytes)
}

/// Like [`encoded_registry_page_item_bytes`] but reports an over-limit record as
/// `Ok(Err(encoded_bytes))` instead of failing, so a resilient page can skip and
/// report it rather than poisoning the whole listing. The outer `Err` is still
/// reserved for genuine encoding failures.
fn encoded_registry_page_item_bytes_checked(
    item: &impl Serialize,
    kind: &str,
    identifier: &str,
) -> Result<std::result::Result<usize, u64>> {
    let item_bytes = serde_json::to_vec(item)
        .with_context(|| format!("failed to encode {kind} page record '{identifier}'"))?
        .len();
    if item_bytes > MAX_REGISTRY_PAGE_ITEM_BYTES {
        return Ok(Err(item_bytes as u64));
    }
    Ok(Ok(item_bytes))
}

fn ensure_registry_page_response_fits(
    response: &DaemonRegistryResponseV1,
    kind: &str,
) -> Result<()> {
    let bytes = serde_json::to_vec(response)
        .with_context(|| format!("failed to encode bounded {kind} page response"))?
        .len();
    if bytes > MAX_REGISTRY_PAGE_RESPONSE_BYTES {
        anyhow::bail!(
            "{kind} page response encoded to {bytes} bytes; maximum is \
             {MAX_REGISTRY_PAGE_RESPONSE_BYTES}"
        );
    }
    Ok(())
}

fn ensure_legacy_response_fits(
    response: &DaemonResponse,
    request_name: &str,
    paginated_request_name: &str,
) -> Result<()> {
    let bytes = serde_json::to_vec(response)
        .with_context(|| format!("failed to encode legacy {request_name} response"))?
        .len();
    if bytes > MAX_SOCKET_MESSAGE_BYTES {
        anyhow::bail!(
            "{request_name} response is {bytes} bytes and exceeds the \
             {MAX_SOCKET_MESSAGE_BYTES}-byte transport limit; use {paginated_request_name}"
        );
    }
    Ok(())
}

fn rollback_initial_task_admission(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
    generation: TaskGenerationId,
    replaced_task: Option<TaskRecord>,
    admission_error: anyhow::Error,
) -> anyhow::Error {
    match rollback_failed_initial_task_admission(state.clone(), task_id, generation, replaced_task)
    {
        Ok(()) => admission_error,
        Err(rollback_error) => {
            if let Ok(guard) = state.lock().map_err(lock_err) {
                guard.shutdown.request();
            }
            rollback_error.context(format!(
                "initial task admission for '{task_id}' failed ({admission_error:#}) and rollback \
                 also failed"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL_LANE_DEADLOCK_TIMEOUT: Duration = Duration::from_secs(10);

    async fn exchange(
        stream: &mut tokio::io::DuplexStream,
        request: &DaemonRequest,
    ) -> DaemonResponse {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, request).unwrap();
        stream.write_all(&encoded).await.unwrap();
        let mut header = [0_u8; 8];
        stream.read_exact(&mut header).await.unwrap();
        let length = usize::try_from(u64::from_be_bytes(header)).unwrap();
        let mut body = vec![0_u8; length];
        stream.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn frame(seq: u64) -> DaemonEventFrame {
        DaemonEventFrame {
            seq,
            task_id: "task-1".to_string(),
            event: DaemonEvent {
                kind: "test".to_string(),
                occurred_at_unix: seq,
                data: json!({ "seq": seq }),
            },
        }
    }

    #[test]
    fn broker_task_status_dispatch_matches_the_owning_facade() {
        let state = crate::tests::support::daemon_test_state();
        let task_id = "broker-facade-parity";
        crate::tests::support::insert_admitted_task_record(
            &state,
            TaskRecord {
                task_id: task_id.to_string(),
                latest_context_version: Some("7".to_string()),
                latest_context_reason: Some("facade parity".to_string()),
                ..TaskRecord::default()
            },
        );
        let request = BrokerTaskStatusRequest {
            task_id: task_id.to_string(),
        };
        let direct = broker_task_status(state.clone(), request.clone()).unwrap();
        let (watch_tx, _watch_rx) = WatchIngress::new(1);

        let dispatched = handle_request(
            state.clone(),
            watch_tx,
            DaemonRequest::BrokerTaskStatus { request },
        )
        .unwrap();
        let DaemonResponse::BrokerTaskStatus { response } = dispatched else {
            panic!("broker task status dispatch returned the wrong response variant");
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::to_value(direct).unwrap()
        );
        crate::tests::support::shutdown_test_persistence(&state);
    }

    #[test]
    fn replay_after_seq_returns_only_later_frames() {
        let selected = select_replay_events(vec![frame(1), frame(2), frame(3)], 0, Some(1));
        assert_eq!(
            selected.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn replay_last_limits_frames_after_cursor() {
        let selected =
            select_replay_events(vec![frame(1), frame(2), frame(3), frame(4)], 2, Some(1));
        assert_eq!(
            selected.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    fn registry_revision(revision: u64) -> RegistryRevisionV1 {
        RegistryRevisionV1 {
            instance_id: "test-registry-instance".to_string(),
            revision,
        }
    }

    #[test]
    fn task_registry_pages_are_ordered_and_cursor_forward() {
        let tasks = ["task-c", "task-a", "task-b"]
            .into_iter()
            .map(|task_id| {
                (
                    task_id.to_string(),
                    TaskRecord {
                        task_id: task_id.to_string(),
                        ..TaskRecord::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        let first = build_task_list_page(
            &tasks,
            &registry_revision(7),
            &TaskListPageRequestV1 {
                snapshot_revision: None,
                after_task_id: None,
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(
            first
                .tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-a", "task-b"]
        );
        assert_eq!(first.snapshot_revision, registry_revision(7));
        assert_eq!(first.next_after_task_id.as_deref(), Some("task-b"));

        let second = build_task_list_page(
            &tasks,
            &registry_revision(7),
            &TaskListPageRequestV1 {
                snapshot_revision: Some(first.snapshot_revision),
                after_task_id: first.next_after_task_id,
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(
            second
                .tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-c"]
        );
        assert_eq!(second.next_after_task_id, None);
    }

    fn registry_page_index(
        watches: &[WatchRegistration],
        revision: RegistryRevisionV1,
    ) -> crate::state::RegistryPageIndex {
        let mut index = crate::state::RegistryPageIndex {
            revision,
            ..crate::state::RegistryPageIndex::default()
        };
        for (position, watch) in watches.iter().enumerate() {
            assert!(index
                .watch_positions
                .insert(watch.watch_id.clone(), position)
                .is_none());
            index
                .watch_ids_by_task
                .entry(watch.spec.task_id.clone())
                .or_default()
                .insert(watch.watch_id.clone());
        }
        index
    }

    #[test]
    fn watch_registry_pages_filter_then_sort() {
        let watch = |watch_id: &str, task_id: &str| WatchRegistration {
            watch_id: watch_id.to_string(),
            spec: WatchSpec {
                task_id: task_id.to_string(),
                ..WatchSpec::default()
            },
            ..WatchRegistration::default()
        };
        let watches = vec![
            watch("watch-c", "task-a"),
            watch("watch-a", "task-b"),
            watch("watch-b", "task-a"),
        ];
        let index = registry_page_index(&watches, registry_revision(9));

        let page = build_watch_list_page(
            &watches,
            &index,
            &registry_revision(9),
            &WatchListPageRequestV1 {
                snapshot_revision: None,
                task_id: Some("task-a".to_string()),
                after_watch_id: None,
                limit: MAX_REGISTRY_PAGE_LIMIT,
            },
        )
        .unwrap();

        assert_eq!(page.total, 2);
        assert_eq!(
            page.watches
                .iter()
                .map(|watch| watch.watch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["watch-b", "watch-c"]
        );
        assert_eq!(page.next_after_watch_id, None);
    }

    #[test]
    fn registry_pages_reject_invalid_limits_and_stale_cursors() {
        let tasks = BTreeMap::from([(
            "task-a".to_string(),
            TaskRecord {
                task_id: "task-a".to_string(),
                ..TaskRecord::default()
            },
        )]);
        for limit in [0, MAX_REGISTRY_PAGE_LIMIT + 1] {
            let error = build_task_list_page(
                &tasks,
                &registry_revision(7),
                &TaskListPageRequestV1 {
                    snapshot_revision: None,
                    after_task_id: None,
                    limit,
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("page limit"));
        }

        let error = build_task_list_page(
            &tasks,
            &registry_revision(7),
            &TaskListPageRequestV1 {
                snapshot_revision: None,
                after_task_id: Some("task-missing".to_string()),
                limit: 1,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("not present in the live registry"));

        let error = build_task_list_page(
            &tasks,
            &registry_revision(8),
            &TaskListPageRequestV1 {
                snapshot_revision: Some(registry_revision(7)),
                after_task_id: None,
                limit: 1,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed during pagination"));
        assert!(error.to_string().contains("restart pagination"));

        let error = build_task_list_page(
            &tasks,
            &RegistryRevisionV1 {
                instance_id: "replacement-daemon".to_string(),
                revision: 7,
            },
            &TaskListPageRequestV1 {
                snapshot_revision: Some(registry_revision(7)),
                after_task_id: None,
                limit: 1,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed during pagination"));
    }

    #[test]
    fn registry_snapshot_rejects_same_cardinality_in_place_mutation() {
        let state = crate::tests::support::daemon_test_state();
        {
            let mut guard = state.lock().unwrap();
            guard.tasks.tasks.insert(
                "task-a".to_string(),
                TaskRecord {
                    task_id: "task-a".to_string(),
                    last_error: Some("before".to_string()),
                    ..TaskRecord::default()
                },
            );
            persist_state_for_test(&guard).unwrap();
        }
        let first = handle_registry_request_v1(
            state.clone(),
            DaemonRegistryRequestV1::TaskListPage {
                request: TaskListPageRequestV1 {
                    snapshot_revision: None,
                    after_task_id: None,
                    limit: 1,
                },
            },
        )
        .unwrap();
        let first_revision = match first {
            DaemonRegistryResponseV1::TaskListPage { page } => page.snapshot_revision,
            other => panic!("unexpected first page response: {other:?}"),
        };

        {
            let mut guard = state.lock().unwrap();
            guard.tasks.tasks.get_mut("task-a").unwrap().last_error = Some("after".to_string());
            assert_eq!(guard.tasks.tasks.len(), 1);
            persist_state_for_test(&guard).unwrap();
        }
        let error = handle_registry_request_v1(
            state.clone(),
            DaemonRegistryRequestV1::TaskListPage {
                request: TaskListPageRequestV1 {
                    snapshot_revision: Some(first_revision),
                    after_task_id: None,
                    limit: 1,
                },
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("changed during pagination"));
        crate::tests::support::shutdown_test_persistence(&state);
    }

    /// Verifies that an oversized record is reported without hiding healthy tasks.
    #[test]
    fn registry_pages_skip_and_report_an_individually_oversized_record() {
        let healthy_id = "task-healthy";
        let oversized_id = "task-oversized";
        let tasks = BTreeMap::from([
            (
                healthy_id.to_string(),
                TaskRecord {
                    task_id: healthy_id.to_string(),
                    ..TaskRecord::default()
                },
            ),
            (
                oversized_id.to_string(),
                TaskRecord {
                    task_id: oversized_id.to_string(),
                    last_error: Some("x".repeat(MAX_REGISTRY_PAGE_ITEM_BYTES)),
                    ..TaskRecord::default()
                },
            ),
        ]);

        // The oversized record no longer poisons the whole listing: it is
        // skipped and reported, and the healthy task is still returned.
        let page = build_task_list_page(
            &tasks,
            &registry_revision(7),
            &TaskListPageRequestV1 {
                snapshot_revision: None,
                after_task_id: None,
                limit: 10,
            },
        )
        .unwrap();

        assert_eq!(
            page.tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec![healthy_id]
        );
        assert_eq!(page.omitted_oversized.len(), 1);
        assert_eq!(page.omitted_oversized[0].task_id, oversized_id);
        assert!(page.omitted_oversized[0].encoded_bytes >= MAX_REGISTRY_PAGE_ITEM_BYTES as u64);
        assert_eq!(page.total, 2);
    }

    #[test]
    fn task_registry_page_stops_at_its_byte_budget_before_the_item_limit() {
        let tasks = (0..3)
            .map(|index| {
                let task_id = format!("task-{index}");
                (
                    task_id.clone(),
                    TaskRecord {
                        task_id,
                        last_error: Some("x".repeat(800 * 1024)),
                        ..TaskRecord::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        let page = build_task_list_page(
            &tasks,
            &registry_revision(7),
            &TaskListPageRequestV1 {
                snapshot_revision: None,
                after_task_id: None,
                limit: 3,
            },
        )
        .unwrap();

        assert_eq!(page.tasks.len(), 2);
        assert_eq!(page.next_after_task_id.as_deref(), Some("task-1"));
        let response = DaemonRegistryResponseV1::TaskListPage { page };
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_REGISTRY_PAGE_RESPONSE_BYTES);
    }

    #[test]
    fn legacy_registry_responses_fail_before_an_oversized_frame_write() {
        let response = DaemonResponse::WatchList {
            watches: vec![WatchRegistration {
                watch_id: "watch-oversized".to_string(),
                last_error: Some("x".repeat(MAX_SOCKET_MESSAGE_BYTES)),
                ..WatchRegistration::default()
            }],
        };

        let error =
            ensure_legacy_response_fits(&response, "watch_list", "watch_list_page_v1").unwrap_err();

        assert!(error.to_string().contains("exceeds the"));
        assert!(error.to_string().contains("use watch_list_page_v1"));
    }

    fn deadline_config() -> DaemonRuntimeConfig {
        DaemonRuntimeConfig {
            frame_header_timeout: Duration::from_secs(1),
            frame_body_timeout: Duration::from_secs(1),
            frame_write_timeout: Duration::from_secs(1),
            ..DaemonRuntimeConfig::default()
        }
    }

    fn fresh_hook_artifact_packet() -> packet28_daemon_protocol::hooks::HookReducerPacket {
        packet28_daemon_protocol::hooks::HookReducerPacket {
            packet_type: "packet28.hook.fs.v2".to_string(),
            tool_name: "Read".to_string(),
            operation_kind: suite_packet_core::ToolOperationKind::Read,
            summary: "fresh task artifact".to_string(),
            cacheable: Some(false),
            mutation: Some(false),
            artifact: Some(json!({"source": "fresh-task-admission-regression"})),
            ..packet28_daemon_protocol::hooks::HookReducerPacket::default()
        }
    }

    #[test]
    fn fresh_hook_artifact_request_admits_registry_before_namespace_and_end_flush() {
        let state = crate::tests::support::daemon_test_state_with_persistence_debounce(
            Duration::from_secs(1),
        );
        let root = crate::tests::support::daemon_test_root(&state);
        let task_id = "fresh-hook-artifact";
        let (watch_tx, _watch_rx) = WatchIngress::new(1);

        let response = handle_request_and_flush(
            state.clone(),
            watch_tx,
            DaemonRequest::HookIngest {
                request: packet28_daemon_protocol::hooks::HookIngestRequest {
                    task_id: task_id.to_string(),
                    event_kind: packet28_daemon_protocol::hooks::HookEventKind::PostToolUse,
                    reducer_packet: Some(fresh_hook_artifact_packet()),
                    ..packet28_daemon_protocol::hooks::HookIngestRequest::default()
                },
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            DaemonResponse::HookIngest {
                response: packet28_daemon_protocol::hooks::HookIngestResponse {
                    accepted: true,
                    ..
                }
            }
        ));

        let registry = packet28_daemon_core::storage::load_task_registry(&root).unwrap();
        assert!(registry.tasks.contains_key(task_id));
        let storage_id = task_storage_id(task_id).unwrap();
        let hook_artifacts = task_artifact_dir(&root, &storage_id).join("hook-artifacts");
        assert_eq!(std::fs::read_dir(hook_artifacts).unwrap().count(), 1);
        crate::tests::support::shutdown_test_persistence(&state);
    }

    #[test]
    fn fresh_broker_artifact_request_admits_registry_before_namespace_and_end_flush() {
        let state = crate::tests::support::daemon_test_state_with_persistence_debounce(
            Duration::from_secs(1),
        );
        let root = crate::tests::support::daemon_test_root(&state);
        let task_id = "fresh-broker-artifact";
        let (watch_tx, _watch_rx) = WatchIngress::new(1);

        let response = handle_request_and_flush(
            state.clone(),
            watch_tx,
            DaemonRequest::BrokerGetContext {
                request: BrokerGetContextRequest {
                    task_id: task_id.to_string(),
                    action: Some(BrokerAction::Inspect),
                    max_sections: Some(1),
                    persist_artifacts: Some(true),
                    ..BrokerGetContextRequest::default()
                },
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            DaemonResponse::BrokerGetContext {
                response: BrokerGetContextResponse {
                    artifact_id: Some(_),
                    ..
                }
            }
        ));

        let registry = packet28_daemon_core::storage::load_task_registry(&root).unwrap();
        assert!(registry.tasks.contains_key(task_id));
        let storage_id = task_storage_id(task_id).unwrap();
        assert!(task_brief_markdown_path(&root, &storage_id).is_file());
        assert!(task_brief_json_path(&root, &storage_id).is_file());
        assert!(task_state_json_path(&root, &storage_id).is_file());
        crate::tests::support::shutdown_test_persistence(&state);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_frame_header_hits_its_owned_deadline() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0_u8; 4]).await.unwrap();

        let error = read_frame_bytes(&mut server, &deadline_config())
            .await
            .unwrap_err();
        assert!(matches!(error, FrameReadError::Deadline("header")));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_frame_body_hits_its_owned_deadline() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&2_u64.to_be_bytes()).await.unwrap();
        client.write_all(b"{").await.unwrap();

        let error = read_frame_bytes(&mut server, &deadline_config())
            .await
            .unwrap_err();
        assert!(matches!(error, FrameReadError::Deadline("body")));
    }

    #[tokio::test(start_paused = true)]
    async fn non_reading_peer_hits_frame_write_deadline() {
        let (_client, mut server) = tokio::io::duplex(32);
        let response = DaemonResponse::Error {
            message: "x".repeat(64 * 1_024),
        };
        let error = write_async_frame(
            &mut server,
            response,
            Duration::from_secs(1),
            &BlockingPool::new(1),
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("daemon frame write deadline exceeded"));
    }

    #[tokio::test(start_paused = true)]
    async fn handoff_wait_uses_owned_timer_deadline() {
        let state = crate::tests::support::daemon_test_state();
        let error = await_task_handoff(
            state.clone(),
            TaskAwaitHandoffRequest {
                task_id: "task-await-timeout".to_string(),
                timeout_ms: Some(100),
                poll_ms: Some(10),
                after_context_version: None,
            },
            BlockingPool::new(1),
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("timed out waiting for Packet28 handoff"));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_signal_cancels_handoff_wait() {
        let state = crate::tests::support::daemon_test_state();
        let wait_state = state.clone();
        let waiter = tokio::spawn(async move {
            await_task_handoff(
                wait_state,
                TaskAwaitHandoffRequest {
                    task_id: "task-await-cancel".to_string(),
                    timeout_ms: Some(60_000),
                    poll_ms: Some(10_000),
                    after_context_version: None,
                },
                BlockingPool::new(1),
            )
            .await
        });
        tokio::task::yield_now().await;
        state.lock().unwrap().shutdown.request();

        let error = waiter.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("daemon stopped while waiting"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_ack_uses_reserved_control_lane_before_publishing_shutdown() {
        let state = crate::tests::support::daemon_test_state();
        let pool = BlockingPool::new(1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let worker_pool = pool.clone();
        let data_worker = tokio::spawn(async move {
            worker_pool
                .run(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        assert_eq!(pool.available_permits(), 0);
        assert_eq!(
            pool.available_control_permits(),
            crate::runtime::CONTROL_BLOCKING_OPERATIONS
        );

        let (mut client, server_stream) = tokio::io::duplex(4 * 1_024);
        let (watch_tx, _watch_rx) = WatchIngress::new(1);
        let server_state = state.clone();
        let server_pool = pool.clone();
        let server = tokio::spawn(async move {
            handle_connection(
                server_state,
                watch_tx,
                server_stream,
                deadline_config(),
                server_pool,
            )
            .await
        });

        let response = tokio::time::timeout(
            CONTROL_LANE_DEADLOCK_TIMEOUT,
            exchange(&mut client, &DaemonRequest::Stop),
        )
        .await
        .expect("Stop deadlocked behind saturated data work");
        assert!(matches!(
            response,
            DaemonResponse::Ack { ref message } if message == "stopping"
        ));
        server.await.unwrap().unwrap();
        assert!(state.lock().unwrap().shutdown.is_requested());
        assert!(pool.is_shutting_down());

        release_tx.send(()).unwrap();
        data_worker.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_cancel_uses_reserved_control_lane_when_data_is_saturated() {
        let state = crate::tests::support::daemon_test_state();
        crate::tests::support::insert_admitted_task_record(
            &state,
            TaskRecord {
                task_id: "task-control-cancel".to_string(),
                ..TaskRecord::default()
            },
        );
        flush_persistence(&state).unwrap();
        {
            let mut guard = state.lock().unwrap();
            guard
                .task_generations
                .create("task-control-cancel")
                .unwrap();
        }
        let pool = BlockingPool::new(1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let worker_pool = pool.clone();
        let data_worker = tokio::spawn(async move {
            worker_pool
                .run(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();

        let (mut client, server_stream) = tokio::io::duplex(4 * 1_024);
        let (watch_tx, _watch_rx) = WatchIngress::new(1);
        let server_state = state.clone();
        let server_pool = pool.clone();
        let server = tokio::spawn(async move {
            handle_connection(
                server_state,
                watch_tx,
                server_stream,
                deadline_config(),
                server_pool,
            )
            .await
        });
        let response = tokio::time::timeout(
            CONTROL_LANE_DEADLOCK_TIMEOUT,
            exchange(
                &mut client,
                &DaemonRequest::TaskCancel {
                    task_id: "task-control-cancel".to_string(),
                },
            ),
        )
        .await
        .expect("TaskCancel deadlocked behind saturated data work");
        assert!(matches!(
            response,
            DaemonResponse::TaskCancel { task: Some(_), .. }
        ));

        state.lock().unwrap().shutdown.request();
        server.await.unwrap().unwrap();
        release_tx.send(()).unwrap();
        data_worker.await.unwrap().unwrap();
    }

    #[test]
    fn context_store_handlers_observe_and_prune_the_live_kernel_owner() {
        let state = crate::tests::support::daemon_test_state();
        let root = state.lock().unwrap().root.to_string_lossy().to_string();
        let (watch_tx, _watch_rx) = WatchIngress::new(1);
        let execute = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::Execute {
                request: KernelRequest {
                    target: "contextq.assemble".to_string(),
                    input_packets: vec![context_kernel_core::KernelPacket::from_value(
                        json!({
                            "packet_id": "live-context",
                            "tool": "packet28d",
                            "reducer": "context",
                            "sections": [{
                                "title": "Live context",
                                "body": "daemon immediate visibility marker",
                                "refs": [],
                                "relevance": 1.0
                            }]
                        }),
                        None,
                    )],
                    ..KernelRequest::default()
                },
            },
        )
        .unwrap();
        assert!(matches!(execute, DaemonResponse::Execute { .. }));

        let listed = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::ContextStoreList {
                request: ContextStoreListRequest {
                    root: root.clone(),
                    limit: 20,
                    ..ContextStoreListRequest::default()
                },
            },
        )
        .unwrap();
        let entries = match listed {
            DaemonResponse::ContextStoreList { response } => response.entries,
            response => panic!("unexpected list response: {response:?}"),
        };
        assert_eq!(entries.len(), 1);

        let fetched = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::ContextStoreGet {
                request: ContextStoreGetRequest {
                    root: root.clone(),
                    key: entries[0].cache_key.clone(),
                },
            },
        )
        .unwrap();
        assert!(matches!(
            fetched,
            DaemonResponse::ContextStoreGet {
                response: ContextStoreGetResponse { entry: Some(_) }
            }
        ));

        let recalled = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::ContextRecall {
                request: ContextRecallRequest {
                    query: "immediate visibility".to_string(),
                    root: root.clone(),
                    limit: 10,
                    since: Some(0),
                    ..ContextRecallRequest::default()
                },
            },
        )
        .unwrap();
        assert!(matches!(
            recalled,
            DaemonResponse::ContextRecall {
                response: ContextRecallResponse { ref hits, .. }
            } if hits.len() == 1
        ));

        let legacy_default_root_stats = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::ContextStoreStats {
                request: ContextStoreStatsRequest {
                    root: String::new(),
                },
            },
        )
        .unwrap();
        assert!(matches!(
            legacy_default_root_stats,
            DaemonResponse::ContextStoreStats {
                response: ContextStoreStatsResponse {
                    stats: context_memory_core::ContextStoreStats { entries: 1, .. }
                }
            }
        ));

        let pruned = handle_request(
            state.clone(),
            watch_tx.clone(),
            DaemonRequest::ContextStorePrune {
                request: ContextStorePruneDaemonRequest {
                    root: root.clone(),
                    all: true,
                    ttl_secs: None,
                },
            },
        )
        .unwrap();
        assert!(matches!(
            pruned,
            DaemonResponse::ContextStorePrune {
                response: ContextStorePruneResponse {
                    report: context_memory_core::ContextStorePruneReport {
                        removed: 1,
                        remaining: 0,
                        ..
                    }
                }
            }
        ));

        let stats = handle_request(
            state.clone(),
            watch_tx,
            DaemonRequest::ContextStoreStats {
                request: ContextStoreStatsRequest { root },
            },
        )
        .unwrap();
        assert!(matches!(
            stats,
            DaemonResponse::ContextStoreStats {
                response: ContextStoreStatsResponse {
                    stats: context_memory_core::ContextStoreStats { entries: 0, .. }
                }
            }
        ));
    }
}
