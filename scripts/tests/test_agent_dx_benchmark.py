"""Negative controls for the agent-DX benchmark contract, checkers and validator.

These prove that broken semantics, missing cases, skipped checks, tampered
evidence and stale product-test receipts fail. They need no Packet28 binary;
the workflow run itself exercises the real binaries.
"""

import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import types
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))
import agent_dx_contract as contract
import agent_dx_native as native
import agent_dx_reduction as reduction
import benchmark_agent_dx as runner
import validate_agent_dx_benchmark as validator
sys.path.pop(0)

# Authentic wire captured by the benchmark on the fixed product (run r2/run2).
NATIVE_FIXTURES = SCRIPTS / "tests" / "fixtures" / "agent_dx_native"
NATIVE_WIRE = (NATIVE_FIXTURES / "retrieval.wire").read_bytes()
NATIVE_STEPS = json.loads((NATIVE_FIXTURES / "retrieval.steps.json").read_text())["steps"]
FRESH_WIRE = (NATIVE_FIXTURES / "after-edit.wire").read_bytes()
FRESH_FORCED = json.loads((NATIVE_FIXTURES / "forced-index.json").read_text())

# Visible output of the real renderer at source 6d92eb0f for two frozen cases.
GOOD_PR_VIEW = (
    "gh pr view: PR #71 OPEN by usharma123 - feat: integrate native Codex lifecycle continuation hooks\n"
    "url: https://github.com/usharma123/Packet28/pull/71\n## Problem and behavior\n\nCodex setup previously "
    "wrote MCP and instructions but did not install native lifecycle hooks. `hook codex` also attributed events "
    "and fresh session task IDs to Claude. Codex setup now writes project `.codex/hooks.json`, keeps user handlers "
    "and unknown events, and replaces only exactly generated\n"
    "[content omitted; use original gh command for full output]\n"
)
GOOD_CARGO = (
    "cargo test reported 37 passed and 1 failed\nFAIL tests::discount_rounds_to_nearest_cent\n\n"
    "thread 'tests::discount_rounds_to_nearest_cent' (18967413) panicked at src/lib.rs:233:9:\n"
    "assertion `left == right` failed: 15% off 999 cents\nleft: 850\nright: 849\n"
)


def case(name):
    return next(c for c in reduction.load_manifest()["cases"] if c["case"] == name)


def streams_for(name, reduced_stdout, reduced_stderr=b""):
    raw_stdout, raw_stderr, errors = reduction.read_fixture_inputs(case(name))
    assert not errors, errors
    return {"raw_stdout": raw_stdout, "raw_stderr": raw_stderr,
            "reduced_stdout": reduced_stdout, "reduced_stderr": reduced_stderr}


class FixtureManifestTests(unittest.TestCase):
    def test_every_fixture_matches_its_hash_and_declares_provenance(self):
        manifest = reduction.load_manifest()
        names = [c["case"] for c in manifest["cases"]]
        self.assertEqual(len(names), len(set(names)))
        for item in manifest["cases"]:
            with self.subTest(case=item["case"]):
                _, _, errors = reduction.read_fixture_inputs(item)
                self.assertEqual(errors, [])
                self.assertIn(item["role"], {"verbose", "correctness"})
                self.assertIn(item["provenance"]["kind"], {"captured", "reconstruction", "synthetic"})

    def test_all_four_github_command_shapes_are_required(self):
        contracts = {c["contract"] for c in reduction.load_manifest()["cases"]}
        self.assertTrue({"gh_pr_view", "gh_pr_list", "gh_run_list", "gh_run_view", "cargo_test"} <= contracts)

    def test_altered_fixture_bytes_fail(self):
        altered = dict(case("gh_pr_list_5"), stdout_sha256="0" * 64)
        _, _, errors = reduction.read_fixture_inputs(altered)
        self.assertTrue(any("differ from the manifest" in e for e in errors))


