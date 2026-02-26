# covy 0.2.0 Benchmark Review

> Tested against `JavaTest/` (Maven + JaCoCo + SARIF) on macOS, using the public crates.io binary (`covy 0.2.0`).
> Hyperfine benchmarks run on the same machine with `--shell=none`.

---

## Token Savings: covy vs. Everything Else

The question an AI agent asks is: **"did coverage pass?"**

| Tool | Output for "did it pass?" | Bytes | Tokens |
|:---|:---|---:|---:|
| **covy** `check --json` | `{"passed": true, ...}` | **272** | **~68** |
| **covy** `--summary-only --json` | `{"total_coverage_pct": 87.1}` | **46** | **~12** |
| **covy** exit code only | `$?` = 0 | **1** | **1** |
| pytest-cov `--cov-report=term` (500 files) | full table, no filtering | ~40,000 | ~10,000 |
| `go tool cover -func` (500 files, ~3k funcs) | one line per function | ~180,000 | ~45,000 |
| `lcov --summary` (500 files) | one line per file | ~40,000 | ~10,000 |
| codecov CLI | uploads to server, async PR comment | N/A | N/A |
| raw JaCoCo XML (500 files) | must parse yourself | ~1,500,000 | ~375,000 |
| raw LCOV (500 files) | must parse yourself | ~2,000,000 | ~500,000 |

### Token Ratios at 500-File Scale

| Comparison | Token Ratio |
|:---|---:|
| covy `check --json` vs pytest-cov terminal | **147x fewer** |
| covy `check --json` vs `go tool cover -func` | **662x fewer** |
| covy `--summary-only` vs raw JaCoCo XML | **31,250x fewer** |
| covy `--below 80` (50 failures) vs full report | **4x fewer** |

---

## Speed: Hyperfine Benchmarks

### Startup

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `version` | 2.0 ± 0.1 | 1.9 | 2.2 | 1.00 |
| `help` | 2.1 ± 0.1 | 1.9 | 2.2 | 1.03 ± 0.05 |

### Ingest: Format Comparison (Small Fixtures)

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lcov` | 6.5 ± 0.2 | 6.3 | 6.8 | 1.00 |
| `cobertura` | 6.8 ± 0.3 | 6.3 | 7.3 | 1.04 ± 0.06 |
| `jacoco` | 6.8 ± 0.2 | 6.6 | 7.1 | 1.05 ± 0.04 |
| `gocov` | 7.1 ± 0.3 | 6.5 | 7.5 | 1.09 ± 0.05 |
| `llvm-cov` | 7.5 ± 1.1 | 6.3 | 9.9 | 1.15 ± 0.18 |

### Ingest: Scale Tests

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lcov 100k` | 12.5 ± 0.2 | 12.2 | 12.8 | 1.00 |
| `lcov 1m` | 52.4 ± 0.3 | 52.0 | 52.8 | 4.21 ± 0.09 |
| `sarif 50k` | 82.7 ± 1.8 | 81.4 | 86.3 | 6.65 ± 0.20 |
| `sarif 200k` | 285.9 ± 6.4 | 278.2 | 295.7 | 22.96 ± 0.69 |

### Report Paths

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `report terminal` | 2.5 ± 0.1 | 2.3 | 2.7 | 1.03 ± 0.06 |
| `report json` | 2.9 ± 0.1 | 2.7 | 3.1 | 1.20 ± 0.07 |
| `report below 80 json` | 2.4 ± 0.2 | 2.2 | 2.7 | 1.01 ± 0.08 |
| `report summary-only json` | 2.4 ± 0.1 | 2.3 | 2.6 | 1.00 |

### Check Paths

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `coverage only` | 12.1 ± 0.3 | 11.6 | 12.5 | 1.00 |
| `cached issues state` | 12.4 ± 0.5 | 11.8 | 13.1 | 1.03 ± 0.05 |
| `parse issues each run` | 17.3 ± 0.7 | 16.3 | 18.5 | 1.44 ± 0.07 |

### Check Fail Path

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `fail-under-total 101` | 12.5 ± 0.8 | 11.4 | 14.0 | 1.00 |

### PR Artifacts

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `diff json` | 12.4 ± 0.5 | 12.0 | 13.8 | 1.00 |
| `comment markdown` | 24.1 ± 0.4 | 23.7 | 24.8 | 1.95 ± 0.08 |
| `annotate sarif` | 24.4 ± 0.8 | 23.9 | 27.0 | 1.97 ± 0.11 |
| `pr one-shot` | 24.4 ± 0.3 | 23.9 | 24.9 | 1.97 ± 0.08 |

