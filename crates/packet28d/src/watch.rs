use super::*;

pub(crate) fn register_task_and_watches(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    spec: TaskSubmitSpec,
) -> Result<(TaskRecord, Vec<WatchRegistration>)> {
    let root = {
        let guard = state.lock().map_err(lock_err)?;
        guard.root.clone()
    };
    let spec = normalize_task_submit_spec(&root, spec)?;

    let removed_watch_ids = {
        let guard = state.lock().map_err(lock_err)?;
        guard
            .tasks
            .tasks
            .get(&spec.task_id)
            .map(|task| task.watch_ids.clone())
            .unwrap_or_default()
    };
    for watch_id in removed_watch_ids {
        let _ = remove_watch(state.clone(), &watch_id)?;
    }

    let mut registrations = Vec::new();
    {
        let mut guard = state.lock().map_err(lock_err)?;
        let watch_ids = spec
            .watches
            .iter()
            .map(|watch: &WatchSpec| {
                let mut watch = watch.clone();
                watch.task_id = spec.task_id.clone();
                if watch.root.trim().is_empty() {
                    watch.root = guard.root.to_string_lossy().to_string();
                }
                let registration = WatchRegistration {
                    watch_id: watch_id_for(&watch),
                    spec: watch,
                    active: true,
                    last_event_at_unix: None,
                    last_error: None,
                };
                guard.watches.watches.push(registration.clone());
                registrations.push(registration.clone());
                registration.watch_id
            })
            .collect::<Vec<_>>();
        let task = TaskRecord {
            task_id: spec.task_id.clone(),
            watch_ids,
            sequence_present: true,
            sequence: Some(spec.sequence.clone()),
            ..TaskRecord::default()
        };
        guard.tasks.tasks.insert(spec.task_id.clone(), task.clone());
    }

    let mut installed_watch_ids: Vec<String> = Vec::new();
    for registration in &registrations {
        if let Err(err) = install_watch(
            state.clone(),
            watch_tx.clone(),
            registration.watch_id.clone(),
        ) {
            let _ = remove_watch(state.clone(), &registration.watch_id);
            for watch_id in &installed_watch_ids {
                let _ = remove_watch(state.clone(), watch_id);
            }
            let mut guard = state.lock().map_err(lock_err)?;
            guard.tasks.tasks.remove(&spec.task_id);
            guard.watches.watches.retain(|watch| {
                !registrations
                    .iter()
                    .any(|candidate| candidate.watch_id == watch.watch_id)
            });
            persist_state(&guard)?;
            return Err(err);
        }
        installed_watch_ids.push(registration.watch_id.clone());
    }

    {
        let guard = state.lock().map_err(lock_err)?;
        persist_state(&guard)?;
    }

    let task = state
        .lock()
        .map_err(lock_err)?
        .tasks
        .tasks
        .get(&spec.task_id)
        .cloned()
        .ok_or_else(|| anyhow!("task disappeared after registration"))?;
    Ok((task, registrations))
}

