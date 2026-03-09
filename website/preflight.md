# Preflight

## Overview

Preflight is the primary agent entry point. It maps a natural language task description to the right reducers, runs them, and returns one bounded JSON payload with structured results.

```bash
Packet28 preflight --task "fix coverage regression in AuthService" --json
```

## How It Works

1. **Extract anchors**: Parse the task description for file paths, symbol names, and terms
2. **Classify tags**: Match terms against keyword sets (coverage, diff, build, stack, test)
3. **Select reducers**: Map tags to reducer sets, filtered by availability and budget
4. **Execute**: Run each selected reducer and accumulate results
5. **Recall**: Query the BM25 index with the task description + extracted anchors
6. **Return**: One JSON payload with selection metadata, reducer packets, recall hits, and totals

## Heuristic Selection

| Task mentions | Reducers selected |
| --- | --- |
| coverage, jacoco, lcov, gate, cover | cover + diff + map + recall |
| diff, change, regression, review, pr, patch, branch | diff + map + recall |
| build, compile, lint, warning, error, diagnostic | build + diff + recall |
| stack, trace, exception, failure, crash, panic | stack + map + recall |
| test, tests, impact, flaky, flake | impact + diff + recall |
| (none of the above) | diff + map + recall |

Execution order: cover → diff → map → stack → build → impact → recall.

Reducers are trimmed when cumulative planned cost exceeds `--budget-tokens` (default 5000). Recall always runs last.

## Anchor Extraction

Anchors are extracted from the task description:

- **Paths**: Tokens containing `/`, `\`, or known file extensions (`.rs`, `.java`, `.py`, `.ts`, etc.)
- **Symbols**: Tokens containing `::`, `_`, `.`, or starting with an uppercase letter
- **Terms**: Lowercased tokens >= 3 characters, excluding stopwords

Anchors flow into:
- `map repo --focus-path` and `--focus-symbol`
- Recall query construction
- Preflight output for consumer inspection

## Availability Checks

Each reducer has an availability predicate:

| Reducer | Available when |
| --- | --- |
| Cover | `--coverage` paths provided OR `.covy/state/latest.bin` exists |
| Diff | Root is a git repository |
| Map | Always |
| Recall | Always |
| Stack | `--stack-input` provided |
| Build | `--build-input` provided |
| Impact | Testmap file exists (default `.covy/state/testmap.bin`) |

When a reducer is selected by heuristics but unavailable, it appears in `selection.skipped` with a reason.

## Coverage Source Resolution

When both `--coverage` paths and a cached state file (`.covy/state/latest.bin`) exist, preflight uses the explicit `--coverage` paths and ignores the state file. When only the state file exists, it uses that. This prevents the "Cannot combine positional coverage paths with --input" conflict.

## Budget

- `--budget-tokens` (default 5000): Planning budget for reducer selection
- Each reducer has a planning cost estimate (cover: 800, diff: 1200, map: 2000, recall: 600, stack: 500, build: 600, impact: 900)
- Reducers are added in execution order until the budget would be exceeded
- `--include` overrides bypass budget trimming
- `--exclude` removes reducers regardless of heuristics

Output totals include:
- `planned_over_budget`: Whether planning estimates exceeded the budget
- `actual_over_budget`: Whether actual post-execution token totals exceeded the budget
- `over_budget`: Set to `actual_over_budget` value

## Output Schema

```json
{
  "schema_version": "suite.preflight.v1",
  "task": "fix coverage regression in AuthService",
  "root": "/path/to/repo",
  "task_id": null,
  "profile": "compact",
  "selection": {
    "tags": ["coverage"],
    "anchors": {
      "paths": [],
      "symbols": ["AuthService"],
      "terms": ["authservice", "coverage", "fix", "regression"]
    },
    "selected_reducers": ["cover", "diff", "map", "recall"],
    "skipped": []
  },
  "results": {
    "packets": [
      {
        "reducer": "cover",
        "packet_type": "suite.cover.check.v1",
        "cache_hit": false,
        "packet": { "...suite.packet.v1 wrapper..." }
      }
    ],
    "recall": {
      "query": "fix coverage regression in AuthService AuthService",
      "hits": []
    }
  },
  "totals": {
    "est_tokens": 4600,
    "est_bytes": 18400,
    "runtime_ms": 45,
    "tool_calls": 4,
    "packet_count": 3,
    "cache_hits": 0,
    "recall_hits": 0,
    "planned_over_budget": false,
    "actual_over_budget": false,
    "over_budget": false
  }
}
```

## CLI Options

```
--task <TASK>                    Natural-language task description (required)
--root <ROOT>                    Repo root for reducers and persistence (default: .)
--task-id <TASK_ID>              Task ID for recall scoping
--base <BASE>                    Git base ref
--head <HEAD>                    Git head ref
--budget-tokens <N>              Planning token budget (default: 5000)
--limit-recall <N>               Max recall hits (default: 4)
--focus-path <PATH>              Explicit focus paths (repeatable)
--focus-symbol <SYMBOL>          Explicit focus symbols (repeatable)
--coverage <PATH>                Coverage report file paths (repeatable)
--stack-input <PATH>             Stack trace / log file
--build-input <PATH>             Build / lint log file
--testmap <PATH>                 Testmap path (default: .covy/state/testmap.bin)
--include <REDUCER>              Force-include reducers (repeatable)
--exclude <REDUCER>              Force-exclude reducers (repeatable)
--json [compact|full|handle]     JSON output profile
--pretty                         Pretty-print JSON output
```

## Daemon Support

Preflight works through the daemon with `--via-daemon`. Each reducer execution and recall query is routed through the daemon's persistent kernel and cache.

## Agent Surface

### Prompt Generator

```bash
Packet28 agent-prompt --format claude
```

Generates instruction fragments that teach agents to use preflight before broad file reads.

### Wrapper Binary

```bash
packet28-agent --task "investigate flaky test" -- codex exec "review"
```

Runs preflight, persists the result, exports env vars (`PACKET28_PREFLIGHT_PATH`, `PACKET28_ROOT`), then executes the delegated agent command.
