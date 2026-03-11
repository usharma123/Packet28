# Agent Search Benchmark

`scripts/bench_agent_search.py` benchmarks Packet28's slim-first `packet28.search` flow against plain `rg` on real code-search tasks.

It uses two real workspaces:

- `.` for self-hosting Packet28 cases
- `apache/` for medium-size external-repo cases

Default cases:

- `broker-write-state-request`
- `repo-index-snapshot`
- `packet28-search`
- `abbreviate`
- `reflection-equals`
- `shuffle`

What it measures:

- surfaced file quality against explicit expected paths
- median latency across repeated runs
- compact payload size in bytes and estimated tokens
- search match counts from the slim response
- naive `rg` candidate-file and line counts

The Packet28 side measures the first-pass `response_mode="slim"` response only. Full grouped matches stay behind `packet28.fetch_tool_result` and are not counted unless a follow-up expansion is requested.

Quick run:

```bash
python3 scripts/bench_agent_search.py
```

Write machine-readable and markdown artifacts:

```bash
python3 scripts/bench_agent_search.py \
  --json-output /tmp/agent-search-bench.json \
  --markdown-output /tmp/agent-search-bench.md
```

Run one case:

```bash
python3 scripts/bench_agent_search.py --case abbreviate --iterations 5
```

List available cases:

```bash
python3 scripts/bench_agent_search.py --list-cases
```