pub(crate) fn run_sequence_for_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<context_kernel_core::KernelSequenceResponse> {
    loop {
        let (kernel, sequence) = {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            let sequence = task
                .sequence
                .clone()
                .ok_or_else(|| anyhow!("task '{}' has no stored sequence", task_id))?;
            task.running = true;
            task.pending_replan = false;
            task.last_started_at_unix = Some(now_unix());
            task.last_error = None;
            persist_state(&guard)?;
            (guard.kernel.clone(), sequence)
        };
        let _ = emit_task_event(
            state.clone(),
            task_id,
            "task_started",
            json!({"task_id": task_id, "step_count": sequence.steps.len()}),
        );

        let mut observer = TaskSequenceObserver {
            state: state.clone(),
            task_id: task_id.to_string(),
        };
        let result = kernel.execute_sequence_with_observer(sequence, &mut observer);

        let rerun = {
            let mut guard = state.lock().map_err(lock_err)?;
            let task = guard
                .tasks
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow!("unknown task '{task_id}'"))?;
            task.running = false;
            task.last_completed_at_unix = Some(now_unix());
            match &result {
                Ok(response) => {
                    task.last_request_id = Some(response.request_id);
                    task.last_sequence_metadata = Some(response.metadata.clone());
                    task.last_error = None;
                }
                Err(err) => {
                    task.last_error = Some(err.to_string());
                    daemon_log(&format!("task run failed task_id={} error={err}", task_id));
                }
            }
            let rerun = task.pending_replan && !task.cancel_requested;
            if rerun {
                task.last_replan_at_unix = Some(now_unix());
            }
            persist_state(&guard)?;
            rerun
        };

        if let Ok(_response) = &result {
            let mut summary =
                refresh_task_context_summary(state.clone(), task_id)?.unwrap_or_else(|| json!({}));
            let _ = set_context_reason(&state, task_id, "replan_applied");
            if let Some(response) = refresh_broker_context_for_task(&state, task_id, None)? {
                if let Some(object) = summary.as_object_mut() {
                    object.insert(
                        "changed_section_ids".to_string(),
                        Value::Array(
                            response
                                .delta
                                .changed_sections
                                .iter()
                                .map(|section| Value::String(section.id.clone()))
                                .collect(),
                        ),
                    );
                    object.insert(
                        "removed_section_ids".to_string(),
                        Value::Array(
                            response
                                .delta
                                .removed_section_ids
                                .iter()
                                .map(|id| Value::String(id.clone()))
                                .collect(),
                        ),
                    );
                    object.insert(
                        "reason".to_string(),
                        Value::String("replan_applied".to_string()),
                    );
                    object.insert(
                        "context_version".to_string(),
                        Value::String(response.context_version.clone()),
                    );
                    object.insert(
                        "brief_path".to_string(),
                        Value::String(
                            task_brief_markdown_path(
                                &state.lock().map_err(lock_err)?.root.clone(),
                                task_id,
                            )
                            .to_string_lossy()
                            .to_string(),
                        ),
                    );
                }
            }
            let _ = emit_task_event(state.clone(), task_id, "context_updated", summary);
        }

        match result {
            Ok(_) if rerun => {
                continue;
            }
            Ok(response) => {
                let _ = emit_task_event(
                    state.clone(),
                    task_id,
                    "task_completed",
                    json!({"task_id": task_id, "request_id": response.request_id}),
                );
                return Ok(response);
            }
            Err(err) => {
                let _ = emit_task_event(
                    state.clone(),
                    task_id,
                    "task_failed",
                    json!({"task_id": task_id, "error": err.to_string()}),
                );
                return Err(err.into());
            }
        }
    }
}

pub(crate) fn cancel_task(
    state: Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<(Option<TaskRecord>, Vec<String>)> {
    let watch_ids = {
        let mut guard = state.lock().map_err(lock_err)?;
        let Some(task) = guard.tasks.tasks.get_mut(task_id) else {
            return Ok((None, Vec::new()));
        };
        task.cancel_requested = true;
        task.watch_ids.clone()
    };
    for watch_id in &watch_ids {
        let _ = remove_watch(state.clone(), watch_id)?;
    }
    let mut guard = state.lock().map_err(lock_err)?;
    let removed = guard.tasks.tasks.remove(task_id);
    persist_state(&guard)?;
    Ok((removed, watch_ids))
}

pub(crate) fn remove_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_id: &str,
) -> Result<Option<WatchRegistration>> {
    let mut guard = state.lock().map_err(lock_err)?;
    guard.watcher_handles.remove(watch_id);
    let removed = if let Some(index) = guard
        .watches
        .watches
        .iter()
        .position(|watch| watch.watch_id == watch_id)
    {
        Some(guard.watches.watches.remove(index))
    } else {
        None
    };
    for task in guard.tasks.tasks.values_mut() {
        task.watch_ids.retain(|candidate| candidate != watch_id);
    }
    persist_state(&guard)?;
    Ok(removed)
}

pub(crate) fn restore_watchers(
    state: &Arc<Mutex<DaemonState>>,
    watch_tx: &Sender<WatchEventMsg>,
) -> Result<()> {
    let watch_ids = state
        .lock()
        .map_err(lock_err)?
        .watches
        .watches
        .iter()
        .map(|watch| watch.watch_id.clone())
        .collect::<Vec<_>>();
    for watch_id in watch_ids {
        if let Err(err) = install_watch(state.clone(), watch_tx.clone(), watch_id.clone()) {
            daemon_log(&format!("failed to restore watch {watch_id}: {err}"));
        }
    }
    Ok(())
}