class ReductionContractTests(unittest.TestCase):
    def errors(self, name, reduced_stdout, reduced_stderr=b"", exit_code=None):
        item = case(name)
        exit_code = item["exit_code"] if exit_code is None else exit_code
        errors, _, _ = reduction.reduction_errors(item, streams_for(name, reduced_stdout, reduced_stderr), exit_code)
        return errors

    def test_real_renderings_hold(self):
        self.assertEqual(self.errors("gh_pr_view_long_description", GOOD_PR_VIEW.encode()), [])
        self.assertEqual(self.errors("cargo_test_one_failure", GOOD_CARGO.encode()), [])

    def test_pr_view_broken_semantics_fail(self):
        lines = GOOD_PR_VIEW.splitlines(keepends=True)
        mutations = {
            "summary only": lines[0],
            "missing URL": "".join(lines[:1] + lines[2:]),
            "missing notice": "".join(lines[:-1]),
            "expanded body": "".join(lines[:-1]) + "x" * 400 + "\n" + lines[-1],
            "rewritten body": GOOD_PR_VIEW.replace("Codex setup previously", "Codex setup formerly"),
        }
        for label, text in mutations.items():
            with self.subTest(label):
                self.assertNotEqual(self.errors("gh_pr_view_long_description", text.encode()), [])

    def test_passthrough_and_exit_or_stderr_changes_fail(self):
        raw = streams_for("gh_pr_view_long_description", b"")["raw_stdout"]
        self.assertTrue(any("not reduced" in e for e in self.errors("gh_pr_view_long_description", raw)))
        self.assertTrue(any("exit" in e for e in self.errors("gh_pr_view_long_description", GOOD_PR_VIEW.encode(), exit_code=0 + 1)))
        self.assertNotEqual(self.errors("gh_pr_view_long_description", GOOD_PR_VIEW.encode(), b"warning\n"), [])

    def test_failed_read_must_keep_everything_and_its_exit(self):
        raw = streams_for("gh_pr_view_failed_exit7", b"")
        kept = raw["raw_stdout"] + raw["raw_stderr"]
        self.assertEqual(self.errors("gh_pr_view_failed_exit7", b"gh pr view failed\n" + kept), [])
        self.assertNotEqual(self.errors("gh_pr_view_failed_exit7", b"gh pr view failed\n" + kept[:-40]), [])
        self.assertNotEqual(self.errors("gh_pr_view_failed_exit7", b"gh pr view failed\n" + kept, exit_code=0), [])

    def test_cargo_failure_facts_and_noise_are_checked(self):
        mutations = {
            "wrong counts": GOOD_CARGO.replace("37 passed", "38 passed"),
            "missing failing test": GOOD_CARGO.replace("FAIL tests::discount_rounds_to_nearest_cent\n", ""),
            "missing panic location": GOOD_CARGO.replace("panicked at src/lib.rs:233:9", "panicked"),
            "passing noise kept": GOOD_CARGO + "test tests::total_of_01_items_matches_sum ... ok\n",
            "assertion deleted": GOOD_CARGO.replace("assertion `left == right` failed: 15% off 999 cents\n", ""),
            "compared values deleted": GOOD_CARGO.replace("left: 850\nright: 849\n", ""),
            "assertion and values deleted": GOOD_CARGO.split("assertion")[0],
        }
        for label, text in mutations.items():
            with self.subTest(label):
                self.assertNotEqual(self.errors("cargo_test_one_failure", text.encode()), [])

    def test_cargo_failure_evidence_ignores_spacing_but_not_content(self):
        # The raw block indents the compared values; keeping that indentation is fine.
        respaced = GOOD_CARGO.replace("left: 850", "  left: 850").replace("right: 849", " right: 849")
        self.assertEqual(self.errors("cargo_test_one_failure", respaced.encode()), [])
        errors = self.errors("cargo_test_one_failure", GOOD_CARGO.replace("right: 849", "right: 848").encode())
        self.assertTrue(any("right: 849" in e for e in errors), errors)

    def test_run_view_counts_are_derived_from_raw_not_the_reducer(self):
        bad = ("gh run view: fix/scope-closure-registry-recovery Hook Benchmark Suite usharma123/Packet28#74 "
               "(19 jobs, 6 annotations)\nX benchmark in 4m24s (ID 112613078067)\nX Validate hook benchmark thresholds\n"
               "Process completed with exit code 1.\n")
        errors = self.errors("gh_run_view_failed_benchmark_run", bad.encode())
        self.assertTrue(any("(1 job, 2 annotations)" in e for e in errors), errors)
        good = bad.replace("(19 jobs, 6 annotations)", "(1 job, 2 annotations)")
        self.assertEqual(self.errors("gh_run_view_failed_benchmark_run", good.encode()), [])
        self.assertNotEqual(self.errors("gh_run_view_failed_benchmark_run",
                                        good.replace("X Validate hook benchmark thresholds\n", "").encode()), [])
        self.assertEqual(self.errors("gh_run_view_failed_benchmark_run",
                                     good.replace("Process completed with exit code 1.\n", "").encode()), [])

    def test_declared_facts_must_come_from_raw_input(self):
        item = dict(case("pytest_one_failure"), facts=["invented fact"])
        errors, _, _ = reduction.reduction_errors(item, streams_for("pytest_one_failure", b"invented fact\n"), 1)
        self.assertTrue(any("does not occur in the raw input" in e for e in errors))

    def test_short_inputs_may_expand_but_verbose_inputs_may_not(self):
        raw = streams_for("ruff_two_errors", b"")
        expanded = b"[FAIL] ruff check src\n" + raw["raw_stdout"]
        self.assertEqual(self.errors("ruff_two_errors", expanded), [])


class HookAuthorityTests(unittest.TestCase):
    def test_nested_authority_fields_are_found(self):
        self.assertEqual(runner._authority_keys({"hookSpecificOutput": {"updatedInput": {"command": "x"}}}), ["updatedInput"])
        self.assertEqual(runner._authority_keys([{"permissionDecision": "allow"}]), ["permissionDecision"])
        self.assertEqual(runner._authority_keys({"hookSpecificOutput": {"hookEventName": "PreToolUse"}}), [])


