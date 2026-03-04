# Coverage CLI Suite

This workspace ships four first-class binaries:

- `covy`: all-in-one coverage + diagnostics workflow (ingest, check, report, PR artifacts, path tooling).
- `diffy`: diff-focused coverage/diagnostics gate analysis.
- `testy`: test impact analysis, sharding, and testmap generation.
- `Packet28`: umbrella CLI with domain routing (`Packet28 cover ...`, `Packet28 diff ...`, `Packet28 test ...`, `Packet28 map ...`, `Packet28 proxy ...`).

## When To Use Each

- Use `covy` when you want one CLI handling end-to-end coverage and diagnostics workflows.
- Use `diffy` when you only need diff gate analysis and issue-aware pass/fail output.
- Use `testy` when you are working specifically on impact planning, sharding, and map artifacts.
- Use `Packet28` when you want a single umbrella command namespace split by domain.

## Quickstart (All Binaries)

```bash
# Build all binaries
cargo build --release -p covy-cli -p diffy-cli -p testy-cli -p suite-cli
```

`covy` examples:

```bash
./target/release/covy ingest tests/fixtures/lcov/basic.info --issues tests/fixtures/sarif/basic.sarif
./target/release/covy check tests/fixtures/lcov/basic.info --issues tests/fixtures/sarif/basic.sarif --max-new-errors 0 --json
./target/release/covy report --issues
```

`diffy` examples:

```bash
./target/release/diffy analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --no-issues-state --json
```

`testy` examples:

```bash
./target/release/testy testmap build --manifest artifacts/testmap-manifest.jsonl --output .covy/state/testmap.bin
./target/release/testy impact --base HEAD --head HEAD --testmap .covy/state/testmap.bin --json
./target/release/testy shard plan --shards 4 --tasks-json artifacts/tasks.json --json
```

`Packet28` examples:

```bash
./target/release/Packet28 cover check --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --no-issues-state --json
./target/release/Packet28 diff analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --no-issues-state --json
./target/release/Packet28 test impact --base HEAD --head HEAD --testmap .covy/state/testmap.bin --json
./target/release/Packet28 map repo --repo-root . --json
./target/release/Packet28 proxy run --json -- git status
./target/release/Packet28 map repo --repo-root . --cache --json
./target/release/Packet28 proxy run --cache --json -- git status
./target/release/Packet28 map repo --repo-root . --json --packet-detail rich --pretty
./target/release/Packet28 proxy run --json --packet-detail rich --pretty -- git status
./target/release/Packet28 guard validate --context-config context.yaml
./target/release/Packet28 guard check --packet packet.json --context-config context.yaml
./target/release/Packet28 stack slice --input artifacts/stack.log --json
./target/release/Packet28 build reduce --input artifacts/build.log --json
./target/release/Packet28 --output artifacts/map.json map repo --repo-root . --json
```

Guard policy `context.yaml` canonical V1 shape:

```yaml
version: 1
policy:
  tools:
    allowlist: ["diffy", "contextq"]
  reducers:
    allowlist: ["analyze", "assemble", "contextq.assemble", "diffy.analyze"]
  paths:
    include: ["src/**"]
    exclude: ["src/private/**"]
  token_budget:
    cap: 5000
  runtime_budget:
    cap_ms: 5000
  tool_call_budget:
    cap: 10
  redaction:
    forbidden_patterns: ["(?i)password", "(?i)secret"]
  human_review:
    required: false
    on_policy_violation: true
    on_budget_violation: true
    on_redaction_violation: true
    paths: ["src/security/**"]
```

## Governed Local Workflow (Usable V1)

1. Validate policy config:

```bash
./target/release/Packet28 guard validate --context-config context.yaml
```

2. Run diff analysis through the governed kernel path:

```bash
./target/release/Packet28 diff analyze \
  --coverage tests/fixtures/lcov/basic.info \
  --base HEAD \
  --head HEAD \
  --no-issues-state \
  --json \
  --context-config context.yaml \
  --context-budget-tokens 5000
```

