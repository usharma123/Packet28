#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKET28_BIN="${BENCH_PACKET28_BIN:-$ROOT_DIR/target/debug/Packet28}"

if [[ "${BENCH_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -q -p suite-cli --bin Packet28 -p packet28d --bin packet28d
fi

python3 - "$ROOT_DIR" "$PACKET28_BIN" <<'PY'
import json
import os
import signal
import statistics
import subprocess
import sys
import time
from typing import Any


ROOT_DIR = os.path.abspath(sys.argv[1])
PACKET28_BIN = os.path.abspath(sys.argv[2])
OUTPUT_MODE = os.environ.get("BENCH_OUTPUT", "table").strip().lower() or "table"


CASES = [
    {
        "name": "coverage_write_state_struct",
        "root": ROOT_DIR,
        "action": "inspect",
        "query": "Where is BrokerWriteStateRequest defined?",
        "search_query": "BrokerWriteStateRequest",
        "expected_path": "crates/packet28-daemon-core/src/lib.rs",
        "expected_snippets": ["pub struct BrokerWriteStateRequest"],
        "rg_terms": ["BrokerWriteStateRequest"],
        "rg_paths": ["crates"],
    },
    {
        "name": "coverage_code_evidence_summary",
        "root": ROOT_DIR,
        "action": "inspect",
        "query": "Where is build_code_evidence_summary defined?",
        "search_query": "build_code_evidence_summary",
        "expected_path": "crates/packet28d/src/main.rs",
        "expected_snippets": ["fn build_code_evidence_summary"],
        "rg_terms": ["build_code_evidence_summary"],
        "rg_paths": ["crates/packet28d/src/main.rs"],
    },
    {
        "name": "apache_abbreviate_test",
        "root": os.path.join(ROOT_DIR, "apache"),
        "action": "inspect",
        "query": "Where is StringUtils.abbreviate tested?",
        "search_query": "StringUtils.abbreviate",
        "expected_path": "src/test/java/org/apache/commons/lang3/StringUtilsAbbreviateTest.java",
        "expected_snippets": ["class StringUtilsAbbreviateTest"],
        "rg_terms": ["StringUtils.abbreviate", "StringUtilsAbbreviateTest"],
        "rg_paths": [
            "src/test/java/org/apache/commons/lang3",
            "src/main/java/org/apache/commons/lang3",
        ],
    },
    {
        "name": "apache_abbreviate_middle_impl",
        "root": os.path.join(ROOT_DIR, "apache"),
        "action": "inspect",
        "query": "How is abbreviateMiddle implemented?",
        "search_query": "abbreviateMiddle",
        "expected_path": "src/main/java/org/apache/commons/lang3/StringUtils.java",
        "expected_snippets": ["public static String abbreviateMiddle"],
        "rg_terms": ["abbreviateMiddle"],
        "rg_paths": [
            "src/main/java/org/apache/commons/lang3",
            "src/test/java/org/apache/commons/lang3",
        ],
    },
    {
        "name": "apache_choose_tool_abbreviate_middle",
        "root": os.path.join(ROOT_DIR, "apache"),
        "action": "choose_tool",
        "query": "Need to inspect how abbreviateMiddle works before editing it",
        "search_query": "abbreviateMiddle",
        "expected_path": "src/main/java/org/apache/commons/lang3/StringUtils.java",
        "expected_snippets": ["public static String abbreviateMiddle"],
        "rg_terms": ["abbreviateMiddle"],
        "rg_paths": [
            "src/main/java/org/apache/commons/lang3",
            "src/test/java/org/apache/commons/lang3",
        ],
    },
]


def estimate_tokens_from_bytes(byte_count: int) -> int:
    return max(1, (byte_count + 3) // 4) if byte_count > 0 else 0


class McpClient:
    def __init__(self, root: str):
        self.root = root
        self.child = subprocess.Popen(
            [PACKET28_BIN, "mcp", "serve", "--root", root],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if self.child.stdin is None or self.child.stdout is None:
            raise RuntimeError("failed to start Packet28 MCP process")
        self.stdin = self.child.stdin
        self.stdout = self.child.stdout
        self._next_id = 1
        self._initialize()

    def _write(self, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8")
        message = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8") + body
        self.stdin.write(message)
        self.stdin.flush()

    def _read(self) -> dict[str, Any]:
        headers = {}
        while True:
            line = self.stdout.readline()
            if not line:
                stderr = b""
                if self.child.stderr is not None:
                    try:
                        stderr = self.child.stderr.read() or b""
                    except Exception:
                        stderr = b""
                raise RuntimeError(
                    f"Packet28 MCP process exited unexpectedly for {self.root}: {stderr.decode('utf-8', errors='replace')}"
                )
            decoded = line.decode("utf-8").strip()
            if not decoded:
                break
            key, value = decoded.split(":", 1)
            headers[key.strip().lower()] = value.strip()
        length = int(headers["content-length"])
        body = self.stdout.read(length)
        return json.loads(body.decode("utf-8"))

    def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        self._write(payload)
        while True:
            message = self._read()
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(
                        f"MCP {method} failed for {self.root}: {json.dumps(message['error'])}"
                    )
                return message["result"]

    def tool_call(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        return result["structuredContent"]

    def _initialize(self) -> None:
        self.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "bench", "version": "1"},
            },
        )

    def close(self) -> None:
        try:
            self.stdin.close()
        except Exception:
            pass
        if self.child.poll() is None:
            try:
                self.child.send_signal(signal.SIGTERM)
                self.child.wait(timeout=5)
            except Exception:
                self.child.kill()
                self.child.wait(timeout=5)
        subprocess.run(
            [PACKET28_BIN, "daemon", "stop", "--root", self.root],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


def rg_baseline(case: dict[str, Any]) -> dict[str, Any]:
    combined = []
    total_bytes = 0
    total_duration_ms = 0.0
    commands = []
    for term in case["rg_terms"]:
        cmd = ["rg", "-n", "-C", "2", "--fixed-strings", term, *case["rg_paths"]]
        started = time.perf_counter()
        run = subprocess.run(
            cmd,
            cwd=case["root"],
            capture_output=True,
            text=True,
            check=False,
        )
        duration_ms = (time.perf_counter() - started) * 1000.0
        total_duration_ms += duration_ms
        stdout = run.stdout or ""
        commands.append({"term": term, "duration_ms": round(duration_ms, 1), "exit_code": run.returncode})
        if stdout:
            total_bytes += len(stdout.encode("utf-8"))
            combined.append(f"$ {' '.join(cmd)}\n{stdout.rstrip()}")
    combined_text = "\n\n".join(combined)
    return {
        "bytes": total_bytes,
        "est_tokens": estimate_tokens_from_bytes(total_bytes),
        "duration_ms": round(total_duration_ms, 1),
        "output": combined_text,
        "commands": commands,
    }


def section_body(payload: dict[str, Any], section_id: str) -> str:
    for section in payload.get("sections", []):
        if section.get("id") == section_id:
            return section.get("body", "") or ""
    return ""


def run_case(client: McpClient, case: dict[str, Any]) -> dict[str, Any]:
    task_id = f"bench-{case['name']}-{int(time.time() * 1000)}"
    expected_path = case["expected_path"]

    search_started = time.perf_counter()
    search_payload = client.tool_call(
        "packet28.search",
        {
            "task_id": task_id,
            "query": case["search_query"],
            "paths": case.get("rg_paths", []),
            "fixed_string": True,
            "case_sensitive": True,
            "whole_word": False,
        },
    )
    search_duration_ms = round((time.perf_counter() - search_started) * 1000.0, 1)

    client.tool_call(
        "packet28.write_state",
        {
            "task_id": task_id,
            "op": "intention",
            "text": case["query"],
            "note": "Benchmark handoff packet after targeted search and inspection",
            "step_id": "investigating",
            "paths": [expected_path],
        },
    )
    client.tool_call(
        "packet28.write_state",
        {
            "task_id": task_id,
            "op": "checkpoint_save",
            "checkpoint_id": "bench-handoff",
            "note": "Benchmark checkpoint for fresh-worker bootstrap",
            "paths": [expected_path],
        },
    )
    handoff_started = time.perf_counter()
    handoff_payload = client.tool_call(
        "packet28.prepare_handoff",
        {
            "task_id": task_id,
            "query": case["query"],
            "response_mode": "full",
        },
    )
    handoff_duration_ms = round((time.perf_counter() - handoff_started) * 1000.0, 1)

    baseline = rg_baseline(case)

    search_body = search_payload.get("compact_preview", "") or ""
    search_paths = set(search_payload.get("paths", []))
    handoff_context = handoff_payload.get("context") or {}
    discovered_paths = set(handoff_context.get("discovered_paths", []))
    search_evidence = section_body(handoff_context, "search_evidence")
    code_evidence = section_body(handoff_context, "code_evidence")
    brief = handoff_context.get("brief", "") or ""
    handoff_brief = handoff_context.get("brief", "") or ""

    snippet_hits = {
        snippet: (snippet in code_evidence or snippet in brief)
        for snippet in case["expected_snippets"]
    }
    search_path_hit = expected_path in search_paths
    inspect_path_hit = (
        expected_path in discovered_paths
        or expected_path in search_evidence
        or expected_path in brief
    )
    code_snippet_hit = all(snippet_hits.values())

    broker_bytes = len(brief.encode("utf-8"))
    search_preview_bytes = len(search_body.encode("utf-8"))
    broker_est_tokens = int(
        handoff_context.get("est_tokens") or estimate_tokens_from_bytes(broker_bytes)
    )
    handoff_bytes = len(handoff_brief.encode("utf-8"))
    handoff_est_tokens = int(
        handoff_context.get("est_tokens") or estimate_tokens_from_bytes(handoff_bytes)
    )
    reduction_vs_rg = None
    search_reduction_vs_rg = None
    if baseline["bytes"] > 0:
        reduction_vs_rg = round((1.0 - (broker_bytes / baseline["bytes"])) * 100.0, 1)
        search_reduction_vs_rg = round((1.0 - (search_preview_bytes / baseline["bytes"])) * 100.0, 1)

    return {
        "name": case["name"],
        "root": case["root"],
        "action": case["action"],
        "query": case["query"],
        "expected_path": expected_path,
        "search_query": case["search_query"],
        "search_match_count": int(search_payload.get("match_count") or 0),
        "search_returned_match_count": int(search_payload.get("returned_match_count") or 0),
        "search_path_hit": search_path_hit,
        "inspect_path_hit": inspect_path_hit,
        "code_snippet_hit": code_snippet_hit,
        "effective_for_agent": bool(search_path_hit and inspect_path_hit and code_snippet_hit),
        "search_preview_bytes": search_preview_bytes,
        "search_preview_est_tokens": estimate_tokens_from_bytes(search_preview_bytes),
        "broker_brief_bytes": broker_bytes,
        "broker_est_tokens": broker_est_tokens,
        "handoff_ready": bool(handoff_payload.get("handoff_ready")),
        "handoff_brief_bytes": handoff_bytes,
        "handoff_est_tokens": handoff_est_tokens,
        "rg_bytes": baseline["bytes"],
        "rg_est_tokens": baseline["est_tokens"],
        "reduction_vs_rg_pct": reduction_vs_rg,
        "search_reduction_vs_rg_pct": search_reduction_vs_rg,
        "search_duration_ms": search_duration_ms,
        "handoff_duration_ms": handoff_duration_ms,
        "rg_duration_ms": baseline["duration_ms"],
        "search_evidence_excerpt": search_evidence[:240],
        "code_evidence_excerpt": code_evidence[:240],
        "snippet_hits": snippet_hits,
        "rg_commands": baseline["commands"],
    }


def main() -> int:
    results = []
    clients: dict[str, McpClient] = {}
    try:
        for case in CASES:
            root = case["root"]
            if root not in clients:
                clients[root] = McpClient(root)
            results.append(run_case(clients[root], case))
    finally:
        for client in clients.values():
            client.close()

    if OUTPUT_MODE == "json":
        print(json.dumps({"cases": results}, indent=2))
        return 0

    print("Packet28 Agent Search Benchmark")
    print("")
    print(
        "case | action | agent_ok | search_hit | inspect_hit | code_hit | broker_tokens | rg_tokens | brief_reduction | search_reduction"
    )
    print(
        "--- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---:"
    )
    for result in results:
        reduction = (
            f"{result['reduction_vs_rg_pct']}%"
            if result["reduction_vs_rg_pct"] is not None
            else "n/a"
        )
        search_reduction = (
            f"{result['search_reduction_vs_rg_pct']}%"
            if result["search_reduction_vs_rg_pct"] is not None
            else "n/a"
        )
        print(
            f"{result['name']} | {result['action']} | "
            f"{'yes' if result['effective_for_agent'] else 'no'} | "
            f"{'yes' if result['search_path_hit'] else 'no'} | "
            f"{'yes' if result['inspect_path_hit'] else 'no'} | "
            f"{'yes' if result['code_snippet_hit'] else 'no'} | "
            f"{result['broker_est_tokens']} | {result['rg_est_tokens']} | "
            f"{reduction} | {search_reduction}"
        )

    effective_count = sum(1 for item in results if item["effective_for_agent"])
    reduction_values = [
        item["reduction_vs_rg_pct"]
        for item in results
        if item["reduction_vs_rg_pct"] is not None
    ]
    search_reduction_values = [
        item["search_reduction_vs_rg_pct"]
        for item in results
        if item["search_reduction_vs_rg_pct"] is not None
    ]

    print("")
    print(
        f"effective cases: {effective_count}/{len(results)}"
    )
    if reduction_values:
        print(
            "median brief reduction vs rg: "
            f"{statistics.median(reduction_values):.1f}%"
        )
    if search_reduction_values:
        print(
            "median search preview reduction vs rg: "
            f"{statistics.median(search_reduction_values):.1f}%"
        )

    print("")
    for result in results:
        print(
            f"[{result['name']}] expected={result['expected_path']} "
            f"search={result['search_match_count']} "
            f"broker={result['broker_est_tokens']}t/{result['broker_brief_bytes']}b "
            f"rg={result['rg_est_tokens']}t/{result['rg_bytes']}b "
            f"durations(search={result['search_duration_ms']}ms handoff={result['handoff_duration_ms']}ms rg={result['rg_duration_ms']}ms)"
        )
        print(f"query: {result['query']}")
        print(
            f"hits: search_path={result['search_path_hit']} inspect_path={result['inspect_path_hit']} code_snippet={result['code_snippet_hit']}"
        )
        print(f"search evidence: {result['search_evidence_excerpt'] or '(none)'}")
        print(f"code evidence: {result['code_evidence_excerpt'] or '(none)'}")
        print("")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY
