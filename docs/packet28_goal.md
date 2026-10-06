# Packet28 Full RTK + ICM Parity Goal

## Objective

Turn Packet28 into an end-to-end local AI coding context product with full Packet28-native parity against:

- RTK: https://github.com/rtk-ai/rtk
- ICM: https://github.com/rtk-ai/icm

Packet28 should not become a source clone. It should match the useful behavior and user-facing capability of RTK and ICM using Packet28 packets, schemas, daemon, MCP server, reducers, local storage, runtime hooks, and editor integrations.

The product thesis:

Packet28 is the local context operating system for coding agents:

- compress what agents see
- remember what matters
- expose context through MCP
- prove every editor integration works
- show local savings, sessions, memory, and integration health

## Current Reality

Packet28 is useful today as a context-reduction, search, and runtime helper. It is not yet a magic replacement for all agent workflows.

Some queries and workflows will still be better handled by native tools, especially when the agent needs exact raw output, broad exploratory control, or tool-specific behavior that Packet28 has not modeled yet. Packet28 needs more repeated real-world runs across different repositories, agents, and task shapes before it should be called fully mature.

Do not claim full RTK or ICM parity until the claim is backed by:

- current source inspection of both upstream repos
- a complete parity matrix in `docs/PARITY.md`
- passing targeted tests for each claimed equivalent
- real-world experiment artifacts showing measured utility
- release smoke evidence after publishing

## Reference Sources

Before changing Packet28 code, inspect the current upstream sources:

1. RTK repo: https://github.com/rtk-ai/rtk
   - Identify CLI proxy behavior, command rewriting, reducer surface, hooks, supported commands, savings analytics, session/discover behavior, config, supported agents, tests, and architecture.

2. RTK docs: https://www.rtk-ai.app/docs/
   - Extract user-facing commands, setup flows, support promises, and product expectations.

3. ICM repo: https://github.com/rtk-ai/icm
   - Identify memory model, SQLite schema, FTS/vector search, MCP tools, graph/memoir model, feedback tools, init flow, supported editors, tests, and architecture.

4. ICM product page: https://www.rtk-ai.app/icm/
   - Extract user-facing memory, graph, feedback, MCP, and dashboard expectations.

Use these sources to derive a parity contract. Do not copy code blindly.

## Full Parity Definition

Full parity means every meaningful RTK and ICM capability is either implemented in Packet28, documented as implemented differently with equivalent user value, or explicitly accepted as a non-goal.

### RTK Parity Areas

Packet28 must cover:

- CLI proxy and command wrapper behavior through `packet28 run <command...>` and runtime hooks installed by `Packet28 setup`.
- Command rewrite planning through `packet28 rewrite "<command>"`, hook rewrites, and MCP/runtime equivalents.
- Output reducers that emit compact human output, structured packets, raw artifact recovery pointers, token estimates, savings percent, provenance, exit code, and fallback reason.
- Supported command families: git, cargo, npm, pytest, grep/rg, find, ls/tree, cat/head/tail, docker, and GitHub CLI.
- Savings analytics through `packet28 gain`, missed-savings discovery through `packet28 discover`, and session analytics through `packet28 session`.
- Configurable filtering/reduction policy, including any upstream RTK custom filter or config concepts that materially affect user workflows.
- Agent/editor integrations for Claude, Cursor, Codex, Windsurf, OpenCode, Gemini, Copilot, Cline, Roo, Kilo, and Antigravity where supported by upstream expectations.
- Safe fallback behavior that preserves original exit codes, does not hide critical errors, and records why reduction was skipped.
- Local-first privacy behavior with no telemetry, signup, cloud dependency, or API key requirement for the core product.

### ICM Parity Areas

Packet28 must cover:

- Local permanent memory through `packet28 memory store`, `packet28 memory recall`, `packet28 memory list`, and MCP equivalents.
- Topics, importance, timestamps, source metadata, decay fields, and recall scoring.
- Memory consolidation through `packet28 memory consolidate`.
- Local SQLite storage at `~/.packet28/packet28.db`, with FTS/BM25 search and a vector-ready schema.
- Knowledge graph and memoir-style behavior through `packet28 graph create`, `packet28 graph add-concept`, `packet28 graph link`, and `packet28 graph inspect`.
- Typed relations such as `depends_on`, `part_of`, `refines`, `contradicts`, and `supersedes`.
- Feedback and correction loop through `packet28 feedback record`, `packet28 feedback search`, and `packet28 feedback stats`.
- Transcript, command, reduction, agent session, and MCP-call history sufficient to support recall, audit, and dashboard views.
- Wake-up/context-pack extraction through `packet28 wakeup`.
- MCP tools for search, reduce, rewrite, memory, feedback, graph, handoff, and doctor operations.
- Local dashboard or TUI showing savings, missed savings, sessions, memory, graph, feedback, MCP activity, and integration health.

## Workstreams

### 1. Source-Backed Parity Audit

Create or update `docs/PARITY.md` so it is the source of truth for parity.

The matrix must include:

| Upstream | Capability | Command/API/tool | Upstream source files/docs | Packet28 equivalent | Status | Test or evidence |
|---|---|---|---|---|---|---|

Rules:

- Clone or inspect current `rtk-ai/rtk` and `rtk-ai/icm` before updating the matrix.
- Mark each item as `implemented`, `partial`, `missing`, or `non-goal`.
- Include a test name, experiment artifact, or explicit implementation issue for every item.
- Remove any language that implies Packet28 is at full parity when only the current acceptance slice is covered.

### 2. RTK Runtime And Reduction Parity

Implement or complete:

- `packet28 rewrite "<command>"`
- `packet28 run <command...>`
- runtime hook rewrites installed by `Packet28 setup`
- `packet28 gain`
- `packet28 discover`
- `packet28 session`
- reducer coverage for the RTK command families listed above
- raw-output artifact capture and recovery through Packet28 artifact IDs
- fallback recording for every unsupported or unsafe command

Acceptance tests must prove:

- failing exit codes are preserved
- critical errors are not hidden
- unsupported commands pass through safely
- raw output remains recoverable
- compact output is materially smaller than raw output for noisy commands
- reducer drift fixtures fail when compact output loses decisive status counts or failing-test lines
- hook-driven operation works where Packet28 claims transparent runtime support

### 3. ICM Memory, Graph, And Feedback Parity

Implement or complete:

- SQLite-backed memory tables for events, commands, reductions, memories, memory chunks, concepts, relations, feedback, agent sessions, and MCP calls
- FTS/BM25 recall and vector-ready schema fields
- CLI and MCP memory store/recall/list/consolidate flows
- graph creation, linking, relation typing, and inspection
- feedback recording, search, stats, and application counts where useful
- transcript/session capture that feeds recall and dashboard views
- `packet28 wakeup` for context-pack extraction

Acceptance tests must prove:

- memory recall works through CLI and MCP
- memory lint flags stale runtime-specific advice without rejecting generic project memories
- feedback is searchable and summarized
- graph relations are inspectable
- consolidation does not erase important source metadata
- wake-up packs contain relevant recent context and durable memories

### 4. Integration, Setup, And Doctor Reliability

Implement or harden:

- `packet28 init --agent <agent>`
- `packet28 init --mode all`
- `packet28 setup --runtime <agent> --yes`
- `packet28 doctor --agent <agent> --root .`
- `packet28 mcp smoke-test --from-config <agent>`

For Windsurf, do not claim full transparent rewrite/hook support unless a real test proves command interception.

Windsurf acceptance bar:

- MCP config works
- rules are installed
- existing MCP servers are preserved
- generated config completes MCP initialize and tools/list
- `packet28 doctor --agent windsurf --root .` passes in a clean fixture
- Packet28 clearly reports rewrite support as `guaranteed`, `best-effort`, or `rules-only/guidance-only`

