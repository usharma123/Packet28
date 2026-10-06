# Packet28 daemon runtime

This document is the operating contract for the Packet28 daemon stack. It
describes ownership, startup and shutdown order, persistence and recovery,
transport behavior, compatibility, request failure semantics, and the exact
public documentation inventory.

## Ownership boundaries

| Owner | Responsibility | Must not own |
| --- | --- | --- |
| `packet28-daemon-protocol` | Wire DTOs, request/response tags, bounded JSON framing, and deterministic endpoint paths. | Runtime, storage, kernels, memory, search, or Tokio orchestration. |
| `packet28-daemon-client` | Authenticated runtime discovery, Unix server-credential verification, and the TCP capability prelude. | Daemon runtime, persistence, kernels, memory, search, or Tokio orchestration. |
| `packet28-daemon-core` | Typed storage errors, task/event persistence, integrity, leases, trust checks, recovery, and retention. | The server lifecycle or new wire contracts. Its root re-exports are a frozen `0.2.x` compatibility facade. |
| `packet28d::application` | One daemon instance: recovery, state construction, Tokio ownership, listeners, workers, cancellation, joining, and final cleanup. | CLI parsing or broker implementation details. |
| `packet28d::broker` | Context, handoff, write-state, search, rendering, limits, snapshots, and a small explicit crate-internal facade. | Application lifecycle ownership or parent-prelude wildcard imports. |
| `packet28d` binary | CLI parsing, error presentation, and process exit. | Protocol dispatch, persistence, transport, or daemon state. |

`scripts/check_architecture.py` enforces the thin binary, broker file inventory,
explicit imports, protocol/runtime dependency direction, and the public
documentation inventory below.

## Public documentation inventory

Every public module in `packet28-daemon-protocol`,
`packet28-daemon-client`, and `packet28-daemon-core`, and every public module
or export in `packet28d`, appears exactly once here. The protocol's two root
aliases and daemon-core's frozen 182-name root inventory are classified as
compatibility groups; exact source inventories prevent additions from hiding
inside those groups. `covered` means the row names a compile-checked source
example. `excluded` means the public seam is better covered by the named exact
JSON, compatibility, failure-path, process, or platform tests. An exclusion is
deliberate coverage, not an undocumented omission.

| Owner | Public module or export | Classification | Evidence or exclusion |
| --- | --- | --- | --- |
| packet28-daemon-protocol | broker | excluded | wire-dto-json-compat-tests |
| packet28-daemon-protocol | commands | excluded | command-json-dispatch-tests |
| packet28-daemon-protocol | context_store | excluded | context-store-process-tests |
| packet28-daemon-protocol | frame | covered | protocol-frame-runnable |
| packet28-daemon-protocol | hooks | excluded | hook-ingest-json-tests |
| packet28-daemon-protocol | index | excluded | index-state-process-tests |
| packet28-daemon-protocol | logging | excluded | runtime-log-rotation-process-tests |
| packet28-daemon-protocol | message | excluded | request-response-json-tests |
| packet28-daemon-protocol | paths | excluded | path-endpoint-tests |
| packet28-daemon-protocol | process | excluded | session-detach-process-tests |
| packet28-daemon-protocol | registry | covered | protocol-registry-migration-runnable+json-compat-tests |
| packet28-daemon-protocol | task | covered | protocol-task-lifecycle-runnable+compile_fail |
| packet28-daemon-protocol | root_compatibility | excluded | exact-two-name-root-allowlist |
| packet28-daemon-client | runtime_discovery | covered | daemon-client-discovery-runnable |
| packet28-daemon-client | transport | excluded | authenticated-transport-process-tests |
| packet28-daemon-core | integrity | excluded | integrity-corruption-tests |
| packet28-daemon-core | retention | excluded | retention-recovery-process-tests |
| packet28-daemon-core | storage | covered | daemon-core-storage-runnable |
| packet28-daemon-core | task_store_lease | excluded | lease-authority-process-tests |
| packet28-daemon-core | trust | excluded | trust-platform-tests |
| packet28-daemon-core | root_compatibility | excluded | exact-182-name-frozen-v0-inventory |
| packet28d | serve | excluded | non-hermetic-process-lifecycle-owner |
| packet28d | serve_with_managed_log | excluded | non-hermetic-process-lifecycle-owner |
| packet28d | start | excluded | p28-bootstrap-authority-process-tests |
| packet28d | shared_repository_scan | covered | packet28d-shared-scan-no_run+feature-shared-repository-scan |

