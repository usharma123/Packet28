use super::*;

/// Former bounded suffix window retained for public API compatibility.
///
/// Strict event-log integrity validation now streams from byte zero. This
/// value remains useful to construct regression fixtures proving that
/// corruption outside the former suffix cannot be accepted.
pub const MAX_TASK_EVENT_TAIL_SCAN_BYTES: usize = 3 * (MAX_TASK_EVENT_LINE_BYTES + 1);

/// Loads one committed task/watch checkpoint and authenticated event tails
/// while holding the task registry as the cross-registry lock.
///
/// Legacy task and watch documents are accepted only when both omit the
/// checkpoint generation. Once either document carries a generation, both
/// must carry the same value. New checkpoints additionally use a hash-bound
/// commit manifest and recovery journal. If a process stops between canonical
/// registry publications, the loader selects the prior manifest-authorized
/// pair; bytes outside the recorded publication phases fail closed.
///
/// # Errors
///
/// Returns [`DaemonCoreError::RegistryCheckpointGenerationMismatch`] when the
/// task and watch documents are from different durable generations. Returns
/// the same registry, event-tail, validation, and filesystem errors as
/// [`load_task_registry_with_event_tails`] and [`load_watch_registry`].
pub fn load_task_watch_registry_checkpoint_with_event_tails(
    root: &Path,
) -> Result<(TaskRegistry, WatchRegistry, BTreeMap<String, Option<u64>>)> {
    #[cfg(unix)]
    {
        let writer_lease = acquire_task_store_writer_lease(root)?;
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Shared,
            || Ok(()),
            |daemon| {
                let (registry, watches, _, _) =
                    load_task_watch_registry_checkpoint_under_task_lock(root, daemon)?;
                let mut tails = BTreeMap::new();
                for task_id in registry.tasks.keys() {
                    let storage_id = checked_task_storage_id(root, task_id)?;
                    let tail =
                        task_event_log_tail_sequence_admitted(root, &storage_id, &writer_lease)?;
                    tails.insert(task_id.clone(), tail);
                }
                Ok((registry, watches, tails))
            },
        )
    }
    #[cfg(not(unix))]
    {
        let registry_path = task_registry_path(root);
        let writer_lease = acquire_task_store_writer_lease(root)?;
        with_registry_lock(root, &registry_path, RegistryLockMode::Shared, || {
            let (registry, watches, _, _) =
                load_task_watch_registry_checkpoint_portable_under_task_lock(root)?;
            let mut tails = BTreeMap::new();
            for task_id in registry.tasks.keys() {
                let storage_id = checked_task_storage_id(root, task_id)?;
                let tail = task_event_log_tail_sequence_portable(root, &storage_id)?;
                tails.insert(task_id.clone(), tail);
            }
            let _ = writer_lease;
            Ok((registry, watches, tails))
        })
    }
}

