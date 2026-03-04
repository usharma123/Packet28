# Wire Profiles

## Overview
Packet28 Phase 1 defines bounded machine output profiles for all scoped commands.

Default rule: compact for machine mode.

## Profiles

### `compact`
Default for `--json` and `--json=compact`.

Behavior:
- bounded payload projection
- sampled/truncated arrays
- truncation metadata when bounded

Standard truncation fields when applicable:
- `truncated`
- `returned_count`
- `total_count`

### `full`
Enabled with `--json=full`.

Behavior:
- full payload representation
- same canonical packet semantics/hash

Use cases:
- debugging
- local inspection
- golden test baselines

### `handle`
Enabled with `--json=handle`.

Behavior:
- full packet artifact persisted to disk
- wire output remains compact
- `payload.artifact_handle` provides expansion reference

## Artifact Store Contract
Root:

- `.packet28/artifacts/`

Artifact path:

- `.packet28/artifacts/<handle_id>.json`

Handle fields:

- `handle_id`
- `packet_type`
- `packet_hash`
- `artifact_sha256`
- `path`
- `created_at_unix`

## Expansion Command
Use Packet28 generic fetch command:

```bash
Packet28 packet fetch --handle <handle_id> --json=full
Packet28 packet fetch --handle <handle_id> --json=compact
```

`--json=handle` is coerced to `full` in fetch mode.

## Boundedness Rules
Compact profile must not emit unbounded arrays.

Large domains must emit bounded compact payloads and use handle expansion:

- `map repo`
- `proxy run`
- `diff analyze`
- `test impact`
- `stack slice`
- `build reduce`
- `context assemble`

## Semantic Consistency Rule
`compact`, `full`, and `handle` projections for the same semantic packet must preserve canonical `packet.hash`.