def rewrite_response(wire, request_id, change):
    """Apply `change(structured)` to one response of a captured wire."""
    out = []
    for line in wire.splitlines(keepends=True):
        message = json.loads(line[2:])
        if line.startswith(b"< ") and message.get("id") == request_id:
            change(message["result"]["structuredContent"])
            line = b"< " + json.dumps(message).encode() + b"\n"
        out.append(line)
    return b"".join(out)


def drop_exchange(wire, request_id):
    return b"".join(line for line in wire.splitlines(keepends=True) if json.loads(line[2:]).get("id") != request_id)


SEARCH, GLOB, SEARCH_FETCH, READ = 2, 3, 4, 5


class NativeRetrievalContractTests(unittest.TestCase):
    """Captured-response mutations the R1 runner and validator accepted."""

    def checks(self, wire):
        return native.check_native_retrieval(wire, native.fixture_files())[0]

    def failed(self, wire):
        return {key: detail for key, (passed, detail) in self.checks(wire).items() if not passed}

    def test_authentic_capture_passes_against_source_derived_facts(self):
        self.assertEqual(self.failed(NATIVE_WIRE), {})
        self.assertEqual(len(native.expected_matches(native.fixture_files(), native.SEARCH_QUERY)), 32)
        ledger, errors = native.native_ledger(NATIVE_WIRE, NATIVE_STEPS)
        self.assertEqual(errors, [])
        self.assertEqual(ledger["required_retrieval"]["round_trips"], 2)

    def test_missing_match_with_consistent_self_reported_counts_fails(self):
        def drop(payload):
            lines = payload["content"].split("\n")
            payload["content"] = "\n".join(line for line in lines if "region_07.rs:4:" not in line)
            payload["match_count"] = payload["returned_match_count"] = 31
        wire = rewrite_response(rewrite_response(NATIVE_WIRE, SEARCH_FETCH, drop), SEARCH,
                                lambda payload: payload.update(match_count=31))
        failed = self.failed(wire)
        self.assertIn("1 source matches missing", failed["fetched_matches_equal_source"])
        self.assertIn("source has 32 matches", failed["search_returns_fetchable_artifact"])

    def test_duplicated_match_fails(self):
        def duplicate(payload):
            payload["content"] += "\n" + payload["content"].split("\n")[0]
        failed = self.failed(rewrite_response(NATIVE_WIRE, SEARCH_FETCH, duplicate))
        self.assertIn("extra or duplicated", failed["fetched_matches_equal_source"])

    def test_full_body_in_slim_and_extra_diagnostics_fail(self):
        full = json.loads(NATIVE_WIRE.splitlines()[SEARCH_FETCH * 2 - 1][2:])["result"]["structuredContent"]["content"]
        failed = self.failed(rewrite_response(NATIVE_WIRE, SEARCH, lambda payload: payload.update(
            full_content=full * 3, diagnostics=[f"warning {n}" for n in range(5)])))
        detail = failed["search_returns_fetchable_artifact"]
        self.assertIn("undocumented fields ['full_content']", detail)
        self.assertIn("diagnostics has 5 entries", detail)
        self.assertIn("is not smaller", detail)
        # A long preview in a documented field is caught too.
        failed = self.failed(rewrite_response(NATIVE_WIRE, SEARCH, lambda payload: payload.update(compact_preview=full)))
        self.assertIn("compact_preview is not a single line", failed["search_returns_fetchable_artifact"])

    def test_slim_glob_listing_every_path_fails(self):
        failed = self.failed(rewrite_response(NATIVE_WIRE, GLOB, lambda payload: payload.update(
            paths=native.expected_glob(native.fixture_files()))))
        self.assertIn("undocumented fields ['paths']", failed["glob_paths_exist"])

    def test_deleted_required_exchange_fails_contract_and_ledger(self):
        wire = drop_exchange(NATIVE_WIRE, SEARCH_FETCH)
        self.assertIn("missing ['search_fetch']", self.failed(wire)["fetched_matches_equal_source"])
        _, errors = native.native_ledger(wire, NATIVE_STEPS)
        self.assertTrue(any("search_fetch exchange was not executed" in e for e in errors), errors)
        steps = [step for step in NATIVE_STEPS if step["request_id"] != SEARCH_FETCH]
        _, errors = native.native_ledger(NATIVE_WIRE, steps)
        self.assertTrue(any("is not counted in any phase" in e for e in errors), errors)

    def test_misclassified_or_misreported_step_fails(self):
        steps = copy.deepcopy(NATIVE_STEPS)
        next(step for step in steps if step["request_id"] == READ)["phase"] = "verification_retrieval"
        _, errors = native.native_ledger(NATIVE_WIRE, steps)
        self.assertTrue(any("read exchange is counted as 'verification_retrieval'" in e for e in errors), errors)
        steps = copy.deepcopy(NATIVE_STEPS)
        next(step for step in steps if step["request_id"] == SEARCH_FETCH)["response_bytes"] = 10
        _, errors = native.native_ledger(NATIVE_WIRE, steps)
        self.assertTrue(any("recorded 10 B" in e for e in errors), errors)

    def test_required_retrieval_delay_is_counted_in_the_required_total(self):
        base = native.ledger_metrics(native.native_ledger(NATIVE_WIRE, NATIVE_STEPS)[0])
        steps = copy.deepcopy(NATIVE_STEPS)
        next(step for step in steps if step["request_id"] == SEARCH_FETCH)["elapsed_ms"] += 5000
        slow = native.ledger_metrics(native.native_ledger(NATIVE_WIRE, steps)[0])
        self.assertAlmostEqual(slow["required_elapsed_ms"] - base["required_elapsed_ms"], 5000, places=1)
        self.assertEqual(slow["ledgers"]["verification_retrieval"], base["ledgers"]["verification_retrieval"])