3. Run impact analysis through the same governed kernel path:

```bash
./target/release/Packet28 test impact \
  --base HEAD \
  --head HEAD \
  --testmap .covy/state/testmap.bin \
  --json \
  --context-config context.yaml \
  --context-budget-tokens 5000
```

4. Validate a packet directly against policy:

```bash
./target/release/Packet28 guard check --packet packet.json --context-config context.yaml
```

5. Assemble one or more packet files with a hard budget:

```bash
./target/release/Packet28 context assemble \
  --packet a.json \
  --packet b.json \
  --budget-tokens 5000 \
  --budget-bytes 32000

# Optional governed assembly with policy enforcement + audit metadata
./target/release/Packet28 context assemble \
  --packet a.json \
  --packet b.json \
  --context-config context.yaml
```

`Packet28 diff analyze` and `Packet28 test impact` now both route reducers through `context-kernel-core`, and when `--context-config` is set they continue into governed `contextq` assembly with kernel governance audit metadata.

Machine-mode shape and exits for repeatable local demos:

- Scoped machine commands emit one wrapper: `schema_version: "suite.packet.v1"`, `packet_type`, and `packet`.
- Optional debug/audit/cache metadata is placed at `packet.payload.debug`.
- `--json` and `--json=compact` are bounded compact mode defaults.
- `--json=full` emits full payload projection.
- `--json=handle` emits compact payload + `payload.artifact_handle` and persists full artifact under `.packet28/artifacts/`.
- `Packet28 packet fetch --handle <id> --json=full|compact` expands handle artifacts.
- `--packet-detail` and `--report json` are accepted one-release compatibility shims and map internally to profile behavior.
- `--legacy-json` emits previous command-specific top-level JSON shapes for one release.
- Add `--pretty` for human-readable JSON formatting.
- `Packet28 cover check` and `Packet28 diff analyze` default to terminal output unless `--json` or `--report json` is set.
- `Packet28 --output <path> ...` redirects stdout to a file sink for scripting and multi-step pipelines.
- `Packet28 map repo --cache` and `Packet28 proxy run --cache` enable disk-backed kernel cache reuse under `.packet28/packet-cache-v1.bin`.
- `guard` commands support `--context-config`; legacy `--config` remains supported for compatibility.
- `contextq` budget trim metadata is emitted under `budget_trim` (truncated/dropped/estimated/cap fields).
- Exit codes are stable: `0` success, `1` policy/gate denial, `2` usage/runtime errors.

Phase 1 protocol docs:

- `docs/packet-envelope-v1.md`
- `docs/machine-output-contract.md`
- `docs/wire-profiles.md`
- `docs/schema-registry.md`
- `docs/ci-agent-examples.md`

## Next Reducer Wave (V1)

These reducers now run through the same governed kernel path as `diff` and `test`:

```bash
./target/release/Packet28 stack slice \
  --input artifacts/stack.log \
  --json \
  --context-config context.yaml

./target/release/Packet28 build reduce \
  --input artifacts/build.log \
  --json \
  --context-config context.yaml
```

- `stacky` parses stack traces/failing logs, normalizes frames, dedupes repeated failures, fingerprints similar failures, and surfaces first actionable frames.
- `buildy` parses compiler/linter output, dedupes repeated diagnostics, groups root causes, and ranks fix order.

## Recent Changes (v0.2.0)

- `covy ingest` now auto-resolves `[ingest].report_paths` from `covy.toml` when positional coverage paths are omitted.
- `covy ingest` now returns clearer "no input files" errors with matched-config hints.
- `covy report` adds `--below <percent>` to focus on low-coverage files.
- `covy report` adds `--summary-only` to print only total coverage (terminal or JSON).
- CLI tracing ergonomics improved for automation: `--json` / `-q` default to warn-level logs, and `COVY_LOG` overrides the filter.
- Added `JavaTest/` Maven example project with JaCoCo + SARIF artifacts for Java workflow validation.
- Removed stale benchmark submodule pointers (`commons-lang-rebench`, `commons-lang-sharded`).
- Workspace and crate metadata updated for crates.io release, with version bumped to `0.2.0`.

