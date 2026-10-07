#!/usr/bin/env python3
"""Run the Packet28 agent-DX workflow benchmark against prebuilt binaries.

One isolated workspace is driven through the workflow an agent depends on:
setup and a fresh index, disabled and re-enabled hooks, capture-only hook
authority, native MCP search and retrieval, fresh and concurrent sessions on
one task, handoff across a stop boundary and a true daemon restart, recovery
from damaged task history, and a bounded running log. Frozen command output is
then replayed through the explicit CLI. Delegated contracts are bound to named
Rust tests in a same-tree product test log.

The runner never builds. Every CLI call records argv, exit, elapsed time and
complete stdout/stderr; every MCP exchange records the complete wire messages.
It writes summary.json even when a scenario fails, and exits 1 if any required
check failed. Validate the result with scripts/validate_agent_dx_benchmark.py.
"""

from __future__ import annotations

import argparse
import datetime
import fcntl
import hashlib
import http.client
import json
import os
import platform
import queue
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

import agent_dx_contract as contract
import agent_dx_native as native
import agent_dx_reduction as reduction
from benchmark_common import estimate_tokens

ROOT = Path(__file__).resolve().parent.parent
SCHEMA = "packet28.agent_dx_benchmark.v1"
BINARIES = ("Packet28", "packet28d", "p28")
CREDENTIAL_ENV = ("GH_TOKEN", "GITHUB_TOKEN", "ANTHROPIC_API_KEY", "OPENAI_API_KEY")
TASK_SEARCH = native.TASK_SEARCH
TASK_SHARED = "agent-dx-shared"
DEFINITION_PATH = native.DEFINITION_PATH
GIT_IDENTITY = ["-c", "user.name=Packet28 Fixture", "-c", "user.email=fixture@example.invalid",
                "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"]


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


class Evidence:
    """Owns the artifact directory and the invocation ledger."""

    def __init__(self, artifact_dir: Path):
        self.dir = artifact_dir
        self.invocations: list[dict] = []
        self.hashes: dict[str, str] = {}

    def write(self, rel: str, data: bytes) -> dict:
        path = self.dir / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        self.hashes[rel] = sha256_bytes(data)
        return {"path": rel, "bytes": len(data), "sha256": self.hashes[rel]}

    def write_json(self, rel: str, value) -> dict:
        return self.write(rel, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


class Workspace:
    """Isolated HOME, PATH and fixture repository driven through real binaries."""

    def __init__(self, evidence: Evidence, bin_dir: Path, work_dir: Path):
        self.evidence = evidence
        self.bin_dir = bin_dir
        self.packet28 = bin_dir / "Packet28"
        self.work = work_dir
        self.home = work_dir / "home"
        self.repo = work_dir / "repo"
        self.daemon_dir = self.repo / ".packet28" / "daemon"
        env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TMPDIR") if key in os.environ}
        for key, rel in {
            "HOME": ".", "CODEX_HOME": ".codex", "CLAUDE_CONFIG_DIR": ".claude",
            "XDG_CONFIG_HOME": ".config", "XDG_CACHE_HOME": ".cache",
            "XDG_DATA_HOME": ".local/share", "XDG_STATE_HOME": ".local/state",
        }.items():
            path = self.home / rel
            path.mkdir(parents=True, exist_ok=True)
            env[key] = str(path)
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '/usr/bin:/bin')}"
        env["NO_COLOR"] = "1"
        # docs/operations.md: managed logs read this from the launching environment.
        env["PACKET28_DAEMON_LOG_MAX_BYTES"] = str(contract.LOG_MAX_BYTES)
        self.env = env
        self.counter = 0

    def run(self, scenario: str, label: str, argv: list, stdin: bytes | None = None,
            timeout: float = 90, check: bool = True) -> subprocess.CompletedProcess:
        self.counter += 1
        argv = [str(item) for item in argv]
        stem = f"scenarios/{scenario}/cli/{self.counter:03d}-{label}"
        started = time.monotonic()
        try:
            completed = subprocess.run(argv, cwd=self.repo, env=self.env, input=stdin,
                                       capture_output=True, timeout=timeout, check=False)
        except subprocess.TimeoutExpired as exc:
            completed = subprocess.CompletedProcess(argv, 124, exc.stdout or b"", (exc.stderr or b"") + b"\n[benchmark timeout]\n")
        elapsed = round((time.monotonic() - started) * 1000, 1)
        record = {
            "scenario": scenario,
            "label": label,
            "argv": argv,
            "cwd": str(self.repo),
            "exit_code": completed.returncode,
            "elapsed_ms": elapsed,
            "stdin": self.evidence.write(f"{stem}.stdin", stdin) if stdin is not None else None,
            "stdout": self.evidence.write(f"{stem}.stdout", completed.stdout),
            "stderr": self.evidence.write(f"{stem}.stderr", completed.stderr),
        }
        self.evidence.invocations.append(record)
        if check and completed.returncode != 0:
            raise CheckFailure(f"{label} exited {completed.returncode}: "
                               f"{completed.stderr.decode(errors='replace')[-600:]}", [record["stderr"]["path"]])
        completed.evidence = record  # type: ignore[attr-defined]
        return completed

    def pk(self, scenario: str, label: str, *args, **kwargs) -> subprocess.CompletedProcess:
        return self.run(scenario, label, [self.packet28, *args], **kwargs)

    def git(self, scenario: str, label: str, *args) -> str:
        return self.run(scenario, label, ["git", *GIT_IDENTITY, *args]).stdout.decode()

    def runtime(self) -> dict:
        return json.loads((self.daemon_dir / "runtime.json").read_text())

    def hook_config_path(self) -> Path:
        return self.daemon_dir / "hook-runtime-v1.json"

    def instance_lock_free(self) -> bool:
        path = self.daemon_dir / ".daemon-instance.lock"
        if not path.exists():
            return True
        with path.open("rb") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(lock, fcntl.LOCK_UN)
            return True

    def owned_processes(self) -> list[str]:
        listing = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True, check=False).stdout
        marker = str(self.repo)
        return [line.strip() for line in listing.splitlines()
                if marker in line and (" serve" in line or "serve-http" in line or "packet28d" in line)]

    def capture_count(self, runtime: str = "codex") -> int:
        db = self.home / ".packet28" / "packet28.db"
        if not db.exists():
            return 0
        connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            return connection.execute(
                "SELECT COUNT(*) FROM hook_events WHERE runtime=? AND event_kind=?",
                (runtime, "post_tool_use"),
            ).fetchone()[0]
        except sqlite3.OperationalError:
            return 0
        finally:
            connection.close()

    def generated_handler(self, event: str) -> str:
        hooks = json.loads((self.repo / ".codex" / "hooks.json").read_text())
        for group in hooks["hooks"][event]:
            for hook in group["hooks"]:
                if "hook codex" in hook.get("command", ""):
                    return hook["command"]
        raise CheckFailure(f"setup generated no Codex {event} handler", [])

    def handler(self, scenario: str, label: str, event: str, payload: dict) -> subprocess.CompletedProcess:
        payload = {"hook_event_name": event, "cwd": str(self.repo), **payload}
        return self.run(scenario, label, ["sh", "-c", self.generated_handler(event)],
                        stdin=json.dumps(payload).encode())