/// Loads the strict durable task registry and every authenticated event tail
/// under one shared registry authority lock.
///
/// This is the daemon-startup reconciliation primitive. On Unix targets, the
/// registry is decoded once, then each admitted task's complete event log is
/// streamed while the same registry binding remains locked. That avoids
/// decoding an O(tasks)-sized registry once per task and prevents a writer
/// from changing admission between the registry snapshot and its tail
/// observations. The portable fallback preserves the same validation
/// contract through the existing per-task tail API.
///
/// # Errors
///
/// Returns the same typed registry, identifier, event-tail, authority-limit,
/// and filesystem errors as [`load_task_registry`] and
/// [`task_event_log_tail_sequence`].
pub fn load_task_registry_with_event_tails(
    root: &Path,
) -> Result<(TaskRegistry, BTreeMap<String, Option<u64>>)> {
    #[cfg(unix)]
    {
        let registry_path = task_registry_path(root);
        let writer_lease = acquire_task_store_writer_lease(root)?;
        with_anchored_task_registry_lock(
            root,
            RegistryLockMode::Shared,
            || Ok(()),
            |daemon| {
                let registry = match daemon
                    .read_file_limited(OsStr::new(TASK_REGISTRY_FILE_NAME), MAX_TASK_REGISTRY_BYTES)
                {
                    Ok(raw) => decode_task_registry(&registry_path, &raw)?,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                        TaskRegistry::default()
                    }
                    Err(source) => {
                        return Err(task_registry_read_error(daemon, &registry_path, source));
                    }
                };
                let mut tails = BTreeMap::new();
                for task_id in registry.tasks.keys() {
                    let storage_id = checked_task_storage_id(root, task_id)?;
                    let tail =
                        task_event_log_tail_sequence_admitted(root, &storage_id, &writer_lease)?;
                    tails.insert(task_id.clone(), tail);
                }
                Ok((registry, tails))
            },
        )
    }
    #[cfg(not(unix))]
    {
        let registry = load_task_registry(root)?;
        let mut tails = BTreeMap::new();
        for task_id in registry.tasks.keys() {
            tails.insert(
                task_id.clone(),
                task_event_log_tail_sequence(root, task_id)?,
            );
        }
        Ok((registry, tails))
    }
}

/// Returns the authenticated sequence of the last complete durable event.
///
/// The complete log is streamed with bounded per-line memory. A malformed
/// complete frame, cross-task frame, zero sequence, duplicate, or gap is
/// rejected instead of skipped. A trailing non-newline suffix is ignored by
/// this read-only reconciliation operation and will be truncated by
/// [`append_next_task_event`] while holding the same exclusive event lock.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] when the task is not
/// durably admitted. Returns [`DaemonCoreError::InvalidTaskEventFrame`] for an
/// invalid authoritative tail, [`DaemonCoreError::AuthorityJsonLimitExceeded`]
/// for a structurally excessive frame, or [`DaemonCoreError::Io`] for bounded
/// streaming read, lock, descriptor, or namespace failures.
pub fn task_event_log_tail_sequence(root: &Path, task_id: &str) -> Result<Option<u64>> {
    let task_id = checked_task_storage_id(root, task_id)?;
    let writer_lease = acquire_task_store_writer_lease(root)?;
    with_registered_task_storage_id(root, &task_id, || {
        #[cfg(unix)]
        {
            task_event_log_tail_sequence_admitted(root, &task_id, &writer_lease)
        }
        #[cfg(not(unix))]
        {
            let _ = writer_lease;
            task_event_log_tail_sequence_portable(root, &task_id)
        }
    })
}

/// Allocates and appends the next durable task-event sequence under one
/// exclusive event-file lock.
///
/// The event log is the sequence owner. Every complete frame is streamed and
/// validated, `tail + 1` is assigned with overflow checking, and the returned
/// frame is synchronized before the lock is released. A bounded trailing
/// crash-partial suffix is truncated and synchronized under that same lock
/// before the new complete frame is appended.
///
/// # Errors
///
/// Returns [`DaemonCoreError::InvalidTaskRegistry`] when the task is not
/// admitted, [`DaemonCoreError::TaskSuperseded`] when its history was moved to
/// a linked successor, [`DaemonCoreError::InvalidTaskEventFrame`] for malformed,
/// conflicting, cross-task, or exhausted sequence authority,
/// [`DaemonCoreError::AuthorityJsonLimitExceeded`] for an excessive event, or
/// [`DaemonCoreError::StorageMutationAuthorityLost`] when synchronized bytes
/// cannot be reauthenticated before return.
pub fn append_next_task_event(
    root: &Path,
    task_id: &str,
    event: &DaemonEvent,
) -> Result<DaemonEventFrame> {
    let task_id = checked_task_storage_id(root, task_id)?;
    preflight_task_event(root, &task_id, event)?;

    let writer_lease = acquire_task_store_writer_lease(root)?;
    let _registry_admission = acquire_registry_writer_admission(&writer_lease)?;
    registry_delta::with_continuable_task_admission(root, &task_id, &writer_lease, || {
        #[cfg(unix)]
        {
            append_next_task_event_admitted(root, &task_id, &writer_lease, event)
        }
        #[cfg(not(unix))]
        {
            let _ = writer_lease;
            append_next_task_event_portable(root, &task_id, event)
        }
    })
}

