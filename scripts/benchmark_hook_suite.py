#!/usr/bin/env python3

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

from benchmark_common import estimate_tokens, resolve_shell, run_capture
from hook_benchmark_thresholds import DEFAULT_THRESHOLDS, eligible_for_mean


def default_cases(gh_repo: str | None, gh_pr_number: str | None, gh_run_id: str | None) -> list[tuple[str, list[str]]]:
    cases = [
        ("git_status", ["git", "status"]),
        ("fs_head", ["head", "-n", "5", "README.md"]),
        ("rust_test", ["cargo", "test", "-p", "packet28-reducer-core", "--lib"]),
    ]
    if gh_repo:
        cases.append(("gh_pr_list", ["gh", "pr", "list", "--repo", gh_repo, "--limit", "5"]))
        if gh_pr_number:
            cases.append(("gh_pr_view", ["gh", "pr", "view", gh_pr_number, "--repo", gh_repo]))
        cases.append(("gh_run_list", ["gh", "run", "list", "--repo", gh_repo, "--limit", "5"]))
        if gh_run_id:
            cases.append(("gh_run_view", ["gh", "run", "view", gh_run_id, "--repo", gh_repo]))
    return cases


def fixture_cases(root: Path) -> list[dict]:
    fixtures = root / "scripts" / "benchmark_fixtures"
    return [
        {
            "case": "python_pytest_fixture",
            "command": "python3 -m pytest tests",
            "stdout_path": str(fixtures / "python" / "pytest_fail.stdout.txt"),
            "stderr_path": None,
            "exit_code": 1,
        },
        {
            "case": "python_ruff_check_fixture",
            "command": "ruff check src",
            "stdout_path": str(fixtures / "python" / "ruff_check.stdout.txt"),
            "stderr_path": None,
            "exit_code": 1,
        },
        {
            "case": "javascript_tsc_fixture",
            "command": "npx tsc --noEmit",
            "stdout_path": None,
            "stderr_path": str(fixtures / "javascript" / "tsc_fail.stderr.txt"),
            "exit_code": 2,
        },
        {
            "case": "javascript_eslint_fixture",
            "command": "eslint src",
            "stdout_path": str(fixtures / "javascript" / "eslint_fail.stdout.txt"),
            "stderr_path": None,
            "exit_code": 1,
        },
        {
            "case": "javascript_vitest_fixture",
            "command": "vitest run",
            "stdout_path": str(fixtures / "javascript" / "vitest_fail.stdout.txt"),
            "stderr_path": None,
            "exit_code": 1,
        },
        {
            "case": "go_test_fixture",
            "command": "go test ./...",
            "stdout_path": str(fixtures / "go" / "go_test.stdout.txt"),
            "stderr_path": None,
            "exit_code": 1,
        },
        {
            "case": "go_lint_fixture",
            "command": "golangci-lint run",
            "stdout_path": None,
            "stderr_path": str(fixtures / "go" / "golangci_lint.stderr.txt"),
            "exit_code": 1,
        },
        {
            "case": "infra_kubectl_get_fixture",
            "command": "kubectl get pods",
            "stdout_path": str(fixtures / "infra" / "kubectl_get.stdout.txt"),
            "stderr_path": None,
            "exit_code": 0,
        },
        {
            "case": "infra_curl_fixture",
            "command": "curl https://example.com",
            "stdout_path": str(fixtures / "infra" / "curl_fetch.stdout.txt"),
            "stderr_path": None,
            "exit_code": 0,
        },
    ]


def derive_origin_repo(root: Path) -> str | None:
    completed = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        cwd=str(root),
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        return None
    raw = completed.stdout.strip()
    if not raw:
        return None
    match = re.search(r"github\.com[:/](?P<owner>[^/]+)/(?P<repo>[^/.]+)(?:\.git)?$", raw)
    if not match:
        return None
    return f"{match.group('owner')}/{match.group('repo')}"


