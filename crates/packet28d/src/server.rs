use super::*;
use packet28_daemon_core::TaskMarkHandoffConsumedResponse;

pub(crate) fn handle_connection(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    stream: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    loop {
        let request = match read_socket_message(&mut reader) {
            Ok(value) => value,
            Err(err) if is_benign_disconnect_error(&err) => return Ok(()),
            Err(err) => {
                let response = DaemonResponse::Error {
                    message: err.to_string(),
                };
                write_socket_response(&mut writer, &response)?;
                return Ok(());
            }
        };
        if let DaemonRequest::TaskSubscribe {
            task_id,
            replay_last,
        } = request
        {
            return handle_task_subscribe(state, &mut writer, task_id, replay_last);
        }
        let response = match handle_request(state.clone(), watch_tx.clone(), request) {
            Ok(value) => value,
            Err(err) => {
                daemon_log(&format!("daemon request failed: {err}"));
                DaemonResponse::Error {
                    message: err.to_string(),
                }
            }
        };
        write_socket_response(&mut writer, &response)?;
    }
}

fn handle_task_subscribe(
    state: Arc<Mutex<DaemonState>>,
    writer: &mut BufWriter<UnixStream>,
    task_id: String,
    replay_last: usize,
) -> Result<()> {
    let root = state.lock().map_err(lock_err)?.root.clone();
    let replay = load_task_events(&root, &task_id)?;
    let replay = if replay_last == 0 || replay_last >= replay.len() {
        replay
    } else {
        replay[replay.len().saturating_sub(replay_last)..].to_vec()
    };
    write_socket_response(
        writer,
        &DaemonResponse::TaskSubscribeAck {
            task_id: task_id.clone(),
            replayed: replay.len(),
        },
    )?;
    for frame in replay {
        match write_socket_message(writer, &frame) {
            Ok(()) => {}
            Err(err) if is_benign_disconnect_error(&err) => return Ok(()),
            Err(err) => return Err(err),
        }
    }

    let (tx, rx) = mpsc::channel();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        guard
            .subscribers
            .entry(task_id.clone())
            .or_default()
            .push(tx);
    }

    while let Ok(frame) = rx.recv() {
        match write_socket_message(writer, &frame) {
            Ok(()) => {}
            Err(err) if is_benign_disconnect_error(&err) => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn write_socket_response(
    writer: &mut BufWriter<UnixStream>,
    response: &DaemonResponse,
) -> Result<()> {
    match write_socket_message(writer, response) {
        Ok(()) => Ok(()),
        Err(err) if is_benign_disconnect_error(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_benign_disconnect_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| {
                matches!(
                    io_err.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
                )
            })
    })
}

fn handle_request(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    request: DaemonRequest,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Execute { request } => {
            let kernel = kernel_for_request(&state, &request)?;
            let response = kernel.execute(request)?;
            Ok(DaemonResponse::Execute { response })
        }
        DaemonRequest::ExecuteSequence { spec } => {
            let (task, watches) = register_task_and_watches(state.clone(), watch_tx, spec)?;
            let response = match run_sequence_for_task(state.clone(), &task.task_id) {
                Ok(response) => response,
                Err(err) => {
                    daemon_log(&format!(
                        "initial task run failed task_id={} error={err}",
                        task.task_id
                    ));
                    let _ = cancel_task(state.clone(), &task.task_id);
                    return Err(err);
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
                let _ = cancel_task(state.clone(), &task.task_id);
                return Err(anyhow!(message));
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
        DaemonRequest::Stop => {
            let root = {
                let mut guard = state.lock().map_err(lock_err)?;
                guard.shutting_down = true;
                let _ = guard.index_tx.send(IndexCommand::Shutdown);
                guard.root.clone()
            };
            wake_listener(&root);
            Ok(DaemonResponse::Ack {
                message: "stopping".to_string(),
            })
        }
        DaemonRequest::TaskStatus { task_id } => {
            let task = state
                .lock()
                .map_err(lock_err)?
                .tasks
                .tasks
                .get(&task_id)
                .cloned();
            Ok(DaemonResponse::TaskStatus { task })
        }
        DaemonRequest::TaskAwaitHandoff { request } => {
            let response = task_await_handoff(state, request)?;
            Ok(DaemonResponse::TaskAwaitHandoff { response })
        }
        DaemonRequest::TaskMarkHandoffConsumed { request } => {
            let response = TaskMarkHandoffConsumedResponse {
                handoff: crate::broker_handoff::mark_handoff_consumed(
                    &state,
                    &request.task_id,
                    &request.handoff_id,
                )?,
            };
            Ok(DaemonResponse::TaskMarkHandoffConsumed { response })
        }
        DaemonRequest::TaskLaunchAgent { request } => {
            let response = task_launch_agent(state, request)?;
            Ok(DaemonResponse::TaskLaunchAgent { response })
        }
        DaemonRequest::TaskCancel { task_id } => {
            let removed = cancel_task(state.clone(), &task_id)?;
            Ok(DaemonResponse::TaskCancel {
                task: removed.0,
                removed_watch_ids: removed.1,
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
            Ok(DaemonResponse::WatchList { watches })
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
            let response = run_context_store_list(request)?;
            Ok(DaemonResponse::ContextStoreList { response })
        }
        DaemonRequest::ContextStoreGet { request } => {
            let response = run_context_store_get(request)?;
            Ok(DaemonResponse::ContextStoreGet { response })
        }
        DaemonRequest::ContextStorePrune { request } => {
            let response = run_context_store_prune(request)?;
            Ok(DaemonResponse::ContextStorePrune { response })
        }
        DaemonRequest::ContextStoreStats { request } => {
            let response = run_context_store_stats(request)?;
            Ok(DaemonResponse::ContextStoreStats { response })
        }
        DaemonRequest::ContextRecall { request } => {
            let response = run_context_recall(request)?;
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
        DaemonRequest::HookIngest { request } => {
            let response = hook_ingest(state, request)?;
            Ok(DaemonResponse::HookIngest { response })
        }
        DaemonRequest::Packet28Search { request } => {
            let response = daemon_packet28_search(state, request)?;
            Ok(DaemonResponse::Packet28Search { response })
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