/// Appends an event using authenticated in-memory registry authority.
///
/// This avoids rereading and decoding the complete task-registry checkpoint
/// for every daemon event. `authority` can only be created by loading durable
/// checkpoint-plus-WAL state and advances only through authenticated WAL
/// appends.
///
/// # Errors
///
/// Returns [`DaemonCoreError::Io`] when the authority or lease does not own
/// `root`. Other errors match [`append_next_task_event`].
pub fn append_next_task_event_with_authority(
    root: &Path,
    authority: &RegistryAdmissionAuthority,
    task_id: &str,
    event: &DaemonEvent,
) -> Result<DaemonEventFrame> {
    require_daemon_lifecycle_lease(root, authority.lease())?;
    let task_id = checked_task_storage_id(root, task_id)?;
    authority.require_continuable_task(root, &task_id)?;
    preflight_task_event(root, &task_id, event)?;
    #[cfg(unix)]
    {
        append_next_task_event_admitted(root, &task_id, authority.lease(), event)
    }
    #[cfg(not(unix))]
    {
        append_next_task_event_portable(root, &task_id, event)
    }
}

fn preflight_task_event(root: &Path, task_id: &TaskStorageId, event: &DaemonEvent) -> Result<()> {
    // Use the widest possible decimal sequence during preflight so successful
    // admission guarantees that the final encoded line fits the same bound.
    let preflight = DaemonEventFrame {
        seq: u64::MAX,
        task_id: task_id.as_str().to_string(),
        event: event.clone(),
    };
    let _ = encode_task_event_frame(root, task_id, &preflight)?;
    Ok(())
}

#[derive(Debug)]
pub(super) struct TaskEventTailInspection {
    pub(super) tail: Option<DaemonEventFrame>,
    pub(super) complete_len: u64,
    pub(super) has_partial_suffix: bool,
}

#[cfg(unix)]
pub(super) fn task_event_log_tail_sequence_admitted(
    root: &Path,
    task_id: &TaskStorageId,
    lease: &crate::task_store_lease::TaskStoreLease,
) -> Result<Option<u64>> {
    task_event_log_tail_sequence_admitted_with_observer(root, task_id, lease, || Ok(()))
}

#[cfg(unix)]
pub(super) fn task_event_log_tail_sequence_admitted_with_observer(
    root: &Path,
    task_id: &TaskStorageId,
    lease: &crate::task_store_lease::TaskStoreLease,
    after_read: impl FnOnce() -> Result<()>,
) -> Result<Option<u64>> {
    let path = task_event_log_path(root, task_id);
    let Some(events) = open_task_events_capability_for_read(root)? else {
        return Ok(None);
    };
    let file_name = event_log_file_name(task_id);
    validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
    let file = match events.open_read_file(OsStr::new(&file_name)) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonCoreError::io(
                "failed to open task event log tail",
                &path,
                source,
            ));
        }
    };
    let mut lock = AnchoredFileLock::lock_existing(
        &events,
        OsStr::new(&file_name),
        path.clone(),
        file,
        AnchoredFileLockMode::Shared,
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire authenticated task event tail lock",
            &path,
            source,
        )
    })?;
    let result = lease
        .validate_namespace_attachment()
        .and_then(|()| {
            events.validate_display_path_attachment().map_err(|source| {
                DaemonCoreError::io("task event namespace is detached", &path, source)
            })
        })
        .and_then(|()| validate_anchored_event_namespace_aliases(&events, &file_name, &path))
        .and_then(|()| inspect_locked_task_event_tail(lock.file_mut(), &path, task_id))
        .and_then(|inspection| after_read().map(|()| inspection))
        .and_then(|inspection| {
            lease
                .validate_namespace_attachment()
                .and_then(|()| {
                    events.validate_display_path_attachment().map_err(|source| {
                        DaemonCoreError::io("task event namespace is detached", &path, source)
                    })
                })
                .and_then(|()| {
                    validate_anchored_event_namespace_aliases(&events, &file_name, &path)
                })
                .and_then(|()| {
                    lock.validate_attachment().map_err(|source| {
                        DaemonCoreError::io(
                            "task event tail binding changed while locked",
                            &path,
                            source,
                        )
                    })
                })
                .map(|()| inspection.tail.map(|frame| frame.seq))
        });
    let finish = lock.finish();
    match (result, finish) {
        (Ok(sequence), Ok(())) => Ok(sequence),
        (Ok(_), Err(AnchoredFileLockFinishError::Attachment(source))) => Err(DaemonCoreError::io(
            "task event tail lock changed before unlock",
            &path,
            source,
        )),
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock task event tail",
            &path,
            source,
        )),
        (Err(error), _) => Err(error),
    }
}