class CheckFailure(Exception):
    def __init__(self, message: str, evidence: list[str]):
        super().__init__(message)
        self.evidence = evidence


class McpSession:
    """One `Packet28 mcp serve` process with complete wire capture.

    The session owns its process from the moment it starts: if initialize
    fails or times out, the process is stopped and the partial wire and
    stderr are persisted before the failure propagates.
    """

    INITIALIZE_TIMEOUT = 60.0
    EXIT_TIMEOUT = 20.0
    READER_JOIN_TIMEOUT = 10.0

    def __init__(self, ws: Workspace, scenario: str, name: str):
        self.ws = ws
        self.scenario = scenario
        self.name = name
        self.rel = f"scenarios/{scenario}/mcp/{name}"
        self.wire: list[bytes] = []
        self.steps: list[dict] = []
        self.next_id = 1
        self.closed = False
        self.stderr_chunks: list[bytes] = []
        self.process = subprocess.Popen(
            [str(ws.packet28), "mcp", "serve", "--root", str(ws.repo)], cwd=ws.repo, env=ws.env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.lines: queue.Queue = queue.Queue()
        self.readers = [threading.Thread(target=self._pump, daemon=True),
                        threading.Thread(target=self._drain_stderr, daemon=True)]
        for reader in self.readers:
            reader.start()
        try:
            self.request("initialize", {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "packet28-agent-dx-benchmark", "version": "1"},
            }, timeout=self.INITIALIZE_TIMEOUT)
        except BaseException:
            self.close()
            raise

    def _pump(self) -> None:
        for line in self.process.stdout:  # type: ignore[union-attr]
            self.lines.put(line)
        self.lines.put(None)

    def _drain_stderr(self) -> None:
        for chunk in iter(lambda: self.process.stderr.read1(65536), b""):  # type: ignore[union-attr]
            self.stderr_chunks.append(chunk)

    def request(self, method: str, params: dict, timeout: float = 60) -> tuple[dict, bytes, float]:
        message_id = self.next_id
        self.next_id += 1
        line = json.dumps({"jsonrpc": "2.0", "id": message_id, "method": method, "params": params}).encode() + b"\n"
        self.wire.append(b"> " + line)
        started = time.monotonic()
        try:
            self.process.stdin.write(line)  # type: ignore[union-attr]
            self.process.stdin.flush()  # type: ignore[union-attr]
        except (BrokenPipeError, ValueError, OSError) as exc:
            raise CheckFailure(f"MCP server stopped accepting {method}: {exc}", [f"{self.rel}.wire", f"{self.rel}.stderr"])
        deadline = started + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise CheckFailure(f"MCP {method} timed out after {timeout}s", [f"{self.rel}.wire", f"{self.rel}.stderr"])
            try:
                raw = self.lines.get(timeout=remaining)
            except queue.Empty:
                continue
            if raw is None:
                raise CheckFailure(f"MCP server exited during {method}", [f"{self.rel}.wire", f"{self.rel}.stderr"])
            self.wire.append(b"< " + raw)
            try:
                message = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if message.get("id") == message_id:
                return message, raw, round((time.monotonic() - started) * 1000, 1)

    def call(self, tool: str, arguments: dict, phase: str, allow_error: bool = False) -> dict:
        request_id = self.next_id
        message, raw, elapsed = self.request("tools/call", {"name": tool, "arguments": arguments})
        result = message.get("result") or {}
        error = message.get("error") or (result.get("isError") and result.get("content"))
        payload = result.get("structuredContent")
        step = {
            "session": self.name, "request_id": request_id, "tool": tool, "phase": phase, "elapsed_ms": elapsed,
            "response_bytes": len(raw), "response_est_tokens": estimate_tokens(raw.decode("utf-8", "replace")),
            "structured_bytes": len(json.dumps(payload).encode()) if payload is not None else 0,
            "task_id": arguments.get("task_id"), "artifact_id": (payload or {}).get("artifact_id"),
            "error": error or None,
        }
        self.steps.append(step)
        if error and not allow_error:
            raise CheckFailure(f"MCP {tool} failed: {json.dumps(error)[:400]}", [f"{self.rel}.wire"])
        return payload or {}

    def close(self) -> None:
        """Stop the owned process within bounded waits and persist everything captured."""
        if self.closed:
            return
        self.closed = True
        try:
            if self.process.stdin:
                self.process.stdin.close()
        except OSError:
            pass
        killed = False
        try:
            self.process.wait(timeout=self.EXIT_TIMEOUT)
        except subprocess.TimeoutExpired:
            killed = True
            self.process.kill()
            try:
                self.process.wait(timeout=self.EXIT_TIMEOUT)
            except subprocess.TimeoutExpired:
                pass
        # Readers finish at EOF once the process has exited; never wait forever
        # in case a grandchild still holds a pipe.
        for reader in self.readers:
            reader.join(timeout=self.READER_JOIN_TIMEOUT)
        if not any(reader.is_alive() for reader in self.readers):
            for pipe in (self.process.stdout, self.process.stderr):
                if pipe:
                    pipe.close()
        self.ws.evidence.write(f"{self.rel}.wire", b"".join(self.wire))
        self.ws.evidence.write(f"{self.rel}.stderr", b"".join(self.stderr_chunks))
        self.ws.evidence.write_json(f"{self.rel}.steps.json", {
            "exit_code": self.process.returncode, "killed": killed,
            "readers_finished": not any(reader.is_alive() for reader in self.readers), "steps": self.steps,
        })


