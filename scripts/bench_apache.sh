#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COVY_BIN_DEFAULT="$ROOT_DIR/target/release/covy"

WORK_DIR="${TMPDIR:-/tmp}/covy-bench-apache-$(date +%Y%m%d-%H%M%S)"
REPO_DIR=""
M2_REPO="/tmp/m2"
SHARDS=8
MODES="full,seq,par"
SKIP_BUILD=0
REUSE_REPO=0
COVY_BIN="$COVY_BIN_DEFAULT"
TIMINGS_PATH=""
EXPECTED_TESTS_FILE=""

MAVEN_BASE_ARGS=()
MAVEN_TEST_GOALS=(
  org.jacoco:jacoco-maven-plugin:prepare-agent
  test
  org.jacoco:jacoco-maven-plugin:report
)

usage() {
  cat <<'USAGE'
Usage: scripts/bench_apache.sh [options]

Runs Covy benchmark flows against apache/commons-lang:
1) full maven + jacoco + covy check
2) sequential sharded runs + covy merge
3) parallel sharded runs + covy merge (two-pass: seed + measured)

Options:
  --work-dir <path>      Benchmark workspace root (default: /tmp/covy-bench-apache-<timestamp>)
  --repo-dir <path>      Repo directory to use/clone into (default: <work-dir>/commons-lang)
  --m2-repo <path>       Maven local repo path (default: /tmp/m2)
  --timings-path <path>  Timings state path (default: <work-dir>/state/testtimings.bin)
  --shards <n>           Number of shards for seq/par modes (default: 8)
  --modes <csv>          Subset of modes: full,seq,par (default: full,seq,par)
  --covy-bin <path>      Covy binary path (default: target/release/covy)
  --skip-build           Do not build Covy binary
  --reuse-repo           Reuse existing repo-dir instead of recloning
  -h, --help             Show this help
USAGE
}

log() {
  printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*" >&2
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

require_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || fail "Missing required command: $cmd"
}

has_mode() {
  local needle="$1"
  [[ ",$MODES," == *",$needle,"* ]]
}

extract_real_secs() {
  local log_file="$1"
  awk '/^real /{v=$2} END{if(v==""){v="0"}; print v}' "$log_file"
}

extract_full_test_summary() {
  local log_file="$1"
  awk '
    /\] Tests run:/ {line=$0}
    END {
      if (line == "") {
        print "Tests run: unknown"
      } else {
        sub(/^.*\] /, "", line)
        print line
      }
    }
  ' "$log_file"
}

coverage_from_report_json() {
  local json_file="$1"
  python3 - "$json_file" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
cov = sum(file.get("lines_covered", 0) for file in data.get("files", []))
ins = sum(file.get("lines_instrumented", 0) for file in data.get("files", []))
pct = (cov / ins * 100.0) if ins else 0.0
print(f"{pct:.2f}\t{cov}\t{ins}\t{len(data.get('files', []))}")
PY
}

normalize_set() {
  local input="$1"
  local output="$2"
  if [[ -s "$input" ]]; then
    sed '/^\s*$/d' "$input" | sort -u > "$output"
  else
    : > "$output"
  fi
}

fail_with_diff() {
  local label="$1"
  local diff_file="$2"
  echo "ERROR: parity mismatch for $label" >&2
  if [[ -s "$diff_file" ]]; then
    echo "--- $diff_file ---" >&2
    cat "$diff_file" >&2
  fi
  exit 1
}

compare_test_sets() {
  local expected_raw="$1"
  local actual_raw="$2"
  local diff_file="$3"
  local label="$4"
  local expected_norm actual_norm missing unexpected

  expected_norm="${diff_file}.expected.norm"
  actual_norm="${diff_file}.actual.norm"
  missing="${diff_file}.missing"
  unexpected="${diff_file}.unexpected"

  normalize_set "$expected_raw" "$expected_norm"
  normalize_set "$actual_raw" "$actual_norm"

  comm -23 "$expected_norm" "$actual_norm" > "$missing"
  comm -13 "$expected_norm" "$actual_norm" > "$unexpected"

  {
    echo "label: $label"
    echo "expected_count: $(wc -l < "$expected_norm" | tr -d ' ')"
    echo "actual_count: $(wc -l < "$actual_norm" | tr -d ' ')"
    echo
    echo "missing_from_actual:"
    cat "$missing"
    echo
    echo "unexpected_in_actual:"
    cat "$unexpected"
  } > "$diff_file"

  if [[ -s "$missing" || -s "$unexpected" ]]; then
    fail_with_diff "$label" "$diff_file"
  fi
}

