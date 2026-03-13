# Machine Output Contract

## Canonical Wrapper

All Packet28 machine-mode commands emit one top-level wrapper:

```json
{
  "schema_version": "suite.packet.v1",
  "packet_type": "suite.<domain>.<action>.v1",
  "packet": { "...EnvelopeV1...": true }
}
```

## Wrapper Rules

- `schema_version` is always `"suite.packet.v1"`
- `packet_type` identifies the packet contract and parser route
- `packet` is always canonical `EnvelopeV1<T>` or a profile-projected payload under the same envelope

## Machine Mode Profiles

- `--json` and `--json=compact`: compact profile
- `--json=full`: full payload profile
- `--json=handle`: compact payload plus `payload.artifact_handle`

See `wire-profiles.md` for details.

## Error Output

When a command fails in machine mode, it emits a structured error:

```json
{
  "schema_version": "suite.error.v1",
  "command": "Packet28 diff analyze",
  "target": "diff analyze",
  "message": "No coverage input found for diff gate",
  "causes": [],
  "retry_hint": null
}
```

## Exit Contract

- `0`: command succeeded and policy/gate passed
- `1`: command succeeded but gate/policy/domain failed
- `2+`: runtime/config/execution failure

## Stdout/Stderr Discipline

In machine mode:

- `stdout`: one JSON object only
- `stderr`: warnings, logs, hints, or structured errors

## Scoped Commands

Commands emitting `suite.packet.v1` wrappers include:

- `cover check`
- `diff analyze`
- `test impact`
- `test shard`
- `test map`
- `stack slice`
- `build reduce`
- `context assemble`
