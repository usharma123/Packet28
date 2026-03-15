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


def run_case(root: Path, artifact_dir: Path, case_name: str, argv: list[str]) -> dict:
    artifact_path = artifact_dir / f"{case_name}.json"
    cmd = [
        sys.executable,
        str(root / "scripts" / "benchmark_hook_rewrite.py"),
        "--root",
        str(root),
        "--json",
        "--artifact-path",
        str(artifact_path),
        "--",
        *argv,
    ]
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
                "command": " ".join(argv),
                "error": f"Invalid JSON output: {exc}: {(completed.stderr or completed.stdout).strip()}",
                "artifact_path": str(artifact_path),
            }
            artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
            return error_payload
        payload["case"] = case_name
        payload["status"] = "ok"
        payload["artifact_path"] = str(artifact_path)
        return payload
    error_payload = {
        "case": case_name,
        "status": "error",
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
                "command": case["command"],
                "error": f"Invalid JSON output: {exc}: {(completed.stderr or completed.stdout).strip()}",
                "artifact_path": str(artifact_path),
            }
            artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
            return error_payload
        payload["case"] = case["case"]
        payload["status"] = "ok"
        payload["artifact_path"] = str(artifact_path)
        artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload
    error_payload = {
        "case": case["case"],
        "status": "error",
        "command": case["command"],
        "error": (completed.stderr or completed.stdout).strip(),
        "artifact_path": str(artifact_path),
    }
    artifact_path.write_text(json.dumps(error_payload, indent=2) + "\n", encoding="utf-8")
    return error_payload


def build_summary(
    results: list[dict],
    root: Path,
    artifact_dir: Path,
    gh_repo: str | None,
    gh_pr_number: str | None,
    gh_run_id: str | None,
) -> dict:
    ok_results = [result for result in results if result["status"] == "ok"]
    token_reductions = [
        result["token_reduction_pct"]
        for result in ok_results
        if result.get("raw_est_tokens", 0) > 0
    ]
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
        "mean_token_reduction_pct": (
            round(sum(token_reductions) / len(token_reductions), 1)
            if token_reductions
            else None
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
    for result in summary["results"]:
        if result["status"] != "ok":
            lines.append(f"{result['case']}: ERROR")
            lines.append(f"  command: {result['command']}")
            lines.append(f"  detail: {result['error']}")
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
        "# Hook Benchmark Suite",
        "",
        f"- Artifact dir: `{summary['artifact_dir']}`",
        f"- GitHub repo: `{summary['gh_repo'] or '<none>'}`",
        f"- PR seed: `{summary['gh_pr_number'] or '<none>'}`",
        f"- Run seed: `{summary['gh_run_id'] or '<none>'}`",
    ]
    if summary.get("mean_token_reduction_pct") is not None:
        lines.append(
            f"- Mean token reduction across non-empty raw outputs: `{summary['mean_token_reduction_pct']}%`"
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
            lines.append(
                f"| `{result['case']}` | error | error | n/a | `{result['error'][:120]}` |"
            )
            continue
        preview = (result["reduced_preview"].strip() or "<empty>").replace("|", "\\|")
        lines.append(
            f"| `{result['case']}` | {result['raw_est_tokens']} | {result['reduced_est_tokens']} | {result['token_reduction_pct']}% | `{preview}` |"
        )
    return "\n".join(lines) + os.linesep


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run a Packet28 hook rewrite benchmark suite and save JSON artifacts."
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
        run_case(root, artifact_dir, name, argv)
        for name, argv in default_cases(gh_repo, gh_pr_number, gh_run_id)
    ]
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