build_expected_manifest() {
  local out_file="$1"
  mkdir -p "$(dirname "$out_file")"
  rg --files "$REPO_DIR/src/test/java" -g '*Test.java' \
    | sed -E 's#^.*/src/test/java/##; s#/#.#g; s#\.java$##' \
    | sort -u > "$out_file"

  if [[ ! -s "$out_file" ]]; then
    fail "No tests discovered in $REPO_DIR/src/test/java with pattern *Test.java"
  fi
}

extract_executed_test_classes() {
  local surefire_dir="$1"
  local out_file="$2"

  [[ -d "$surefire_dir" ]] || fail "Missing surefire reports dir: $surefire_dir"

  python3 - "$surefire_dir" "$out_file" <<'PY'
import glob
import os
import sys
import xml.etree.ElementTree as ET

reports_dir, out_file = sys.argv[1], sys.argv[2]
paths = sorted(glob.glob(os.path.join(reports_dir, "TEST-*.xml")))
if not paths:
    raise SystemExit(f"No surefire XML files found in {reports_dir}")

classes = set()
for path in paths:
    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as exc:
        raise SystemExit(f"Failed to parse XML {path}: {exc}") from exc

    for testcase in root.iter("testcase"):
        classname = (testcase.attrib.get("classname") or "").strip()
        if classname:
            classes.add(classname)

if not classes:
    raise SystemExit(f"No testcase classname entries found in {reports_dir}")

with open(out_file, "w", encoding="utf-8") as fh:
    for item in sorted(classes):
        fh.write(item + "\n")
PY
}

record_maven_invocation() {
  local out_file="$1"
  local tests_csv="${2:-}"
  {
    printf '%q ' "${MAVEN_BASE_ARGS[@]}"
    if [[ -n "$tests_csv" ]]; then
      printf '%q ' "-Dtest=$tests_csv"
    fi
    printf '%q ' "${MAVEN_TEST_GOALS[@]}"
    echo
  } > "$out_file"
}

collect_shard_union() {
  local shard_dir="$1"
  local out_file="$2"

  if ! compgen -G "$shard_dir/shard-*.txt" >/dev/null; then
    fail "No shard files found under $shard_dir"
  fi

  cat "$shard_dir"/shard-*.txt | sed '/^\s*$/d' | sort -u > "$out_file"
}

plan_shards() {
  local run_dir="$1"
  local label="$2"

  mkdir -p "$run_dir/shards" "$run_dir/meta"
  cp "$EXPECTED_TESTS_FILE" "$run_dir/tests.txt"

  "$COVY_BIN" shard plan \
    --shards "$SHARDS" \
    --tests-file "$run_dir/tests.txt" \
    --timings "$TIMINGS_PATH" \
    --write-files "$run_dir/shards" \
    --json > "$run_dir/shard-plan.json"

  collect_shard_union "$run_dir/shards" "$run_dir/meta/planned-tests.txt"
  compare_test_sets \
    "$EXPECTED_TESTS_FILE" \
    "$run_dir/meta/planned-tests.txt" \
    "$run_dir/meta/planned-vs-expected.diff" \
    "$label planned-vs-expected"
}

