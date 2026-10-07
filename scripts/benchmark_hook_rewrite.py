#!/usr/bin/env python3

import argparse
import json
import shlex
import sys
import time
from pathlib import Path

from benchmark_common import estimate_tokens, resolve_shell, run_capture


def explicit_cli_args(root: Path, argv: list[str], task_id: str) -> list[str]:
    """Choose existing explicit CLI routes without changing shell expressions."""
    if not argv or any(token in {"|", "||", "&&", ";", "2>&1", ">", "<"} for token in argv):
        raise ValueError("shell expressions have no equivalent explicit route in this benchmark")
    if argv[0] in {"git", "cargo", "gh"}:
        return argv.copy()
    if argv[0] == "head" and len(argv) == 4 and argv[1] == "-n":
        count = int(argv[2])
        if count <= 0:
            raise ValueError("head line count must be positive")
        window = ["--line-start", "1", "--line-end", str(count)]
        path = argv[3]
    elif argv[0] == "cat" and len(argv) == 2:
        window = []
        path = argv[1]
    else:
        raise ValueError(f"no explicit benchmark route for {shlex.join(argv)}")
    if path == "-" or path.startswith("-"):
        raise ValueError("stdin and option-shaped paths have no equivalent explicit read route")
    return [
        "--via-daemon", "--daemon-root", str(root), "compact", "read",
        "--root", str(root), "--task-id", task_id, "--cwd", str(root),
        *window, path,
    ]


def is_capture_only(payload: dict) -> bool:
    if not isinstance(payload, dict):
        return False
    specific = payload.get("hookSpecificOutput", {})
    return isinstance(specific, dict) and all(
        key not in payload and key not in specific
        for key in ("updatedInput", "permissionDecision", "decision")
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare raw Bash output against explicitly requested Packet28 CLI reduction."
    )
    parser.add_argument("--root", default=".", help="Repository root")
    parser.add_argument("--task-id", default=None, help="Optional task id")
    parser.add_argument("--session-id", default=None, help="Optional session id")
    parser.add_argument("--json", action="store_true", help="Emit JSON instead of markdown")
    parser.add_argument(
        "--artifact-path",
        default=None,
        help="Optional JSON artifact output path",
    )
    parser.add_argument(
        "--shell",
        default=None,
        help="Shell binary to use for raw command execution",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER, help="Command to benchmark")
    args = parser.parse_args()

    if args.command and args.command[0] == "--":
        args.command = args.command[1:]
    if not args.command:
        parser.error("command required after '--'")

    root = Path(args.root).resolve()
    command_text = shlex.join(args.command)
    task_id = args.task_id or f"bench-hook-{int(time.time())}"
    session_id = args.session_id or f"bench-session-{int(time.time())}"
    try:
        shell_path = resolve_shell(args.shell)
    except FileNotFoundError as exc:
        raise SystemExit(f"benchmark shell setup failed: {exc}") from exc

    pretool_payload = json.dumps(
        {
            "hook_event_name": "PreToolUse",
            "task_id": task_id,
            "session_id": session_id,
            "cwd": str(root),
            "tool_name": "Bash",
            "tool_input": {"command": command_text},
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
    hook = run_capture(hook_cmd, root, pretool_payload)
    if hook.returncode != 0:
        raise SystemExit(f"capture-only hook failed ({hook.returncode}): {hook.stderr or hook.stdout}")
    if "allowing runtime action after processing error" in hook.stderr:
        raise SystemExit(f"hook processing failed: {hook.stderr.strip()}")
    hook_payload = json.loads(hook.stdout.strip() or "{}")
    if not is_capture_only(hook_payload):
        raise SystemExit("PreToolUse changed native input or permission authority")
    try:
        explicit_args = explicit_cli_args(root, args.command, task_id)
    except ValueError as exc:
        raise SystemExit(str(exc)) from exc
    explicit_cmd = ["cargo", "run", "-q", "-p", "suite-cli", "--bin", "Packet28", "--", *explicit_args]
    raw = run_capture([shell_path, "-lc", command_text], root)
    reduced = run_capture(explicit_cmd, root)
    raw_visible = raw.stdout + raw.stderr
    reduced_visible = reduced.stdout + reduced.stderr
    reduced_exit_code = reduced.returncode
    integrity_error = None
    if raw.returncode != reduced.returncode:
        integrity_error = f"explicit CLI exit {reduced.returncode} differs from raw exit {raw.returncode}"
    read_window_integrity = None
    if args.command[0] == "head":
        raw_lines = raw.stdout.splitlines()
        expected = "\n".join(f"{index}|{line}" for index, line in enumerate(raw_lines, 1)) + "\n"
        read_window_integrity = {
            "passed": raw.returncode == reduced.returncode == 0 and reduced.stdout == expected,
            "line_start": 1,
            "line_end": int(args.command[2]),
            "raw_line_count": len(raw_lines),
        }
        if not read_window_integrity["passed"]:
            integrity_error = "explicit read changed the original head window, contents, or successful exit"
    status = "error" if integrity_error else "ok"
    payload = {
        "status": status,
        "command": command_text,
        "rewritten_command": None,
        "explicit_command": shlex.join(["Packet28", *explicit_args]),
        "pretool_capture_only": True,
        "chosen_shell": shell_path,
        "compact_path": "explicit_cli",
        "estimate_scope": "visible_cli_output",
        "raw_output_recoverable": False,
        "raw_exit_code": raw.returncode,
        "reduced_exit_code": reduced_exit_code,
        "raw_bytes": len(raw_visible.encode("utf-8")),
        "raw_est_tokens": estimate_tokens(raw_visible),
        "reduced_bytes": len(reduced_visible.encode("utf-8")),
        "reduced_est_tokens": estimate_tokens(reduced_visible),
        "raw_preview": raw_visible[:400],
        "reduced_preview": reduced_visible[:400],
    }
    if integrity_error:
        payload["error"] = integrity_error
    if read_window_integrity is not None:
        payload["read_window_integrity"] = read_window_integrity
    if payload["raw_est_tokens"]:
        payload["token_reduction_pct"] = round(
            100
            * (payload["raw_est_tokens"] - payload["reduced_est_tokens"])
            / payload["raw_est_tokens"],
            1,
        )
    else:
        payload["token_reduction_pct"] = 0.0
    payload["measured_at_unix"] = int(time.time())

    if args.artifact_path:
        artifact_path = Path(args.artifact_path)
        artifact_path.parent.mkdir(parents=True, exist_ok=True)
        artifact_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        print(
            "\n".join(
                [
                    f"command: {payload['command']}",
                    f"raw: {payload['raw_bytes']} bytes / {payload['raw_est_tokens']} tokens (exit {payload['raw_exit_code']})",
                    f"reduced: {payload['reduced_bytes']} bytes / {payload['reduced_est_tokens']} tokens (exit {payload['reduced_exit_code']})",
                    f"reduction: {payload['token_reduction_pct']}%",
                    "",
                    "reduced preview:",
                    payload["reduced_preview"].rstrip(),
                ]
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