class Scenario:
    def __init__(self, scenario_id: str):
        self.id = scenario_id
        self.checks: list[dict] = []
        self.metrics: dict = {}
        self.error: str | None = None
        self.started = time.monotonic()

    def check(self, check_id: str, passed: bool, detail: str, evidence: list[str] | None = None) -> bool:
        self.checks.append({
            "id": check_id, "passed": bool(passed), "detail": detail,
            "contract": contract.check_statement(self.id, check_id), "evidence": evidence or [],
        })
        return bool(passed)

    def result(self) -> dict:
        recorded = {check["id"] for check in self.checks}
        for missing in contract.MEASURED[self.id]["checks"]:
            if missing not in recorded:
                self.check(missing, False, f"not reached: {self.error or 'scenario stopped early'}")
        self.metrics["elapsed_ms"] = round((time.monotonic() - self.started) * 1000, 1)
        return {
            "id": self.id,
            "intent": contract.MEASURED[self.id]["intent"],
            "status": "passed" if all(check["passed"] for check in self.checks) else "failed",
            "checks": self.checks,
            "metrics": self.metrics,
            "error": self.error,
        }


def write_fixture_repo(repo: Path) -> None:
    for rel, data in native.fixture_files().items():
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        (repo / rel).write_bytes(data)


# ---------------------------------------------------------------- scenarios