pub(crate) fn install_watch(
    state: Arc<Mutex<DaemonState>>,
    watch_tx: Sender<WatchEventMsg>,
    watch_id: String,
) -> Result<()> {
    let spec = {
        let guard = state.lock().map_err(lock_err)?;
        guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == watch_id)
            .map(|watch| watch.spec.clone())
            .ok_or_else(|| anyhow!("unknown watch '{watch_id}'"))?
    };

    let callback_watch_id = watch_id.clone();
    let mut watcher = PollWatcher::new(
        move |result: notify::Result<Event>| match result {
            Ok(event) => {
                let _ = watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    paths: event.paths,
                    error: None,
                });
            }
            Err(err) => {
                let _ = watch_tx.send(WatchEventMsg {
                    watch_id: callback_watch_id.clone(),
                    paths: Vec::new(),
                    error: Some(err.to_string()),
                });
            }
        },
        Config::default()
            .with_poll_interval(Duration::from_millis(spec.debounce_ms.unwrap_or(250))),
    )?;

    let paths = watch_paths(&spec);
    for path in &paths {
        let mode = if matches!(spec.kind, WatchKind::Git | WatchKind::File) {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(path, mode)?;
    }

    let mut guard = state.lock().map_err(lock_err)?;
    if let Some(watch) = guard
        .watches
        .watches
        .iter_mut()
        .find(|watch| watch.watch_id == watch_id)
    {
        watch.active = true;
        watch.last_error = None;
    }
    guard.watcher_handles.insert(watch_id.clone(), watcher);
    persist_state(&guard)?;
    daemon_log(&format!(
        "installed watch watch_id={watch_id} task_id={} kind={:?}",
        spec.task_id, spec.kind
    ));
    Ok(())
}

