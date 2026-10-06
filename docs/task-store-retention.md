# Task-store inspection and retention

Packet28 exposes supported, timestamped observations for workspace-local task
state. Values from a particular machine or audit are snapshots, not product
constants:

```console
Packet28 daemon storage inspect --root /path/to/workspace
Packet28 daemon storage inspect --root /path/to/workspace --json --pretty
```

The report distinguishes the logical size of the whole `.packet28` tree from
the task state governed by retention. Managed task bytes are the sum of compact
serialized task-registry records, task artifacts, daemon task-event logs, and
durable retention quarantine. If a registry cannot be decoded or read, its raw
file size is counted once as protected managed state; it is never reported as
an empty store. Opaque non-UTF artifact names and malformed event names are
likewise measured as protected state and are never deletion candidates.
Inspection caps task-registry reads at 64 MiB and active-task records at 1 MiB;
larger regular files are measured from metadata, reported as unreliable, and
protect every candidate without allocating their contents.
Before authority JSON is materialized, readers also enforce a depth limit of
64, aggregate value/entry/token budgets, a 65,536-entry per-container limit,
decoded-string accounting, and a 65,536-record registry limit. Duplicate
decoded object keys and trailing input are corrupt authority, not
last-value-wins input.

The retention bound uses these stable logical file bytes. Reports also expose
native allocated filesystem bytes for the whole state tree, combined registry
authority (checkpoint plus authenticated WAL), artifact tree, event tree,
quarantine, and their managed total;
`allocated_bytes_supported` is false when those fields must fall back to
logical size. Quarantine logical/allocated bytes and group count make stranded
transactions visible. Every report includes `observed_at_unix`, a schema
version, record and entry counts, oldest/newest known task timestamps,
active-task count, corruption/safety issues, and before and after metrics.
Historical registry fields that were persisted in Unix milliseconds are
normalized to seconds before age comparison and reporting.

## Plan before applying

Cleanup is a dry run unless `--apply` is present, and at least one explicit
bound is required:

```console
# Select tasks strictly older than seven days without changing state.
Packet28 daemon storage cleanup \
  --root /path/to/workspace \
  --max-age-seconds 604800

# Preview the oldest-first plan needed to reach 512 MiB.
Packet28 daemon storage cleanup \
  --root /path/to/workspace \
  --max-bytes 536870912 \
  --json --pretty

# Apply both bounds after reviewing the plan and stopping packet28d.
Packet28 daemon storage cleanup \
  --root /path/to/workspace \
  --max-age-seconds 604800 \
  --max-bytes 536870912 \
  --apply
```

Boundaries are exact: a task exactly at the age limit is retained, and no
size-based candidate is selected when managed bytes equal the size limit.
Size cleanup selects the oldest eligible candidates first. Applying an
age-based plan may also bring the store below a configured size bound.

## Oversized task records

A task record whose compact JSON exceeds 1 MiB cannot be carried by a registry
page: listings omit it (`omitted_oversized`) so the rest of the store stays
available, but the record itself becomes hard to inspect. Packet28 warns
before that point. Every record at or above 512 KiB is reported:

- by `daemon storage inspect` as `record_size_warnings` (largest first, with
  `warning` or `over_page_limit` levels), with or without a running daemon;
- by the daemon's bounded status (`record_size_warning_count` and up to 32
  `record_size_warnings`), refreshed on every task write;
- in `packet28d.log` once per threshold crossing, with the command to plan
  archival.

`cleanup --max-bytes` is a whole-task, oldest-first deletion bound and requires
a stopped daemon, so it can select older healthy tasks before the oversized one
and cannot run beside a live daemon. Targeted record archival is the online,
evidence-preserving alternative:

