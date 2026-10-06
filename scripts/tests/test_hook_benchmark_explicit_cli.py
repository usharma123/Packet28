import contextlib
import io
import json
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import benchmark_hook_rewrite as benchmark
import benchmark_hook_suite as suite
import test_token_usage as token_usage
import validate_hook_benchmarks as validator
from hook_benchmark_thresholds import eligible_for_mean
sys.path.pop(0)


class ExplicitBenchmarkTests(unittest.TestCase):
    def run_case(self, root, argv, raw_exit=0, reduced_exit=0, hook_payload=None):
        calls = []
        if hook_payload is None:
            hook_payload = {"hookSpecificOutput": {"hookEventName": "PreToolUse"}}

        def capture(command, cwd, stdin_text=None):
            calls.append((command, cwd, stdin_text))
            if "hook" in command:
                return subprocess.CompletedProcess(command, 0, json.dumps(hook_payload), "")
            if command[:2] == ["/bin/sh", "-lc"]:
                self.assertEqual(shlex.split(command[2]), argv)
                raw_text = "title\n\nthird\nfourth\nfifth\n" if argv[0] == "head" else "raw output\n" * 100
                return subprocess.CompletedProcess(command, raw_exit, raw_text, "")
            if argv[0] == "head":
                return subprocess.CompletedProcess(command, reduced_exit, "1|title\n2|\n3|third\n4|fourth\n5|fifth\n", "")
            return subprocess.CompletedProcess(command, reduced_exit, "explicit reduction\n", "")

        output = io.StringIO()
        with patch.object(benchmark, "run_capture", side_effect=capture), \
             patch.object(benchmark, "resolve_shell", return_value="/bin/sh"), \
             patch.object(sys, "argv", ["benchmark", "--root", str(root), "--task-id", "case-task", "--json", "--", *argv]), \
             contextlib.redirect_stdout(output):
            self.assertEqual(benchmark.main(), 0)
        return json.loads(output.getvalue()), calls

    def test_all_seven_live_cases_keep_their_corpus_and_execute_explicit_routes(self):
        cases = suite.default_cases("owner/repo", "12", "34")
        self.assertEqual([name for name, _ in cases], [
            "git_status", "fs_head", "rust_test", "gh_pr_list", "gh_pr_view", "gh_run_list", "gh_run_view",
        ])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            for name, argv in cases:
                with self.subTest(case=name):
                    payload, calls = self.run_case(root, argv)
                    self.assertEqual(len(calls), 3)
                    self.assertEqual(payload["status"], "ok")
                    self.assertTrue(payload["pretool_capture_only"])
                    self.assertIsNone(payload["rewritten_command"])
                    self.assertEqual(payload["compact_path"], "explicit_cli")
                    self.assertEqual(payload["estimate_scope"], "visible_cli_output")
                    cli_args = calls[2][0][calls[2][0].index("--") + 1:]
                    if name == "fs_head":
                        self.assertTrue(payload["read_window_integrity"]["passed"])
                        self.assertLess(payload["token_reduction_pct"], 0)
                        self.assertEqual(cli_args, [
                            "--via-daemon", "--daemon-root", str(root), "compact", "read",
                            "--root", str(root), "--task-id", "case-task", "--cwd", str(root),
                            "--line-start", "1", "--line-end", "5", "README.md",
                        ])
                    else:
                        self.assertEqual(cli_args, argv)

    def test_exit_mismatch_fails_live_integrity(self):
        with tempfile.TemporaryDirectory() as directory:
            payload, _ = self.run_case(Path(directory), ["git", "status"], raw_exit=7)
        self.assertEqual(payload["status"], "error")
        self.assertIn("differs from raw exit 7", payload["error"])

    def test_head_content_mismatch_fails_integrity(self):
        replies = [
            subprocess.CompletedProcess([], 0, "{}", ""),
            subprocess.CompletedProcess([], 0, "first\nsecond\nthird\nfourth\nfifth\n", ""),
            subprocess.CompletedProcess([], 0, "1|first\n2|second\n3|changed\n4|fourth\n5|fifth\n", ""),
        ]
        with patch.object(benchmark, "run_capture", side_effect=replies), \
             patch.object(benchmark, "resolve_shell", return_value="/bin/sh"), \
             patch.object(sys, "argv", ["benchmark", "--json", "--", "head", "-n", "5", "README.md"]), \
             contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(benchmark.main(), 0)
        payload = json.loads(output.getvalue())
        self.assertEqual(payload["status"], "error")
        self.assertFalse(payload["read_window_integrity"]["passed"])

    def test_head_negative_reduction_remains_in_mean_and_requires_integrity(self):
        head = {
            "case": "fs_head", "status": "ok", "surface": "live",
            "raw_est_tokens": 62, "reduced_est_tokens": 65, "token_reduction_pct": -4.8,
            "raw_exit_code": 0, "reduced_exit_code": 0,
            "read_window_integrity": {"passed": True, "line_start": 1, "line_end": 5, "raw_line_count": 5},
        }
        self.assertTrue(eligible_for_mean(head))
        summary = suite.build_summary([head], Path("/fixture"), Path("/artifacts"), None, None, None)
        self.assertEqual(summary["mean_token_reduction_pct"], -4.8)
        errors, notes = validator.validate(summary)
        self.assertTrue(any("below required 85.0%" in error for error in errors))
        self.assertFalse(any("fs_head:" in error for error in errors))
        self.assertTrue(any("62 raw -> 65 visible tokens (-4.8%) remains" in note for note in notes))
        head["read_window_integrity"]["passed"] = False
        errors, _ = validator.validate(summary)
        self.assertTrue(any("content/window/exit integrity failed" in error for error in errors))

    def test_authority_fields_never_become_executable_benchmark_input(self):
        for key in ("updatedInput", "permissionDecision", "decision"):
            for nested in (False, True):
                payload = {"hookSpecificOutput": {key: {"command": "touch unauthorized"}}} if nested else {key: "allow"}
                with self.subTest(key=key, nested=nested), \
                     patch.object(benchmark, "run_capture", return_value=subprocess.CompletedProcess([], 0, json.dumps(payload), "")) as capture, \
                     patch.object(benchmark, "resolve_shell", return_value="/bin/sh"), \
                     patch.object(sys, "argv", ["benchmark", "--json", "--", "git", "status"]):
                    with self.assertRaisesRegex(SystemExit, "permission authority"):
                        benchmark.main()
                    self.assertEqual(capture.call_count, 1)

    def test_ad_hoc_shell_expression_remains_unchanged_without_an_explicit_route(self):
        original = "cargo test --lib 2>&1 | tail -30"
        calls = []

        def run(command, cwd, stdin_text=None):
            calls.append(command)
            if "hook" in command:
                return subprocess.CompletedProcess(command, 0, "{}", "")
            return subprocess.CompletedProcess(command, 0, "raw\n", "")

        with patch.object(token_usage, "run", side_effect=run):
            result = token_usage.run_hook_case(Path("/fixture"), "cargo_test", original, "/bin/sh")
        self.assertEqual(calls, [["/bin/sh", "-lc", original], ["Packet28", "hook", "claude", "--root", "/fixture"]])
        self.assertEqual(result["status"], "capture_only")
        self.assertEqual(result["reduced_tokens"], result["raw_tokens"])


if __name__ == "__main__":
    unittest.main()