run_one_shard() {
  local repo="$1"
  local shard_file="$2"
  local shard_name="$3"
  local run_dir="$4"
  local label="$5"

  local tests_csv start end elapsed xml_src xml_dst bin_dst log_file ingest_log executed_file planned_file
  local diff_file status_file xml_bytes

  planned_file="$run_dir/meta/planned-$shard_name.txt"
  normalize_set "$shard_file" "$planned_file"
  [[ -s "$planned_file" ]] || fail "$label: shard $shard_name has no planned tests"

  tests_csv="$(paste -sd, "$shard_file")"
  [[ -n "$tests_csv" ]] || fail "$label: shard $shard_name produced empty selector list"

  record_maven_invocation "$run_dir/meta/maven-$shard_name.cmd" "$tests_csv"

  rm -f "$repo/target/jacoco.exec"
  rm -rf "$repo/target/site/jacoco" "$repo/target/surefire-reports"

  start="$(date +%s)"
  set +e
  (
    cd "$repo"
    "${MAVEN_BASE_ARGS[@]}" \
      "-Dtest=$tests_csv" \
      "${MAVEN_TEST_GOALS[@]}"
  ) > "$run_dir/logs/mvn-$shard_name.log" 2>&1
  local mvn_ec=$?
  set -e
  end="$(date +%s)"
  elapsed="$((end - start))"

  [[ "$mvn_ec" -eq 0 ]] || fail "$label: Maven failed for $shard_name (see $run_dir/logs/mvn-$shard_name.log)"

  executed_file="$run_dir/meta/executed-$shard_name.txt"
  extract_executed_test_classes "$repo/target/surefire-reports" "$executed_file"

  diff_file="$run_dir/meta/parity-$shard_name.diff"
  compare_test_sets "$planned_file" "$executed_file" "$diff_file" "$label $shard_name planned-vs-executed"

  xml_src="$repo/target/site/jacoco/jacoco.xml"
  xml_dst="$run_dir/xml/jacoco-$shard_name.xml"
  bin_dst="$run_dir/bin/$shard_name.bin"
  [[ -s "$xml_src" ]] || fail "$label: missing jacoco XML for $shard_name at $xml_src"
  cp "$xml_src" "$xml_dst"
  xml_bytes="$(wc -c < "$xml_dst" | tr -d ' ')"

  ingest_log="$run_dir/logs/covy-ingest-$shard_name.log"
  set +e
  "$COVY_BIN" ingest "$xml_dst" --format jacoco --output "$bin_dst" --color never > "$ingest_log" 2>&1
  local covy_ec=$?
  set -e
  [[ "$covy_ec" -eq 0 ]] || fail "$label: covy ingest failed for $shard_name (see $ingest_log)"

  status_file="$run_dir/status/$shard_name.tsv"
  printf "%s\t%s\t%s\t%s\t%s\n" "$shard_name" "$mvn_ec" "$covy_ec" "$elapsed" "$xml_bytes" > "$status_file"
}