## Performance Notes (Important)

- Don’t use `cargo run` for perf measurements; it adds startup overhead per invocation.
- In this repo, that overhead is roughly `~0.6s` per command.
- Use the built binary (`target/debug/covy` or `target/release/covy`) for timing and CI perf checks.
- For repeated checks, ingest diagnostics once and let `check/diff/github` reuse `.covy/state/issues.bin` by default.
- Passing `--issues <sarif>` to `check` forces SARIF parse again on each invocation (slower, expected).

Example:

```bash
# Good for perf
./target/release/covy check tests/fixtures/lcov/basic.info --base HEAD --head HEAD --json > /dev/null

# Not suitable for perf benchmarking
cargo run -q -p covy-cli -- check tests/fixtures/lcov/basic.info --base HEAD --head HEAD --json > /dev/null
```

## Machine Mode Contract

- Use `--json` for machine-readable stdout payloads.
- Logs/warnings/errors are emitted to stderr.
- Exit codes:
  - `0` success
  - `1` quality/gate failure
  - `2` usage/runtime failure
- `covy ingest -q` and `covy merge -q` emit JSON summaries even without `--json`.
- Deprecation warnings for legacy aliases are suppressed in `--json` / `-q` machine mode.
  Set `COVY_DEPRECATION_WARNINGS=1` to re-enable deprecation warnings.

Output flags use `--output*` as canonical names. Legacy aliases such as `--out`, `--out-comment`,
`--out-sarif`, `--out-coverage`, and `--out-issues` are accepted with deprecation warnings.

## Configuration

Use `covy.toml` (see `covy.toml.example`).

If `covy ingest` is run without coverage path arguments, it will try `[ingest].report_paths` from the config automatically.

Issue gate config:

```toml
[gate.issues]
max_new_errors = 0
max_new_warnings = 5
# max_new_issues = 10
```

Impact + path config:

```toml
[impact]
testmap_path = ".covy/state/testmap.bin"
max_tests = 25
target_coverage = 0.90
stale_after_days = 14
allow_stale = true
test_id_strategy = "junit"

[paths]
strip_prefix = ["/home/runner/work/repo/repo", "/__w/repo/repo"]
replace_prefix = [{ from = "/workspace", to = "." }]
ignore_globs = ["**/target/**", "**/node_modules/**", "**/bazel-out/**"]
case_sensitive = true
```

## Coverage Report Filtering

Focus on only low-coverage files:

```bash
./target/release/covy report --below 80
```

Emit only total coverage for CI summaries:

```bash
./target/release/covy report --summary-only --json
```

## TIA Workflow

Build test impact map from per-test coverage:

```bash
./target/release/covy impact record \
  --base-ref main \
  --output .covy/state/testmap.bin \
  --per-test-lcov-dir artifacts/per-test-lcov \
  --summary-json .covy/state/testmap.json
```

Plan tests for a diff:

```bash
./target/release/covy impact plan \
  --base-ref origin/main \
  --head-ref HEAD \
  --testmap .covy/state/testmap.bin \
  --max-tests 25 \
  --target-coverage 0.9 \
  --format json > plan.json
```

Optional execution:

```bash
./target/release/covy impact run --plan plan.json -- pytest {tests}
```

Print input schema/examples:

```bash
./target/release/covy impact record --schema
./target/release/covy impact run --schema
```

`{tests}` is a placeholder in this README, not special `covy` syntax. Replace it with real pytest path/pattern arguments.

Example (single test file):

```bash
./target/release/covy impact run --plan plan.json -- pytest tests/test_example.py
```

Example (directory + `-k` expression; quote when your shell could split/expand unexpectedly):