def discover_latest_pr_number(root: Path, gh_repo: str) -> str | None:
    completed = subprocess.run(
        [
            "gh",
            "pr",
            "list",
            "--repo",
            gh_repo,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number",
        ],
        cwd=str(root),
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        return None
    try:
        payload = json.loads(completed.stdout or "[]")
    except json.JSONDecodeError:
        return None
    if not payload:
        return None
    return str(payload[0].get("number")) if payload[0].get("number") is not None else None


def discover_latest_run_id(root: Path, gh_repo: str) -> str | None:
    completed = subprocess.run(
        [
            "gh",
            "run",
            "list",
            "--repo",
            gh_repo,
            "--limit",
            "1",
            "--json",
            "databaseId",
        ],
        cwd=str(root),
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        return None
    try:
        payload = json.loads(completed.stdout or "[]")
    except json.JSONDecodeError:
        return None
    if not payload:
        return None
    return (
        str(payload[0].get("databaseId"))
        if payload[0].get("databaseId") is not None
        else None
    )


def run_case(root: Path, artifact_dir: Path, case_name: str, argv: list[str], shell: str | None) -> dict:
    artifact_path = artifact_dir / f"{case_name}.json"
    cmd = [
        sys.executable,
        str(root / "scripts" / "benchmark_hook_rewrite.py"),
        "--root",
        str(root),
        "--json",
        "--artifact-path",
        str(artifact_path),
    ]
    if shell:
        cmd.extend(["--shell", shell])
    cmd.extend(["--", *argv])
    completed = subprocess.run(
        cmd,
        cwd=str(root),
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode == 0:
        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            error_payload = {
                "case": case_name,
                "status": "error",
                "surface": "live",
                "command": " ".join(argv),
                "error": f"Invalid JSON output: {exc}: {(completed.stderr or completed.stdout).strip()}",
                "artifact_path": str(artifact_path),
            }
            artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
            return error_payload
        payload["case"] = case_name
        payload["status"] = payload.get("status", "ok")
        payload["surface"] = "live"
        payload.setdefault("raw_output_recoverable", True)
        payload["artifact_path"] = str(artifact_path)
        return payload
    error_payload = {
        "case": case_name,
        "status": "error",
        "surface": "live",
        "command": " ".join(argv),
        "error": (completed.stderr or completed.stdout).strip(),
        "artifact_path": str(artifact_path),
    }
    artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
    return error_payload


def run_fixture_case(root: Path, artifact_dir: Path, case: dict) -> dict:
    artifact_path = artifact_dir / f"{case['case']}.json"
    cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "suite-cli",
        "--bin",
        "Packet28",
        "--",
        "hook",
        "reduce-fixture",
        "--command",
        case["command"],
        "--stdout-path",
        case["stdout_path"] or "/dev/null",
        "--exit-code",
        str(case["exit_code"]),
        "--json",
    ]
    if case.get("stderr_path"):
        cmd.extend(["--stderr-path", case["stderr_path"]])
    completed = subprocess.run(
        cmd,
        cwd=str(root),
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode == 0:
        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            error_payload = {
                "case": case["case"],
                "status": "error",
                "surface": "fixture",
                "command": case["command"],
                "error": f"Invalid JSON output: {exc}: {(completed.stderr or completed.stdout).strip()}",
                "artifact_path": str(artifact_path),
            }
            artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
            return error_payload
        payload["case"] = case["case"]
        payload["status"] = "ok"
        payload["surface"] = "fixture"
        payload.setdefault("compact_path", "fixture_reduce")
        payload.setdefault("raw_output_recoverable", False)
        payload["artifact_path"] = str(artifact_path)
        artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload
    error_payload = {
        "case": case["case"],
        "status": "error",
        "surface": "fixture",
        "command": case["command"],
        "error": (completed.stderr or completed.stdout).strip(),
        "artifact_path": str(artifact_path),
    }
    artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
    return error_payload


def run_compact_grep_integrity_case(root: Path, artifact_dir: Path, shell: str | None) -> dict:
    case_name = "grep_basic_alternation_integrity"
    artifact_path = artifact_dir / f"{case_name}.json"
    pattern = r"fn classify\|Mutation\|fn classify_command"
    target_path = "crates/packet28-reducer-core/src/command.rs"
    command_text = f"grep -n '{pattern}' {target_path}"
    try:
        shell_path = resolve_shell(shell)
    except FileNotFoundError as exc:
        payload = {
            "case": case_name,
            "status": "error",
            "surface": "live",
            "command": command_text,
            "error": f"benchmark shell setup failed: {exc}",
            "artifact_path": str(artifact_path),
        }
        artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload

    raw = run_capture([shell_path, "-lc", command_text], root)
    compact_cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "suite-cli",
        "--bin",
        "Packet28",
        "--",
        "compact",
        "grep",
        "--root",
        str(root),
        "--basic-regexp",
        pattern,
        target_path,
    ]
    reduced = run_capture(compact_cmd, root)
    raw_visible = raw.stdout + raw.stderr
    reduced_visible = reduced.stdout + reduced.stderr
    required = [
        f"{target_path}:16:",
        f"{target_path}:34:",
    ]
    forbidden = ["Search found 0 matches", "0 matches for"]
    integrity_errors = []
    if raw.returncode != 0:
        integrity_errors.append(f"raw grep exited {raw.returncode}")
    if reduced.returncode != 0:
        integrity_errors.append(f"compact grep exited {reduced.returncode}")
    for needle in required:
        if needle not in reduced_visible:
            integrity_errors.append(f"missing compact grep line marker: {needle}")
    for needle in forbidden:
        if needle in reduced_visible:
            integrity_errors.append(f"forbidden compact grep output: {needle}")

    raw_tokens = estimate_tokens(raw_visible)
    reduced_tokens = estimate_tokens(reduced_visible)
    payload = {
        "case": case_name,
        "status": "ok" if not integrity_errors else "error",
        "surface": "live",
        "command": command_text,
        "rewritten_command": " ".join(compact_cmd),
        "compact_path": "native_compact_grep",
        "raw_output_recoverable": True,
        "raw_exit_code": raw.returncode,
        "reduced_exit_code": reduced.returncode,
        "raw_bytes": len(raw_visible.encode("utf-8")),
        "raw_est_tokens": raw_tokens,
        "reduced_bytes": len(reduced_visible.encode("utf-8")),
        "reduced_est_tokens": reduced_tokens,
        "raw_preview": raw_visible[:400],
        "reduced_preview": reduced_visible[:400],
        "token_reduction_pct": round(100.0 * (raw_tokens - reduced_tokens) / raw_tokens, 1)
        if raw_tokens
        else 0.0,
        "required_reduced_substrings": required,
        "forbidden_reduced_substrings": forbidden,
        "integrity_errors": integrity_errors,
        "artifact_path": str(artifact_path),
    }
    if integrity_errors:
        payload["error"] = "; ".join(integrity_errors)
    artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return payload


def run_bash_posttool_grep_capture_case(root: Path, artifact_dir: Path, shell: str | None) -> dict:
    case_name = "bash_posttool_grep_capture_integrity"
    artifact_path = artifact_dir / f"{case_name}.json"
    task_id = f"bench-posttool-grep-{int(time.time())}"
    pattern = r"fn classify\|Mutation\|fn classify_command"
    target_path = "crates/packet28-reducer-core/src/command.rs"
    command_text = f"grep -n '{pattern}' {target_path}"
    required_regions = [
        f"{target_path}:16-16",
        f"{target_path}:34-34",
    ]
    try:
        shell_path = resolve_shell(shell)
    except FileNotFoundError as exc:
        payload = {
            "case": case_name,
            "status": "error",
            "surface": "live",
            "command": command_text,
            "error": f"benchmark shell setup failed: {exc}",
            "artifact_path": str(artifact_path),
        }
        artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload

    raw = run_capture([shell_path, "-lc", command_text], root)
    raw_visible = raw.stdout + raw.stderr
    hook_payload = json.dumps(
        {
            "hook_event_name": "PostToolUse",
            "task_id": task_id,
            "session_id": f"{task_id}-session",
            "cwd": str(root),
            "tool_name": "Bash",
            "tool_input": {"command": command_text},
            "tool_response": {
                "stdout": raw.stdout,
                "stderr": raw.stderr,
                "exit_code": raw.returncode,
            },
        }
    )
    hook_cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "suite-cli",
        "--bin",
        "Packet28",
        "--",
        "hook",
        "claude",
        "--root",
        str(root),
    ]
    hook = run_capture(hook_cmd, root, hook_payload)
    status_cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "suite-cli",
        "--bin",
        "Packet28",
        "--",
        "daemon",
        "task",
        "status",
        "--root",
        str(root),
        "--task-id",
        task_id,
        "--json",
    ]
    status = run_capture(status_cmd, root)
    packet = None
    status_payload = {}
    if status.returncode == 0:
        try:
            status_payload = json.loads(status.stdout or "{}")
        except json.JSONDecodeError:
            status_payload = {}
        for entry in status_payload.get("hook_reducer_cache", {}).values():
            if entry.get("canonical_command_kind") == "grep":
                packet = entry
                break

    integrity_errors = []
    if raw.returncode != 0:
        integrity_errors.append(f"raw grep exited {raw.returncode}")
    if hook.returncode != 0:
        integrity_errors.append(f"PostToolUse hook exited {hook.returncode}")
    if (
        not hook.stdout.strip()
        and "allowing runtime action after processing error" in hook.stderr
    ):
        integrity_errors.append(f"PostToolUse hook processing failed: {hook.stderr.strip()}")
    if status.returncode != 0:
        integrity_errors.append(f"task status exited {status.returncode}: {status.stderr.strip()}")
    if packet is None:
        integrity_errors.append("task status did not contain a grep hook reducer cache entry")
        packet = {}
    if packet.get("reducer_family") != "shell_native":
        integrity_errors.append(
            f"expected reducer_family shell_native, got {packet.get('reducer_family')!r}"
        )
    if packet.get("canonical_command_kind") != "grep":
        integrity_errors.append(
            f"expected canonical_command_kind grep, got {packet.get('canonical_command_kind')!r}"
        )
    regions = packet.get("regions", [])
    for region in required_regions:
        if region not in regions:
            integrity_errors.append(f"missing captured grep region: {region}")
    reduced_preview = packet.get("compact_preview") or packet.get("summary") or ""
    for needle in [f"{target_path}:16:", f"{target_path}:34:"]:
        if needle not in reduced_preview:
            integrity_errors.append(f"missing compact preview line marker: {needle}")
    if "0 matches" in reduced_preview:
        integrity_errors.append("captured grep preview reported 0 matches")

    raw_tokens = estimate_tokens(raw_visible)
    reduced_tokens = estimate_tokens(reduced_preview)
    payload = {
        "case": case_name,
        "status": "ok" if not integrity_errors else "error",
        "surface": "live",
        "command": command_text,
        "rewritten_command": None,
        "compact_path": "bash_grep_post_capture",
        "raw_output_recoverable": True,
        "raw_exit_code": raw.returncode,
        "reduced_exit_code": hook.returncode,
        "raw_bytes": len(raw_visible.encode("utf-8")),
        "raw_est_tokens": raw_tokens,
        "reduced_bytes": len(reduced_preview.encode("utf-8")),
        "reduced_est_tokens": reduced_tokens,
        "raw_preview": raw_visible[:400],
        "reduced_preview": reduced_preview[:400],
        "token_reduction_pct": round(100.0 * (raw_tokens - reduced_tokens) / raw_tokens, 1)
        if raw_tokens
        else 0.0,
        "task_id": task_id,
        "required_regions": required_regions,
        "captured_regions": regions,
        "integrity_errors": integrity_errors,
        "artifact_path": str(artifact_path),
    }
    if integrity_errors:
        payload["error"] = "; ".join(integrity_errors)
    artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return payload