```console
# Plan (no changes) for one exact task, or every record of at least 64 KiB.
Packet28 daemon storage archive-record --root . --task-id task-big
Packet28 daemon storage archive-record --root . --min-record-bytes 524288 --json

# Archive while packet28d keeps serving every other task.
Packet28 daemon storage archive-record --root . --task-id task-big --apply

# Retrieve and verify the complete original record.
Packet28 daemon storage show-archived-record --root . --task-id task-big \
  --output task-big.record.json
```

Archival never deletes evidence. The serving daemon writes the complete
original record to `.packet28/task/<task-id>/record-archive/<blake3>.task-record.json`
(owner-only, synchronized, read back, never replaced), then replaces the
registry record with a compact tombstone through the registry WAL. The
tombstone keeps the task identity, lifecycle, event high-water, watch
relationships, recovery links, and every top-level value of at most 4 KiB; an
`archived` pointer records the blake3 digest and exact length of the archive,
the omitted field names and sizes, the reason, and the inspection command. The
event log and task artifacts are untouched, and unselected records are not
rewritten.

Size selection never goes below 64 KiB, and exact targeting refuses records
below that floor. Archival refuses running, cancelling, replan-pending, and
agent-active tasks; the agent's active task; tasks with active watches or
in-progress daemon work; recovery predecessors and successors; and tasks named
by another record's handoff or bootstrap ownership. A per-task maintenance
fence is admitted only when no client request names the task and blocks every
later mutation of it; the daemon lock is never held across archive I/O. An
archived task is terminal: continuation, event appends (including standalone
writers), and re-registration of its identity are rejected; status, listing,
and idempotent cancellation still return the tombstone.

A crash before the tombstone is durable leaves the original record
authoritative and at most an unreferenced archive, which a retry reuses. Once
the tombstone is durable, its pointer is the only authority for the original.
Whole-task retention removes an archive together with the task's registry
record, event log, and artifacts, because the archive lives in that task's
artifact namespace.

## Safety model

Retention resolves the workspace and requires `.packet28` to be a real
workspace-local directory. Symlinked state and candidates are protected, and
symlinks are never followed. Active tasks, storage-key collisions, unreadable
or special entries, malformed registry or active-task state, and candidates
that change after inspection are protected and reported. Supported task
identifiers are injective portable components: non-empty lowercase ASCII
`[a-z0-9_-]+`, at most 242 bytes, excluding DOS device names such as `con`,
`nul`, `com1`, and `lpt1`. Uppercase, punctuation, separators, whitespace, and
Unicode are rejected without normalization. Context-version filename stems
use the same grammar with a separate 250-byte limit because only the `.json`
suffix must fit. Historical invalid spellings are read only to fail closed and
protect state; supported writers never create them.
Task-registry map keys must match their embedded task identifiers. Public
writes reject a mismatch before taking a lifecycle lease or changing state;
historical or manually written mismatches are counted in full as protected
corrupt registry state.

A newly introduced registry identifier cannot adopt an exact pre-existing
artifact directory or event log, and portable case/trailing-dot aliases are
rejected. An exact managed entry is usable only when the previous strict
registry already bound it to the same task. Normal load/change/save cycles
merge known registry fields into the strict existing JSON so unknown
forward-compatible root and record fields survive.

Apply is available only on verified Unix targets and filesystems where the
startup probe confirms atomic no-replace rename support; it otherwise fails
before managed state is moved. The supported concurrency contract requires
every task-registry, active-task, artifact, and event mutation to hold a shared
task-store lifecycle lease for its complete operation. Only the daemon owns a
documented long-lived task-store lease; CLI, MCP, hook, artifact, and spool
producers acquire narrow operation leases and never retain one while waiting
for a child process or serving a session. A conforming daemon uses a gap-safe
startup handoff: it takes the exclusive recovery lease, recovers quarantine,
acquires the long-lived shared lease, and rechecks quarantine while the shared
lease prevents a new cleanup. If cleanup won the exclusive-to-shared
conversion gap, startup repeats recovery before loading mutable state.
Conflicted recovery state refuses daemon readiness. This handoff participates
even when `.packet28` did not exist at the first check.

