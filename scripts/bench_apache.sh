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
AUTO_FALLBACK=1
FALLBACK_SHARDS=6
SETUP_THRESHOLD_S="15.0"
WARM_CACHE=1
OFFLINE=1
PARAM_BUCKETING_ENABLED=0

MAVEN_BASE_ARGS=()
MAVEN_RUNTIME_ARGS=()
MAVEN_TEST_GOALS=(
  org.jacoco:jacoco-maven-plugin:prepare-agent
  test
)

PAR_PASS1_SHARDS=0
PAR_PASS1_MEDIAN_SETUP_S="0.000"
PAR_EFFECTIVE_SHARDS=0
PAR_FALLBACK_USED=0

BUCKETED_TEST_CLASSES=(
  org.apache.commons.lang3.time.FastDateParser_TimeZoneStrategyTest
)

usage() {
  cat <<'USAGE'
Usage: scripts/bench_apache.sh [options]

Runs Covy benchmark flows against apache/commons-lang:
1) full maven + jacoco + covy check
2) sequential sharded runs + jacoco exec merge + single covy ingest/report
3) parallel sharded runs + jacoco exec merge + single covy ingest/report (two-pass: seed + measured)

Options:
  --work-dir <path>      Benchmark workspace root (default: /tmp/covy-bench-apache-<timestamp>)
  --repo-dir <path>      Repo directory to use/clone into (default: <work-dir>/commons-lang)
  --m2-repo <path>       Maven local repo path (default: /tmp/m2)
  --timings-path <path>  Timings state path (default: <work-dir>/state/testtimings.bin)
  --shards <n>           Number of shards for seq/par modes (default: 8)
  --modes <csv>          Subset of modes: full,seq,par (default: full,seq,par)
  --covy-bin <path>      Covy binary path (default: target/release/covy)
  --auto-fallback        Enable automatic fallback from 8 shards to fallback shard count (default: enabled)
  --no-auto-fallback     Disable shard-count fallback logic
  --fallback-shards <n>  Shard count to use when fallback triggers (default: 6)
  --setup-threshold-s <n>
                         Median setup threshold in seconds to trigger fallback (default: 15.0)
  --warm-cache           Run Maven warm-cache preflight before benchmark modes (default: enabled)
  --no-warm-cache        Disable warm-cache preflight
  --offline              Run Maven shard jobs in offline mode (default: enabled)
  --online               Disable Maven offline mode for shard jobs
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

extract_surefire_test_exec_secs() {
  local surefire_dir="$1"
  python3 - "$surefire_dir" <<'PY'
import glob
import os
import sys
import xml.etree.ElementTree as ET

reports_dir = sys.argv[1]
paths = sorted(glob.glob(os.path.join(reports_dir, "TEST-*.xml")))
if not paths:
    print("0.000")
    raise SystemExit(0)

total = 0.0
for path in paths:
    root = ET.parse(path).getroot()
    for testcase in root.iter("testcase"):
        raw = testcase.attrib.get("time")
        if not raw:
            continue
        try:
            total += float(raw)
        except ValueError:
            continue
print(f"{total:.3f}")
PY
}