def build_summary(
    results: list[dict],
    root: Path,
    artifact_dir: Path,
    gh_repo: str | None,
    gh_pr_number: str | None,
    gh_run_id: str | None,
) -> dict:
    ok_results = [result for result in results if result["status"] == "ok"]
    live_results = [result for result in results if result.get("surface") == "live"]
    fixture_results = [result for result in results if result.get("surface") == "fixture"]
    eligible_results = [result for result in ok_results if eligible_for_mean(result)]
    eligible_live_results = [result for result in eligible_results if result.get("surface") == "live"]
    eligible_fixture_results = [
        result for result in eligible_results if result.get("surface") == "fixture"
    ]
    live_failures = [result["case"] for result in live_results if result["status"] != "ok"]
    compact_coverage = [
        result for result in ok_results if result.get("compact_path") and result.get("raw_est_tokens", 0) > 0
    ]

    def mean_reduction(items: list[dict]) -> float | None:
        if not items:
            return None
        raw_total = sum(result["raw_est_tokens"] for result in items)
        reduced_total = sum(result["reduced_est_tokens"] for result in items)
        if raw_total <= 0:
            return None
        return round(100.0 * (raw_total - reduced_total) / raw_total, 1)

    return {
        "root": str(root),
        "gh_repo": gh_repo,
        "gh_pr_number": gh_pr_number,
        "gh_run_id": gh_run_id,
        "artifact_dir": str(artifact_dir),
        "measured_at_unix": int(time.time()),
        "case_count": len(results),
        "success_count": len(ok_results),
        "error_count": len(results) - len(ok_results),
        "mean_token_reduction_pct": mean_reduction(eligible_results),
        "live_case_mean_token_reduction_pct": mean_reduction(eligible_live_results),
        "fixture_mean_token_reduction_pct": mean_reduction(eligible_fixture_results),
        "eligible_case_count": len(eligible_results),
        "eligible_live_case_count": len(eligible_live_results),
        "eligible_fixture_case_count": len(eligible_fixture_results),
        "expected_live_case_count": len(live_results),
        "successful_live_case_count": sum(1 for result in live_results if result["status"] == "ok"),
        "live_case_integrity_ok": not live_failures,
        "live_case_failures": live_failures,
        "compact_path_coverage_pct": (
            round(100.0 * len(compact_coverage) / len(ok_results), 1) if ok_results else None
        ),
        "recoverable_output_case_count": sum(
            1 for result in ok_results if result.get("raw_output_recoverable")
        ),
        "head_integrity_contract": (
            "fs_head preserves the exact first five lines and exit; line numbering can increase "
            "visible tokens; exact content integrity replaces the former 70% per-case savings assertion, "
            "and its actual result remains in the 85% aggregate"
        ),
        "results": results,
    }


