#!/usr/bin/env python3

import argparse
import json
import shlex
import sys
import time
from pathlib import Path

from benchmark_common import estimate_tokens, resolve_shell, run_capture


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare raw Bash output against Packet28 hook rewrite output."
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
        help="Shell binary to use for raw and rewritten command execution",
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
    rewrite = run_capture(hook_cmd, root, pretool_payload)
    if rewrite.returncode not in (0, 2):
        raise SystemExit(
            f"hook rewrite failed ({rewrite.returncode}): {rewrite.stderr or rewrite.stdout}"
        )
    rewrite_payload = json.loads(rewrite.stdout.strip() or "{}")
    rewritten = (
        rewrite_payload.get("hookSpecificOutput", {})
        .get("updatedInput", {})
        .get("command")
    )
    raw = run_capture([shell_path, "-lc", command_text], root)
    raw_visible = raw.stdout + raw.stderr
    if rewritten:
        reduced = run_capture([shell_path, "-lc", rewritten], root)
        reduced_visible = reduced.stdout + reduced.stderr
        reduced_exit_code = reduced.returncode
        status = "ok"
        compact_path = "hook_rewrite"
        raw_output_recoverable = True
    else:
        reduced_visible = raw_visible
        reduced_exit_code = raw.returncode
        status = "passthrough"
        compact_path = "passthrough"
        raw_output_recoverable = False

    payload = {
        "status": status,
        "command": command_text,
        "rewritten_command": rewritten,
        "chosen_shell": shell_path,
        "compact_path": compact_path,
        "raw_output_recoverable": raw_output_recoverable,
        "raw_exit_code": raw.returncode,
        "reduced_exit_code": reduced_exit_code,
        "raw_bytes": len(raw_visible.encode("utf-8")),
        "raw_est_tokens": estimate_tokens(raw_visible),
        "reduced_bytes": len(reduced_visible.encode("utf-8")),
        "reduced_est_tokens": estimate_tokens(reduced_visible),
        "raw_preview": raw_visible[:400],
        "reduced_preview": reduced_visible[:400],
    }
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
