#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

BIN="${BENCH_BIN:-target/release/covy}"

if [[ -z "${BENCH_SKIP_GENERATE:-}" ]]; then
  "$ROOT_DIR/benchmarks/generate_fixtures.sh"
fi

if [[ -z "${BENCH_SKIP_BUILD:-}" ]]; then
  if [[ "$BIN" == "target/release/covy" ]]; then
    echo "Building release binary..."
    cargo build --release -p covy-cli
  elif [[ "$BIN" == "target/debug/covy" ]]; then
    echo "Building debug binary..."
    cargo build -p covy-cli
  fi
fi

if [[ ! -x "$BIN" ]]; then
  echo "Binary not found: $BIN" >&2
  exit 1
fi

if ! "$BIN" ingest --help | grep -q -- "--issues"; then
  echo "Binary at $BIN does not support '--issues' for 'ingest'." >&2
  echo "Rebuild your binary (or unset BENCH_SKIP_BUILD) and try again." >&2
  exit 1
fi

# Ensure baseline state exists for report benchmarks.
"$BIN" ingest tests/fixtures/lcov/basic.info --color never >/dev/null

if command -v hyperfine >/dev/null 2>&1; then
  echo "Running hyperfine benchmarks with $BIN"
  HF_COMMON=(--shell=none)

  hyperfine "${HF_COMMON[@]}" --warmup 3 --runs 20 \
    "$BIN report --input .covy/state/latest.bin --color never" \
    --command-name "report small"

  hyperfine "${HF_COMMON[@]}" --warmup 3 --runs 20 \
    "$BIN ingest tests/fixtures/lcov/basic.info --color never" \
    --command-name "ingest small"

  hyperfine "${HF_COMMON[@]}" --warmup 3 --runs 20 \
    "$BIN check tests/fixtures/lcov/basic.info --no-issues-state --base HEAD --head HEAD --report json --color never" \
    --command-name "check small"

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 10 \
    "$BIN ingest benchmarks/generated/lcov-100k.info --color never" \
    --command-name "ingest lcov 100k"

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 10 \
    "$BIN ingest benchmarks/generated/lcov-1m.info --color never" \
    --command-name "ingest lcov 1m"

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 10 \
    "$BIN ingest --issues benchmarks/generated/sarif-50k.sarif --color never" \
    --command-name "ingest sarif 50k"

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 10 \
    "$BIN ingest --issues benchmarks/generated/sarif-200k.sarif --color never" \
    --command-name "ingest sarif 200k"

  # Prime diagnostics state to benchmark the cached fast path.
  "$BIN" ingest --issues benchmarks/generated/sarif-50k.sarif --color never >/dev/null

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 8 \
    "$BIN check benchmarks/generated/lcov-100k.info --base HEAD --head HEAD --report json --color never --max-new-errors 999999" \
    --command-name "check combined 100k+cached-state"

  # Prime large diagnostics state, then benchmark cached large-state checks.
  "$BIN" ingest --issues benchmarks/generated/sarif-200k.sarif --color never >/dev/null

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 8 \
    "$BIN check benchmarks/generated/lcov-100k.info --base HEAD --head HEAD --report json --color never --max-new-errors 999999" \
    --command-name "check combined 100k+200k(cached-state)"

  hyperfine "${HF_COMMON[@]}" --warmup 2 --runs 8 \
    "$BIN check benchmarks/generated/lcov-100k.info --issues benchmarks/generated/sarif-50k.sarif --base HEAD --head HEAD --report json --color never --max-new-errors 999999" \
    --command-name "check combined 100k+50k(parse)"
else
  echo "hyperfine not found. Running fallback timer loop."

  run_case() {
    local name="$1"
    local iterations="$2"
    shift 2

    local total_ns=0

    echo "== $name =="
    "$@" >/dev/null
    for ((i=1; i<=iterations; i++)); do
      local start_ns
      local end_ns
      start_ns=$(date +%s%N)
      "$@" >/dev/null
      end_ns=$(date +%s%N)
      local elapsed_ns=$((end_ns - start_ns))
      total_ns=$((total_ns + elapsed_ns))
      awk -v n="$elapsed_ns" 'BEGIN{printf "run: %.3f ms\n", n/1000000}'
    done

    awk -v n="$total_ns" -v it="$iterations" 'BEGIN{printf "avg: %.3f ms\n\n", (n/it)/1000000}'
  }

  run_case "report small" 10 "$BIN" report --input .covy/state/latest.bin --color never
  run_case "ingest small" 10 "$BIN" ingest tests/fixtures/lcov/basic.info --color never
  run_case "check small" 10 "$BIN" check tests/fixtures/lcov/basic.info --no-issues-state --base HEAD --head HEAD --report json --color never
  run_case "ingest lcov 100k" 10 "$BIN" ingest benchmarks/generated/lcov-100k.info --color never
  run_case "ingest lcov 1m" 5 "$BIN" ingest benchmarks/generated/lcov-1m.info --color never
  run_case "ingest sarif 50k" 5 "$BIN" ingest --issues benchmarks/generated/sarif-50k.sarif --color never
  run_case "ingest sarif 200k" 3 "$BIN" ingest --issues benchmarks/generated/sarif-200k.sarif --color never

  # Prime diagnostics state to benchmark the cached fast path.
  "$BIN" ingest --issues benchmarks/generated/sarif-50k.sarif --color never >/dev/null

  run_case "check combined 100k+cached-state" 5 "$BIN" check benchmarks/generated/lcov-100k.info --base HEAD --head HEAD --report json --color never --max-new-errors 999999
  "$BIN" ingest --issues benchmarks/generated/sarif-200k.sarif --color never >/dev/null
  run_case "check combined 100k+200k(cached-state)" 5 "$BIN" check benchmarks/generated/lcov-100k.info --base HEAD --head HEAD --report json --color never --max-new-errors 999999
  run_case "check combined 100k+50k(parse)" 5 "$BIN" check benchmarks/generated/lcov-100k.info --issues benchmarks/generated/sarif-50k.sarif --base HEAD --head HEAD --report json --color never --max-new-errors 999999
fi