def render_text(summary: dict) -> str:
    lines = [
        f"artifact dir: {summary['artifact_dir']}",
        f"gh repo: {summary['gh_repo'] or '<none>'}",
        f"gh pr: {summary['gh_pr_number'] or '<none>'}",
        f"gh run: {summary['gh_run_id'] or '<none>'}",
    ]
    if summary.get("mean_token_reduction_pct") is not None:
        lines.append(
            f"eligible weighted mean reduction: {summary['mean_token_reduction_pct']}% "
            f"(live={summary.get('live_case_mean_token_reduction_pct')}, "
            f"fixture={summary.get('fixture_mean_token_reduction_pct')})"
        )
    lines.append(
        "live integrity: "
        f"{summary['successful_live_case_count']}/{summary['expected_live_case_count']} successful"
    )
    lines.append(summary["head_integrity_contract"])
    for result in summary["results"]:
        if result["status"] != "ok":
            detail = result.get("error") or f"status={result.get('status', 'unknown')}"
            lines.append(f"{result['case']}: ERROR")
            lines.append(f"  command: {result['command']}")
            lines.append(f"  detail: {detail}")
            lines.append(f"  artifact: {result['artifact_path']}")
            continue
        lines.append(
            f"{result['case']}: {result['raw_est_tokens']}t raw -> {result['reduced_est_tokens']}t reduced "
            f"({result['token_reduction_pct']}% reduction)"
        )
        lines.append(f"  command: {result['command']}")
        lines.append(
            f"  reduced preview: {result['reduced_preview'].strip() or '<empty>'}"
        )
        lines.append(f"  artifact: {result['artifact_path']}")
    return "\n".join(lines)


