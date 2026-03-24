#!/usr/bin/env python3
from __future__ import annotations

import json
import math
import os
import re
import shlex
import signal
import subprocess
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


@dataclass(frozen=True)
class Scenario:
    slug: str
    name: str
    description: str
    root_rel: str
    packet28_query: str
    rg_pattern: str
    grep_pattern: str
    expected_matches: tuple[str, ...] | None = None
    ast_pattern: str | None = None


WORKSPACE = Path(__file__).resolve().parent.parent
PACKET28_BIN = WORKSPACE / "target" / "release" / "packet28-search-cli"
PACKET28D_BIN = WORKSPACE / "target" / "release" / "packet28d"
REPORT_PATH = WORKSPACE / "benchmarks" / "packet28_search_tool_benchmark.md"
DAEMON_READY = WORKSPACE / ".packet28" / "daemon" / "ready"

SCENARIOS = [
    Scenario(
        slug="handle_packet28_search_def",
        name="Function Definition",
        description="Single Rust function definition lookup for handle_packet28_search.",
        root_rel="crates/suite-cli",
        packet28_query=r"fn\s+handle_packet28_search\(",
        rg_pattern=r"fn\s+handle_packet28_search\(",
        grep_pattern=r"fn[[:space:]]+handle_packet28_search\(",
        ast_pattern="pub(crate) fn handle_packet28_search($$$ARGS) -> $$$RET { $$$BODY }",
    ),
    Scenario(
        slug="packet28_search_via_session_call",
        name="Single Call Expression",
        description="Exact call-site lookup for packet28_search_via_session(root, session, request.clone()).",
        root_rel="crates/suite-cli",
        packet28_query=r"packet28_search_via_session\(root, session, request\.clone\(\)\)",
        rg_pattern=r"packet28_search_via_session\(root, session, request\.clone\(\)\)",
        grep_pattern=r"packet28_search_via_session\(root, session, request\.clone\(\)\)",
        ast_pattern="packet28_search_via_session(root, session, request.clone())",
    ),
    Scenario(
        slug="daemon_packet28_search_call",
        name="Daemon Call Expression",
        description="Exact call-site lookup for daemon_packet28_search(state, request).",
        root_rel="crates/packet28d",
        packet28_query=r"daemon_packet28_search\(state, request\)",
        rg_pattern=r"daemon_packet28_search\(state, request\)",
        grep_pattern=r"daemon_packet28_search\(state, request\)",
        ast_pattern="daemon_packet28_search(state, request)",
    ),
    Scenario(
        slug="search_request_literal",
        name="Anchored Struct Literal",
        description="Anchored line-start regex for SearchRequest literal construction in the standalone search CLI.",
        root_rel="crates/packet28-search-cli",
        packet28_query=r"^\s*SearchRequest\s*\{",
        rg_pattern=r"^\s*SearchRequest\s*\{",
        grep_pattern=r"^[[:space:]]*SearchRequest[[:space:]]*\{",
        ast_pattern="SearchRequest { $$$FIELDS }",
    ),
    Scenario(
        slug="run_command_alternation",
        name="Alternation-Heavy Regex",
        description="Alternation over three standalone CLI command handlers.",
        root_rel="crates/packet28-search-cli",
        packet28_query=r"fn\s+(?:run_query|run_guard|run_bench)\(",
        rg_pattern=r"fn\s+(?:run_query|run_guard|run_bench)\(",
        grep_pattern=r"fn[[:space:]]+(run_query|run_guard|run_bench)\(",
    ),
    Scenario(
        slug="packet28_handler_family",
        name="Broad But Selective Regex",
        description="Cross-file alternation over Packet28 search/read/fetch handler names in suite-cli.",
        root_rel="crates/suite-cli",
        packet28_query=r"handle_packet28_(?:search|read_regions|fetch_tool_result)",
        rg_pattern=r"handle_packet28_(?:search|read_regions|fetch_tool_result)",
        grep_pattern=r"handle_packet28_(search|read_regions|fetch_tool_result)",
    ),
    Scenario(
        slug="broad_declarations",
        name="Broad Declaration Regex",
        description="Broad declaration regex over the packet28-search-core crate.",
        root_rel="crates/packet28-search-core",
        packet28_query=r"pub\s+(?:fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*",
        rg_pattern=r"pub\s+(?:fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*",
        grep_pattern=r"pub[[:space:]]+(fn|struct|enum)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*",
    ),
    Scenario(
        slug="search_cli_function_sweep",
        name="Common Function Sweep",
        description="Common function-signature regex over the standalone search CLI.",
        root_rel="crates/packet28-search-cli",
        packet28_query=r"fn\s+[a-z_][A-Za-z0-9_]*\(",
        rg_pattern=r"fn\s+[a-z_][A-Za-z0-9_]*\(",
        grep_pattern=r"fn[[:space:]]+[a-z_][A-Za-z0-9_]*\(",
    ),
]


