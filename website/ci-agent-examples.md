# CI and Agent Integration Examples

## Parsing Packet Output

All Packet28 machine-mode commands emit `suite.packet.v1` JSON wrappers. Parse path:

1. Read `schema_version` (always `"suite.packet.v1"`)
2. Read `packet_type` (e.g. `"suite.diff.analyze.v1"`)
3. Read `packet.hash` (canonical blake3 hash)
4. Read `packet.payload` (reducer-specific data)

## Agent Integration

### Live Broker (Recommended Entry Point)

Packet28 is intended to sit alongside the live agent loop as a hooks-first reducer plus handoff broker. The normal loop is:

1. Start `Packet28 mcp serve`
2. Install Claude hooks with `Packet28 setup --runtime claude`
3. Let hooks persist slim reducer packets during the active turn
4. Use `packet28.write_intention` only when the task objective changes materially
5. Fetch full context artifacts only when explicit inspection is needed.
6. Let the daemon assemble handoff after threshold or stop boundaries
7. Let the daemon or wrapper relaunch a fresh worker with the handoff packet

### MCP Startup

```bash
# Start Packet28 as an MCP server for the current repo
Packet28 mcp serve --root .
```

### Stored Context Inspection

Use stored handoff/context artifacts for explicit inspection instead of fetching thick context mid-turn.

Use `packet28.fetch_context` only when you need to inspect a stored handoff/context artifact in full:

```json
{
  "name": "packet28.fetch_context",
  "arguments": {
    "task_id": "task-auth-broker",
    "artifact_id": "artifact-123"
  }
}
```

### Checkpointed Handoff

Use a dedicated intention write for semantic objective changes:

```json
{
  "name": "packet28.write_intention",
  "arguments": {
    "task_id": "task-auth-broker",
    "text": "Wire the broker handoff flow through the MCP surface.",
    "note": "The next worker should resume from the handoff artifact rather than a thick mid-turn context fetch.",
    "step_id": "editing",
    "paths": ["crates/suite-cli/src/cmd_mcp.rs"],
    "symbols": ["handle_packet28_prepare_handoff"]
  }
}
```

The daemon uses that intention plus hook-captured reducer packets to prepare the next handoff packet automatically at threshold or stop boundaries. Use `packet28.prepare_handoff` only when you need to inspect or bootstrap explicitly.

### Fresh-Worker Bootstrap

Fresh workers should start from a checkpointed handoff, not from a one-shot planning envelope.

```bash
Packet28 daemon task await-handoff \
  --root . \
  --task-id task-auth-broker \
  --timeout-ms 300000 \
  --poll-ms 250 \
  --json
```

### Agent Prompt Fragments

Generate instruction fragments for agent config files:

```bash
# Append to CLAUDE.md
Packet28 agent-prompt --format claude >> CLAUDE.md

# Generate AGENTS.md fragment
Packet28 agent-prompt --format agents

# Generate .cursorrules fragment
Packet28 agent-prompt --format cursor
```

### Runtime Adapter Examples

#### Claude

- Start `Packet28 mcp serve`
- Keep `task_id` and a single mutable Packet28 context block in the agent session
- Treat the latest Packet28 brief as the only canonical Packet28 context block; replace older Packet28 briefs instead of appending them
- Let Claude hooks rewrite supported Bash commands and capture reducer packets invisibly inside the turn
- Use `packet28.write_intention` only when the objective changes materially
- Assemble handoff at stop/threshold boundaries and relaunch a fresh worker instead of bloating the active session

#### Codex / AGENTS.md

- Use `Packet28 agent-prompt --format agents` to seed the runtime instructions
- Keep one mutable Packet28 block in the prompt and replace it when a newer brief supersedes the old one
- Let hooks rewrite supported shell commands and capture routine tool activity without visible MCP reducer calls
- Use `packet28.write_intention` for semantic resume breadcrumbs and `packet28.prepare_handoff` only for explicit bootstrap or inspection

#### Cursor

- Start MCP once per workspace
- Replace the prior Packet28 context block instead of appending Packet28 history
- Let hooks rewrite supported shell commands and capture reducer packets in the turn, then relaunch from handoff after threshold or stop boundaries
- Keep `.packet28/task/<task_id>/brief.md` only as a fallback bridge
- Treat `verbosity` as a compatibility alias; prefer explicit section limits

### Wrapper Binary

`packet28-agent` bootstraps a worker from a checkpointed handoff:

```bash
packet28-agent \
  --task "investigate flaky parser test" \
  --wait-for-handoff \
  -- codex exec "review the failure"
```

```bash
packet28-agent \
  --task-id task-auth-broker \
  --wait-for-handoff \
  -- codex exec "continue the task"
```

The wrapper:
1. Waits for a checkpointed handoff when `--wait-for-handoff` is enabled
2. Persists the startup packet to `.packet28/agent/latest-bootstrap.json` and the assembled handoff to `.packet28/agent/latest-handoff.json`
3. Exports `PACKET28_BOOTSTRAP_MODE`, `PACKET28_BOOTSTRAP_PATH`, and `PACKET28_ROOT` to the child process
4. Exports `PACKET28_HANDOFF_PATH`, `PACKET28_HANDOFF_ARTIFACT_ID`, and `PACKET28_HANDOFF_CHECKPOINT_ID` for resumed workers
5. Asks `packet28d` to block until a ready handoff exists; if the last worker already consumed a handoff, the wait targets a newer context version
6. Executes the delegated command, propagating its exit code