def render_markdown(summary: dict) -> str:
    lines = [
        "# Explicit CLI and Hook Capture Benchmark Suite",
        "",
        f"- Artifact dir: `{summary['artifact_dir']}`",
        f"- GitHub repo: `{summary['gh_repo'] or '<none>'}`",
        f"- PR seed: `{summary['gh_pr_number'] or '<none>'}`",
        f"- Run seed: `{summary['gh_run_id'] or '<none>'}`",
        f"- Head contract: {summary['head_integrity_contract']}",
    ]
    if summary.get("mean_token_reduction_pct") is not None:
        lines.append(
            f"- Weighted mean token reduction across eligible cases: `{summary['mean_token_reduction_pct']}%`"
        )
    if summary.get("live_case_mean_token_reduction_pct") is not None:
        lines.append(
            f"- Live-case weighted eligible mean reduction: `{summary['live_case_mean_token_reduction_pct']}%`"
        )
    if summary.get("fixture_mean_token_reduction_pct") is not None:
        lines.append(
            f"- Fixture weighted eligible mean reduction: `{summary['fixture_mean_token_reduction_pct']}%`"
        )
    lines.append(
        f"- Live benchmark integrity: `{summary['successful_live_case_count']}/{summary['expected_live_case_count']} successful`"
    )
    if summary.get("compact_path_coverage_pct") is not None:
        lines.append(
            f"- Compact-path coverage across successful cases: `{summary['compact_path_coverage_pct']}%`"
        )
    lines.extend(
        [
            "",
            "| Case | Raw Tokens | Reduced Tokens | Reduction | Preview |",
            "| --- | ---: | ---: | ---: | --- |",
        ]
    )
    for result in summary["results"]:
        if result["status"] != "ok":
            detail = (result.get("error") or f"status={result.get('status', 'unknown')}")[:120]
            detail = str(detail).replace("|", "\\|")
            lines.append(
                f"| `{result['case']}` | {result.get('raw_est_tokens', 'error')} | {result.get('reduced_est_tokens', 'error')} | n/a | `{detail}` |"
            )
            continue
        preview = (result["reduced_preview"].strip() or "<empty>").replace("|", "\\|")
        lines.append(
            f"| `{result['case']}` | {result['raw_est_tokens']} | {result['reduced_est_tokens']} | {result['token_reduction_pct']}% | `{preview}` |"
        )
    return "\n".join(lines) + os.linesep


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run explicit CLI reduction and capture-only hook integrity benchmarks."
    )
    parser.add_argument("--root", default=".", help="Repository root")
    parser.add_argument("--json", action="store_true", help="Emit JSON")
    parser.add_argument(
        "--artifact-dir",
        default=None,
        help="Directory for per-case and summary JSON artifacts",
    )
    parser.add_argument(
        "--gh-repo",
        default=None,
        help="GitHub repo in owner/name form for gh benchmark cases",
    )
    parser.add_argument(
        "--derive-gh-repo",
        action="store_true",
        help="Use the git origin remote as the gh repo benchmark target",
    )
    parser.add_argument(
        "--shell",
        default=None,
        help="Shell binary used for raw live benchmark execution",
    )
    args = parser.parse_args()

    root = Path(args.root).resolve()
    gh_repo = args.gh_repo
    if args.derive_gh_repo and not gh_repo:
        gh_repo = derive_origin_repo(root)
    if gh_repo and shutil.which("gh") is None:
        gh_repo = None
    gh_pr_number = discover_latest_pr_number(root, gh_repo) if gh_repo else None
    gh_run_id = discover_latest_run_id(root, gh_repo) if gh_repo else None

    artifact_dir = (
        Path(args.artifact_dir).resolve()
        if args.artifact_dir
        else root / ".packet28" / "benchmarks" / f"hook-suite-{int(time.time())}"
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)

    results = [
        run_case(root, artifact_dir, name, argv, args.shell)
        for name, argv in default_cases(gh_repo, gh_pr_number, gh_run_id)
    ]
    results.append(run_compact_grep_integrity_case(root, artifact_dir, args.shell))
    results.append(run_bash_posttool_grep_capture_case(root, artifact_dir, args.shell))
    results.extend(run_fixture_case(root, artifact_dir, case) for case in fixture_cases(root))
    summary = build_summary(results, root, artifact_dir, gh_repo, gh_pr_number, gh_run_id)
    (artifact_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + os.linesep, encoding="utf-8"
    )
    (artifact_dir / "summary.md").write_text(render_markdown(summary), encoding="utf-8")

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(render_text(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