pub(crate) fn spawn_watch_processor(
    state: Arc<Mutex<DaemonState>>,
    watch_rx: Receiver<WatchEventMsg>,
) {
    thread::spawn(move || {
        let mut pending = HashMap::<String, PendingWatchEvent>::new();
        loop {
            flush_due_watch_events(state.clone(), &mut pending);
            let timeout = next_watch_timeout(&pending).unwrap_or(Duration::from_secs(60));
            match watch_rx.recv_timeout(timeout) {
                Ok(message) => {
                    if state
                        .lock()
                        .map_err(lock_err)
                        .map(|guard| guard.shutting_down)
                        .unwrap_or(false)
                    {
                        break;
                    }
                    merge_watch_event(state.clone(), &mut pending, message);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if state
                        .lock()
                        .map_err(lock_err)
                        .map(|guard| guard.shutting_down)
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

fn process_watch_event(state: Arc<Mutex<DaemonState>>, message: WatchEventMsg) -> Result<()> {
    let (task_id, error_message) = {
        let guard = state.lock().map_err(lock_err)?;
        let Some(registration) = guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == message.watch_id)
        else {
            return Ok(());
        };
        (registration.spec.task_id.clone(), message.error.clone())
    };

    {
        let mut guard = state.lock().map_err(lock_err)?;
        if let Some(watch) = guard
            .watches
            .watches
            .iter_mut()
            .find(|watch| watch.watch_id == message.watch_id)
        {
            watch.last_event_at_unix = Some(now_unix());
            watch.last_error = error_message.clone();
        }
        persist_state(&guard)?;
    }

    if let Some(error) = error_message {
        let _ = emit_task_event(
            state.clone(),
            &task_id,
            "watch_error",
            json!({
                "watch_id": message.watch_id,
                "error": error,
            }),
        );
        return Ok(());
    }

    let _ = emit_task_event(
        state.clone(),
        &task_id,
        "watch_triggered",
        json!({
            "watch_id": message.watch_id,
            "paths": message
                .paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
        }),
    );

    let _ = set_context_reason(&state, &task_id, "watch_triggered");
    let _ = refresh_task_context_summary(state.clone(), &task_id)?;
    let _ = refresh_broker_context_for_task(&state, &task_id, None)?;

    {
        let mut guard = state.lock().map_err(lock_err)?;
        if let Some(task) = guard.tasks.tasks.get_mut(&task_id) {
            task.pending_replan = true;
        }
        persist_state(&guard)?;
    }

    let _ = run_sequence_for_task(state, &task_id);
    Ok(())
}

pub(crate) fn watch_paths(spec: &WatchSpec) -> Vec<PathBuf> {
    match spec.kind {
        WatchKind::Git => vec![PathBuf::from(&spec.root).join(".git")],
        WatchKind::File => spec
            .paths
            .iter()
            .map(|path| PathBuf::from(&spec.root).join(path))
            .collect(),
        WatchKind::TestReport => spec
            .paths
            .iter()
            .map(|path| PathBuf::from(&spec.root).join(path))
            .collect(),
    }
}

pub(crate) fn watch_id_for(spec: &WatchSpec) -> String {
    let digest = blake3::hash(
        serde_json::to_string(spec)
            .unwrap_or_else(|_| format!("{:?}", spec))
            .as_bytes(),
    );
    format!("watch-{}", &digest.to_hex()[..16])
}

fn normalize_task_submit_spec(root: &Path, mut spec: TaskSubmitSpec) -> Result<TaskSubmitSpec> {
    if spec.task_id.trim().is_empty() {
        anyhow::bail!("task_id cannot be empty");
    }
    spec.sequence.reactive.enabled = true;
    spec.sequence.reactive.task_id = Some(spec.task_id.clone());
    if spec.sequence.steps.is_empty() {
        anyhow::bail!("sequence must contain at least one step");
    }
    spec.sequence = normalize_sequence_request(spec.sequence).map_err(|source| anyhow!(source))?;

    for watch in &mut spec.watches {
        watch.task_id = spec.task_id.clone();
        if watch.root.trim().is_empty() {
            watch.root = root.to_string_lossy().to_string();
        }
        let watch_root = resolve_root(Path::new(&watch.root));
        if !watch_root.exists() {
            anyhow::bail!("watch root '{}' does not exist", watch_root.display());
        }
        for path in watch_paths(watch) {
            if !path.exists() {
                anyhow::bail!("watch path '{}' does not exist", path.display());
            }
        }
    }

    Ok(spec)
}

fn merge_watch_event(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<String, PendingWatchEvent>,
    message: WatchEventMsg,
) {
    let debounce_ms = watch_debounce_ms(&state, &message.watch_id).unwrap_or(250);
    let due_at = Instant::now() + Duration::from_millis(debounce_ms);
    let entry = pending
        .entry(message.watch_id.clone())
        .or_insert_with(|| PendingWatchEvent {
            watch_id: message.watch_id.clone(),
            paths: Vec::new(),
            error: None,
            due_at,
        });
    entry.due_at = due_at;
    if entry.error.is_none() {
        entry.error = message.error.clone();
    }
    for path in message.paths {
        if !entry.paths.iter().any(|existing| existing == &path) {
            entry.paths.push(path);
        }
    }
}

fn flush_due_watch_events(
    state: Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<String, PendingWatchEvent>,
) {
    let now = Instant::now();
    let ready_ids = pending
        .iter()
        .filter(|(_, item)| item.due_at <= now)
        .map(|(watch_id, _)| watch_id.clone())
        .collect::<Vec<_>>();
    for watch_id in ready_ids {
        if let Some(item) = pending.remove(&watch_id) {
            let message = WatchEventMsg {
                watch_id: item.watch_id,
                paths: item.paths,
                error: item.error,
            };
            if let Err(err) = process_watch_event(state.clone(), message) {
                daemon_log(&format!("watch event processing failed: {err}"));
            }
        }
    }
}

fn next_watch_timeout(pending: &HashMap<String, PendingWatchEvent>) -> Option<Duration> {
    let now = Instant::now();
    pending
        .values()
        .map(|item| item.due_at.saturating_duration_since(now))
        .min()
}

fn watch_debounce_ms(state: &Arc<Mutex<DaemonState>>, watch_id: &str) -> Option<u64> {
    state.lock().ok().and_then(|guard| {
        guard
            .watches
            .watches
            .iter()
            .find(|watch| watch.watch_id == watch_id)
            .and_then(|watch| watch.spec.debounce_ms)
    })
}
