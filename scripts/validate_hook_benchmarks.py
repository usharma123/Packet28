#!/usr/bin/env python3

import argparse
import json
import sys
from pathlib import Path


DEFAULT_THRESHOLDS = {
    "mean_token_reduction_pct": 85.0,
    "cases": {
        "git_status": {"min_reduction_pct": 90.0, "min_raw_tokens": 100},
        "fs_head": {"min_reduction_pct": 70.0, "min_raw_tokens": 40},
        "rust_test": {"min_reduction_pct": 90.0, "min_raw_tokens": 100},
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


def load_summary(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def validate(summary: dict) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    notes: list[str] = []

    mean = summary.get("mean_token_reduction_pct")
    if mean is None:
        errors.append("summary is missing mean_token_reduction_pct")
    elif mean < DEFAULT_THRESHOLDS["mean_token_reduction_pct"]:
        errors.append(
            f"mean token reduction {mean}% is below required {DEFAULT_THRESHOLDS['mean_token_reduction_pct']}%"
        )
    else:
        notes.append(f"mean token reduction {mean}% passed")

    by_case = {result.get("case"): result for result in summary.get("results", [])}
    for case, config in DEFAULT_THRESHOLDS["cases"].items():
        result = by_case.get(case)
        if result is None:
            notes.append(f"{case}: skipped (case not present)")
            continue
        if result.get("status") != "ok":
            errors.append(f"{case}: benchmark execution failed")
            continue
        raw_tokens = result.get("raw_est_tokens", 0)
        if raw_tokens < config["min_raw_tokens"]:
            notes.append(
                f"{case}: skipped threshold check (raw tokens {raw_tokens} < {config['min_raw_tokens']})"
            )
            continue
        reduction = result.get("token_reduction_pct")
        if reduction is None:
            errors.append(f"{case}: missing token_reduction_pct")
            continue
        if reduction < config["min_reduction_pct"]:
            errors.append(
                f"{case}: reduction {reduction}% is below required {config['min_reduction_pct']}%"
            )
        else:
            notes.append(f"{case}: reduction {reduction}% passed")

    return errors, notes


def render_markdown(summary: dict, errors: list[str], notes: list[str]) -> str:
    lines = [
        "# Hook Benchmark Validation",
        "",
        f"- Summary: `{summary.get('artifact_dir', '<unknown>')}/summary.json`",
        f"- Status: `{'failed' if errors else 'passed'}`",
        "",
    ]
    if notes:
        lines.append("## Passed Checks")
        lines.append("")
        for note in notes:
            lines.append(f"- {note}")
        lines.append("")
    if errors:
        lines.append("## Failures")
        lines.append("")
        for error in errors:
            lines.append(f"- {error}")
        lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate Packet28 hook benchmark artifacts against regression thresholds."
    )
    parser.add_argument(
        "summary_path",
        help="Path to hook benchmark summary.json",
    )
    parser.add_argument(
        "--markdown-path",
        default=None,
        help="Optional markdown output path for validation results",
    )
    args = parser.parse_args()

    summary_path = Path(args.summary_path).resolve()
    summary = load_summary(summary_path)
    errors, notes = validate(summary)
    markdown = render_markdown(summary, errors, notes)

    if args.markdown_path:
        markdown_path = Path(args.markdown_path).resolve()
        markdown_path.parent.mkdir(parents=True, exist_ok=True)
        markdown_path.write_text(markdown, encoding="utf-8")

    sys.stdout.write(markdown + "\n")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
