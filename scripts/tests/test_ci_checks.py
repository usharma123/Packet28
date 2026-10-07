from __future__ import annotations

import copy
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from scripts.ci import select_checks
from scripts import verify_ci_policy

ROOT = Path(__file__).resolve().parents[2]
GATE = ROOT / "scripts/validate_full_gate.sh"


class SelectionTests(unittest.TestCase):
    def test_ordinary_source_and_docs_defer_expensive_checks(self):
        self.assertEqual(select_checks.select(["crates/a/src/lib.rs", "docs/dev.md"]),
                         {"packages": False, "dependencies": False})

    def test_graph_inputs_and_ci_changes_run_both_checks(self):
        for path in ("Cargo.toml", "crates/a/Cargo.toml", "Cargo.lock",
                     "Cargo.direct-minimal.lock", "rust-toolchain.toml",
                     ".cargo/config.toml", "scripts/ci/select_checks.py",
                     ".github/workflows/build.yml", "scripts/package_cargo_workspace.py"):
            with self.subTest(path=path):
                self.assertTrue(all(select_checks.select([path]).values()))

    def test_package_only_changes(self):
        for path in ("npm/a/index.js", "package/install.js", "package.json",
                     "crates/a/build.rs", ".npmignore", "LICENSE"):
            with self.subTest(path=path):
                self.assertEqual(select_checks.select([path]),
                                 {"packages": True, "dependencies": False})

    def test_missing_history_and_non_pr_runs_select_everything(self):
        for event in ("pull_request", "push", "schedule", "workflow_dispatch"):
            with self.subTest(event=event), tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / "output"
                env = dict(os.environ, GITHUB_EVENT_NAME=event, GITHUB_OUTPUT=str(output))
                result = subprocess.run(["python3", str(ROOT / "scripts/ci/select_checks.py")],
                                        cwd=tmp, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(set(output.read_text().splitlines()),
                                 {"dependencies=true", "packages=true"})

    def test_merge_diff_includes_earlier_commits_deletions_and_renames(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            def git(*args):
                return subprocess.run(["git", *args], cwd=root, check=True,
                                      capture_output=True, text=True)
            git("init", "-b", "base")
            git("config", "user.name", "CI test")
            git("config", "user.email", "ci@example.invalid")
            (root / "Cargo.toml").write_text("fixture")
            (root / "readme").write_text("base")
            git("add", ".")
            git("commit", "-m", "base")
            git("checkout", "-b", "feature")
            git("mv", "Cargo.toml", "old-manifest")
            git("commit", "-m", "rename manifest")
            (root / "readme").write_text("last commit only changes docs")
            git("add", ".")
            git("commit", "-m", "docs")
            git("checkout", "base")
            git("merge", "--no-ff", "feature", "-m", "synthetic merge")
            output = root / "outputs"
            env = dict(os.environ, GITHUB_EVENT_NAME="pull_request", GITHUB_OUTPUT=str(output))
            subprocess.run(["python3", str(ROOT / "scripts/ci/select_checks.py")],
                           cwd=root, env=env, check=True, capture_output=True)
            self.assertEqual(set(output.read_text().splitlines()),
                             {"dependencies=true", "packages=true"})

    def test_aggregate_rejects_failure_cancellation_and_unexpected_skips(self):
        needs = {job: {"result": "success"}
                 for job in (*select_checks.REQUIRED_JOBS, *select_checks.OPTIONAL_JOBS)}
        needs["changes"]["outputs"] = {job: "true" for job in select_checks.OPTIONAL_JOBS}
        self.assertTrue(select_checks.results_ok(needs))
        for job in needs:
            for result in ("failure", "cancelled", "skipped"):
                with self.subTest(job=job, result=result):
                    broken = copy.deepcopy(needs)
                    broken[job]["result"] = result
                    self.assertFalse(select_checks.results_ok(broken))
        for job in select_checks.OPTIONAL_JOBS:
            needs["changes"]["outputs"][job] = "false"
            needs[job]["result"] = "skipped"
        self.assertTrue(select_checks.results_ok(needs))
        needs["changes"]["outputs"].pop("packages")
        self.assertFalse(select_checks.results_ok(needs))


class GateTests(unittest.TestCase):
    def commands(self, *args):
        result = subprocess.run([str(GATE), "--list", *args], cwd=ROOT,
                                text=True, capture_output=True, check=True)
        return result.stdout.splitlines()

    def test_ci_phases_cover_full_gate_except_redundant_check_and_build(self):
        phases = ("policy", "lint", "tests", "docs", "audit", "dependencies", "packages")
        split = [command for phase in phases for command in self.commands("--phase", phase)]
        full = self.commands()
        extra = {"+ cargo check --workspace --all-targets --all-features --locked",
                 "+ cargo build --workspace --all-targets --all-features --locked"}
        self.assertEqual(set(full), set(split) | extra)
        self.assertEqual(len(split), len(set(split)))
        self.assertEqual(len(full), len(set(full)))

    def test_policy_does_not_compile_and_lint_keeps_hazard_checks(self):
        policy = "\n".join(self.commands("--phase", "policy"))
        lint = "\n".join(self.commands("--phase", "lint"))
        self.assertNotIn("check_rust_hazards.py", policy)
        self.assertNotIn("cargo test", policy)
        self.assertIn("check_rust_hazards.py", lint)
        self.assertNotIn("+ cargo clippy", lint)

    def test_msrv_does_not_repeat_policy_or_tests(self):
        self.assertEqual(self.commands("--msrv"),
                         ["+ cargo check --workspace --all-targets --all-features --locked"])

    def test_release_cannot_be_combined_with_partial_validation(self):
        for args in (("--phase", "tests", "--release-tag", "v1.0.0"),
                     ("--msrv", "--phase", "tests"), ("--phase", "typo")):
            self.assertEqual(subprocess.run([str(GATE), "--list", *args],
                                           capture_output=True).returncode, 2)
        release = self.commands("--release-tag", "v1.0.0")
        self.assertTrue(set(self.commands()).issubset(release))
        self.assertTrue(any("--final --source-rev HEAD" in line for line in release))

    def test_workflow_cannot_omit_a_phase_or_weaken_aggregate(self):
        build = (ROOT / ".github/workflows/build.yml").read_text()
        self.assertEqual(verify_ci_policy.split_ci_wiring_errors(build), [])
        for fragment in ("--phase tests", "--phase packages", "if: always()",
                         "--check-results", "msrv, audit"):
            with self.subTest(fragment=fragment):
                self.assertTrue(verify_ci_policy.split_ci_wiring_errors(build.replace(fragment, "")))


if __name__ == "__main__":
    unittest.main()
