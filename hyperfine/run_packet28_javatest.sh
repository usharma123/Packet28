#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_JAVATEST_DIR="$ROOT_DIR/JavaTest"
BENCH_REPO_DIR="${PACKET28_JAVATEST_REPO_DIR:-$SOURCE_JAVATEST_DIR/.packet28-bench-repo}"
RESULTS_ROOT="${PACKET28_RESULTS_DIR:-$ROOT_DIR/hyperfine/results/packet28-javatest}"

WARMUP="${PACKET28_WARMUP:-1}"
RUNS="${PACKET28_RUNS:-5}"

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

require_file() {
  [[ -f "$1" ]] || fail "Missing required file: $1"
}

build_packet28() {
  if [[ -n "${PACKET28_BIN:-}" ]]; then
    [[ -x "$PACKET28_BIN" ]] || fail "PACKET28_BIN is not executable: $PACKET28_BIN"
    echo "$PACKET28_BIN"
    return
  fi

  log "Building Packet28 release binary"
  cargo build --release -p suite-cli >/dev/null

  local bin="$ROOT_DIR/target/release/Packet28"
  [[ -x "$bin" ]] || fail "Packet28 binary not found after build: $bin"
  echo "$bin"
}

write_bench_config() {
  cat >"$BENCH_REPO_DIR/covy.toml" <<'EOF'
[project]
name = "java-test-benchmark"
source_root = "."

[ingest]
report_paths = ["target/site/jacoco/jacoco.xml"]

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
max_new_issues = 999999

[impact]
testmap_path = ".covy/state/testmap.bin"
max_tests = 8
target_coverage = 0.90
stale_after_days = 14
allow_stale = true
test_id_strategy = "junit"

[shard]
timings_path = ".covy/state/testtimings.bin"
algorithm = "lpt"
unknown_test_seconds = 1.0
EOF

  cat >"$BENCH_REPO_DIR/context.yaml" <<'EOF'
version: 1
policy:
  tools:
    allowlist: ["covy", "diffy", "testy", "stacky", "buildy", "contextq", "mapy", "proxy"]
  reducers:
    allowlist:
      - "analyze"
      - "impact"
      - "slice"
      - "reduce"
      - "assemble"
      - "contextq.assemble"
      - "governed.assemble"
      - "diffy.analyze"
      - "testy.impact"
      - "stacky.slice"
      - "buildy.reduce"
      - "mapy.repo"
      - "proxy.run"
      - "guardy.check"
  paths:
    include: ["**"]
    exclude: []
  token_budget:
    cap: 50000
  runtime_budget:
    cap_ms: 30000
  tool_call_budget:
    cap: 20
  redaction:
    forbidden_patterns: []
  human_review:
    required: false
    on_policy_violation: true
    on_budget_violation: true
    on_redaction_violation: true
    paths: []
EOF

  cat >"$BENCH_REPO_DIR/.gitignore" <<'EOF'
target/
.covy/
.packet28/
bench/generated/
bench/captures/
bench/profile-captures/
EOF
}

copy_source_project() {
  [[ -d "$SOURCE_JAVATEST_DIR" ]] || fail "JavaTest source dir not found: $SOURCE_JAVATEST_DIR"

  rm -rf "$BENCH_REPO_DIR"
  mkdir -p "$BENCH_REPO_DIR"

  rsync -a \
    --exclude '.git' \
    --exclude '.packet28-bench-repo' \
    --exclude '.covy' \
    --exclude '.packet28' \
    --exclude 'target' \
    "$SOURCE_JAVATEST_DIR/" \
    "$BENCH_REPO_DIR/"

  mkdir -p \
    "$BENCH_REPO_DIR/bench/generated" \
    "$BENCH_REPO_DIR/bench/captures" \
    "$BENCH_REPO_DIR/bench/profile-captures" \
    "$BENCH_REPO_DIR/.covy/state"

  write_bench_config
}

init_git_repo() {
  (
    cd "$BENCH_REPO_DIR"
    git init -q
    git add .
    git -c user.name=Packet28 -c user.email=packet28@example.com commit -q -m "base"
  )
}

apply_head_changes() {
  python3 - "$BENCH_REPO_DIR" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])

calc = root / "src/main/java/com/example/Calculator.java"
text = calc.read_text()
old = "    public int add(int a, int b) {\n        return a + b;\n    }\n"
new = "    public int add(int a, int b) {\n        int sum = a + b;\n        return sum;\n    }\n"
if old not in text:
    raise SystemExit(f"expected Calculator.add block in {calc}")
