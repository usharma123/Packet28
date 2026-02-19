# Benchmark Suite

This folder provides a repeatable performance harness for `covy`.

## Rules

- Benchmark the built binary, not `cargo run`.
- Prefer release mode for stable comparisons.
- Keep command output redirected to `/dev/null` so rendering cost does not dominate parser/gate timing.

## Quick Run

```bash
./benchmarks/run.sh
```

What it does:

- Builds `target/release/covy`
- Generates synthetic large fixtures in `benchmarks/generated/`
- Runs benchmark cases using `hyperfine` when available
- Falls back to a builtin timer loop when `hyperfine` is not installed
- Uses `--no-issues-state` for the `check small` case so it stays coverage-only
- Benchmarks both diagnostics paths for combined checks:
  - cached state (`issues.bin`) fast path
  - explicit SARIF parse path (`--issues ...`)

## Generated Fixtures

- `benchmarks/generated/lcov-100k.info`
- `benchmarks/generated/lcov-1m.info`
- `benchmarks/generated/sarif-50k.sarif`
- `benchmarks/generated/sarif-200k.sarif`

## Optional knobs

```bash
# keep generated files, skip regeneration
BENCH_SKIP_GENERATE=1 ./benchmarks/run.sh

# use debug binary instead of release
BENCH_BIN=target/debug/covy ./benchmarks/run.sh

# skip build step (only if you know binary is fresh)
BENCH_SKIP_BUILD=1 ./benchmarks/run.sh
```
