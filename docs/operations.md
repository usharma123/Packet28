# Operations

This guide covers normal daemon, MCP, state, retention, and recovery operations.
For exact protocol and persistence invariants, use the
[daemon runtime contract](daemon-runtime.md).

## Health check

From the managed workspace:

```bash
packet28 doctor --root .
packet28 daemon status --root . --json
```

`doctor` checks daemon discovery, index state, MCP configuration, notifications,
and broker round trips. Resolve its first failing layer before changing task
state manually.

## Daemon lifecycle

```bash
packet28 daemon start --root .
packet28 daemon status --root . --json
packet28 daemon stop --root .
```

Only one daemon instance may own a workspace. Startup recovers authenticated
state, constructs persistence owners, restores watches, queues index work, and
publishes readiness before accepting client work. Shutdown cancels work, drains
owners, joins child tasks, and checkpoints durable state.

Do not infer readiness from a process ID or socket file. Clients discover the
selected endpoint and wait for readiness through
`.packet28/daemon/runtime.json`.

## Transport and authentication

Packet28 prefers an effective-user-specific Unix socket. It may fall back to a
workspace-local Unix socket or capability-authenticated loopback TCP.

- Unix clients verify the daemon UID; the daemon verifies the peer UID.
- TCP clients present a per-instance capability before any request.
- Runtime metadata containing a capability must be owner-only.
- A legacy TCP runtime without a capability is rejected and must be restarted.

Do not copy runtime metadata between machines or daemon instances.

## MCP modes

Native mode:

```bash
packet28 mcp serve --root .
```

Installed wrapper:

```bash
packet28-mcp --root .
```

Proxy mode:

```bash
packet28 mcp proxy --root . --upstream-config .mcp.proxy.json
```

Use native mode for Packet28-only tools. Use proxy mode when upstream MCP tool
activity must feed task context. Requests are ID-correlated, bounded, and
timeout-aware; one late upstream response cannot be reused for a later request.

## State layout

```text
.packet28/
├── packet-cache-v3.bin
├── packet-cache-v3.backup.bin
├── packet-cache-v3.wal
├── packet-cache-v3.lock
├── artifacts/
├── agent/
└── daemon/
    ├── runtime.json
    ├── packet28d.log
    ├── task-registry-v1.json
    ├── watch-registry-v1.json
    ├── task-watch-checkpoint-v1.json
    └── tasks/<task-id>/
```

The exact files may evolve by version. Treat the directory as application-owned
state: use supported inspect, task, artifact, and retention commands instead of
editing it.

## Inspect storage

```bash
packet28 daemon storage inspect --root .
packet28 daemon storage inspect --root . --json --pretty
```

The report distinguishes whole-tree bytes, managed task bytes, artifacts,
events, quarantine, active tasks, protected state, corruption issues, logical
bytes, and allocated bytes. Values are timestamped observations, not product
constants.

## Preview and apply retention

Retention requires at least one explicit bound and defaults to a dry run:

```bash
packet28 daemon storage cleanup \
  --root . \
  --max-age-seconds 604800

packet28 daemon storage cleanup \
  --root . \
  --max-bytes 536870912 \
  --json --pretty
```

To apply a reviewed plan:

```bash
packet28 daemon stop --root .

packet28 daemon storage cleanup \
  --root . \
  --max-age-seconds 604800 \
  --max-bytes 536870912 \
  --apply
```

Cleanup acquires exclusive lifecycle and instance coordination. It protects
active, malformed, aliased, symlinked, special, unreadable, or changed entries.
It stages candidates through a journaled workspace-local quarantine and
recovers interrupted transactions before new cleanup.

Read [Task-store retention](task-store-retention.md) before scheduling cleanup.

## Corruption and recovery

Packet28 distinguishes disposable cache state from authoritative task state:

- authenticated cache checkpoints have a durable backup and checksummed WAL;
- torn final WAL frames can be truncated at the last valid boundary;
- complete checksum failures, gaps, conflicting generations, and changed
  authority fail closed;
- task/watch paired checkpoints use a commit manifest and recovery journal;
- malformed task authority is protected, never treated as an empty registry.

When startup reports corruption:

1. stop retrying writers against the same workspace;
2. preserve `.packet28/` for diagnosis;
3. capture `packet28 doctor --root . --json` and daemon logs;
4. run supported storage inspection;
5. follow the typed recovery error and retention documentation.

Do not delete or rewrite task registries to make the daemon start.

### Registry bytes that match no checkpoint phase

If startup reports that canonical registry bytes do not match a journaled
checkpoint publication phase, or that checkpoint generations disagree, stop the
daemon and classify the state first:

```bash
packet28 daemon stop --root .
packet28 daemon storage repair --root . --json --pretty
packet28 daemon storage repair --root . --apply
```

Repair restores only bytes the checkpoint already authenticates:

- a committed image whose strict, duplicate-key-rejecting re-encoding has the
  exact byte length and BLAKE3 digest in the commit manifest, such as a
  whitespace-only reformat;
- the retained journal base image while that base is still the committed
  checkpoint after an interrupted publication, and only when the registry
  delta WAL replays from the base revision.

Before writing, `--apply` copies every affected registry image and the
checkpoint manifest, journal, and journal images to an owner-only
`.packet28/daemon/registry-repair/repair-*` archive with a hashed receipt. A
durable repair journal makes startup refuse until the repair is complete; rerun
`--apply` after an interruption. The WAL, manifest, and journal are never
deleted or rewritten.

A substantive edit, a missing committed image, or unusable checkpoint metadata
is reported as `NO SAFE RECOVERY` with exit status `1`. Nothing is changed and
edited content is never adopted; restore the file from a backup instead. Corrupt
task event logs are inspected only after the registry resolves.

## Search and index

Setup builds the repository indexes. Check status through:

```bash
packet28 daemon status --root . --json
packet28 doctor --root . --json
```

The map and search indexes publish immutable generations. Incremental overlays
remain tied to their base generation and compact by policy. A stale or
unsupported forced index request fails explicitly instead of returning results
that look current.

## Logs and troubleshooting

The workspace daemon log is normally:

```text
.packet28/daemon/packet28d.log
```

Useful checks:

```bash
packet28 doctor --root . --json
packet28 daemon status --root . --json
packet28 daemon storage inspect --root . --json --pretty
packet28 mcp smoke-test --help
```

Common causes:

| Symptom | Check |
| --- | --- |
| Client cannot connect | readiness and endpoint in `runtime.json`; owner/mode checks |
| TCP migration error | stop and restart the legacy daemon to publish a capability |
| Index not ready | daemon/index status and workspace freshness |
| Retention refuses to run | active daemon, lifecycle lease, unsupported filesystem, or protected corruption |
| Handoff is incomplete | task admission, hook/MCP setup, artifact availability, and broker warnings |
| MCP timeout | upstream process health, bounded inflight work, and stderr/stdout framing |

Do not work around authentication, ownership, or corruption failures with broad
permission changes.
