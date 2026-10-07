#!/usr/bin/env python3
"""Select expensive PR checks and validate the aggregate job result."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import PurePosixPath
import subprocess
import sys


REQUIRED_JOBS = ("changes", "policy", "lint", "tests", "msrv", "audit")
OPTIONAL_JOBS = ("dependencies", "packages")


def select(paths: list[str]) -> dict[str, bool]:
    checks = dict.fromkeys(OPTIONAL_JOBS, False)
    for path in paths:
        name = PurePosixPath(path).name
        # Include deleted files and both sides of renames (git --no-renames).
        graph = (
            name in {"Cargo.toml", "Cargo.lock", "Cargo.direct-minimal.lock"}
            or path.startswith((".cargo/", "rust-toolchain", "scripts/", ".github/"))
            or "direct-minimum" in path
        )
        checks["dependencies"] |= graph
        checks["packages"] |= (
            graph
            or name == "build.rs"
            or path.startswith(("npm/", "package/"))
            or name in {"package.json", "package-lock.json", ".npmignore"}
            or name.startswith(("LICENSE", "COPYING"))
        )
    return checks


def results_ok(needs: dict) -> bool:
    if any(needs.get(job, {}).get("result") != "success" for job in REQUIRED_JOBS):
        return False
    outputs = needs["changes"].get("outputs", {})
    for job in OPTIONAL_JOBS:
        selected = outputs.get(job)
        if selected not in {"true", "false"}:
            return False
        allowed = {"success"} if selected == "true" else {"success", "skipped"}
        if needs.get(job, {}).get("result") not in allowed:
            return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-results", action="store_true")
    args = parser.parse_args()
    if args.check_results:
        needs = json.loads(os.environ["NEEDS_JSON"])
        for job, detail in needs.items():
            print(f"{job}: {detail.get('result')}")
        return 0 if results_ok(needs) else 1

    checks = dict.fromkeys(OPTIONAL_JOBS, True)
    if os.environ.get("GITHUB_EVENT_NAME") == "pull_request":
        # Checkout is the synthetic merge commit, with both parents fetched.
        # Compare its base parent to the merge result, never just the last PR
        # commit. On missing history, run everything rather than skip checks.
        try:
            result = subprocess.run(
                ["git", "diff", "--name-only", "--no-renames", "-z", "HEAD^1", "HEAD"],
                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            paths = result.stdout.decode("utf-8", errors="surrogateescape").split("\0")
            checks = select([path for path in paths if path])
        except subprocess.CalledProcessError:
            print("Cannot inspect the PR merge diff; running all checks.", file=sys.stderr)
    lines = [f"{job}={str(enabled).lower()}" for job, enabled in checks.items()]
    print("\n".join(lines))
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