```bash
./target/release/covy impact run --plan plan.json -- pytest tests/ -k "mytest"
```

## PR Artifacts

Generate PR comment markdown:

```bash
./target/release/covy comment --base-ref origin/main --head-ref HEAD --format markdown --output comment.md
```

Generate SARIF annotations:

```bash
./target/release/covy annotate --output covy.sarif --max-findings 200
```

One-shot artifact generation:

```bash
./target/release/covy pr --output-comment comment.md --output-sarif covy.sarif
```

Machine summary for one-shot artifacts:

```bash
./target/release/covy pr --output-comment comment.md --output-sarif covy.sarif --json
```

State paths for `comment`, `annotate`, and `pr` default to `.covy/state/latest.bin` and
`.covy/state/issues.bin`, and can be overridden with `--coverage-state-path` and
`--diagnostics-state-path`.

## Doctor + Path Mapping

Check repo/ref/config/report-path health:

```bash
./target/release/covy doctor --base-ref origin/main --head-ref HEAD
```

`[ingest].report_paths` patterns are resolved relative to the directory containing your `--config` file.

Learn/write mapping rules:

```bash
./target/release/covy map-paths --learn --write
```

Explain one path mapping decision:

```bash
./target/release/covy map-paths --explain /__w/repo/repo/src/main/java/com/foo/App.java
```

Machine-readable diagnostics:

```bash
./target/release/covy doctor --json
./target/release/covy map-paths --learn --json
```

## Project Initialization

Initialize `covy.toml` and `.covy/` in the current directory:

```bash
./target/release/covy init
```

Initialize at git repo root instead:

```bash
./target/release/covy init --repo-root
```

## CI Templates

Reference templates:

- GitHub Actions: `scripts/ci/github-actions.yml`
- GitLab CI: `scripts/ci/gitlab-ci.yml`

## Sharding Workflow

`covy` stays runner-agnostic: your CI executes tests, while `covy` plans shards and merges artifacts.

1. Generate `tasks.json` from your test adapter:

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "id": "com.foo.BarTest",
      "selector": "com.foo.BarTest",
      "est_ms": 1200,
      "tags": ["unit"],
      "module": "core"
    },
    {
      "id": "tests/test_mod.py::test_one",
      "selector": "tests/test_mod.py::test_one",
      "est_ms": 900,
      "tags": ["slow"]
    }
  ]
}
```

2. Plan shards (PR tier excludes `slow` by default):

```bash
./target/release/covy shard plan --shards 8 --tasks-json tasks.json --tier pr --write-files .covy/shards --json
```

Print shard plan input schemas:

```bash
./target/release/covy shard plan --schema
```

`testmap build` also supports JSON summary output:

```bash
./target/release/covy testmap build --manifest manifests/*.jsonl --output .covy/state/testmap.bin --json
```

3. Run shard files with your test runner and produce coverage/diagnostics artifacts.

4. Update timing history for future plans:

```bash
./target/release/covy shard update --junit-xml "artifacts/**/junit.xml" --timings-jsonl "artifacts/**/timings.jsonl" --json
```

5. Merge shard artifacts back into canonical state:

```bash
./target/release/covy merge --coverage "artifacts/**/coverage.bin" --issues "artifacts/**/issues.bin" --json
```

## Benchmarking

Use the standard benchmark harness in `benchmarks/`:

```bash
./benchmarks/run.sh
```

The harness rebuilds the binary by default to avoid stale-command mismatches.

It includes:

- built-binary micro checks (small fixture)
- scale tests for `100k` and `1M` LCOV lines
- scale tests for `50k` and `200k` SARIF issues

See `benchmarks/README.md` for details.

## Java + JaCoCo Example

Use `JavaTest/` for a full Java sample (Maven tests, JaCoCo XML, and SARIF artifacts).

```bash
cd JavaTest
mvn -q test jacoco:report
../target/release/covy ingest target/site/jacoco/jacoco.xml --format jacoco --issues annotations.sarif --json
```