finalize_shard_status() {
  local run_dir="$1"
  local table="$2"
  cat "$run_dir"/status/*.tsv | sort > "$table"
}

collect_executed_union() {
  local run_dir="$1"
  local out_file="$2"
  cat "$run_dir"/meta/executed-*.txt | sed '/^\s*$/d' | sort -u > "$out_file"
}

merge_and_report() {
  local run_dir="$1"

  /usr/bin/time -p -o "$run_dir/merged/merge.time" "$COVY_BIN" merge \
    --coverage "$run_dir/bin/shard-*.bin" \
    --output-coverage "$run_dir/merged/latest.bin" --json > "$run_dir/merged/merge-summary.json"

  /usr/bin/time -p -o "$run_dir/merged/report.time" "$COVY_BIN" report \
    --input "$run_dir/merged/latest.bin" --format json > "$run_dir/merged/report.json"
}

run_full() {
  local dir="$WORK_DIR/full"
  mkdir -p "$dir/meta"

  local full_log="$dir/mvn_full.log"
  local report_log="$dir/mvn_report.log"
  local covy_log="$dir/covy_check.log"

  log "Running full Maven test + JaCoCo"
  record_maven_invocation "$dir/meta/maven-full.cmd"
  set +e
  (
    cd "$REPO_DIR"
    /usr/bin/time -p "${MAVEN_BASE_ARGS[@]}" "${MAVEN_TEST_GOALS[@]}"
  ) >"$full_log" 2>&1
  FULL_MVN_EC=$?
  set -e
  FULL_MVN_REAL="$(extract_real_secs "$full_log")"
  [[ "$FULL_MVN_EC" -eq 0 ]] || fail "full: Maven test run failed (see $full_log)"

  extract_executed_test_classes "$REPO_DIR/target/surefire-reports" "$dir/meta/executed-tests.txt"
  compare_test_sets \
    "$EXPECTED_TESTS_FILE" \
    "$dir/meta/executed-tests.txt" \
    "$dir/meta/full-vs-expected.diff" \
    "full expected-vs-executed"

  log "Generating jacoco.xml for full run (if needed)"
  (
    cd "$REPO_DIR"
    /usr/bin/time -p mvn -q -Dmaven.repo.local="$M2_REPO" org.jacoco:jacoco-maven-plugin:report
  ) >"$report_log" 2>&1
  FULL_REPORT_REAL="$(extract_real_secs "$report_log")"

  log "Running covy check on full-run jacoco.xml"
  /usr/bin/time -p "$COVY_BIN" check "$REPO_DIR/target/site/jacoco/jacoco.xml" \
    --format jacoco --color never >"$covy_log" 2>&1
  FULL_COVY_REAL="$(extract_real_secs "$covy_log")"
  FULL_COVY_COVERAGE="$(awk '/Total coverage:/{v=$3} END{if(v==""){v="unknown"}; print v}' "$covy_log")"
  FULL_TEST_SUMMARY="$(extract_full_test_summary "$full_log")"
}

run_seq() {
  local dir="$WORK_DIR/seq"
  mkdir -p "$dir/logs" "$dir/xml" "$dir/bin" "$dir/meta" "$dir/status" "$dir/merged"

  log "Building shard plan for sequential mode"
  plan_shards "$dir" "seq"

  log "Running sequential shard jobs"
  local seq_start seq_end
  seq_start="$(date +%s)"
  for shard_file in "$dir"/shards/shard-*.txt; do
    local shard_name
    shard_name="$(basename "$shard_file" .txt)"
    run_one_shard "$REPO_DIR" "$shard_file" "$shard_name" "$dir" "seq"
  done
  seq_end="$(date +%s)"

  SEQ_WALL="$((seq_end - seq_start))"
  finalize_shard_status "$dir" "$dir/meta/shard_status.tsv"
  SEQ_SUM_SHARDS="$(awk -F'\t' '{s+=$4} END{print s+0}' "$dir/meta/shard_status.tsv")"

  collect_executed_union "$dir" "$dir/meta/executed-tests.txt"
  compare_test_sets \
    "$EXPECTED_TESTS_FILE" \
    "$dir/meta/executed-tests.txt" \
    "$dir/meta/executed-vs-expected.diff" \
    "seq expected-vs-executed"

  log "Merging sequential shard artifacts"
  merge_and_report "$dir"

  local seq_cov
  seq_cov="$(coverage_from_report_json "$dir/merged/report.json")"
  SEQ_COVERAGE_PCT="$(echo "$seq_cov" | awk '{print $1}')"
}

run_parallel_pass() {
  local pass_dir="$1"
  local pass_label="$2"

  mkdir -p "$pass_dir/logs" "$pass_dir/xml" "$pass_dir/bin" "$pass_dir/meta" "$pass_dir/status" "$pass_dir/merged" "$pass_dir/clones"

  plan_shards "$pass_dir" "$pass_label"

  log "$pass_label: preparing isolated clones"
  for i in $(seq 1 "$SHARDS"); do
    rm -rf "$pass_dir/clones/shard-$i"
    git clone --quiet --shared "$REPO_DIR" "$pass_dir/clones/shard-$i"
  done

  local pass_start pass_end
  pass_start="$(date +%s)"

  local pids=()
  for i in $(seq 1 "$SHARDS"); do
    local shard_name="shard-$i"
    local repo="$pass_dir/clones/$shard_name"
    local shard_file="$pass_dir/shards/$shard_name.txt"

    [[ -f "$shard_file" ]] || fail "$pass_label: missing planned shard file $shard_file"

    run_one_shard "$repo" "$shard_file" "$shard_name" "$pass_dir" "$pass_label" &
    pids+=("$!")
  done

  local any_failed=0
  for pid in "${pids[@]}"; do
    if ! wait "$pid"; then
      any_failed=1
    fi
  done
  [[ "$any_failed" -eq 0 ]] || fail "$pass_label: one or more shards failed"

  pass_end="$(date +%s)"
  local pass_wall="$((pass_end - pass_start))"

  finalize_shard_status "$pass_dir" "$pass_dir/meta/shard_status.tsv"
  collect_executed_union "$pass_dir" "$pass_dir/meta/executed-tests.txt"
  compare_test_sets \
    "$EXPECTED_TESTS_FILE" \
    "$pass_dir/meta/executed-tests.txt" \
    "$pass_dir/meta/executed-vs-expected.diff" \
    "$pass_label expected-vs-executed"

  merge_and_report "$pass_dir"

  local pass_cov
  pass_cov="$(coverage_from_report_json "$pass_dir/merged/report.json")"

  if [[ "$pass_label" == "par-pass1" ]]; then
    PAR_PASS1_WALL="$pass_wall"
  fi

  echo "$pass_wall|$(echo "$pass_cov" | awk '{print $1}')"
}

run_par() {
  local dir="$WORK_DIR/par"
  local pass1_dir="$dir/pass1"
  local pass2_dir="$dir/pass2"
  mkdir -p "$dir"

  log "Running parallel pass 1 (seed timings)"
  local pass1_out
  pass1_out="$(run_parallel_pass "$pass1_dir" "par-pass1")"

  log "Updating timings from pass 1 JUnit XML"
  local update_glob="$pass1_dir/clones/shard-*/target/surefire-reports/TEST-*.xml"
  {
    printf '%q ' "$COVY_BIN" shard update --junit-xml "$update_glob" --timings "$TIMINGS_PATH" --junit-id-granularity class --json
    echo
  } > "$pass1_dir/meta/shard-update.cmd"
  "$COVY_BIN" shard update \
    --junit-xml "$update_glob" \
    --timings "$TIMINGS_PATH" \
    --junit-id-granularity class \
    --json > "$pass1_dir/meta/timings-update.json"

  log "Running parallel pass 2 (measured)"
  local pass2_out
  pass2_out="$(run_parallel_pass "$pass2_dir" "par-pass2")"

  PAR_WALL="${pass2_out%%|*}"
  PAR_COVERAGE_PCT="${pass2_out##*|}"

  PAR_SUM_SHARDS="$(awk -F'\t' '{s+=$4} END{print s+0}' "$pass2_dir/meta/shard_status.tsv")"
  PAR_MAX_SHARD="$(awk -F'\t' 'BEGIN{m=0}{if($4>m)m=$4}END{print m}' "$pass2_dir/meta/shard_status.tsv")"
}

