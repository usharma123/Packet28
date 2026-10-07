# PER-03 incremental index publication

Date: 2026-07-28

This experiment validates the decision to replace whole-snapshot rewrites with
immutable base generations and incremental overlay segments in `mapy-core` and
`packet28-search-core`.

## Decision gate

Keep the incremental architecture only when the median single-path update:

1. preserves the existing search/index results (covered by the crate regression
   suites);
2. reduces bytes published per update; and
3. does not regress elapsed time on the deterministic release workload.

The original 2026-07-28 measurements below passed all three conditions.
Compaction is measured separately because its cost is intentionally amortized
over eight segment publications. The final-base security revalidation in
[BR-17 publication-fence revalidation](#br-17-publication-fence-revalidation)
supersedes that elapsed-time result for the current integrated tree. The
current Mapy correctness and published-byte gates pass, but the elapsed-time
gate fails. The architecture is retained for its bounded durable publication;
no Mapy latency improvement is claimed.

## Reproduce

```text
CARGO_TARGET_DIR=/tmp/packet28-per03-bench-target \
  cargo run --offline --release --locked -p mapy-core \
  --example per03_incremental_index
python3 benchmarks/per-03-incremental-index/verify.py
```

Environment:

- Darwin 24.6.0 arm64
- rustc 1.93.1 (01f6ddf75 2026-02-11)
- cargo 1.93.1 (083ac5135 2025-12-15)
- implementation commits `948aae6` (mapy), `832e40f` (regex),
  `bd9fe24` (writer ownership, integrity, and retention), and `b474315`
  (daemon adoption)

The example creates and removes isolated repositories under the system temporary
directory. Each reported duration is the median of five updates. The summary
table is the median of three independent example invocations.

## Workload and evidence boundary

The mapy fixture contains 1,024 deterministic Rust files. After seeding 96
changed paths, each measured update modifies one path. `mapy_legacy` executes
the former operation directly: clone the complete `RepoIndexSnapshot`, call
`update_repo_index`, serialize the complete snapshot, and rewrite the complete
artifact. `mapy_incremental` indexes and publishes only the supplied path.

The regex fixture contains 256 deterministic Rust files with 96 live overlay
paths. The former implementation rebuilt all live overlay documents for every
change. `regex_full_overlay_model` reproduces that work by supplying all 96 live
paths; it is a controlled work-model, not a checkout of the historical binary.
`regex_incremental` supplies only the changed path. Correctness parity with the
reducer, deletion/tombstone behavior, corruption recovery, retained-reader
ownership, and threshold compaction are enforced by the search-core tests.

Publication bytes are the sum of new or changed files after an update. Unchanged
immutable base and segment artifacts are deliberately excluded. Mapy's legacy
value is the complete encoded snapshot size.

## Results

| Path | Median update (µs) | Published bytes | Time reduction | Byte reduction |
| --- | ---: | ---: | ---: | ---: |
| Mapy whole snapshot | 9,062 | 3,490,797 | baseline | baseline |
| Mapy incremental generation | 3,145 | 5,027 | 65.29% | 99.86% |
| Regex full-overlay work-model | 261,103 | 225,693 | baseline | baseline |
| Regex incremental segment | 7,712 | 27,383 | 97.05% | 87.87% |

The three run medians were:

```text
mapy whole snapshot:       9062, 9512, 6315 µs
mapy incremental:          3245, 3145, 2383 µs
regex full-overlay model:  255386, 425949, 261103 µs
regex incremental:         5979, 9163, 7712 µs
```

The slowest observed incremental update remained faster than the fastest
corresponding whole-rebuild observation.

## BR-17 publication-fence revalidation

Date: 2026-07-30

The benchmark was rerun at baseline `43753c58` after the blind review found
that the incremental writer reloaded, hashed, and decoded the complete
published base and every segment while holding the publication lock. The
benchmark also used `saturating_sub`, so a regression was printed as a
`0.00%` reduction.

Rebased commit `a9e9c5f6` replaces that reload with a generation/digest
comparison under the publication lock. Follow-ups `a150f514` and `252c8dbe`
authenticate and pin the current and next generation records plus every
referenced artifact through publication, retain the already authenticated
in-memory generation, and prune only from authenticated records. Stable Unix
file identity avoids unchanged-artifact hashing; platforms without a stable
change token conservatively rehash retained artifacts. Commits `161ea15a` and
`72a53bea` apply the same fence to direct, policy-change, and prepared/shared
rebuilds, authenticate the exact bounded bytes written to both manifest files,
restore both pre-publication files on failure, and carry the policy update's
original authenticated prestate across its full scan. Benchmark deltas are
signed `(after - before) / before` values, where a positive time delta is a
regression.

### Historical pre-state-fs result

The exact release command under [Reproduce](#reproduce) was originally run
three times before and after the source-stack change:

| Revision/path | Invocation medians (µs) | Median (µs) | Delta versus paired legacy |
| --- | --- | ---: | ---: |
| `43753c58` whole snapshot | 6,506; 5,251; 5,466 | 5,466 | baseline |
| `43753c58` incremental generation | 16,965; 15,732; 18,205 | 16,965 | +210.37% |
| `fc134fe1` whole snapshot | 5,742; 4,871; 4,836 | 4,871 | baseline |
| `fc134fe1` incremental generation | 4,345; 4,764; 5,651 | 4,764 | -2.20% |

Those historical measurements preceded the descriptor-anchored
`packet28-state-fs` publication substrate. In that evidence boundary, the
incremental invocation median improved from 16,965 to 4,764 µs (-71.92%, or
3.56× faster).

### Final-base durable-state decision

The current source was last measured on 2026-10-06 at the uncommitted
scope-closure integration tree `be3136d0`, after `packet28-state-fs` gained its
additive rotating-log module and the search-core full rebuild began attesting
stable dirty workspaces; the earlier `e3c6de30` snapshot had extended the byte
snapshot from `mapy-v1/` to the complete `.packet28/index/` publication scope.
That correction includes the changed
`.mapy-v1.generation-high-water.json` durability leaf. State publication
synchronizes temporary files and parent directories. Three exact, uncontended
release invocations reported:

| Revision/path | Invocation medians (µs) | Median (µs) | Delta versus paired legacy |
| --- | --- | ---: | ---: |
| current whole snapshot | 5,222; 5,039; 5,075 | 5,075 | baseline |
| current incremental generation | 79,892; 59,650; 66,826 | 66,826 | +1,216.77% |

The per-invocation deltas were +1,429.88%, +1,083.69%, and +1,216.55%.
Therefore the elapsed-time decision gate fails on the durable state base.
The whole-snapshot comparator uses a plain `fs::write`; the incremental
transaction durably publishes the high-water mark, immutable segment,
authenticated generation record, and current/previous manifests. This
comparison is valid for detecting wall-clock regression, but not for claiming
equivalent power-loss behavior.

The decision is explicit: retain the incremental architecture for correct,
bounded, authenticated publication, and reject the Mapy latency-improvement
claim. Durability-barrier coalescing is not adopted without a separate
power-loss-equivalence experiment. That prevents a benchmark target from
weakening descriptor anchoring, write-before-manifest ordering, writer-lease
authentication, or publication authentication.

Each run published 5,367 bytes across the complete durable publication scope
versus 3,490,797 bytes for the whole-snapshot model, a 99.85% reduction. The
additional 44 bytes versus the prior 5,323-byte claim are the changed
generation high-water file that the old `mapy-v1/`-only snapshot omitted. Each
run reported the same work:

```text
publication_metadata_bytes_decoded=2711
repository_artifact_bytes_decoded=3439
repository_artifacts_decoded=1
repository_artifact_bytes_hashed=6878
repository_artifact_metadata_checks=42
changed_paths_considered=1
```

The focused invariant seeds four retained segments, measures the new segment's
persisted byte length independently, and asserts exactly one decoded artifact,
exactly that many decoded bytes, and exactly twice that many hashed bytes on
the Unix stable-identity path (new digest plus persisted-byte authentication).
It also asserts that the segment is smaller than the retained base, so the
bounded-work claim cannot pass merely because both instrumentation and the
expected value are zero. The policy-change regression independently asserts
one full-base decode and two full-base hashes.

Dedicated regressions cover detached writer-lock replacement before current,
after current, and after previous publication, guarded prepared rollback,
guarded pruning, guarded clear, bounded canonical generation records,
same-size base corruption with restored mtime, current/previous
manifest-output replacement, policy-scan prestate mutation, public manifest
policy/count/identity tampering, and prepared publication mutation. Detached
writers fail without publishing, rolling back, pruning, or clearing over the
successor generation.

## Compaction cost

The eighth segment publication compacts only live overlay entries and retains
the immutable base generation.

| Path | Median compaction (µs) | Published bytes |
| --- | ---: | ---: |
| Mapy | 72,841 | 328,862 |
| Regex | 410,269 | 223,531 |

Current Mapy compaction observations were 72,841, 65,917, and 87,581 µs
(median 72,841 µs); the published-byte scope includes the generation high-water
leaf. Current regex observations were 409,399, 431,447, and 410,269 µs
(median 410,269 µs). The historical regex observations were 301,916, 348,383,
and 307,572 µs. The regex compaction
cost is approximately one former full-overlay update, but occurs once per eight
segment publications; ordinary updates retain the measured incremental
behavior.

## Daemon adoption

The packet28d regression benchmark exercises the production orchestration path:
a 256-file full build followed by five one-file updates through
`perform_incremental_index_update`. It asserts shared base ownership, records
all changed files beneath `.packet28/index`, and fails if the legacy aggregate
snapshot is written.

```text
CARGO_TARGET_DIR=/tmp/packet28-per03-daemon-final \
  cargo test --offline --locked -p packet28d \
  index::tests::daemon_incremental_publication_benchmark -- --nocapture
```

This is a debug-profile integration measurement, so elapsed time is diagnostic;
the artifact-size invariant is the decision evidence. The original source-stack
measurements were:

| Path | Median update (µs) | Published bytes | Reference bytes | Byte reduction |
| --- | ---: | ---: | ---: | ---: |
| Daemon incremental publication | 29,767 | 27,048 | 1,410,270 initial generation | 98.08% |

The three benchmark invocations reported 29,536, 44,033, and 29,767 µs. All
three published 27,048 bytes for the measured update and reported
`legacy_snapshot_written=false`.

On the current BR-17 repair worktree, three exact debug-profile invocations
reported 319,263, 336,050, and 338,329 µs (median 336,050 µs). All three
published 27,966 bytes, retained a 1,410,899-byte initial generation, and
reported `legacy_snapshot_written=false`. The current artifact-size reduction
is 98.02%; the debug elapsed time is recorded for reproducibility and is not
the release wall-clock decision gate.

## Integrity and ownership validation

The final measurements include BLAKE3 artifact digests, a repository-local
exclusive writer lock, generation compare-and-swap, and current-plus-previous
artifact retention. Regression tests cover stale concurrent writers,
structurally valid byte mutation, traversal attempts, bounded retention,
restart recovery, deletion tombstones, retained readers, and daemon clear.
These checks deliberately preserve the evidence boundary: pruning is
best-effort after publication. The original benchmark did not exercise durable
state publication; the final-base revalidation includes the state layer's file
and parent-directory synchronization, but this timing experiment is not itself
a power-loss recovery proof.

## Interpretation

The result supports immutable generations with manifest-last publication,
bounded overlay segments, and reader-owned `Arc` layers. Recovery trusts only
the current or explicitly retained previous manifest and never promotes orphan
artifacts. On the final base, descriptor-anchored durable publication makes the
wall-clock objective partial; retaining the architecture is not evidence that
the elapsed-time decision gate passed.
