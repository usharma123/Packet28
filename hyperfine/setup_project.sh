#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${HYPERFINE_PROJECT_DIR:-$ROOT_DIR/hyperfine/project}"
GENERATED_DIR="$ROOT_DIR/hyperfine/generated"
PER_TEST_COUNT="${HYPERFINE_PER_TEST_COUNT:-200}"

rm -rf "$PROJECT_DIR"
mkdir -p \
  "$PROJECT_DIR/src" \
  "$PROJECT_DIR/fixtures/generated" \
  "$PROJECT_DIR/data/per-test-lcov" \
  "$PROJECT_DIR/artifacts" \
  "$PROJECT_DIR/.covy/state"

"$ROOT_DIR/hyperfine/generate_fixtures.sh"

cp -R "$ROOT_DIR/tests/fixtures/." "$PROJECT_DIR/fixtures/"
cp "$GENERATED_DIR/lcov-100k.info" "$PROJECT_DIR/fixtures/generated/lcov-100k.info"
cp "$GENERATED_DIR/lcov-1m.info" "$PROJECT_DIR/fixtures/generated/lcov-1m.info"
cp "$GENERATED_DIR/sarif-50k.sarif" "$PROJECT_DIR/fixtures/generated/sarif-50k.sarif"
cp "$GENERATED_DIR/sarif-200k.sarif" "$PROJECT_DIR/fixtures/generated/sarif-200k.sarif"

cat >"$PROJECT_DIR/src/main.rs" <<'EOF'
fn main() {
    let x = 1;
    let y = 2;
    println!("{}", x + y);
}
EOF

cat >"$PROJECT_DIR/src/lib.rs" <<'EOF'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
EOF

cat >"$PROJECT_DIR/README.md" <<'EOF'
# Covy Hyperfine Benchmark Project
EOF

cat >"$PROJECT_DIR/covy.toml" <<'EOF'
[project]
name = "covy-hyperfine"
source_root = "."

[ingest]
report_paths = ["fixtures/lcov/basic.info"]

[diff]
base = "HEAD~1"
head = "HEAD"

[gate]
fail_under_total = 0.0
fail_under_changed = 0.0
fail_under_new = 0.0

[gate.issues]
max_new_errors = 999999
max_new_warnings = 999999

[impact]
testmap_path = ".covy/state/testmap.bin"
max_tests = 50
target_coverage = 0.90
stale_after_days = 14
allow_stale = true
test_id_strategy = "junit"

[shard]
timings_path = ".covy/state/testtimings.bin"
algorithm = "lpt"
unknown_test_seconds = 2.0

[merge]
strict = true
output_coverage = ".covy/state/latest.bin"
output_issues = ".covy/state/issues.bin"
EOF

MANIFEST_PATH="$PROJECT_DIR/data/test-report.jsonl"
TASKS_PATH="$PROJECT_DIR/data/tasks.json"
TESTS_PATH="$PROJECT_DIR/data/tests.txt"

: >"$MANIFEST_PATH"
: >"$TESTS_PATH"

for i in $(seq 1 "$PER_TEST_COUNT"); do
  id=$(printf "%04d" "$i")
  report="$PROJECT_DIR/data/per-test-lcov/test-$id.info"
  cp "$PROJECT_DIR/fixtures/lcov/basic.info" "$report"
  printf '{"test_id":"suite::test_%s","language":"python","coverage_report":"%s"}\n' "$id" "$report" >>"$MANIFEST_PATH"
  printf 'suite::test_%s\n' "$id" >>"$TESTS_PATH"
done

{
  echo '{'
  echo '  "schema_version": 1,'
  echo '  "tasks": ['
  for i in $(seq 1 "$PER_TEST_COUNT"); do
    id=$(printf "%04d" "$i")
    est_ms=$((200 + (i % 9) * 75))
    comma=","
    if [[ "$i" -eq "$PER_TEST_COUNT" ]]; then
      comma=""
    fi
    printf '    {"id":"suite::test_%s","selector":"suite::test_%s","est_ms":%d,"tags":["unit"]}%s\n' "$id" "$id" "$est_ms" "$comma"
  done
  echo '  ]'
  echo '}'
} >"$TASKS_PATH"

(
  cd "$PROJECT_DIR"
  git init -q
  git add .
  git -c user.name=Hyperfine -c user.email=hyperfine@example.com commit -q -m "base"

  cat >src/main.rs <<'EOF'
fn main() {
    let x = 1;
    let y = 3;
    println!("{}", x + y);
}
EOF

  cat >src/lib.rs <<'EOF'
pub fn add(a: i32, b: i32) -> i32 {
    a + b + 1
}
EOF

  git add src/main.rs src/lib.rs
  git -c user.name=Hyperfine -c user.email=hyperfine@example.com commit -q -m "head"
)

echo "Prepared benchmark project at $PROJECT_DIR"
