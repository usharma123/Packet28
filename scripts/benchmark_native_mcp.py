#!/usr/bin/env python3

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from benchmark_common import estimate_tokens


def write_fixture_repo(root: Path) -> None:
    src = root / "src"
    nested = src / "nested"
    docs = root / "docs"
    nested.mkdir(parents=True, exist_ok=True)
    docs.mkdir(parents=True, exist_ok=True)

    alpha_lines = [
        "pub struct AlphaSignal {",
        "    value: i32,",
        "}",
        "",
        "impl AlphaSignal {",
        "    pub fn new(value: i32) -> Self {",
        "        Self { value }",
        "    }",
        "",
    ]
    for idx in range(1, 41):
        alpha_lines.extend(
            [
                f"    pub fn alpha_step_{idx}(&self) -> i32 {{",
                f"        self.value + {idx}",
                "    }",
                "",
            ]
        )
    alpha_lines.extend(
        [
            "    pub fn alpha_summary(&self) -> &'static str {",
            '        "AlphaSignal summary"',
            "    }",
            "}",
            "",
            "pub fn alpha_marker() -> &'static str {",
            '    "AlphaSignal marker"',
            "}",
        ]
    )
    (src / "alpha.rs").write_text("\n".join(alpha_lines) + "\n", encoding="utf-8")

    for idx in range(1, 13):
        (src / f"module_{idx:02d}.rs").write_text(
            "\n".join(
                [
                    f"pub fn module_{idx:02d}() -> &'static str {{",
                    f'    "AlphaSignal module {idx:02d}"',
                    "}",
                    "",
                    f"pub const MODULE_{idx:02d}_NAME: &str = \"AlphaSignal-{idx:02d}\";",
                ]
            )
            + "\n",
            encoding="utf-8",
        )

    (nested / "delta.rs").write_text(
        "\n".join(
            [
                "pub enum DeltaState {",
                "    Ready,",
                "    Waiting,",
                "}",
                "",
                "pub fn delta_status() -> &'static str {",
                '    "AlphaSignal nested delta"',
                "}",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    (docs / "alpha.md").write_text(
        "\n".join(
            [
                "# AlphaSignal Notes",
                "",
                "AlphaSignal appears in multiple source files for deterministic search benchmarks.",
                "Use AlphaSignal for native Packet28 benchmark coverage.",
            ]
        )
        + "\n",
        encoding="utf-8",
    )


def init_repo(root: Path) -> None:
    subprocess.run(["git", "init"], cwd=str(root), capture_output=True, check=False)


def write_mcp_message(stdin, value: dict) -> None:
    body = json.dumps(value, separators=(",", ":")).encode("utf-8")
    stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    stdin.write(body)
    stdin.flush()


def read_mcp_message(stdout) -> dict:
    headers = {}
    while True:
        line = stdout.readline()
        if not line:
            raise RuntimeError("MCP server closed stdout")
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode("utf-8").partition(":")
        headers[name.strip().lower()] = value.strip()
    content_length = int(headers["content-length"])
    payload = stdout.read(content_length)
    return json.loads(payload.decode("utf-8"))


def read_mcp_message_for_id(stdout, expected_id: int) -> dict:
    while True:
        payload = read_mcp_message(stdout)
        if payload.get("id") == expected_id:
            return payload


def initialize_mcp_session(stdin, stdout) -> None:
    write_mcp_message(
        stdin,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "benchmark-native-mcp", "version": "1"},
            },
        },
    )
    read_mcp_message_for_id(stdout, 1)


def json_preview(payload: dict) -> str:
    compact = json.dumps(payload, ensure_ascii=True, separators=(",", ":"))
    return compact[:400]


def benchmark_case(stdin, stdout, request_id: int, task_id: str, case_name: str, tool_name: str, arguments: dict) -> dict:
    try:
        slim_args = dict(arguments)
        slim_args["task_id"] = task_id
        slim_args["response_mode"] = "slim"
        write_mcp_message(
            stdin,
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": slim_args},
            },
        )
        slim_response = read_mcp_message_for_id(stdout, request_id)
        slim_payload = slim_response["result"]["structuredContent"]
        artifact_id = slim_payload.get("artifact_id")
        if not artifact_id:
            raise RuntimeError(f"{tool_name} did not return an artifact_id")

        write_mcp_message(
            stdin,
            {
                "jsonrpc": "2.0",
                "id": request_id + 100,
                "method": "tools/call",
                "params": {
                    "name": "packet28.fetch_tool_result",
                    "arguments": {"task_id": task_id, "artifact_id": artifact_id},
                },
            },
        )
        full_response = read_mcp_message_for_id(stdout, request_id + 100)
        full_payload = full_response["result"]["structuredContent"]

        raw_text = json.dumps(full_payload, ensure_ascii=True, separators=(",", ":"))
        reduced_text = json.dumps(slim_payload, ensure_ascii=True, separators=(",", ":"))
        raw_tokens = estimate_tokens(raw_text)
        reduced_tokens = estimate_tokens(reduced_text)
        reduction_pct = (
            round(100 * (raw_tokens - reduced_tokens) / raw_tokens, 1) if raw_tokens else 0.0
        )

        return {
            "case": case_name,
            "status": "ok",
            "tool_name": tool_name,
            "compact_path": "native_slim_artifact",
            "raw_output_recoverable": True,
            "artifact_fetch_succeeded": True,
            "artifact_id": artifact_id,
            "raw_bytes": len(raw_text.encode("utf-8")),
            "raw_est_tokens": raw_tokens,
            "reduced_bytes": len(reduced_text.encode("utf-8")),
            "reduced_est_tokens": reduced_tokens,
            "artifact_fetch_est_tokens": estimate_tokens(raw_text),
            "token_reduction_pct": reduction_pct,
            "raw_preview": json_preview(full_payload),
            "reduced_preview": json_preview(slim_payload),
        }
    except Exception as exc:  # pragma: no cover - benchmark harness error reporting
        return {
            "case": case_name,
            "status": "error",
            "tool_name": tool_name,
            "error": str(exc),
        }


