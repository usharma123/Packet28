# Wire Profiles

## Overview

Packet28 defines bounded machine output profiles for all commands that emit packets.

Default rule: compact for machine mode.

## Profiles

### `compact`

Default for `--json` and `--json=compact`.

Behavior:
- Bounded payload projection
- Sampled/truncated arrays
- Truncation metadata when bounded

Standard truncation fields when applicable:
- `truncated`
- `returned_count`
- `total_count`

### `full`

Enabled with `--json=full`.

Behavior:
- Full payload representation
- Same canonical packet semantics and hash

Use cases:
- Debugging
- Local inspection
- Golden test baselines
- Agent consumption when compact is insufficient

### `handle`

Enabled with `--json=handle`.

Behavior:
- Full packet artifact persisted to disk
- Wire output remains compact
- `payload.artifact_handle` provides expansion reference

## Artifact Store Contract

Root: `.packet28/artifacts/`

Artifact path: `.packet28/artifacts/<handle_id>.json`

Handle fields:
- `handle_id`
- `packet_type`
- `packet_hash`
- `artifact_sha256`
- `path`
- `created_at_unix`

## Expansion Command

```bash
Packet28 packet fetch --handle <handle_id> --json=full
Packet28 packet fetch --handle <handle_id> --json=compact
```

`--json=handle` is coerced to `full` in fetch mode.

## Boundedness Rules

Compact profile must not emit unbounded arrays.

Domains with large payloads must emit bounded compact payloads and use handle expansion:

- `map repo`
- `proxy run`
- `diff analyze`
- `test impact`
- `stack slice`
- `build reduce`
- `context assemble`

Compact `map repo` inlines `path`/`name` context in ranked entries so agents do not need to join opaque indices against envelope refs.

## Semantic Consistency Rule

`compact`, `full`, and `handle` projections for the same semantic packet must preserve the canonical `packet.hash`. The hash is computed before profile projection.

## Recall Profile

Recall entries follow the same projection rules as other reducer packets:
- `compact` and `handle`: low-signal metadata such as `created_at_unix` and `matched_tokens` may be stripped
- `full`: all supported fields are preserved
