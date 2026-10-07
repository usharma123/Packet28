#!/usr/bin/env python3
"""Independently validate a Packet28 agent-DX benchmark artifact directory.

The validator does not trust the runner's verdicts. It requires every
contract scenario and check, rechecks every evidence file against its recorded
hash, re-derives each explicit-CLI result from the persisted byte streams and
the repository's fixture manifest, binds binaries and product-test receipts to
the runtime source, and requires every delegated Rust test to have passed in
the supplied product test log. Each failure names the contract that failed and
where its evidence is.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

import agent_dx_contract as contract
import agent_dx_native as native
import agent_dx_reduction as reduction

ROOT = Path(__file__).resolve().parent.parent
RUNTIME_PATHS = ("crates", "Cargo.toml", "Cargo.lock")
NATIVE_WIRE = "scenarios/native_retrieval/mcp/retrieval.wire"
NATIVE_STEPS = "scenarios/native_retrieval/mcp/retrieval.steps.json"
NATIVE_FIXTURE = "scenarios/native_retrieval/fixture"
FRESHNESS_WIRE = "scenarios/source_freshness/mcp/after-edit.wire"
FRESHNESS_FORCED = "scenarios/source_freshness/forced-index.json"
FRESHNESS_EDITED = f"scenarios/source_freshness/edited/{native.FRESHNESS_PATH}"


def sha256_file(path: Path) -> str | None:
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


class Report:
    def __init__(self, artifact_dir: Path):
        self.artifact_dir = artifact_dir
        self.failures: list[str] = []
        self.notes: list[str] = []

    def fail(self, scope: str, statement: str, detail: str, evidence: list[str] | None = None) -> None:
        where = ", ".join(str(self.artifact_dir / item) for item in (evidence or [])) or str(self.artifact_dir / "summary.json")
        self.failures.append(f"FAIL {scope}: {statement}. {detail}. Evidence: {where}")


def git_tree_entry(root: Path, tree: str, path: str) -> str | None:
    done = subprocess.run(["git", "rev-parse", f"{tree}:{path}"], cwd=root, capture_output=True, text=True, check=False)
    return done.stdout.strip() if done.returncode == 0 else None


def runtime_matches(root: Path, tree: str | None, expected: dict) -> tuple[bool, str]:
    if not tree:
        return False, "no source tree recorded"
    differing = [path for path in RUNTIME_PATHS if git_tree_entry(root, tree, path) != expected.get(path)]
    return not differing, f"tree {tree} differs in {differing}" if differing else f"tree {tree} has the same runtime source"


def check_evidence(summary: dict, report: Report) -> None:
    hashes = summary.get("evidence_sha256")
    if not isinstance(hashes, dict) or not hashes:
        report.fail("evidence", "every evidence file is hashed", "summary has no evidence_sha256")
        return
    for rel, digest in hashes.items():
        if sha256_file(report.artifact_dir / rel) != digest:
            report.fail("evidence", "persisted evidence is unchanged", f"{rel} is missing or its bytes differ", [rel])
    invocations_path = report.artifact_dir / "invocations.json"
    if not invocations_path.is_file():
        report.fail("evidence", "every CLI invocation is recorded", "invocations.json is missing")
        return
    invocations = json.loads(invocations_path.read_text())
    if not invocations:
        report.fail("evidence", "every CLI invocation is recorded", "no invocations were recorded", ["invocations.json"])
    for record in invocations:
        if not record.get("argv") or not isinstance(record.get("exit_code"), int):
            report.fail("evidence", "every invocation records argv and exit", f"incomplete record {record.get('label')}", ["invocations.json"])
        for stream in ("stdout", "stderr"):
            item = record.get(stream) or {}
            if hashes.get(item.get("path")) != item.get("sha256"):
                report.fail("evidence", "every invocation keeps complete output",
                            f"{record.get('label')} {stream} is not bound to a hashed file", ["invocations.json"])
    report.notes.append(f"{len(hashes)} evidence files and {len(invocations)} CLI invocations verified")


def check_binaries(summary: dict, report: Report, root: Path, bin_dir: Path | None) -> None:
    binaries = {item.get("name"): item for item in summary.get("binaries", [])}
    for name in ("Packet28", "packet28d"):
        item = binaries.get(name) or {}
        if not item.get("sha256"):
            report.fail("source binding", "the measured binaries are identified", f"{name} has no sha256")
        elif bin_dir is not None and sha256_file(bin_dir / name) != item["sha256"]:
            report.fail("source binding", "the measured binaries are identified", f"{bin_dir / name} does not match the recorded sha256")
    binding = summary.get("binary_binding")
    expected = (summary.get("source") or {}).get("runtime_trees") or {}
    if binding:
        data = json.loads((report.artifact_dir / binding["path"]).read_text())
        bound = {item.get("name"): item.get("sha256") for item in data.get("binaries", [])}
        for name in ("Packet28", "packet28d"):
            if bound.get(name) != (binaries.get(name) or {}).get("sha256"):
                report.fail("source binding", "binaries come from the bound build", f"{name} hash differs from the build receipt", [binding["path"]])
        ok, detail = runtime_matches(root, data.get("runtime_source_tree"), expected)
        if not ok:
            report.fail("source binding", "binaries were built from this runtime source", detail, [binding["path"]])
        else:
            report.notes.append(f"binaries bound by receipt; {detail}")
    else:
        report.notes.append("no external build receipt: binaries are taken as built from this checkout in the same job")
    check_source_status(summary, report)


def check_source_status(summary: dict, report: Report) -> None:
    """Re-parse the recorded `git status -z` receipts; trust no runner-derived path list.

    The canonical gate regenerates a tracked JavaTest cache, so only runtime and
    benchmark sources (contract.RELEVANT_SOURCE_PREFIXES) make a measurement
    unattributable. A change between the start and end receipts means the
    measured source moved during the run.
    """
    hashes = summary.get("evidence_sha256") or {}
    parsed = {}
    for label in ("source", "source_end"):
        record = ((summary.get(label) or {}).get("status_porcelain_z")) or {}
        rel = record.get("path")
        if not rel or hashes.get(rel) != record.get("sha256") or sha256_file(report.artifact_dir / rel) != record.get("sha256"):
            report.fail("source binding", "the measured source state is recorded", f"{label} git status receipt is missing or altered",
                        [rel] if rel else None)
            continue
        parsed[label] = contract.parse_porcelain_z((report.artifact_dir / rel).read_bytes())
        dirty = contract.relevant_dirty(parsed[label])
        if dirty:
            report.fail("source binding", "measurements come from unmodified runtime and benchmark sources",
                        f"{label} modified or untracked: {dirty}", [rel])
    start, end = summary.get("source") or {}, summary.get("source_end") or {}
    if len(parsed) == 2 and ((start.get("commit"), start.get("tree")) != (end.get("commit"), end.get("tree"))
                             or parsed["source"] != parsed["source_end"]):
        report.fail("source binding", "the source did not change while the benchmark ran",
                    f"start {start.get('commit')} {len(parsed['source'])} status entries, "
                    f"end {end.get('commit')} {len(parsed['source_end'])} status entries")


def check_scenarios(summary: dict, report: Report) -> None:
    by_id = {}
    for scenario in summary.get("scenarios", []):
        if scenario.get("id") in by_id:
            report.fail(scenario["id"], "each scenario runs once", "duplicate scenario result")
        by_id[scenario.get("id")] = scenario
    for scenario_id, spec in contract.MEASURED.items():
        scenario = by_id.get(scenario_id)
        if scenario is None:
            report.fail(scenario_id, spec["intent"], "required scenario is missing")
            continue
        checks = {check.get("id"): check for check in scenario.get("checks", [])}
        hashes = summary.get("evidence_sha256") or {}
        for check in scenario.get("checks", []):
            unbound = [item for item in check.get("evidence") or [] if check.get("passed") and item not in hashes]
            if unbound:
                report.fail(f"{scenario_id}/{check.get('id')}", "check evidence is persisted and hashed",
                            f"evidence {unbound} is not in the hash ledger")
        for check_id, statement in spec["checks"].items():
            check = checks.get(check_id)
            if check is None:
                report.fail(f"{scenario_id}/{check_id}", statement, "required check is missing or was skipped",
                            [f"scenarios/{scenario_id}"])
            elif check.get("passed") is not True:
                report.fail(f"{scenario_id}/{check_id}", statement, str(check.get("detail")),
                            check.get("evidence") or [f"scenarios/{scenario_id}"])
        if scenario.get("error"):
            report.fail(scenario_id, spec["intent"], f"scenario stopped: {scenario['error']}", [f"scenarios/{scenario_id}/error.txt"])
    unknown = set(by_id) - set(contract.MEASURED)
    if unknown:
        report.fail("scenarios", "results match the versioned contract", f"unknown scenarios {sorted(unknown)}")


def _evidence_bytes(summary: dict, report: Report, rel: str, scope: str, statement: str) -> bytes | None:
    """Bytes of a hashed evidence file, or None after reporting why it is unusable."""
    digest = (summary.get("evidence_sha256") or {}).get(rel)
    path = report.artifact_dir / rel
    if digest is None or sha256_file(path) != digest:
        report.fail(scope, statement, f"{rel} is missing, unhashed or altered", [rel])
        return None
    return path.read_bytes()


def _scenario(summary: dict, scenario_id: str) -> dict:
    return next((item for item in summary.get("scenarios", []) if item.get("id") == scenario_id), {})


def check_native(summary: dict, report: Report) -> None:
    """Rerun the native retrieval contract on the captured wire and recompute its ledger."""
    scope = "native_retrieval"
    statement = contract.MEASURED[scope]["intent"]
    files = native.fixture_files()
    for rel, data in files.items():
        if _evidence_bytes(summary, report, f"{NATIVE_FIXTURE}/{rel}", scope, "the fixture source is persisted") not in (None, data):
            report.fail(scope, "the benchmark measured the declared fixture", f"{rel} differs from its declared bytes",
                        [f"{NATIVE_FIXTURE}/{rel}"])
    wire = _evidence_bytes(summary, report, NATIVE_WIRE, scope, statement)
    steps_raw = _evidence_bytes(summary, report, NATIVE_STEPS, scope, statement)
    if wire is None or steps_raw is None:
        return
    checks, _ = native.check_native_retrieval(wire, files)
    for check_id, (passed, detail) in checks.items():
        if not passed:
            report.fail(f"{scope}/{check_id}", contract.check_statement(scope, check_id), f"rechecked from the wire: {detail}", [NATIVE_WIRE])
    ledger, errors = native.native_ledger(wire, json.loads(steps_raw).get("steps", []))
    for error in errors:
        report.fail(f"{scope}/ledger_matches_wire", contract.check_statement(scope, "ledger_matches_wire"), error,
                    [NATIVE_WIRE, NATIVE_STEPS])
    reported = _scenario(summary, scope).get("metrics") or {}
    for key, value in native.ledger_metrics(ledger).items():
        if reported.get(key) != value:
            report.fail(f"{scope}/ledger_matches_wire", "reported native metrics match the recorded exchanges",
                        f"{key} reported {json.dumps(reported.get(key))}, recomputed {json.dumps(value)}", [NATIVE_STEPS])
    report.notes.append(f"native retrieval rechecked from {NATIVE_WIRE}: {sum(p for p, _ in checks.values())}/{len(checks)} "
                        f"contract checks, {ledger['required_retrieval']['round_trips']} required retrievals counted")


def check_freshness(summary: dict, report: Report) -> None:
    """Rerun the freshness contract on the captured search wire and forced-index replies."""
    scope = "source_freshness"
    statement = contract.MEASURED[scope]["intent"]
    expected_edit = native.edited_freshness_source(native.fixture_files()[native.FRESHNESS_PATH])
    edited = _evidence_bytes(summary, report, FRESHNESS_EDITED, scope, statement)
    wire = _evidence_bytes(summary, report, FRESHNESS_WIRE, scope, statement)
    forced = _evidence_bytes(summary, report, FRESHNESS_FORCED, scope, statement)
    if edited is None or wire is None or forced is None:
        return
    if edited != expected_edit:
        report.fail(scope, "the benchmark made the declared edit", "edited source differs from the declared edit", [FRESHNESS_EDITED])
    for check_id, (passed, detail) in native.check_freshness(wire, json.loads(forced), expected_edit).items():
        if not passed:
            report.fail(f"{scope}/{check_id}", contract.check_statement(scope, check_id), f"rechecked from the wire: {detail}",
                        [FRESHNESS_WIRE, FRESHNESS_FORCED])
    report.notes.append(f"source freshness rechecked from {FRESHNESS_WIRE} and {FRESHNESS_FORCED}")


def check_reduction(summary: dict, report: Report) -> None:
    manifest = reduction.load_manifest()
    results = {item.get("case"): item for item in summary.get("reduction_cases", [])}
    for case in manifest["cases"]:
        name = case["case"]
        scope = f"explicit_cli_reduction/{name}"
        result = results.get(name)
        if result is None:
            report.fail(scope, "every frozen case is replayed", "case result is missing")
            continue
        streams = {}
        records = result.get("streams") or {}
        for label in reduction.STREAM_LABELS:
            record = records.get(label) or {}
            path = report.artifact_dir / str(record.get("path"))
            if not path.is_file() or sha256_file(path) != record.get("sha256"):
                report.fail(scope, "the four streams are persisted", f"{label} is missing or altered", [str(record.get("path"))])
                continue
            streams[label] = path.read_bytes()
        if len(streams) != len(reduction.STREAM_LABELS):
            continue
        for stream in ("stdout", "stderr"):
            if hashlib.sha256(streams[f"raw_{stream}"]).hexdigest() != case[f"{stream}_sha256"]:
                report.fail(scope, "the reducer received the frozen bytes", f"raw {stream} differs from the manifest hash",
                            [records[f"raw_{stream}"]["path"]])
        if result.get("stub_invocations") != [case["argv"][1:]]:
            report.fail(scope, "Packet28 ran the recorded command exactly once",
                        f"stub invocations {result.get('stub_invocations')!r}", [records["reduced_stdout"]["path"]])
        errors, _, measured = reduction.reduction_errors(case, streams, result.get("reduced_exit_code"))
        for error in errors:
            report.fail(scope, f"{case['contract']} contract ({case['role']})", error, [records["reduced_stdout"]["path"]])
        for key, value in measured.items():
            if result.get(key) != value:
                report.fail(scope, "reported measurements match the persisted streams",
                            f"{key} reported {result.get(key)}, recomputed {value}", [records["reduced_stdout"]["path"]])
    unknown = set(results) - {case["case"] for case in manifest["cases"]}
    if unknown:
        report.fail("explicit_cli_reduction", "results match the fixture manifest", f"unknown cases {sorted(unknown)}")


def parse_test_log(text: str) -> dict[tuple[str, str], str]:
    """Map (binary source, test name) to its outcome from `cargo test` output."""
    outcomes: dict[tuple[str, str], str] = {}
    source = ""
    running = re.compile(r"^\s*Running (?:unittests )?(\S+)")
    result = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b")
    for line in text.splitlines():
        match = running.match(line)
        if match:
            source = match.group(1)
            continue
        match = result.match(line)
        if match:
            outcomes[(source, match.group(1))] = match.group(2)
    return outcomes


def delegated_outcome(outcomes: dict, test: str) -> str | None:
    path, _, name = test.partition("::")
    file_name = Path(path).name
    for (source, full_name), outcome in outcomes.items():
        if "/tests/" in path:
            if Path(source).name == file_name and full_name == name:
                return outcome
        elif source.endswith("src/lib.rs") and full_name.split("::")[0] == Path(path).stem and full_name.endswith(f"::{name}"):
            return outcome
    return None


def check_delegated(summary: dict, report: Report, root: Path) -> None:
    receipt = summary.get("product_tests")
    if not receipt:
        report.fail("delegated", "delegated contracts ran as named product tests", "no product test log was supplied")
        return
    log_path = report.artifact_dir / receipt["log"]["path"]
    if not log_path.is_file():
        report.fail("delegated", "delegated contracts ran as named product tests", "product test log is missing")
        return
    ok, detail = runtime_matches(root, receipt.get("source_tree"), (summary.get("source") or {}).get("runtime_trees") or {})
    if not ok:
        report.fail("delegated", "product tests ran on the measured runtime source", detail, [receipt["log"]["path"]])
    outcomes = parse_test_log(log_path.read_text(errors="replace"))
    passed = 0
    for contract_id, spec in contract.DELEGATED.items():
        for test in spec["tests"]:
            source_file, _, name = test.partition("::")
            if not re.search(rf"\bfn {re.escape(name)}\s*\(", (root / source_file).read_text() if (root / source_file).is_file() else ""):
                report.fail(f"delegated/{contract_id}", spec["intent"], f"{test} no longer exists in the source")
                continue
            outcome = delegated_outcome(outcomes, test)
            if outcome != "ok":
                report.fail(f"delegated/{contract_id}", spec["intent"],
                            f"{test} {'did not run' if outcome is None else outcome} in the product test log", [receipt["log"]["path"]])
            else:
                passed += 1
    report.notes.append(f"{passed} delegated product tests passed in the supplied log ({detail})")


def render(summary: dict, report: Report) -> str:
    lines = ["# Packet28 agent-DX benchmark validation", "",
             f"- Status: `{'failed' if report.failures else 'passed'}`",
             f"- Source: `{(summary.get('source') or {}).get('commit')}`; binaries: "
             + ", ".join(f"`{b['name']}` `{str(b.get('sha256'))[:12]}`" for b in summary.get("binaries", [])), ""]
    if report.failures:
        lines += ["## Failed contracts", ""] + [f"- {item}" for item in report.failures] + [""]
    lines += ["## Checks", ""] + [f"- {item}" for item in report.notes] + [""]
    lines += ["## Diagnostics (reported, never gated)", "",
              "| Scenario | Metric | Value |", "| --- | --- | --- |"]
    for scenario in summary.get("scenarios", []):
        for key, value in (scenario.get("metrics") or {}).items():
            if key != "ledgers":
                lines.append(f"| `{scenario.get('id')}` | {key} | `{json.dumps(value)}` |")
    lines += ["", "| CLI case | Role | Raw B / tok | Visible B / tok | Savings | Contract |", "| --- | --- | ---: | ---: | ---: | --- |"]
    for case in summary.get("reduction_cases", []):
        lines.append(f"| `{case.get('case')}` | {case.get('role')} | {case.get('raw_bytes')} / {case.get('raw_est_tokens')} | "
                     f"{case.get('reduced_bytes')} / {case.get('reduced_est_tokens')} | {case.get('token_reduction_pct')}% | "
                     f"{'held' if case.get('passed') else 'FAILED'} |")
    return "\n".join(lines) + "\n"


def validate(summary: dict, artifact_dir: Path, root: Path = ROOT, bin_dir: Path | None = None) -> Report:
    report = Report(artifact_dir)
    if summary.get("schema") != "packet28.agent_dx_benchmark.v1" or summary.get("contract_version") != contract.SCHEMA_VERSION:
        report.fail("schema", "the artifact uses the current contract", f"schema={summary.get('schema')} version={summary.get('contract_version')}")
        return report
    check_evidence(summary, report)
    check_binaries(summary, report, root, bin_dir)
    check_scenarios(summary, report)
    check_native(summary, report)
    check_freshness(summary, report)
    check_reduction(summary, report)
    check_delegated(summary, report, root)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("summary_path")
    parser.add_argument("--bin-dir", help="Recheck binary hashes against these files")
    parser.add_argument("--markdown-path")
    args = parser.parse_args()
    summary_path = Path(args.summary_path).resolve()
    if not summary_path.is_file():
        print(f"FAIL artifact: the benchmark wrote no summary. Evidence: {summary_path.parent}")
        return 1
    summary = json.loads(summary_path.read_text())
    report = validate(summary, summary_path.parent, ROOT, Path(args.bin_dir).resolve() if args.bin_dir else None)
    markdown = render(summary, report)
    if args.markdown_path:
        Path(args.markdown_path).write_text(markdown)
    sys.stdout.write(markdown)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
