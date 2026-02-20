# covy

`covy` is a fast Rust CLI for coverage and diagnostics gating.

## Why covy

- Parse coverage from multiple formats (`lcov`, `cobertura`, `jacoco`, `gocov`, `llvm-cov`).
- Ingest diagnostics from SARIF.
- Gate PRs on both signals: coverage thresholds + no new issues (errors/warnings/combined) on changed lines.
- Render terminal, JSON, markdown, and GitHub annotations.

## Quick Start

```bash
# Build once
cargo build --release -p covy-cli

# Use the built binary for real runs
./target/release/covy ingest tests/fixtures/lcov/basic.info --issues tests/fixtures/sarif/basic.sarif
./target/release/covy check tests/fixtures/lcov/basic.info --issues tests/fixtures/sarif/basic.sarif --max-new-errors 0
./target/release/covy report --issues
```

## Performance Notes (Important)

- Don’t use `cargo run` for perf measurements; it adds startup overhead per invocation.
- In this repo, that overhead is roughly `~0.6s` per command.
- Use the built binary (`target/debug/covy` or `target/release/covy`) for timing and CI perf checks.
- For repeated checks, ingest diagnostics once and let `check/diff/github` reuse `.covy/state/issues.bin` by default.
- Passing `--issues <sarif>` to `check` forces SARIF parse again on each invocation (slower, expected).

Example:

```bash
# Good for perf
./target/release/covy check tests/fixtures/lcov/basic.info --base HEAD --head HEAD --report json > /dev/null

# Not suitable for perf benchmarking
cargo run -q -p covy-cli -- check tests/fixtures/lcov/basic.info --base HEAD --head HEAD --report json > /dev/null
```

## Configuration

Use `covy.toml` (see `covy.toml.example`).

Issue gate config:

```toml
[gate.issues]
max_new_errors = 0
max_new_warnings = 5
# max_new_issues = 10
```

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
