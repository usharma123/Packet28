# Daemon Protocol

## Overview

`packet28d` is a local Unix socket daemon that provides persistent state, file watching, task lifecycle management, and command routing for long-running agent workflows.

## Transport

- Unix domain socket at `.packet28/daemon/packet28d.sock`
- Thread-per-connection accept loop (blocking accept + `thread::spawn`)
- JSON-over-socket: one JSON object per request, one JSON object per response
- Connection-scoped: each CLI invocation opens a new connection

## Lifecycle

```bash
# Start the daemon (auto-starts if not running when --via-daemon is used)
Packet28 daemon start --root .

# Check status
Packet28 daemon status --root . --json

# Stop the daemon
Packet28 daemon stop --root .
```

Runtime info is persisted to `.packet28/daemon/runtime.json`:
- `pid`: Daemon process ID
- `socket_path`: Absolute path to the socket
- `started_at_unix`: Startup timestamp

## Request / Response Protocol

All requests and responses are JSON-serialized `DaemonRequest` / `DaemonResponse` enums.

### Kernel Execution

```
DaemonRequest::Execute { request: KernelRequest }
→ DaemonResponse::Execute { response: KernelResponse }
```

Routes a single `KernelRequest` through the daemon's kernel instance. The daemon's kernel shares a persistent `PacketCache`, so results from prior requests are cached and available for recall.

### Task Submission

```
DaemonRequest::ExecuteSequence { spec: TaskSubmitSpec }
→ DaemonResponse::ExecuteSequence { response, task: TaskRecord, watches: Vec<WatchRegistration> }
```

Submits a multi-step task with optional file watches. The `TaskSubmitSpec` contains:

- `sequence: KernelSequenceRequest` — DAG of kernel steps with dependencies
- `watches: Vec<WatchSpec>` — File/git/test report watchers

Step IDs are auto-generated if blank or missing (via `normalize_sequence_request`).

### Task Lifecycle

```
DaemonRequest::TaskStatus { task_id }
→ DaemonResponse::TaskStatus { task: TaskRecord }

DaemonRequest::TaskCancel { task_id }
→ DaemonResponse::TaskCancel { task, removed_watch_ids }
```

Failed tasks automatically clean up their watches.

### Task Streaming

```
DaemonRequest::TaskSubscribe { task_id, replay_last }
→ DaemonResponse::TaskSubscribeAck { task_id, replayed }
→ (streaming) step_started, step_completed, step_failed, replan_applied, context_updated
```

After the initial ack, the connection stays open and the daemon streams per-step lifecycle events. Events include:

- `step_started`: Step execution began
- `step_completed`: Step finished successfully
- `step_failed`: Step failed with error
- `replan_applied`: Reactive mutation applied to the sequence
- `context_updated`: Summary of working set tokens and evictable tokens

`replay_last: true` replays the most recent event for each completed step.

### Watch Management

```
DaemonRequest::WatchList { task_id }
→ DaemonResponse::WatchList { watches }

DaemonRequest::WatchRemove { watch_id }
→ DaemonResponse::WatchRemove { removed }
```

Watch kinds:
- `File`: Glob pattern matching (e.g. `src/**/*.rs`)
- `Git`: Git ref change detection
- `TestReport`: Test result file monitoring

### Context Operations

```
DaemonRequest::ContextRecall { request }
→ DaemonResponse::ContextRecall { response }

DaemonRequest::ContextStoreList/Get/Prune/Stats { request }
→ DaemonResponse::ContextStore* { response }
```

These use the daemon's in-memory `PacketCache`, which persists to disk.

### Direct Domain Commands

```
DaemonRequest::CoverCheck { request }
→ DaemonResponse::CoverCheck { response }
```

Some commands bypass the kernel for efficiency.

## File Watching and Replan

When watches detect file changes:

1. Events are debounce-coalesced via `PendingWatchEvent` with a `due_at` timestamp
2. On flush, the daemon triggers a reactive replan for the associated task
3. The replan refreshes task context using `ScheduleMutation` (cancel/replace/append steps)
4. Subscribers receive `replan_applied` and `context_updated` events

## Persistence

| File | Purpose |
| --- | --- |
| `.packet28/daemon/packet28d.sock` | Unix socket |
| `.packet28/daemon/runtime.json` | PID, socket path, startup time |
| `.packet28/daemon/packet28d.log` | Daemon log output |
| `.packet28/daemon/watch-registry-v1.json` | Active watches (survives restart) |
| `.packet28/daemon/task-registry-v1.json` | Task state (survives restart) |
| `.packet28/daemon/tasks/<id>/events.jsonl` | Per-task event log |
| `.packet28/packet-cache-v2.bin` | Persistent packet cache |

## CLI Integration

Any Packet28 command can be routed through the daemon with `--via-daemon`:

```bash
Packet28 diff analyze --coverage report.xml --via-daemon --json
Packet28 map repo --repo-root . --via-daemon --json
Packet28 context recall --query "coverage gap" --via-daemon --json
```

The daemon auto-starts if not already running. `--daemon-root` overrides the workspace root for socket resolution.

## Error Handling

- Broken pipe on subscriber disconnect is suppressed (not logged as error)
- Failed task submissions clean up associated watches
- Socket write errors on benign disconnects are silently ignored
