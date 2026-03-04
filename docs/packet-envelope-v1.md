# Packet Envelope V1

## Purpose
`EnvelopeV1<T>` is the canonical internal packet frame for Packet28 Phase 1.

Design rule: canonical internally, bounded externally.

Every scoped reducer must construct and return `EnvelopeV1<T>` before any wire-profile projection.

## Required Fields
These fields are required for every packet and are stable across packet types:

- `version`
- `tool`
- `kind`
- `hash`
- `summary`
- `budget_cost`
- `provenance`
- `payload`

## Optional Fields
Optional fields are allowed but must not replace required contract fields:

- `files`
- `symbols`
- `risk`
- `confidence`

## Canonical Hash Rules
`EnvelopeV1::canonical_hash()` uses canonical JSON key ordering and excludes volatile runtime values from hash semantics.

Excluded from hash invariants:

- `hash`
- `budget_cost.est_tokens`
- `budget_cost.est_bytes`
- `budget_cost.payload_est_tokens`
- `budget_cost.payload_est_bytes`
- `budget_cost.runtime_ms`
- `provenance.generated_at_unix`

Hash implementation:

1. Serialize envelope to JSON value.
2. Normalize excluded fields to deterministic neutral values.
3. Canonicalize object key ordering recursively.
4. Hash canonical bytes.

Invariant: same semantic packet => same hash across runs and JSON key order differences.

## Canonical Serialization Rules
- Object keys are canonicalized in stable lexical order before hashing.
- Arrays preserve order as part of semantics.
- Required fields are always present.
- Optional fields may be omitted when empty/`None`.

## Null and Empty Handling
- Required fields are never omitted.
- Optional vectors use omission for empty values where configured.
- Optional scalar/object fields are omitted for `None`.
- Payload-specific nullable fields are packet-type specific and documented in schema registry.

## Budget Metadata Requirements
`budget_cost` is always present and includes:

- `est_tokens`
- `est_bytes`
- `runtime_ms`
- `tool_calls`

Optional estimates:

- `payload_est_tokens`
- `payload_est_bytes`

`with_canonical_hash_and_real_budget()` computes estimate convergence and then re-seals hash.

## Provenance Requirements
`provenance` is always present and includes:

- `inputs`
- `git_base` (optional)
- `git_head` (optional)
- `generated_at_unix`

## Phase 1 Scope
Phase 1 packet types using this envelope contract:

- `suite.cover.check.v1`
- `suite.diff.analyze.v1`
- `suite.test.impact.v1`
- `suite.stack.slice.v1`
- `suite.build.reduce.v1`
- `suite.map.repo.v1`
- `suite.proxy.run.v1`
- `suite.context.assemble.v1`
- `suite.guard.check.v1`