#[cfg(unix)]
fn append_next_task_event_admitted(
    root: &Path,
    task_id: &TaskStorageId,
    lease: &crate::task_store_lease::TaskStoreLease,
    event: &DaemonEvent,
) -> Result<DaemonEventFrame> {
    append_next_task_event_admitted_with_observers(
        root,
        task_id,
        lease,
        event,
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(unix)]
pub(super) fn append_next_task_event_admitted_with_observers(
    root: &Path,
    task_id: &TaskStorageId,
    lease: &crate::task_store_lease::TaskStoreLease,
    event: &DaemonEvent,
    after_tail_read: impl FnOnce() -> Result<()>,
    after_sync: impl FnOnce() -> Result<()>,
) -> Result<DaemonEventFrame> {
    let path = task_event_log_path(root, task_id);
    let file_name = event_log_file_name(task_id);
    let events = open_task_events_capability_for_write(root)?;
    validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
    let file = events
        .open_append_file(OsStr::new(&file_name))
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open task event log for sequence allocation",
                &path,
                source,
            )
        })?;
    validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
    let mut lock = AnchoredFileLock::lock_existing(
        &events,
        OsStr::new(&file_name),
        path.clone(),
        file,
        AnchoredFileLockMode::Exclusive,
    )
    .map_err(|source| {
        DaemonCoreError::io(
            "failed to acquire exclusive task event sequence lock",
            &path,
            source,
        )
    })?;
    let result = (|| -> Result<DaemonEventFrame> {
        lease.validate_namespace_attachment()?;
        events
            .validate_display_path_attachment()
            .map_err(|source| {
                DaemonCoreError::io("task event namespace is detached", &path, source)
            })?;
        validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
        lock.validate_attachment().map_err(|source| {
            DaemonCoreError::io(
                "task event sequence binding changed while locked",
                &path,
                source,
            )
        })?;

        let inspection = inspect_locked_task_event_tail(lock.file_mut(), &path, task_id)?;
        after_tail_read()?;
        let sequence =
            next_task_event_sequence(&path, inspection.tail.as_ref().map(|frame| frame.seq))?;
        let frame = DaemonEventFrame {
            seq: sequence,
            task_id: task_id.as_str().to_string(),
            event: event.clone(),
        };
        let bytes = encode_task_event_frame(root, task_id, &frame)?;

        lease.validate_namespace_attachment()?;
        events
            .validate_display_path_attachment()
            .map_err(|source| {
                DaemonCoreError::io("task event namespace is detached", &path, source)
            })?;
        validate_anchored_event_namespace_aliases(&events, &file_name, &path)?;
        lock.validate_attachment().map_err(|source| {
            DaemonCoreError::io(
                "task event sequence binding changed before append",
                &path,
                source,
            )
        })?;
        if inspection.has_partial_suffix {
            lock.file_mut()
                .set_len(inspection.complete_len)
                .map_err(|source| {
                    DaemonCoreError::io(
                        "failed to truncate crash-partial task event suffix",
                        &path,
                        source,
                    )
                })?;
            sync_task_event_file(lock.file(), &path)?;
        }
        lock.file_mut().write_all(&bytes).map_err(|source| {
            DaemonCoreError::io("failed to append allocated task event", &path, source)
        })?;
        sync_task_event_file(lock.file(), &path)?;
        after_sync()?;

        let postcheck = lease
            .validate_namespace_attachment()
            .and_then(|()| {
                events.validate_display_path_attachment().map_err(|source| {
                    DaemonCoreError::io("task event namespace is detached", &path, source)
                })
            })
            .and_then(|()| validate_anchored_event_namespace_aliases(&events, &file_name, &path))
            .and_then(|()| {
                lock.validate_attachment().map_err(|source| {
                    DaemonCoreError::io(
                        "task event sequence binding changed after append",
                        &path,
                        source,
                    )
                })
            });
        if let Err(error) = postcheck {
            return Err(storage_mutation_authority_lost(
                "append next task event",
                &path,
                error,
            ));
        }
        Ok(frame)
    })();
    let finish = lock.finish();
    match (result, finish) {
        (Ok(frame), Ok(())) => Ok(frame),
        (Ok(_), Err(AnchoredFileLockFinishError::Attachment(source))) => {
            Err(DaemonCoreError::StorageMutationAuthorityLost {
                operation: "append next task event",
                path,
                source,
            })
        }
        (Ok(_), Err(AnchoredFileLockFinishError::Unlock(source))) => Err(DaemonCoreError::io(
            "failed to unlock allocated task event",
            &path,
            source,
        )),
        (Err(error), _) => Err(error),
    }
}

