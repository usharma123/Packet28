# Schema Registry

## Purpose
Packet28 Phase 1 makes packet contracts explicit in code and artifacts.

Code registry source:

- `crates/suite-packet-core/src/registry.rs`

Schema artifacts:

- `schemas/packet-wrapper/suite.packet.v1.schema.json`
- `schemas/packet-types/<packet_type>.schema.json`
- `schemas/snapshots/<packet_type>/{compact,full,handle}.json`

## Registered Packet Types
- `suite.cover.check.v1`
- `suite.diff.analyze.v1`
- `suite.test.impact.v1`
- `suite.stack.slice.v1`
- `suite.build.reduce.v1`
- `suite.map.repo.v1`
- `suite.proxy.run.v1`
- `suite.context.assemble.v1`
- `suite.guard.check.v1`

## Registry Contract Fields
Each packet type contract defines:

- required payload fields
- optional payload fields
- boundedness rules
- one-release compatibility notes

## Compatibility Policy
Phase 1 compatibility shim window: one release.

Supported during shim:
- legacy `--json` boolean style mapped to compact
- legacy `--report json` mapped to compact where supported
- legacy `--packet-detail` mapped to profile/detail behavior
- explicit `--legacy-json` for old top-level output shapes

After shim window, legacy wrapper shapes may be removed.

## Versioning Rules
- Wrapper schema is versioned (`suite.packet.v1`).
- Packet types are independently versioned (`suite.<domain>.<action>.v1`).
- Breaking payload shape changes require packet type version bump.
- Non-breaking payload additions are additive optional fields only.

## Validation Workflow
1. Parse wrapper (`suite.packet.v1`).
2. Route by `packet_type`.
3. Validate envelope required fields.
4. Validate payload contract for packet type.
5. Apply profile-specific boundedness checks.