def build_summary(results: list[dict], root: Path, artifact_dir: Path) -> dict:
    ok_results = [result for result in results if result["status"] == "ok"]
    mean = (
        round(sum(result["token_reduction_pct"] for result in ok_results) / len(ok_results), 1)
        if ok_results
        else None
    )
    return {
        "root": str(root),
        "artifact_dir": str(artifact_dir),
        "measured_at_unix": int(time.time()),
        "case_count": len(results),
        "success_count": len(ok_results),
        "error_count": len(results) - len(ok_results),
        "mean_token_reduction_pct": mean,
        "artifact_fetch_success_count": sum(
            1 for result in ok_results if result.get("artifact_fetch_succeeded")
        ),
        "compact_path_coverage_pct": (
            round(100.0 * len(ok_results) / len(results), 1) if results else None
        ),
        "results": results,
    }


def render_markdown(summary: dict) -> str:
    lines = [
        "# Native MCP Benchmark Suite",
        "",
        f"- Artifact dir: `{summary['artifact_dir']}`",
    ]
    if summary.get("mean_token_reduction_pct") is not None:
        lines.append(
            f"- Mean token reduction across native slim/full comparisons: `{summary['mean_token_reduction_pct']}%`"
        )
    lines.append(
        f"- Artifact fetch success count: `{summary['artifact_fetch_success_count']}/{summary['case_count']}`"
    )
    lines.extend(
        [
            "",
            "| Case | Tool | Raw Tokens | Reduced Tokens | Reduction | Preview |",
            "| --- | --- | ---: | ---: | ---: | --- |",
        ]
    )
    for result in summary["results"]:
        if result["status"] != "ok":
            lines.append(
                f"| `{result['case']}` | `{result.get('tool_name', '<unknown>')}` | error | error | n/a | `{result['error'][:120]}` |"
            )
            continue
        preview = result["reduced_preview"].replace("|", "\\|")
        lines.append(
            f"| `{result['case']}` | `{result['tool_name']}` | {result['raw_est_tokens']} | {result['reduced_est_tokens']} | {result['token_reduction_pct']}% | `{preview}` |"
        )
    return "\n".join(lines) + os.linesep


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run a Packet28 native MCP benchmark suite and save JSON artifacts."
    )
    parser.add_argument("--root", default=".", help="Workspace root used to launch Packet28")
    parser.add_argument("--json", action="store_true", help="Emit JSON")
    parser.add_argument(
        "--artifact-dir",
        default=None,
        help="Directory for per-case and summary JSON artifacts",
    )
    args = parser.parse_args()

    root = Path(args.root).resolve()
    artifact_dir = (
        Path(args.artifact_dir).resolve()
        if args.artifact_dir
        else root / ".packet28" / "benchmarks" / f"native-suite-{int(time.time())}"
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)
    fixture_root = artifact_dir / "native-fixture-repo"
    fixture_root.mkdir(parents=True, exist_ok=True)
    write_fixture_repo(fixture_root)
    init_repo(fixture_root)

    child = subprocess.Popen(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "suite-cli",
            "--bin",
            "Packet28",
            "--",
            "mcp",
            "serve",
            "--root",
            str(fixture_root),
        ],
        cwd=str(root),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if child.stdin is None or child.stdout is None:
        raise RuntimeError("failed to open MCP pipes")

    try:
        initialize_mcp_session(child.stdin, child.stdout)
        task_id = "task-native-benchmark"
        results = [
            benchmark_case(
                child.stdin,
                child.stdout,
                2,
                task_id,
                "native_search",
                "packet28.search",
                {"query": "AlphaSignal"},
            ),
            benchmark_case(
                child.stdin,
                child.stdout,
                3,
                task_id,
                "native_read_regions",
                "packet28.read_regions",
                {"path": "src/alpha.rs", "line_start": 1, "line_end": 60},
            ),
            benchmark_case(
                child.stdin,
                child.stdout,
                4,
                task_id,
                "native_glob",
                "packet28.glob",
                {"pattern": "src/**/*.rs"},
            ),
        ]
    finally:
        if child.stdin:
            child.stdin.close()
        if child.poll() is None:
            child.kill()
        child.wait()
        subprocess.run(
            [
                "cargo",
                "run",
                "-q",
                "-p",
                "suite-cli",
                "--bin",
                "Packet28",
                "--",
                "daemon",
                "stop",
                "--root",
                str(fixture_root),
            ],
            cwd=str(root),
            capture_output=True,
            check=False,
            text=True,
        )

    summary = build_summary(results, root, artifact_dir)
    for result in results:
        (artifact_dir / f"{result['case']}.json").write_text(
            json.dumps(result, indent=2) + os.linesep,
            encoding="utf-8",
        )
    (artifact_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + os.linesep,
        encoding="utf-8",
    )
    (artifact_dir / "summary.md").write_text(render_markdown(summary), encoding="utf-8")

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(render_markdown(summary), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
