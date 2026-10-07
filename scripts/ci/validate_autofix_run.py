#!/usr/bin/env python3
"""Validate that a Codex autofix run came from trusted default-branch CI."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

ALLOWED_WORKFLOWS = frozenset(
    {
        "Build",
        "Agent DX Benchmark",
        "Context Anomalies",
        "Experiment Manifest",
        "Handoff Readiness",
        "Memory Lint",
        "Reducer Drift",
        "Release",
    }
)
COMMIT_SHA = re.compile(r"[0-9a-f]{40}")


def _required_text(payload: dict[str, Any], field: str) -> str:
    value = payload.get(field)
    if not isinstance(value, str) or not value or any(
        character in value for character in "\r\n"
    ):
        raise ValueError(f"run metadata field {field!r} must be nonempty text")
    return value


def validate_run(
    payload: dict[str, Any], *, repository: str, default_branch: str
) -> dict[str, str]:
    """Return the immutable repair identity after enforcing the trust boundary."""

    if payload.get("status") != "completed" or payload.get("conclusion") != "failure":
        raise ValueError("autofix requires a completed failed run")
    if _required_text(payload, "name") not in ALLOWED_WORKFLOWS:
        raise ValueError("run workflow is not eligible for autofix")
    if _required_text(payload, "head_branch") != default_branch:
        raise ValueError("run did not execute on the repository default branch")

    run_repository = payload.get("repository")
    head_repository = payload.get("head_repository")
    if not isinstance(run_repository, dict) or run_repository.get("full_name") != repository:
        raise ValueError("run belongs to a different repository")
    if not isinstance(head_repository, dict) or head_repository.get("full_name") != repository:
        raise ValueError("run head belongs to an untrusted repository")

    target_ref = _required_text(payload, "head_sha")
    if COMMIT_SHA.fullmatch(target_ref) is None:
        raise ValueError("run head is not an immutable commit SHA")
    return {
        "target_ref": target_ref,
        "run_url": _required_text(payload, "html_url"),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--default-branch", required=True)
    args = parser.parse_args()

    payload = json.loads(args.metadata.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("run metadata must be a JSON object")
    print(
        json.dumps(
            validate_run(
                payload,
                repository=args.repository,
                default_branch=args.default_branch,
            ),
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