Task-storage admission is also operation-scoped. Each artifact or event
operation takes lifecycle ownership, authenticates the anchored registry lock,
binds the exact registry inode plus the complete validated record encoding,
performs one bounded operation, and revalidates that fingerprint before
return. No public token can retain this authority or survive same-identifier
removal and re-admission. `TaskEnsure` is the idempotent daemon boundary for a
producer that needs a fresh task: it validates the portable identifier and
durably saves the candidate registry before committing it to daemon memory or
returning. Supported producers publish active state and task artifacts only
after that boundary and repeat the admission step at most once if retention
removed an otherwise empty record before the next operation began. They never
blindly retry a write whose durability is uncertain.

Managed artifact paths returned in protocol payloads are display metadata, not
filesystem authority. Producers and consumers use the admitted artifact
facade while its lifecycle lease, anchored registry lock, current exact task
admission, and retained state/task/location descriptors remain live.
Single-file reads and metadata distinguish an authenticated absence from an
unsafe entry. Version-directory snapshots bound entry count, each file, and
aggregate bytes; require portable `.json` names; sort deterministically; and
reject the whole snapshot for unknown entries, invalid UTF-8, links, special
files, identity changes, or a directory change between authenticated passes.
Dashboard, compact/raw-fetch, MCP handoff discovery, broker rendering, hook
context, and task-status consumers never reopen these display paths.

Lifecycle, daemon-instance, registry, active-task, and event locks retain the
locked descriptor and reauthenticate its exact filename, identity, regular
file type, and single-link count immediately after `flock` and again before
unlock. A mismatch before bytes are changed is a zero-mutation failure. A
mismatch after a data or directory barrier returns
`StorageMutationAuthorityLost`, which explicitly means the mutation may have
committed and must not be retried blindly.

Every explicit apply takes the corresponding exclusive lease before its only
full task-store scan, even when no candidates will remain. Under that lease it
loads task state, recovers quarantine, removes strictly named stale
task-registry atomic-write files under the registry lock, and builds the policy
plan before cleanup and reporting. If a writer wins the race, apply fails
without mutation. If cleanup wins, new writers wait until cleanup releases the
lease, so data created after final revalidation cannot be deleted by the
in-flight plan. A separate
exclusive daemon-instance lease is required to prevent two daemons from
loading and publishing independent state for one workspace. The daemon takes
that lease before recovery and retains it for its lifetime. After winning the
exclusive lifecycle lease, retention must non-blockingly take the instance
lease in shared mode before any scan or mutation; if daemon startup owns it,
retention releases lifecycle ownership without changing task state. Holding
the shared instance gate also prevents a new daemon from entering the
lifecycle conversion window during cleanup. The readiness marker remains an
additional compatibility check, so stop `packet28d` before applying a reviewed
plan.

Each candidate is re-scanned, checked by filesystem identity and a nanosecond
metadata-tree fingerprint, moved into a workspace-local quarantine, and
checked again for active or changed state. Quarantine creation, staging,
rollback, and deletion use retained directory descriptors with no-follow
`openat`, `renameat`, and `unlinkat` operations. Deletion first isolates the
verified inode under a no-replace tombstone name and rechecks its identity; a
replaced path or directory is left protected rather than unlinked.
Quarantine and transaction directories must remain owned by the effective
user with mode `0700`. Newly created public state directories are corrected to
their exact requested mode before parent publication. Existing authority files
must be owner-held, single-link regular files on the retained filesystem.
Group/other-writable authority or an extended ACL is an authenticity failure
and is rejected rather than silently repaired; a still-authentic owner-only
file may have its mode corrected to `0600` through its verified descriptor and
synchronized. Atomic temporary and lock files use exclusive, no-follow opens
and are corrected to owner-only mode before synchronization, so a restrictive
process umask cannot publish unreadable durable state.
Cross-directory renames synchronize the destination directory before the
source; a crash between those operations can leave recoverable duplication
rather than a durable source removal without a durable destination.
Regular-file recovery recognizes two same-inode links, durably preserves the
original, and removes the verified quarantine duplicate. Duplicate directory
parent links are not a supported filesystem state and remain a fail-closed
recovery conflict. On Apple platforms, data-file barriers use `F_FULLFSYNC`
after `fsync`; an unavailable or failed full barrier is reported instead of
being treated as power-loss durability. Directory namespace barriers use
`fsync`, because `F_FULLFSYNC` is not a portable directory operation across
macOS filesystems.
The implementation probes atomic no-replace rename support before moving
managed state and fails closed when the filesystem cannot provide it.

