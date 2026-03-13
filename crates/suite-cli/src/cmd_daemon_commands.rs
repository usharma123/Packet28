use anyhow::{anyhow, Context, Result};
use packet28_daemon_core::{
    read_socket_message, DaemonEventFrame, DaemonIndexClearRequest, DaemonIndexRebuildRequest,
    DaemonIndexStatusRequest, DaemonRequest, DaemonResponse, TaskAwaitHandoffRequest,
    TaskLaunchAgentRequest, TaskSubmitSpec,
};

#[cfg(unix)]
use std::io::BufReader;

#[cfg(not(unix))]
use crate::cmd_daemon::daemon_not_supported;
use crate::cmd_daemon::{
    ensure_daemon, resolve_root_arg, send_request, subscribe_task, IndexArgs, IndexCommands,
    JsonRootArgs, StatusRootArgs, TaskArgs, TaskCommands, WatchArgs, WatchCommands,
};

pub(crate) fn run_start(args: StatusRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    ensure_daemon(&root)?;
    println!("daemon_started root={}", root.display());
    Ok(0)
}

pub(crate) fn run_stop(args: StatusRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    match send_request(&root, &DaemonRequest::Stop) {
        Ok(DaemonResponse::Ack { message }) => {
            println!("{message}");
            Ok(0)
        }
        Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!("unexpected daemon response: {other:?}")),
        Err(_) => {
            println!("stopping");
            Ok(0)
        }
    }
}

