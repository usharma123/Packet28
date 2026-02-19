#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$ROOT_DIR/benchmarks/generated"
mkdir -p "$OUT_DIR"

LCOV_100K_LINES="${LCOV_100K_LINES:-100000}"
LCOV_1M_LINES="${LCOV_1M_LINES:-1000000}"
SARIF_50K_ISSUES="${SARIF_50K_ISSUES:-50000}"
SARIF_200K_ISSUES="${SARIF_200K_ISSUES:-200000}"

gen_lcov() {
  local out="$1"
  local lines="$2"
  awk -v n="$lines" 'BEGIN {
    print "TN:";
    print "SF:src/generated.rs";
    for (i = 1; i <= n; i++) {
      printf "DA:%d,1\n", i;
    }
    print "end_of_record";
  }' > "$out"
}

gen_sarif() {
  local out="$1"
  local issues="$2"

  {
    printf '{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"bench-lint","rules":[{"id":"bench/rule"}]}},"results":[\n'

    awk -v n="$issues" 'BEGIN {
      for (i = 1; i <= n; i++) {
        printf "{\"ruleId\":\"bench/rule\",\"level\":\"warning\",\"message\":{\"text\":\"Synthetic issue %d\"},\"locations\":[{\"physicalLocation\":{\"artifactLocation\":{\"uri\":\"src/generated.rs\"},\"region\":{\"startLine\":%d}}}],\"partialFingerprints\":{\"primaryLocationLineHash\":\"fp-%d\"}}", i, i, i;
        if (i < n) {
          printf ",";
        }
        printf "\n";
      }
    }'

    printf ']}]}\n'
  } > "$out"
}

echo "Generating LCOV fixtures..."
gen_lcov "$OUT_DIR/lcov-100k.info" "$LCOV_100K_LINES"
gen_lcov "$OUT_DIR/lcov-1m.info" "$LCOV_1M_LINES"

echo "Generating SARIF fixtures..."
gen_sarif "$OUT_DIR/sarif-50k.sarif" "$SARIF_50K_ISSUES"
gen_sarif "$OUT_DIR/sarif-200k.sarif" "$SARIF_200K_ISSUES"

echo "Done."
ls -lh \
  "$OUT_DIR/lcov-100k.info" \
  "$OUT_DIR/lcov-1m.info" \
  "$OUT_DIR/sarif-50k.sarif" \
  "$OUT_DIR/sarif-200k.sarif"