POSIX does not provide an identity-conditional `unlinkat`: another
uncooperative process running as the same user could still race the final
identity check and unlink syscall. Packet28 narrows that syscall boundary by
holding the exclusive lifecycle lease, using an owner-only directory and a
fresh isolated tombstone name, retaining the parent descriptor, and checking
the device/inode immediately before the final unlink. Deterministic
replacement tests exercise swaps before that final check for both files and
directories; they do not claim to eliminate the irreducible interval between
the check and syscall. The lifecycle lease is the required coordination
boundary for every supported writer; arbitrary same-user processes that bypass
it are outside the supported concurrency contract.

Inspection has explicit resource bounds: one recursive scan visits at most
65,536 entries to a maximum depth of 64, and reports at most 1,024 distinct
issues with a final truncation sentinel. Crossing a scan bound fails closed
without returning partial metrics or creating quarantine state. Quarantine
recovery bounds group count and immediate entries independently, while
capability-relative measurement and deletion enforce the same depth and entry
budgets. Startup recovery uses only a workspace/state identity anchor before
the lease rather than recursively scanning unrelated task content, and the
exclusive-to-shared handoff aborts after a bounded number of adversarial
retries. These limits are safety ceilings, not retention-policy bounds.

Task-event storage follows the same authority boundary. A first append is
allowed only after the exact identifier is present in the strict durable task
registry. The append retains the anchored registry lock and exact admission
fingerprint so authority cannot change between that check and the data
barrier. Event paths are opened relative to retained no-follow directory
descriptors; case aliases, symlinks, special files, cross-device entries, and
multiply linked logs are rejected. First-file publication is serialized on a
fresh retained directory descriptor, the complete JSON line is appended under
an exclusive authenticated file lock, and the data barrier completes before
the final authority check and unlock.

The event log, not the in-memory task registry, allocates the next sequence.
While holding that exclusive event-file lock, append-next streams every
complete frame with bounded per-line memory, requires the exact task identifier
and contiguous sequence, assigns `tail + 1`, appends, and synchronizes before
registry publication.
Independent daemon processes therefore cannot allocate the same sequence.
The registry's `last_event_seq` is a derived high-water mark: startup and
write admission advance a lagging value from the authenticated log before the
next event is allocated. A registry value ahead of its log is corruption and
is never used to skip a sequence.

A crash may leave one final non-newline suffix. Append-next boundedly validates
the complete prefix, truncates and synchronizes only that incomplete suffix,
then appends its replacement frame. A newline-terminated malformed frame,
cross-task frame, duplicate, conflict, or gap is durable corruption: both tail
inspection and append-next return a typed error with the log bytes unchanged.

Daemon persistence has one lifecycle owner. Callers submit keyed task/watch
deltas while the daemon state mutex is held; no caller clones or serializes the
complete registries there. A monotonically increasing revision orders each
mutation. The owner coalesces later changes to the same key, appends one
checksummed WAL range, and synchronizes it after the mutex is released. Request
barriers flush through a requested WAL revision. Full paired checkpoints use a
fixed debounce that coalesces bursts without extending the deadline
indefinitely.

A failed WAL append retains the exact pending range for retry. A failed full
checkpoint leaves the already-synchronized WAL authoritative and retries the
checkpoint without re-appending the delta. Orderly shutdown checkpoints a
trailing WAL; a timed-out shutdown detaches rather than blocking forever, but
the worker retains its task-store lifecycle lease until it actually exits.