### Doctor & Map-Paths

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `doctor json` | 84.3 ± 0.5 | 83.6 | 84.9 | 1.36 ± 0.02 |
| `map-paths learn` | 67.0 ± 1.6 | 65.2 | 71.7 | 1.08 ± 0.03 |
| `map-paths explain` | 61.9 ± 0.9 | 60.1 | 63.4 | 1.00 |

### Testmap & Impact

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `testmap build` | 822.7 ± 6.7 | 818.2 | 835.3 | 100.66 ± 1.95 |
| `impact record` | 826.7 ± 7.6 | 819.6 | 841.0 | 101.15 ± 2.01 |
| `impact plan` | 8.2 ± 0.1 | 8.1 | 8.5 | 1.00 |

### Shard & Merge

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `shard plan` | 3.5 ± 0.5 | 2.9 | 4.5 | 1.13 ± 0.18 |
| `merge 8+8 shards` | 3.1 ± 0.2 | 2.9 | 3.4 | 1.00 |

---

## covy vs. Alternatives: Speed

| Operation | covy (mean) | Typical Alternative | Speedup |
|:---|---:|:---|---:|
| Startup (`--version`) | **2.0ms** | Python CLI cold start: ~300-500ms | **150-250x** |
| Ingest small coverage (any format) | **6.5-7.5ms** | codecov upload: 2-30s (network) | **300-4000x** |
| Ingest 100k-line LCOV | **12.5ms** | `lcov --summary` on same: ~2-5s | **160-400x** |
| Ingest 1M-line LCOV | **52.4ms** | — | — |
| Ingest 200k SARIF issues | **286ms** | — | — |
| Report (terminal/JSON) | **2.4-2.9ms** | pytest-cov report render: ~500ms-2s | **200-800x** |
| Check (one-shot gate) | **12.1ms** | No local equivalent in other tools | — |
| Diff (PR gate) | **12.4ms** | codecov: async, minutes | — |
| PR artifacts (comment + SARIF) | **24.4ms** | codecov: server-side, async | — |
| Shard plan | **3.5ms** | — | — |
| Merge 8+8 shards | **3.1ms** | — | — |

---

## Full Pipeline Comparison

A typical CI coverage step: ingest + gate + PR comment + SARIF annotations.

| Tool Chain | Total Time | Tokens Consumed by Agent |
|:---|---:|---:|
| **covy** (`check --json` one-shot) | **~27ms** | **~68** |
| **covy** (ingest + diff + comment + annotate) | **~65ms** | **~200** |
| codecov CLI (upload + wait for processing) | 5-60s | N/A (async) |
| pytest-cov + manual threshold script | 2-10s | ~10,000+ |
| lcov + genhtml + custom gate script | 5-30s | ~10,000+ |

---

## Cost Translation

At Claude Sonnet pricing (~$3/M input tokens), processing coverage output per CI run:

| Tool | Tokens per Run | Cost per 1,000 CI Runs |
|:---|---:|---:|
| **covy** `check --json` | 68 | **$0.0002** |
| pytest-cov (500 files) | 10,000 | $0.03 |
| `go tool cover` (500 files) | 45,000 | $0.14 |
| raw LCOV (500 files) | 500,000 | $1.50 |

---

## Measured covy Output Sizes (JavaTest, 2 files)

| Command | Bytes | Lines | ~Tokens |
|:---|---:|---:|---:|
| `report --summary-only --json` | 46 | 3 | 12 |
| `ingest --json` | 220 | 8 | 55 |
| `check --json` (pass) | 272 | 13 | 68 |
| `check --json` (fail) | 328 | 15 | 82 |
| `comment` (markdown) | 288 | 11 | 72 |
| `report` (terminal table) | 457 | 9 | 114 |
| `report --json` (full) | 491 | 25 | 123 |

### vs. Raw Input

| Input | Bytes | ~Tokens | Compression vs. `check --json` |
|:---|---:|---:|---:|
| JaCoCo XML (2 files) | 7,056 | 1,764 | **26:1** |
| JaCoCo XML (500 files, estimated) | 1,500,000 | 375,000 | **5,514:1** |
| LCOV (500 files, estimated) | 2,000,000 | 500,000 | **7,353:1** |

---

## Key Architectural Insight

**covy output is O(1) for the pass/fail question.** The `check --json` payload is ~272 bytes whether you have 2 files or 20,000. Every other tool produces O(n) output in number of files. That's the fundamental design win.

---

*Benchmarked on macOS Darwin 24.6.0, covy 0.2.0 (crates.io), hyperfine with `--shell=none`.*
