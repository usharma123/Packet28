#!/usr/bin/env python3
"""Explicit CLI reduction checks for the agent-DX workflow benchmark.

Frozen raw command output is replayed through the real `Packet28 <tool> ...`
renderer behind a stub executable placed first on PATH. The stub records every
invocation, so a replay proves that Packet28 ran the recorded command exactly
once and received exactly the frozen bytes. Measurements always cover the
complete visible CLI stdout and stderr, never a reducer summary field.

Each semantic check derives its expectations from the raw input and the
documented reducer contract (docs/command-reduction.md). It does not compare
against a stored copy of an earlier rendering.

The PR-view contract checker and stub replay are adapted from the PR #74
benchmark correction (local commit 5e59f6ef and its unfinished replay helper).
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shlex
import subprocess
import tempfile
from pathlib import Path

from benchmark_common import estimate_tokens

FIXTURE_DIR = Path(__file__).resolve().parent / "benchmark_fixtures" / "agent_dx"
MANIFEST_PATH = FIXTURE_DIR / "manifest.json"

# Documented `gh pr view` preview contract (docs/command-reduction.md).
OMISSION_NOTICE = "[content omitted; use original gh command for full output]"
PR_BODY_MAX_BYTES = 320
PR_BODY_MAX_LINES = 8
FILTERED_BODY_PREFIXES = ("![", "<!--", "<img", "[![")
PR_IDENTITY_KEYS = {"title", "state", "number", "author", "url"}
# A failing cargo test keeps its name and the first lines of its own output.
CARGO_FAILURE_PREVIEW_LINES = 5

SCRUBBED_ENV = (
    "GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST",
)
INVOCATION_MARK = "--packet28-stub-invocation--"
STUB = """#!/bin/sh
{ printf '%s\\n' '""" + INVOCATION_MARK + """'; for arg in "$@"; do printf '%s\\n' "$arg"; done; } >> "$PACKET28_BENCH_STUB_INVOCATION"
/bin/cat "$PACKET28_BENCH_STUB_STDOUT"
/bin/cat "$PACKET28_BENCH_STUB_STDERR" >&2
exit "$PACKET28_BENCH_STUB_EXIT"
"""
STREAM_LABELS = ("raw_stdout", "raw_stderr", "reduced_stdout", "reduced_stderr")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load_manifest(path: Path = MANIFEST_PATH) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def read_fixture_inputs(case: dict, fixture_dir: Path = FIXTURE_DIR) -> tuple[bytes, bytes, list[str]]:
    """Read frozen raw streams and verify them against their manifest hashes."""
    errors = []
    streams = []
    for stream in ("stdout", "stderr"):
        name = case.get(stream)
        data = b""
        if name:
            path = fixture_dir / name
            if not path.is_file():
                errors.append(f"frozen fixture file is missing: {path}")
            else:
                data = path.read_bytes()
        if sha256_bytes(data) != case.get(f"{stream}_sha256"):
            errors.append(f"frozen {stream} bytes differ from the manifest hash")
        streams.append(data)
    return streams[0], streams[1], errors


def run_with_stub(
    cwd: Path,
    command: list[str],
    program: str,
    expected_args: list[str],
    raw_stdout: bytes,
    raw_stderr: bytes,
    raw_exit: int,
    timeout: float = 120,
) -> tuple[subprocess.CompletedProcess | None, list[str], list[list[str]]]:
    """Run `command` with a stub `program` first on PATH replaying one response.

    The stub must be invoked exactly once with `expected_args`; anything else
    means the measurement did not render the frozen input.
    """
    errors = []
    completed = None
    invocations: list[list[str]] = []
    with tempfile.TemporaryDirectory(prefix="packet28-agent-dx-stub-") as runtime:
        runtime_dir = Path(runtime)
        stub_dir = runtime_dir / "bin"
        stub_dir.mkdir()
        stub = stub_dir / program
        stub.write_text(STUB, encoding="utf-8")
        stub.chmod(0o755)
        stdout_path = runtime_dir / "stdout"
        stderr_path = runtime_dir / "stderr"
        stdout_path.write_bytes(raw_stdout)
        stderr_path.write_bytes(raw_stderr)
        invocation = runtime_dir / "invocation"
        env = {key: value for key, value in os.environ.items() if key not in SCRUBBED_ENV}
        env.update({
            "PATH": f"{stub_dir}{os.pathsep}{os.environ.get('PATH', '')}",
            "HOME": str(runtime_dir),
            "GH_CONFIG_DIR": str(runtime_dir / "gh-config"),
            "NO_COLOR": "1",
            "PACKET28_BENCH_STUB_STDOUT": str(stdout_path),
            "PACKET28_BENCH_STUB_STDERR": str(stderr_path),
            "PACKET28_BENCH_STUB_EXIT": str(raw_exit),
            "PACKET28_BENCH_STUB_INVOCATION": str(invocation),
        })
        try:
            completed = subprocess.run(
                command, cwd=str(cwd), env=env, capture_output=True, timeout=timeout, check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            errors.append(f"explicit CLI replay failed to run: {exc}")
        if invocation.is_file():
            for line in invocation.read_text(encoding="utf-8").splitlines():
                if line == INVOCATION_MARK:
                    invocations.append([])
                elif invocations:
                    invocations[-1].append(line)
    if completed is not None:
        if not invocations:
            errors.append(f"stub {program} was not invoked; the output did not come from the frozen input")
        elif invocations != [expected_args]:
            errors.append(f"Packet28 invoked {program} {invocations!r}, expected exactly once with {expected_args!r}")
    return completed, errors, invocations


def _tab_fields(header) -> dict[str, str]:
    fields = {}
    for line in header:
        key, separator, value = line.partition(":\t")
        if separator and key not in fields:
            fields[key] = value
    return fields


def _pr_view_errors(raw_stdout: str, reduced_stdout: str) -> tuple[list[str], dict]:
    lines = raw_stdout.splitlines()
    separator = next((index for index, line in enumerate(lines) if line.strip() == "--"), None)
    if separator is None or not lines[0].startswith("title:\t"):
        return ["raw gh pr view output has no recognized metadata header"], {}
    header = lines[:separator]
    fields = _tab_fields(line.strip() for line in header)
    if any(not fields.get(key) for key in ("title", "state", "number")):
        return ["raw gh pr view header lacks title, state, or number"], {}
    author = fields.get("author")
    expected_identity = (
        f"gh pr view: PR #{fields['number']} {fields['state']} by {author} - {fields['title']}"
        if author
        else f"gh pr view: PR #{fields['number']} {fields['state']} - {fields['title']}"
    )
    filtered = False
    body_lines = []
    for line in lines[separator + 1:]:
        trimmed = line.strip()
        if trimmed.startswith(FILTERED_BODY_PREFIXES):
            filtered = True
            continue
        body_lines.append(trimmed)
    full_body = "\n".join(body_lines)
    metadata_omitted = any(
        value.strip() and key not in PR_IDENTITY_KEYS for key, value in _tab_fields(header).items()
    )

    errors = []
    reduced_lines = reduced_stdout.split("\n")
    if reduced_lines and reduced_lines[-1] == "":
        reduced_lines.pop()
    if not reduced_lines or reduced_lines[0] != expected_identity:
        errors.append(f"identity line differs from {expected_identity!r}")
    rest = reduced_lines[1:]
    url = fields.get("url")
    if url:
        if not rest or rest[0] != f"url: {url}":
            errors.append(f"URL line 'url: {url}' is missing after the identity line")
        else:
            rest = rest[1:]
    has_notice = bool(rest) and rest[-1] == OMISSION_NOTICE
    rendered_body = "\n".join(rest[:-1] if has_notice else rest)
    rendered_body_bytes = len(rendered_body.encode("utf-8"))
    if full_body.strip() and not rendered_body.strip():
        errors.append("body preview is missing")
    if not full_body.startswith(rendered_body):
        errors.append("body preview is not a prefix of the original body")
    if rendered_body_bytes > PR_BODY_MAX_BYTES:
        errors.append(f"body preview has {rendered_body_bytes} bytes, above {PR_BODY_MAX_BYTES}")
    if rendered_body and rendered_body.count("\n") + 1 > PR_BODY_MAX_LINES:
        errors.append(f"body preview has more than {PR_BODY_MAX_LINES} lines")
    # The CLI trims trailing whitespace from the preview, so a complete body
    # can render without its trailing blank lines and still omit nothing.
    known_omission = filtered or metadata_omitted
    if not has_notice and (known_omission or rendered_body.rstrip() != full_body.rstrip()):
        errors.append("omission notice is missing although content was omitted")
    if has_notice and not known_omission and rendered_body == full_body:
        errors.append("omission notice is present although nothing was omitted")
    budget = len(expected_identity.encode("utf-8")) + 1 + PR_BODY_MAX_BYTES + 1 + len(OMISSION_NOTICE) + 1
    if url:
        budget += len(f"url: {url}".encode("utf-8")) + 1
    reduced_bytes = len(reduced_stdout.encode("utf-8"))
    if reduced_bytes > budget:
        errors.append(f"visible output has {reduced_bytes} bytes, above its derived {budget}-byte budget")
    return errors, {
        "rendered_body_bytes": rendered_body_bytes,
        "omission_notice": has_notice,
        "derived_output_budget_bytes": budget,
    }


def _cargo_failure_blocks(raw: str) -> dict[str, list[str]]:
    """Map each failing test to its own captured output lines."""
    blocks: dict[str, list[str]] = {}
    current = None
    for line in raw.splitlines():
        trimmed = line.strip()
        match = re.fullmatch(r"---- (.+) stdout ----", trimmed)
        if match:
            current = match.group(1)
            blocks[current] = []
            continue
        if current is not None:
            if trimmed == "failures:" or trimmed.startswith("---- "):
                current = None
                continue
            blocks[current].append(trimmed)
    return blocks


def _cargo_test_errors(raw: str, reduced_stdout: str, raw_exit: int) -> tuple[list[str], dict]:
    passed = failed = 0
    results = [line.strip() for line in raw.splitlines() if line.strip().startswith("test result:")]
    if not results:
        return ["raw cargo test output has no `test result:` line"], {}
    for line in results:
        for segment in line.split(";"):
            words = segment.split()
            if len(words) >= 2 and words[-1] == "passed":
                passed += int(words[-2])
            if len(words) >= 2 and words[-1] == "failed":
                failed += int(words[-2])
    errors = []
    lines = reduced_stdout.splitlines()
    if raw_exit != 0 or failed:
        expected = f"cargo test reported {passed} passed and {failed} failed"
    else:
        expected = f"cargo test passed ({passed} tests)"
    if not lines or lines[0] != expected:
        errors.append(f"first line differs from the derived count line {expected!r}")
    blocks = _cargo_failure_blocks(raw)
    if failed and len(blocks) != failed:
        errors.append(f"raw output names {len(blocks)} failure blocks for {failed} failures")
    budget = len(expected.encode("utf-8")) + 1
    for name, block in blocks.items():
        if f"FAIL {name}" not in lines:
            errors.append(f"failing test {name} is not named")
        panic = next((line for line in block if " panicked at " in line), None)
        if panic is None:
            errors.append(f"raw failure block for {name} has no panic location")
        elif panic not in lines:
            errors.append(f"panic location for {name} was not kept: {panic!r}")
        preview = block[:CARGO_FAILURE_PREVIEW_LINES]
        # Name line, its first output lines, and the blank separator between failures.
        budget += len(f"FAIL {name}".encode("utf-8")) + 2 + sum(len(line.encode("utf-8")) + 1 for line in preview)
    passing_noise = [line for line in lines if re.fullmatch(r"test \S+ \.\.\. ok", line.strip())]
    if passing_noise:
        errors.append(f"{len(passing_noise)} passing-test lines were kept")
    reduced_bytes = len(reduced_stdout.encode("utf-8"))
    if reduced_bytes > budget:
        errors.append(f"visible output has {reduced_bytes} bytes, above its derived {budget}-byte budget")
    return errors, {
        "passed": passed,
        "failed": failed,
        "failing_tests": sorted(blocks),
        "derived_output_budget_bytes": budget,
    }


def _gh_list_errors(raw: str, reduced_stdout: str, label: str, noun: str) -> tuple[list[str], dict]:
    rows = [line for line in raw.splitlines() if line.strip()]
    if not rows:
        return [f"raw {label} output has no rows"], {}
    first = rows[0].split("\t")
    lines = reduced_stdout.splitlines()
    head = lines[0] if lines else ""
    errors = []
    if not head.startswith(f"{label}: {len(rows)} {noun}(s)"):
        errors.append(f"first line does not report the {len(rows)} raw {noun} rows")
    for field in first[:2]:
        if field and field not in head:
            errors.append(f"first {noun} field {field!r} is missing")
    return errors, {"rows": len(rows), "first_row_fields": first[:4]}


def _run_view_sections(raw: str) -> dict[str, list[str]]:
    sections: dict[str, list[str]] = {"": []}
    current = ""
    for line in raw.splitlines():
        if line.strip() in {"JOBS", "ANNOTATIONS"} and line == line.strip():
            current = line.strip()
            sections[current] = []
            continue
        sections[current].append(line)
    return sections


def _gh_run_view_errors(raw: str, reduced_stdout: str) -> tuple[list[str], dict]:
    """`gh run view` keeps the run identity, true job/annotation counts, failed jobs and failed steps.

    Jobs are unindented entries of the JOBS section (indented lines are their
    steps). Annotations are unindented status-marked entries of the
    ANNOTATIONS section (the following `job: path#line` lines locate them).
    """
    sections = _run_view_sections(raw)
    header = next((line.strip() for line in sections[""] if line.strip()), "")
    title = header[1:].split("·")[0].strip() if header[:1] in {"X", "✓", "*", "-", "!"} else ""
    jobs = [line for line in sections.get("JOBS", []) if line.strip() and not line[:1].isspace()]
    marker = re.compile(r"^[X✓!*-] \S")
    annotations = [line for line in sections.get("ANNOTATIONS", []) if marker.match(line)]
    failed_jobs = [line.strip() for line in jobs if line.startswith("X ")]
    failed_steps = [line.strip() for line in sections.get("JOBS", []) if line[:1].isspace() and line.strip().startswith("X ")]
    failed_annotations = [line[2:].strip() for line in annotations if line.startswith("X ")]
    expected = (
        f"gh run view: {title} ({len(jobs)} job{'' if len(jobs) == 1 else 's'}, "
        f"{len(annotations)} annotation{'' if len(annotations) == 1 else 's'})"
    )
    lines = [line.strip() for line in reduced_stdout.splitlines()]
    errors = []
    if not title or not jobs:
        errors.append("raw gh run view output has no recognized title or JOBS section")
    elif not lines or lines[0] != expected:
        errors.append(f"summary line {lines[0] if lines else ''!r} differs from the derived {expected!r}")
    for fact in [*failed_jobs, *failed_steps]:
        if not any(fact in line for line in lines):
            errors.append(f"failure fact {fact!r} is missing")
    # No documented contract promises annotation text in the preview; report it.
    kept = [fact for fact in failed_annotations if any(fact in line for line in lines)]
    return errors, {
        "failed_annotations_kept": kept,
        "jobs": len(jobs),
        "annotations": len(annotations),
        "failed_jobs": failed_jobs,
        "failed_steps": failed_steps,
        "failed_annotations": failed_annotations,
    }


def _facts_errors(raw: str, visible: str, facts: list[str]) -> tuple[list[str], dict]:
    errors = []
    if not facts:
        errors.append("diagnostic case declares no required facts")
    for fact in facts:
        if fact not in raw:
            errors.append(f"declared fact {fact!r} does not occur in the raw input")
        elif fact not in visible:
            errors.append(f"required fact {fact!r} is missing from the visible output")
    return errors, {"required_facts": facts}


def check_semantics(
    contract: str,
    raw_stdout: bytes,
    raw_stderr: bytes,
    raw_exit: int,
    reduced_stdout: bytes,
    reduced_stderr: bytes,
    reduced_exit: int | None,
    facts: list[str] | None = None,
) -> dict:
    """Check visible explicit-CLI output against the raw output it reduced."""
    errors = []
    details: dict = {}
    if reduced_exit != raw_exit:
        errors.append(f"explicit CLI exit {reduced_exit} differs from the command exit {raw_exit}")
    try:
        raw_out = raw_stdout.decode("utf-8")
        raw_err = raw_stderr.decode("utf-8")
        red_out = reduced_stdout.decode("utf-8")
        red_err = reduced_stderr.decode("utf-8")
    except UnicodeDecodeError as exc:
        return {"contract": contract, "passed": False, "errors": [*errors, f"non-UTF-8 stream: {exc}"]}
    if contract == "gh_pr_view":
        if raw_exit != 0:
            if f"{raw_out}{raw_err}" not in f"{red_out}{red_err}":
                errors.append("failed gh pr view did not keep its complete stdout and stderr")
        else:
            if red_err:
                errors.append("explicit CLI wrote unexpected stderr")
            view_errors, details = _pr_view_errors(raw_out, red_out)
            errors.extend(view_errors)
    elif contract == "cargo_test":
        if red_err:
            errors.append("explicit CLI wrote unexpected stderr")
        test_errors, details = _cargo_test_errors(f"{raw_out}\n{raw_err}", red_out, raw_exit)
        errors.extend(test_errors)
    elif contract in {"gh_pr_list", "gh_run_list", "gh_run_view"}:
        if red_err:
            errors.append("explicit CLI wrote unexpected stderr")
        if contract == "gh_run_view":
            gh_errors, details = _gh_run_view_errors(raw_out, red_out)
        elif contract == "gh_pr_list":
            gh_errors, details = _gh_list_errors(raw_out, red_out, "gh pr list", "PR")
        else:
            gh_errors, details = _gh_list_errors(raw_out, red_out, "gh run list", "run")
        errors.extend(gh_errors)
    elif contract == "diagnostic_facts":
        fact_errors, details = _facts_errors(f"{raw_out}{raw_err}", f"{red_out}{red_err}", facts or [])
        errors.extend(fact_errors)
    else:
        errors.append(f"no reduction contract named {contract!r}")
    return {"contract": contract, "passed": not errors, "errors": errors, **details}


def measurement(raw_visible: bytes, reduced_visible: bytes) -> dict:
    raw_tokens = estimate_tokens(raw_visible.decode("utf-8", errors="replace"))
    reduced_tokens = estimate_tokens(reduced_visible.decode("utf-8", errors="replace"))
    return {
        "raw_bytes": len(raw_visible),
        "raw_est_tokens": raw_tokens,
        "reduced_bytes": len(reduced_visible),
        "reduced_est_tokens": reduced_tokens,
        "token_reduction_pct": round(100.0 * (raw_tokens - reduced_tokens) / raw_tokens, 1) if raw_tokens else 0.0,
    }


def reduction_errors(case: dict, streams: dict[str, bytes], reduced_exit: int | None) -> tuple[list[str], dict, dict]:
    """Return contract errors, semantic details and measurement for one case.

    `verbose` cases must also produce strictly smaller visible output than the
    raw command; `correctness` cases may expand (a short failure keeps every
    byte plus a summary line).
    """
    semantics = check_semantics(
        case["contract"], streams["raw_stdout"], streams["raw_stderr"], case["exit_code"],
        streams["reduced_stdout"], streams["reduced_stderr"], reduced_exit, case.get("facts"),
    )
    measured = measurement(
        streams["raw_stdout"] + streams["raw_stderr"],
        streams["reduced_stdout"] + streams["reduced_stderr"],
    )
    errors = list(semantics["errors"])
    if case["role"] == "verbose" and (
        measured["reduced_bytes"] >= measured["raw_bytes"]
        or measured["reduced_est_tokens"] >= measured["raw_est_tokens"]
    ):
        errors.append(
            f"verbose input was not reduced: {measured['raw_bytes']} raw bytes / "
            f"{measured['raw_est_tokens']} tokens -> {measured['reduced_bytes']} visible bytes / "
            f"{measured['reduced_est_tokens']} tokens"
        )
    return errors, semantics, measured


def replay_case(cwd: Path, packet28: Path, case: dict, fixture_dir: Path = FIXTURE_DIR) -> tuple[dict, dict[str, bytes]]:
    """Replay one frozen case and return its result plus all four streams."""
    raw_stdout, raw_stderr, errors = read_fixture_inputs(case, fixture_dir)
    streams = {"raw_stdout": raw_stdout, "raw_stderr": raw_stderr, "reduced_stdout": b"", "reduced_stderr": b""}
    reduced_exit = None
    invocations: list[list[str]] = []
    argv = case["argv"]
    if not errors:
        completed, replay_errors, invocations = run_with_stub(
            cwd, [str(packet28), *argv], argv[0], argv[1:], raw_stdout, raw_stderr, case["exit_code"],
        )
        errors.extend(replay_errors)
        if completed is not None:
            streams["reduced_stdout"] = completed.stdout
            streams["reduced_stderr"] = completed.stderr
            reduced_exit = completed.returncode
    contract_errors, semantics, measured = reduction_errors(case, streams, reduced_exit)
    errors.extend(contract_errors)
    result = {
        "case": case["case"],
        "role": case["role"],
        "contract": case["contract"],
        "command": shlex.join(argv),
        "explicit_command": shlex.join(["Packet28", *argv]),
        "raw_exit_code": case["exit_code"],
        "reduced_exit_code": reduced_exit,
        "stub_invocations": invocations,
        "fixture_sha256": {"stdout": case.get("stdout_sha256"), "stderr": case.get("stderr_sha256")},
        "provenance": case.get("provenance", {}),
        "semantics": semantics,
        **measured,
        "passed": not errors,
        "errors": errors,
    }
    return result, streams
