# Packet28 documentation

This directory separates current operating contracts from historical evidence.
Start with the smallest document that matches what you are doing.

## Start here

| Audience | Read first |
| --- | --- |
| New user | [Getting started](getting-started.md) |
| Agent-runtime integrator | [Getting started](getting-started.md), then [Operations](operations.md) |
| Maintainer | [Architecture](architecture.md), [Development](development.md), and [CONTRIBUTING](../CONTRIBUTING.md) |
| Daemon or storage contributor | [Daemon runtime](daemon-runtime.md) and [Task-store retention](task-store-retention.md) |
| Release operator | [Release package verification](release-package-verification.md) and [Cargo publication policy](cargo-publication-policy.md) |
| Audit reviewer | [Architecture-audit remediation ledger](architecture-audit-remediation-20260728.md) |

## Current product and operating guides

- [Getting started](getting-started.md): install, configure an agent runtime,
  run a first task, and understand the main commands.
- [Architecture](architecture.md): layers, data flow, crate ownership,
  dependency rules, compatibility, persistence, and extension points.
- [Operations](operations.md): daemon lifecycle, endpoint discovery, MCP modes,
  state layout, retention, corruption, and troubleshooting.
- [Development](development.md): local workflow, test strategy, canonical gates,
  benchmarks, dependencies, and releases.
- [Context-kernel composition](context-kernel-composition.md): mechanism,
  built-ins, and the compatibility facade.
- [Instruction rendering modes](instruction-rendering-modes.md): stable
  instruction prefix versus adaptive broker brief.
- [Agent DX benchmark](agent-dx-benchmark.md): required agent workflow
  outcomes, frozen explicit-CLI corpus, and reported diagnostics.
- [Integration-test harness](integration-test-harness.md): bounded child-process,
  MCP, timeout, and cleanup ownership.
- [Rust safety and panic policy](rust-safety-and-panic-policy.md): enforced
  unsafe and panic rules.

## Daemon, storage, and release contracts

- [Daemon runtime](daemon-runtime.md)
- [Task-store inspection and retention](task-store-retention.md)
- [Direct-minimum dependency graph](direct-minimum-dependencies.md)
- [Cargo publication policy](cargo-publication-policy.md)
- [Release package verification](release-package-verification.md)
- [Context anomaly runbook](context-anomalies/RUNBOOK.md)

These documents contain mechanically checked source inventories, compatibility
tables, and safety rules. Update them with the code that changes their
invariants.

## Evidence and experiments

`docs/experiments/`, `benchmarks/`, `docs/task-store-metrics/`, and
`docs/audits/` are evidence, not timeless product claims. Each capture should
identify its source revision, environment, command, fixture, and scope.

Important experiment families include:

- prompt-cache and stable-instruction-prefix measurements;
- runtime-starvation and Tokio orchestration evidence;
- persistence, cache, index, SQLite, test-map, and allocation benchmarks;
- fixture and real-repository behavior suites;
- search-backend comparisons and release smoke results.

Provider-side prompt placement, cost, and instruction adherence remain external
evidence boundaries until a provider-instrumented run records them.

## Historical design material

The following files remain useful as decision history but are not current
contracts:

- `AGENTIC_CONTEXT_IDEAS.md`
- `ARCHITECTURE_PLAN.md`
- `PARITY.md`
- `PRODUCT.md`
- `packet28_goal.md`
- `codex/packet28-codebase-health-goal.md`
- dated release, smoke, metric, and experiment captures

When a historical statement conflicts with a current operating contract or the
source, the current contract and mechanically checked source win.

## Documentation rules

- README content is product-facing and concise.
- This index owns navigation and evidence classification.
- `architecture.md` owns the system-level model.
- `daemon-runtime.md` and `task-store-retention.md` own detailed runtime and
  storage guarantees.
- Source rustdoc owns public API behavior and examples.
- The remediation ledger owns audit finding-to-commit/test accounting.
- Generated counts and result blocks must be updated through their scripts.
