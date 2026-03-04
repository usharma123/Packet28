# CI and Agent Examples

All Phase 1 scoped commands share one parse path:

1. read `schema_version`
2. read `packet_type`
3. read `packet.hash`
4. read `packet.payload`

## Bash + jq

```bash
set -euo pipefail

out="$(Packet28 diff analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --no-issues-state --json)"

schema="$(jq -r '.schema_version' <<<"$out")"
ptype="$(jq -r '.packet_type' <<<"$out")"
hash="$(jq -r '.packet.hash' <<<"$out")"
passed="$(jq -r '.packet.payload.gate_result.passed // .packet.payload.passed // false' <<<"$out")"

echo "schema=$schema type=$ptype hash=$hash passed=$passed"
```

## Python

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

## TypeScript (Node)

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

## GitHub Actions

```yaml
name: packet28-gate
on: [pull_request]

jobs:
  diff-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build Packet28
        run: cargo build --release -p suite-cli

      - name: Run diff analyze (machine mode)
        id: diff
        run: |
          OUT=$(./target/release/Packet28 diff analyze \
            --coverage tests/fixtures/lcov/basic.info \
            --base HEAD~1 \
            --head HEAD \
            --no-issues-state \
            --json)
          echo "$OUT" > packet.json
          echo "packet_type=$(jq -r '.packet_type' packet.json)" >> "$GITHUB_OUTPUT"
          echo "packet_hash=$(jq -r '.packet.hash' packet.json)" >> "$GITHUB_OUTPUT"
          echo "passed=$(jq -r '.packet.payload.gate_result.passed // .packet.payload.passed // false' packet.json)" >> "$GITHUB_OUTPUT"

      - name: Enforce gate
        run: |
          if [ "${{ steps.diff.outputs.passed }}" != "true" ]; then
            echo "Gate failed for packet hash ${{ steps.diff.outputs.packet_hash }}"
            exit 1
          fi
```

## Exit Handling
Unified process contract in CI wrappers:

- `0`: command + policy passed
- `1`: command succeeded but domain gate/policy failed
- `2+`: runtime/config/execution failure
