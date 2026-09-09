# Packet28

Packet28 is a local context runtime for coding agents. It turns noisy developer
artifacts—search results, coverage reports, diffs, build logs, stack traces,
test maps, and repository structure—into bounded packets that are cheap to
inspect, persist, recall, and hand to the next worker.

It can run as a CLI, an MCP server or proxy, and a workspace daemon. The goal is
simple: let agents spend their context on decisions and edits instead of
repeatedly rediscovering the repository.

## What Packet28 does

- Reduces large tool outputs into typed `suite.packet.v1` envelopes with
  provenance and budget estimates.
- Maintains indexed search, packet recall, task state, watches, and durable
  handoffs under the workspace-local `.packet28/` directory.
- Integrates with Claude Code, Codex, Cursor, and Windsurf through generated MCP,
  hook, and instruction configuration.
- Preserves full artifacts behind handles when a compact response is not enough.
- Provides coverage, diff, test-impact, diagnostics, repository-map, and policy
  commands for agents and CI.

Packet28 is local-first. Agent processes and local tools talk to a daemon bound
to an authenticated Unix socket when available, with a capability-authenticated
loopback fallback.

## Install

The packaged binaries support macOS and Linux on x64 and arm64.

```bash
npm install --global packet28
packet28 --version
```

To try the setup flow without a global install:

```bash
npx packet28@latest
```

To build from source, install the pinned Rust toolchain and build the two main
packages:

```bash
rustup show
cargo build --release --locked -p suite-cli -p packet28d
./target/release/Packet28 --version
```

The npm package exposes `packet28`, `packet28-agent`, `packet28-mcp`, and `p28`.
Source builds also include the `Packet28` umbrella CLI and `packet28d`.

## First run

Configure every detected agent runtime, start the daemon, build the local
indexes, and verify the integration:

```bash
packet28 setup --runtime all --yes
packet28 doctor --root .
```

Run Packet28 as a native MCP server:

```bash
packet28-mcp --root .
```

Or proxy upstream MCP servers so their tool activity becomes part of the next
Packet28 brief:

```bash
packet28 mcp proxy --root . --upstream-config .mcp.proxy.json
```

See [Getting started](docs/getting-started.md) for runtime-specific setup,
configuration, and a guided first task.

## The agent loop

Packet28 keeps routine tool traffic small and moves durable context assembly to
explicit boundaries:

1. Hooks and MCP tools capture search, read, command, and reducer results as
   compact packets.
2. `packet28.write_intention` records a meaningful objective or next-step
   change.
3. The daemon persists task state and relevant artifacts outside the active
   worker's context.
4. `packet28.prepare_handoff` produces a bounded brief for inspection or a fresh
   worker.
5. `packet28-agent` can relaunch delegated work from that checkpoint.

Direct tools remain the right choice for trivial edits. Packet28 is most useful
when the task spans many artifacts, multiple turns, or repeated repository
exploration.

## Common commands

| Goal | Command |
| --- | --- |
| Check integration health | `packet28 doctor --root .` |
| Start or inspect the daemon | `packet28 daemon start --root .` / `packet28 daemon status --root . --json` |
| Search the workspace index | `p28 "symbol or pattern"` |
| Recall persisted context | `packet28 context recall --root . --query "what changed" --json` |
| Inspect coverage changes | `packet28 cover check --coverage coverage/lcov.info --base main --head HEAD --json` |
| Analyze a diff | `packet28 diff analyze --coverage coverage/lcov.info --base main --head HEAD --json` |
| Reduce diagnostics | `packet28 build reduce --input build.log --json` |
| Reduce a stack trace | `packet28 stack slice --input crash.log --json` |
| Map a repository area | `packet28 map repo --repo-root . --focus-symbol AuthService --json` |
| Inspect local state | `packet28 daemon storage inspect --root . --json --pretty` |
| Preview retention | `packet28 daemon storage cleanup --root . --max-age-seconds 604800` |

Run `packet28 --help` or `packet28 <command> --help` for the complete CLI.

## Architecture

Packet28 is a Rust workspace of 34 crates organized into four layers:

```text
agent and CI surfaces
        │
        ▼
CLI, MCP, daemon, and authenticated protocol
        │
        ▼
context kernel, scheduling, memory, policy, and search
        │
        ▼
reducers and shared packet/storage contracts
```

The important boundaries are intentionally narrow:

- shared crates own wire types, packet schemas, filesystem authority, and
  portable invariants;
- reducers transform one artifact family without owning orchestration;
- the context kernel composes reducers, budgets, cache, recall, and policy;
- the daemon owns lifecycle, persistence, watches, task execution, and the
  Tokio orchestration boundary;
- CLIs and MCP adapters translate user requests and present typed failures.

The compatibility facade, daemon protocol, persistence owner, and runtime
dependency rules are mechanically checked. Read [Architecture](docs/architecture.md)
for data flow, crate ownership, extension rules, and migration guidance.

## State and safety

Packet28 stores workspace-local state under `.packet28/`. Important durable
state is checksummed, versioned, and written through authenticated filesystem
capabilities. Daemon task/watch registries use checkpoint plus WAL recovery;
cache corruption fails closed or falls back to an authenticated baseline.

Retention is always a dry run unless `--apply` is provided. Active, malformed,
aliased, symlinked, or concurrently changing state is protected instead of
deleted. Stop `packet28d` before applying a reviewed cleanup plan.

Read [Operations](docs/operations.md), the
[daemon runtime contract](docs/daemon-runtime.md), and
[task-store retention](docs/task-store-retention.md) before automating daemon
or storage maintenance.

## Configuration

`covy.toml` is the project configuration entry point for coverage ingestion,
diff refs, quality gates, path mapping, cache, impact, shard, and merge policy.
Start from [covy.toml.example](covy.toml.example).

`context.yaml` is optional and constrains tools, reducers, paths, budgets,
redaction, and human-review behavior. Runtime setup preserves existing valid
agent configuration and refuses to replace malformed JSON or TOML.

Instruction files have two modes:

- a stable prefix containing long-lived repository rules; and
- an adaptive broker brief containing task-specific state.

Keeping those concerns separate improves prompt-cache stability without
pretending that local cache hits prove provider-side cost or instruction
adherence. See [Instruction rendering modes](docs/instruction-rendering-modes.md).

## Documentation

- [Documentation map](docs/README.md)
- [Getting started](docs/getting-started.md)
- [Architecture](docs/architecture.md)
- [Operations](docs/operations.md)
- [Development and validation](docs/development.md)
- [Contributing](CONTRIBUTING.md)
- [Release package verification](docs/release-package-verification.md)
- [Architecture-audit remediation ledger](docs/architecture-audit-remediation-20260728.md)

## Development

The fastest focused gate is:

```bash
scripts/validate_refactor_batch.sh
```

The canonical local/CI gate is:

```bash
scripts/validate_full_gate.sh
```

It verifies the locked dependency graph, formatting, workspace check/build,
strict Clippy, all-feature tests and doctests, strict rustdoc, architecture
rules, supply-chain policy, direct-minimum dependencies, packaging, and
repository-maintained experiments. The exact MSRV path is:

```bash
rustup run 1.88.0 scripts/validate_full_gate.sh --msrv
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing public APIs, protocol
types, persistence, FFI, or release automation.

## Project stats

<!-- BEGIN GENERATED PROJECT STATS -->
- 274,706 lines across 686 Rust files
- 34 crates in the workspace
- 8 Cargo binary targets (including one internal generator)
<!-- END GENERATED PROJECT STATS -->