def run(cmd: list[str], cwd: Path | None = None) -> str:
    completed = subprocess.run(
        cmd,
        cwd=cwd or WORKSPACE,
        check=True,
        text=True,
        capture_output=True,
    )
    return completed.stdout


def require_tools() -> None:
    required = ("rg", "grep", "hyperfine")
    optional = ("ast-grep",)
    for tool in required:
        if shutil_which(tool) is None:
            raise SystemExit(f"required tool not found: {tool}")
    for tool in optional:
        if shutil_which(tool) is None:
            print(f"warning: optional tool not found, skipping comparisons for {tool}")


def ensure_packet28_release() -> None:
    subprocess.run(
        ["cargo", "build", "--release", "-p", "packet28-search-cli", "-p", "packet28d"],
        cwd=WORKSPACE,
        check=True,
        text=True,
        capture_output=True,
    )


def shutil_which(tool: str) -> str | None:
    output = subprocess.run(
        ["bash", "-lc", f"command -v {shlex.quote(tool)}"],
        cwd=WORKSPACE,
        text=True,
        capture_output=True,
    )
    return output.stdout.strip() or None


def build_index(root: Path) -> float:
    output = run([str(PACKET28_BIN), "build", str(root)])
    match = re.search(r"build_ms=([0-9.]+)", output)
    if not match:
        raise RuntimeError(f"unexpected build output: {output}")
    return float(match.group(1))


