# CI and Agent Integration Examples

## Parsing Packet Output

All Packet28 machine-mode commands emit `suite.packet.v1` JSON wrappers. Parse path:

1. Read `schema_version` (always `"suite.packet.v1"`)
2. Read `packet_type` (e.g. `"suite.diff.analyze.v1"`)
3. Read `packet.hash` (canonical blake3 hash)
4. Read `packet.payload` (reducer-specific data)

## Agent Integration

### Preflight (Recommended Entry Point)

Preflight is the primary agent integration surface. It classifies a natural language task, selects the right reducers, runs them, and returns one bounded JSON payload.

```bash
# Run preflight for a task
out="$(Packet28 preflight \
  --task "fix coverage regression in AuthService" \
  --json)"

# Parse the result
selected="$(jq -r '.selection.selected_reducers | join(",")' <<<"$out")"
est_tokens="$(jq -r '.totals.est_tokens' <<<"$out")"
echo "reducers=$selected tokens=$est_tokens"

# Extract individual reducer packets
jq -r '.results.packets[] | "\(.reducer): \(.packet.packet.summary)"' <<<"$out"

# Extract recall hits
jq -r '.results.recall.hits[] | "score=\(.score) target=\(.target)"' <<<"$out"
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

### Wrapper Binary

`packet28-agent` runs preflight automatically before delegating to an agent runtime:

```bash
packet28-agent \
  --task "investigate flaky parser test" \
  -- codex exec "review the failure"
```

The wrapper:
1. Runs preflight with the task description
2. Persists the result to `.packet28/agent/latest-preflight.json`
3. Exports `PACKET28_PREFLIGHT_PATH` and `PACKET28_ROOT` to the child process
4. Executes the delegated command, propagating its exit code

### Reading Preflight in an Agent

```python
import json
import os

# Read the preflight payload injected by packet28-agent
preflight_path = os.environ.get("PACKET28_PREFLIGHT_PATH")
if preflight_path:
    with open(preflight_path) as f:
        preflight = json.load(f)

    for packet in preflight["results"]["packets"]:
        reducer = packet["reducer"]
        summary = packet["packet"]["packet"]["summary"]
        print(f"[{reducer}] {summary}")

    for hit in preflight["results"]["recall"]["hits"]:
        print(f"recall: score={hit['score']:.3f} {hit['summary']}")
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

      - name: Run preflight
        id: preflight
        run: |
          OUT=$(./target/release/Packet28 preflight \
            --task "review PR changes" \
            --json)
          echo "$OUT" > preflight.json
          echo "est_tokens=$(jq -r '.totals.est_tokens' preflight.json)" >> "$GITHUB_OUTPUT"

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
  "command": "Packet28 preflight",
  "target": "preflight",
  "message": "No coverage input found",
  "causes": [],
  "retry_hint": null
}
```