Each paired registry checkpoint first synchronizes exact copies of the prior
committed task and watch images and a descriptor journal. It publishes the new
watch and task images before atomically replacing a compact commit manifest
that binds their generation, lengths, and BLAKE3 digests. The manifest is the
commit point. If a process stops earlier, startup accepts only the precise
base/target combinations recorded by the journal and recovers the prior pair.
Arbitrary mixed generations, malformed manifests, and altered journal images
remain corruption. Existing generation-only and generation-free registry
pairs migrate at the explicit startup checkpoint before readiness.

The registry WAL records a contiguous first/last revision, an atomic keyed
task/watch delta, observable watch insertion order, and a checksum. Exact-tail
retries with identical bytes are idempotent; a changed retry, complete
corruption, revision gap, invalid relationship, or symlinked WAL fails closed.
Only a final incomplete frame may be repaired, by truncating and synchronizing
that suffix. A successful paired checkpoint binds the replayed revision and
resets the WAL after the checkpoint commit.

On verified Unix targets, inspection, planning, revalidation, removal,
recovery, and accounting all use the same checkpoint-plus-WAL authority.
Portable read-only inspection currently measures WAL storage but treats the
checkpoint as its record authority; retention apply remains unsupported there.
A schema-v2 committed retention journal binds its task-record values to both
the replayed registry revision and paired-checkpoint generation that were
revalidated. Recovery may finish an already-absent removal after authority
advances, but it never removes a present task that was re-admitted—even with
byte-identical record data—through either a newer WAL revision or a
same-revision checkpoint replacement. A legacy schema-v1 committed journal
or a schema-v2 journal created from an unpaired legacy checkpoint has no
checkpoint-generation fence: recovery completes it only when its records are
already absent and otherwise reports a conflict for operator review. Recovery
also refuses an absent-record completion if the current WAL revision or paired
checkpoint generation is older than the journaled authority.

For task events, subscriber publication follows the synchronized event-log
append and the in-memory high-water update. Before appending an event, the owner
first synchronizes every causally prior registry revision; if the event append
then fails, those registry deltas remain replayable without inventing an event.
The derived high-water update is staged as a later task delta and may be
checkpointed later. This does not make the registry a second sequence
authority: after a crash, startup composes the checkpoint and WAL, strictly
streams every authenticated event log, advances lagging high-waters, and
synchronously checkpoints reconciliation before serving requests. A registry
ahead of its log, or a nonzero high-water with no log, fails startup as
corruption. The event lane spans the causal WAL barrier, append
acknowledgement, high-water update, delta submission, and bounded subscriber
sends, so concurrent publications remain sequence ordered without holding the
daemon state mutex during filesystem I/O.

The paginated event reader returns at most 4 MiB and 4,096 decoded frames per
call; a line is capped at 1 MiB. A nonzero cursor also reads at most one bounded
predecessor frame to verify continuity across pages. Cursors must come from a
prior successful page. The reader uses a verified descriptor with
nonblocking/no-follow opens, never allocates an entire log, and leaves a
trailing partial line at the caller's prior cursor. Malformed or semantically
invalid complete JSON, a wrong task identifier, duplicate, or gap is a typed
error before any cursor is returned. A failed admission or final
descriptor/name postcheck discards the buffered length, page, or whole-log
result. The compatibility whole-log helper is capped at 64 MiB and 65,536
frames; larger consumers must paginate.

Inspection reads the task registry and active-task pointer as one authority
view while holding the existing registry lock in shared mode. It never creates
a missing state directory, registry lock, registry file, or active pointer
merely to inspect an empty workspace. An active pointer absent from the same
reliable registry snapshot is inconsistent authority: the pointer is cleared
from the report, the view becomes unreliable, and every candidate is
protected. A terminal `Cancelled` task remains durably registered and keeps
its event history readable; it is inactive and therefore becomes eligible for
normal retention policy instead of being deleted as an untracked side effect
of cancellation.

