#!/usr/bin/env bash
set -euo pipefail

# Fixed eval harness for Packet28 autoresearch.
# DO NOT MODIFY during experiments — this is the ground truth metric.
# Like prepare.py in Karpathy's autoresearch.

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

# Create logs dir
mkdir -p autoresearch/logs

# --- Config ---
RESULTS_FILE="autoresearch/results.tsv"
RUN_ID="$(git rev-parse --short=7 HEAD)-$(date +%s)"
TARGET_CRATES=("mapy-core" "contextq-core" "packet28-reducer-core")

# --- Step 1: Run tests (gate) ---
echo "=== Running workspace tests ==="
TESTS_PASS="true"
TEST_START_NS=$(date +%s%N)

if cargo test --workspace 2>&1; then
  echo "All tests passed."
else
  echo "TESTS FAILED"
  TESTS_PASS="false"
fi

TEST_END_NS=$(date +%s%N)
TEST_TIME_MS=$(( (TEST_END_NS - TEST_START_NS) / 1000000 ))

# --- Step 2: Run target crate tests specifically ---
echo ""
echo "=== Target crate tests ==="
for crate in "${TARGET_CRATES[@]}"; do
  if cargo test -p "$crate" 2>&1; then
    echo "  $crate: PASS"
  else
    echo "  $crate: FAIL"
    TESTS_PASS="false"
  fi
done

# --- Step 3: Lightweight benchmark (if hyperfine available) ---
AVG_LATENCY_MS="0"
if command -v hyperfine >/dev/null 2>&1; then
  echo ""
  echo "=== Benchmark: cargo test -p mapy-core ==="
  BENCH_OUT=$(hyperfine --shell=none --warmup 1 --runs 3 \
    "cargo test -p mapy-core --lib" \
    --export-json /dev/stdout 2>/dev/null || true)
  if [ -n "$BENCH_OUT" ]; then
    AVG_LATENCY_MS=$(echo "$BENCH_OUT" | grep -o '"mean":[0-9.]*' | head -1 | cut -d: -f2 | awk '{printf "%.0f", $1 * 1000}')
  fi
else
  echo ""
  echo "hyperfine not found — skipping benchmark timing."
  echo "Install: brew install hyperfine"
  AVG_LATENCY_MS="$TEST_TIME_MS"
fi

# --- Step 4: Compute score ---
# Score formula: task_success_rate * 100 - 0.001 * avg_tokens - 0.01 * avg_latency_ms
# task_success_rate and avg_tokens are placeholders until wired to real eval
AVG_TOKENS="0"
TASK_SUCCESS_RATE="0"
if [ "$TESTS_PASS" = "true" ]; then
  # Placeholder: score = 100 - 0.01 * latency_ms (full score pending real eval cases)
  SCORE=$(awk "BEGIN {printf \"%.2f\", 100 - 0.01 * $AVG_LATENCY_MS}")
else
  SCORE="0"
fi

# --- Step 5: Append results ---
echo ""
echo "=== Results ==="
echo "run_id:        $RUN_ID"
echo "tests_pass:    $TESTS_PASS"
echo "test_time_ms:  $TEST_TIME_MS"
echo "avg_latency:   ${AVG_LATENCY_MS}ms"
echo "score:         $SCORE"

# Initialize header if file doesn't exist or is empty
if [ ! -s "$RESULTS_FILE" ]; then
  printf 'run_id\ttarget\ttests_pass\ttask_success_rate\tavg_tokens\tavg_latency_ms\tscore\tstatus\tnotes\n' > "$RESULTS_FILE"
fi

# Append row (status and notes filled by the agent after review)
printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
  "$RUN_ID" "reducer-ranking" "$TESTS_PASS" "$TASK_SUCCESS_RATE" \
  "$AVG_TOKENS" "$AVG_LATENCY_MS" "$SCORE" "pending" "" \
  >> "$RESULTS_FILE"

echo ""
echo "Row appended to $RESULTS_FILE"
echo "Log: autoresearch/logs/${RUN_ID}.log"

# --- Dogfooding ---
echo ""
echo "=== Verify local Packet28 setup ==="
echo "  packet28 setup --root ."
echo "  packet28 doctor --root ."
