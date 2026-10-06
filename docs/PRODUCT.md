# Packet28 Local Context Product Slice

## Usage

Setup Windsurf:

```bash
npx packet28@latest
Packet28 init --agent windsurf --yes --root .
Packet28 init --agent cline --yes --root .
Packet28 setup --runtime copilot --yes --root .
Packet28 init --mode all --yes --root .
Packet28 setup --runtime windsurf --yes --root .
Packet28 doctor --agent windsurf --root .
Packet28 mcp smoke-test --from-config windsurf
```

Running `Packet28` with no subcommand launches the same setup wizard. The
wizard is modeled after modern `npx` setup flows: detect installed agent
runtimes, show a plan before writing files, preserve existing config, support
non-interactive `--yes` mode through `Packet28 setup --yes`, verify daemon/index
health, and print exact next-step commands. Claude, Cursor, Codex, and Windsurf
keep their current MCP/hook tiers; Copilot, Gemini, OpenCode, Hermes, Cline,
Roo Code, Kilo Code, and Antigravity are currently explicit instruction-file
targets until hook/plugin parity is implemented and tested.

Plan and run reducer-aware commands:

```bash
Packet28 rewrite --json "git status --short"
Packet28 run --root . git status --short
Packet28 run --root . cargo check
Packet28 run --root . npm test
Packet28 run --root . pytest
Packet28 run --root . grep TODO src
Packet28 run --root . docker logs app
Packet28 run --root . gh pr checks 1
```

Repo-local rewrite exclusions can be configured in `covy.toml`:

```toml
[packet28.rewrite]
exclude_commands = ["curl", "playwright"]
transparent_prefixes = ["poetry run", "direnv exec ."]
```

Inspect savings and missed savings:

```bash
Packet28 gain --root .
Packet28 discover --root .
Packet28 session --root .
Packet28 dashboard --root .
```

Inspect context-quality trends:

```bash
Packet28 verify context-anomalies --root . --json
Packet28 dashboard --root . --json
Packet28 dashboard --root . --context-anomaly-history docs/context-anomalies/history.jsonl --json
Packet28 verify context-anomalies --root . --max-trend-age-ms 604800000 --json
```

`verify context-anomalies` appends compact history under `.packet28`, the dashboard `context_anomalies` tile reports latest and recurring hidden-category trend fields, fixture replay avoids mutating live history, and `max_trend_age_ms` gates stale trend data.
Runbook: `docs/context-anomalies/RUNBOOK.md`.

Use local memory, feedback, transcripts, and graph:

```bash
Packet28 memory store "Important local project fact" \
  --topic project --importance high --keywords project,decision \
  --source cli --raw "verbatim source"
Packet28 memory recall "project fact" --topic project --keyword decision
Packet28 memory list --topic project --sort importance --all
Packet28 memory update 1 --content "Updated project fact" --topic project
Packet28 memory topics
Packet28 memory stats
Packet28 memory health --topic project --consolidation-threshold 10
Packet28 memory consolidate --topic project
Packet28 memory embed --all --dimensions 384
Packet28 memory decay --factor 0.95
Packet28 memory prune --threshold 0.1 --dry-run
Packet28 memory forget 1
Packet28 memory forget --topic obsolete-topic
Packet28 wakeup --query project --max-tokens 500 --format markdown --json
Packet28 feedback record "bad reducer output" "prefer focused summaries" \
  --topic reducers --context "test output was noisy" \
  --predicted "show everything" --reason "too many irrelevant lines" --source cli
Packet28 feedback search reducer
Packet28 feedback list --topic reducers
Packet28 feedback apply 1
Packet28 feedback delete 1
Packet28 feedback stats
Packet28 transcript append "Need compact transcript recall" \
  --session project-session --agent codex --role user --source cli
Packet28 transcript search "transcript recall"
Packet28 transcript show project-session
Packet28 transcript list
Packet28 transcript stats
Packet28 graph create --name Packet28 --description "Packet28 graph memoir"
Packet28 graph list
Packet28 graph show Packet28
Packet28 graph add-concept Packet28 --memoir Packet28 --label domain:context --confidence 0.82 --source-id memory:packet28
Packet28 graph refine Packet28 "local context runtime with reducers"
Packet28 graph link Packet28 Reducers --relation uses
Packet28 graph search context --memoir Packet28 --label domain:context
Packet28 graph inspect-concept Packet28 --memoir Packet28 --depth 1
Packet28 graph distill --from-topic reducers --into Packet28
Packet28 graph export --format dot
Packet28 graph stats
Packet28 graph inspect
Packet28 graph delete Packet28
Packet28 learn --project-dir . --project-name Packet28 --json
```

