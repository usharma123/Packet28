# Machine Output Contract

## Canonical Wrapper
All Packet28 Phase 1 scoped machine outputs use one top-level wrapper:

```json
{
  "schema_version": "suite.packet.v1",
  "packet_type": "suite.<domain>.<action>.v1",
  "packet": { "...EnvelopeV1...": true }
}
```

## Wrapper Rules
- `schema_version` is always `suite.packet.v1`.
- `packet_type` identifies the packet contract and parser route.
- `packet` is always canonical `EnvelopeV1<T>` (or profile-projected payload under the same envelope).

## Machine Mode Profiles
- `--json` and `--json=compact`: compact profile (default)
- `--json=full`: full payload profile
- `--json=handle`: compact payload + `payload.artifact_handle`

## Required Packet Fields
Inside `packet`, these fields are always present:

- `version`
- `tool`
- `kind`
- `hash`
- `summary`
- `budget_cost`
- `provenance`
- `payload`

## Debug/Metadata Placement
Wrapper shape is fixed. Extra diagnostics must not drift wrapper shape.

Allowed location for optional debug/audit/cache hints:

- `packet.payload.debug`

## Exit Contract
Packet28 CLI exit semantics in machine mode:

- `0`: command succeeded and policy/gate passed
- `1`: command succeeded but gate/policy/domain failed (including `proxy run` child non-zero)
- `2+`: runtime/config/execution failure

## Stdout/Stderr Discipline
In machine mode:

- `stdout`: one JSON object only (the wrapper)
- `stderr`: warnings/logs/hints/errors

## Compatibility Shim (One Release)
- Legacy `--json` boolean behavior maps to compact profile.
- Legacy `--packet-detail` is accepted and internally mapped to profile/detail behavior.
- Legacy `--report json` is accepted and mapped to compact profile where applicable.
- `--legacy-json` emits previous command-specific top-level shapes for one release.

## Phase 1 Scoped Commands
- `cover check`
- `diff analyze`
- `test impact`
- `stack slice`
- `build reduce`
- `map repo`
- `proxy run`
- `context assemble`
- `guard check`