<!-- packet28d-public owner=packet28-daemon-protocol item=broker classification=excluded evidence=wire-dto-json-compat-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=commands classification=excluded evidence=command-json-dispatch-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=context_store classification=excluded evidence=context-store-process-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=frame classification=covered evidence=protocol-frame-runnable -->
<!-- packet28d-public owner=packet28-daemon-protocol item=hooks classification=excluded evidence=hook-ingest-json-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=index classification=excluded evidence=index-state-process-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=logging classification=excluded evidence=runtime-log-rotation-process-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=message classification=excluded evidence=request-response-json-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=paths classification=excluded evidence=path-endpoint-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=process classification=excluded evidence=session-detach-process-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=registry classification=covered evidence=protocol-registry-migration-runnable+json-compat-tests -->
<!-- packet28d-public owner=packet28-daemon-protocol item=task classification=covered evidence=protocol-task-lifecycle-runnable+compile_fail -->
<!-- packet28d-public owner=packet28-daemon-protocol item=root_compatibility classification=excluded evidence=exact-two-name-root-allowlist -->
<!-- packet28d-public owner=packet28-daemon-client item=runtime_discovery classification=covered evidence=daemon-client-discovery-runnable -->
<!-- packet28d-public owner=packet28-daemon-client item=transport classification=excluded evidence=authenticated-transport-process-tests -->
<!-- packet28d-public owner=packet28-daemon-core item=integrity classification=excluded evidence=integrity-corruption-tests -->
<!-- packet28d-public owner=packet28-daemon-core item=retention classification=excluded evidence=retention-recovery-process-tests -->
<!-- packet28d-public owner=packet28-daemon-core item=storage classification=covered evidence=daemon-core-storage-runnable -->
<!-- packet28d-public owner=packet28-daemon-core item=task_store_lease classification=excluded evidence=lease-authority-process-tests -->
<!-- packet28d-public owner=packet28-daemon-core item=trust classification=excluded evidence=trust-platform-tests -->
<!-- packet28d-public owner=packet28-daemon-core item=root_compatibility classification=excluded evidence=exact-182-name-frozen-v0-inventory -->
<!-- packet28d-public owner=packet28d item=serve classification=excluded evidence=non-hermetic-process-lifecycle-owner -->
<!-- packet28d-public owner=packet28d item=serve_with_managed_log classification=excluded evidence=non-hermetic-process-lifecycle-owner -->
<!-- packet28d-public owner=packet28d item=start classification=excluded evidence=p28-bootstrap-authority-process-tests -->
<!-- packet28d-public owner=packet28d item=shared_repository_scan classification=covered evidence=packet28d-shared-scan-no_run+feature-shared-repository-scan -->