class SourceFreshnessContractTests(unittest.TestCase):
    EDITED = native.edited_freshness_source(native.fixture_files()[native.FRESHNESS_PATH])

    def failed(self, wire=FRESH_WIRE, forced=FRESH_FORCED):
        return {k: d for k, (ok, d) in native.check_freshness(wire, forced, self.EDITED).items() if not ok}

    def test_authentic_capture_passes(self):
        self.assertEqual(self.failed(), {})

    def stale_group(self, text):
        return {"groups": [{"matches": [{"path": native.FRESHNESS_PATH, "line": 8, "text": text}]}], "match_count": 1}

    def test_stale_normal_search_fails(self):
        old_line = 'pub const DISCOUNT_AUDIT: &str = "ledger-discount-v1";'
        new_id, old_id = 2, 3
        wire = rewrite_response(FRESH_WIRE, old_id, lambda payload: payload.update(self.stale_group(old_line)))
        self.assertIn("replaced_text_not_served", self.failed(wire))
        wire = rewrite_response(FRESH_WIRE, new_id, lambda payload: payload.update(groups=[], match_count=0))
        self.assertIn("edit_visible_to_search", self.failed(wire))

    def test_forced_index_must_refuse_or_be_current(self):
        old_line = 'pub const DISCOUNT_AUDIT: &str = "ledger-discount-v1";'
        stale = dict(FRESH_FORCED, after_edit={"type": "packet28_search", "response": {"groups": [], "match_count": 0}})
        self.assertIn("forced_index_never_stale", self.failed(forced=stale))
        current = dict(FRESH_FORCED, after_edit={"type": "packet28_search", "response": self.stale_group(
            old_line.replace("ledger-discount-v1", native.FRESHNESS_NEW))})
        self.assertEqual(self.failed(forced=current), {})
        refused_before = dict(FRESH_FORCED, before_edit={"type": "error", "message": "not ready"})
        self.assertIn("forced_index_never_stale", self.failed(forced=refused_before))


def git(root, *args):
    subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false",
                    *args], cwd=root, check=True, capture_output=True)


