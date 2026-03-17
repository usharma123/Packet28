#!/usr/bin/env python3

import argparse
import json
import sys
from pathlib import Path


DEFAULT_THRESHOLDS = {
    "mean_token_reduction_pct": 75.0,
    "cases": {
        "native_search": {"min_reduction_pct": 80.0},
        "native_read_regions": {"min_reduction_pct": 60.0},
        "native_glob": {"min_reduction_pct": 80.0},
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

    if summary.get("artifact_fetch_success_count") != summary.get("case_count"):
        errors.append(
            "artifact fetch coverage failed: "
            f"{summary.get('artifact_fetch_success_count', 0)}/{summary.get('case_count', 0)}"
        )
    else:
        notes.append(
            "artifact fetch coverage passed "
            f"({summary.get('artifact_fetch_success_count', 0)}/{summary.get('case_count', 0)})"
        )

    by_case = {result.get("case"): result for result in summary.get("results", [])}
    for case, config in DEFAULT_THRESHOLDS["cases"].items():
        result = by_case.get(case)
        if result is None:
            errors.append(f"{case}: benchmark result missing")
            continue
        if result.get("status") != "ok":
            errors.append(f"{case}: benchmark execution failed")
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
        "# Native MCP Benchmark Validation",
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
        description="Validate Packet28 native MCP benchmark artifacts against regression thresholds."
    )
    parser.add_argument("summary_path", help="Path to native MCP benchmark summary.json")
    parser.add_argument(
        "--markdown-path",
        default=None,
        help="Optional markdown output path for validation results",
    )
    args = parser.parse_args()

    summary = load_summary(Path(args.summary_path).resolve())
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