print_summary() {
  local full_total=0 seq_speed=0 par_speed=0 par_vs_seq=0
  if has_mode full; then
    full_total="$(awk -v a="$FULL_MVN_REAL" -v b="$FULL_REPORT_REAL" -v c="$FULL_COVY_REAL" 'BEGIN{printf "%.2f", a+b+c}')"
  fi
  if has_mode full && has_mode seq; then
    seq_speed="$(awk -v f="$full_total" -v s="$SEQ_WALL" 'BEGIN{if(s==0){print "0"} else {printf "%.2f", f/s}}')"
  fi
  if has_mode full && has_mode par; then
    par_speed="$(awk -v f="$full_total" -v p="$PAR_WALL" 'BEGIN{if(p==0){print "0"} else {printf "%.2f", f/p}}')"
  fi
  if has_mode seq && has_mode par; then
    par_vs_seq="$(awk -v s="$SEQ_WALL" -v p="$PAR_WALL" 'BEGIN{if(p==0){print "0"} else {printf "%.2f", s/p}}')"
  fi

  echo
  echo "=== Apache Covy Benchmark Summary ==="
  echo "work_dir: $WORK_DIR"
  echo "repo_dir: $REPO_DIR"
  echo "timings_path: $TIMINGS_PATH"
  echo "shards: $SHARDS"
  echo
  if has_mode full; then
    echo "full.mvn_exit_code: $FULL_MVN_EC"
    echo "full.test_summary: $FULL_TEST_SUMMARY"
    echo "full.mvn_real_s: $FULL_MVN_REAL"
    echo "full.report_real_s: $FULL_REPORT_REAL"
    echo "full.covy_check_real_s: $FULL_COVY_REAL"
    echo "full.total_real_s: $full_total"
    echo "full.coverage_pct: $FULL_COVY_COVERAGE"
    echo "full.executed_tests: $WORK_DIR/full/meta/executed-tests.txt"
    echo
  fi
  if has_mode seq; then
    echo "seq.wall_s: $SEQ_WALL"
    echo "seq.sum_shard_s: $SEQ_SUM_SHARDS"
    echo "seq.coverage_pct: $SEQ_COVERAGE_PCT"
    echo "seq.expected_tests: $WORK_DIR/seq/meta/expected-tests.txt"
    echo "seq.executed_tests: $WORK_DIR/seq/meta/executed-tests.txt"
    echo "seq.status_tsv: $WORK_DIR/seq/meta/shard_status.tsv"
    echo
  fi
  if has_mode par; then
    echo "par.pass1.wall_s: ${PAR_PASS1_WALL:-0}"
    echo "par.wall_s: $PAR_WALL"
    echo "par.sum_shard_s: $PAR_SUM_SHARDS"
    echo "par.max_shard_s: $PAR_MAX_SHARD"
    echo "par.coverage_pct: $PAR_COVERAGE_PCT"
    echo "par.pass1.dir: $WORK_DIR/par/pass1"
    echo "par.pass2.dir: $WORK_DIR/par/pass2"
    echo "par.status_tsv: $WORK_DIR/par/pass2/meta/shard_status.tsv"
    echo
  fi
  if has_mode full && has_mode seq; then
    echo "speed.seq_vs_full_x: $seq_speed"
  fi
  if has_mode full && has_mode par; then
    echo "speed.par_vs_full_x: $par_speed"
  fi
  if has_mode seq && has_mode par; then
    echo "speed.par_vs_seq_x: $par_vs_seq"
  fi
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --work-dir)
        WORK_DIR="$2"
        shift 2
        ;;
      --repo-dir)
        REPO_DIR="$2"
        shift 2
        ;;
      --m2-repo)
        M2_REPO="$2"
        shift 2
        ;;
      --timings-path)
        TIMINGS_PATH="$2"
        shift 2
        ;;
      --shards)
        SHARDS="$2"
        shift 2
        ;;
      --modes)
        MODES="$2"
        shift 2
        ;;
      --covy-bin)
        COVY_BIN="$2"
        shift 2
        ;;
      --skip-build)
        SKIP_BUILD=1
        shift
        ;;
      --reuse-repo)
        REUSE_REPO=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        usage
        exit 1
        ;;
    esac
  done
}