median_setup_secs() {
  local status_tsv="$1"
  python3 - "$status_tsv" <<'PY'
import sys

values = []
with open(sys.argv[1], encoding="utf-8") as fh:
    for line in fh:
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 7:
            continue
        try:
            values.append(float(parts[6]))
        except ValueError:
            continue

if not values:
    print("0.000")
    raise SystemExit(0)

values.sort()
n = len(values)
mid = n // 2
if n % 2 == 1:
    med = values[mid]
else:
    med = (values[mid - 1] + values[mid]) / 2.0
print(f"{med:.3f}")
PY
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
    if [[ "${#MAVEN_RUNTIME_ARGS[@]}" -gt 0 ]]; then
      printf '%q ' "${MAVEN_RUNTIME_ARGS[@]}"
    fi
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

supports_param_bucketing() {
  local test_file="$REPO_DIR/src/test/java/org/apache/commons/lang3/time/FastDateParser_TimeZoneStrategyTest.java"
  [[ -f "$test_file" ]] || return 1
  rg -q "TEST_SHARD_COUNT" "$test_file"
}

inject_bucketed_tests() {
  local shard_dir="$1"
  local shard_count="$2"
  local class_name shard_file

  [[ "$PARAM_BUCKETING_ENABLED" -eq 1 ]] || return 0

  for class_name in "${BUCKETED_TEST_CLASSES[@]}"; do
    local present=0
    for shard_file in "$shard_dir"/shard-*.txt; do
      if rg -qxF "$class_name" "$shard_file"; then
        present=1
        break
      fi
    done
    [[ "$present" -eq 1 ]] || continue

    for i in $(seq 1 "$shard_count"); do
      shard_file="$shard_dir/shard-$i.txt"
      if ! rg -qxF "$class_name" "$shard_file"; then
        echo "$class_name" >> "$shard_file"
      fi
      sort -u "$shard_file" -o "$shard_file"
    done
  done
}

plan_shards() {
  local run_dir="$1"
  local label="$2"
  local shard_count="$3"

  mkdir -p "$run_dir/shards" "$run_dir/meta"
  cp "$EXPECTED_TESTS_FILE" "$run_dir/tests.txt"

  "$COVY_BIN" shard plan \
    --shards "$shard_count" \
    --tests-file "$run_dir/tests.txt" \
    --timings "$TIMINGS_PATH" \
    --write-files "$run_dir/shards" \
    --json > "$run_dir/shard-plan.json"

  inject_bucketed_tests "$run_dir/shards" "$shard_count"

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
  local shard_count="$6"

  local tests_csv start end elapsed exec_dst log_file executed_file planned_file
  local diff_file status_file exec_bytes test_exec setup_secs shard_index

  planned_file="$run_dir/meta/planned-$shard_name.txt"
  normalize_set "$shard_file" "$planned_file"
  [[ -s "$planned_file" ]] || fail "$label: shard $shard_name has no planned tests"

  tests_csv="$(paste -sd, "$shard_file")"
  [[ -n "$tests_csv" ]] || fail "$label: shard $shard_name produced empty selector list"

  record_maven_invocation "$run_dir/meta/maven-$shard_name.cmd" "$tests_csv"

  exec_dst="$run_dir/exec/$shard_name.exec"
  rm -f "$repo/target/jacoco.exec" "$exec_dst"
  rm -rf "$repo/target/site/jacoco" "$repo/target/surefire-reports"

  shard_index="${shard_name#shard-}"
  shard_index="$((shard_index - 1))"

  start="$(date +%s)"
  set +e
  (
    cd "$repo"
    if [[ "$PARAM_BUCKETING_ENABLED" -eq 1 ]]; then
      TEST_SHARD_INDEX="$shard_index" \
        TEST_SHARD_COUNT="$shard_count" \
        "${MAVEN_BASE_ARGS[@]}" \
        "${MAVEN_RUNTIME_ARGS[@]}" \
        "-Dtest=$tests_csv" \
        "-Djacoco.destFile=$exec_dst" \
        "${MAVEN_TEST_GOALS[@]}"
    else
      "${MAVEN_BASE_ARGS[@]}" \
        "${MAVEN_RUNTIME_ARGS[@]}" \
        "-Dtest=$tests_csv" \
        "-Djacoco.destFile=$exec_dst" \
        "${MAVEN_TEST_GOALS[@]}"
    fi
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

  [[ -s "$exec_dst" ]] || fail "$label: missing jacoco exec for $shard_name at $exec_dst"
  exec_bytes="$(wc -c < "$exec_dst" | tr -d ' ')"
  test_exec="$(extract_surefire_test_exec_secs "$repo/target/surefire-reports")"
  setup_secs="$(awk -v wall="$elapsed" -v tests="$test_exec" 'BEGIN{s=wall-tests; if(s<0)s=0; printf "%.3f", s}')"

  status_file="$run_dir/status/$shard_name.tsv"
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
    "$shard_name" "$mvn_ec" "0" "$elapsed" "$exec_bytes" "$test_exec" "$setup_secs" > "$status_file"
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
  local report_repo="$2"
  local merge_pom="$run_dir/meta/jacoco-merge-pom.xml"
  local merged_exec="$run_dir/jacoco/merged.exec"
  local xml_file="$run_dir/jacoco/jacoco.xml"
  local merge_log="$run_dir/logs/mvn-jacoco-merge.log"
  local report_log="$run_dir/logs/mvn-jacoco-report.log"

  mkdir -p "$run_dir/jacoco" "$run_dir/report" "$run_dir/bin"
  if ! compgen -G "$run_dir/exec/*.exec" >/dev/null; then
    fail "No per-shard jacoco exec files found under $run_dir/exec"
  fi

  cat > "$merge_pom" <<EOF
<project xmlns="http://maven.apache.org/POM/4.0.0"
         xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 https://maven.apache.org/xsd/maven-4.0.0.xsd">
  <modelVersion>4.0.0</modelVersion>
  <groupId>local.covy</groupId>
  <artifactId>jacoco-merge</artifactId>
  <version>1.0.0</version>
  <build>
    <plugins>
      <plugin>
        <groupId>org.jacoco</groupId>
        <artifactId>jacoco-maven-plugin</artifactId>
        <version>0.8.14</version>
        <configuration>
          <fileSets>
            <fileSet>
              <directory>$run_dir/exec</directory>
              <includes>
                <include>*.exec</include>
              </includes>
            </fileSet>
          </fileSets>
          <destFile>$merged_exec</destFile>
        </configuration>
      </plugin>
    </plugins>
  </build>
</project>
EOF

  {
    printf '%q ' "${MAVEN_BASE_ARGS[@]}"
    printf '%q ' "${MAVEN_RUNTIME_ARGS[@]}"
    printf '%q ' "-f" "$merge_pom" "org.jacoco:jacoco-maven-plugin:merge"
    echo
  } > "$run_dir/meta/jacoco-merge.cmd"

  set +e
  /usr/bin/time -p -o "$run_dir/jacoco/merge.time" \
    "${MAVEN_BASE_ARGS[@]}" "${MAVEN_RUNTIME_ARGS[@]}" \
    -f "$merge_pom" org.jacoco:jacoco-maven-plugin:merge > "$merge_log" 2>&1
  local merge_ec=$?
  set -e
  [[ "$merge_ec" -eq 0 ]] || fail "JaCoCo merge failed (see $merge_log)"
  [[ -s "$merged_exec" ]] || fail "Merged JaCoCo exec not created: $merged_exec"

  if [[ ! -d "$report_repo/target/classes" ]]; then
    set +e
    (
      cd "$report_repo"
      "${MAVEN_BASE_ARGS[@]}" "${MAVEN_RUNTIME_ARGS[@]}" -DskipTests test-compile
    ) > "$run_dir/logs/mvn-ensure-classes.log" 2>&1
    local classes_ec=$?
    set -e
    [[ "$classes_ec" -eq 0 ]] || fail "Unable to build classes for JaCoCo report (see $run_dir/logs/mvn-ensure-classes.log)"
  fi

  rm -f "$xml_file"
  {
    printf '%q ' "${MAVEN_BASE_ARGS[@]}"
    printf '%q ' "${MAVEN_RUNTIME_ARGS[@]}"
    printf '%q ' "-Djacoco.dataFile=$merged_exec" "-Djacoco.outputDirectory=$run_dir/jacoco" "-Djacoco.formats=XML"
    printf '%q ' "org.jacoco:jacoco-maven-plugin:report"
    echo
  } > "$run_dir/meta/jacoco-report.cmd"

  set +e
  (
    cd "$report_repo"
    /usr/bin/time -p -o "$run_dir/jacoco/report.time" \
      "${MAVEN_BASE_ARGS[@]}" "${MAVEN_RUNTIME_ARGS[@]}" \
      "-Djacoco.dataFile=$merged_exec" \
      "-Djacoco.outputDirectory=$run_dir/jacoco" \
      -Djacoco.formats=XML \
      org.jacoco:jacoco-maven-plugin:report
  ) > "$report_log" 2>&1
  local report_ec=$?
  set -e
  [[ "$report_ec" -eq 0 ]] || fail "JaCoCo report generation failed (see $report_log)"
  [[ -s "$xml_file" ]] || fail "Missing merged JaCoCo XML at $xml_file"

  set +e
  /usr/bin/time -p -o "$run_dir/report/ingest.time" \
    "$COVY_BIN" ingest "$xml_file" --format jacoco --output "$run_dir/bin/coverage.bin" --color never \
    > "$run_dir/logs/covy-ingest.log" 2>&1
  local ingest_ec=$?
  set -e
  [[ "$ingest_ec" -eq 0 ]] || fail "covy ingest failed (see $run_dir/logs/covy-ingest.log)"

  /usr/bin/time -p -o "$run_dir/report/report.time" "$COVY_BIN" report \
    --input "$run_dir/bin/coverage.bin" --format json > "$run_dir/report/report.json"
}

cache_state_snapshot() {
  local repo_path="$1"
  local prefix="$2"
  local out_file="$3"

  if [[ -d "$repo_path" ]]; then
    local file_count byte_count
    file_count="$(find "$repo_path" -type f 2>/dev/null | wc -l | tr -d ' ')"
    byte_count="$(du -sk "$repo_path" 2>/dev/null | awk '{print $1}')"
    {
      echo "${prefix}_state=present"
      echo "${prefix}_files=$file_count"
      echo "${prefix}_kilobytes=$byte_count"
    } >> "$out_file"
  else
    {
      echo "${prefix}_state=absent"
      echo "${prefix}_files=0"
      echo "${prefix}_kilobytes=0"
    } >> "$out_file"
  fi
}

record_global_metadata_start() {
  local meta_dir="$WORK_DIR/meta"
  local file="$meta_dir/environment.txt"
  mkdir -p "$meta_dir"

  {
    echo "date_utc=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "work_dir=$WORK_DIR"
    echo "repo_dir=$REPO_DIR"
    echo "timings_path=$TIMINGS_PATH"
    echo "m2_repo=$M2_REPO"
    echo "configured_shards=$SHARDS"
    echo "auto_fallback=$AUTO_FALLBACK"
    echo "fallback_shards=$FALLBACK_SHARDS"
    echo "setup_threshold_s=$SETUP_THRESHOLD_S"
    echo "warm_cache=$WARM_CACHE"
    echo "offline=$OFFLINE"
    echo "param_bucketing_enabled=$PARAM_BUCKETING_ENABLED"
    echo "covy_sha=$(git -C "$ROOT_DIR" rev-parse --short HEAD)"
    echo "target_repo_sha=$(git -C "$REPO_DIR" rev-parse --short HEAD)"
    printf 'maven_base_args='
    printf '%q ' "${MAVEN_BASE_ARGS[@]}"
    echo
    printf 'maven_runtime_args='
    printf '%q ' "${MAVEN_RUNTIME_ARGS[@]}"
    echo
  } > "$file"

  cache_state_snapshot "$M2_REPO" "m2_start" "$file"
  mvn -v > "$meta_dir/maven-version.txt" 2>&1 || true
}

record_global_metadata_end() {
  local file="$WORK_DIR/meta/environment.txt"
  {
    echo "par_pass1_shards=$PAR_PASS1_SHARDS"
    echo "par_pass1_median_setup_s=$PAR_PASS1_MEDIAN_SETUP_S"
    echo "par_effective_shards=$PAR_EFFECTIVE_SHARDS"
    echo "par_fallback_used=$PAR_FALLBACK_USED"
  } >> "$file"
  cache_state_snapshot "$M2_REPO" "m2_end" "$file"
}

warm_cache_once() {
  local warm_dir="$WORK_DIR/warmup"
  mkdir -p "$warm_dir"
  log "Running Maven warm-cache preflight"

  set +e
  (
    cd "$REPO_DIR"
    "${MAVEN_BASE_ARGS[@]}" -DskipTests dependency:go-offline test-compile
  ) > "$warm_dir/mvn-warmup.log" 2>&1
  local warm_ec=$?
  set -e
  [[ "$warm_ec" -eq 0 ]] || fail "Warm-cache preflight failed (see $warm_dir/mvn-warmup.log)"

  set +e
  (
    cd "$REPO_DIR"
    "${MAVEN_BASE_ARGS[@]}" org.jacoco:jacoco-maven-plugin:help -Ddetail=false -Dgoal=merge
  ) > "$warm_dir/mvn-jacoco-plugin.log" 2>&1
  local jacoco_plugin_ec=$?
  set -e
  [[ "$jacoco_plugin_ec" -eq 0 ]] || fail "JaCoCo plugin prefetch failed (see $warm_dir/mvn-jacoco-plugin.log)"

  if [[ "$OFFLINE" -eq 1 ]]; then
    log "Validating offline Maven preflight"
    set +e
    (
      cd "$REPO_DIR"
      "${MAVEN_BASE_ARGS[@]}" -o -DskipTests test-compile
    ) > "$warm_dir/mvn-offline-check.log" 2>&1
    local offline_ec=$?
    set -e
    [[ "$offline_ec" -eq 0 ]] || fail "Offline validation failed (see $warm_dir/mvn-offline-check.log)"
  fi
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
    /usr/bin/time -p "${MAVEN_BASE_ARGS[@]}" "${MAVEN_RUNTIME_ARGS[@]}" "${MAVEN_TEST_GOALS[@]}"
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
    /usr/bin/time -p "${MAVEN_BASE_ARGS[@]}" "${MAVEN_RUNTIME_ARGS[@]}" org.jacoco:jacoco-maven-plugin:report
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
  mkdir -p "$dir/logs" "$dir/exec" "$dir/bin" "$dir/meta" "$dir/status" "$dir/jacoco" "$dir/report"

  log "Building shard plan for sequential mode"
  plan_shards "$dir" "seq" "$SHARDS"

  log "Running sequential shard jobs"
  local seq_start seq_end
  seq_start="$(date +%s)"
  for shard_file in "$dir"/shards/shard-*.txt; do
    local shard_name
    shard_name="$(basename "$shard_file" .txt)"
    run_one_shard "$REPO_DIR" "$shard_file" "$shard_name" "$dir" "seq" "$SHARDS"
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
  merge_and_report "$dir" "$REPO_DIR"

  local seq_cov
  seq_cov="$(coverage_from_report_json "$dir/report/report.json")"
  SEQ_COVERAGE_PCT="$(echo "$seq_cov" | awk '{print $1}')"
}

run_parallel_pass() {
  local pass_dir="$1"
  local pass_label="$2"
  local shard_count="$3"

  mkdir -p "$pass_dir/logs" "$pass_dir/exec" "$pass_dir/bin" "$pass_dir/meta" "$pass_dir/status" "$pass_dir/jacoco" "$pass_dir/report" "$pass_dir/clones"

  plan_shards "$pass_dir" "$pass_label" "$shard_count"

  log "$pass_label: preparing isolated clones"
  for i in $(seq 1 "$shard_count"); do
    rm -rf "$pass_dir/clones/shard-$i"
    git clone --quiet --shared "$REPO_DIR" "$pass_dir/clones/shard-$i"
  done

  local pass_start pass_end
  pass_start="$(date +%s)"

  local pids=()
  for i in $(seq 1 "$shard_count"); do
    local shard_name="shard-$i"
    local repo="$pass_dir/clones/$shard_name"
    local shard_file="$pass_dir/shards/$shard_name.txt"

    [[ -f "$shard_file" ]] || fail "$pass_label: missing planned shard file $shard_file"

    run_one_shard "$repo" "$shard_file" "$shard_name" "$pass_dir" "$pass_label" "$shard_count" &
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

  merge_and_report "$pass_dir" "$REPO_DIR"

  local pass_cov
  pass_cov="$(coverage_from_report_json "$pass_dir/report/report.json")"

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

  PAR_PASS1_SHARDS="$SHARDS"
  PAR_EFFECTIVE_SHARDS="$SHARDS"
  PAR_FALLBACK_USED=0

  log "Running parallel pass 1 (seed timings)"
  local pass1_out
  pass1_out="$(run_parallel_pass "$pass1_dir" "par-pass1" "$PAR_PASS1_SHARDS")"

  PAR_PASS1_MEDIAN_SETUP_S="$(median_setup_secs "$pass1_dir/meta/shard_status.tsv")"
  log "par-pass1 median setup/build seconds: $PAR_PASS1_MEDIAN_SETUP_S"

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

  local use_fallback=0
  if [[ "$AUTO_FALLBACK" -eq 1 && "$SHARDS" -eq 8 && "$FALLBACK_SHARDS" -gt 0 ]]; then
    use_fallback="$(awk -v median="$PAR_PASS1_MEDIAN_SETUP_S" -v threshold="$SETUP_THRESHOLD_S" 'BEGIN{if (median > threshold) print 1; else print 0}')"
  fi
  if [[ "$use_fallback" -eq 1 ]]; then
    PAR_EFFECTIVE_SHARDS="$FALLBACK_SHARDS"
    PAR_FALLBACK_USED=1
    log "Setup inflation detected (median=$PAR_PASS1_MEDIAN_SETUP_S > threshold=$SETUP_THRESHOLD_S), using fallback shard count: $PAR_EFFECTIVE_SHARDS"
  fi

  {
    echo "pass1_shards=$PAR_PASS1_SHARDS"
    echo "pass1_median_setup_s=$PAR_PASS1_MEDIAN_SETUP_S"
    echo "setup_threshold_s=$SETUP_THRESHOLD_S"
    echo "fallback_used=$PAR_FALLBACK_USED"
    echo "effective_shards=$PAR_EFFECTIVE_SHARDS"
  } > "$dir/meta/parallel-decision.txt"

  log "Running parallel pass 2 (measured)"
  local pass2_out
  pass2_out="$(run_parallel_pass "$pass2_dir" "par-pass2" "$PAR_EFFECTIVE_SHARDS")"

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
  echo "shards.configured: $SHARDS"
  echo "offline: $OFFLINE"
  echo "warm_cache: $WARM_CACHE"
  echo "auto_fallback: $AUTO_FALLBACK"
  echo "metadata: $WORK_DIR/meta/environment.txt"
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
    echo "par.pass1.shards: $PAR_PASS1_SHARDS"
    echo "par.pass1.wall_s: ${PAR_PASS1_WALL:-0}"
    echo "par.pass1.median_setup_s: $PAR_PASS1_MEDIAN_SETUP_S"
    echo "par.wall_s: $PAR_WALL"
    echo "par.sum_shard_s: $PAR_SUM_SHARDS"
    echo "par.max_shard_s: $PAR_MAX_SHARD"
    echo "par.fallback_used: $PAR_FALLBACK_USED"
    echo "par.effective_shards: $PAR_EFFECTIVE_SHARDS"
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
      --auto-fallback)
        AUTO_FALLBACK=1
        shift
        ;;
      --no-auto-fallback)
        AUTO_FALLBACK=0
        shift
        ;;
      --fallback-shards)
        FALLBACK_SHARDS="$2"
        shift 2
        ;;
      --setup-threshold-s)
        SETUP_THRESHOLD_S="$2"
        shift 2
        ;;
      --warm-cache)
        WARM_CACHE=1
        shift
        ;;
      --no-warm-cache)
        WARM_CACHE=0
        shift
        ;;
      --offline)
        OFFLINE=1
        shift
        ;;
      --online)
        OFFLINE=0
        shift
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
  MAVEN_RUNTIME_ARGS=()
  if [[ "$OFFLINE" -eq 1 ]]; then
    MAVEN_RUNTIME_ARGS=(-o)
  fi

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

  if supports_param_bucketing; then
    PARAM_BUCKETING_ENABLED=1
    log "Parameter bucketing support detected in target repo"
  else
    PARAM_BUCKETING_ENABLED=0
    log "Parameter bucketing support not detected in target repo"
  fi

  record_global_metadata_start

  if [[ "$WARM_CACHE" -eq 1 ]]; then
    warm_cache_once
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

  record_global_metadata_end
  print_summary
}

main "$@"