MCP tools exposed by the product slice:

- `packet28.search`
- `packet28.reduce`
- `packet28.rewrite`
- `packet28.memory_store`
- `packet28.memory_recall`
- `packet28.memory_list`
- `packet28.memory_update`
- `packet28.memory_forget`
- `packet28.memory_topics`
- `packet28.memory_stats`
- `packet28.memory_health`
- `packet28.memory_consolidate`
- `packet28.memory_embed`
- `packet28.memory_decay`
- `packet28.memory_prune`
- `packet28.feedback_record`
- `packet28.feedback_search`
- `packet28.feedback_list`
- `packet28.feedback_apply`
- `packet28.feedback_delete`
- `packet28.feedback_stats`
- `packet28.wakeup`
- `packet28.learn_project`
- `packet28.transcript_append`
- `packet28.transcript_list`
- `packet28.transcript_show`
- `packet28.transcript_search`
- `packet28.transcript_stats`
- `packet28.graph_add_concept`
- `packet28.graph_refine`
- `packet28.graph_link`
- `packet28.graph_search`
- `packet28.graph_export`
- `packet28.graph_stats`
- `packet28.graph_delete`
- `packet28.graph_inspect`
- `packet28.prepare_handoff`
- `packet28.handoff`
- `packet28.doctor`
- `packet28.task_status`
- `packet28.capabilities`

## Support Tiers

| Runtime | Tier | Notes |
|---|---|---|
| Claude Code | Generated integration preserves native command arguments | Hooks capture/correlate tool activity where supported. Automatic command rewriting is disabled, including stored legacy flags; use explicit Packet28 CLI/MCP reduction. Doctor verifies installed configuration, not live host permission enforcement. |
| Cursor | Generated integration preserves native command arguments | Hooks capture/correlate tool activity where supported. Automatic command rewriting is disabled, including stored legacy flags; use explicit Packet28 CLI/MCP reduction. Doctor verifies installed configuration, not live host permission enforcement. |
| Codex | MCP, instructions, and lifecycle command hooks | Setup writes Codex MCP registration and `.codex/hooks.json`. Enable hooks, trust the project, and review handlers in Codex `/hooks`. Capture and handoff use the native lifecycle JSON contract; hooks preserve the original command and host execution policy. Doctor cannot verify host trust or execution. |
| Windsurf | MCP/rules verified, command rewrite guidance-only | Doctor validates config, rules, daemon/index, and MCP initialize/tools-list from generated config. Packet28 does not claim command interception. |

## Local Storage

Packet28 stores memory, feedback, transcript, and graph data locally at:

```text
~/.packet28/packet28.db
```

The SQLite schema includes:

- `events`
- `commands`
- `reductions`
- `memories`
- `memory_chunks`
- `memory_embeddings`
- `memories_fts`
- `concepts`
- `concepts_fts`
- `relations`
- `feedback`
- `feedback_fts`
- `agent_sessions`
- `transcript_sessions`
- `transcript_messages`
- `transcript_messages_fts`
- `mcp_calls`

Reducer run savings are stored repo-locally under `.packet28/run-savings.jsonl`.

## Explicit Deferrals

- Full RTK command catalog, custom TOML filters, telemetry, and release packaging.
- Stronger ICM embedding backends, broader wake-up project metadata and runtime fixtures, semantic memoir distillation, cloud/import/upgrade, and web dashboard.
- Windsurf hook command interception until a real runtime test proves it.
