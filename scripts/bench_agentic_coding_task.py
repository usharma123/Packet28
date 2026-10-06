#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from benchmark_common import estimate_tokens


TASK_ID = "agentic-coding-tokenomics-20260518"
FIX_PATH = Path("src/auth/user_id.rs")


def write_fixture(root: Path) -> None:
    (root / "src" / "auth").mkdir(parents=True, exist_ok=True)
    (root / "tests").mkdir(parents=True, exist_ok=True)
    (root / "docs").mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text(
        "\n".join(
            [
                "[package]",
                'name = "agentic-tokenomics-fixture"',
                'version = "0.1.0"',
                'edition = "2021"',
                "",
                "[lib]",
                'path = "src/lib.rs"',
                "",
                "[workspace]",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (root / "src" / "lib.rs").write_text(
        "\n".join(
            [
                "pub mod auth;",
                "",
                "#[cfg(test)]",
                "mod tests {",
                "    use crate::auth::{normalize_user_id, resolve_login_alias};",
                "",
                "    #[test]",
                "    fn normalize_user_id_trims_alias_input() {",
                '        assert_eq!(normalize_user_id(" Alice.Example "), "alice.example");',
                "    }",
                "",
                "    #[test]",
                "    fn resolve_login_alias_uses_normalized_id() {",
                '        assert_eq!(resolve_login_alias(" Bob "), "user:bob");',
                "    }",
                "}",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (root / "src" / "auth" / "mod.rs").write_text(
        "\n".join(
            [
                "mod user_id;",
                "",
                "pub use user_id::{normalize_user_id, resolve_login_alias, UserId};",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    filler = []
    for idx in range(1, 51):
        filler.extend(
            [
                f"pub fn login_alias_fixture_hint_{idx}(value: &str) -> bool {{",
                f'    value.contains("alias-{idx}") || value.contains("UserId")',
                "}",
                "",
            ]
        )
    (root / FIX_PATH).write_text(
        "\n".join(
            [
                "#[derive(Debug, Clone, PartialEq, Eq)]",
                "pub struct UserId(String);",
                "",
                "impl UserId {",
                "    pub fn new(raw: &str) -> Self {",
                "        Self(normalize_user_id(raw))",
                "    }",
                "",
                "    pub fn as_str(&self) -> &str {",
                "        &self.0",
                "    }",
                "}",
                "",
                "pub fn normalize_user_id(input: &str) -> String {",
                "    input.to_ascii_lowercase()",
                "}",
                "",
                "pub fn resolve_login_alias(input: &str) -> String {",
                '    format!("user:{}", normalize_user_id(input))',
                "}",
                "",
                "// Agent task fixture: the bug is in normalize_user_id.",
                "// A typical coding agent should search for the symbol, inspect this file,",
                "// run the focused tests, patch the normalization, and rerun the tests.",
                "",
                *filler,
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (root / "tests" / "integration.rs").write_text(
        "\n".join(
            [
                "use agentic_tokenomics_fixture::auth::resolve_login_alias;",
                "",
                "#[test]",
                "fn integration_login_alias_is_normalized() {",
                '    assert_eq!(resolve_login_alias(" Carol "), "user:carol");',
                "}",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    (root / "docs" / "login_alias.md").write_text(
        "\n".join(
            [
                "# Login Alias Notes",
                "",
                "The login alias flow stores every UserId in lowercase and without surrounding whitespace.",
                "Search terms for this fixture include normalize_user_id, resolve_login_alias, UserId, and login alias.",
            ]
        )
        + "\n",
        encoding="utf-8",
    )


def apply_fix(root: Path) -> None:
    path = root / FIX_PATH
    text = path.read_text(encoding="utf-8")
    path.write_text(
        text.replace("    input.to_ascii_lowercase()", "    input.trim().to_ascii_lowercase()"),
        encoding="utf-8",
    )


def init_repo(root: Path) -> None:
    subprocess.run(["git", "init"], cwd=root, capture_output=True, text=True, check=False)
    subprocess.run(["git", "add", "."], cwd=root, capture_output=True, text=True, check=False)
    subprocess.run(
        ["git", "commit", "-m", "fixture"],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )


def fixture_environment(root: Path) -> dict[str, str]:
    environment = os.environ.copy()
    # The two traces share a package name, so a global Cargo target can reuse
    # the fixed baseline artifact for the still-broken Packet28 workspace.
    environment["CARGO_TARGET_DIR"] = str(root / "target")
    return environment


def run_command(root: Path, command: list[str], *, shell: bool = False) -> dict[str, Any]:
    started = time.perf_counter()
    completed = subprocess.run(
        command if not shell else command[0],
        cwd=root,
        env=fixture_environment(root),
        text=True,
        capture_output=True,
        shell=shell,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    visible = completed.stdout + completed.stderr
    return {
        "command": command[0] if shell else " ".join(command),
        "exit_code": completed.returncode,
        "elapsed_ms": round(elapsed_ms, 3),
        "bytes": len(visible.encode("utf-8")),
        "tokens": estimate_tokens(visible),
        "preview": visible[:400],
    }


class McpSession:
    def __init__(self, packet28_bin: Path, root: Path) -> None:
        self.packet28_bin = packet28_bin
        self.root = root
        self.proc: subprocess.Popen[str] | None = None
        self.next_id = 1

    def __enter__(self) -> "McpSession":
        self.proc = subprocess.Popen(
            [str(self.packet28_bin), "mcp", "serve", "--root", str(self.root)],
            cwd=self.root,
            env=fixture_environment(self.root),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.request(
            "initialize",
            {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "agentic-coding-tokenomics", "version": "1"},
            },
        )
        self.notify("notifications/initialized", {})
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self.proc is None:
            return
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait(timeout=5)

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return self._recv(request_id)

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        if "error" in response:
            raise RuntimeError(f"{name} failed: {response['error']}")
        return response["result"]["structuredContent"]

    def _send(self, message: dict[str, Any]) -> None:
        assert self.proc is not None and self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def _recv(self, request_id: int) -> dict[str, Any]:
        assert self.proc is not None and self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                stderr = self.proc.stderr.read() if self.proc and self.proc.stderr else ""
                raise RuntimeError(f"MCP server exited before id={request_id}: {stderr}")
            payload = json.loads(line)
            if payload.get("id") == request_id:
                return payload


def payload_tokens(payload: dict[str, Any]) -> int:
    return estimate_tokens(json.dumps(payload, separators=(",", ":"), ensure_ascii=True))


def record_mcp_step(
    session: McpSession,
    name: str,
    tool: str,
    arguments: dict[str, Any],
    *,
    fetch_artifact: bool = True,
) -> dict[str, Any]:
    started = time.perf_counter()
    payload = session.call_tool(tool, arguments)
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    artifact_id = payload.get("artifact_id")
    fetched = None
    if fetch_artifact and artifact_id:
        fetched = session.call_tool(
            "packet28.fetch_tool_result",
            {"task_id": arguments.get("task_id", TASK_ID), "artifact_id": artifact_id},
        )
    return {
        "name": name,
        "tool": tool,
        "status": "ok",
        "elapsed_ms": round(elapsed_ms, 3),
        "tokens": payload_tokens(payload),
        "bytes": len(json.dumps(payload, separators=(",", ":")).encode("utf-8")),
        "artifact_id": artifact_id,
        "artifact_fetch_succeeded": fetched is not None if artifact_id else None,
        "artifact_fetch_tokens": payload_tokens(fetched) if fetched is not None else 0,
        "payload": payload,
    }


def run_explicit_packet28_cargo(root: Path, packet28_bin: Path) -> dict[str, Any]:
    command = "cargo test --lib"
    pretool_payload = json.dumps(
        {
            "hook_event_name": "PreToolUse",
            "task_id": TASK_ID,
            "session_id": f"{TASK_ID}-session",
            "cwd": str(root),
            "tool_name": "Bash",
            "tool_input": {"command": command},
        }
    )
    hook = subprocess.run(
        [str(packet28_bin), "hook", "claude", "--root", str(root)],
        cwd=root,
        env=fixture_environment(root),
        input=pretool_payload,
        text=True,
        capture_output=True,
        check=False,
    )
    payload = json.loads(hook.stdout.strip() or "{}")
    specific = payload.get("hookSpecificOutput", {})
    capture_only = hook.returncode == 0 and all(
        key not in payload and key not in specific
        for key in ("updatedInput", "permissionDecision", "decision")
    )
    raw = run_command(root, ["cargo", "test", "--lib"])
    reduced = run_command(root, [str(packet28_bin), "cargo", "test", "--lib"])
    reduction = (
        round(100.0 * (raw["tokens"] - reduced["tokens"]) / raw["tokens"], 1)
        if raw["tokens"]
        else 0.0
    )
    return {
        "name": command,
        "tool": "packet28.explicit_cli",
        "status": "ok" if capture_only and raw["exit_code"] == reduced["exit_code"] else "failed",
        "command": command,
        "explicit_command": f"{packet28_bin} cargo test --lib",
        "pretool_capture_only": capture_only,
        "tokens": reduced["tokens"],
        "raw_tokens": raw["tokens"],
        "reduced_tokens": reduced["tokens"],
        "reduction_pct": reduction,
        "raw_output_recoverable": False,
        "exit_code": reduced["exit_code"],
        "raw_exit_code": raw["exit_code"],
        "preview": reduced["preview"],
    }


def run_normal_trace(root: Path) -> list[dict[str, Any]]:
    steps = [
        run_command(root, ["find", "src", "tests", "docs", "-type", "f"]),
        run_command(
            root,
            [
                "rg",
                "-n",
                "normalize_user_id|resolve_login_alias|login alias|UserId",
                "src",
                "tests",
                "docs",
            ],
        ),
        run_command(root, ["sed", "-n", "1,140p", str(FIX_PATH)]),
        run_command(root, ["cargo", "test", "--lib"]),
    ]
    apply_fix(root)
    steps.append(run_command(root, ["cargo", "test", "--lib"]))
    return steps


def run_packet28_trace(root: Path, packet28_bin: Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    steps: list[dict[str, Any]] = []
    with McpSession(packet28_bin, root) as session:
        steps.append(
            record_mcp_step(
                session,
                "glob source files",
                "packet28.glob",
                {
                    "task_id": TASK_ID,
                    "pattern": "src/**/*.rs",
                    "response_mode": "slim",
                },
            )
        )
        steps.append(
            record_mcp_step(
                session,
                "search symbol",
                "packet28.search",
                {
                    "task_id": TASK_ID,
                    "query": "normalize_user_id",
                    "paths": ["src", "tests", "docs"],
                    "fixed_string": True,
                    "response_mode": "slim",
                },
            )
        )
        steps.append(
            record_mcp_step(
                session,
                "read target file",
                "packet28.read_regions",
                {
                    "task_id": TASK_ID,
                    "path": str(FIX_PATH),
                    "line_start": 1,
                    "line_end": 28,
                    "response_mode": "slim",
                },
            )
        )
        steps.append(
            record_mcp_step(
                session,
                "patch risk",
                "packet28.patch_risk",
                {"task_id": TASK_ID, "paths": [str(FIX_PATH)]},
                fetch_artifact=False,
            )
        )
        steps.append(run_explicit_packet28_cargo(root, packet28_bin))
        apply_fix(root)
        steps.append(run_explicit_packet28_cargo(root, packet28_bin))
        validate = record_mcp_step(
            session,
            "validate tool outcome",
            "packet28.validate_tool_outcome",
            {
                "task_id": TASK_ID,
                "command": "cargo test --lib",
                "focus_paths": [str(FIX_PATH)],
            },
            fetch_artifact=False,
        )
        steps.append(validate)
        feature_checks = {
            "glob_artifact_fetch": bool(steps[0].get("artifact_fetch_succeeded")),
            "search_artifact_fetch": bool(steps[1].get("artifact_fetch_succeeded")),
            "read_regions_artifact_fetch": bool(steps[2].get("artifact_fetch_succeeded")),
            "patch_risk_returned_required_checks": bool(
                steps[3].get("payload", {}).get("required_checks")
            ),
            "pretool_hooks_preserve_commands_and_permissions": all(
                step.get("pretool_capture_only") for step in steps[4:6]
            ),
            "explicit_cli_pre_fix_failure_preserved": (
                steps[4].get("status") == "ok" and steps[4].get("exit_code") != 0
            ),
            "explicit_cli_rerun_observes_source_fix": (
                steps[5].get("status") == "ok" and steps[5].get("exit_code") == 0
            ),
            "post_fix_tests_passed": steps[5].get("exit_code") == 0,
            "validate_tool_outcome_returned_status": bool(
                steps[6].get("payload", {}).get("status")
            ),
        }
    subprocess.run(
        [str(packet28_bin), "daemon", "stop", "--root", str(root)],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    return steps, feature_checks


def summarize(normal_steps: list[dict[str, Any]], packet_steps: list[dict[str, Any]]) -> dict[str, Any]:
    normal_tokens = sum(step["tokens"] for step in normal_steps)
    packet_tokens = sum(step["tokens"] for step in packet_steps)
    artifact_tokens = sum(step.get("artifact_fetch_tokens", 0) for step in packet_steps)
    packet_tokens_with_artifacts = packet_tokens + artifact_tokens
    saved = normal_tokens - packet_tokens
    saved_with_artifacts = normal_tokens - packet_tokens_with_artifacts
    pct = round(100.0 * saved / normal_tokens, 1) if normal_tokens else 0.0
    pct_with_artifacts = (
        round(100.0 * saved_with_artifacts / normal_tokens, 1) if normal_tokens else 0.0
    )
    explicit_cli_steps = [
        step
        for step in packet_steps
        if step.get("tool") == "packet28.explicit_cli" and step.get("status") == "ok"
    ]
    explicit_cli_raw_tokens = sum(step.get("raw_tokens", 0) for step in explicit_cli_steps)
    explicit_cli_reduced_tokens = sum(step.get("reduced_tokens", 0) for step in explicit_cli_steps)
    explicit_cli_reduction_pct = (
        round(100.0 * (explicit_cli_raw_tokens - explicit_cli_reduced_tokens) / explicit_cli_raw_tokens, 1)
        if explicit_cli_raw_tokens
        else None
    )
    artifact_steps = [step for step in packet_steps if step.get("artifact_fetch_tokens", 0) > 0]
    artifact_full_tokens = sum(step.get("artifact_fetch_tokens", 0) for step in artifact_steps)
    artifact_slim_tokens = sum(step.get("tokens", 0) for step in artifact_steps)
    artifact_slim_reduction_pct = (
        round(100.0 * (artifact_full_tokens - artifact_slim_tokens) / artifact_full_tokens, 1)
        if artifact_full_tokens
        else None
    )
    return {
        "normal_context_tokens": normal_tokens,
        "packet28_context_tokens": packet_tokens,
        "packet28_context_with_optional_artifacts_tokens": packet_tokens_with_artifacts,
        "saved_context_tokens": saved,
        "saved_context_with_optional_artifacts_tokens": saved_with_artifacts,
        "savings_pct": pct,
        "savings_with_optional_artifacts_pct": pct_with_artifacts,
        "normal_step_count": len(normal_steps),
        "packet28_step_count": len(packet_steps),
        "packet28_optional_artifact_fetch_tokens": artifact_tokens,
        "explicit_cli_raw_tokens": explicit_cli_raw_tokens,
        "explicit_cli_reduced_tokens": explicit_cli_reduced_tokens,
        "explicit_cli_reduction_pct": explicit_cli_reduction_pct,
        "artifact_full_tokens": artifact_full_tokens,
        "artifact_slim_tokens": artifact_slim_tokens,
        "artifact_slim_reduction_pct": artifact_slim_reduction_pct,
    }


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    passed_feature_checks = sum(1 for ok in report["packet28_feature_checks"].values() if ok)
    total_feature_checks = len(report["packet28_feature_checks"])
    lines = [
        "# Packet28 Agentic Coding Tokenomics",
        "",
        "This experiment compares a normal shell-tool trace against a Packet28 trace on the same generated Rust bug-fix task.",
        "",
        "## Result",
        "",
        f"- Normal-tool context: `{summary['normal_context_tokens']}` estimated tokens",
        f"- Packet28 context: `{summary['packet28_context_tokens']}` estimated tokens",
        f"- Saved context: `{summary['saved_context_tokens']}` estimated tokens",
        f"- Savings: `{summary['savings_pct']}%`",
        f"- Packet28 context if every optional artifact is fetched into context: `{summary['packet28_context_with_optional_artifacts_tokens']}` estimated tokens",
        f"- Savings with optional artifact fetches included: `{summary['savings_with_optional_artifacts_pct']}%`",
        f"- Explicit CLI route reduction: `{summary['explicit_cli_reduction_pct']}%`",
        f"- Slim MCP payload reduction versus full artifact payloads: `{summary['artifact_slim_reduction_pct']}%`",
        f"- Normal steps: `{summary['normal_step_count']}`",
        f"- Packet28 steps, including extra safety/features: `{summary['packet28_step_count']}`",
        f"- Optional full-artifact fetch tokens verified but not counted in slim context: `{summary['packet28_optional_artifact_fetch_tokens']}`",
        f"- Feature checks passed: `{passed_feature_checks}/{total_feature_checks}`",
        "",
        "Measured savings combine slim MCP responses with explicitly requested CLI reduction. Native hooks preserve commands and permissions; they do not reduce command output. Artifact fetches are reported separately because fetching every artifact into context is a different usage mode.",
        "",
        "## Feature Checks",
        "",
        "| Feature | Status |",
        "| --- | --- |",
    ]
    for name, ok in report["packet28_feature_checks"].items():
        lines.append(f"| `{name}` | {'ok' if ok else 'failed'} |")
    lines.extend(
        [
            "",
            "## Step Token Comparison",
            "",
            "| Trace | Step | Tool/Command | Exit/Status | Tokens | Notes |",
            "| --- | --- | --- | --- | ---: | --- |",
        ]
    )
    for idx, step in enumerate(report["normal_steps"], start=1):
        lines.append(
            f"| normal | {idx} | `{step['command']}` | `{step['exit_code']}` | {step['tokens']} | raw output |"
        )
    for idx, step in enumerate(report["packet28_steps"], start=1):
        status = step.get("exit_code", step.get("status", "ok"))
        note = "slim MCP payload"
        if step.get("tool") == "packet28.explicit_cli":
            note = f"explicit CLI reduction {step.get('reduction_pct', 0.0)}%; native PreToolUse preserved"
        elif step.get("artifact_id"):
            note = f"artifact `{step['artifact_id']}` fetch ok={step.get('artifact_fetch_succeeded')}"
        lines.append(
            f"| packet28 | {idx} | `{step.get('tool', step.get('command', ''))}` | `{status}` | {step['tokens']} | {note} |"
        )
    lines.extend(
        [
            "",
            "## Task Outcome",
            "",
            "- Both traces applied the same deterministic fix: `input.trim().to_ascii_lowercase()`.",
            f"- Packet28 post-fix focused Rust tests: `{'passed' if report['packet28_feature_checks'].get('post_fix_tests_passed') else 'failed'}`.",
            f"- Measured context savings for this trace: `{summary['savings_pct']}%`.",
            "",
        ]
    )
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare normal tools vs Packet28 on a generated agentic coding task."
    )
    parser.add_argument("--packet28-bin", default="target/debug/Packet28")
    parser.add_argument("--artifact-dir", required=True)
    parser.add_argument("--keep-work", action="store_true")
    parser.add_argument("--min-slim-savings-pct", type=float, default=90.0)
    parser.add_argument("--min-with-artifacts-savings-pct", type=float, default=50.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path.cwd()
    packet28_bin = (repo_root / args.packet28_bin).resolve()
    artifact_dir = Path(args.artifact_dir).resolve()
    artifact_dir.mkdir(parents=True, exist_ok=True)
    work_dir = artifact_dir / "work"
    if work_dir.exists():
        shutil.rmtree(work_dir)
    normal_root = work_dir / "normal"
    packet_root = work_dir / "packet28"
    write_fixture(normal_root)
    write_fixture(packet_root)
    init_repo(normal_root)
    init_repo(packet_root)

    normal_steps = run_normal_trace(normal_root)
    packet_steps, feature_checks = run_packet28_trace(packet_root, packet28_bin)
    summary = summarize(normal_steps, packet_steps)
    report = {
        "measured_at_unix": int(time.time()),
        "task_id": TASK_ID,
        "packet28_bin": str(packet28_bin),
        "normal_root": str(normal_root),
        "packet28_root": str(packet_root),
        "summary": summary,
        "normal_steps": normal_steps,
        "packet28_steps": packet_steps,
        "packet28_feature_checks": feature_checks,
    }
    (artifact_dir / "results.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    markdown = render_markdown(report)
    (artifact_dir / "summary.md").write_text(markdown + "\n", encoding="utf-8")
    print(markdown)
    if not args.keep_work:
        shutil.rmtree(work_dir, ignore_errors=True)
    if not all(feature_checks.values()):
        return 1
    if summary["savings_pct"] < args.min_slim_savings_pct:
        print(
            f"slim savings {summary['savings_pct']}% below required {args.min_slim_savings_pct}%",
            file=sys.stderr,
        )
        return 1
    if summary["savings_with_optional_artifacts_pct"] < args.min_with_artifacts_savings_pct:
        print(
            "savings with optional artifacts "
            f"{summary['savings_with_optional_artifacts_pct']}% below required "
            f"{args.min_with_artifacts_savings_pct}%",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
