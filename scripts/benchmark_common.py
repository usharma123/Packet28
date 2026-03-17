#!/usr/bin/env python3

import os
import shutil
import subprocess
from pathlib import Path


def estimate_tokens(text: str) -> int:
    return max(1, (len(text.encode("utf-8")) + 3) // 4) if text else 0


def resolve_shell(explicit_shell: str | None = None) -> str:
    candidates: list[str] = []
    if explicit_shell:
        candidates.append(explicit_shell)

    env_shell = os.environ.get("SHELL")
    if env_shell:
        candidates.append(env_shell)

    candidates.extend(["bash", "sh"])

    seen = set()
    for candidate in candidates:
        if not candidate or candidate in seen:
            continue
        seen.add(candidate)
        if os.path.isabs(candidate) and os.access(candidate, os.X_OK):
            return candidate
        resolved = shutil.which(candidate)
        if resolved:
            return resolved
    raise FileNotFoundError("no compatible shell found (tried explicit shell, $SHELL, bash, sh)")


def run_capture(
    cmd: list[str],
    cwd: Path,
    stdin_text: str | None = None,
    timeout: int | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        input=stdin_text,
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout,
    )
