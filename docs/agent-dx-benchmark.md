# Agent DX benchmark

The agent-DX benchmark checks that Packet28 does what a coding agent relies on.
It runs one deterministic workflow against prebuilt binaries, replays frozen
command output through the explicit CLI, and binds the contracts it cannot
create locally to named Rust tests. Correctness, evidence, recovery and
authority gate the result. Savings, latency and round trips are reported but
never gated.

It replaces the hook benchmark suite. That suite gated mean and per-command
token-savings percentages (80, 85 and 90%) chosen in March 2026. It measured
mutable live GitHub output and, for the language fixtures, only the reducer's
summary field. A short live PR description could fail it even when the
renderer behaved correctly.

## Run it

```bash
cargo build --locked -p suite-cli -p packet28d -p packet28-search-cli --bins
cargo test --locked -p packet28d --test task_record_archive --test status_pagination \
  --test registry_repair_startup --test daemon_lifecycle 2>&1 | tee product-tests.log
# ...the other delegated test targets; see .github/workflows/agent-dx-benchmark.yml
python3 scripts/benchmark_agent_dx.py --bin-dir target/debug \
  --product-test-log product-tests.log --artifact-dir .packet28/benchmarks/agent-dx
python3 scripts/validate_agent_dx_benchmark.py .packet28/benchmarks/agent-dx/summary.json \
  --bin-dir target/debug
```

The runner never builds. It needs Git, a Unix socket transport, and no
network or credentials. It strips GitHub and provider tokens, uses an isolated
`HOME`, sets `PACKET28_DAEMON_LOG_MAX_BYTES=4096`, and stops every daemon and
hook server it started. It always writes `summary.json` and exits 1 if a
required check failed. A full run takes about 15 seconds on a debug build.

Pass `--binary-binding` with a build receipt when the binaries were built from
another checkout. Pass `--product-test-tree` when the test log came from
another Git tree. The validator accepts either only if that tree's `crates/`,
`Cargo.toml` and `Cargo.lock` match the measured checkout.

## What is required

`scripts/agent_dx_contract.py` lists every required scenario and check. A
missing scenario, a missing or skipped check, or a scenario that stopped early
fails validation.

| Scenario | The agent can rely on |
| --- | --- |
| `setup_fresh_index` | Setup in a committed repository reports `index ready`. The index attests HEAD plus setup's own files, user source is unchanged, and indexed search finds user code and the generated guidance. |
| `hooks_disabled_honesty` | With hooks disabled, doctor fails and says why without rewriting config, and the generated handler records nothing. Explicit setup re-enables capture, keeps a user handler, captures exactly once, and doctor passes. |
| `hooks_capture_only` | Claude and Codex `PreToolUse` never return `updatedInput`, `permissionDecision` or `decision`, even when a legacy `rewrite_enabled=true` is stored. |
| `native_retrieval` | Slim search stays within its field limits, and the definition the task needs lies beyond the preview. The fetched artifact belongs to the task, is `path:line:text`, is untruncated, and every line equals the source. An explicit read returns the exact lines, including a blank and a non-ASCII line. |
| `same_task_sessions` | Sequential and concurrent fresh MCP processes on one task succeed with distinct artifacts. A later process retrieves every one unchanged. |
| `handoff_cold_restart` | After an intention and a Stop hook, a handoff is ready. When `daemon stop` returns, the instance lock is free, readiness is withdrawn and the process has exited. One `daemon start` succeeds, and a new MCP process resumes the latest intention. |
| `corrupt_history_recovery` | Damaged task history continues through a successor. The damaged bytes are quarantined exactly, inherited context stays readable, and the successor starts at sequence 1. |
| `runtime_log_bounds` | The same daemon process keeps at most four log generations, each within the threshold, through a 128-error burst, and keeps the latest diagnostic. |
| `explicit_cli_reduction` | Every frozen case keeps its derived contract (below). |
| `cleanup` | No owned process or instance lock remains. |

Delegated contracts need seeded storage, fault injection or held locks:
oversized-record archival, 5,000-task paging, registry/journal repair,
lifecycle deadlines, fresh command execution, PR-view edge cases and running
log rotation. The validator requires each named test to exist in the source
and to report `ok` in the supplied `cargo test` log. A test that did not run
fails.

## Explicit CLI corpus

`scripts/benchmark_fixtures/agent_dx/manifest.json` lists each frozen input
with its argv, exit code, SHA-256 hashes and provenance (`captured`,
`reconstruction` or `synthetic`). Each case runs once through the real
`Packet28 <tool> ...` route. A stub executable first on `PATH` replays the
frozen bytes and records its arguments. The stub must be called exactly once
with the recorded arguments. The measurement covers the complete visible
stdout and stderr.

Each contract is derived from the raw input and the documented renderer:

- `gh_pr_view`: identity and URL lines, a body that is a prefix of the
  original within 320 bytes and eight lines, an omission notice exactly when
  something was omitted, and a total within the derived byte budget. A failed
  read keeps all of its stdout, stderr and exit code.
- `gh_pr_list`, `gh_run_list`: the row count and the first row's fields.
- `gh_run_view`: the true job and annotation counts (unindented entries only),
  plus every failed job and failed step. Whether annotation text is kept is
  reported, not gated, because no renderer contract promises it.
- `cargo_test`: pass/fail counts from `test result:` lines, every failing
  test with its panic location, no passing-test lines, and a budget of each
  failure's first five lines.
- `diagnostic_facts`: declared facts must occur in the raw input and in the
  visible output.

`verbose` cases must also be smaller in both bytes and estimated tokens.
`correctness` cases may expand; a failed read or a two-line lint result keeps
everything and adds a summary. The validator recomputes every result from the
persisted streams and the repository manifest.

Refresh a fixture only with a real capture. Never pad or repeat text to change
a ratio. Record the capture time and tool version, update the hashes, and label
any constructed input.

## Reported diagnostics

The summary reports bytes and estimated tokens (`ceil(UTF-8 bytes / 4)`) for
every CLI case, including negative savings. It also reports elapsed time per
call and per scenario, setup, stop and start time, and log generation sizes.
For native retrieval it keeps separate ledgers:

- acquisition: the slim search and glob responses;
- required retrieval: acquisition plus the fetch and read the task needs;
- verification and all-full retrieval: fetches made only to prove evidence
  is recoverable.

Each ledger counts complete JSON-RPC response lines, round trips and elapsed
time. These numbers are not provider token, cost or productivity claims. Any
future floor should come from repeated, reviewed runs of this fixed corpus.

## Evidence and failures

The artifact directory holds `summary.json`, `invocations.json` (argv, exit,
elapsed time and hashed stdout/stderr for every CLI call), complete MCP wire
transcripts, the four streams of every CLI case, copied inputs, and a SHA-256
for every file. Each validator failure line names the contract, the observed
detail and the evidence path, for example:

```text
FAIL explicit_cli_reduction/vitest_one_failure: diagnostic_facts contract (correctness).
required fact 'expected 500 to be 200' is missing from the visible output.
Evidence: .../reduction/vitest_one_failure.reduced.stdout
```

CI publishes the validation report and uploads all artifacts even when a step
fails.
