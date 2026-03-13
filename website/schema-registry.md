# Schema Registry

## Purpose

Packet28 makes packet contracts explicit in code and schema artifacts.

Code registry source: `crates/suite-packet-core/src/registry.rs`

Schema artifacts:

- `schemas/packet-wrapper/suite.packet.v1.schema.json`
- `schemas/packet-types/<packet_type>.schema.json`
- `schemas/snapshots/<packet_type>/{compact,full,handle}.json`

## Registered Packet Types

### Reducer Packets

| Packet Type | Reducer | Description |
| --- | --- | --- |
| `suite.cover.check.v1` | `covy` | Coverage quality gate result |
| `suite.diff.analyze.v1` | `diffy` | Diff analysis against quality gate |
| `suite.test.impact.v1` | `testy` | Impacted tests from git diff |
| `suite.stack.slice.v1` | `stacky` | Deduplicated stack trace failures |
| `suite.build.reduce.v1` | `buildy` | Grouped build diagnostics |
| `suite.map.repo.v1` | `mapy` | Ranked repo structure map |
| `suite.proxy.run.v1` | `proxy` | Safe command execution output |

### Context Packets

| Packet Type | Component | Description |
| --- | --- | --- |
| `suite.context.assemble.v1` | `contextq` | Merged bounded context from multiple packets |
| `suite.context.correlate.v1` | `contextq` | Cross-packet correlation insights |
| `suite.context.manage.v1` | `contextq` | Budget-aware context management guidance |
| `suite.guard.check.v1` | `guardy` | Policy evaluation result |

### Agent State Packets

| Packet Type | Component | Description |
| --- | --- | --- |
| `suite.agent.state.v1` | `agenty` | Agent state event (focus, decision, checkpoint) |
| `suite.agent.snapshot.v1` | `agenty` | Agent state snapshot |

### Meta Packets

| Schema Version | Description |
| --- | --- |
| `suite.packet.v1` | Standard packet wrapper |
| `suite.error.v1` | Structured error output |

## Registry Contract

Each packet type defines:

- Required payload fields
- Optional payload fields
- Boundedness rules (compact profile truncation behavior)

## Versioning Rules

- Wrapper schema is versioned (`suite.packet.v1`)
- Packet types are independently versioned (`suite.<domain>.<action>.v1`)
- Breaking payload changes require a packet type version bump
- Non-breaking additions are additive optional fields only

## Validation Workflow

1. Parse wrapper (`suite.packet.v1`)
2. Route by `packet_type`
3. Validate envelope required fields (`version`, `tool`, `kind`, `hash`, `summary`, `budget_cost`, `provenance`, `payload`)
4. Validate payload contract for the specific packet type
5. Apply profile-specific boundedness checks

## Kernel Target to Packet Type Mapping

| Kernel Target | Output Packet Type |
| --- | --- |
| `diffy.analyze` | `suite.diff.analyze.v1` |
| `testy.impact` | `suite.test.impact.v1` |
| `stacky.slice` | `suite.stack.slice.v1` |
| `buildy.reduce` | `suite.build.reduce.v1` |
| `mapy.repo` | `suite.map.repo.v1` |
| `proxy.run` | `suite.proxy.run.v1` |
| `contextq.assemble` | `suite.context.assemble.v1` |
| `contextq.correlate` | `suite.context.correlate.v1` |
| `contextq.manage` | `suite.context.manage.v1` |
| `governed.assemble` | `suite.context.assemble.v1` |
| `guardy.check` | `suite.guard.check.v1` |
| `agenty.state.write` | `suite.agent.state.v1` |
| `agenty.state.snapshot` | `suite.agent.snapshot.v1` |