main() {
  parse_args "$@"

  require_cmd git
  require_cmd mvn
  require_cmd rg
  require_cmd python3
  require_cmd /usr/bin/time

  mkdir -p "$WORK_DIR"
  if [[ -z "$REPO_DIR" ]]; then
    REPO_DIR="$WORK_DIR/commons-lang"
  fi
  if [[ -z "$TIMINGS_PATH" ]]; then
    TIMINGS_PATH="$WORK_DIR/state/testtimings.bin"
  fi
  mkdir -p "$(dirname "$TIMINGS_PATH")"

  MAVEN_BASE_ARGS=(
    mvn
    -q
    "-Dmaven.repo.local=$M2_REPO"
    -DskipITs
    -DfailIfNoTests=false
    -Dsurefire.failIfNoSpecifiedTests=false
  )

  if [[ "$SKIP_BUILD" -eq 0 ]]; then
    log "Building covy release binary"
    (cd "$ROOT_DIR" && cargo build --release -p covy-cli >/dev/null)
  fi
  [[ -x "$COVY_BIN" ]] || fail "Covy binary not found or not executable: $COVY_BIN"

  if [[ "$REUSE_REPO" -eq 1 && -d "$REPO_DIR/.git" ]]; then
    log "Reusing existing repo at $REPO_DIR"
  else
    log "Cloning apache/commons-lang into $REPO_DIR"
    rm -rf "$REPO_DIR"
    git clone --depth 1 https://github.com/apache/commons-lang.git "$REPO_DIR" >/dev/null
  fi

  EXPECTED_TESTS_FILE="$WORK_DIR/seq/meta/expected-tests.txt"
  build_expected_manifest "$EXPECTED_TESTS_FILE"

  if has_mode full; then
    run_full
  fi
  if has_mode seq; then
    run_seq
  fi
  if has_mode par; then
    run_par
  fi

  print_summary
}

main "$@"
