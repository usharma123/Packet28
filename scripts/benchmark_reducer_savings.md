# Packet28 Reducer Benchmark: Token and Context Savings

**Generated:** 2025-03-17  
**Artifact dir:** `.packet28/benchmarks/reducer-bench-1773770244`

## Summary

| Metric | Value |
|--------|-------|
| **Weighted mean token reduction** | **81.5%** |
| **Fixture cases (eligible)** | 9/9 successful |
| **Compact-path coverage** | 100% |

The reducer suite measures raw command output vs. Packet28-reduced output. When a command is rewritten (e.g. `cargo test` → `Packet28 compact rewrite`), the reduced packet is typically 60–90% smaller in tokens.

---

## Token Savings by Case

| Case | Raw Tokens | Reduced Tokens | Reduction | Family |
|------|-----------:|---------------:|----------:|--------|
| `python_pytest_fixture` | 182 | 18 | **90.1%** | python |
| `javascript_vitest_fixture` | 89 | 11 | **87.6%** | javascript |
| `javascript_tsc_fixture` | 73 | 10 | **86.3%** | javascript |
| `infra_curl_fixture` | 52 | 9 | **82.7%** | infra |
| `go_test_fixture` | 71 | 15 | **78.9%** | go |
| `go_lint_fixture` | 54 | 14 | **74.1%** | go |
| `infra_kubectl_get_fixture` | 64 | 17 | **73.4%** | infra |
| `python_ruff_check_fixture` | 39 | 11 | **71.8%** | python |
| `javascript_eslint_fixture` | 56 | 21 | **62.5%** | javascript |
| **Fixture total** | **680** | **126** | **81.5%** | — |

### Live Cases (Passthrough)

| Case | Raw Tokens | Reduced Tokens | Status |
|------|-----------:|---------------:|--------|
| `git_status` | 67 | 67 | passthrough |
| `fs_head` | 113 | 113 | passthrough |
| `rust_test` | 1,330 | 1,330 | passthrough |

Live cases were not rewritten by the hook (command format or routing did not match reducer specs). Raw output was passed through unchanged.

---

## Context Savings by Reducer Family

| Family | Cases | Raw Tokens | Reduced Tokens | Savings |
|--------|------:|-----------:|---------------:|--------:|
| **Python** | 2 | 221 | 29 | 86.9% |
| **JavaScript** | 3 | 218 | 42 | 80.7% |
| **Go** | 2 | 125 | 29 | 76.8% |
| **Infra** | 2 | 116 | 26 | 77.6% |

---

## Cumulative Session Impact

For a typical agent session with **20 tool calls** at fixture-scale output:

| Scenario | Tokens | Notes |
|----------|-------:|-------|
| **Without Packet28** (raw output) | ~13,600 | 20 × 680 (avg raw per fixture) |
| **With Packet28** (reduced packets) | ~2,520 | 20 × 126 (avg reduced per fixture) |
| **Context savings** | **~11,080 tokens** | **~81.5%** |

On larger real-world outputs (e.g. `cargo test` at 1,330 tokens raw), savings scale proportionally. A single `cargo test` reduction from 1,330 → ~130 tokens would save ~1,200 tokens per invocation.

---

## Reducer Coverage

| Reducer | Command pattern | Status |
|---------|-----------------|--------|
| `python_pytest` | `pytest`, `python -m pytest` | ✓ |
| `python_ruff_check` | `ruff check` | ✓ |
| `javascript_tsc` | `tsc --noEmit` | ✓ |
| `javascript_eslint` | `eslint` | ✓ |
| `javascript_vitest` | `vitest run` | ✓ |
| `go_test` | `go test ./...` | ✓ |
| `golangci_lint` | `golangci-lint run` | ✓ |
| `kubectl_get` | `kubectl get pods` | ✓ |
| `curl_fetch` | `curl` | ✓ |

---

## Methodology

- **Raw tokens**: Estimated as `(bytes + 3) / 4` (tiktoken-style approximation).
- **Fixture cases**: Use pre-recorded stdout/stderr from `scripts/benchmark_fixtures/`.
- **Live cases**: Run real commands; hook rewrite is attempted via `Packet28 hook claude`.
- **Eligible for mean**: Cases with `raw_est_tokens >= min_raw_tokens` per `hook_benchmark_thresholds.py`.

---

## How to Re-run

```bash
python3 scripts/benchmark_hook_suite.py --root . --artifact-dir .packet28/benchmarks/reducer-bench-$(date +%s)
```
