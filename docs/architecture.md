# Architecture

Packet28 is a local context runtime with three entry surfaces—CLI, MCP, and a
workspace daemon—over one set of reducer, packet, search, memory, and policy
contracts.

## System model

```text
Agent runtime or CI
  │
  ├─ Packet28 CLI
  ├─ MCP native server or upstream proxy
  └─ runtime hooks / packet28-agent
          │
          ▼
Authenticated daemon protocol and task orchestration
          │
          ▼
Context kernel, scheduler, search, memory, and policy
          │
          ▼
Artifact reducers and shared packet/storage contracts
          │
          ▼
Workspace source, Git, reports, and .packet28 durable state
```

The daemon is optional for one-shot reducer commands. It becomes the lifecycle
owner when a workflow needs persistent tasks, watches, event streams, indexed
state, recovery, or cross-turn handoff.

## Four layers

Packet28's 34 workspace crates are grouped by responsibility rather than by
product command.

### Contracts and platform primitives

These crates own data that must remain stable across process and crate
boundaries:

- `suite-packet-core`: packet envelopes, references, budget cost, provenance,
  and packet-family identifiers;
- `suite-foundation-core`: configuration, path mapping, gate policy, and shared
  cache/snapshot primitives;
- `packet28-binary-codec`: bounded durable binary framing;
- `packet28-state-fs`: authenticated filesystem capabilities, portable names,
  locks, and atomic mutation primitives;
- `packet28-daemon-protocol`: bounded daemon wire DTOs and framing.

They do not own product orchestration.

### Reducers and analysis

Reducers convert one raw artifact family into bounded structured output:

| Area | Owners |
| --- | --- |
| Coverage ingestion and gates | `covy-ingest`, `covy-core`, `diffy-core` |
| Test impact and sharding | `testy-core`, `testy-cli-common` |
| Build and stack diagnostics | `buildy-core`, `stacky-core` |
| Repository maps and search | `mapy-core`, `packet28-search-core`, `packet28-reducer-core` |
| Safe command reduction | `suite-proxy-core`, `suite-ingest` |

Reducers may depend on shared contracts. They must not acquire daemon lifecycle,
transport, or task-store ownership.

### Context runtime

The context runtime composes reducer output:

- `context-kernel-mechanism` owns target-neutral execution, cancellation,
  budgets, cache/persistence lifecycle, scheduling, governance interfaces, and
  reactive plans;
- `context-kernel-builtins` registers Packet28's built-in targets and adapters;
- `context-kernel-core` preserves the supported `0.2.x` compatibility surface;
- `context-memory-core` owns packet persistence, recall indexes, and local
  SQLite memory workflows;
- `context-scheduler-core` owns dependency ordering and budget-aware plans;
- `contextq-core` owns bounded assembly, correlation, and context guidance;
- `guardy-core` and `suite-policy-core` own governance evaluation.

The mechanism layer contains no Packet28 target strings or dependencies on
built-in reducers. See [Context-kernel composition](context-kernel-composition.md).

### Process surfaces

- `suite-cli` owns the umbrella CLI, MCP adapters, setup, hooks, and
  presentation.
- `packet28-daemon-client` owns authenticated endpoint discovery and transport.
- `packet28-daemon-core` owns task/event storage, leases, integrity, recovery,
  and retention.
- `packet28d` owns one daemon instance: startup, shutdown, task/watch workers,
  the bounded blocking pool, and Tokio orchestration.
- standalone compatibility CLIs (`covy`, `diffy`, `testy`, `p28`) expose narrow
  domain workflows.

Within the MCP upstream adapter, `proxy_upstream::reverse_requests` owns
reverse-request IDs, pending-request and byte limits, batch assembly, and
completion/timeout accounting. Its state is private. The upstream adapter owns
child processes, transport I/O, shutdown, and delivery of the returned messages.
This keeps protocol accounting testable without starting an upstream process.

The `packet28d` binary is intentionally thin. Application lifecycle belongs in
the library; protocol DTOs do not depend on runtime or storage.

## Primary flows

### One-shot command

1. The CLI parses and validates user input.
2. A reducer or kernel target reads the required artifact.
3. The result is wrapped with provenance, references, and budget estimates.
4. The selected output profile renders compact, full, or handle-backed output.

