# Getting started

Packet28 can be used as a standalone CLI, an MCP server, or a persistent
workspace daemon. The setup command configures the detected agent runtimes
without replacing malformed or unrelated user configuration.

## Requirements

- macOS or Linux on x64 or arm64 for the packaged binaries;
- Node.js 18 or newer for the npm wrapper;
- one supported agent runtime if you want MCP/hooks integration;
- Git for repository-aware diff, map, and task workflows.

Source builds use the pinned Rust toolchain in `rust-toolchain.toml`. The
project's declared minimum Rust version is 1.88.0.

## Install

With npm:

```bash
npm install --global packet28
packet28 --version
```

Install-free setup:

```bash
npx packet28@latest
```

From source:

```bash
cargo build --release --locked -p suite-cli -p packet28d
./target/release/Packet28 --version
```

The installed wrapper command is lowercase `packet28`; the source-built
umbrella binary is `Packet28`.

## Configure an agent runtime

From the repository you want Packet28 to manage:

```bash
packet28 setup --runtime all --yes
packet28 doctor --root .
```

Omit `--yes` for the interactive plan. Choose a single runtime slug instead of
`all` when you do not want every detected integration.

Setup may create or update repository-local MCP, hook, and instruction files
and user-level runtime configuration. Existing valid JSON/TOML is merged;
invalid configuration is reported and left unchanged.

Setup adds `.packet28/` to the repository's `.gitignore` when needed. This does
not untrack runtime files that were already committed. The full regex index
requires a clean Git working tree, so setup can complete with indexing deferred
until you commit or stash its configuration and instruction changes. Then run:

```bash
packet28 daemon index rebuild --root .
packet28 daemon index status --root . --json
```

## Claude Code and Codex continuation

Configure only the host you use:

```bash
packet28 setup --runtime claude --yes
packet28 doctor --agent claude --root .

packet28 setup --runtime codex --yes
packet28 doctor --agent codex --root .
```

Claude Code setup installs MCP and lifecycle hooks. Codex setup installs MCP,
`AGENTS.md` guidance, and project-local `.codex/hooks.json`. The user MCP
configuration honors `CODEX_HOME` when set. In Codex, enable
lifecycle hooks, trust the project, then review the generated handlers in
`/hooks`. Setup does not change Codex approval rules or trust decisions.

Codex hooks capture tool results and checkpoint task context without rewriting
shell commands. This preserves the command identity used by Codex permission
rules. Use explicit Packet28 CLI/MCP calls when you want reduced output.
`doctor --agent codex` checks generated configuration and a local Packet28
MCP round trip; it reports host hook enablement, trust, and execution as
unverified.

For either host, save the current objective with `packet28.write_intention`,
prepare the latest handoff with `packet28.prepare_handoff`, and fetch it with
`packet28.fetch_context` when continuing work. Daemon restarts retain task
objectives, active decisions, and handoff artifacts. Hosts own model sessions
and execution; Packet28 does not start a model provider during these checks.

## Start the daemon

```bash
packet28 daemon start --root .
packet28 daemon status --root . --json
```

The daemon publishes its selected authenticated endpoint and readiness state in
`.packet28/daemon/runtime.json`. Clients use that file; do not hard-code a
socket path.

Stop it with:

```bash
packet28 daemon stop --root .
```

## Run MCP

Native server:

```bash
packet28-mcp --root .
```

Source build:

```bash
./target/release/Packet28 mcp serve --root .
```

Proxy upstream servers:

```bash
packet28 mcp proxy --root . --upstream-config .mcp.proxy.json
```

The proxy is useful when upstream tool results should be captured in the same
task context. Native mode is simpler when Packet28 is the only MCP server.

## Try the core workflows

Search:

```bash
p28 "AuthService"
```

Recall:

```bash
packet28 context recall \
  --root . \
  --query "coverage gap AuthService" \
  --limit 5 \
  --json
```

Coverage and diff:

```bash
packet28 cover check \
  --coverage coverage/lcov.info \
  --base main \
  --head HEAD \
  --json

packet28 diff analyze \
  --coverage coverage/lcov.info \
  --base main \
  --head HEAD \
  --json
```

Diagnostics and repository map:

```bash
packet28 build reduce --input build.log --json
packet28 stack slice --input crash.log --json
packet28 map repo --repo-root . --focus-symbol AuthService --json
```

Pass `--via-daemon --daemon-root .` to supported commands when you want the
persistent runtime to own execution.

## Configure project defaults

Copy [covy.toml.example](../covy.toml.example) to `covy.toml`, then set:

- coverage report paths and path-prefix mapping;
- default diff base/head;
- total, changed, and new-line coverage gates;
- changed-line diagnostic limits;
- test-impact, sharding, cache, and merge behavior.

Use optional `context.yaml` governance for tool/reducer allowlists, path
constraints, budgets, redaction, and review requirements.

## Understand output profiles

Reducer and context commands use typed packet envelopes.

- compact JSON is the bounded default for agent use;
- full JSON includes the complete payload;
- handle output keeps a compact response and persists the full artifact for
  explicit later retrieval.

Use `packet28 packet fetch` or the matching MCP artifact tool when the compact
view is insufficient.

## Next

- [Architecture](architecture.md)
- [Operations](operations.md)
- [Instruction rendering modes](instruction-rendering-modes.md)
- [Task-store retention](task-store-retention.md)

## Native command permissions

Generated runtime integrations preserve the host's original command arguments.
Hooks capture results and correlate task activity without returning rewritten
commands or permission decisions. This also applies when an older
`.packet28/daemon/hook-runtime-v1.json` contains `rewrite_enabled: true`; that legacy flag
is inactive. `Packet28 hook rewrite status --json` reports actual capture-only
behavior and the stored flag separately. `Packet28 hook rewrite on` returns an
error without changing configuration. `Packet28 hook rewrite off` clears the old
flag.

Use explicit Packet28 CLI or MCP reduction when you want reduced output. An
opaque wrapper changes which command a native host permission rule matches.
Re-run setup for OpenCode or Hermes to replace previously installed automatic
rewrite plugins with command-preserving adapters.

`Packet28 rewrite` and `Packet28 compact rewrite` no longer return executable
wrappers. JSON output retains route metadata with `applied: false` and
`rewritten_command: null`; plain output is empty. This also makes previously
installed OpenCode and Hermes rewrite plugins pass through original commands
when they invoke the updated binary. Explicit executing reducers and MCP
reduction remain available.