calc.write_text(text.replace(old, new, 1))

string_utils = root / "src/main/java/com/example/StringUtils.java"
text = string_utils.read_text()
old = "    public boolean isPalindrome(String s) {\n        if (s == null) {\n            return false;\n        }\n        String reversed = reverse(s);\n        return s.equals(reversed);\n    }\n"
new = "    public boolean isPalindrome(String s) {\n        if (s == null) {\n            return false;\n        }\n        String reversed = reverse(s);\n        boolean matches = s.equals(reversed);\n        return matches;\n    }\n"
if old not in text:
    raise SystemExit(f"expected StringUtils.isPalindrome block in {string_utils}")
string_utils.write_text(text.replace(old, new, 1))
PY

  (
    cd "$BENCH_REPO_DIR"
    git add src/main/java/com/example/Calculator.java src/main/java/com/example/StringUtils.java
    git -c user.name=Packet28 -c user.email=packet28@example.com commit -q -m "head"
  )
}

refresh_javatest_artifacts() {
  log "Running Maven tests to refresh JaCoCo and Surefire artifacts"
  (
    cd "$BENCH_REPO_DIR"
    mvn -q test
  )
}

generate_support_files() {
  python3 - "$BENCH_REPO_DIR" <<'PY'
from pathlib import Path
import json
import xml.etree.ElementTree as ET
import sys

root = Path(sys.argv[1])
reports = sorted((root / "target/surefire-reports").glob("TEST-*.xml"))
if not reports:
    raise SystemExit("no surefire reports found under target/surefire-reports")

manifest = []
tasks = []
for report in reports:
    suite = ET.parse(report).getroot()
    class_name = suite.attrib.get("name")
    if not class_name:
      class_name = report.stem.removeprefix("TEST-")
    duration_ms = int(round(float(suite.attrib.get("time", "0")) * 1000))
    duration_ms = max(duration_ms, 1)
    manifest.append({
        "test_id": class_name,
        "language": "java",
        "duration_ms": duration_ms,
        "coverage_report": "target/site/jacoco/jacoco.xml",
    })
    tasks.append({
        "id": class_name,
        "selector": class_name,
        "est_ms": duration_ms,
        "tags": ["java", "unit"],
    })

(root / "bench/test-report.jsonl").write_text(
    "".join(json.dumps(record) + "\n" for record in manifest)
)
(root / "bench/tasks.json").write_text(
    json.dumps({"schema_version": 1, "tasks": tasks}, indent=2) + "\n"
)
(root / "bench/tests.txt").write_text("".join(f"{record['test_id']}\n" for record in manifest))

stack_log = """java.lang.ArithmeticException: Cannot divide by zero
  at com.example.Calculator.divide(src/main/java/com/example/Calculator.java:20)
  at com.example.CalculatorTest.testDivideByZero(src/test/java/com/example/CalculatorTest.java:31)

java.lang.ArithmeticException: Cannot divide by zero
  at com.example.Calculator.divide(src/main/java/com/example/Calculator.java:20)
  at com.example.CalculatorTest.testDivideByZero(src/test/java/com/example/CalculatorTest.java:31)

java.lang.IllegalStateException: invalid palindrome
  at com.example.StringUtils.isPalindrome(src/main/java/com/example/StringUtils.java:17)
  at com.example.StringUtilsTest.testIsPalindrome(src/test/java/com/example/StringUtilsTest.java:17)
"""
(root / "bench/java-stack.log").write_text(stack_log)

build_log = """src/main/java/com/example/StringUtils.java:24:5: error: cannot find symbol [JAVA0001]
src/main/java/com/example/StringUtils.java:24:5: error: cannot find symbol [JAVA0001]
src/main/java/com/example/Calculator.java:20:9: warning: unreachable statement [JAVA0002]
error[JAVA0100]: incompatible types
  --> src/test/java/com/example/CalculatorTest.java:12:13
"""
(root / "bench/java-build.log").write_text(build_log)
PY
}

run_in_repo() {
  local cmd="$1"
  local -a argv
  read -r -a argv <<<"$cmd"
  (
    cd "$BENCH_REPO_DIR"
    "${argv[@]}"
  )
}