#[cfg(not(unix))]
pub(super) fn task_event_log_tail_sequence_portable(
    root: &Path,
    task_id: &TaskStorageId,
) -> Result<Option<u64>> {
    let path = task_event_log_path(root, task_id);
    let Some(mut file) = open_task_event_file_for_read(root, task_id, &path)? else {
        return Ok(None);
    };
    FileExt::lock_shared(&file)
        .map_err(|source| DaemonCoreError::io("failed to lock task event tail", &path, source))?;
    let result = inspect_locked_task_event_tail(&mut file, &path, task_id)
        .map(|tail| tail.tail.map(|f| f.seq));
    let unlock = FileExt::unlock(&file)
        .map_err(|source| DaemonCoreError::io("failed to unlock task event tail", &path, source));
    match (result, unlock) {
        (Ok(sequence), Ok(())) => Ok(sequence),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

#[cfg(not(unix))]
fn append_next_task_event_portable(
    root: &Path,
    task_id: &TaskStorageId,
    event: &DaemonEvent,
) -> Result<DaemonEventFrame> {
    let path = task_event_log_path(root, task_id);
    let directory = task_events_dir(root);
    let file_name = event_log_file_name(task_id);
    ensure_portable_real_directory(&directory)?;
    validate_portable_event_namespace_aliases(&directory, &file_name, &path)?;
    validate_portable_event_file_type(&path)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .map_err(|source| {
            DaemonCoreError::io(
                "failed to open portable task event sequence log",
                &path,
                source,
            )
        })?;
    FileExt::lock_exclusive(&file).map_err(|source| {
        DaemonCoreError::io(
            "failed to lock portable task event sequence log",
            &path,
            source,
        )
    })?;
    let result = (|| -> Result<DaemonEventFrame> {
        validate_portable_event_namespace_aliases(&directory, &file_name, &path)?;
        validate_portable_event_file_type(&path)?;
        let inspection = inspect_locked_task_event_tail(&mut file, &path, task_id)?;
        let sequence =
            next_task_event_sequence(&path, inspection.tail.as_ref().map(|frame| frame.seq))?;
        let frame = DaemonEventFrame {
            seq: sequence,
            task_id: task_id.as_str().to_string(),
            event: event.clone(),
        };
        let bytes = encode_task_event_frame(root, task_id, &frame)?;
        if inspection.has_partial_suffix {
            file.set_len(inspection.complete_len).map_err(|source| {
                DaemonCoreError::io(
                    "failed to truncate portable crash-partial event suffix",
                    &path,
                    source,
                )
            })?;
            sync_task_event_file(&file, &path)?;
        }
        file.write_all(&bytes).map_err(|source| {
            DaemonCoreError::io(
                "failed to append portable allocated task event",
                &path,
                source,
            )
        })?;
        sync_task_event_file(&file, &path)?;
        Ok(frame)
    })();
    let unlock = FileExt::unlock(&file).map_err(|source| {
        DaemonCoreError::io(
            "failed to unlock portable task event sequence log",
            &path,
            source,
        )
    });
    match (result, unlock) {
        (Ok(frame), Ok(())) => Ok(frame),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

pub(super) fn inspect_locked_task_event_tail(
    file: &mut fs::File,
    path: &Path,
    task_id: &TaskStorageId,
) -> Result<TaskEventTailInspection> {
    let len = file
        .metadata()
        .map_err(|source| DaemonCoreError::io("failed to inspect task event tail", path, source))?
        .len();
    file.seek(SeekFrom::Start(0))
        .map_err(|source| DaemonCoreError::io("failed to seek task event log", path, source))?;
    let mut reader = BufReader::new(&mut *file);
    let mut complete_len = 0_u64;
    let mut tail: Option<DaemonEventFrame> = None;
    let mut has_partial_suffix = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_TASK_EVENT_LINE_BYTES.saturating_add(2) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|source| DaemonCoreError::io("failed to read task event log", path, source))?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            if line.len() > MAX_TASK_EVENT_LINE_BYTES {
                return Err(task_event_limit_error(
                    path,
                    "crash-partial tail bytes",
                    line.len() as u64,
                    MAX_TASK_EVENT_LINE_BYTES as u64,
                ));
            }
            has_partial_suffix = true;
            break;
        }

        let frame = decode_complete_task_event_frame(
            path,
            task_id,
            complete_len,
            &line[..line.len().saturating_sub(1)],
        )?;
        if let Some(previous) = tail.as_ref() {
            let expected = next_task_event_sequence(path, Some(previous.seq))?;
            if frame.seq != expected {
                return Err(DaemonCoreError::InvalidTaskEventFrame {
                    path: path.to_path_buf(),
                    message: format!(
                        "task event sequence is not contiguous at byte {complete_len}: expected {expected}, found {}",
                        frame.seq
                    ),
                });
            }
        } else if frame.seq != 1 {
            return Err(DaemonCoreError::InvalidTaskEventFrame {
                path: path.to_path_buf(),
                message: format!("first task event sequence must be 1, found {}", frame.seq),
            });
        }
        complete_len += read as u64;
        tail = Some(frame);
    }
    drop(reader);

    if file
        .metadata()
        .map_err(|source| DaemonCoreError::io("failed to re-inspect task event log", path, source))?
        .len()
        != len
    {
        return Err(DaemonCoreError::io(
            "task event log changed during locked validation",
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "locked event length changed during streaming validation",
            ),
        ));
    }
    Ok(TaskEventTailInspection {
        tail,
        complete_len,
        has_partial_suffix,
    })
}

fn storage_mutation_authority_lost(
    operation: &'static str,
    path: &Path,
    error: DaemonCoreError,
) -> DaemonCoreError {
    let source = match error {
        DaemonCoreError::Io { source, .. } => source,
        other => std::io::Error::other(other),
    };
    DaemonCoreError::StorageMutationAuthorityLost {
        operation,
        path: path.to_path_buf(),
        source,
    }
}