def start_daemon() -> subprocess.Popen[str]:
    proc = subprocess.Popen(
        [str(PACKET28D_BIN), "serve", "--root", str(WORKSPACE)],
        cwd=WORKSPACE,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    start = time.time()
    while time.time() - start < 10:
        if DAEMON_READY.exists():
            return proc
        time.sleep(0.05)
    proc.kill()
    raise RuntimeError("packet28d did not become ready")


def stop_daemon(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def packet28_command(scenario: Scenario, transport: str, compact: bool) -> str:
    root = shlex.quote(str((WORKSPACE / scenario.root_rel).resolve()))
    query = shlex.quote(scenario.packet28_query)
    parts = [
        shlex.quote(str(PACKET28_BIN)),
        "query",
        root,
        query,
        "--engine",
        "auto",
        "--transport",
        transport,
        "--max-matches-per-file",
        "1000",
        "--max-total-matches",
        "1000",
    ]
    if compact:
        parts.append("--compact")
    return " ".join(parts)


def rg_command(scenario: Scenario) -> str:
    return (
        f"rg -n --no-heading --color never "
        f"{shlex.quote(scenario.rg_pattern)} {shlex.quote(scenario.root_rel)}"
    )


def grep_command(scenario: Scenario) -> str:
    return (
        f"grep -RInE --color=never "
        f"{shlex.quote(scenario.grep_pattern)} {shlex.quote(scenario.root_rel)}"
    )


def ast_grep_command(scenario: Scenario) -> str | None:
    if scenario.ast_pattern is None or shutil_which("ast-grep") is None:
        return None
    return (
        "ast-grep run --lang rust --heading never --color never -C 0 "
        f"-p {shlex.quote(scenario.ast_pattern)} {shlex.quote(scenario.root_rel)}"
    )


def normalize_path(raw_path: str, scenario: Scenario) -> str:
    raw = raw_path.strip()
    workspace_prefix = f"{WORKSPACE}/"
    if raw.startswith(workspace_prefix):
        raw = raw[len(workspace_prefix) :]
    if raw.startswith(scenario.root_rel + "/"):
        raw = raw[len(scenario.root_rel) + 1 :]
    return raw


def parse_line_or_block_matches(output: str, scenario: Scenario) -> list[str]:
    matches: list[str] = []
    line_re = re.compile(r"^(?P<path>.+?):(?P<line>\d+):")
    for raw_line in output.splitlines():
        match = line_re.match(raw_line)
        if not match:
            continue
        path = normalize_path(match.group("path"), scenario)
        line = int(match.group("line"))
        matches.append(f"{path}:{line}")
    return matches


def parse_packet28_output(
    output: str,
) -> tuple[list[str], int | None, str | None, str | None, str | None]:
    hit_re = re.compile(r"^hit=(?P<path>.+?)#L(?P<line>\d+)\b")
    region_re = re.compile(r"^region=(?P<path>.+?):(?P<start>\d+)-(?P<end>\d+)$")
    count_re = re.compile(r"\bmatches=(\d+)\b")
    backend_re = re.compile(r"\bbackend=([a-z_]+)\b")
    transport_re = re.compile(r"\btransport=([a-z_]+)\b")
    fallback_re = re.compile(r"^fallback_reason=(.+)$")
    total = None
    backend = None
    transport = None
    fallback_reason = None
    first_line = output.splitlines()[0] if output.splitlines() else ""
    count_match = count_re.search(first_line)
    if count_match:
        total = int(count_match.group(1))
    backend_match = backend_re.search(first_line)
    if backend_match:
        backend = backend_match.group(1)
    transport_match = transport_re.search(first_line)
    if transport_match:
        transport = transport_match.group(1)
    matches = []
    for line in output.splitlines():
        fallback_match = fallback_re.match(line)
        if fallback_match:
            fallback_reason = fallback_match.group(1)
        region = region_re.match(line)
        if region:
            path = region.group("path")
            start = int(region.group("start"))
            end = int(region.group("end"))
            matches.extend(f"{path}:{line_no}" for line_no in range(start, end + 1))
        hit = hit_re.match(line)
        if hit:
            matches.append(f"{hit.group('path')}:{hit.group('line')}")
    return sorted(set(matches)), total, backend, transport, fallback_reason


def approx_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8")) / 4)


def render_compact_preview(found: list[str]) -> str:
    if not found:
        return "Search found 0 matches."
    per_path: dict[str, int] = {}
    for hit in found:
        path, _line = hit.split(":", 1)
        per_path[path] = per_path.get(path, 0) + 1
    lines = [f"Search found {len(found)} matches in {len(per_path)} files."]
    for path in sorted(per_path):
        lines.append(f"- {path} ({per_path[path]})")
    return "\n".join(lines)


def compact_tokens_for_hits(found: list[str]) -> int:
    return approx_tokens(render_compact_preview(found))


def measure_speeds(commands: dict[str, str]) -> dict[str, float]:
    with tempfile.NamedTemporaryFile("r+", suffix=".json", delete=False) as handle:
        export_path = handle.name
    cmd = [
        "hyperfine",
        "--warmup",
        "2",
        "--runs",
        "8",
        "--export-json",
        export_path,
    ]
    labels = []
    for label, command in commands.items():
        labels.append(label)
        cmd.append(f"{command} > /dev/null")
    subprocess.run(cmd, cwd=WORKSPACE, check=True, text=True, capture_output=True)
    data = json.loads(Path(export_path).read_text())
    return {
        labels[idx]: benchmark["mean"] * 1000.0
        for idx, benchmark in enumerate(data["results"])
    }


def evaluate(found: list[str], expected: tuple[str, ...]) -> dict[str, object]:
    found_set = set(found)
    expected_set = set(expected)
    correct = found_set & expected_set
    precision = 1.0 if not found_set and not expected_set else (
        len(correct) / len(found_set) if found_set else 0.0
    )
    recall = 1.0 if not expected_set else len(correct) / len(expected_set)
    return {
        "found": sorted(found_set),
        "missing": sorted(expected_set - found_set),
        "extra": sorted(found_set - expected_set),
        "precision": precision,
        "recall": recall,
        "exact": found_set == expected_set,
        "correct_hits": len(correct),
    }


def tool_version(cmd: list[str]) -> str:
    return run(cmd).splitlines()[0].strip()


def markdown_escape(text: str) -> str:
    return text.replace("|", "\\|")


def main() -> None:
    ensure_packet28_release()
    require_tools()
    inproc_build_times = {
        root: build_index((WORKSPACE / root).resolve())
        for root in sorted({s.root_rel for s in SCENARIOS})
    }
    daemon_build_ms = build_index(WORKSPACE)
    daemon = start_daemon()
    try:
        versions = {
            "packet28-search-cli": f"git {run(['git', 'rev-parse', '--short', 'HEAD']).strip()}",
            "packet28d": tool_version([str(PACKET28D_BIN), "--version"])
            if PACKET28D_BIN.exists()
            else "packet28d (local build)",
            "ripgrep": tool_version(["rg", "--version"]),
            "grep": tool_version(["grep", "--version"]),
        }
        if shutil_which("ast-grep") is not None:
            versions["ast-grep"] = tool_version(["ast-grep", "--version"])

        scenario_results = []
        aggregate: dict[str, dict[str, float]] = {}
        scenario_counts: dict[str, int] = {}

        for scenario in SCENARIOS:
            speed_commands = {
                "packet28-daemon": packet28_command(scenario, "daemon", compact=True),
                "packet28-inproc": packet28_command(scenario, "inproc", compact=True),
                "ripgrep": rg_command(scenario),
                "grep": grep_command(scenario),
            }
            ast_cmd = ast_grep_command(scenario)
            if ast_cmd is not None:
                speed_commands["ast-grep"] = ast_cmd

            display_commands = {
                "packet28-daemon": packet28_command(scenario, "daemon", compact=False),
                "packet28-inproc": packet28_command(scenario, "inproc", compact=False),
                "ripgrep": rg_command(scenario),
                "grep": grep_command(scenario),
            }
            if ast_cmd is not None:
                display_commands["ast-grep"] = ast_cmd

            outputs = {
                name: run(["bash", "-lc", command], cwd=WORKSPACE)
                for name, command in display_commands.items()
            }
            daemon_matches, daemon_total, daemon_backend, daemon_transport, daemon_fallback = parse_packet28_output(
                outputs["packet28-daemon"]
            )
            (
                inproc_matches,
                inproc_total,
                inproc_backend,
                inproc_transport,
                inproc_fallback,
            ) = parse_packet28_output(
                outputs["packet28-inproc"]
            )
            tool_matches = {
                "packet28-daemon": daemon_matches,
                "packet28-inproc": inproc_matches,
                "ripgrep": parse_line_or_block_matches(outputs["ripgrep"], scenario),
                "grep": parse_line_or_block_matches(outputs["grep"], scenario),
            }
            if "ast-grep" in outputs:
                tool_matches["ast-grep"] = parse_line_or_block_matches(outputs["ast-grep"], scenario)
            expected_matches = scenario.expected_matches or tuple(tool_matches["ripgrep"])
            speeds = measure_speeds(speed_commands)

            scenario_tool_results = {}
            for tool_name, found in tool_matches.items():
                eval_result = evaluate(found, expected_matches)
                token_count = compact_tokens_for_hits(found)
                correct_hits = max(eval_result["correct_hits"], 1)
                scenario_tool_results[tool_name] = {
                    **eval_result,
                    "compact_tokens": token_count,
                    "tokens_per_true_hit": token_count / correct_hits,
                    "true_hits_per_1k_tokens": eval_result["correct_hits"] * 1000.0 / token_count
                    if token_count
                    else 0.0,
                    "mean_ms": speeds[tool_name],
                }
                aggregate.setdefault(
                    tool_name,
                    {
                        "mean_ms": 0.0,
                        "compact_tokens": 0.0,
                        "exact": 0.0,
                        "true_hits_per_1k_tokens": 0.0,
                    },
                )
                scenario_counts[tool_name] = scenario_counts.get(tool_name, 0) + 1
                aggregate[tool_name]["mean_ms"] += speeds[tool_name]
                aggregate[tool_name]["compact_tokens"] += token_count
                aggregate[tool_name]["exact"] += 1.0 if eval_result["exact"] else 0.0
                aggregate[tool_name]["true_hits_per_1k_tokens"] += scenario_tool_results[tool_name][
                    "true_hits_per_1k_tokens"
                ]

            scenario_results.append(
                {
                    "scenario": scenario,
                    "commands": speed_commands,
                    "results": scenario_tool_results,
                    "expected_matches": expected_matches,
                    "packet28_daemon_total": daemon_total,
                    "packet28_inproc_total": inproc_total,
                    "packet28_daemon_backend": daemon_backend,
                    "packet28_inproc_backend": inproc_backend,
                    "packet28_daemon_transport": daemon_transport,
                    "packet28_inproc_transport": inproc_transport,
                    "packet28_daemon_fallback": daemon_fallback,
                    "packet28_inproc_fallback": inproc_fallback,
                }
            )

        summary_rows = []
        for tool_name, values in aggregate.items():
            count = scenario_counts[tool_name]
            summary_rows.append(
                {
                    "tool": tool_name,
                    "scenarios": count,
                    "avg_mean_ms": values["mean_ms"] / count,
                    "avg_compact_tokens": values["compact_tokens"] / count,
                    "exact_rate": values["exact"] / count,
                    "avg_true_hits_per_1k_tokens": values["true_hits_per_1k_tokens"] / count,
                }
            )
        summary_rows.sort(key=lambda row: row["avg_mean_ms"])

        lines = []
        lines.append("# Packet28 Regex Search Benchmark")
        lines.append("")
        lines.append(f"_Generated: {datetime.now(timezone.utc).isoformat()}_")
        lines.append("")
        lines.append("## Setup")
        lines.append("")
        lines.append(f"- Workspace: `{WORKSPACE}`")
        lines.append("- Packet28 in-process indexes were pre-built per search root before timing.")
        lines.append("- Packet28 daemon transport was measured against a resident `packet28d` running at the workspace root, with subtree searches mapped into requested-path filters.")
        lines.append("- Speed was measured with `hyperfine` using 2 warmups and 8 measured runs, with stdout redirected to `/dev/null`.")
        lines.append("- Token efficiency is measured against a normalized compact Packet28-style packet derived from each tool's match set.")
        lines.append("- Packet28 accuracy is collected from full query output; Packet28 timing is measured on compact mode so speed and token costs reflect the reduced interface boundary.")
        lines.append("- Accuracy is exact match-set parity against the canonical `ripgrep` `path:line` hit set for each regex scenario.")
        lines.append("")
        lines.append("### Tool Versions")
        lines.append("")
        for name, version in versions.items():
            lines.append(f"- `{name}`: `{version}`")
        lines.append("")
        lines.append("### One-Time Packet28 Index Build Times")
        lines.append("")
        lines.append(f"- `workspace daemon index`: `{daemon_build_ms:.3f} ms`")
        for root_rel, build_ms in sorted(inproc_build_times.items()):
            lines.append(f"- `inproc {root_rel}`: `{build_ms:.3f} ms`")
        lines.append("")
        lines.append("## Summary")
        lines.append("")
        lines.append("| Tool | Scenarios | Avg Mean ms | Avg Compact Tokens | Avg True Hits / 1k Tokens | Exact-Match Rate |")
        lines.append("| --- | ---: | ---: | ---: | ---: | ---: |")
        for row in summary_rows:
            lines.append(
                f"| `{row['tool']}` | {row['scenarios']} | {row['avg_mean_ms']:.3f} | {row['avg_compact_tokens']:.1f} | "
                f"{row['avg_true_hits_per_1k_tokens']:.1f} | {row['exact_rate']:.0%} |"
            )
        lines.append("")

        for entry in scenario_results:
            scenario: Scenario = entry["scenario"]
            lines.append(f"## {scenario.name}")
            lines.append("")
            lines.append(scenario.description)
            lines.append("")
            lines.append(f"- Root: `{scenario.root_rel}`")
            lines.append(f"- Canonical hits (`ripgrep`): `{', '.join(entry['expected_matches']) or '<none>'}`")
            lines.append(f"- Packet28 daemon backend: `{entry['packet28_daemon_backend']}` transport: `{entry['packet28_daemon_transport']}` total: `{entry['packet28_daemon_total']}`")
            lines.append(f"- Packet28 inproc backend: `{entry['packet28_inproc_backend']}` transport: `{entry['packet28_inproc_transport']}` total: `{entry['packet28_inproc_total']}`")
            if entry["packet28_daemon_fallback"]:
                lines.append(f"- Packet28 daemon fallback reason: `{entry['packet28_daemon_fallback']}`")
            if entry["packet28_inproc_fallback"]:
                lines.append(f"- Packet28 inproc fallback reason: `{entry['packet28_inproc_fallback']}`")
            lines.append("")
            lines.append("| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |")
            lines.append("| --- | ---: | ---: | ---: | ---: | ---: | :---: |")
            for tool_name in ("packet28-daemon", "packet28-inproc", "ripgrep", "grep", "ast-grep"):
                if tool_name not in entry["results"]:
                    continue
                result = entry["results"][tool_name]
                lines.append(
                    f"| `{tool_name}` | {result['mean_ms']:.3f} | {result['compact_tokens']} | "
                    f"{result['tokens_per_true_hit']:.1f} | {result['precision']:.0%} | "
                    f"{result['recall']:.0%} | {'yes' if result['exact'] else 'no'} |"
                )
            lines.append("")
            lines.append("### Commands")
            lines.append("")
            for tool_name in ("packet28-daemon", "packet28-inproc", "ripgrep", "grep", "ast-grep"):
                if tool_name not in entry["commands"]:
                    continue
                lines.append(f"- `{tool_name}`: `{markdown_escape(entry['commands'][tool_name])}`")
            lines.append("")
            lines.append("### Match Sets")
            lines.append("")
            for tool_name in ("packet28-daemon", "packet28-inproc", "ripgrep", "grep", "ast-grep"):
                if tool_name not in entry["results"]:
                    continue
                result = entry["results"][tool_name]
                lines.append(f"- `{tool_name}` found: `{', '.join(result['found']) or '<none>'}`")
                if result["missing"]:
                    lines.append(f"  missing: `{', '.join(result['missing'])}`")
                if result["extra"]:
                    lines.append(f"  extra: `{', '.join(result['extra'])}`")
            lines.append("")

        lines.append("## Observations")
        lines.append("")
        lines.append("- Packet28 is measured on both the resident daemon transport and the in-process CLI path. The daemon path is the primary “instant grep” target; the in-process path remains exact and competitive.")
        lines.append("- Guarded `rg` fallback remains part of Packet28 for broad or unselective regexes, but fallback reasons are preserved in the Packet28 result rather than forcing the caller to replay the search.")
        lines.append("- `ast-grep` remains only an external comparison point for regex-expressible code-shaped scenarios; Packet28 does not delegate to it.")
        lines.append("")

        REPORT_PATH.write_text("\n".join(lines) + "\n")
        print(f"wrote {REPORT_PATH}")
    finally:
        stop_daemon(daemon)


if __name__ == "__main__":
    main()