prepare_store_and_handles() {
  local packet28_bin="$1"

  log "Priming Packet28 artifacts, cache entries, and packet handles"

  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/map_repo_full.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/map_repo_handle.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=handle"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/build_reduce_full.json build reduce --input bench/java-build.log --json=full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/stack_slice_full.json stack slice --input bench/java-stack.log --json=full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/proxy_run_full.json proxy run --json=full -- find src -type f"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/test_map_summary.json test map --manifest bench/test-report.jsonl --json"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/test_impact_full.json test impact --base HEAD~1 --head HEAD --testmap .covy/state/testmap.bin --json=full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/diff_analyze_full.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --json=full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/context_assemble_full.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --json=full"

  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/cache_map_repo.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --cache --json=compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/cache_diff_analyze.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --cache --json=compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/generated/cache_context_assemble.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --cache --json=compact"

  python3 - "$BENCH_REPO_DIR" "$packet28_bin" <<'PY'
from pathlib import Path
import json
import subprocess
import sys

root = Path(sys.argv[1])
packet28_bin = sys.argv[2]

handle_json = json.loads((root / "bench/generated/map_repo_handle.json").read_text())
handle = (
    handle_json.get("packet", {})
    .get("payload", {})
    .get("artifact_handle", {})
    .get("handle_id")
)
if not handle:
    raise SystemExit("map repo handle capture did not include packet.payload.artifact_handle.handle_id")
(root / "bench/generated/packet_handle.txt").write_text(handle + "\n")

list_json = json.loads(
    subprocess.check_output(
        [
            packet28_bin,
            "--config",
            str(root / "covy.toml"),
            "context",
            "store",
            "list",
            "--root",
            str(root),
            "--json",
        ],
        text=True,
        cwd=root,
    )
)
entries = list_json.get("entries", [])
if not entries:
    raise SystemExit("context store list returned no entries after cache priming")
(root / "bench/generated/store_key.txt").write_text(entries[0]["cache_key"] + "\n")
PY
}

capture_profile_variants() {
  local packet28_bin="$1"
  local compact="compact"
  local full="full"
  local handle="handle"

  log "Capturing compact/full/handle profile outputs for token comparison"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/diff_analyze.$compact.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/diff_analyze.$full.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/diff_analyze.$handle.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/test_impact.$compact.json test impact --base HEAD~1 --head HEAD --testmap .covy/state/testmap.bin --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/test_impact.$full.json test impact --base HEAD~1 --head HEAD --testmap .covy/state/testmap.bin --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/test_impact.$handle.json test impact --base HEAD~1 --head HEAD --testmap .covy/state/testmap.bin --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/context_assemble.$compact.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/context_assemble.$full.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/context_assemble.$handle.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/stack_slice.$compact.json stack slice --input bench/java-stack.log --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/stack_slice.$full.json stack slice --input bench/java-stack.log --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/stack_slice.$handle.json stack slice --input bench/java-stack.log --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/build_reduce.$compact.json build reduce --input bench/java-build.log --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/build_reduce.$full.json build reduce --input bench/java-build.log --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/build_reduce.$handle.json build reduce --input bench/java-build.log --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/map_repo.$compact.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=$compact"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/map_repo.$full.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=$full"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/map_repo.$handle.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=$handle"

  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/proxy_run.$compact.json proxy run --json=$compact -- find src -type f"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/proxy_run.$full.json proxy run --json=$full -- find src -type f"
  run_in_repo "$packet28_bin --config covy.toml --output bench/profile-captures/proxy_run.$handle.json proxy run --json=$handle -- find src -type f"
}

format_command_for_hyperfine() {
  printf '%s' "$1"
}

