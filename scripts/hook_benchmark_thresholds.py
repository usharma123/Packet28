#!/usr/bin/env python3

DEFAULT_THRESHOLDS = {
    "mean_token_reduction_pct": 85.0,
    "cases": {
        "git_status": {"min_reduction_pct": 90.0, "min_raw_tokens": 100},
        "fs_head": {"min_reduction_pct": 70.0, "min_raw_tokens": 40},
        "rust_test": {"min_reduction_pct": 90.0, "min_raw_tokens": 100},
        "gh_pr_list": {"min_reduction_pct": 80.0, "min_raw_tokens": 60},
        "gh_pr_view": {"min_reduction_pct": 80.0, "min_raw_tokens": 100},
        "gh_run_list": {"min_reduction_pct": 80.0, "min_raw_tokens": 80},
        "gh_run_view": {"min_reduction_pct": 90.0, "min_raw_tokens": 200},
        "python_pytest_fixture": {"min_reduction_pct": 80.0, "min_raw_tokens": 60},
        "python_ruff_check_fixture": {"min_reduction_pct": 60.0, "min_raw_tokens": 20},
        "javascript_tsc_fixture": {"min_reduction_pct": 80.0, "min_raw_tokens": 60},
        "javascript_eslint_fixture": {"min_reduction_pct": 60.0, "min_raw_tokens": 20},
        "javascript_vitest_fixture": {"min_reduction_pct": 80.0, "min_raw_tokens": 60},
        "go_test_fixture": {"min_reduction_pct": 70.0, "min_raw_tokens": 20},
        "go_lint_fixture": {"min_reduction_pct": 70.0, "min_raw_tokens": 20},
        "infra_kubectl_get_fixture": {"min_reduction_pct": 50.0, "min_raw_tokens": 20},
        "infra_curl_fixture": {"min_reduction_pct": 50.0, "min_raw_tokens": 20},
    },
}


def eligible_for_mean(result: dict) -> bool:
    if result.get("status") != "ok":
        return False
    case = result.get("case")
    raw_tokens = result.get("raw_est_tokens", 0)
    if raw_tokens <= 0:
        return False
    config = DEFAULT_THRESHOLDS["cases"].get(case)
    if not config:
        return False
    return raw_tokens >= config["min_raw_tokens"]
