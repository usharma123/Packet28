#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <covy_bin> <project_dir>" >&2
  exit 1
fi

COVY_BIN="$1"
PROJECT_DIR="$2"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

assert_file() {
  local path="$1"
  [[ -f "$path" ]] || fail "expected file not found: $path"
}

assert_contains() {
  local needle="$1"
  local file="$2"
  grep -q "$needle" "$file" || fail "expected '$needle' in $file"
}

[[ -x "$COVY_BIN" ]] || fail "covy binary is not executable: $COVY_BIN"
[[ -d "$PROJECT_DIR" ]] || fail "project dir does not exist: $PROJECT_DIR"

(
  cd "$PROJECT_DIR"

  "$COVY_BIN" ingest fixtures/lcov/basic.info --format lcov --output .covy/state/latest.bin --color never >/dev/null
  assert_file ".covy/state/latest.bin"

  "$COVY_BIN" report --input .covy/state/latest.bin --json --color never >artifacts/smoke-report.json
  assert_contains "total_coverage_pct" "artifacts/smoke-report.json"

  "$COVY_BIN" ingest --issues fixtures/sarif/basic.sarif --color never >/dev/null
  assert_file ".covy/state/issues.bin"

  "$COVY_BIN" check fixtures/lcov/basic.info --no-issues-state --base HEAD~1 --head HEAD --report json --color never >artifacts/smoke-check-pass.json
  assert_contains "\"passed\"" "artifacts/smoke-check-pass.json"

  set +e
  "$COVY_BIN" check fixtures/lcov/basic.info --no-issues-state --base HEAD~1 --head HEAD --fail-under-total 101 --report json --color never >/dev/null 2>artifacts/smoke-check-fail.stderr
  rc=$?
  set -e
  if [[ "$rc" -ne 1 ]]; then
    fail "expected gate failure exit code 1, got $rc"
  fi

  "$COVY_BIN" pr --base-ref HEAD~1 --head-ref HEAD \
    --output-comment artifacts/smoke-comment.md \
    --output-sarif artifacts/smoke.sarif \
    --coverage-state-path .covy/state/latest.bin \
    --diagnostics-state-path .covy/state/issues.bin \
    --json --color never >artifacts/smoke-pr.json

  assert_file "artifacts/smoke-comment.md"
  assert_file "artifacts/smoke.sarif"
  assert_contains "\"comment\"" "artifacts/smoke-pr.json"
  assert_contains "\"sarif\"" "artifacts/smoke-pr.json"
)

echo "Smoke checks passed for $COVY_BIN"