pub(crate) fn run_status(args: JsonRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    match send_request(&root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => {
            if args.json {
                crate::cmd_common::emit_json(&serde_json::to_value(status)?, args.pretty)?;
            } else {
                println!("pid={}", status.pid);
                println!("root={}", status.workspace_root);
                println!("socket={}", status.socket_path);
                println!("log={}", status.log_path);
                println!("tasks={}", status.tasks.len());
                println!("watches={}", status.watches.len());
                if let Some(index) = status.index {
                    println!(
                        "index={} generation={} ready={} dirty={}",
                        index.manifest.status,
                        index.manifest.generation,
                        index.ready,
                        index.dirty_file_count
                    );
                }
            }
            Ok(0)
        }
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub(crate) fn run_index(args: IndexArgs) -> Result<i32> {
    match args.command {
        IndexCommands::Status(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexStatus {
                    request: DaemonIndexStatusRequest {
                        root: root.to_string_lossy().to_string(),
                    },
                },
            )? {
                DaemonResponse::DaemonIndexStatus { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("status={}", response.manifest.status);
                        println!("generation={}", response.manifest.generation);
                        println!("ready={}", response.ready);
                        println!("fallback_mode={}", response.fallback_mode);
                        println!("dirty_files={}", response.dirty_file_count);
                        println!("queued_files={}", response.queued_file_count);
                        println!("indexed_files={}", response.manifest.indexed_files);
                        println!("total_files={}", response.manifest.total_files);
                        if let Some(err) = response.manifest.last_error {
                            println!("last_error={err}");
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        IndexCommands::Rebuild(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            let full = args.full || args.paths.is_empty();
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexRebuild {
                    request: DaemonIndexRebuildRequest {
                        root: root.to_string_lossy().to_string(),
                        full,
                        paths: args.paths,
                    },
                },
            )? {
                DaemonResponse::DaemonIndexRebuild { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("accepted={}", response.accepted);
                        println!("full={}", response.full);
                        if let Some(generation) = response.generation {
                            println!("generation={generation}");
                        }
                        if !response.queued_paths.is_empty() {
                            println!("queued_paths={}", response.queued_paths.join(","));
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        IndexCommands::Clear(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexClear {
                    request: DaemonIndexClearRequest {
                        root: root.to_string_lossy().to_string(),
                    },
                },
            )? {
                DaemonResponse::DaemonIndexClear { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("cleared={}", response.cleared);
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
    }
}

pub(crate) fn run_task(args: TaskArgs) -> Result<i32> {
    match args.command {
        TaskCommands::Submit(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            let raw = std::fs::read_to_string(&args.spec)
                .with_context(|| format!("failed to read task spec '{}'", args.spec))?;
            let spec: TaskSubmitSpec = serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in '{}'", args.spec))?;
            match send_request(&root, &DaemonRequest::ExecuteSequence { spec })? {
                DaemonResponse::ExecuteSequence {
                    response,
                    task,
                    watches,
                } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::json!({
                                "task": task,
                                "watches": watches,
                                "response": response,
                            }),
                            args.pretty,
                        )?;
                    } else {
                        let ids = watches
                            .iter()
                            .map(|watch| watch.watch_id.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                        println!(
                            "task={} request_id={} watch_ids={}",
                            task.task_id, response.request_id, ids
                        );
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Status(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskStatus {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::TaskStatus { task } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(task)?, args.pretty)?;
                    } else if let Some(task) = task {
                        println!("task={}", task.task_id);
                        println!("running={}", task.running);
                        println!("watch_ids={}", task.watch_ids.join(","));
                    } else {
                        println!("task not found");
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::AwaitHandoff(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskAwaitHandoff {
                    request: TaskAwaitHandoffRequest {
                        task_id: args.task_id,
                        timeout_ms: Some(args.timeout_ms),
                        poll_ms: Some(args.poll_ms),
                        after_context_version: args.after_context_version,
                    },
                },
            )? {
                DaemonResponse::TaskAwaitHandoff { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("waited_ms={}", response.waited_ms);
                        println!("polls={}", response.polls);
                        println!("handoff_ready={}", response.task_status.handoff_ready);
                        if let Some(reason) = response.task_status.handoff_reason {
                            println!("handoff_reason={reason}");
                        }
                        if let Some(checkpoint_id) =
                            response.task_status.latest_handoff_checkpoint_id
                        {
                            println!("latest_handoff_checkpoint_id={checkpoint_id}");
                        }
                        if let Some(artifact_id) = response.task_status.latest_handoff_artifact_id {
                            println!("latest_handoff_artifact_id={artifact_id}");
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::LaunchAgent(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskLaunchAgent {
                    request: TaskLaunchAgentRequest {
                        task_id: args.task_id,
                        task: args.task,
                        wait_for_handoff: args.wait_for_handoff,
                        handoff_timeout_ms: Some(args.handoff_timeout_ms),
                        handoff_poll_ms: Some(args.handoff_poll_ms),
                        command: args.command,
                    },
                },
            )? {
                DaemonResponse::TaskLaunchAgent { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("task_id={}", response.task_id);
                        println!("pid={}", response.pid);
                        println!("bootstrap_mode={}", response.bootstrap_mode);
                        println!("bootstrap_path={}", response.bootstrap_path);
                        println!("log_path={}", response.log_path);
                        if let Some(checkpoint_id) = response.handoff_checkpoint_id {
                            println!("handoff_checkpoint_id={checkpoint_id}");
                        }
                        if let Some(artifact_id) = response.handoff_artifact_id {
                            println!("handoff_artifact_id={artifact_id}");
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Cancel(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskCancel {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::TaskCancel {
                    task,
                    removed_watch_ids,
                } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::json!({
                                "task": task,
                                "removed_watch_ids": removed_watch_ids,
                            }),
                            args.pretty,
                        )?;
                    } else {
                        println!("removed_watch_ids={}", removed_watch_ids.join(","));
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Watch(args) => {
            #[cfg(not(unix))]
            {
                let _ = args;
                return daemon_not_supported();
            }
            #[cfg(unix)]
            {
                let root = resolve_root_arg(&args.root);
                ensure_daemon(&root)?;
                let (stream, replayed) = subscribe_task(&root, &args.task_id, args.replay_last)?;
                let mut reader = BufReader::new(stream);
                if !args.json {
                    println!("task={} replayed={}", args.task_id, replayed);
                }
                loop {
                    let frame: DaemonEventFrame = match read_socket_message(&mut reader) {
                        Ok(frame) => frame,
                        Err(err) => {
                            if args.json {
                                return Err(err);
                            }
                            println!("stream closed");
                            return Ok(0);
                        }
                    };
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(frame)?, args.pretty)?;
                        continue;
                    }
                    println!(
                        "[{}] seq={} kind={}",
                        frame.event.occurred_at_unix, frame.seq, frame.event.kind
                    );
                    if let Some(text) = frame
                        .event
                        .data
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                    {
                        println!("  {text}");
                    } else if let Some(step_id) = frame
                        .event
                        .data
                        .get("step_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        println!("  step={step_id}");
                    } else if let Some(paths) = frame
                        .event
                        .data
                        .get("paths")
                        .and_then(serde_json::Value::as_array)
                    {
                        let joined = paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",");
                        if !joined.is_empty() {
                            println!("  paths={joined}");
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn run_watch(args: WatchArgs) -> Result<i32> {
    match args.command {
        WatchCommands::List(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::WatchList {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::WatchList { watches } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(watches)?, args.pretty)?;
                    } else {
                        for watch in watches {
                            println!(
                                "watch_id={} task_id={} kind={:?} paths={}",
                                watch.watch_id,
                                watch.spec.task_id,
                                watch.spec.kind,
                                watch.spec.paths.join(",")
                            );
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        WatchCommands::Remove(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::WatchRemove {
                    watch_id: args.watch_id,
                },
            )? {
                DaemonResponse::WatchRemove { removed } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(removed)?, args.pretty)?;
                    } else if let Some(watch) = removed {
                        println!("removed watch_id={}", watch.watch_id);
                    } else {
                        println!("watch not found");
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
    }
}
