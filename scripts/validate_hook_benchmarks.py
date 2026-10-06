#!/usr/bin/env python3

import argparse
import json
import sys
from pathlib import Path

from hook_benchmark_thresholds import DEFAULT_THRESHOLDS


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

    if not summary.get("live_case_integrity_ok", False):
        failures = summary.get("live_case_failures", [])
        if failures:
            errors.append(
                "live benchmark integrity failed: "
                + ", ".join(failures)
            )
        else:
            errors.append("live benchmark integrity failed")
    else:
        notes.append(
            "live benchmark integrity passed "
            f"({summary.get('successful_live_case_count', 0)}/"
            f"{summary.get('expected_live_case_count', 0)} successful)"
        )

    by_case = {result.get("case"): result for result in summary.get("results", [])}
    for case, config in DEFAULT_THRESHOLDS["cases"].items():
        result = by_case.get(case)
        if result is None:
            notes.append(f"{case}: skipped (case not present)")
            continue
        if result.get("status") != "ok":
            if result.get("status") == "passthrough":
                errors.append(f"{case}: command was not reduced by the explicitly requested CLI")
            else:
                detail = result.get("error") or "benchmark execution failed"
                errors.append(f"{case}: {detail}")
            continue
        reduced_preview = result.get("reduced_preview", "")
        for needle in config.get("required_reduced_substrings", []):
            if needle not in reduced_preview:
                errors.append(f"{case}: reduced output missing required substring {needle!r}")
        for needle in config.get("forbidden_reduced_substrings", []):
            if needle in reduced_preview:
                errors.append(f"{case}: reduced output contained forbidden substring {needle!r}")
        if config.get("read_window_integrity"):
            integrity = result.get("read_window_integrity", {})
            if (not integrity.get("passed")
                    or integrity.get("line_start") != 1
                    or integrity.get("line_end") != 5
                    or integrity.get("raw_line_count") != 5
                    or result.get("raw_exit_code") != 0
                    or result.get("reduced_exit_code") != 0):
                errors.append(f"{case}: exact five-line content/window/exit integrity failed")
            else:
                notes.append(
                    f"{case}: exact five-line content/window/exit integrity passed; "
                    f"{result.get('raw_est_tokens')} raw -> {result.get('reduced_est_tokens')} visible tokens "
                    f"({result.get('token_reduction_pct')}%) remains in the weighted mean. "
                    "Full contents plus line numbers replace the former 70% per-case savings claim."
                )
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
        "# Explicit CLI and Hook Capture Benchmark Validation",
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