Every group contains a versioned, normalized-component journal that is
synchronized before staging. `precommit` groups restore by atomic no-replace
rename and never overwrite a source recreated by a writer. The durable
`committed` marker is written while the registry's exclusive lock is held,
before matching records are removed. Recovery is required before mutable
daemon state is loaded and runs at the start of explicit apply: precommit
groups are restored, committed groups finish registry removal and deletion,
and corrupt/conflicting groups remain measured and reported. A committed
marker that became visible but whose parent-directory sync failed is still the
point of no return and is completed by recovery; it is never rolled back
in-process. A committed group is fully validated before its registry records
are changed. Journals derive
their only path authority from a validated storage key and a closed component
kind, bind raw registry records to that key, preserve unknown record fields,
and cap bytes, record count, and component count before recovery acts. Compact
journal encoding uses a bound derived from twice the 64 MiB accepted registry
size plus a fixed structural envelope, so every single record accepted by the
writer remains representable in a recovery journal.
Traversal, unknown fields, duplicate kinds, unexpected group entries, and
identity substitutions are rejected. An owner-only group that is completely
empty because the process stopped just before journal publication or just
after journal deletion is safe to remove by verified identity; any non-empty
group without a valid journal remains protected. Strictly shaped, regular
journal-write, no-replace-probe, and deletion transients are recoverable in
their documented crash windows; lookalikes, non-regular entries, and unknown
residue remain protected.

Every precommit failure performs an explicit rollback. An action is `skipped`
only after rollback is confirmed; rollback or cleanup failure produces a
`failed` action, leaves recovery authority durable, and makes byte accounting
conservative. Each action reports planned, removed, and remaining logical
bytes. `byte_accounting_reliable=true` means the
removed/remaining split was measured through the retained directory
capabilities; otherwise the report
uses the conservative full planned size as remaining and zero as removed.
`action_byte_accounting_reliable` is true only when every action has a reliable
split. Aggregate removed bytes therefore include measured partial progress
even when the task outcome is `failed`, while `failed_logical_bytes` reports
the planned bytes still retained by failed actions. A later failure cannot
hide candidates already removed or restored earlier in the same plan.

The JSON format is the automation interface. Consumers should check
`schema_version`, retain the observation timestamp with any recorded values,
and surface `issues` plus `remaining_over_limit_bytes` rather than assuming a
requested bound was achievable. `final_rescan_reliable=false` means cleanup
made per-action progress but the post-apply store rescan failed; in that case,
`metrics_after` is the last reliable pre-action snapshot and must not be
presented as a fresh observation.

An apply command still emits its complete JSON or text report before returning
a non-zero status when an action failed, recovery found a conflicted group, the
final rescan failed, or byte-progress accounting became unreliable. Protected
or skipped candidates remain reportable safe outcomes and do not by themselves
change the command status.

Schema version 2 adds quarantine metrics, explicit failed-action and recovery
accounting, and the per-action removed/remaining split. The Rust deserializer
accepts schema-version-1 reports: absent numeric fields become zero and absent
reliability flags become false. Callers constructing report structs directly
must initialize the version-2 fields, and exhaustive matches over
`RetentionOutcome` must handle `failed`.

A [timestamped remediation-worktree snapshot](task-store-metrics/20260728/README.md)
records the volatile evidence separately from this interface contract.

## Implementation ownership

The private `retention::scan` module owns read-only traversal, depth and entry
budgets, metadata fingerprints, and byte/count aggregation. Path inspection and
capability-relative revalidation use the same aggregation rules. Retention
keeps candidate selection, authenticated registry snapshots, lease admission,
and quarantine mutations. A successful scan supplies measurements; it does not
authorize deletion or replace revalidation under the retention lease.

## Corrupt event-log recovery