No daemon is required unless `--via-daemon` is selected.

### Persistent task

1. A client authenticates the endpoint from
   `.packet28/daemon/runtime.json`.
2. `packet28d` decodes one bounded request and admits synchronous work through
   its bounded blocking pool.
3. Task/watch changes are staged under the daemon-state lock.
4. The persistence owner writes ordered WAL deltas and causally ordered task
   events outside that lock.
5. Subscribers receive bounded generation-fenced events; slow consumers are
   disconnected and can resume from their last sequence.
6. Shutdown cancels, drains, joins, checkpoints, and removes runtime state in a
   fixed order.

The detailed contract is [Daemon runtime](daemon-runtime.md).

### Handoff

1. Hooks and MCP adapters ingest compact packets into the active task.
2. Stable instructions remain in the repository prefix; task-specific state
   stays in the adaptive broker brief.
3. The broker ranks and prunes sections within byte/token budgets.
4. The handoff artifact is persisted and can bootstrap `packet28-agent` or
   another worker.

This separation reduces repeated context without making unsupported claims
about provider-side cache placement or cost.

The hook reducer runner executes every explicitly requested command. Cached
packets never replace execution. Each successfully captured completion records fresh
`CommandFinished` evidence, even when output repeats.
The daemon may still reuse derived packets from other capture events.
Capture is optional. If capture setup fails, the runner executes the original
command with inherited output streams. If capture fails after execution, it
returns the original exit code and replays the available captured stdout and stderr bytes
without executing the command again.

## Storage ownership

All workspace-local Packet28 state lives under `.packet28/`.

| State | Owner |
| --- | --- |
| Packet cache checkpoint, backup, and WAL | `context-memory-core` persistence owner |
| Repository search generations and overlays | search/index owners |
| Task/watch checkpoint and WAL | daemon persistence owner |
| Task event logs and artifacts | admitted task-store capabilities |
| Runtime endpoint and readiness | daemon application lifecycle |
| Retention quarantine and recovery journal | exclusive retention authority |

Durable formats are bounded, versioned, checksummed, and fail closed on
authority or integrity errors. A path string is display metadata, not storage
authority; mutations use retained, authenticated descriptors.

## Async boundary

Tokio is restricted to daemon and MCP orchestration: listeners, correlated
requests, bounded connection tasks, subprocess I/O, timeouts, cancellation, and
drainable workers. CPU-heavy and synchronous filesystem work crosses a bounded
blocking boundary.

Core reducers, packet contracts, storage formats, and compatibility libraries
do not gain an async runtime dependency merely because an adapter is async.
Architecture checks enforce that direction.

## Compatibility

The repository preserves several explicit `0.2.x` seams:

- packet wrapper and packet-family wire formats;
- daemon protocol tags and legacy exhaustive status behavior;
- context-kernel root constructors and supported exports;
- daemon-core's frozen compatibility facade;
- legacy domain CLI behavior where documented.

Additive protocol work gets bounded paging or a new versioned envelope rather
than silently changing legacy semantics. Public changes require source
compatibility tests, wire snapshots, migration notes, or an explicit break.

## Extension rules

When adding behavior:

1. Put the invariant in the lowest crate that can own it without importing a
   higher layer.
2. Expose a narrow typed operation instead of re-exporting implementation
   modules.
3. Keep parsing/presentation at process edges and typed errors in libraries.
4. Inject runtime-specific behavior through an adapter or service registry, not
   repeated `match` switchboards.
5. Feature-gate architectural experiments until behavior parity and measurement
   justify default adoption.
6. Update architecture tests when a deliberate boundary changes.

Run `python3 scripts/check_architecture.py` and the relevant metadata/source
guards with every boundary change.

## Repository layout

```text
crates/        Rust libraries and binaries
scripts/       policy, validation, benchmark, packaging, and release tooling
schemas/       packet schemas and compatibility snapshots
benchmarks/    versioned performance evidence
docs/          current contracts, guides, experiments, audits, and releases
npm/           wrapper and platform package manifests
.github/       CI, benchmark, and release workflows
```

For contributor workflow and validation, read [Development](development.md) and
[CONTRIBUTING](../CONTRIBUTING.md).