`packet28d::serve` and `packet28d::serve_with_managed_log` are intentionally
excluded from a runnable happy-path doctest. They change the process working
directory, acquire workspace leases, publish runtime files, bind a listener,
and block until shutdown, so a runnable doctest would not be hermetic.
Lifecycle and process tests cover that boundary instead. The managed-log
variant additionally owns the size-rotated workspace log described in
[operations](operations.md#logs-and-troubleshooting).

`packet28d::start` (`packet28d start --root`) is excluded for the same reason:
it takes the startup lease, waits for a stopping daemon to release the instance
lease, and spawns `serve`. It is the bootstrap path for `p28`, which may not
link `packet28-daemon-core`. The `p28` daemon process tests cover a held
shutdown and the bounded authority timeout.

Bootstrap, in `packet28d start` and the Packet28 CLI, keeps two phases with
separate fixed deadlines. An instance-lease owner that is neither serving nor
starting, such as a stopping daemon, gets 10 s to release authority. A daemon
that bootstrap spawned, or an owner whose authenticated runtime metadata has no
readiness time and whose endpoint accepts connections, gets 30 s to answer
bounded V1 status with the same pid and workspace root. Each status request is
bounded by the time left in its phase. Runtime metadata only selects what to
wait for; cleanup and spawn still require the released instance lease, and
unauthentic or malformed metadata fails closed. A timed-out caller leaves a
starting daemon running. Startup-lease acquisition precedes both deadlines and
blocks behind another bootstrap or an explicit stop, so neither phase bounds a
whole call. Gated-startup process tests in `p28` and the CLI hold a real
registry lock to keep startup pending past 10 s.

The checker also requires each source anchor below to resolve to exactly one
Rustdoc fence of the declared kind and to contain the relevant API operations.
This covers the public `DaemonCoreError` re-export and the frozen core-root
compatibility boundary in addition to the module rows.

| Anchor | Source | Fence |
| --- | --- | --- |
| protocol-frame-runnable | crates/packet28-daemon-protocol/src/frame.rs | runnable |
| protocol-registry-migration-runnable | crates/packet28-daemon-protocol/src/registry.rs | runnable |
| protocol-task-lifecycle-runnable | crates/packet28-daemon-protocol/src/task.rs | runnable |
| protocol-task-lifecycle-compile_fail | crates/packet28-daemon-protocol/src/task.rs | compile_fail |
| daemon-client-discovery-runnable | crates/packet28-daemon-client/src/lib.rs | runnable |
| daemon-core-storage-runnable | crates/packet28-daemon-core/src/lib.rs | runnable |
| daemon-core-error-source-chain-runnable | crates/packet28-daemon-core/src/error.rs | runnable |
| daemon-core-root-compatibility-compile_fail | crates/packet28-daemon-core/src/lib.rs | compile_fail |
| packet28d-shared-scan-no_run | crates/packet28d/src/shared_repository_scan.rs | no_run |

<!-- packet28d-anchor id=protocol-frame-runnable source=crates/packet28-daemon-protocol/src/frame.rs fence=runnable -->
<!-- packet28d-anchor id=protocol-registry-migration-runnable source=crates/packet28-daemon-protocol/src/registry.rs fence=runnable -->
<!-- packet28d-anchor id=protocol-task-lifecycle-runnable source=crates/packet28-daemon-protocol/src/task.rs fence=runnable -->
<!-- packet28d-anchor id=protocol-task-lifecycle-compile_fail source=crates/packet28-daemon-protocol/src/task.rs fence=compile_fail -->
<!-- packet28d-anchor id=daemon-client-discovery-runnable source=crates/packet28-daemon-client/src/lib.rs fence=runnable -->
<!-- packet28d-anchor id=daemon-core-storage-runnable source=crates/packet28-daemon-core/src/lib.rs fence=runnable -->
<!-- packet28d-anchor id=daemon-core-error-source-chain-runnable source=crates/packet28-daemon-core/src/error.rs fence=runnable -->
<!-- packet28d-anchor id=daemon-core-root-compatibility-compile_fail source=crates/packet28-daemon-core/src/lib.rs fence=compile_fail -->
<!-- packet28d-anchor id=packet28d-shared-scan-no_run source=crates/packet28d/src/shared_repository_scan.rs fence=no_run -->

## Startup and request flow

`packet28d::serve(root)` resolves the nearest repository root and owns this
order:

1. Change to the resolved workspace, acquire the single-daemon instance lease,
   recover task-store quarantine under authority, and retain the task-store
   lease.
2. Ensure the daemon directory, load runtime configuration, bind the preferred
   Unix transport or its fallback, generate a fresh owner capability when TCP
   is selected, and write initial owner-only `runtime.json` metadata with no
   readiness timestamp.
3. Construct the persistent kernel registry, load task/watch registries and
   authenticated event-log tails, start the single persistence owner, and
   reconcile lagging registry high-water marks.
4. Load index state, construct `DaemonState`, restore watchers, and enqueue
   initial index work.
5. Rewrite `runtime.json` with `ready_at_unix`, publish the readiness file, then
   create the Tokio runtime and start the transport, watch, background, and
   index owners.
6. Authenticate the accepted Unix peer UID or the TCP capability prelude,
   decode a bounded protocol frame, dispatch it in `server`, cross the bounded
   blocking pool for synchronous filesystem/kernel/index work, flush request
   persistence, and encode one response frame.

The listener and initial runtime metadata therefore exist before registry load
and persistence startup, but clients must wait for readiness before sending
work. The protocol crate remains usable by clients without linking runtime or
storage implementations.

## Persistence, admission, and recovery

Task-event logs are the durable sequence authority. An append validates the
existing authenticated tail, allocates the next sequence, and synchronizes the
frame. Callers stage only the task/watch records changed while holding the
daemon-state mutex. Staging assigns an ordered revision; one persistence owner
coalesces same-key changes and appends a checksummed registry WAL batch after
the mutex is released. Before an event append, the owner drains every required
registry revision, so causally prior watch/task metadata is durable before the
event. The post-event high-water update becomes a later keyed delta.

Request barriers wait for the requested WAL revision, not for a complete
registry rewrite. The owner debounces full paired checkpoints, forces them for
new task namespace admission and explicit startup promotion, and writes one on
orderly shutdown when the checkpoint trails the WAL. A failed WAL append stays
pending; a failed checkpoint leaves the synchronized WAL replayable and is
retried without re-appending it. WAL exhaustion checkpoints the current image
before retrying the pending batch.

Startup loads one authority composed from the paired checkpoint, contiguous WAL
revisions, and authenticated event tails. A torn final WAL frame is truncated
and synchronized; a complete checksum failure, changed exact-revision retry,
revision gap, or invalid task/watch relationship fails closed. Startup
reconciles event high-water marks in memory, forces the resulting authority
through a full checkpoint, and only then publishes readiness.

Task/watch snapshots use a recovery journal and a final commit manifest. The
prior committed task and watch images plus their BLAKE3 descriptors are made
durable before either canonical registry is replaced. The watch image and task
image are then published, and only an atomic
`task-watch-checkpoint-v1.json` replacement commits the new generation.
Startup recognizes only the exact journaled publication phases: a crash after
one or both canonical replacements automatically reloads the prior committed
pair, while unrelated generation changes, malformed metadata, and digest
mismatches remain fail-closed corruption. Legacy pairs without a manifest are
accepted and promoted by the next daemon checkpoint.

A task must be durably admitted to the registry before any managed artifact or
event namespace is created. Artifact and hook writers call the admission fence,
which waits for the persistence owner to publish the required revision. Startup
then either reconciles a lagging registry from valid event tails or fails
closed if a registry is ahead of its log, a durable frame is corrupt, or
recovery authority is conflicted.

Each task generation admits at most one delegated agent launch at a time. The
daemon starts a process-group leader in a launch-gate shim, registers that
owned child, stages its task-scoped PID and launch metadata while the state lock
is held, waits for that exact WAL revision, and durably appends
`task.agent_launch_started` before releasing the requested command. A later
task mutation receives a later revision, so a delayed launch durability wait
cannot publish a stale full-registry snapshot over it. A concurrent launch for
the same task is rejected until the owned child waiter has recorded completion
and removed the child. If the daemon crashes or persistence fails between spawn
and the ownership barrier, closing the gate pipe makes the shim exit without
executing delegated work.

Startup compares the event tail with the committed checkpoint and registry WAL.
A corrupt log, or a missing or valid truncated log behind that committed high-water,
fences its task before the persistence owner starts. Existing bytes move to the
reserved quarantine name after the file identity and observed tail are rechecked
under an exclusive log lock. A missing log has no evidence file to move. A log
whose tail is ahead of the registry follows ordinary forward reconciliation.
The damaged task becomes a terminal `superseded_by` record, and work
continues under a newly admitted linked successor with a fresh event sequence;
see [Corrupt event-log recovery](task-store-retention.md#corrupt-event-log-recovery).
Startup records constant-size recovery links before readiness. Successor snapshots
read predecessor packets in place using the authenticated registry lineage. Continuation requests and subscriptions that name a
superseded task are rejected with an error naming its successor.

An interrupted move resumes the existing reciprocal link and reserved quarantine
name. It does not create another successor or reuse event numbers on the damaged
identity. Startup still fails if the final reconciliation sees an unexplained
registry-ahead tail after recovery.

A pending replan without its stored sequence becomes idle with an explanatory
`last_error`, and that change is durable before readiness. An uncompleted agent
whose persisted process group is still live continues to block startup.

Detailed retention, journal, corruption, and descriptor-confinement guarantees
are in [Task-store retention](task-store-retention.md).

## Transport, endpoint discovery, and compatibility

Unix domain sockets are the primary transport. The preferred socket is placed
under an effective-user-specific `0700` temporary directory whose owner, mode,
ACL, and namespace ancestry are authenticated before bind. A workspace-local
Unix socket is the permission fallback; forced TCP and Unix-permission failure
use a loopback TCP listener. Bound Unix socket nodes are owner-authenticated
and published with `0600` permissions. Both transports use an eight-byte
big-endian length followed by one bounded JSON value, the same cancellation
signal, connection cap, read/write deadlines, and owned Tokio connection task
set.
TCP authentication has its own smaller pending-connection cap and one-second
default deadline. An unauthenticated peer therefore cannot consume the normal
authenticated connection budget; both controls are independently configurable.

Clients must discover the endpoint from `.packet28/daemon/runtime.json` after
the readiness file appears. Its `socket_path` field is the selected endpoint:
either a Unix path or `tcp://127.0.0.1:<port>`. The conventional socket path is
not authoritative when fallback transport is active. Runtime discovery is
published as an authenticated owner-only regular file. A TCP runtime also
contains `transport_auth`, a redacted-in-debug 256-bit per-instance capability.
The suite CLI, `p28`, and the Linux and macOS instruction shims all follow this
same discovery and authentication contract.
Capability-bearing metadata is rejected if any group or other permission bit
is present, including read-only exposure; legacy secret-free Unix metadata may
remain owner-readable with conventional `0644` permissions.
The client sends that value as the first framed message and waits for an
authentication acknowledgement before sending any `DaemonRequest`.

Unix authentication is mutual: accepts fail closed unless the client
credential UID matches the daemon's effective UID, and clients verify the
connected server credential UID before sending any frame. TCP accepts fail
closed on a missing, malformed, stale, or incorrect capability; authentication
failures all use the same response and cannot dispatch commands. A `0.2.x`
runtime file that names a TCP endpoint without `transport_auth` is treated as a
legacy insecure daemon: the upgraded client reports an explicit
stop-and-restart migration error instead of using it.

Subscriber and watch queues are bounded. A slow subscriber is disconnected
after its queued frames drain and resumes from its last sequence with
`TaskSubscribe.after_seq`. Watch overflow is coalesced for the current
generation and reported on the next trigger before conservative replanning.

The frozen legacy `Status` wire shape remains an exhaustive task/watch dump
when it fits its 1 MiB response budget. If the complete vectors do not fit, it
returns an explicit error directing callers to registry V1; it never returns a
prefix that looks exhaustive.

Bounded liveness uses `DaemonRegistryRequestV1::Status`
(`registry_status_v1` on the wire). Its response always reports complete
task/watch counts and a registry revision. Large index path vectors are
omitted with `index_truncated` set. Clients that need complete state use the
lexicographically ordered registry V1 `TaskListPage` and `WatchListPage`
requests. Each accepts 1–256 records, caps individual encoded records at
1 MiB and responses at 4 MiB, and returns an exclusive last-ID cursor plus
`snapshot_revision`. Every request after the first must echo that revision;
the token combines a random daemon-instance ID with a monotonic mutation
counter. Any intervening mutation or daemon restart is rejected so pages from
different registry states cannot be combined. Invalid limits, stale cursors, and records too large for
the bounded page contract produce an explicit error frame. The legacy
`WatchList` and `TaskStatus` requests retain their ordinary small-response
shape, but return an explicit pagination/size error rather than attempting an
oversized transport write.

`packet28-daemon-client` is the preferred connection dependency;
`packet28-daemon-protocol` remains the implementation-free wire dependency.
`packet28-daemon-core` retains an explicit, frozen root facade through the
`0.2.x` line for source compatibility; new protocol items are not added there.
Protocol framing returns `FrameError`, daemon storage returns
`DaemonCoreError`, and `packet28d::serve` adds application context at the
process boundary with `anyhow::Error`.

## Request completion and legacy errors

Every ordinary blocking request runs its handler and then requests a
persistence flush even when the handler failed. A successful handler with a
failed flush becomes a failed request rather than returning a success that was
not checkpointed. If both fail, the handler failure remains in the internal
source chain and the flush failure is attached as context.

The legacy runtime wire boundary represents all such failures as
`DaemonResponse::Error { message: String }`. That conversion preserves a
human-readable message but erases typed `FrameError`, `DaemonCoreError`, and
`anyhow` source-chain structure from the response. Clients must not infer a
stable typed error kind from the string.

Clap reports standalone `packet28d` argument errors with status `2`. For
startup, runtime, persistence, or cleanup failure, `main` prints the error
chain to stderr and also exits with status `2`.

## `TaskCancel` contract

`TaskCancel` has a dedicated bounded blocking lane, separate from both normal
work and the short control-codec lane, so saturated data work cannot prevent
cancellation or `Stop` progress. For an existing task, cancellation:

1. Obtains the current generation (creating its token if needed), marks that
   generation cancelled, moves the task lifecycle to cancelling, and queues
   persistence.
2. Removes every registered watch and watcher handle.
3. Sends `SIGTERM` to each owned delegated process group, allows a 250 ms grace,
   escalates remaining groups to `SIGKILL`, and waits up to five seconds for
   child waiters to reap them.
4. Waits up to 30 seconds for all generation operations and children to become
   idle. Generation checks prevent late completion, context, artifact, or
   replacement publication.
5. Removes the task, subscribers, and current generation, queues the final
   state, and returns the removed task and watch IDs only after the enclosing
   request flush succeeds.

A missing task is idempotent and returns `task: null` with no removed watch
IDs. A process, quiescence, state, or flush failure returns the legacy
`DaemonResponse::Error` string; it does not misreport successful durable
cancellation.

## Shutdown

A stop request first acknowledges on the active transport and requests the
shared shutdown signal. The supervisor then:

1. Withdraws readiness, marks state as shutting down, requests cancellation for
   every active task generation, clears watcher ownership, and stops new index
   work.
2. Stops accepting connections and joins transport, watch, background, and
   index owners within one grace deadline.
3. Waits for admitted blocking mutations; work that cannot be cancelled keeps
   its lifecycle leases until it actually exits.
4. Shuts down persistent kernel caches and flushes the persistence owner.
5. Removes runtime files, then releases task-store and daemon-instance leases.

The first lifecycle failure is the error returned by `serve`. Later cleanup
failures are logged as additional shutdown failures and do not replace or join
that returned error.

## Validation map

The canonical gate runs the architecture checker and the full workspace tests.
Focused daemon evidence is:

```text
cargo check -p packet28d --all-targets --all-features --locked
cargo clippy -p packet28d --all-targets --all-features --locked -- -D warnings
cargo test -p packet28d --all-features --locked
cargo test -p packet28-daemon-protocol --all-features --locked --doc
cargo test -p packet28-daemon-client --all-features --locked --doc
cargo test -p packet28-daemon-core --all-features --locked --doc
python3 scripts/check_architecture.py
python3 -m unittest scripts.tests.test_check_architecture
```

The daemon tests include authenticated TCP success, missing/wrong TCP
capability rejection, Unix peer-owner enforcement, endpoint metadata
permissions, TCP/Unix stop parity, connection caps and deadlines,
slow-subscriber replay, cancellation generation fencing, process-group reap,
persistence failure and bounded shutdown, startup high-water reconciliation,
artifact-admission ordering, and task-store corruption/recovery cases.