Startup quarantines at most 64 corrupt admitted task event logs per attempt.
The strict event reader still rejects malformed frames, gaps, cross-task frames,
and oversized frames. Filesystem, namespace, lease, and lock errors still fail
closed, and healthy tasks are unchanged.

A damaged task never continues under its own identity. Resetting its sequence
and appending again would restart at 1 and hide the new events from any
subscriber or MCP session whose cursor was already past the old high-water.
Instead, one WAL delta, written before any rename:

- fences the damaged task: lifecycle `Cancelled`, high-water 0 (its log is
  absent after the move), a `last_error` explanation, and a `superseded_by`
  link;
- admits a new successor task, `<id>-recovered-<n>` (digest-shortened for long
  identifiers, skipping registered, aliasing, and pre-existing namespaces),
  with a `recovered_from` link;
- reserves an exact unused quarantine name,
  `*.events.jsonl.corrupt-<timestamp>[-<collision>]`, in both links.

Both links record the predecessor, successor, integrity failure, prior
high-water, recovery time, and reserved file name, so either record still
identifies the evidence if retention later removes the other. The log is then
moved to exactly the reserved name with a no-replace rename. If startup stops
before the rename, the next attempt reuses the durable link and retries the
same name without creating another successor. An occupied reserved name fails
closed rather than being overwritten or replaced by an unrecorded name. The
quarantined file is an unrecognized event entry, so retention reports and
protects it and never deletes it.

Every event writer refuses the superseded identity with `TaskSuperseded`. The
standalone and legacy writers check the replayed checkpoint+WAL image under the
registry lock, because the link is first durable only in the WAL; a torn final
WAL frame makes them fail closed until daemon startup repairs it. The daemon
rejects continuation requests (sequence submission, hooks, broker reads and
writes, handoff waits, agent launch, and subscriptions) for a superseded task
with an error that names the successor. Status, task listings, watch listings,
and idempotent cancellation still show the fenced record and its link. Restart
reconciliation treats the fenced task as cancelled: it removes its watches,
restores no replan, and still refuses to start while a persisted agent process
group is live rather than signal an unauthenticated PID.

The successor is seeded only from trusted state, never from the quarantined log.
From the predecessor's authenticated registry record it inherits the objective
request (without a stale `since_version`), linked decisions, resolved
questions, question texts, and the latest ready or consumed handoff descriptor.
That descriptor keeps the predecessor `task_id`, which names the namespace
owning its untouched artifact, and handoff reuse loads the artifact from that
owner. Startup publishes one constant-size recovery link for each successor.
Snapshots read predecessor packet-cache events in place using the authenticated
registry lineage, retaining each artifact owner. No aggregate history copy is
written, so histories exceeding a cache-record limit cannot block startup.
Artifact fetches preserve explicit task IDs and can search authenticated
predecessors; ambiguous inherited handles require the original owner ID.
Retention protects namespaces referenced by recovery links and handoff
descriptors. Lifecycle, watches, sequence, agent process, hook session, and
context-version pointers are not inherited.

Claude Code hooks, MCP sessions (including Codex integrations), the reducer runner, and
`packet28-agent` resolve a superseded identifier through its links after the
daemon is ready and adopt the successor as the active task. The daemon
checkpoints the links before publishing readiness. Wrappers and MCP task
selection ensure daemon readiness before resolving a continuation, including
the first cold start. Idle MCP notification sessions deliver a
`packet28.task_recovered` receipt before transferring tracking to the successor
with its own sequence and byte cursor. A strict read failure is retried only
after readiness confirms a reciprocal recovery link to a different identity.

The root cause of the corrupt logs observed so far is not established. One
unconfirmed candidate is a log written before sequence allocation moved into the
event log: earlier binaries appended caller-assigned sequences from the
in-memory registry high-water before persisting the registry. A crash between
those writes could leave a stale high-water for the next startup. This source
evidence does not establish the cause of any particular damaged log.