You can also wait on the daemon directly before choosing how to relaunch:

```bash
Packet28 daemon task await-handoff \
  --root . \
  --task-id task-auth-broker \
  --after-context-version 42 \
  --timeout-ms 300000 \
  --poll-ms 250 \
  --json
```

Or let the daemon wait and spawn the next worker itself:

```bash
Packet28 daemon task launch-agent \
  --root . \
  --task-id task-auth-broker \
  --wait-for-handoff \
  --json \
  -- codex exec "continue the task"
```

### Reading Bootstrap Context in an Agent

```python
import json
import os

# Read the broker bootstrap payload injected by packet28-agent.
bootstrap_path = os.environ.get("PACKET28_BOOTSTRAP_PATH")
bootstrap_mode = os.environ.get("PACKET28_BOOTSTRAP_MODE", "handoff")
if bootstrap_path:
    with open(bootstrap_path) as f:
        broker_context = json.load(f)

    print(f"bootstrap_mode={bootstrap_mode}")
    print(f"context_version={broker_context['context_version']}")
    print(broker_context["brief"])

    latest_intention = broker_context.get("latest_intention")
    if latest_intention:
        print(f"resume: {latest_intention['text']}")
```

### Daemon Task Workflow

For long-running tasks that need persistent state and file watching:

```bash
# Start the daemon
Packet28 daemon start --root .

# Submit a multi-step task with watches
Packet28 daemon task submit --root . --spec '{
  "sequence": {
    "steps": [
      {"id": "analyze", "target": "diffy.analyze", "reducer_input": {...}},
      {"id": "map", "target": "mapy.repo", "reducer_input": {...}}
    ]
  },
  "watches": [
    {"kind": "file", "patterns": ["src/**/*.rs"]}
  ]
}'

# Subscribe to live task events
Packet28 daemon task subscribe --root . --task-id <id>

# Route any command through the daemon for caching
Packet28 diff analyze --coverage report.xml --via-daemon --json
```

### Recall

Query prior context across tasks:

```bash
# Search for related prior work
Packet28 context recall \
  --root . \
  --query "coverage gap AuthService" \
  --limit 5 \
  --json
```

## CI Integration

### Bash + jq

```bash
set -euo pipefail

out="$(Packet28 diff analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --no-issues-state --json)"

schema="$(jq -r '.schema_version' <<<"$out")"
ptype="$(jq -r '.packet_type' <<<"$out")"
hash="$(jq -r '.packet.hash' <<<"$out")"
passed="$(jq -r '.packet.payload.gate_result.passed // .packet.payload.passed // false' <<<"$out")"

echo "schema=$schema type=$ptype hash=$hash passed=$passed"
```

### Python

```python
import json
import subprocess

raw = subprocess.check_output([
    "Packet28", "map", "repo",
    "--repo-root", ".",
    "--json=compact",
], text=True)

packet = json.loads(raw)
assert packet["schema_version"] == "suite.packet.v1"

packet_type = packet["packet_type"]
packet_hash = packet["packet"]["hash"]
payload = packet["packet"]["payload"]

print(packet_type, packet_hash, payload.get("truncated", False))
```

### TypeScript (Node)

```ts
import { execFileSync } from "node:child_process";

const raw = execFileSync("Packet28", [
  "proxy", "run",
  "--json=handle",
  "--",
  "git", "status", "--short",
], { encoding: "utf8" });

const wrapper = JSON.parse(raw);
if (wrapper.schema_version !== "suite.packet.v1") {
  throw new Error("unexpected schema version");
}

const handle = wrapper.packet.payload.artifact_handle;
if (handle) {
  const expanded = execFileSync("Packet28", [
    "packet", "fetch",
    "--handle", handle.handle_id,
    "--json=full",
  ], { encoding: "utf8" });
  const full = JSON.parse(expanded);
  console.log(full.packet.hash);
}
```

### GitHub Actions

```yaml
name: packet28-gate
on: [pull_request]

jobs:
  diff-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build Packet28
        run: cargo build --release -p suite-cli -p packet28d

      - name: Run diff gate
        id: diff
        run: |
          OUT=$(./target/release/Packet28 diff analyze \
            --coverage tests/fixtures/lcov/basic.info \
            --base HEAD~1 \
            --head HEAD \
            --no-issues-state \
            --json)
          echo "$OUT" > packet.json
          echo "passed=$(jq -r '.packet.payload.gate_result.passed // .packet.payload.passed // false' packet.json)" >> "$GITHUB_OUTPUT"

      - name: Enforce gate
        run: |
          if [ "${{ steps.diff.outputs.passed }}" != "true" ]; then
            echo "Gate failed"
            exit 1
          fi
```

## Exit Handling

Unified process contract:

- `0`: command succeeded and policy/gate passed
- `1`: command succeeded but gate/policy/domain failed
- `2+`: runtime/config/execution failure

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
