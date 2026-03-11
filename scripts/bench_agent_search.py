#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any
import re


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PACKET28_BIN = REPO_ROOT / "target" / "release" / "Packet28"
PATH_CAPTURE_RE = re.compile(
    r"^- (?P<path>.+?\.[A-Za-z0-9]+?)(?::\d+|\s+\[|$)"
)


@dataclass(frozen=True)
class Case:
    case_id: str
    repo: str
    root: Path
    query: str
    needle: str
    expected_paths: tuple[str, ...]
    rg_paths: tuple[str, ...]
    literal: bool = True
    whole_word: bool = False


@dataclass
class CaseResult:
    case_id: str
    repo: str
    root: str
    query: str
    needle: str
    expected_paths: list[str]
    broker: dict[str, Any]
    rg: dict[str, Any]


CASES: tuple[Case, ...] = (
    Case(
        case_id="broker-write-state-request",
        repo="coverage",
        root=REPO_ROOT,
        query="Where is BrokerWriteStateRequest defined and used?",
        needle="BrokerWriteStateRequest",
        expected_paths=(
            "crates/packet28-daemon-core/src/lib.rs",
            "crates/packet28d/src/main.rs",
            "crates/suite-cli/src/broker_client.rs",
            "crates/suite-cli/src/cmd_mcp.rs",
        ),
        rg_paths=("crates",),
        whole_word=True,
    ),
    Case(
        case_id="repo-index-snapshot",
        repo="coverage",
        root=REPO_ROOT,
        query="Where is RepoIndexSnapshot defined and used?",
        needle="RepoIndexSnapshot",
        expected_paths=(
            "crates/mapy-core/src/lib.rs",
            "crates/packet28d/src/main.rs",
        ),
        rg_paths=("crates",),
        whole_word=True,
    ),
    Case(
        case_id="packet28-search",
        repo="coverage",
        root=REPO_ROOT,
        query="Where is packet28.search defined and used?",
        needle="packet28.search",
        expected_paths=(
            "crates/suite-cli/src/cmd_mcp.rs",
            "crates/suite-cli/tests/e2e_smoke.rs",
        ),
        rg_paths=("crates",),
    ),
    Case(
        case_id="abbreviate",
        repo="apache",
        root=REPO_ROOT / "apache",
        query="Where is abbreviate defined and used?",
        needle="abbreviate",
        expected_paths=(
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            "src/test/java/org/apache/commons/lang3/StringUtilsAbbreviateTest.java",
        ),
        rg_paths=("src/main/java", "src/test/java"),
        whole_word=True,
    ),
    Case(
        case_id="reflection-equals",
        repo="apache",
        root=REPO_ROOT / "apache",
        query="Where is reflectionEquals defined and used?",
        needle="reflectionEquals",
        expected_paths=(
            "src/main/java/org/apache/commons/lang3/builder/EqualsBuilder.java",
            "src/test/java/org/apache/commons/lang3/builder/EqualsBuilderTest.java",
        ),
        rg_paths=("src/main/java", "src/test/java"),
        whole_word=True,
    ),
    Case(
        case_id="shuffle",
        repo="apache",
        root=REPO_ROOT / "apache",
        query="Where is shuffle defined and used?",
        needle="shuffle",
        expected_paths=(
            "src/main/java/org/apache/commons/lang3/ArrayUtils.java",
            "src/test/java/org/apache/commons/lang3/ArrayUtilsTest.java",
        ),
        rg_paths=("src/main/java", "src/test/java"),
        whole_word=True,
    ),
)


class McpSession:
    def __init__(self, packet28_bin: Path, root: Path) -> None:
        self.packet28_bin = packet28_bin
        self.root = root
        self.proc: subprocess.Popen[str] | None = None

    def __enter__(self) -> "McpSession":
        self.proc = subprocess.Popen(
            [str(self.packet28_bin), "mcp", "serve", "--root", str(self.root)],
            cwd=self.root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "bench-agent-search", "version": "1"},
                },
            }
        )
        self._recv(1)
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self.proc is None:
            return
        try:
            self.proc.kill()
        except ProcessLookupError:
            pass
        self.proc.wait(timeout=5)

    def call(self, name: str, arguments: dict[str, Any], request_id: int) -> dict[str, Any]:
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        response = self._recv(request_id)
        if "error" in response:
            raise RuntimeError(f"MCP error for {name}: {response['error']}")
        return response["result"]["structuredContent"]

    def _send(self, message: dict[str, Any]) -> None:
        assert self.proc is not None and self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def _recv(self, request_id: int) -> dict[str, Any]:
        assert self.proc is not None and self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if line == "":
                stderr = ""
                if self.proc.stderr is not None:
                    stderr = self.proc.stderr.read().strip()
                raise RuntimeError(
                    f"MCP server exited before response for id={request_id}. stderr={stderr}"
                )
            payload = json.loads(line)
            if payload.get("id") == request_id:
                return payload