def scenario_setup_fresh_index(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    write_fixture_repo(ws.repo)
    ws.git(sid, "git-init", "init", "-q")
    ws.git(sid, "git-add", "add", ".")
    ws.git(sid, "git-commit", "commit", "-qm", "Ledger fixture")
    head = ws.git(sid, "git-head", "rev-parse", "HEAD").strip()
    source_before = (ws.repo / DEFINITION_PATH).read_bytes()
    setup = ws.pk(sid, "setup-codex", "setup", "--root", ws.repo, "--runtime", "codex", "--yes", timeout=180)
    out = setup.stdout.decode(errors="replace")
    s.metrics["setup_ms"] = setup.evidence["elapsed_ms"]
    s.check("setup_succeeded", "index ready" in out and "index deferred" not in out,
            "setup reported `index ready`" if "index ready" in out else "setup did not report `index ready`",
            [setup.evidence["stdout"]["path"]])
    manifest_path = ws.repo / ".packet28" / "index" / "regex-v1" / "manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    dirty = ws.git(sid, "git-status", "status", "--porcelain").strip()
    rel = ws.evidence.write("scenarios/setup_fresh_index/regex-manifest.json", manifest_path.read_bytes() if manifest_path.exists() else b"")["path"]
    s.check("index_attests_setup_changes", manifest.get("workspace_attested_commit") == head and bool(dirty),
            f"attested={manifest.get('workspace_attested_commit')} head={head} dirty_paths={len(dirty.splitlines())}", [rel])
    s.check("user_source_unchanged", (ws.repo / DEFINITION_PATH).read_bytes() == source_before,
            f"{DEFINITION_PATH} sha256 {sha256_bytes(source_before)}")
    session = McpSession(ws, sid, "fresh-index")
    try:
        user = session.call("packet28_search", {"task_id": "agent-dx-setup", "query": "DISCOUNT_AUDIT", "fixed_string": True, "response_mode": "full"}, "initial")
        guidance = session.call("packet28_search", {"task_id": "agent-dx-setup", "query": "packet28", "fixed_string": True, "paths": ["AGENTS.md"], "response_mode": "full"}, "initial")
    finally:
        session.close()
    engine = (user.get("engine") or {}).get("engine")
    s.check("indexed_search_finds_user_code", engine == "indexed_regex" and DEFINITION_PATH in json.dumps(user.get("paths", [])),
            f"engine={engine} paths={user.get('paths')}", [f"{session.rel}.wire"])
    s.check("indexed_search_finds_generated_guidance", "AGENTS.md" in json.dumps(guidance.get("paths", [])),
            f"engine={(guidance.get('engine') or {}).get('engine')} paths={guidance.get('paths')}", [f"{session.rel}.wire"])
    s.metrics["mcp_round_trips"] = len(session.steps)


def scenario_hooks_disabled(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    config_path = ws.hook_config_path()
    config = json.loads(config_path.read_text())
    config["hooks_enabled"] = False
    config_path.write_text(json.dumps(config, indent=2))
    disabled_bytes = config_path.read_bytes()
    doctor = ws.pk(sid, "doctor-disabled", "doctor", "--root", ws.repo, "--agent", "codex", "--json", check=False, timeout=90)
    text = (doctor.stdout + doctor.stderr).decode(errors="replace")
    s.check("disabled_doctor_reports_reason", doctor.returncode != 0 and "hook ingest is disabled" in text,
            f"exit={doctor.returncode}; reason {'present' if 'hook ingest is disabled' in text else 'missing'}",
            [doctor.evidence["stdout"]["path"]])
    s.check("disabled_config_unchanged", config_path.read_bytes() == disabled_bytes, "hook-runtime-v1.json bytes after doctor")
    payload = {"session_id": "agent-dx-disabled", "tool_name": "Bash",
               "tool_input": {"command": "cat src/teller/discount.rs"}, "tool_response": "fixture"}
    before = ws.capture_count()
    disabled = ws.handler(sid, "handler-disabled", "PostToolUse", payload)
    s.check("disabled_handler_captures_nothing", disabled.returncode == 0 and ws.capture_count() == before,
            f"exit={disabled.returncode} capture delta={ws.capture_count() - before}", [disabled.evidence["stdout"]["path"]])
    # A user's own handler must survive explicit setup.
    hooks_path = ws.repo / ".codex" / "hooks.json"
    hooks = json.loads(hooks_path.read_text())
    user_handler = {"hooks": [{"type": "command", "command": "true agent-dx-user-handler", "timeout": 5}]}
    hooks["hooks"]["PostToolUse"].append(user_handler)
    hooks_path.write_text(json.dumps(hooks, indent=2))
    ws.pk(sid, "setup-reactivate", "setup", "--root", ws.repo, "--runtime", "codex", "--yes", timeout=180)
    enabled = json.loads(config_path.read_text()).get("hooks_enabled")
    preserved = "agent-dx-user-handler" in hooks_path.read_text()
    s.check("setup_reactivates_hooks", enabled is True and preserved,
            f"hooks_enabled={enabled} user_handler_preserved={preserved}")
    before = ws.capture_count()
    captured = ws.handler(sid, "handler-enabled", "PostToolUse", payload)
    s.check("reactivated_handler_captures_once", captured.returncode == 0 and ws.capture_count() == before + 1,
            f"exit={captured.returncode} capture delta={ws.capture_count() - before}")
    doctor = ws.pk(sid, "doctor-enabled", "doctor", "--root", ws.repo, "--agent", "codex", "--json", check=False, timeout=90)
    s.check("enabled_doctor_passes", doctor.returncode == 0, f"exit={doctor.returncode}", [doctor.evidence["stdout"]["path"]])


def _authority_keys(value) -> list[str]:
    found = []
    if isinstance(value, dict):
        for key, item in value.items():
            if key in contract.AUTHORITY_FIELDS:
                found.append(key)
            found.extend(_authority_keys(item))
    elif isinstance(value, list):
        for item in value:
            found.extend(_authority_keys(item))
    return found


def scenario_hooks_capture_only(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    commands = ["cat src/teller/discount.rs", "git status", "gh pr view 71 --repo owner/repo", "cargo test", "rm -rf build"]

    def probe(tag: str) -> tuple[list[str], list[str]]:
        problems, evidence = [], []
        for runtime in ("codex", "claude"):
            for index, command in enumerate(commands):
                payload = {"hook_event_name": "PreToolUse", "session_id": f"agent-dx-pre-{tag}", "cwd": str(ws.repo),
                           "tool_name": "Bash", "tool_input": {"command": command}}
                done = ws.pk(sid, f"pretool-{tag}-{runtime}-{index}", "hook", runtime, "--root", ws.repo,
                             stdin=json.dumps(payload).encode(), check=False)
                evidence.append(done.evidence["stdout"]["path"])
                text = done.stdout.decode(errors="replace").strip()
                if done.returncode != 0:
                    problems.append(f"{runtime} `{command}` exited {done.returncode}")
                if text:
                    try:
                        keys = _authority_keys(json.loads(text))
                    except json.JSONDecodeError:
                        keys = [key for key in contract.AUTHORITY_FIELDS if key in text]
                    if keys:
                        problems.append(f"{runtime} `{command}` returned {keys}")
        return problems, evidence

    problems, evidence = probe("default")
    s.check("pretool_never_rewrites_or_decides", not problems,
            "; ".join(problems) or f"{len(commands) * 2} PreToolUse probes returned no authority fields", evidence[:4])
    config_path = ws.hook_config_path()
    config = json.loads(config_path.read_text())
    config["rewrite_enabled"] = True
    config_path.write_text(json.dumps(config, indent=2))
    legacy_bytes = config_path.read_bytes()
    problems, evidence = probe("legacy")
    unchanged = config_path.read_bytes() == legacy_bytes
    s.check("legacy_rewrite_flag_inactive", not problems and unchanged,
            "; ".join(problems) or f"stored rewrite_enabled=true stayed inactive; config unchanged={unchanged}", evidence[:4])
    config["rewrite_enabled"] = False
    config_path.write_text(json.dumps(config, indent=2))


def scenario_native_retrieval(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    files = native.fixture_files()
    # The expected facts come from the declared fixture bytes; the workspace must still hold them.
    changed = [rel for rel, data in files.items() if (ws.repo / rel).read_bytes() != data]
    for rel, data in files.items():
        ws.evidence.write(f"scenarios/{sid}/fixture/{rel}", data)
    ws.evidence.write_json(f"scenarios/{sid}/expected.json", {
        "query": native.SEARCH_QUERY, "matches": native.expected_matches(files, native.SEARCH_QUERY),
        "glob": native.expected_glob(files), "read": native.expected_read(files),
        "fixture_sha256": {rel: sha256_bytes(data) for rel, data in files.items()},
        "workspace_differs": changed,
    })
    if changed:
        raise CheckFailure(f"workspace fixture differs from its declared bytes: {changed}", [])
    read_start, _ = native.expected_read(files)
    session = McpSession(ws, sid, "retrieval")
    try:
        search = session.call("packet28_search", {"task_id": TASK_SEARCH, "query": native.SEARCH_QUERY, "fixed_string": True}, "initial")
        glob = session.call("packet28_glob", {"task_id": TASK_SEARCH, "pattern": native.GLOB_PATTERN}, "initial")
        # Required retrieval: the definition is beyond the slim preview.
        session.call("packet28_fetch_tool_result", {"task_id": TASK_SEARCH, "artifact_id": search.get("artifact_id")},
                     "required_retrieval", allow_error=True)
        read = session.call("packet28_read_regions", {"task_id": TASK_SEARCH, "path": DEFINITION_PATH,
                                                     "line_start": read_start, "line_end": read_start + native.READ_LINES - 1},
                            "required_retrieval", allow_error=True)
        session.call("packet28_fetch_tool_result", {"task_id": TASK_SEARCH, "artifact_id": glob.get("artifact_id")},
                     "verification_retrieval", allow_error=True)
        session.call("packet28_fetch_tool_result", {"task_id": TASK_SEARCH, "artifact_id": read.get("artifact_id")},
                     "verification_retrieval", allow_error=True)
    finally:
        session.close()
    wire = b"".join(session.wire)
    checks, _ = native.check_native_retrieval(wire, files)
    for check_id, (passed, detail) in checks.items():
        s.check(check_id, passed, detail, [f"{session.rel}.wire"])
    ledger, errors = native.native_ledger(wire, session.steps)
    s.check("ledger_matches_wire", not errors, "; ".join(errors) or "six tool calls counted once in their contract phases",
            [f"{session.rel}.wire", f"{session.rel}.steps.json"])
    s.metrics["search_engine"] = (search.get("engine") or {}).get("engine")
    s.metrics.update(native.ledger_metrics(ledger))
    s.metrics["token_estimate"] = "ceil(utf8_bytes/4) of complete JSON-RPC response lines"
    s.metrics["native_evidence"] = {"wire": f"{session.rel}.wire", "steps": f"{session.rel}.steps.json",
                                    "fixture_dir": f"scenarios/{sid}/fixture"}


def scenario_same_task_sessions(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    published: list[tuple[str, str]] = []  # (artifact id, sha256 of first fetch)

    def search_once(name: str, query: str, read: bool = False) -> None:
        session = McpSession(ws, sid, name)
        try:
            found = session.call("packet28_search", {"task_id": TASK_SHARED, "query": query, "fixed_string": True}, "initial")
            ids = [found["artifact_id"]]
            if read:
                ids.append(session.call("packet28_read_regions", {"task_id": TASK_SHARED, "path": DEFINITION_PATH,
                                                                 "line_start": 1, "line_end": 3}, "initial")["artifact_id"])
            for artifact in ids:
                full = session.call("packet28_fetch_tool_result", {"task_id": TASK_SHARED, "artifact_id": artifact}, "verification_retrieval")
                published.append((artifact, sha256_bytes(json.dumps(full, sort_keys=True).encode())))
        finally:
            session.close()

    sequential_errors = []
    for name, query, read in (("sequential-a", "apply_discount", False), ("sequential-b", "DISCOUNT_AUDIT", True)):
        try:
            search_once(name, query, read)
        except CheckFailure as exc:
            sequential_errors.append(f"{name}: {exc}")
    s.check("sequential_fresh_sessions_succeed", not sequential_errors, "; ".join(sequential_errors) or "two fresh processes, same task")
    concurrent_errors: list[str] = []

    def worker(name: str) -> None:
        try:
            search_once(name, "member_price")
        except CheckFailure as exc:
            concurrent_errors.append(f"{name}: {exc}")

    threads = [threading.Thread(target=worker, args=(f"concurrent-{tag}",)) for tag in "cd"]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=120)
    s.check("concurrent_sessions_succeed", not concurrent_errors and not any(t.is_alive() for t in threads),
            "; ".join(concurrent_errors) or "two concurrent processes, same task")
    ids = [artifact for artifact, _ in published]
    s.check("artifact_ids_distinct", len(ids) == 5 and len(set(ids)) == len(ids), f"{len(ids)} artifacts, {len(set(ids))} distinct: {ids}")
    session = McpSession(ws, sid, "later-reader")
    changed = []
    try:
        for artifact, digest in published:
            full = session.call("packet28_fetch_tool_result", {"task_id": TASK_SHARED, "artifact_id": artifact}, "verification_retrieval", allow_error=True)
            if sha256_bytes(json.dumps(full, sort_keys=True).encode()) != digest:
                changed.append(artifact)
    finally:
        session.close()
    s.check("earlier_evidence_retrievable", bool(published) and not changed,
            f"{len(published) - len(changed)}/{len(published)} earlier artifacts fetched unchanged", [f"{session.rel}.wire"])
    s.metrics["mcp_processes"] = 5


def wait_until(predicate, seconds: float) -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def stop_daemon(ws: Workspace, scenario: str, label: str) -> dict:
    pid = ws.runtime().get("pid")
    ws.pk(scenario, label, "daemon", "stop", "--root", ws.repo, timeout=60)
    observed = {
        "pid": pid,
        "lock_free_on_return": ws.instance_lock_free(),
        "ready_removed_on_return": not (ws.daemon_dir / "ready").exists(),
        "runtime_removed_on_return": not (ws.daemon_dir / "runtime.json").exists(),
    }
    observed["process_exited"] = wait_until(lambda: not _pid_alive(pid), 5)
    return observed


def _pid_alive(pid) -> bool:
    if not pid:
        return False
    try:
        os.kill(int(pid), 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def scenario_handoff_cold_restart(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    session_id = "agent-dx-continuation"
    ws.handler(sid, "session-start", "SessionStart", {"session_id": session_id, "source": "startup"})
    ws.handler(sid, "prompt", "UserPromptSubmit", {"session_id": session_id, "prompt": "Fix discount rounding"})
    task = json.loads((ws.repo / ".packet28" / "agent" / "active-task.json").read_text())["task_id"]
    state["task"] = task
    latest = "Round apply_discount to the nearest cent and keep DISCOUNT_AUDIT"
    session = McpSession(ws, sid, "before-stop")
    try:
        session.call("packet28_write_intention", {"task_id": task, "text": "Inspect the discount module", "paths": [DEFINITION_PATH]}, "initial")
        session.call("packet28_write_intention", {"task_id": task, "text": latest, "paths": [DEFINITION_PATH]}, "initial")
        ws.handler(sid, "pretool", "PreToolUse", {"session_id": session_id, "tool_name": "Bash", "tool_input": {"command": "cargo test"}})
        ws.handler(sid, "posttool", "PostToolUse", {"session_id": session_id, "tool_name": "Bash",
                                                    "tool_input": {"command": "cargo test"}, "tool_response": "test result: FAILED. 37 passed; 1 failed"})
        ws.handler(sid, "stop-hook", "Stop", {"session_id": session_id, "stop_hook_active": False})
        handoff = session.call("packet28_prepare_handoff", {"task_id": task, "response_mode": "full"}, "initial")
    finally:
        session.close()
    artifact = ((handoff.get("context") or {}).get("artifact_id"))
    state["handoff_artifact"] = artifact
    state["latest_intention"] = latest
    s.check("handoff_ready_after_stop_boundary",
            handoff.get("handoff_ready") is True and (handoff.get("latest_intention") or {}).get("text") == latest and bool(artifact),
            f"ready={handoff.get('handoff_ready')} reason={handoff.get('handoff_reason')} latest={(handoff.get('latest_intention') or {}).get('text')!r}",
            [f"{session.rel}.wire"])
    stopped = stop_daemon(ws, sid, "daemon-stop")
    s.metrics["stop_ms"] = ws.evidence.invocations[-1]["elapsed_ms"]
    s.check("stop_releases_authority", all(stopped[key] for key in ("lock_free_on_return", "ready_removed_on_return", "process_exited")),
            json.dumps(stopped))
    started = ws.pk(sid, "daemon-start", "daemon", "start", "--root", ws.repo, check=False, timeout=60)
    s.metrics["start_ms"] = started.evidence["elapsed_ms"]
    new_pid = ws.runtime().get("pid") if (ws.daemon_dir / "runtime.json").exists() else None
    s.check("restart_without_retry", started.returncode == 0 and new_pid not in (None, stopped["pid"]),
            f"exit={started.returncode} old_pid={stopped['pid']} new_pid={new_pid}", [started.evidence["stderr"]["path"]])
    session = McpSession(ws, sid, "after-restart")
    try:
        fetched = session.call("packet28_fetch_context", {"task_id": task, "artifact_id": artifact}, "required_retrieval")
        search = session.call("packet28_search", {"task_id": task, "query": "DISCOUNT_AUDIT", "fixed_string": True, "response_mode": "full"}, "initial")
    finally:
        session.close()
    got = (fetched.get("latest_intention") or {}).get("text")
    s.check("fresh_session_resumes_handoff", got == latest, f"latest_intention={got!r}", [f"{session.rel}.wire"])
    engine = (search.get("engine") or {}).get("engine")
    s.check("index_serves_after_restart", engine == "indexed_regex" and search.get("match_count", 0) >= 1,
            f"engine={engine} match_count={search.get('match_count')}")


def scenario_corrupt_history(ws: Workspace, s: Scenario, state: dict) -> None:
    sid = s.id
    predecessor = state.get("task")
    if not predecessor:
        raise CheckFailure("no continuation task from handoff_cold_restart", [])
    stop_daemon(ws, sid, "daemon-stop")
    log = ws.daemon_dir / "tasks" / f"{predecessor}.events.jsonl"
    damaged = log.read_bytes() + b'{"seq": "torn-agent-dx-frame\n'
    log.write_bytes(damaged)
    damaged_rel = ws.evidence.write("scenarios/corrupt_history_recovery/damaged-events.jsonl", damaged)["path"]
    ws.pk(sid, "daemon-start", "daemon", "start", "--root", ws.repo, timeout=60)
    session = McpSession(ws, sid, "after-corruption")
    try:
        written = session.call("packet28_write_intention", {"task_id": predecessor, "text": "Continue from the recovered successor",
                                                            "paths": [DEFINITION_PATH]}, "initial", allow_error=True)
        status = session.call("packet28_task_status", {"task_id": predecessor}, "initial", allow_error=True)
        inherited = session.call("packet28_fetch_context", {"task_id": predecessor, "artifact_id": state.get("handoff_artifact")},
                                 "required_retrieval", allow_error=True)
    finally:
        session.close()
    s.check("first_call_after_corruption_succeeds", written.get("accepted") is True,
            f"accepted={written.get('accepted')}", [f"{session.rel}.wire"])
    registry = json.loads((ws.daemon_dir / "task-registry-v1.json").read_text())
    old = registry.get("tasks", {}).get(predecessor, {})
    lineage = old.get("superseded_by") or {}
    successor = lineage.get("successor_task_id")
    record = registry.get("tasks", {}).get(successor or "", {})
    resolved = (status.get("task") or {}).get("task_id")
    s.check("successor_lineage_recorded",
            bool(successor) and successor != predecessor and record.get("recovered_from") == lineage and resolved == successor,
            f"predecessor={predecessor} successor={successor} status_resolves_to={resolved}")
    quarantine = ws.daemon_dir / "tasks" / str(lineage.get("quarantined_event_log", "missing"))
    exact = quarantine.is_file() and quarantine.read_bytes() == damaged
    s.check("damaged_history_quarantined_exactly", exact and not log.exists(),
            f"quarantine={lineage.get('quarantined_event_log')} exact={exact} original_removed={not log.exists()}", [damaged_rel])
    got = (inherited.get("latest_intention") or {}).get("text")
    s.check("inherited_context_available", got == state.get("latest_intention"), f"latest_intention={got!r}")
    successor_log = ws.daemon_dir / "tasks" / f"{successor}.events.jsonl"
    frames = [json.loads(line) for line in successor_log.read_bytes().splitlines() if line] if successor_log.is_file() else []
    s.check("successor_history_starts_at_one", bool(frames) and frames[0].get("seq") == 1 and all(f.get("task_id") == successor for f in frames),
            f"{len(frames)} successor frames, first seq={frames[0].get('seq') if frames else None}")


def daemon_rpc(runtime: dict, message: dict) -> dict:
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(10)
        sock.connect(runtime["socket_path"])
        data = json.dumps(message).encode()
        sock.sendall(struct.pack(">Q", len(data)) + data)

        def exact(count: int) -> bytes:
            buffer = b""
            while len(buffer) < count:
                chunk = sock.recv(count - len(buffer))
                if not chunk:
                    raise CheckFailure("daemon closed the connection", [])
                buffer += chunk
            return buffer

        return json.loads(exact(struct.unpack(">Q", exact(8))[0]))


def log_generations(path: Path) -> list[bytes | None]:
    return [(path if index == 0 else path.with_name(f"{path.name}.{index}")).read_bytes()
            if (path if index == 0 else path.with_name(f"{path.name}.{index}")).exists() else None
            for index in range(contract.LOG_GENERATIONS + 1)]


def scenario_runtime_log_bounds(ws: Workspace, s: Scenario, state: dict) -> None:
    runtime = ws.runtime()
    pid = runtime.get("pid")
    if not runtime.get("socket_path"):
        raise CheckFailure("daemon runtime has no Unix socket; this scenario requires the Unix transport", [])
    errors = 0
    for index in range(128):
        reply = daemon_rpc(runtime, {"type": "packet_fetch", "request": {"handle": f"agent-dx-missing-{index:03d}", "root": str(ws.repo)}})
        errors += reply.get("type") == "error"
    log = ws.daemon_dir / "packet28d.log"
    previous = None
    deadline = time.monotonic() + 15
    while True:
        current = log_generations(log)
        if current == previous and b"agent-dx-missing-127" in (current[0] or b""):
            break
        if time.monotonic() > deadline:
            break
        previous = current
        time.sleep(0.3)
    sizes = [len(item) if item is not None else None for item in current]
    hook_sizes = [len(item) if item is not None else None for item in log_generations(ws.daemon_dir / "packet28-hook-http.log")]
    for index, data in enumerate(current):
        if data is not None:
            ws.evidence.write(f"scenarios/runtime_log_bounds/packet28d.log.{index}", data)
    s.check("same_daemon_process", ws.runtime().get("pid") == pid and _pid_alive(pid) and errors == 128,
            f"pid before={pid} after={ws.runtime().get('pid')} error replies={errors}/128")
    within = (all(size is None or size <= contract.LOG_MAX_BYTES for size in sizes + hook_sizes)
              and sizes[contract.LOG_GENERATIONS] is None and hook_sizes[contract.LOG_GENERATIONS] is None
              and sizes[1] is not None)
    s.check("generations_within_limit", within,
            f"daemon generations={sizes} hook generations={hook_sizes} limit={contract.LOG_MAX_BYTES}")
    s.check("diagnostics_retained", b"agent-dx-missing-127" in (current[0] or b""), "latest missing-packet diagnostic in active log",
            ["scenarios/runtime_log_bounds/packet28d.log.0"])
    s.metrics.update({"daemon_log_generation_bytes": sizes, "hook_log_generation_bytes": hook_sizes})


def scenario_source_freshness(ws: Workspace, s: Scenario, state: dict) -> None:
    """Edit an indexed tracked file without reporting it, then query it."""
    sid = s.id
    path = ws.repo / native.FRESHNESS_PATH
    original = path.read_bytes()
    if original != native.fixture_files()[native.FRESHNESS_PATH]:
        raise CheckFailure(f"{native.FRESHNESS_PATH} differs from its committed fixture bytes", [])
    runtime = ws.runtime()
    if not runtime.get("socket_path"):
        raise CheckFailure("daemon runtime has no Unix socket; the forced-index request needs the Unix transport", [])

    def forced(query: str) -> dict:
        return daemon_rpc(runtime, {"type": "packet28_search", "request": {
            "request": {"query": query, "fixed_string": True}, "force_indexed": True}})

    replies = {"before_edit": forced(native.FRESHNESS_OLD)}
    edited = native.edited_freshness_source(original)
    session = None
    try:
        path.write_bytes(edited)
        ws.evidence.write(f"scenarios/{sid}/edited/{native.FRESHNESS_PATH}", edited)
        replies["after_edit"] = forced(native.FRESHNESS_NEW)
        session = McpSession(ws, sid, "after-edit")
        for query in (native.FRESHNESS_NEW, native.FRESHNESS_OLD):
            session.call("packet28_search", {"task_id": "agent-dx-freshness", "query": query, "fixed_string": True,
                                             "response_mode": "full"}, "initial", allow_error=True)
    finally:
        if session is not None:
            session.close()
        path.write_bytes(original)
        ws.evidence.write_json(f"scenarios/{sid}/forced-index.json", replies)
    checks = native.check_freshness(b"".join(session.wire), replies, edited)
    for check_id, (passed, detail) in checks.items():
        s.check(check_id, passed, detail, [f"{session.rel}.wire", f"scenarios/{sid}/forced-index.json"])
    s.check("fixture_restored", path.read_bytes() == original, f"{native.FRESHNESS_PATH} sha256 {sha256_bytes(path.read_bytes())}")
    s.metrics["freshness_evidence"] = {"wire": f"{session.rel}.wire", "forced": f"scenarios/{sid}/forced-index.json",
                                       "edited": f"scenarios/{sid}/edited/{native.FRESHNESS_PATH}"}


def scenario_explicit_cli(ws: Workspace, s: Scenario, state: dict, results: list[dict]) -> None:
    manifest = reduction.load_manifest()
    replay_dir = ws.work / "replay"
    replay_dir.mkdir(exist_ok=True)
    for case in manifest["cases"]:
        result, streams = reduction.replay_case(replay_dir, ws.packet28, case)
        result["streams"] = {label: ws.evidence.write(f"reduction/{case['case']}.{label.replace('_', '.')}", data)
                             for label, data in streams.items()}
        results.append(result)
    replayed = [r for r in results if r["stub_invocations"] == [next(c for c in manifest["cases"] if c["case"] == r["case"])["argv"][1:]]]
    s.check("every_frozen_case_replayed", len(replayed) == len(manifest["cases"]),
            f"{len(replayed)}/{len(manifest['cases'])} cases rendered their frozen input through one stub call")
    broken = [f"{r['case']}: {'; '.join(r['errors'])}" for r in results if not r["passed"]]
    s.check("derived_contracts_hold", not broken, " | ".join(broken) or "all derived contracts held",
            [f"reduction/{r['case']}.reduced.stdout" for r in results if not r["passed"]])
    unreduced = [r["case"] for r in results if r["role"] == "verbose"
                 and (r["reduced_bytes"] >= r["raw_bytes"] or r["reduced_est_tokens"] >= r["raw_est_tokens"])]
    s.check("verbose_cases_reduced", not unreduced, ", ".join(unreduced) or "every verbose case is smaller in bytes and tokens")
    verbose = [r for r in results if r["role"] == "verbose"]
    s.metrics.update({
        "cases": len(results),
        "verbose_raw_bytes": sum(r["raw_bytes"] for r in verbose),
        "verbose_reduced_bytes": sum(r["reduced_bytes"] for r in verbose),
        "verbose_raw_est_tokens": sum(r["raw_est_tokens"] for r in verbose),
        "verbose_reduced_est_tokens": sum(r["reduced_est_tokens"] for r in verbose),
    })


def cleanup(ws: Workspace, s: Scenario) -> None:
    if (ws.daemon_dir / "runtime.json").exists():
        ws.pk("cleanup", "daemon-stop", "daemon", "stop", "--root", ws.repo, check=False, timeout=60)
    try:
        config = json.loads(ws.hook_config_path().read_text())
        port, token = config.get("http_hook_port"), config.get("http_hook_token")
        if port and token:
            try:
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                connection.request("POST", "/packet28/shutdown", headers={"x-packet28-hook-token": token})
                connection.getresponse().read()
                connection.close()
            except OSError:
                pass
    except (OSError, ValueError):
        pass
    exited = wait_until(lambda: not ws.owned_processes(), 15)
    remaining = ws.owned_processes()
    s.check("authority_released", ws.instance_lock_free(), "daemon instance lock acquired non-blockingly")
    s.check("owned_processes_exited", exited and not remaining, "; ".join(remaining) or "no process references the workspace")
    for line in remaining:  # Last resort; only processes naming this private workspace.
        try:
            os.kill(int(line.split()[0]), 15)
        except (ProcessLookupError, ValueError, PermissionError):
            pass


# ---------------------------------------------------------------- run metadata


def git_bytes(root: Path, *args: str) -> bytes:
    return subprocess.run(["git", *args], cwd=root, capture_output=True, check=False, timeout=60).stdout


def git_text(*args: str, root: Path = ROOT) -> str:
    return git_bytes(root, *args).decode().strip()


def tool_version(argv: list[str]) -> str | None:
    try:
        return subprocess.run(argv, capture_output=True, text=True, check=False, timeout=10).stdout.splitlines()[0]
    except (OSError, IndexError, subprocess.TimeoutExpired):
        return None


def source_metadata(root: Path = ROOT) -> tuple[dict, bytes]:
    """Commit, tree and every modified, staged or untracked path, from `git status -z`.

    The raw status bytes are returned so the validator can parse them itself.
    """
    status = git_bytes(root, "status", "--porcelain=v1", "-z", "--untracked-files=all")
    entries = contract.parse_porcelain_z(status)
    return {
        "commit": git_text("rev-parse", "HEAD", root=root),
        "tree": git_text("rev-parse", "HEAD^{tree}", root=root),
        "runtime_trees": {path: git_text("rev-parse", f"HEAD:{path}", root=root) for path in ("crates", "Cargo.toml", "Cargo.lock")},
        "dirty": bool(entries),
        "dirty_paths": sorted({entry["path"] for entry in entries} | {entry["orig_path"] for entry in entries if entry.get("orig_path")}),
        "relevant_dirty_paths": contract.relevant_dirty(entries),
        "lock_sha256": sha256_bytes((root / "Cargo.lock").read_bytes()),
        "github": {key: os.environ.get(key) for key in ("GITHUB_SHA", "GITHUB_REF", "GITHUB_RUN_ID", "GITHUB_WORKFLOW")},
    }, status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin-dir", required=True, help="Directory holding prebuilt Packet28, packet28d and p28")
    parser.add_argument("--artifact-dir", required=True)
    parser.add_argument("--binary-binding", help="Optional build receipt naming the source tree of --bin-dir")
    parser.add_argument("--product-test-log", help="`cargo test` log that ran the delegated tests")
    parser.add_argument("--product-test-tree", help="Git tree the product test log was produced from (default: HEAD's tree)")
    parser.add_argument("--keep-workspace", action="store_true")
    args = parser.parse_args()

    for key in CREDENTIAL_ENV:
        os.environ.pop(key, None)
    bin_dir = Path(args.bin_dir).resolve()
    artifact_dir = Path(args.artifact_dir).resolve()
    # Never delete an existing directory: it may hold an earlier failure's evidence.
    if artifact_dir.exists() and (not artifact_dir.is_dir() or any(artifact_dir.iterdir())):
        print(f"[agent-dx] refusing to write into non-empty {artifact_dir}; choose a new or empty --artifact-dir",
              file=sys.stderr)
        return 2
    artifact_dir.mkdir(parents=True, exist_ok=True)
    evidence = Evidence(artifact_dir)
    source, status = source_metadata()
    source["status_porcelain_z"] = evidence.write("inputs/source-status-start.z", status)
    summary: dict = {
        "schema": SCHEMA, "contract_version": contract.SCHEMA_VERSION, "benchmark": contract.BENCHMARK,
        "started_at": utc_now(), "artifact_dir": str(artifact_dir), "source": source,
        "binaries": [], "environment": {
            "os": platform.system().lower(), "arch": platform.machine(), "python": platform.python_version(),
            "git": tool_version(["git", "--version"]), "rg": tool_version(["rg", "--version"]),
            "token_estimator": "ceil_utf8_bytes_div_4",
        },
        "required_scenarios": contract.required_scenarios(), "scenarios": [], "reduction_cases": [],
    }
    for name in BINARIES:
        path = bin_dir / name
        data = path.read_bytes() if path.is_file() else b""
        summary["binaries"].append({"name": name, "path": str(path), "bytes": len(data), "sha256": sha256_bytes(data) if data else None})
    if args.binary_binding:
        summary["binary_binding"] = evidence.write("inputs/binary-binding.json", Path(args.binary_binding).read_bytes())
    if args.product_test_log:
        summary["product_tests"] = {"log": evidence.write("inputs/product-tests.log", Path(args.product_test_log).read_bytes()),
                                    "source_tree": args.product_test_tree or summary["source"]["tree"]}

    work_dir = Path(tempfile.mkdtemp(prefix="packet28-agent-dx-"))
    ws = Workspace(evidence, bin_dir, work_dir)
    ws.repo.mkdir()
    state: dict = {}
    order = [
        ("setup_fresh_index", scenario_setup_fresh_index),
        ("hooks_disabled_honesty", scenario_hooks_disabled),
        ("hooks_capture_only", scenario_hooks_capture_only),
        ("native_retrieval", scenario_native_retrieval),
        ("same_task_sessions", scenario_same_task_sessions),
        ("handoff_cold_restart", scenario_handoff_cold_restart),
        ("corrupt_history_recovery", scenario_corrupt_history),
        ("runtime_log_bounds", scenario_runtime_log_bounds),
        ("source_freshness", scenario_source_freshness),
    ]
    try:
        for scenario_id, function in order:
            scenario = Scenario(scenario_id)
            print(f"[agent-dx] {scenario_id}", flush=True)
            try:
                function(ws, scenario, state)
            except Exception as exc:  # Recorded as the scenario's failure; later scenarios still run.
                scenario.error = f"{type(exc).__name__}: {exc}"
                evidence.write(f"scenarios/{scenario_id}/error.txt", traceback.format_exc().encode())
            summary["scenarios"].append(scenario.result())
        scenario = Scenario("explicit_cli_reduction")
        print("[agent-dx] explicit_cli_reduction", flush=True)
        try:
            scenario_explicit_cli(ws, scenario, state, summary["reduction_cases"])
        except Exception as exc:
            scenario.error = f"{type(exc).__name__}: {exc}"
            evidence.write("scenarios/explicit_cli_reduction/error.txt", traceback.format_exc().encode())
        summary["scenarios"].append(scenario.result())
    finally:
        scenario = Scenario("cleanup")
        try:
            cleanup(ws, scenario)
        except Exception as exc:
            scenario.error = f"{type(exc).__name__}: {exc}"
        summary["scenarios"].append(scenario.result())
        failed = any(x["status"] != "passed" for x in summary["scenarios"])
        # Only the private temporary workspace is ever removed, and only after a clean pass.
        if not args.keep_workspace and not failed and not ws.owned_processes():
            shutil.rmtree(work_dir, ignore_errors=True)
        summary["workspace"] = str(work_dir)
        summary["workspace_kept"] = work_dir.exists()
        summary["finished_at"] = utc_now()
        # A source change during the run would make the start receipt describe a different measurement.
        end, status = source_metadata()
        end["status_porcelain_z"] = evidence.write("inputs/source-status-end.z", status)
        summary["source_end"] = end
        evidence.write("invocations.json", (json.dumps(evidence.invocations, indent=2) + "\n").encode())
        summary["evidence_sha256"] = dict(sorted(evidence.hashes.items()))
        summary["status"] = "passed" if all(x["status"] == "passed" for x in summary["scenarios"]) else "failed"
        (artifact_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    for scenario in summary["scenarios"]:
        print(f"[agent-dx] {scenario['status']:>6} {scenario['id']}")
        for check in scenario["checks"]:
            if not check["passed"]:
                print(f"           FAIL {check['id']}: {check['detail']}")
    print(f"[agent-dx] summary: {artifact_dir / 'summary.json'}")
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