class SourceStatusTests(unittest.TestCase):
    def test_porcelain_keeps_the_status_columns(self):
        entries = contract.parse_porcelain_z(b" M crates/a.rs\0R  crates/new name.rs\0crates/old.rs\0?? x y.txt\0")
        self.assertEqual(entries, [{"status": " M", "path": "crates/a.rs"},
                                   {"status": "R ", "path": "crates/new name.rs", "orig_path": "crates/old.rs"},
                                   {"status": "??", "path": "x y.txt"}])

    def test_real_git_unstaged_staged_renamed_and_untracked_sources_are_relevant(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        for rel in ("crates/a.rs", "crates/b.rs", "crates/old name.rs", "Cargo.lock", "JavaTest/cache.bin"):
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text("original\n")
        git(root, "init", "-q")
        git(root, "add", ".")
        git(root, "commit", "-qm", "base")
        source, status = runner.source_metadata(root)
        self.assertEqual(source["relevant_dirty_paths"], [])
        (root / "crates/a.rs").write_text("unstaged\n")
        (root / "crates/b.rs").write_text("staged\n")
        git(root, "add", "crates/b.rs")
        git(root, "mv", "crates/old name.rs", "crates/new name.rs")
        (root / "crates/c.rs").write_text("untracked\n")
        (root / "JavaTest/cache.bin").write_text("regenerated\n")
        (root / "scripts/__pycache__").mkdir(parents=True)
        (root / "scripts/__pycache__/agent_dx_native.cpython-312.pyc").write_bytes(b"\0")
        source, status = runner.source_metadata(root)
        self.assertEqual(source["relevant_dirty_paths"],
                         ["crates/a.rs", "crates/b.rs", "crates/c.rs", "crates/new name.rs", "crates/old name.rs"])
        self.assertEqual(contract.relevant_dirty(contract.parse_porcelain_z(status)), source["relevant_dirty_paths"])


class McpSessionCleanupTests(unittest.TestCase):
    def session(self, script):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        fake = tmp / "Packet28"
        fake.write_text("#!/bin/sh\n" + script)
        fake.chmod(0o755)
        (tmp / "repo").mkdir()
        evidence = runner.Evidence(tmp / "artifact")
        ws = types.SimpleNamespace(packet28=fake, repo=tmp / "repo", env=dict(os.environ), evidence=evidence)
        return ws, evidence

    def restore_timeouts(self):
        runner.McpSession.INITIALIZE_TIMEOUT, runner.McpSession.EXIT_TIMEOUT = 60.0, 20.0

    def test_initialize_failure_stops_the_process_and_keeps_partial_evidence(self):
        ws, evidence = self.session("echo 'boom: cannot serve' >&2\nexit 3\n")
        with self.assertRaises(runner.CheckFailure):
            runner.McpSession(ws, "probe", "init-fails")
        rel = "scenarios/probe/mcp/init-fails"
        self.assertIn(b'"method": "initialize"', (evidence.dir / f"{rel}.wire").read_bytes())
        self.assertIn(b"boom: cannot serve", (evidence.dir / f"{rel}.stderr").read_bytes())
        steps = json.loads((evidence.dir / f"{rel}.steps.json").read_text())
        self.assertEqual((steps["exit_code"], steps["readers_finished"]), (3, True))
        self.assertEqual(set(evidence.hashes), {f"{rel}.wire", f"{rel}.stderr", f"{rel}.steps.json"})

    def test_initialize_timeout_kills_the_owned_process_within_bounds(self):
        self.addCleanup(self.restore_timeouts)
        runner.McpSession.INITIALIZE_TIMEOUT, runner.McpSession.EXIT_TIMEOUT = 0.5, 0.5
        ws, evidence = self.session("echo 'partial diagnostic' >&2\nexec sleep 30\n")
        started = time.monotonic()
        with self.assertRaises(runner.CheckFailure) as caught:
            runner.McpSession(ws, "probe", "init-hangs")
        self.assertLess(time.monotonic() - started, 10)
        self.assertIn("timed out", str(caught.exception))
        rel = "scenarios/probe/mcp/init-hangs"
        steps = json.loads((evidence.dir / f"{rel}.steps.json").read_text())
        self.assertTrue(steps["killed"])
        self.assertIsNotNone(steps["exit_code"])
        self.assertIn(b"partial diagnostic", (evidence.dir / f"{rel}.stderr").read_bytes())


class ArtifactDirectoryTests(unittest.TestCase):
    def test_existing_evidence_is_never_deleted(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        earlier = tmp / "artifacts" / "summary.json"
        earlier.parent.mkdir()
        earlier.write_text("earlier failure\n")
        done = subprocess.run([sys.executable, str(SCRIPTS / "benchmark_agent_dx.py"), "--bin-dir", str(tmp / "missing"),
                               "--artifact-dir", str(earlier.parent)], capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("refusing to write into non-empty", done.stderr)
        self.assertEqual(earlier.read_text(), "earlier failure\n")


def write(artifact, rel, data, hashes):
    path = artifact / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    hashes[rel] = hashlib.sha256(data).hexdigest()
    return {"path": rel, "bytes": len(data), "sha256": hashes[rel]}


def product_log(skip=None, outcome="ok"):
    lines = []
    for spec in contract.DELEGATED.values():
        for test in spec["tests"]:
            path, _, name = test.partition("::")
            if "/tests/" in path:
                lines.append(f"     Running {path} (target/debug/deps/x)")
                full = name
            else:
                lines.append("     Running unittests src/lib.rs (target/debug/deps/x)")
                full = f"{Path(path).stem}::tests::{name}"
            if test != skip:
                lines.append(f"test {full} ... {outcome}")
    return ("\n".join(lines) + "\n").encode()


class ValidatorTests(unittest.TestCase):
    """Builds a minimal artifact that satisfies the contract, then breaks it."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.artifact = self.tmp / "artifact"
        hashes = {}
        runtime = {path: validator.git_tree_entry(REPO, "HEAD", path) for path in validator.RUNTIME_PATHS}
        tree = validator.subprocess.run(["git", "rev-parse", "HEAD^{tree}"], cwd=REPO, capture_output=True, text=True).stdout.strip()
        invocation = {"label": "x", "argv": ["Packet28", "--version"], "exit_code": 0,
                      "stdout": write(self.artifact, "cli/x.stdout", b"Packet28\n", hashes),
                      "stderr": write(self.artifact, "cli/x.stderr", b"", hashes)}
        cases = []
        for item in reduction.load_manifest()["cases"]:
            raw_stdout, raw_stderr, _ = reduction.read_fixture_inputs(item)
            if item["case"] == "gh_pr_view_long_description":
                visible = GOOD_PR_VIEW.encode()
            elif item["case"] == "cargo_test_one_failure":
                visible = GOOD_CARGO.encode()
            else:
                visible = None
            streams = {"raw_stdout": raw_stdout, "raw_stderr": raw_stderr,
                       "reduced_stdout": visible if visible is not None else raw_stdout,
                       "reduced_stderr": b"" if visible is not None else raw_stderr}
            record = {label: write(self.artifact, f"reduction/{item['case']}.{label}", data, hashes) for label, data in streams.items()}
            _, _, measured = reduction.reduction_errors(item, streams, item["exit_code"])
            cases.append({"case": item["case"], "stub_invocations": [item["argv"][1:]], "reduced_exit_code": item["exit_code"],
                          "streams": record, **measured})
        self.focus = {"gh_pr_view_long_description", "cargo_test_one_failure"}
        self.manifest_cases = reduction.load_manifest()["cases"]
        for rel, data in native.fixture_files().items():
            write(self.artifact, f"{validator.NATIVE_FIXTURE}/{rel}", data, hashes)
        write(self.artifact, validator.NATIVE_WIRE, NATIVE_WIRE, hashes)
        write(self.artifact, validator.NATIVE_STEPS, json.dumps({"steps": NATIVE_STEPS}).encode(), hashes)
        write(self.artifact, validator.FRESHNESS_WIRE, FRESH_WIRE, hashes)
        write(self.artifact, validator.FRESHNESS_FORCED, json.dumps(FRESH_FORCED).encode(), hashes)
        write(self.artifact, validator.FRESHNESS_EDITED,
              native.edited_freshness_source(native.fixture_files()[native.FRESHNESS_PATH]), hashes)
        status = write(self.artifact, "inputs/source-status-start.z", b"?? JavaTest/cache.bin\0", hashes)
        status_end = write(self.artifact, "inputs/source-status-end.z", b"?? JavaTest/cache.bin\0", hashes)
        native_metrics = native.ledger_metrics(native.native_ledger(NATIVE_WIRE, NATIVE_STEPS)[0])
        log = write(self.artifact, "inputs/product-tests.log", product_log(), hashes)
        write(self.artifact, "invocations.json", json.dumps([invocation]).encode(), hashes)
        self.summary = {
            "schema": "packet28.agent_dx_benchmark.v1", "contract_version": contract.SCHEMA_VERSION,
            "source": {"commit": "HEAD", "tree": tree, "runtime_trees": runtime, "dirty": True, "status_porcelain_z": status},
            "source_end": {"commit": "HEAD", "tree": tree, "status_porcelain_z": status_end},
            "binaries": [{"name": "Packet28", "sha256": "a" * 64}, {"name": "packet28d", "sha256": "b" * 64}],
            "scenarios": [{"id": sid, "checks": [{"id": cid, "passed": True, "detail": "ok", "evidence": []}
                                                  for cid in spec["checks"]],
                           "metrics": native_metrics if sid == "native_retrieval" else {}}
                          for sid, spec in contract.MEASURED.items()],
            "reduction_cases": cases,
            "product_tests": {"log": log, "source_tree": tree},
            "evidence_sha256": hashes,
        }

    def failures(self, summary=None):
        return validator.validate(summary or self.summary, self.artifact, REPO).failures

    def only_focus(self, failures):
        return [f for f in failures if not any(f"explicit_cli_reduction/{c['case']}" in f
                                               for c in self.manifest_cases if c["case"] not in self.focus)]

    def test_complete_artifact_passes_for_the_real_renderings(self):
        self.assertEqual(self.only_focus(self.failures()), [])

    def test_missing_scenario_skipped_check_and_failed_check_fail(self):
        summary = copy.deepcopy(self.summary)
        summary["scenarios"] = [s for s in summary["scenarios"] if s["id"] != "native_retrieval"]
        self.assertTrue(any("native_retrieval" in f and "missing" in f for f in self.failures(summary)))
        summary = copy.deepcopy(self.summary)
        summary["scenarios"][0]["checks"].pop()
        self.assertTrue(any("missing or was skipped" in f for f in self.failures(summary)))
        summary = copy.deepcopy(self.summary)
        summary["scenarios"][0]["checks"] = []
        self.assertTrue(any("missing or was skipped" in f for f in self.failures(summary)))
        summary = copy.deepcopy(self.summary)
        summary["scenarios"][-1]["checks"][0]["passed"] = False
        self.assertTrue(any("cleanup/" in f for f in self.failures(summary)))

    def test_missing_reduction_case_fails(self):
        summary = copy.deepcopy(self.summary)
        summary["reduction_cases"] = [c for c in summary["reduction_cases"] if c["case"] != "gh_pr_view_long_description"]
        self.assertTrue(any("gh_pr_view_long_description" in f and "missing" in f for f in self.failures(summary)))

    def test_tampered_or_deleted_stream_fails(self):
        target = self.artifact / "reduction/cargo_test_one_failure.reduced_stdout"
        target.write_bytes(b"cargo test reported 37 passed and 1 failed\n")
        self.assertTrue(any("cargo_test_one_failure" in f for f in self.failures()))
        target.unlink()
        self.assertTrue(any("missing or altered" in f for f in self.failures()))

    def test_stream_must_match_its_own_record_even_if_the_ledger_was_updated(self):
        summary = copy.deepcopy(self.summary)
        rel = "reduction/cargo_test_one_failure.reduced_stdout"
        data = GOOD_CARGO.replace("left: 850", "left:  850").encode()
        (self.artifact / rel).write_bytes(data)
        summary["evidence_sha256"][rel] = hashlib.sha256(data).hexdigest()
        failures = self.failures(summary)
        self.assertTrue(any("cargo_test_one_failure" in f and "missing or altered" in f for f in failures), failures)

    def test_summary_only_stream_with_updated_hash_still_fails_semantics(self):
        summary = copy.deepcopy(self.summary)
        rel = "reduction/gh_pr_view_long_description.reduced_stdout"
        data = GOOD_PR_VIEW.splitlines(keepends=True)[0].encode()
        (self.artifact / rel).write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        summary["evidence_sha256"][rel] = digest
        entry = next(c for c in summary["reduction_cases"] if c["case"] == "gh_pr_view_long_description")
        entry["streams"]["reduced_stdout"]["sha256"] = digest
        failures = self.failures(summary)
        self.assertTrue(any("URL line" in f for f in failures), failures)
        self.assertTrue(any("reported measurements" in f for f in failures))

    def test_passthrough_of_a_verbose_case_fails(self):
        self.assertTrue(any("gh_pr_list_5" in f and "not reduced" in f for f in self.failures()))

    def test_stub_not_invoked_fails(self):
        summary = copy.deepcopy(self.summary)
        next(c for c in summary["reduction_cases"] if c["case"] == "cargo_test_one_failure")["stub_invocations"] = []
        self.assertTrue(any("exactly once" in f for f in self.failures(summary)))

    def test_product_test_receipt_controls(self):
        summary = copy.deepcopy(self.summary)
        summary.pop("product_tests")
        self.assertTrue(any("no product test log" in f for f in self.failures(summary)))
        first = next(iter(contract.DELEGATED.values()))["tests"][0]
        for data, expected in ((product_log(skip=first), "did not run"), (product_log(outcome="FAILED"), "FAILED")):
            (self.artifact / "inputs/product-tests.log").write_bytes(data)
            summary = copy.deepcopy(self.summary)
            summary["product_tests"]["log"]["sha256"] = summary["evidence_sha256"]["inputs/product-tests.log"] = hashlib.sha256(data).hexdigest()
            self.assertTrue(any(expected in f for f in self.failures(summary)), expected)
        summary = copy.deepcopy(self.summary)
        summary["product_tests"]["source_tree"] = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"  # the empty tree
        self.assertTrue(any("differs in" in f for f in self.failures(summary)))

    def test_renamed_delegated_test_fails(self):
        root = self.tmp / "root"
        for test in (t for spec in contract.DELEGATED.values() for t in spec["tests"]):
            source = Path(test.partition("::")[0])
            (root / source).parent.mkdir(parents=True, exist_ok=True)
            if not (root / source).exists():
                shutil.copy(REPO / source, root / source)
        renamed = root / "crates/packet28d/tests/status_pagination.rs"
        renamed.write_text(renamed.read_text().replace("fn seeded_five_thousand_task_daemon", "fn renamed_daemon"))
        report = validator.Report(self.artifact)
        validator.check_delegated(self.summary, report, root)
        self.assertTrue(any("no longer exists" in f for f in report.failures))

    def replace_status(self, summary, label, data):
        rel = f"inputs/source-status-{label}.z"
        (self.artifact / rel).write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        key = "source" if label == "start" else "source_end"
        summary[key]["status_porcelain_z"]["sha256"] = summary["evidence_sha256"][rel] = digest

    def test_dirty_source_and_wrong_schema_fail(self):
        self.assertFalse(any("source" in f for f in self.only_focus(self.failures())))
        for data in (b" M crates/packet28d/src/main.rs\0", b"M  Cargo.lock\0",
                     b"?? scripts/agent_dx_new_helper.py\0", b"R  crates/a b.rs\0crates/a.rs\0"):
            summary = copy.deepcopy(self.summary)
            self.replace_status(summary, "start", data)
            self.replace_status(summary, "end", data)
            failures = self.failures(summary)
            self.assertTrue(any("unmodified runtime" in f for f in failures), data)
        summary = copy.deepcopy(self.summary)
        self.replace_status(summary, "start", b"?? scripts/__pycache__/x.pyc\0")
        self.replace_status(summary, "end", b"?? scripts/__pycache__/x.pyc\0")
        self.assertFalse(any("source binding" in f for f in self.failures(summary)))
        self.assertTrue(self.failures(dict(self.summary, schema="old")))

    def test_source_change_during_the_run_or_missing_receipt_fails(self):
        summary = copy.deepcopy(self.summary)
        self.replace_status(summary, "end", b"?? JavaTest/cache.bin\0 M README.md\0")
        self.assertTrue(any("did not change while the benchmark ran" in f for f in self.failures(summary)))
        summary = copy.deepcopy(self.summary)
        del summary["source_end"]
        self.assertTrue(any("source_end git status receipt" in f for f in self.failures(summary)))

    def test_native_results_are_rechecked_from_the_wire_not_trusted(self):
        summary = copy.deepcopy(self.summary)
        def drop(payload):
            payload["content"] = "\n".join(payload["content"].split("\n")[1:])
            payload["match_count"] = payload["returned_match_count"] = 31
        wire = rewrite_response(NATIVE_WIRE, SEARCH_FETCH, drop)
        (self.artifact / validator.NATIVE_WIRE).write_bytes(wire)
        summary["evidence_sha256"][validator.NATIVE_WIRE] = hashlib.sha256(wire).hexdigest()
        failures = self.failures(summary)  # every runner check still says passed
        self.assertTrue(any("native_retrieval/fetched_matches_equal_source" in f and "source matches missing" in f
                            for f in failures), failures)

    def test_deleted_native_wire_and_altered_metrics_fail(self):
        summary = copy.deepcopy(self.summary)
        metrics = next(x for x in summary["scenarios"] if x["id"] == "native_retrieval")["metrics"]
        metrics["required_retrieval_tokens"] = 0
        metrics["mcp_round_trips"] = 0
        failures = self.failures(summary)
        self.assertTrue(any("required_retrieval_tokens reported 0" in f for f in failures), failures)
        self.assertTrue(any("mcp_round_trips reported 0" in f for f in failures), failures)
        summary = copy.deepcopy(self.summary)
        steps = copy.deepcopy(NATIVE_STEPS)
        next(step for step in steps if step["request_id"] == SEARCH_FETCH)["elapsed_ms"] += 5000
        data = json.dumps({"steps": steps}).encode()
        (self.artifact / validator.NATIVE_STEPS).write_bytes(data)
        summary["evidence_sha256"][validator.NATIVE_STEPS] = hashlib.sha256(data).hexdigest()
        self.assertTrue(any("required_elapsed_ms reported" in f for f in self.failures(summary)))
        (self.artifact / validator.NATIVE_WIRE).unlink()
        self.assertTrue(any(validator.NATIVE_WIRE in f and "missing" in f for f in self.failures()))

    def test_freshness_is_rechecked_from_the_wire(self):
        summary = copy.deepcopy(self.summary)
        forced = dict(FRESH_FORCED, after_edit={"type": "packet28_search", "response": {"groups": [], "match_count": 0}})
        data = json.dumps(forced).encode()
        (self.artifact / validator.FRESHNESS_FORCED).write_bytes(data)
        summary["evidence_sha256"][validator.FRESHNESS_FORCED] = hashlib.sha256(data).hexdigest()
        self.assertTrue(any("source_freshness/forced_index_never_stale" in f for f in self.failures(summary)))

    def test_passed_check_evidence_must_be_hashed(self):
        summary = copy.deepcopy(self.summary)
        summary["scenarios"][0]["checks"][0]["evidence"] = ["scenarios/never-written.wire"]
        self.assertTrue(any("not in the hash ledger" in f for f in self.failures(summary)))

    def test_failure_lines_name_contract_and_evidence(self):
        summary = copy.deepcopy(self.summary)
        summary["scenarios"][3]["checks"][0].update(passed=False, detail="boom", evidence=["scenarios/x.wire"])
        line = next(f for f in self.failures(summary) if "boom" in f)
        self.assertIn(contract.MEASURED[summary["scenarios"][3]["id"]]["checks"][summary["scenarios"][3]["checks"][0]["id"]], line)
        self.assertIn(str(self.artifact / "scenarios/x.wire"), line)


class WorkflowTests(unittest.TestCase):
    def test_benchmark_workflow_always_validates_and_uploads(self):
        text = (REPO / ".github/workflows/agent-dx-benchmark.yml").read_text()
        self.assertIn("scripts/benchmark_agent_dx.py", text)
        self.assertIn("scripts/validate_agent_dx_benchmark.py", text)
        for step in ("Validate agent DX benchmark", "Publish workflow summary", "Upload agent DX benchmark artifacts"):
            block = text.split(f"name: {step}", 1)[1].split("- name:", 1)[0]
            self.assertIn("if: always()", block, step)
        self.assertNotIn("cargo run", text)

    def test_retired_percentage_gates_are_gone(self):
        for name in ("benchmark_hook_suite.py", "validate_hook_benchmarks.py", "hook_benchmark_thresholds.py",
                     "validate_native_benchmarks.py", "benchmark_native_mcp.py"):
            self.assertFalse((SCRIPTS / name).exists(), name)
        for workflow in (REPO / ".github/workflows").glob("*.yml"):
            text = workflow.read_text()
            self.assertNotIn("validate_hook_benchmarks", text, workflow.name)
            self.assertNotIn("Hook Benchmark Suite", text, workflow.name)
        release = (REPO / ".github/workflows/release.yml").read_text()
        self.assertIn("scripts/validate_agent_dx_benchmark.py", release)


if __name__ == "__main__":
    unittest.main()
