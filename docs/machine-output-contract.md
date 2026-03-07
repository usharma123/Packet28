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
- `packet` is always canonical `EnvelopeV1<T>` (or profile-projected payload under the same envelope)

## Machine Mode Profiles

- `--json` and `--json=compact`: compact profile (default)
- `--json=full`: full payload profile
- `--json=handle`: compact payload + `payload.artifact_handle`

See `wire-profiles.md` for details.

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

## Preflight Output

`Packet28 preflight` uses a different top-level schema:

```json
{
  "schema_version": "suite.preflight.v1",
  "task": "...",
  "root": "...",
  "profile": "compact",
  "selection": {
    "tags": ["coverage"],
    "anchors": { "paths": [], "symbols": ["AuthService"], "terms": ["coverage"] },
    "selected_reducers": ["cover", "diff", "map", "recall"],
    "skipped": []
  },
  "results": {
    "packets": [
      { "reducer": "cover", "packet_type": "suite.cover.check.v1", "cache_hit": false, "packet": {} }
    ],
    "recall": { "query": "...", "hits": [] }
  },
  "totals": {
    "est_tokens": 4600,
    "est_bytes": 18400,
    "runtime_ms": 45,
    "tool_calls": 4,
    "packet_count": 3,
    "cache_hits": 0,
    "recall_hits": 2,
    "planned_over_budget": false,
    "actual_over_budget": false,
    "over_budget": false
  }
}
```

Each `packet` within `results.packets` follows the standard `suite.packet.v1` wrapper.

## Error Output

When a command fails in machine mode, it emits a structured error:

```json
{
  "schema_version": "suite.error.v1",
  "command": "Packet28 preflight",
  "target": "preflight",
  "message": "No coverage input found",
  "causes": [],
  "retry_hint": null
}
```

## Exit Contract

Packet28 CLI exit semantics:

- `0`: command succeeded and policy/gate passed
- `1`: command succeeded but gate/policy/domain failed (including `proxy run` child non-zero)
- `2+`: runtime/config/execution failure

## Stdout/Stderr Discipline

In machine mode:

- `stdout`: one JSON object only (the wrapper or preflight response)
- `stderr`: warnings/logs/hints/errors

## Scoped Commands

Commands emitting `suite.packet.v1` wrappers:

- `cover check`
- `diff analyze`
- `test impact`
- `test shard`
- `test map`
- `stack slice`
- `build reduce`
- `map repo`
- `proxy run`
- `context assemble`
- `context correlate`
- `context manage`
- `context state append`
- `context state snapshot`
- `context store list/get/prune/stats`
- `context recall`
- `guard check`
- `packet fetch`

Commands emitting `suite.preflight.v1`:

- `preflight`

Commands emitting plain text:

- `agent-prompt`
- `daemon status/start/stop` (non-JSON mode)
