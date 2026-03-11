# CI and Agent Integration Examples

## Parsing Packet Output

All Packet28 machine-mode commands emit `suite.packet.v1` JSON wrappers. Parse path:

1. Read `schema_version` (always `"suite.packet.v1"`)
2. Read `packet_type` (e.g. `"suite.diff.analyze.v1"`)
3. Read `packet.hash` (canonical blake3 hash)
4. Read `packet.payload` (reducer-specific data)

## Agent Integration

### Live Broker (Recommended Entry Point)

Packet28 is now intended to sit in the live agent loop, not just before it starts. The normal loop is:

1. Start `Packet28 mcp serve`
2. Keep `task_id`, `context_version`, and a local section cache
3. Call `packet28.estimate_context` before cheap or budget-constrained actions
4. For constrained refactors, call `packet28.decompose`, refine the steps locally, then run `packet28.validate_plan`
5. Call `packet28.get_context(..., since_version, response_mode="auto")`
6. Patch the local section cache from `delta.changed_sections` and `delta.removed_section_ids`
7. Replace the prior Packet28 context block instead of appending old Packet28 briefs
8. Call `packet28.write_state` after file reads, edits, checkpoints, decisions, and question updates
9. Listen for `notifications/packet28.context_updated`, with polling fallback via `since_version`

### MCP Startup

```bash
# Start Packet28 as an MCP server for the current repo
Packet28 mcp serve --root .
```

### Cost Preview

Ask Packet28 whether a full fetch is worth it before spending tokens:

```json
{
  "name": "packet28.estimate_context",
  "arguments": {
    "task_id": "task-auth-broker",
    "action": "plan",
    "budget_tokens": 4000,
    "response_mode": "auto",
    "include_sections": ["task_objective", "repo_map", "recommended_actions"]
  }
}
```

The estimate response includes `selected_section_ids`, `est_tokens`, `budget_remaining_tokens`, `section_estimates`, and `eviction_candidates`.

### Slim Search First

Use `packet28.search` when you want cheap steering before asking for full grouped matches:

```json
{
  "name": "packet28.search",
  "arguments": {
    "task_id": "task-auth-broker",
    "query": "BrokerWriteStateRequest",
    "paths": ["crates"],
    "whole_word": true,
    "response_mode": "slim"
  }
}
```

The slim response returns only `compact_preview`, `match_count`, and `artifact_id`. If the preview looks promising, expand the stored full result on demand:

```json
{
  "name": "packet28.fetch_tool_result",
  "arguments": {
    "task_id": "task-auth-broker",
    "artifact_id": "artifact-123"
  }
}
```

### Deterministic Planning

Use Packet28 to generate and validate constrained refactor plans before execution. `packet28.decompose` is experimental and intentionally narrow in this milestone:

```json
{
  "name": "packet28.decompose",
  "arguments": {
    "task_id": "task-auth-broker",
    "task_text": "restructure auth module",
    "intent": "restructure_module",
    "max_steps": 6
  }
}
```

```json
{
  "name": "packet28.validate_plan",
  "arguments": {
    "task_id": "task-auth-broker",
    "steps": [
      {"id": "step-1", "action": "edit", "paths": ["src/auth.rs"]},
      {"id": "step-2", "action": "add_tests", "paths": ["tests/auth_test.rs"], "depends_on": ["step-1"]}
    ],
    "require_read_before_edit": true,
    "require_test_gate": true
  }
}
```

### Full Context Fetch

Fetch the rendered brief plus the structured delta payload only when needed:

```json
{
  "name": "packet28.get_context",
  "arguments": {
    "task_id": "task-auth-broker",
    "action": "edit",
    "since_version": "12",
    "response_mode": "auto",
    "include_sections": ["task_objective", "current_focus", "checkpoint_deltas", "repo_map"],
    "section_item_limits": {
      "repo_map": 6,
      "checkpoint_deltas": 6
    }
  }
}
```

### Preflight (Compatibility Path)

Preflight remains available for one-shot startup context and compatibility wrappers.

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

### Runtime Adapter Examples

#### Claude

- Start `Packet28 mcp serve`
- Keep `task_id`, `context_version`, and a local section cache in the agent session
- Treat the latest Packet28 brief as the only canonical Packet28 context block; replace older Packet28 briefs instead of appending them
- Call `packet28.estimate_context` before low-cost planning or summarization steps
- Use `packet28.decompose` and `packet28.validate_plan` for constrained refactors before execution
- Call `packet28.get_context(..., since_version, response_mode="auto")` before each substantive invocation
- Patch the local section cache from `delta.changed_sections` and `delta.removed_section_ids`
- On `notifications/packet28.context_updated`, refresh on the next invocation
- If notifications are unavailable, fall back to polling with `since_version`

#### Codex / AGENTS.md

- Use `Packet28 agent-prompt --format agents` to seed the runtime instructions
- Keep the local section cache as the authoritative broker state inside the session
- Keep one mutable Packet28 block in the prompt and replace it when a newer brief supersedes the old one
- Use `packet28.decompose` and `packet28.validate_plan` for constrained planning flows
- Prefer explicit `include_sections`, `exclude_sections`, and `section_item_limits`
- Use `packet28.write_state` after file reads, edits, checkpoints, decisions, and question changes
- Poll with `since_version` whenever notification delivery is unavailable or disabled

#### Cursor

- Start MCP once per workspace
- Use `packet28.estimate_context` when near the model budget
- Use `packet28.decompose` and `packet28.validate_plan` before executing constrained refactors
- Use `packet28.get_context(..., response_mode="auto")` for full or delta fetches
- Replace the prior Packet28 context block instead of appending Packet28 history
- Keep `.packet28/task/<task_id>/brief.md` only as a fallback bridge
- Treat `verbosity` as a compatibility alias; prefer explicit section limits

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