def run_broker_case(
    session: McpSession,
    case: Case,
    iterations: int,
) -> dict[str, Any]:
    samples: list[dict[str, Any]] = []
    for iteration in range(iterations):
        task_id = f"bench-agent-search-{case.repo}-{case.case_id}-{iteration + 1}-{int(time.time() * 1000)}"
        started = time.perf_counter()
        payload = session.call(
            "packet28.get_context",
            {
                "task_id": task_id,
                "action": "inspect",
                "query": case.query,
                "response_mode": "full",
                "include_sections": ["task_objective", "search_evidence", "code_evidence"],
                "max_sections": 3,
                "persist_artifacts": False,
            },
            request_id=iteration + 2,
        )
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        surfaced_paths = extract_paths_from_context(payload)
        score = score_paths(surfaced_paths, case.expected_paths)
        samples.append(
            {
                "elapsed_ms": elapsed_ms,
                "est_tokens": payload.get("est_tokens", 0),
                "context_version": payload.get("context_version"),
                "surfaced_paths": surfaced_paths,
                "sections": [section["id"] for section in payload.get("sections", [])],
                "score": score,
            }
        )
    return summarize_samples(samples)


def run_rg_case(case: Case, iterations: int) -> dict[str, Any]:
    samples: list[dict[str, Any]] = []
    command = [
        "rg",
        "-n",
        "--glob",
        "!target/**",
    ]
    if case.literal:
        command.append("-F")
    if case.whole_word:
        command.append("-w")
    command.extend([case.needle, *case.rg_paths])
    for _ in range(iterations):
        started = time.perf_counter()
        completed = subprocess.run(
            command,
            cwd=case.root,
            text=True,
            capture_output=True,
            check=False,
        )
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        if completed.returncode not in (0, 1):
            raise RuntimeError(
                f"rg failed for case={case.case_id} exit={completed.returncode}: {completed.stderr.strip()}"
            )
        lines = [line for line in completed.stdout.splitlines() if line.strip()]
        surfaced_paths = unique_in_order(
            line.split(":", 1)[0]
            for line in lines
            if ":" in line
        )
        score = score_paths(surfaced_paths, case.expected_paths)
        samples.append(
            {
                "elapsed_ms": elapsed_ms,
                "line_count": len(lines),
                "surfaced_paths": surfaced_paths,
                "score": score,
            }
        )
    return summarize_samples(samples)


def summarize_samples(samples: list[dict[str, Any]]) -> dict[str, Any]:
    representative = samples[0]
    return {
        "iterations": len(samples),
        "elapsed_ms": round(statistics.median(sample["elapsed_ms"] for sample in samples), 3),
        "surfaced_paths": representative["surfaced_paths"],
        "score": representative["score"],
        "samples": [
            {
                key: value
                for key, value in sample.items()
                if key not in {"surfaced_paths", "score"}
            }
            | {"score": sample["score"]}
            for sample in samples
        ],
        **(
            {"est_tokens": representative["est_tokens"], "sections": representative["sections"]}
            if "est_tokens" in representative
            else {"line_count": representative["line_count"]}
        ),
    }


def extract_paths_from_context(payload: dict[str, Any]) -> list[str]:
    sections = payload.get("sections", [])
    paths: list[str] = []
    for section in sections:
        if section.get("id") not in {"code_evidence", "search_evidence"}:
            continue
        body = section.get("body", "")
        for line in body.splitlines():
            match = PATH_CAPTURE_RE.match(line.strip())
            if match:
                paths.append(match.group("path"))
    return unique_in_order(paths)


def score_paths(surfaced_paths: list[str], expected_paths: tuple[str, ...]) -> dict[str, Any]:
    expected_set = set(expected_paths)
    hits = [path for path in surfaced_paths if path in expected_set]
    rank = None
    for idx, path in enumerate(surfaced_paths, start=1):
        if path in expected_set:
            rank = idx
            break
    precision = (len(hits) / len(surfaced_paths)) if surfaced_paths else 0.0
    recall = (len(hits) / len(expected_paths)) if expected_paths else 0.0
    return {
        "hit_count": len(hits),
        "expected_count": len(expected_paths),
        "precision": round(precision, 3),
        "recall": round(recall, 3),
        "top_hit_rank": rank,
        "hits": hits,
        "misses": [path for path in expected_paths if path not in hits],
    }


def unique_in_order(items: Any) -> list[str]:
    seen: set[str] = set()
    ordered: list[str] = []
    for item in items:
        if item not in seen:
            seen.add(item)
            ordered.append(item)
    return ordered