main() {
  require_cmd cargo
  require_cmd git
  require_cmd hyperfine
  require_cmd mvn
  require_cmd python3
  require_cmd rsync

  local packet28_bin
  packet28_bin="$(build_packet28)"

  copy_source_project
  init_git_repo
  apply_head_changes
  refresh_javatest_artifacts
  require_file "$BENCH_REPO_DIR/target/site/jacoco/jacoco.xml"
  generate_support_files
  prepare_store_and_handles "$packet28_bin"
  capture_profile_variants "$packet28_bin"

  mkdir -p "$RESULTS_ROOT"
  local run_id
  run_id="$(date +%Y%m%d-%H%M%S)"
  local run_dir="$RESULTS_ROOT/$run_id"
  mkdir -p "$run_dir"

  local meta_tsv="$run_dir/benchmark_captures.tsv"
  : >"$meta_tsv"

  local -a hyperfine_args
  hyperfine_args=(
    --shell=none
    --warmup "$WARMUP"
    --runs "$RUNS"
    --export-json "$run_dir/hyperfine.json"
    --export-markdown "$run_dir/hyperfine.md"
  )

  local handle_id
  handle_id="$(tr -d '\n' <"$BENCH_REPO_DIR/bench/generated/packet_handle.txt")"
  local store_key
  store_key="$(tr -d '\n' <"$BENCH_REPO_DIR/bench/generated/store_key.txt")"

  add_benchmark() {
    local label="$1"
    local capture_rel="$2"
    local cmd="$3"
    local capture_abs="$BENCH_REPO_DIR/$capture_rel"
    mkdir -p "$(dirname "$capture_abs")"
    printf '%s\t%s\n' "$label" "$capture_abs" >>"$meta_tsv"
    hyperfine_args+=(-n "$label" "$(format_command_for_hyperfine "$cmd")")
  }

  add_benchmark "cover check" "bench/captures/cover_check.json" \
    "$packet28_bin --config covy.toml --output bench/captures/cover_check.json cover check --coverage target/site/jacoco/jacoco.xml --format jacoco --base HEAD~1 --head HEAD --no-issues-state --json=compact"
  add_benchmark "diff analyze" "bench/captures/diff_analyze.json" \
    "$packet28_bin --config covy.toml --output bench/captures/diff_analyze.json diff analyze --coverage target/site/jacoco/jacoco.xml --no-issues-state --base HEAD~1 --head HEAD --json=compact"
  add_benchmark "test impact" "bench/captures/test_impact.json" \
    "$packet28_bin --config covy.toml --output bench/captures/test_impact.json test impact --base HEAD~1 --head HEAD --testmap .covy/state/testmap.bin --json=compact"
  add_benchmark "test shard" "bench/captures/test_shard.json" \
    "$packet28_bin --config covy.toml --output bench/captures/test_shard.json test shard --shards 2 --tasks-json bench/tasks.json --timings .covy/state/testtimings.bin --json"
  add_benchmark "test map" "bench/captures/test_map.json" \
    "$packet28_bin --config covy.toml --output bench/captures/test_map.json test map --manifest bench/test-report.jsonl --json"
  add_benchmark "guard validate" "bench/captures/guard_validate.json" \
    "$packet28_bin --config covy.toml --output bench/captures/guard_validate.json guard validate --context-config context.yaml"
  add_benchmark "guard check" "bench/captures/guard_check.json" \
    "$packet28_bin --config covy.toml --output bench/captures/guard_check.json guard check --packet bench/generated/map_repo_full.json --context-config context.yaml --json=compact"
  add_benchmark "context assemble" "bench/captures/context_assemble.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_assemble.json context assemble --packet bench/generated/map_repo_full.json --packet bench/generated/build_reduce_full.json --packet bench/generated/stack_slice_full.json --budget-tokens 30000 --budget-bytes 256000 --json=compact"
  add_benchmark "context store list" "bench/captures/context_store_list.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_store_list.json context store list --root . --json"
  add_benchmark "context store get" "bench/captures/context_store_get.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_store_get.json context store get --root . --key $store_key --json"
  add_benchmark "context store prune" "bench/captures/context_store_prune.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_store_prune.json context store prune --root . --ttl-secs 315360000 --json"
  add_benchmark "context store stats" "bench/captures/context_store_stats.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_store_stats.json context store stats --root . --json"
  add_benchmark "context recall" "bench/captures/context_recall.json" \
    "$packet28_bin --config covy.toml --output bench/captures/context_recall.json context recall --root . --query Calculator --limit 5 --json"
  add_benchmark "stack slice" "bench/captures/stack_slice.json" \
    "$packet28_bin --config covy.toml --output bench/captures/stack_slice.json stack slice --input bench/java-stack.log --json=compact"
  add_benchmark "build reduce" "bench/captures/build_reduce.json" \
    "$packet28_bin --config covy.toml --output bench/captures/build_reduce.json build reduce --input bench/java-build.log --json=compact"
  add_benchmark "map repo" "bench/captures/map_repo.json" \
    "$packet28_bin --config covy.toml --output bench/captures/map_repo.json map repo --repo-root . --focus-path src/main/java/com/example/Calculator.java --include-tests --max-files 20 --max-symbols 80 --json=compact"
  add_benchmark "proxy run" "bench/captures/proxy_run.json" \
    "$packet28_bin --config covy.toml --output bench/captures/proxy_run.json proxy run --json=compact -- find src -type f"
  add_benchmark "packet fetch" "bench/captures/packet_fetch.json" \
    "$packet28_bin --config covy.toml --output bench/captures/packet_fetch.json packet fetch --handle $handle_id --root . --json=full"

  log "Running Packet28 JavaTest benchmark suite"
  (
    cd "$BENCH_REPO_DIR"
    hyperfine "${hyperfine_args[@]}"
  )

  python3 - "$run_dir" "$meta_tsv" "$BENCH_REPO_DIR/bench/profile-captures" "$packet28_bin" "$BENCH_REPO_DIR" "$WARMUP" "$RUNS" <<'PY'
from pathlib import Path
import json
import sys

run_dir = Path(sys.argv[1])
meta_tsv = Path(sys.argv[2])
profile_dir = Path(sys.argv[3])
packet28_bin = sys.argv[4]
bench_repo_dir = sys.argv[5]
warmup = sys.argv[6]
runs = sys.argv[7]

results = json.loads((run_dir / "hyperfine.json").read_text())
captures = {}
for line in meta_tsv.read_text().splitlines():
    if not line.strip():
        continue
    label, path = line.split("\t", 1)
    captures[label] = Path(path)

def approx_tokens(raw: bytes) -> int:
    return len(raw) // 4

def load_json(path: Path):
    return json.loads(path.read_text())

def packet_metrics(data):
    packet = data.get("packet")
    if not isinstance(packet, dict):
        return None, None, None
    budget = packet.get("budget_cost")
    if not isinstance(budget, dict):
        return None, None, None
    return (
        budget.get("est_tokens"),
        budget.get("payload_est_tokens"),
        budget.get("runtime_ms"),
    )

def ms(value):
    if value is None:
        return None
    return round(value * 1000, 3)

def ratio(numerator, denominator):
    if numerator is None or denominator in (None, 0):
        return None
    return numerator / denominator

def percent_delta(current, baseline):
    if current is None or baseline in (None, 0):
        return None
    return ((current - baseline) / baseline) * 100.0

def bool_status(passed):
    return "PASS" if passed else "FAIL"

def format_ratio(value):
    if value is None:
        return ""
    return f"{value:.3f}x"

def format_percent(value):
    if value is None:
        return ""
    return f"{value:+.1f}%"

def format_ms_value(value):
    if value is None:
        return ""
    return f"{value:.3f}"

def render_table(headers, data_rows):
    widths = [len(h) for h in headers]
    for row in data_rows:
        for idx, cell in enumerate(row):
            widths[idx] = max(widths[idx], len(cell))
    sep = "| " + " | ".join("-" * w for w in widths) + " |"
    head = "| " + " | ".join(h.ljust(widths[i]) for i, h in enumerate(headers)) + " |"
    body = [
        "| " + " | ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)) + " |"
        for row in data_rows
    ]
    return "\n".join([head, sep, *body])

rows = []
for result in results["results"]:
    label = result["command"]
    capture_path = captures.get(label)
    output_bytes = None
    output_tokens = None
    packet_est_tokens = None
    payload_est_tokens = None
    packet_runtime_ms = None
    schema_version = None
    packet_type = None
    if capture_path and capture_path.exists():
      raw = capture_path.read_bytes()
      output_bytes = len(raw)
      output_tokens = approx_tokens(raw)
      try:
          data = json.loads(raw)
      except Exception:
          data = None
      if isinstance(data, dict):
          schema_version = data.get("schema_version")
          packet_type = data.get("packet_type")
          packet_est_tokens, payload_est_tokens, packet_runtime_ms = packet_metrics(data)
    rows.append(
        {
            "command": label,
            "mean_ms": ms(result.get("mean")),
            "min_ms": ms(result.get("min")),
            "max_ms": ms(result.get("max")),
            "stddev_ms": ms(result.get("stddev")),
            "output_bytes": output_bytes,
            "approx_output_tokens": output_tokens,
            "packet_est_tokens": packet_est_tokens,
            "payload_est_tokens": payload_est_tokens,
            "packet_runtime_ms": packet_runtime_ms,
            "schema_version": schema_version,
            "packet_type": packet_type,
        }
    )

profile_rows = []
profile_by_command = {}
for path in sorted(profile_dir.glob("*.json")):
    stem = path.stem
    if "." not in stem:
        continue
    name, profile = stem.rsplit(".", 1)
    raw = path.read_bytes()
    data = json.loads(raw)
    packet_est_tokens, payload_est_tokens, _ = packet_metrics(data)
    artifact_handle = (
        data.get("packet", {})
        .get("payload", {})
        .get("artifact_handle", {})
        .get("handle_id")
    )
    row = {
        "command": name.replace("_", " "),
        "command_id": name,
        "profile": profile,
        "output_bytes": len(raw),
        "approx_output_tokens": approx_tokens(raw),
        "packet_est_tokens": packet_est_tokens,
        "payload_est_tokens": payload_est_tokens,
        "artifact_handle": artifact_handle,
    }
    profile_rows.append(row)
    profile_by_command.setdefault(row["command"], {})[profile] = row

context_input_specs = [
    ("map repo", "map_repo", Path(bench_repo_dir) / "bench/generated/map_repo_full.json"),
    ("build reduce", "build_reduce", Path(bench_repo_dir) / "bench/generated/build_reduce_full.json"),
    ("stack slice", "stack_slice", Path(bench_repo_dir) / "bench/generated/stack_slice_full.json"),
]
context_inputs = []
for label, command_id, source_path in context_input_specs:
    source_data = load_json(source_path)
    _, payload_est_tokens, _ = packet_metrics(source_data)
    estimate_source = "payload_est_tokens"
    estimate_tokens = payload_est_tokens
    fallback_compact_tokens = None
    compact_profile = profile_by_command.get(label, {}).get("compact")
    if compact_profile is not None:
        fallback_compact_tokens = compact_profile["approx_output_tokens"]
    if estimate_tokens is None:
        estimate_tokens = fallback_compact_tokens
        estimate_source = "compact_wire_tokens_fallback"
    context_inputs.append(
        {
            "command": label,
            "command_id": command_id,
            "source_path": str(source_path),
            "payload_est_tokens": payload_est_tokens,
            "fallback_compact_tokens": fallback_compact_tokens,
            "estimate_tokens": estimate_tokens,
            "estimate_source": estimate_source,
        }
    )

context_input_total = sum(
    item["estimate_tokens"] for item in context_inputs if item["estimate_tokens"] is not None
)

profile_comparisons = []
shrinkage_checks = []
for command in sorted(profile_by_command):
    variants = profile_by_command[command]
    compact = variants.get("compact")
    full = variants.get("full")
    handle = variants.get("handle")
    compact_tokens = None if compact is None else compact["approx_output_tokens"]
    full_tokens = None if full is None else full["approx_output_tokens"]
    handle_tokens = None if handle is None else handle["approx_output_tokens"]
    compact_to_full = ratio(compact_tokens, full_tokens)
    compact_to_handle = ratio(compact_tokens, handle_tokens)
    handle_to_full = ratio(handle_tokens, full_tokens)
    tiny_payload_exception = full_tokens is not None and full_tokens < 300
    threshold_ratio = 1.0 if tiny_payload_exception else 0.8
    shrinkage_pass = (
        compact_to_handle is not None and compact_to_handle <= threshold_ratio
    )
    check = {
        "kind": "compact_vs_handle_shrinkage",
        "command": command,
        "status": bool_status(shrinkage_pass),
        "passed": shrinkage_pass,
        "compact_tokens": compact_tokens,
        "handle_tokens": handle_tokens,
        "full_tokens": full_tokens,
        "compact_to_handle_ratio": compact_to_handle,
        "compact_to_full_ratio": compact_to_full,
        "handle_to_full_ratio": handle_to_full,
        "threshold_ratio": threshold_ratio,
        "tiny_payload_exception": tiny_payload_exception,
        "details": (
            "full payload under 300 tokens; compact only needs to be <= handle"
            if tiny_payload_exception
            else "compact must be at least 20% smaller than handle"
        ),
    }
    shrinkage_checks.append(check)
    profile_comparisons.append(
        {
            "command": command,
            "compact_tokens": compact_tokens,
            "full_tokens": full_tokens,
            "handle_tokens": handle_tokens,
            "compact_bytes": None if compact is None else compact["output_bytes"],
            "full_bytes": None if full is None else full["output_bytes"],
            "handle_bytes": None if handle is None else handle["output_bytes"],
            "compact_to_full_ratio": compact_to_full,
            "compact_to_handle_ratio": compact_to_handle,
            "handle_to_full_ratio": handle_to_full,
            "shrinkage_check": check,
        }
    )

context_compact = profile_by_command.get("context assemble", {}).get("compact")
context_containment_pass = (
    context_compact is not None
    and context_compact["approx_output_tokens"] is not None
    and context_input_total is not None
    and context_compact["approx_output_tokens"] <= context_input_total
)
containment_check = {
    "kind": "context_assemble_containment",
    "command": "context assemble",
    "status": bool_status(context_containment_pass),
    "passed": context_containment_pass,
    "compact_tokens": None if context_compact is None else context_compact["approx_output_tokens"],
    "input_payload_estimate_total": context_input_total,
    "details": "compact wire tokens must not exceed combined input payload estimate",
    "inputs": context_inputs,
}

checks = [containment_check, *shrinkage_checks]
accepted = all(check["passed"] for check in checks)

def find_previous_accepted_baseline(root: Path, current: Path):
    for candidate in sorted(root.iterdir(), reverse=True):
        if not candidate.is_dir() or candidate == current:
            continue
        summary_path = candidate / "summary.json"
        if not summary_path.exists():
            continue
        try:
            summary = load_json(summary_path)
        except Exception:
            continue
        acceptance = summary.get("acceptance")
        if not isinstance(acceptance, dict):
            continue
        if acceptance.get("accepted") is True:
            return candidate, summary
    return None, None

baseline_dir, baseline_summary = find_previous_accepted_baseline(run_dir.parent, run_dir)
baseline_comparison = None
if baseline_summary is not None:
    baseline_rows = {
        row["command"]: row
        for row in baseline_summary.get("benchmarks", [])
        if isinstance(row, dict) and row.get("command")
    }
    delta_rows = []
    for row in rows:
        command = row["command"]
        baseline_row = baseline_rows.get(command)
        baseline_mean = None if baseline_row is None else baseline_row.get("mean_ms")
        delta_rows.append(
            {
                "command": command,
                "current_mean_ms": row["mean_ms"],
                "baseline_mean_ms": baseline_mean,
                "delta_ms": None if baseline_mean is None or row["mean_ms"] is None else round(row["mean_ms"] - baseline_mean, 3),
                "delta_percent": percent_delta(row["mean_ms"], baseline_mean),
            }
        )
    baseline_comparison = {
        "run_id": baseline_dir.name,
        "summary_path": str(baseline_dir / "summary.json"),
        "commands": delta_rows,
    }

summary = {
    "metadata": {
        "packet28_bin": packet28_bin,
        "benchmark_repo": bench_repo_dir,
        "warmup": int(warmup),
        "runs": int(runs),
        "run_id": run_dir.name,
        "results_dir": str(run_dir),
    },
    "acceptance": {
        "accepted": accepted,
        "status": bool_status(accepted),
        "baseline_run_id": None if baseline_comparison is None else baseline_comparison["run_id"],
    },
    "checks": checks,
    "context_assemble_inputs": {
        "total_estimate_tokens": context_input_total,
        "inputs": context_inputs,
    },
    "benchmarks": rows,
    "profiles": profile_rows,
    "profile_comparisons": profile_comparisons,
    "baseline_comparison": baseline_comparison,
}

(run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
(run_dir / "profile-summary.json").write_text(
    json.dumps(
        {
            "profiles": profile_rows,
            "comparisons": profile_comparisons,
            "checks": {
                "compact_vs_handle_shrinkage": shrinkage_checks,
            },
        },
        indent=2,
    )
    + "\n"
)

timing_table = render_table(
    [
        "Command",
        "Mean ms",
        "Stddev ms",
        "Output bytes",
        "Approx tokens",
        "Packet est",
        "Payload est",
    ],
    [
        [
            row["command"],
            "" if row["mean_ms"] is None else f'{row["mean_ms"]:.3f}',
            "" if row["stddev_ms"] is None else f'{row["stddev_ms"]:.3f}',
            "" if row["output_bytes"] is None else str(row["output_bytes"]),
            "" if row["approx_output_tokens"] is None else str(row["approx_output_tokens"]),
            "" if row["packet_est_tokens"] is None else str(row["packet_est_tokens"]),
            "" if row["payload_est_tokens"] is None else str(row["payload_est_tokens"]),
        ]
        for row in rows
    ],
)

ratio_table = render_table(
    [
        "Command",
        "Compact tok",
        "Full tok",
        "Handle tok",
        "Compact/Full",
        "Compact/Handle",
        "Handle/Full",
        "Shrinkage",
    ],
    [
        [
            row["command"],
            "" if row["compact_tokens"] is None else str(row["compact_tokens"]),
            "" if row["full_tokens"] is None else str(row["full_tokens"]),
            "" if row["handle_tokens"] is None else str(row["handle_tokens"]),
            format_ratio(row["compact_to_full_ratio"]),
            format_ratio(row["compact_to_handle_ratio"]),
            format_ratio(row["handle_to_full_ratio"]),
            row["shrinkage_check"]["status"],
        ]
        for row in profile_comparisons
    ],
)

checks_table = render_table(
    ["Check", "Status", "Actual", "Threshold", "Details"],
    [
        [
            "context assemble containment",
            containment_check["status"],
            ""
            if containment_check["compact_tokens"] is None
            else f'{containment_check["compact_tokens"]} tok',
            f'<= {containment_check["input_payload_estimate_total"]} tok',
            containment_check["details"],
        ],
        *[
            [
                f'{check["command"]} compact-vs-handle',
                check["status"],
                ""
                if check["compact_to_handle_ratio"] is None
                else f'{check["compact_to_handle_ratio"]:.3f}x',
                f'<= {check["threshold_ratio"]:.3f}x',
                check["details"],
            ]
            for check in shrinkage_checks
        ],
    ],
)

context_input_table = render_table(
    ["Input", "Estimate", "Source", "Payload est", "Compact fallback"],
    [
        [
            item["command"],
            "" if item["estimate_tokens"] is None else str(item["estimate_tokens"]),
            item["estimate_source"],
            "" if item["payload_est_tokens"] is None else str(item["payload_est_tokens"]),
            "" if item["fallback_compact_tokens"] is None else str(item["fallback_compact_tokens"]),
        ]
        for item in context_inputs
    ],
)

baseline_md = "No previous accepted baseline found."
if baseline_comparison is not None:
    baseline_md = (
        f'Baseline run: `{baseline_comparison["run_id"]}`\n\n'
        + render_table(
            ["Command", "Current ms", "Baseline ms", "Delta ms", "Delta %"],
            [
                [
                    row["command"],
                    format_ms_value(row["current_mean_ms"]),
                    format_ms_value(row["baseline_mean_ms"]),
                    "" if row["delta_ms"] is None else f'{row["delta_ms"]:+.3f}',
                    format_percent(row["delta_percent"]),
                ]
                for row in baseline_comparison["commands"]
            ],
        )
    )

profile_tables = []
for command in sorted(profile_by_command):
    variants = [
        profile_by_command[command][profile]
        for profile in ("compact", "full", "handle")
        if profile in profile_by_command[command]
    ]
    profile_tables.append(f"### {command}\n")
    profile_tables.append(
        render_table(
            [
                "Profile",
                "Output bytes",
                "Approx tokens",
                "Packet est",
                "Payload est",
                "Handle",
            ],
            [
                [
                    row["profile"],
                    str(row["output_bytes"]),
                    str(row["approx_output_tokens"]),
                    "" if row["packet_est_tokens"] is None else str(row["packet_est_tokens"]),
                    "" if row["payload_est_tokens"] is None else str(row["payload_est_tokens"]),
                    "yes" if row["artifact_handle"] else "",
                ]
                for row in variants
            ],
        )
    )
    profile_tables.append("")

summary_md = f"""# Packet28 JavaTest Benchmark

- Packet28 binary: `{packet28_bin}`
- Benchmark repo: `{bench_repo_dir}`
- Hyperfine warmup/runs: `{warmup}/{runs}`
- Acceptance status: `{bool_status(accepted)}`

## Acceptance Checks

{checks_table}

## Context Assemble Input Estimate

- Combined input payload estimate: `{context_input_total}` tokens

{context_input_table}

## Baseline Comparison

{baseline_md}

## Runtime And Token Summary

{timing_table}

## Profile Token Ratios

{ratio_table}

## Profile Token Comparisons

{chr(10).join(profile_tables).strip()}
"""

(run_dir / "summary.md").write_text(summary_md.rstrip() + "\n")
PY

  log "Benchmark run complete"
  echo "Benchmark repo: $BENCH_REPO_DIR"
  echo "Results directory: $run_dir"
  echo "Summary: $run_dir/summary.md"
}

main "$@"