### 5. Real-World Experiment Harness

Build a repeatable experiment suite that runs Packet28 against real repositories and task types.

Required experiment dimensions:

- repositories: Packet28 itself plus at least two unrelated non-trivial repos
- workflows: search, code review, failing test triage, implementation, docs lookup, and handoff/bootstrap
- comparisons: native tool path versus Packet28 path
- metrics: raw estimated tokens, compact estimated tokens, savings percent, fallback count, indexed-search hit rate, elapsed time, failed commands, and raw artifact recovery success
- artifacts: committed or archived summaries under `docs/experiments/` or another documented evidence path

Minimum maturity gate:

- at least three repeated runs per workflow
- no unexplained daemon/index readiness failures
- broad search uses indexed Packet28 search when claimed
- fallback reasons are explicit
- Packet28 output is sufficient for the task without hiding correctness-critical information

Current evidence:

- `docs/experiments/run_packet28_real_repo_suite.sh` runs the repeatable real-repository suite against clean temporary checkouts of Packet28, ripgrep, and fd.
- `docs/experiments/real-repos/SMOKE_20260511.md` and `docs/experiments/real-repos/SMOKE_20260511.jsonl` record a 3x run across the required workflows. The `Packet28 run --json` path had 0 fallbacks, 0 failed commands, and raw artifact recovery for 54/54 runs.
- The latest run had 9/9 indexed `p28` hits and 0 indexed-search readiness fallbacks after `p28` was changed to wait for daemon index readiness before falling back.
- Use `fff` (`dmtrKovalenko/fff`) instead of rebuilding the search primitive where it is available. Packet28 has an opt-in `p28 --engine fff` MCP adapter, and local fake-MCP plus real upstream `fff-mcp` smoke tests show Packet28-shaped `engine=fff_mcp` output works. `docs/experiments/search-backends/FFF_COMPARE_20260511/summary.md` adds 81 repeated real-repo comparison rows across native `rg`, default `p28`, and `p28 --engine fff`. `p28 --engine auto` selects `fff-mcp` for broad/common-literal planner fallbacks when `fff-mcp` is available, and `P28_FFF_AUTO=prefer` makes auto mode try `fff-mcp` first before falling back to Packet28's native indexed/reducer path. Full adoption still needs broader comparisons that prove the adapter preserves Packet28 reducer packets, raw artifact recovery, fallback reasons, local-first daemon semantics, and parity evidence while improving search behavior. Evidence: `p28_fff_engine_adapts_mcp_grep_results`, `p28_auto_uses_fff_for_broad_index_fallback_when_available`, and `p28_auto_can_prefer_fff_when_configured`.
- The `fff` adapter now applies Packet28 requested-path filters locally after parsing MCP grep output, so a backend response containing broader matches cannot leak irrelevant files into compact agent context. Evidence: `p28_fff_engine_respects_requested_paths`.
- `P28_FFF_AUTO=prefer` now records explicit search fallback provenance when the preferred `fff-mcp` backend is missing or fails before Packet28 falls back to native indexed or legacy search. Evidence: `p28_auto_prefer_records_fff_backend_failure_before_native_fallback`.
- RTK system-command coverage now includes top-level `Packet28 read`, `Packet28 summary`, `Packet28 find`, `Packet28 grep`, `Packet28 json`, `Packet28 deps`, `Packet28 env`, and `Packet28 log`: compact file reading with line windows and lightweight language-aware filtering, command-output summary detection with exit-code preservation, compact find output for common native and Packet28 find syntax, grouped grep output with truncation and file-type filters, JSON compaction/schema rendering with non-JSON extension rejection, dependency manifest summaries for Cargo/package.json/requirements.txt/pyproject.toml/go.mod, categorized environment output with secret masking by default, and log deduplication that normalizes timestamps, IDs, large numbers, hex values, and paths. `Packet28 grep --engine fff` reuses the existing `p28 --engine fff` / `fff-mcp` adapter, so fff remains one optional search backend rather than a second bespoke implementation. Evidence: `cmd_system::tests::*`, `test_system_read_command_filters_and_numbers_files`, `test_system_summary_command_preserves_exit_and_summarizes_output`, `test_system_find_command_supports_native_find_shape`, `test_system_grep_command_groups_matches`, `test_system_grep_fff_engine_delegates_to_p28`, `test_system_json_deps_and_env_commands`, and `test_system_log_command_deduplicates_noisy_lines`.
- RTK formatter command coverage now includes top-level `Packet28 prettier` and `Packet28 format`, preserving formatter exit codes while rendering compact command summaries. `Packet28 format` accepts an explicit formatter name such as `prettier`, `black`, `ruff`, or `biome`, and otherwise makes a local best-effort formatter choice from project files. Evidence: `test_system_prettier_and_format_commands_wrap_formatter_output`.
- RTK language-tool wrapper coverage now includes top-level `Packet28 npm`, `Packet28 npx`, `Packet28 tsc`, `Packet28 vitest`, `Packet28 pytest`, and `Packet28 ruff`, preserving child exit codes and compacting common JS/Python test, typecheck, and lint output through Packet28's local summary surface. Evidence: `test_system_language_tool_commands_wrap_common_rtk_tools`.
- RTK system/infra wrapper coverage now includes top-level `Packet28 ls`, `Packet28 tree`, `Packet28 wc`, `Packet28 wget`, `Packet28 curl`, `Packet28 docker`, `Packet28 kubectl`, `Packet28 gh`, `Packet28 glab`, `Packet28 aws`, `Packet28 psql`, `Packet28 git`, and `Packet28 gt`, using Packet28's reducer API for compact directory listings, trees, counts, download summaries, HTTP title/byte summaries, container lists, Kubernetes pending-row summaries, GitHub/GitLab lists, AWS summaries, SQL table summaries, and Git/Graphite summaries. Evidence: `test_system_infra_and_count_commands_use_reducer_wrappers`.
- RTK build/lint wrapper coverage now includes top-level `Packet28 cargo`, `Packet28 go`, `Packet28 dotnet`, `Packet28 golangci-lint`, `Packet28 gradlew`, `Packet28 bundle`, `Packet28 rake`, `Packet28 rspec`, `Packet28 rubocop`, `Packet28 mypy`, `Packet28 pip`, and `Packet28 pnpm`, using Packet28's reducer API for compact Rust, Go, .NET, Gradle, Ruby, Python, and JS package-manager summaries. Evidence: `test_system_build_and_lint_commands_use_reducer_wrappers`.
- RTK built-in-filter wrapper coverage now includes top-level `Packet28 brew`, `Packet28 composer`, `Packet28 df`, `Packet28 du`, `Packet28 make`, `Packet28 mvn`, `Packet28 poetry`, `Packet28 uv`, `Packet28 terraform`, and `Packet28 tofu`, reusing Packet28's trusted/project/global/built-in TOML filter pipeline for commands where a full reducer would add little value. Evidence: `test_system_filter_backed_rtk_wrappers_use_builtin_filters`.
- A second RTK built-in-filter wrapper batch adds `Packet28 ansible-playbook`, `Packet28 fail2ban-client`, `Packet28 gcloud`, `Packet28 hadolint`, `Packet28 helm`, `Packet28 iptables`, `Packet28 markdownlint`, `Packet28 mix`, `Packet28 ping`, `Packet28 pio`, `Packet28 pre-commit`, `Packet28 ps`, `Packet28 quarto`, `Packet28 rsync`, `Packet28 shellcheck`, `Packet28 shopify`, `Packet28 sops`, `Packet28 systemctl`, `Packet28 trunk`, `Packet28 yamllint`, and `Packet28 liquibase`. Evidence: `test_system_more_filter_backed_rtk_wrappers_use_builtin_filters`.
- A final built-in-filter wrapper batch adds `Packet28 basedpyright`, `Packet28 biome`, `Packet28 gcc`, `Packet28 g++`, `Packet28 gradle`, `Packet28 jira`, `Packet28 jj`, `Packet28 jq`, `Packet28 just`, `Packet28 mise`, `Packet28 nx`, `Packet28 ollama`, `Packet28 oxlint`, `Packet28 skopeo`, `Packet28 ssh`, `Packet28 stat`, `Packet28 swift`, `Packet28 task`, `Packet28 turbo`, `Packet28 ty`, `Packet28 xcodebuild`, and `Packet28 yadm`, and fixes the built-in GCC filter so `g++` commands match the intended compiler filter. Evidence: `test_system_remaining_filter_backed_wrappers_use_builtin_filters`.
- RTK pipe-mode coverage now includes `Packet28 pipe`, with stdin filtering, `--passthrough`, local auto-detection for grep/find/test/json/log-shaped output, and named filters for common RTK pipe filter names. Evidence: `test_system_pipe_filters_stdin_like_rtk_pipe`.
- RTK rewrite registry coverage now includes a source-backed representative for every upstream `RULES` entry from `src/discover/rules.rs`; this audit exposed and fixed a missing `pnpm install/list/outdated` route through a new built-in filter. Evidence: `routes_every_upstream_rtk_rule_representative`.
- RTK installed-runtime coverage now includes a bounded Claude Code `2.1.139` smoke where the real runtime invoked Packet28 `SessionStart`, `UserPromptSubmit`, and `PreToolUse:Bash` hooks and received a reducer-runner rewrite for `git status --short`; a Gemini `0.30.0` isolated-home setup/doctor smoke that verifies required `gemini_hook_config`/`runtime_rewrite_support`; and an OpenCode `1.14.48` isolated-home smoke where `opencode debug config` reports the generated `packet28.ts` plugin as loaded. Evidence: `docs/experiments/runtime-live/SMOKE_20260512.md`, `test_setup_gemini_writes_before_tool_hook_and_prompt`.
- RTK analytics coverage now includes `Packet28 cc-economics`, which merges local Packet28 run-savings records with ccusage-style daily/weekly/monthly Claude Code spend JSON, computes weighted input-token cost per token, estimates dollar savings, supports text/JSON/CSV output, and gracefully reports local savings when ccusage is unavailable. Evidence: `test_cc_economics_merges_ccusage_and_packet28_savings`.
- RTK `gain` coverage now includes direct RTK-style mode flags and short aliases in addition to Packet28's `--format` selector: `--graph/-g`, `--history/-H`, `--failures/-F`, `--daily/-d`, `--weekly/-w`, `--monthly/-m`, `--quota/-q`, and `--all/-a`; graph mode renders a human-facing savings summary with per-route impact bars while CSV remains available through `--format csv`; quota mode accepts RTK-style `--tier/-t pro|5x|20x` plus Packet28's explicit `--quota-tokens` override; `--reset --yes` clears local Packet28 run-savings records. Evidence: `test_run_reduces_git_status`; `test_gain_reports_failed_and_fallback_runs`.
- Packet28 gain JSON now exposes `route_roi`, a per-route ledger of invocation count, compact/raw token totals, saved tokens, savings percent, average saved tokens per invocation, downstream follow-up count, and average follow-ups per invocation so agents can prefer historically high-value context routes. Evidence: `test_run_reduces_git_status`.
- Packet28 dashboard now surfaces the top saved context routes in JSON, text, HTML, and TUI output so high-ROI reducer paths are visible outside the gain command. Evidence: `test_dashboard_shows_local_product_metrics`.
- RTK `gain` disabled-bypass coverage now warns in text mode when recent Claude-style sessions contain unnecessary active `PACKET28_DISABLED`/`RTK_DISABLED` prefixes on Packet28-supported commands, while JSON mode stays machine-clean. Evidence: `test_gain_warns_about_disabled_bypass_sessions_like_rtk`.
- Failed `Packet28 run` records now carry a stable `failure_fingerprint`, and `Packet28 gain --failures` includes fingerprint and repeat-count columns so recurring failures can be grouped for future local advice. Evidence: `test_gain_reports_failed_and_fallback_runs`.
- `Packet28 gain --failures` now also reports the next successful command, changed paths, and a compact `fix_advice` hint in the same working directory after each failure/fallback record, giving repeated failure fingerprints a concrete likely-fix command and edited-file clue without replaying full logs. Evidence: updated `test_gain_reports_failed_and_fallback_runs`.
- `Packet28 gain --failures --remember-advice` now persists confirmed repeated-failure fixes as local feedback memory when a repeated fingerprint has a later successful same-directory run, so durable feedback search and wake-up paths can reuse the fix advice. Evidence: `test_gain_reports_failed_and_fallback_runs`.
- Broker context now includes a derived `failure_advice` section from recent Packet28 run-savings failures, so repeated failure fingerprints and likely fix commands can influence agent planning without requiring a separate `gain --failures` call. Evidence: `broker_context_surfaces_failure_advice_from_run_savings`.
- `Packet28 verify experiments` now validates JSON experiment manifests for covered and explicitly required workflows, command evidence, existing artifact paths, duplicate IDs, metric thresholds, per-metric artifact evidence strings, artifact-backed allowed fallback reasons, runtime versions, and unexpected fallback reasons, making optimization claims auditable against concrete files. Evidence: `test_verify_experiments_checks_manifest_evidence`.
- `Packet28 verify experiments --score` now adds a compact replay scorecard for each workflow, including reproducibility, artifact backing, fallback clarity, freshness, total score, and passing status so experiment evidence can be ranked without rereading every artifact. Evidence: `verify_experiments_score_passes_backed_artifact_under_compact_budget`; `verify_experiments_score_fails_missing_evidence_below_threshold`.
- `docs/experiments/manifest.json` now records a verifier-backed manifest for runtime-live, repeatable fixture-suite, real-repo, fff search-backend, plan-validation, Claude Code navigation, and release-smoke experiments with artifact paths, commands, runtime versions, and metric gates. Evidence: `Packet28 verify experiments --root . --json`.
- `Packet28 doctor` now includes an advisory `experiment_manifest` check so agents can discover whether the repo's experiment evidence is verifier-backed during normal health checks. Evidence: `test_packet28_doctor_reports_healthy_stack`; `test_packet28_doctor_reports_healthy_runtime`.
- MCP now exposes `packet28.verify_experiments` so agent workers can audit the experiment manifest and required workflow coverage without shelling out. Evidence: `verify_experiments_tool_returns_manifest_status`; `tools_list_exposes_product_compatibility_aliases`.
- MCP now exposes `packet28.recommend_next_tool`, which ranks compact next-command suggestions from local run-savings ROI, repeated-failure success advice, and focused changed-path freshness so agents can choose a cheaper or safer next tool directly. Evidence: `recommend_next_tool_changes_with_focus_freshness_and_roi`.
- MCP now exposes `packet28.validate_tool_outcome`, which classifies the latest matching Packet28 run-savings outcome as success, fallback, missing artifact, stale artifact, or failure before agents rely on it. Evidence: `validate_tool_outcome_does_not_treat_fallback_as_success`.
- MCP now exposes `packet28.patch_risk`, which scores pre-edit path risk from shared-path heuristics, cached testmap mappings, missing mappings, and recent failure/fallback records while returning required checks. Evidence: `patch_risk_requires_broader_checks_for_shared_unmapped_paths`.
- MCP now exposes `packet28.verify_handoff`, which reads a stored handoff context artifact and scores whether objective, next-action, debt, and evidence signals are present before a fresh worker replays it. Evidence: `verify_handoff_fails_when_next_action_is_missing`.
- MCP now exposes `packet28.prompt_pressure`, which estimates a stored handoff context plus next worker prompt against a target token budget and names the largest removable sections when the packet is too heavy. Evidence: `prompt_pressure_identifies_largest_removable_section`.
- MCP now exposes `packet28.handoff_diff`, which compares two stored handoff context artifacts and reports compact objective, next-action, evidence-handle, and debt-signal deltas. Evidence: `handoff_diff_reports_changed_next_action_as_top_delta`.
- MCP now exposes `packet28.handoff_compress`, which recommends non-critical handoff sections to drop while preserving objective, next-action, debt, and evidence-freshness anchors. Evidence: `handoff_compress_preserves_objective_and_next_action_sections`.
- MCP now exposes `packet28.handoff_lint_dependencies`, which flags artifact handles referenced in a handoff brief or section text but absent from the packet's available evidence handles. Evidence: `handoff_dependency_lint_flags_missing_artifact_handle`.
- MCP now exposes `packet28.handoff_lint_paths`, which flags repo-relative path references in a handoff brief or section text when they are absent on disk and absent from changed-path metadata. Evidence: `handoff_path_lint_flags_missing_path_reference`.
- MCP now exposes `packet28.handoff_lint_tests`, which flags test-like names mentioned in handoff text when they are not backed by a runnable test command line. Evidence: `handoff_test_lint_flags_named_test_without_command`.
- MCP now exposes `packet28.handoff_lint_stale_commands`, which flags handoff command references whose persisted command event predates the latest relevant edit event. Evidence: `handoff_stale_command_lint_flags_pre_edit_command`.
- MCP now exposes `packet28.handoff_lint_environment`, which flags handoff command references that depend on unset environment variables or missing executables before fresh-worker replay. Evidence: `handoff_environment_lint_flags_missing_env_var`.
- MCP now exposes `packet28.handoff_lint_all`, which runs replay readiness, dependency, path, test, stale-command, and environment checks and returns one compact blocked/ready decision with failing categories. Evidence: `handoff_lint_all_reports_exact_failing_categories`.
- MCP now exposes `packet28.handoff_fix_plan`, which converts failing handoff lint categories into compact repair actions for artifacts, paths, tests, stale commands, environment setup, and replay packet structure. Evidence: `handoff_fix_plan_recommends_path_test_and_env_repairs`.
- MCP now exposes `packet28.handoff_repair_verify`, which compares aggregate handoff lint state before and after a repair and reports cleared, remaining, and regressed categories. Evidence: `handoff_repair_verify_reports_cleared_categories`.
- MCP now exposes `packet28.handoff_lint_trends`, which scans stored task handoff artifacts and reports latest blockers, recurring lint categories, and categories cleared by later artifacts. Evidence: `handoff_lint_trends_reports_recurring_and_cleared_categories`.
- MCP now exposes `packet28.handoff_lint_regressions`, which flags handoff lint categories that were cleared in an ordered artifact sequence and then reappeared in the latest artifact. Evidence: `handoff_lint_regressions_flags_reintroduced_category`.
- `Packet28 dashboard` now includes a compact `handoff_readiness` tile across JSON, text, TUI, and HTML surfaces, showing latest handoff status, latest blockers, recurring categories, and regression count. Evidence: `test_dashboard_shows_local_product_metrics`.
- `Packet28 verify handoffs` now emits a CI-friendly handoff readiness summary and exits nonzero when regression count exceeds `--max-regressions` or `--require-ready` is violated. Evidence: `test_verify_handoffs_reports_ci_summary_and_threshold`.
- `.github/workflows/handoff-readiness.yml` now runs `target/debug/Packet28 verify handoffs --root . --max-regressions 0` on handoff/dashboard/verify changes so regressions surface in CI. Evidence: clean-root local verifier command plus workflow definition.
- Broker context now includes a compact `context_debt` section that aggregates stale changed paths, open questions, unverified edits, and contradicted hypotheses with payoff commands, so agents can clear debt before handoff or finalization. Evidence: `broker_context_debt_clears_after_reads_questions_and_verification`.
- Broker context now includes `evidence_confidence`, a compact high/medium/low score over freshness, verification evidence, fallback provenance, failures, and artifact availability so agents can distinguish strong proof from weak inferred context. Evidence: `broker_evidence_confidence_scores_stale_or_fallback_below_fresh_success`.
- `prepare_handoff` now returns compact contradiction warnings when active `hypothesis:*` assumptions appear contradicted by recent tool evidence mentioning the hypothesis, related path, or symbol, preventing stale assumptions from crossing worker boundaries silently. Evidence: `prepare_handoff_warns_when_tool_evidence_contradicts_active_hypothesis`.
- `prepare_handoff` now includes a compact readiness score with status and reason codes, derived from handoff readiness, checkpoint presence, open questions, contradiction warnings, next-action clarity, and recent verification evidence. Evidence: `prepare_handoff_readiness_score_rises_after_verification_evidence`.
- Broker plan validation now warns with `stale_evidence_after_checkpoint` when an edit step targets a path changed since checkpoint without refreshed read evidence, carrying evidence freshness into the plan gate instead of only context rendering. Evidence: `validate_plan_warns_when_edit_relies_on_stale_evidence_after_checkpoint`.
- Packet28 now has `hypothesis add/confirm/reject/list` commands that persist task assumptions as broker decision events with `hypothesis:` IDs, letting active assumptions travel through broker context and handoff state. Evidence: `test_hypothesis_cli_tracks_active_assumptions`.
- MCP now exposes `packet28.hypothesis_add`, `packet28.hypothesis_list`, and `packet28.hypothesis_resolve` so agents can manage the same active assumptions without shelling out of the broker tool surface. Evidence: `test_packet28_mcp_hypothesis_tools_track_active_assumptions`; `tools_list_exposes_product_compatibility_aliases`.
- Hypothesis checkpoints can now carry related paths, symbols, and artifact IDs through agent snapshots, broker context rendering, CLI JSON, and MCP list output, making assumptions inspectable against concrete evidence anchors. Evidence: `active_decisions_render_related_paths_and_symbols`; `agenty_state_snapshot_derives_current_task_state`; updated hypothesis CLI/MCP smoke tests.
- RTK session coverage now includes RTK-style text output for `Packet28 session --sessions-dir <dir>`: a session/date/command/adoption/output table, compact adoption bar, average adoption, and discover hint, while preserving JSON output for automation. Evidence: `test_session_reports_adoption_from_session_jsonl`, `test_session_adoption_all_and_since_scan_multiple_session_files`.
- RTK discover/session token estimates now pair Claude-style Bash `tool_use` records with `tool_result` output by ID or chronological order, so savings and session output estimates reflect real output size when session logs provide it instead of only command-string length. Evidence: `test_discover_uses_tool_result_output_size_for_token_estimates`.
- RTK discover/session scanning now recursively finds Claude-style JSONL files under project directories, and `Packet28 session` skips `subagents` paths to match RTK's top-level session adoption view. Evidence: `test_discover_recurses_project_session_dirs_like_rtk`; `test_session_skips_subagent_session_files_like_rtk`.
- RTK analytics/discover/learn/session provider breadth has been audited against upstream: RTK's implemented provider path is Claude Code JSONL via `ClaudeProvider`, so Packet28's matching Claude-style session support is the relevant parity target rather than broader provider coverage. Evidence: upstream `src/discover/provider.rs`, `src/discover/mod.rs`, `src/learn/mod.rs`, and `src/analytics/session_cmd.rs`; Packet28 evidence in discover/session/learn/gain tests below.
- RTK discover recommendations now include Packet28-native `missed_opportunities` for supported session commands that were not already run through Packet28, with grouped command examples, Packet28 equivalents, output-size raw estimates, and estimated saveable tokens in JSON/text output. Evidence: `test_discover_reports_rtk_style_missed_packet28_opportunities`.
- RTK discover bypass warnings now include active `PACKET28_DISABLED`/`RTK_DISABLED` session commands when the underlying command is Packet28-supported, while false values such as `PACKET28_DISABLED=0` continue through normal opportunity detection. Evidence: `test_discover_reports_disabled_bypasses_like_rtk`.
- RTK learn coverage now includes CLI correction detection from Claude-style session history: Packet28 pairs Bash tool uses with tool results, detects fail-then-succeed corrections, classifies common error types, filters by frequency/confidence, emits JSON/text, and can write `.claude/rules/cli-corrections.md`. Evidence: `test_learn_detects_cli_correction_from_session_history`.
- ICM dashboard coverage now includes a local static web-view export: `Packet28 dashboard --format html --output <path>` renders savings, memory, graph, feedback, transcript, noisy command, pending extraction, and integration-health metrics without starting a server or leaving the machine. Evidence: `test_dashboard_shows_local_product_metrics`.
- ICM memory recall and vector-search coverage now uses the stronger local `packet28-local-lexical-v2` embedder: normalized tokens, token parts, character n-grams, and memory metadata are hashed into a normalized local vector, while legacy `packet28-local-hash-v1` rows remain readable. Evidence: `test_memory_store_recall_uses_sqlite_home_db` covers embedding creation plus typo-tolerant vector recall; `test_memory_recall_scores_importance_and_keywords`; `test_mcp_memory_store_recall_uses_sqlite_home_db`.
- ICM FTS coverage now includes calibrated memory recall scoring: FTS/BM25 candidates are blended with local vector candidates and reranked with deterministic importance, weight, metadata, content-term, raw-excerpt, and exact-phrase bonuses, while feedback, concept, and transcript search keep FTS-first behavior with LIKE fallback. Evidence: `test_memory_recall_scores_importance_and_keywords` covers exact phrase ranking; `test_feedback_and_graph_cli_use_sqlite`; `test_mcp_memory_store_recall_uses_sqlite_home_db`.
- ICM memoir graph coverage now includes local-first semantic distillation: topic memories create primary graph concepts, secondary keywords create related concepts in the same memoir, and `mentions` relations link the distilled concept neighborhood without calling a remote LLM. Evidence: `test_feedback_and_graph_cli_use_sqlite`; `test_mcp_memory_store_recall_uses_sqlite_home_db`.
- ICM transcript coverage now includes runtime hook transcripts for both successful `PostToolUse` output and failed `PostToolUseFailure` diagnostics, preserving session, agent, source, and project metadata for FTS-backed transcript search. Evidence: `test_hook_records_local_event_log_stats_and_dashboard_count`; `test_hook_failure_output_is_searchable_transcript_context`; `test_transcript_export_import_round_trip`.
- ICM wake-up coverage now includes deterministic CLI/MCP packs plus Claude SessionStart injection with project filtering and budgeted truncation. Non-Claude runtimes remain honest pre/post-tool rewrite and capture adapters because Packet28 does not claim a compatible SessionStart context-injection contract for them. Evidence: `test_packet28_hook_session_start_injects_wakeup_pack`; `test_mcp_memory_store_recall_uses_sqlite_home_db`.
- ICM wake-up packs now support agentic path/symbol/intent scoping through CLI and MCP arguments, with both rendered packs and structured JSON arrays pruned to matching candidates. Evidence: `test_wakeup_scopes_context_by_path_symbol_and_intent`; `test_packet28_hook_session_start_injects_wakeup_pack`.
- ICM hook lifecycle coverage now includes persisted Claude SessionStart/SessionEnd lifecycle events, PostToolUse/PostToolUseFailure capture into pending extraction/transcripts, hook log/stats reporting, and local deterministic pending extraction processing. Evidence: `test_hook_session_end_is_recorded_in_local_lifecycle_log`; `test_hook_records_local_event_log_stats_and_dashboard_count`; `test_memory_pending_extraction_queue_processes_into_memory`.
- ICM MCP tool-surface coverage now includes Packet28-native recurring memory pattern extraction: `Packet28 memory extract-patterns` and MCP `packet28.memory_extract_patterns` group repeated topic memories by deterministic local keyword/content signals and can materialize those patterns as graph concepts in a selected memoir. Evidence: `test_memory_consolidate_preserves_metadata_and_deletes_sources`; `test_mcp_memory_store_recall_uses_sqlite_home_db`; `cmd_mcp::tests::tools_list_exposes_product_compatibility_aliases`.
- RTK graceful hook degradation coverage now includes missing-binary generated command guards for Claude, Cursor, Copilot, Gemini, and Windsurf plus malformed-JSON fail-open checks for Claude, Cursor, Copilot, and Gemini. Evidence: `generated_packet28_hook_command_exits_zero_when_binary_is_missing`; `test_packet28_hooks_degrade_gracefully_on_bad_json_and_no_rewrite`.
- RTK command rewrite planning now normalizes RTK-style runtime prefixes and binary paths for shared reducer routing: bare `sudo`, shell builtin prefixes such as `noglob`/`command`/`builtin`/`exec`/`nocorrect`, `env VAR=...`, and absolute binary paths such as `/usr/bin/git`. Runtime wrapper prefixes are preserved around emitted Packet28 rewrites. Evidence: `routes_sudo_and_env_prefixed_commands_like_rtk`; `routes_rtk_shell_builtin_prefixes_like_rtk`; `builds_rewrite_with_runtime_wrapper_prefixes`; `normalizes_absolute_binary_paths_like_rtk`.
- RTK command rewrite planning now routes upstream `yadm` git-equivalent commands through Packet28's git reducer instead of falling back to TOML filters. Evidence: `classify_yadm_delegates_to_git_reducers`; `routes_yadm_like_rtk_git_alias`.
- RTK command rewrite planning now distinguishes RTK ignored exact/prefix commands such as `cd`, `echo`, `mkdir`, `python -c`, `node -e`, and `pwd` from unsupported commands. Evidence: `marks_rtk_ignored_commands_as_intentional_passthrough`.
- RTK command rewrite planning now normalizes `golangci-lint` global options before `run`, including `-v`, `--color never`, `--color=never`, `--config=...`, and the `golangci` alias. Evidence: `routes_golangci_global_options_before_run_like_rtk`.
- RTK command rewrite planning now covers upstream `cargo install` as a mutating Rust reducer route with compact install output summaries. Evidence: `classify_and_reduce_cargo_install_as_mutation`; `routes_rtk_cargo_install_as_mutation`.
- RTK command rewrite planning now covers upstream `cargo fmt` as a Rust formatter reducer route, marking normal format runs as mutating and `cargo fmt --check` as non-mutating. Evidence: `classify_and_reduce_cargo_fmt_as_mutation`; `routes_rtk_cargo_fmt_as_mutation`.
- RTK file-read rewrite planning now handles compatible `cat -n <file>` by routing the actual file path through Packet28's native compact read path, and declines incompatible `cat` flags such as `-A`/`--show-all` instead of accidentally reading the flag token. Evidence: `routes_cat_line_numbers_like_rtk_and_declines_incompatible_flags`.
- RTK command rewrite planning now covers upstream GitHub/GitLab CLI fetch forms `gh release list` and `glab ci status` through Packet28's GitHub reducer. Evidence: `reduce_rtk_release_and_glab_ci_status_forms`; `routes_rtk_github_release_and_glab_ci_forms`.
- RTK command rewrite planning now covers upstream `psql` connection forms such as `psql -U postgres -d mydb` and PostgreSQL URLs through Packet28's infra reducer, while preserving raw passthrough for exact-output flags such as `-o`. Evidence: `classify_rtk_psql_connection_forms`; `routes_rtk_psql_connection_forms`.
- RTK command rewrite planning now unwraps upstream JavaScript package-runner prefixes for supported reducer-backed tools: `npm exec`, `npm x`, typo-compatible `npm rum`/`npm urn`, `npm run-script`, `pnpm dlx`, `pnpm exec`, and `pnpx` route tsc, eslint, biome/lint, next build, prisma, vitest, jest, and playwright commands to Packet28 reducers instead of passing them through. Evidence: `classify_javascript_accepts_rtk_package_runner_prefixes`; `routes_rtk_javascript_package_runner_prefixes`.
- RTK command rewrite planning now covers upstream `npm run test` through the existing JavaScript test reducer and generic mutating `npx` tools such as `npx svgo` through a filtered `javascript_npx` reducer instead of raw passthrough. Evidence: `classify_javascript_accepts_rtk_package_runner_prefixes`; `classify_and_reduce_generic_npx_as_mutation`; `routes_rtk_javascript_package_runner_prefixes`.
- RTK command rewrite planning now covers upstream Python package-manager forms for `pip3`, `pip outdated`, `pip install`, `pip show`, and `uv pip list/outdated/install/show`, with reducer summaries for install/show output instead of raw passthrough. Evidence: `classify_python_accepts_rtk_pip_package_manager_forms`; `routes_rtk_python_package_manager_forms`; `reduce_pip_install_and_show_summarize_common_output`.
- RTK command rewrite planning now also covers versioned Python module runners such as `python3.11 -m pytest`, matching upstream RTK's `python[0-9.]* -m pytest` rule instead of only accepting `python` and `python3`. Evidence: `classify_python_accepts_versioned_python_module_pytest_like_rtk`; `routes_rtk_python_package_manager_forms`.
- RTK command rewrite planning now covers upstream `ruff format` as a mutating Python reducer route while leaving `ruff format --diff` raw for exact diff output. Evidence: `classify_and_reduce_ruff_format_as_mutation`; `routes_rtk_ruff_format_as_mutation`.
- RTK command rewrite planning now covers upstream Docker mutation forms `docker build`, `docker compose build`, `docker run`, `docker exec`, and `kubectl apply`, marking the resulting reducer specs as mutating and summarizing common output without treating them as read-only fetches. Evidence: `classify_infra_supports_rtk_docker_and_kubectl_mutation_forms`; `routes_rtk_docker_and_kubectl_mutation_forms`; `reduce_rtk_docker_and_kubectl_mutation_forms`.
- RTK command rewrite planning now covers upstream `swift test` through a built-in verified TOML filter, matching the existing `swift build` filter path while preserving raw artifact recovery through `Packet28 run`. Evidence: `builtin_toml_filters_route_to_packet28_run`; `test_verify_filters_runs_inline_toml_tests`.
- RTK command rewrite planning now handles compound `&&`/`||`/`;` commands with mixed supported/unsupported segments and pipe commands that rewrite the left side while leaving pipe consumers raw before resuming after later chain operators, including Claude-style PreToolUse hook rewriting through the shared planner. Evidence: `builds_compound_rewrite_for_supported_and_mixed_segments`; `builds_pipe_rewrite_for_left_side_only_then_resumes_after_chain_operator`; `pretool_rewrites_supported_compound_command`; `test_top_level_rewrite_handles_compound_commands_like_rtk`.
- RTK learn parity now uses first/two-token base commands, a three-command correction lookahead, command-similarity confidence, TDD/user-rejection filtering, and grouping by base command/error/diff token before applying `--min-frequency`/RTK-compatible `--min-occurrences` and `--min-confidence`. Evidence: `cmd_learn::tests::*`; `test_learn_detects_cli_correction_from_session_history`.
- RTK discover parity now includes the upstream-style `--project`/`-p` session filter so missed-savings scans can be scoped to a Claude project path substring instead of always scanning every project under `--sessions-dir`. Evidence: `test_discover_project_filter_limits_session_scan_like_rtk`.
- RTK discover parity now uses paired Bash tool-result content length for command token estimates and distributes those estimates across chained commands, matching RTK's provider-level output-size accounting more closely while retaining command-length fallback when session output is unavailable. Evidence: `test_discover_uses_tool_result_output_size_for_token_estimates`.
- RTK discover/session parity now follows RTK's provider traversal more closely by recursively collecting project JSONL files, while the session adoption report filters `subagents` paths like upstream `rtk session`. Evidence: `test_discover_recurses_project_session_dirs_like_rtk`; `test_session_skips_subagent_session_files_like_rtk`.
- RTK discover parity now reports a missed-savings opportunity table for commands Packet28 already knows how to reduce but that were run raw in session history, matching the upstream `rtk discover` user flow more closely. Evidence: `test_discover_reports_rtk_style_missed_packet28_opportunities`.
- RTK discover parity now detects disabled-prefix bypasses in session history and reports recoverable savings when `PACKET28_DISABLED` or RTK-compatible `RTK_DISABLED` suppressed reduction for commands Packet28 supports. Evidence: `test_discover_reports_disabled_bypasses_like_rtk`.
- RTK gain parity now includes the upstream-style lightweight disabled-bypass warning path in addition to detailed `Packet28 discover` reporting. Evidence: `test_gain_warns_about_disabled_bypass_sessions_like_rtk`.
- RTK runtime hook delegation evidence now proves Claude, Cursor, Copilot, and Gemini PreToolUse-style adapters route RTK-style prefix/path commands through the same Packet28 planner. Evidence: `runtime_pretool_rewrites_use_shared_route_planner`.
- RTK OpenCode/Hermes plugin delegation now has a safer text-mode rewrite contract: no-rewrite `Packet28 rewrite` emits empty stdout instead of a human status string, so generated plugins preserve unsupported commands while still mutating supported rewrites. OpenCode and Hermes both have plugin-level smokes that verify rewrite and pass-through behavior with mocked runtime command runners. Evidence: `test_top_level_rewrite_prints_empty_stdout_on_no_rewrite`; `opencode_plugin_smoke_rewrites_and_passes_through_empty_stdout`; `hermes_plugin_smoke_rewrites_and_passes_through_empty_stdout`.
- RTK CLI command-family coverage now includes top-level `Packet28 smart <file>` with local two-line heuristic source summaries and top-level `Packet28 err <command...>` with exit-preserving failure summaries and JSON output. Evidence: `smart_summarizes_source_file_with_local_heuristics`; `test_system_smart_command_summarizes_source_file`; `test_system_err_command_preserves_exit_and_summarizes_failure`.
- RTK TOML filter parity now includes the embedded RTK-compatible built-in filter catalog as the last filter tier after trusted project and user-global filters. Built-ins use the same Packet28 raw-artifact-preserving custom-filter path, `Packet28 verify filters --filter <name>` can run their inline tests, and transparent rewrite planning routes matching TOML-filter commands to `Packet28 run` when no reducer/proxy route claims them first. Evidence: `builtin_toml_filters_route_to_packet28_run`; `test_run_applies_builtin_rtk_compatible_toml_filter`.
- RTK CLI command-family matrix coverage is now marked implemented in `docs/PARITY.md`: Packet28 exposes direct wrappers for the reducer-backed RTK commands, the RTK-style pipe mode, and all executable built-in TOML filter wrappers including the later basedpyright/biome/gcc/g++/gradle/jira/jj/jq/just/mise/nx/ollama/oxlint/skopeo/ssh/stat/swift/task/turbo/ty/xcodebuild/yadm group. Evidence: `test_system_language_tool_commands_wrap_common_rtk_tools`; `test_system_infra_and_count_commands_use_reducer_wrappers`; `test_system_build_and_lint_commands_use_reducer_wrappers`; `test_system_filter_backed_rtk_wrappers_use_builtin_filters`; `test_system_more_filter_backed_rtk_wrappers_use_builtin_filters`; `test_system_remaining_filter_backed_wrappers_use_builtin_filters`; `test_system_pipe_filters_stdin_like_rtk_pipe`.
- RTK raw `diff ...` rewrite coverage now routes through Packet28's native `fs_diff` reducer, preserving the existing `Packet28 diff analyze` namespace while covering upstream RTK's raw diff rule with compact output and raw artifact recovery. Evidence: `builtin_toml_filters_route_to_packet28_run`; `test_run_applies_builtin_rtk_compatible_toml_filter`.
- RTK reducer output/raw recovery coverage now asserts family-specific compact summaries or previews plus fetchable raw artifacts across git, fs, rust, JavaScript, Python, Go, Ruby, .NET, JVM, infra, and GitHub reducers. Evidence: `test_run_raw_artifact_available_across_reducer_families`.
- RTK AWS reducer coverage now includes upstream Secrets Manager forms: `aws secretsmanager list-secrets` summarizes secret metadata, and `aws secretsmanager get-secret-value` returns safe metadata while omitting secret payload fields from compact previews. Evidence: `reduce_aws_secretsmanager_list_summarizes_secret_metadata`; `reduce_aws_secretsmanager_get_secret_value_redacts_secret_payload`.
- ICM dashboard parity now includes a tested terminal dashboard mode with interactive navigation across Overview, Memory, Graph, Feedback, and Integrations panels, alongside existing JSON/text/static HTML output. Evidence: `test_dashboard_shows_local_product_metrics`.
- Packet28 broker plan validation now uses cached test-map coverage when validating uncovered edit steps: a later test gate must target the mapped tests for the edited path when Packet28 has that mapping, while generic full-suite test steps remain valid with targeted-test, missing-mapping, and stale-testmap warnings. The response also includes `test_gate_score` so agents can compare mapped, broad, stale, and missing evidence quality directly, and `Packet28 plan validate` exposes that same broker check from the CLI. Evidence: `validate_plan_requires_testmap_mapped_gate_for_uncovered_edits`; `validate_plan_accepts_testmap_mapped_or_generic_test_gate`; `validate_plan_warns_when_testmap_has_no_mapping_for_uncovered_edit`; `validate_plan_warns_when_cached_testmap_is_stale`; `plan_validate_subcommand_parses_inline_steps`; `docs/experiments/plan-validation/20260515/plan-validate-mapped.json`.
- Packet28 MCP now exposes broker plan validation as `packet28.validate_plan`, so agents can ask for dependency, read-before-edit, coverage, testmap, and budget validation before executing an implementation plan. Evidence: `tools_list_exposes_product_compatibility_aliases`.
- Packet28 broker choose-tool and edit contexts include an advisory `action_critic` section for missing tool intent, destructive command shapes, broad unscoped searches, missing edit scope, and read-before-edit gaps. To receive that feedback before an action, the agent must explicitly request broker context or MCP `packet28.action_critic` before the original tool call. Evidence: `choose_tool_action_critic_flags_missing_intent_and_risky_commands`; `edit_action_critic_flags_missing_scope_and_unread_paths`; `tools_list_exposes_product_compatibility_aliases`.
- Claude PreToolUse hooks compute critic guidance before execution, but their `additionalContext` reaches the model alongside the tool result on its next request, as specified in the [Claude Code hooks reference](https://code.claude.com/docs/en/hooks#add-context-for-claude). This feedback is advisory for subsequent actions, not a pre-execution safety gate. Hooks preserve native input and permission authority. Evidence: `pretool_hook_output_surfaces_action_critic_without_rewrite`.
- Packet28 broker edit/inspect context now includes `evidence_freshness` when paths or symbols changed since checkpoint, scoring fresh changed-path reads before marking which paths and symbols should be refreshed before relying on cached evidence. Evidence: `broker_edit_context_surfaces_evidence_freshness_for_changed_paths`.
- Packet28 relevant-context rendering now marks cached snippets that mention changed-since-checkpoint paths as `stale_after_change` unless that path has a fresh read, and downranks stale snippets after fresher context in the rendered broker section. Evidence: `relevant_context_marks_and_downranks_stale_changed_path_context`.
- Packet28 reducer search ranking now uses evidence freshness for changed paths, preferring freshly read changed paths over stale changed paths when other reducer-search signals are comparable. Evidence: `search_ranking_prefers_fresh_changed_path_over_stale_changed_path`.
- Packet28 evidence confidence now treats changed symbols as stale evidence signals, rendering confidence for symbol-only changes and lowering confidence until symbol evidence is refreshed. Evidence: `broker_evidence_confidence_penalizes_symbol_only_staleness`.
- Packet28 context debt now emits a direct `stale_symbol` payoff for symbol-only stale evidence, giving agents a concrete inspect/search action even when no changed path is available. Evidence: `broker_context_debt_surfaces_symbol_payoff_without_stale_path`.
- Packet28 evidence confidence now distinguishes artifact-backed symbol verification from unbacked tool claims, keeping changed-symbol verification at medium confidence when no artifact handle or raw artifact is available. Evidence: `broker_evidence_confidence_scores_unbacked_symbol_verification_medium`.
- Packet28 evidence confidence now labels backing directly as `backing=artifact` or `backing=missing` in both the score and reason lines, making artifact-backed versus unbacked symbol evidence readable without interpreting artifact-gap counts. Evidence: `broker_evidence_confidence_orders_symbol_evidence_tiers`.
- Packet28 evidence confidence now renders symbol freshness as `changed_symbols` instead of `stale_symbols`, preserving the freshness signal without implying that successful symbol verification failed. Evidence: `broker_symbol_verification_clears_debt_but_preserves_confidence_staleness`.
- Packet28 now tests the compatibility contract between confidence and debt labels: `changed_symbols` belongs to `evidence_confidence`, while `payoff stale_symbol` remains the context-debt action label. Evidence: `broker_symbol_labels_distinguish_confidence_from_debt_payoff`.
- Packet28 evidence confidence now locks the mixed path/symbol label contract: unread changed paths render as `stale_paths`, while symbol freshness renders as `changed_symbols`. Evidence: `broker_evidence_confidence_distinguishes_stale_paths_from_changed_symbols`.
- Packet28 evidence confidence now emits blocker-specific payoff wording instead of a generic stale/fallback refresh phrase, so medium/low confidence points at stale paths, changed symbols, fallback records, artifact gaps, or failures directly. Evidence: `cargo test -p packet28d broker_evidence_confidence`.
- Packet28 now locks confidence payoff priority directly, so failures outrank freshness, mixed path/symbol freshness outranks fallback and artifact gaps, artifact-backed symbol evidence outranks generic changed-symbol refresh, and usable evidence stays explicit. Evidence: `broker_evidence_confidence_payoff_priority_orders_repair_actions`.
- Packet28 evidence confidence now penalizes repeated artifact gaps more strongly, dropping two unbacked evidence-producing tool calls to medium confidence with an artifact-backed payoff. Evidence: `broker_evidence_confidence_scores_repeated_artifact_gaps_medium`.
- Packet28 evidence confidence now caps missing-backed evidence below high confidence, so even a successful verification without artifact/raw backing reports medium confidence and an artifact-backed payoff. Evidence: `broker_evidence_confidence_caps_missing_backing_below_high`.
- Packet28 evidence confidence now verifies that the missing-backed cap preserves score spread: symbol-only evidence remains medium at 80, unbacked success caps at 84, and failed unbacked symbol evidence remains low at 35. Evidence: `broker_evidence_confidence_missing_backing_keeps_score_spread`.
- Packet28 evidence confidence now includes a compact `risk=` reason code beside the payoff, making score drivers such as failures, mixed freshness, changed symbols, stale paths, and missing backing visible without adding another section line. Evidence: `cargo test -p packet28d broker_evidence_confidence`.
- Packet28 now locks `risk=` priority directly, keeping usable evidence, failures, mixed freshness, stale paths, missing backing, changed symbols, and fallback record risk labels aligned with payoff priority. Evidence: `broker_evidence_confidence_risk_priority_matches_repair_actions`.
- Packet28 now single-sources confidence payoff and risk priority through an internal risk classification, reducing drift between `risk=` and `payoff=` while preserving the compact rendered output. Evidence: `broker_evidence_confidence_payoff_priority_orders_repair_actions`; `broker_evidence_confidence_risk_priority_matches_repair_actions`.
- Packet28 now distinguishes changed-symbol missing backing from generic artifact missing backing in internal risk-class names while keeping both rendered as the compact `risk=missing_backing` label. Evidence: `cargo test -p packet28d broker_evidence_confidence`.
- Packet28 now locks representative confidence reason lines exactly, covering backed success, unbacked changed-symbol success, and mixed freshness with stable `risk=` and `payoff=` output. Evidence: `broker_evidence_confidence_reason_lines_stay_stable`.
- Packet28 now width-bounds representative confidence reason lines that include `risk=` and `payoff=`, keeping the expanded reason signal compact. Evidence: `broker_evidence_confidence_reason_lines_stay_stable`.
- Packet28 now byte-bounds representative confidence bodies after adding `risk=`, keeping backed, unbacked-symbol, and mixed-freshness examples under 512 bytes. Evidence: `broker_evidence_confidence_reason_lines_stay_stable`.
- Packet28 confidence tests now share the `broker_evidence_confidence_` prefix for rendered confidence and helper-priority coverage, so one focused cargo test filter exercises the cluster. Evidence: `cargo test -p packet28d broker_evidence_confidence`.
- Packet28 confidence coverage now has an explicit focused-filter size: `cargo test -p packet28d broker_evidence_confidence` runs 14 confidence tests spanning helper priority, rendered scoring, reason-line stability, and compactness.
- Packet28 confidence test helpers now use the same `broker_evidence_confidence_` prefix as the focused confidence test cluster, including `broker_evidence_confidence_reason_line()`. Evidence: `broker_evidence_confidence_reason_lines_stay_stable`.

### 6. Release Readiness

Before tagging a release that claims parity progress:

- run focused unit/integration tests for setup, MCP, doctor, reducers, search, memory, graph, and feedback
- run `cargo test --workspace` when feasible
- run `cargo clippy --workspace --all-targets -- -D warnings` when feasible
- update `docs/PRODUCT.md`, `docs/ARCHITECTURE_PLAN.md`, and `docs/PARITY.md` if behavior or support tiers changed
- publish only after CI passes
- verify the npm package or install path after release
- record the release smoke evidence

Release `v0.2.54` satisfies these gates for the audited RTK `55f998d` and ICM `92bd851` parity claim. Local `cargo test --workspace` passed; GitHub Hook Benchmark Suite run `25753409843` passed; GitHub Release workflow run `25753420426` passed; GitHub release `v0.2.54` was published; `npm view packet28@0.2.54 version` returned `0.2.54`; and an isolated global npm install smoke verified `packet28 --version`, runtime setup coverage, `rewrite`, `setup --runtime gemini`, and `doctor --agent gemini --json`. Evidence: `docs/experiments/release-smoke/V0_2_54.md`.

## Architecture Target

Additional post-parity ideas for maximizing Packet28's value to coding agents are tracked in `docs/AGENTIC_CONTEXT_IDEAS.md`.

Refactor incrementally toward these boundaries where they reduce complexity:

- `packet28-core`: schemas, `EnvelopeV1`, token estimates, packet formats
- `packet28-reducers`: RTK-style command output reducers
- `packet28-rewrite`: command classification and rewrite decisions
- `packet28-agent`: Claude, Cursor, Codex, Windsurf, OpenCode, Gemini, and other agent adapters
- `packet28-mcp`: MCP tools and stdio server
- `packet28-memory`: SQLite memory, recall, feedback, graph, and wake-up packs
- `packet28-daemon`: indexer, event capture, local API
- `packet28-cli`: init, doctor, setup, run, gain, discover, session, dashboard

If the current workspace uses different crate names, preserve compatibility and refactor incrementally instead of renaming everything at once.

## Commit Rules

Commit per coherent feature. Do not create one giant commit.

Suggested sequence:

1. `docs: define full RTK and ICM parity gates`
2. `docs: update source-backed parity matrix`
3. `test: add setup and MCP smoke coverage`
4. `fix: harden editor setup and doctor checks`
5. `feat: complete command rewrite planner`
6. `feat: complete runtime reducer coverage`
7. `feat: add savings and session analytics`
8. `feat: complete SQLite memory recall`
9. `feat: expose memory and feedback over MCP`
10. `feat: add graph and wakeup flows`
11. `feat: add local dashboard`
12. `test: add real-world Packet28 experiment harness`
13. `docs: update product usage and support tiers`

Every commit must compile, include tests when behavior changes, avoid unrelated formatting churn, and avoid mixing refactor with behavior unless unavoidable.

## Definition Of Done

Packet28 can be described as fully mature and at full RTK + ICM parity only when:

1. `docs/PARITY.md` maps every meaningful current RTK and ICM capability to a Packet28 equivalent, accepted non-goal, or tracked missing item.
2. All claimed RTK runtime, reducer, rewrite, analytics, and session features have tests.
3. All claimed ICM memory, graph, feedback, recall, wake-up, and MCP features have tests.
4. Setup, doctor, and MCP smoke tests pass for every claimed supported agent.
5. Windsurf support is labeled honestly and tested according to its actual interception capability.
6. Real-world experiment artifacts show repeated Packet28 utility across multiple repos and workflows.
7. Raw output recovery works for reduced commands.
8. Fallback reasons are visible and actionable.
9. CI passes.
10. Release smoke verifies the published package or install path.
11. Docs explain exact usage, support tiers, limitations, and remaining non-goals.

As of `v0.2.54`, these gates have passed for the audited RTK and ICM snapshots referenced in `docs/PARITY.md`. Future upstream RTK or ICM changes require a fresh audit before extending the parity claim.

## Non-Goals And Constraints

- Do not add cloud/team analytics yet.
- Do not add telemetry.
- Do not require signup.
- Do not require API keys for core local behavior.
- Do not break Claude, Cursor, Codex, Windsurf, or existing setup behavior while adding parity.
- Do not casually change packet schemas.
- Do not claim full transparent hook support for an editor without proof.
- Do not copy RTK or ICM code blindly.
- Do not hide failures behind pretty output.

## Progress Reporting

Work in checkpoints. After each checkpoint, summarize:

- files changed
- behavior added
- tests run
- commits made
- current failure, if any
- next checkpoint

## Reusable Goal Prompt

```text
/goal Update Packet28 until it can honestly claim full Packet28-native parity with RTK and ICM. Use docs/packet28_goal.md as the roadmap. First inspect current https://github.com/rtk-ai/rtk and https://github.com/rtk-ai/icm source and docs, then update docs/PARITY.md with a complete source-backed matrix. Implement missing RTK runtime/reducer/rewrite/analytics/session behavior and missing ICM memory/graph/feedback/wakeup/MCP/dashboard behavior using Packet28-native architecture. Add tests and repeated real-world experiment artifacts before making maturity claims. Preserve local-first privacy, leave unrelated dirty files untouched, commit per coherent feature, and do not claim full parity until the Definition Of Done in docs/packet28_goal.md is satisfied.
```