def render_markdown(results: list[CaseResult]) -> str:
    broker_median = statistics.median(result.broker["elapsed_ms"] for result in results)
    rg_median = statistics.median(result.rg["elapsed_ms"] for result in results)
    broker_recall = statistics.mean(result.broker["score"]["recall"] for result in results)
    rg_recall = statistics.mean(result.rg["score"]["recall"] for result in results)

    lines = [
        "# Agent Search Benchmark",
        "",
        f"- Cases: {len(results)}",
        f"- Broker median latency: {broker_median:.3f} ms",
        f"- rg median latency: {rg_median:.3f} ms",
        f"- Broker mean recall: {broker_recall:.3f}",
        f"- rg mean recall: {rg_recall:.3f}",
        "",
        "| Case | Repo | Broker recall | Broker top rank | Broker files | Broker ms | Broker tokens | rg recall | rg top rank | rg files | rg lines | rg ms |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for result in results:
        broker_score = result.broker["score"]
        rg_score = result.rg["score"]
        lines.append(
            "| {case} | {repo} | {b_recall:.3f} | {b_rank} | {b_files} | {b_ms:.3f} | {b_tokens} | {r_recall:.3f} | {r_rank} | {r_files} | {r_lines} | {r_ms:.3f} |".format(
                case=result.case_id,
                repo=result.repo,
                b_recall=broker_score["recall"],
                b_rank=broker_score["top_hit_rank"] or "-",
                b_files=len(result.broker["surfaced_paths"]),
                b_ms=result.broker["elapsed_ms"],
                b_tokens=result.broker.get("est_tokens", "-"),
                r_recall=rg_score["recall"],
                r_rank=rg_score["top_hit_rank"] or "-",
                r_files=len(result.rg["surfaced_paths"]),
                r_lines=result.rg.get("line_count", "-"),
                r_ms=result.rg["elapsed_ms"],
            )
        )
    lines.append("")
    for result in results:
        lines.extend(
            [
                f"## {result.case_id}",
                "",
                f"- Repo: `{result.repo}`",
                f"- Query: `{result.query}`",
                f"- Expected paths: {', '.join(f'`{path}`' for path in result.expected_paths)}",
                f"- Broker surfaced: {', '.join(f'`{path}`' for path in result.broker['surfaced_paths']) or '(none)'}",
                f"- rg surfaced: {', '.join(f'`{path}`' for path in result.rg['surfaced_paths']) or '(none)'}",
                "",
            ]
        )
    return "\n".join(lines)


def load_cases(selected_case_ids: list[str]) -> list[Case]:
    if not selected_case_ids:
        return list(CASES)
    by_id = {case.case_id: case for case in CASES}
    unknown = [case_id for case_id in selected_case_ids if case_id not in by_id]
    if unknown:
        raise SystemExit(f"unknown case id(s): {', '.join(unknown)}")
    return [by_id[case_id] for case_id in selected_case_ids]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark Packet28 broker steering against naive rg on real search tasks."
    )
    parser.add_argument(
        "--packet28-bin",
        default=str(DEFAULT_PACKET28_BIN),
        help="Path to the Packet28 binary (default: target/release/Packet28).",
    )
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        help="Benchmark case id to run. Repeat to run multiple cases.",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=3,
        help="Runs per case for median timing (default: 3).",
    )
    parser.add_argument(
        "--json-output",
        help="Optional path to write full JSON results.",
    )
    parser.add_argument(
        "--markdown-output",
        help="Optional path to write the markdown summary.",
    )
    parser.add_argument(
        "--list-cases",
        action="store_true",
        help="List available case ids and exit.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.list_cases:
        for case in CASES:
            print(f"{case.case_id}\t{case.repo}\t{case.query}")
        return 0

    packet28_bin = Path(args.packet28_bin).resolve()
    if not packet28_bin.exists():
        raise SystemExit(f"Packet28 binary not found: {packet28_bin}")

    cases = load_cases(args.case)
    grouped_cases: dict[Path, list[Case]] = {}
    for case in cases:
        grouped_cases.setdefault(case.root, []).append(case)

    results: list[CaseResult] = []
    for root, repo_cases in grouped_cases.items():
        with McpSession(packet28_bin, root) as session:
            for case in repo_cases:
                broker = run_broker_case(session, case, args.iterations)
                rg = run_rg_case(case, args.iterations)
                results.append(
                    CaseResult(
                        case_id=case.case_id,
                        repo=case.repo,
                        root=str(case.root),
                        query=case.query,
                        needle=case.needle,
                        expected_paths=list(case.expected_paths),
                        broker=broker,
                        rg=rg,
                    )
                )

    payload = {
        "generated_at_unix": int(time.time()),
        "packet28_bin": str(packet28_bin),
        "iterations": args.iterations,
        "cases": [asdict(result) for result in results],
    }
    markdown = render_markdown(results)

    if args.json_output:
        Path(args.json_output).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    if args.markdown_output:
        Path(args.markdown_output).write_text(markdown + "\n", encoding="utf-8")

    print(markdown)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
