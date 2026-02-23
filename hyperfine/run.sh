#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${HYPERFINE_PROFILE:-full}"
PROJECT_DIR="${HYPERFINE_PROJECT_DIR:-$ROOT_DIR/hyperfine/project}"
RESULTS_ROOT="${HYPERFINE_RESULTS_DIR:-$ROOT_DIR/hyperfine/results}"

WARMUP_SMALL="${HYPERFINE_WARMUP_SMALL:-3}"
WARMUP_LARGE="${HYPERFINE_WARMUP_LARGE:-2}"
RUNS_SMALL="${HYPERFINE_RUNS_SMALL:-20}"
RUNS_MEDIUM="${HYPERFINE_RUNS_MEDIUM:-12}"
RUNS_LARGE="${HYPERFINE_RUNS_LARGE:-6}"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

log() {
  printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*" >&2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

run_group() {
  local name="$1"
  local warmup="$2"
  local runs="$3"
  shift 3

  log "Running group: $name"
  (
    cd "$PROJECT_DIR"
    hyperfine \
      --shell=none \
      --warmup "$warmup" \
      --runs "$runs" \
      --export-json "$RESULTS_DIR/$name.json" \
      --export-markdown "$RESULTS_DIR/$name.md" \
      "$@"
  )
}

run_group_ignore_failure() {
  local name="$1"
  local warmup="$2"
  local runs="$3"
  shift 3

  log "Running group (ignore failures): $name"
  (
    cd "$PROJECT_DIR"
    hyperfine \
      --shell=none \
      --ignore-failure \
      --warmup "$warmup" \
      --runs "$runs" \
      --export-json "$RESULTS_DIR/$name.json" \
      --export-markdown "$RESULTS_DIR/$name.md" \
      "$@"
  )
}

prepare_merge_shards() {
  local count="${1:-8}"
  (
    cd "$PROJECT_DIR"
    rm -rf data/shards
    mkdir -p data/shards
    for i in $(seq 1 "$count"); do
      "$COVY_BIN" ingest fixtures/lcov/basic.info --format lcov --output "data/shards/coverage-$i.bin" -q --color never >/dev/null
      "$COVY_BIN" ingest --issues fixtures/sarif/basic.sarif -q --color never >/dev/null
      cp .covy/state/issues.bin "data/shards/issues-$i.bin"
    done
  )
}

require_cmd git
require_cmd hyperfine

if [[ -n "${COVY_BIN:-}" ]]; then
  [[ -x "$COVY_BIN" ]] || fail "COVY_BIN is not executable: $COVY_BIN"
else
  COVY_BIN="$("$ROOT_DIR/hyperfine/install_public_covy.sh")"
fi

log "Using covy binary: $COVY_BIN"

"$ROOT_DIR/hyperfine/setup_project.sh"
"$ROOT_DIR/hyperfine/quality_smoke.sh" "$COVY_BIN" "$PROJECT_DIR"

mkdir -p "$RESULTS_ROOT"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
RESULTS_DIR="$RESULTS_ROOT/$RUN_ID"
mkdir -p "$RESULTS_DIR"

run_group "01_startup" "$WARMUP_SMALL" "$RUNS_SMALL" \
  -n "version" "$COVY_BIN --version" \
  -n "help" "$COVY_BIN --help"

run_group "02_ingest_formats_small" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
  -n "lcov" "$COVY_BIN ingest fixtures/lcov/basic.info --format lcov --output .covy/state/ingest-lcov.bin -q --color never" \
  -n "cobertura" "$COVY_BIN ingest fixtures/cobertura/basic.xml --format cobertura --output .covy/state/ingest-cobertura.bin -q --color never" \
  -n "jacoco" "$COVY_BIN ingest fixtures/jacoco/basic.xml --format jacoco --output .covy/state/ingest-jacoco.bin -q --color never" \
  -n "gocov" "$COVY_BIN ingest fixtures/gocov/basic.out --format gocov --output .covy/state/ingest-gocov.bin -q --color never" \
  -n "llvm-cov" "$COVY_BIN ingest fixtures/llvmcov/basic.json --format llvm-cov --output .covy/state/ingest-llvm.bin -q --color never"

run_group "03_ingest_scale" "$WARMUP_LARGE" "$RUNS_LARGE" \
  -n "lcov 100k" "$COVY_BIN ingest fixtures/generated/lcov-100k.info --format lcov --output .covy/state/lcov-100k.bin -q --color never" \
  -n "lcov 1m" "$COVY_BIN ingest fixtures/generated/lcov-1m.info --format lcov --output .covy/state/lcov-1m.bin -q --color never" \
  -n "sarif 50k" "$COVY_BIN ingest --issues fixtures/generated/sarif-50k.sarif -q --color never" \
  -n "sarif 200k" "$COVY_BIN ingest --issues fixtures/generated/sarif-200k.sarif -q --color never"

(
  cd "$PROJECT_DIR"
  "$COVY_BIN" ingest fixtures/generated/lcov-100k.info --format lcov --output .covy/state/latest.bin -q --color never >/dev/null
)

run_group "04_report_paths" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
  -n "report terminal" "$COVY_BIN report --input .covy/state/latest.bin -q --color never" \
  -n "report json" "$COVY_BIN report --input .covy/state/latest.bin --json -q --color never" \
  -n "report below 80 json" "$COVY_BIN report --input .covy/state/latest.bin --below 80 --json -q --color never" \
  -n "report summary-only json" "$COVY_BIN report --input .covy/state/latest.bin --summary-only --json -q --color never"

(
  cd "$PROJECT_DIR"
  "$COVY_BIN" ingest fixtures/lcov/basic.info --format lcov --output .covy/state/latest.bin -q --color never >/dev/null
  "$COVY_BIN" ingest --issues fixtures/generated/sarif-50k.sarif -q --color never >/dev/null
)

run_group "05_check_paths" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
  -n "coverage only" "$COVY_BIN check fixtures/lcov/basic.info --no-issues-state --base HEAD~1 --head HEAD --report json -q --color never" \
  -n "cached issues state" "$COVY_BIN check fixtures/lcov/basic.info --issues-state .covy/state/issues.bin --max-new-errors 999999 --base HEAD~1 --head HEAD --report json -q --color never" \
  -n "parse issues each run" "$COVY_BIN check fixtures/lcov/basic.info --issues fixtures/sarif/basic.sarif --max-new-errors 999999 --base HEAD~1 --head HEAD --report json -q --color never"

run_group_ignore_failure "06_check_fail_path" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
  -n "fail-under-total 101" "$COVY_BIN check fixtures/lcov/basic.info --no-issues-state --base HEAD~1 --head HEAD --fail-under-total 101 --report json -q --color never"

(
  cd "$PROJECT_DIR"
  "$COVY_BIN" ingest fixtures/generated/lcov-100k.info --format lcov --output .covy/state/latest.bin -q --color never >/dev/null
  "$COVY_BIN" ingest --issues fixtures/generated/sarif-50k.sarif -q --color never >/dev/null
)

run_group "07_pr_artifacts" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
  -n "diff json" "$COVY_BIN diff --base HEAD~1 --head HEAD --report json --input .covy/state/latest.bin --issues-state .covy/state/issues.bin -q --color never" \
  -n "comment markdown" "$COVY_BIN comment --base-ref HEAD~1 --head-ref HEAD --output artifacts/comment.md --coverage-state-path .covy/state/latest.bin --diagnostics-state-path .covy/state/issues.bin -q --color never" \
  -n "annotate sarif" "$COVY_BIN annotate --base-ref HEAD~1 --head-ref HEAD --output artifacts/covy.sarif --coverage-state-path .covy/state/latest.bin --diagnostics-state-path .covy/state/issues.bin -q --color never" \
  -n "pr one-shot" "$COVY_BIN pr --base-ref HEAD~1 --head-ref HEAD --output-comment artifacts/pr-comment.md --output-sarif artifacts/pr.sarif --coverage-state-path .covy/state/latest.bin --diagnostics-state-path .covy/state/issues.bin --json -q --color never"

if [[ "$PROFILE" == "full" ]]; then
  run_group "08_doctor_map_paths" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
    -n "doctor json" "$COVY_BIN doctor --base-ref HEAD~1 --head-ref HEAD --json -q --color never" \
    -n "map-paths learn" "$COVY_BIN map-paths --learn --paths fixtures/lcov/basic.info --json -q --color never" \
    -n "map-paths explain" "$COVY_BIN map-paths --explain src/main.rs --json -q --color never"

  (
    cd "$PROJECT_DIR"
    "$COVY_BIN" impact record --base-ref HEAD~1 --per-test-lcov-dir data/per-test-lcov --output .covy/state/testmap.bin --summary-json artifacts/impact-record-summary.json -q --color never >/dev/null
  )

  run_group "09_testmap_impact" "$WARMUP_LARGE" "$RUNS_LARGE" \
    -n "testmap build" "$COVY_BIN testmap build --manifest data/test-report.jsonl --output .covy/state/testmap.bin --timings-output .covy/state/testtimings.bin --json -q --color never" \
    -n "impact record" "$COVY_BIN impact record --base-ref HEAD~1 --per-test-lcov-dir data/per-test-lcov --output .covy/state/testmap.bin --summary-json artifacts/impact-record-summary.json -q --color never" \
    -n "impact plan" "$COVY_BIN impact plan --base-ref HEAD~1 --head-ref HEAD --testmap .covy/state/testmap.bin --max-tests 50 --target-coverage 0.90 --format json -q --color never"

  prepare_merge_shards 8

  MERGE_CMD="$COVY_BIN merge"
  for i in $(seq 1 8); do
    MERGE_CMD="$MERGE_CMD --coverage data/shards/coverage-$i.bin"
  done
  for i in $(seq 1 8); do
    MERGE_CMD="$MERGE_CMD --issues data/shards/issues-$i.bin"
  done
  MERGE_CMD="$MERGE_CMD --output-coverage artifacts/merged-coverage.bin --output-issues artifacts/merged-issues.bin --json -q --color never"

  run_group "10_shard_merge" "$WARMUP_SMALL" "$RUNS_MEDIUM" \
    -n "shard plan" "$COVY_BIN shard plan --shards 8 --tasks-json data/tasks.json --json --write-files artifacts/shards -q --color never" \
    -n "merge 8+8 shards" "$MERGE_CMD"
fi

log "Benchmark run complete."
echo "Results directory: $RESULTS_DIR"
find "$RESULTS_DIR" -maxdepth 1 -name '*.md' | sort
