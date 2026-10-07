from __future__ import annotations

import unittest

from scripts import verify_ci_policy


class ReproducibleCargoPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.build_path = verify_ci_policy.WORKFLOW_DIR / "build.yml"
        cls.build = cls.build_path.read_text(encoding="utf-8")

    def test_repository_msrv_toolchain_explicitly_installs_clippy(self) -> None:
        self.assertEqual(
            verify_ci_policy.msrv_clippy_component_errors(
                self.build_path, self.build
            ),
            [],
        )

    def test_rejects_msrv_toolchain_without_clippy(self) -> None:
        unsafe = self.build.replace(
            "          toolchain: 1.88.0\n          components: clippy\n",
            "          toolchain: 1.88.0\n",
            1,
        )

        errors = verify_ci_policy.msrv_clippy_component_errors(
            self.build_path, unsafe
        )

        self.assertTrue(
            any(
                error.endswith("MSRV toolchain does not install clippy")
                for error in errors
            )
        )

    def test_benchmark_cargo_graph_commands_are_locked_and_scanned(self) -> None:
        self.assertTrue(
            set(verify_ci_policy.BENCHMARK_LOCKED_COMMAND_FILES).issubset(
                verify_ci_policy.LOCKED_COMMAND_FILES
            )
        )
        for path in verify_ci_policy.BENCHMARK_LOCKED_COMMAND_FILES:
            with self.subTest(path=path):
                self.assertEqual(
                    verify_ci_policy.locked_command_errors(
                        path, path.read_text(encoding="utf-8")
                    ),
                    [],
                )

    def test_rejects_unlocked_benchmark_build_and_run_commands(self) -> None:
        cases = {
            "benchmarks/run.sh": "cargo build --release -p covy-cli",
            "benchmarks/run_agent_search_bench.sh": (
                "cargo build -q -p suite-cli --bin Packet28"
            ),
            "benchmarks/per-03-incremental-index/README.md": (
                "cargo run --offline --release -p mapy-core"
            ),
        }
        for relative_path, command in cases.items():
            path = verify_ci_policy.ROOT / relative_path
            with self.subTest(path=relative_path):
                errors = verify_ci_policy.locked_command_errors(path, command)
                self.assertEqual(len(errors), 1)
                self.assertIn(
                    "Cargo graph command lacks --locked", errors[0]
                )


class CanonicalGatePolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.full_gate = (
            verify_ci_policy.ROOT / "scripts" / "validate_full_gate.sh"
        ).read_text(encoding="utf-8")
        cls.workspace_policy = (
            verify_ci_policy.ROOT / "scripts" / "verify_workspace_policy.sh"
        ).read_text(encoding="utf-8")

    def test_repository_release_gate_strictly_finalizes_the_audit_ledger(
        self,
    ) -> None:
        self.assertEqual(
            verify_ci_policy.audit_finalization_wiring_errors(self.full_gate),
            [],
        )

    def test_repository_gate_compiles_the_direct_minimum_graph(self) -> None:
        self.assertEqual(
            verify_ci_policy.direct_minimum_gate_errors(self.full_gate),
            [],
        )

    def test_repository_gate_fetches_locked_graph_before_offline_policy(
        self,
    ) -> None:
        self.assertEqual(
            verify_ci_policy.clean_runner_bootstrap_errors(
                self.full_gate,
                self.workspace_policy,
            ),
            [],
        )

    def test_policy_rejects_gate_without_workspace_bootstrap(self) -> None:
        unsafe = self.full_gate.replace(
            "run_cmd scripts/verify_workspace_policy.sh --bootstrap\n",
            "run_cmd scripts/verify_workspace_policy.sh\n",
            1,
        )

        self.assertEqual(
            verify_ci_policy.clean_runner_bootstrap_errors(
                unsafe,
                self.workspace_policy,
            ),
            [
                "canonical gate does not bootstrap every locked workspace"
            ],
        )

    def test_policy_rejects_missing_per_manifest_fetch(self) -> None:
        unsafe = self.workspace_policy.replace(
            "    cargo fetch \\\n"
            "      --locked \\\n"
            '      --manifest-path "$manifest"\n',
            "",
            1,
        )

        self.assertEqual(
            verify_ci_policy.clean_runner_bootstrap_errors(
                self.full_gate,
                unsafe,
            ),
            [
                "workspace policy does not fetch each discovered locked "
                "manifest"
            ],
        )

    def test_repository_gate_verifies_runtime_starvation_evidence(self) -> None:
        self.assertEqual(
            verify_ci_policy.runtime_starvation_evidence_gate_errors(
                self.full_gate
            ),
            [],
        )

    def test_policy_rejects_gate_without_runtime_starvation_evidence(
        self,
    ) -> None:
        unsafe = self.full_gate.replace(
            "run_cmd python3 "
            "benchmarks/asy-04-runtime-starvation/verify.py\n",
            "",
            1,
        )
        self.assertEqual(
            verify_ci_policy.runtime_starvation_evidence_gate_errors(unsafe),
            [
                "canonical gate does not verify the "
                "runtime-starvation evidence"
            ],
        )

    def test_repository_gate_verifies_incremental_index_evidence(self) -> None:
        self.assertEqual(
            verify_ci_policy.incremental_index_evidence_gate_errors(
                self.full_gate
            ),
            [],
        )

    def test_policy_rejects_gate_without_incremental_index_evidence(
        self,
    ) -> None:
        unsafe = self.full_gate.replace(
            "run_cmd python3 "
            "benchmarks/per-03-incremental-index/verify.py\n",
            "",
            1,
        )
        self.assertEqual(
            verify_ci_policy.incremental_index_evidence_gate_errors(unsafe),
            [
                "canonical gate does not verify the "
                "incremental-index evidence"
            ],
        )

    def test_policy_rejects_gate_without_direct_minimum_graph(self) -> None:
        unsafe = self.full_gate.replace(
            "run_cmd python3 scripts/validate_direct_minimum.py\n",
            "",
            1,
        )
        self.assertEqual(
            verify_ci_policy.direct_minimum_gate_errors(unsafe),
            [
                "canonical gate does not compile the committed "
                "direct-minimum graph"
            ],
        )

    def test_rejects_release_gate_without_strict_ledger_finalization(
        self,
    ) -> None:
        unsafe = self.full_gate.replace(
            "  run_cmd python3 scripts/check_architecture_audit_ledger.py \\\n"
            "    --final --source-rev HEAD^\n",
            "",
            1,
        )
        self.assertEqual(
            verify_ci_policy.audit_finalization_wiring_errors(unsafe),
            [
                "tag-aware canonical gate does not strictly finalize the "
                "audit ledger against HEAD^"
            ],
        )


class AutofixSecurityPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = (
            verify_ci_policy.WORKFLOW_DIR / "codex-autofix.yml"
        ).read_text(encoding="utf-8")

    def test_repository_workflow_satisfies_security_boundary(self) -> None:
        self.assertEqual(
            verify_ci_policy.autofix_security_errors(self.workflow),
            [],
        )

    def test_rejects_automatic_untrusted_branch_execution(self) -> None:
        unsafe = self.workflow.replace(
            "        github.event.workflow_run.head_branch == "
            "github.event.repository.default_branch &&\n",
            "",
        )

        errors = verify_ci_policy.autofix_security_errors(unsafe)

        self.assertIn(
            "automatic runs are not restricted to the trusted default branch",
            errors,
        )

    def test_rejects_manual_arbitrary_ref_or_unvalidated_run(self) -> None:
        unsafe_ref = self.workflow.replace(
            "      run_id:\n",
            "      ref:\n"
            '        description: "Untrusted repair ref"\n'
            "        required: true\n"
            "      run_id:\n",
            1,
        ).replace(
            "ref: ${{ steps.resolve.outputs.target_ref }}",
            "ref: ${{ inputs.ref }}",
            1,
        )
        self.assertIn(
            "manual autofix may not accept an arbitrary repair ref",
            verify_ci_policy.autofix_security_errors(unsafe_ref),
        )

        unsafe_metadata = self.workflow.replace(
            '            python3 "$TRUSTED_RUN_VALIDATOR" \\\n',
            "            printf '%s' '{\"target_ref\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}' \\\n",
            1,
        )
        self.assertIn(
            "manual run metadata is not validated before checkout",
            verify_ci_policy.autofix_security_errors(unsafe_metadata),
        )

    def test_rejects_target_outside_default_branch_history(self) -> None:
        unsafe = self.workflow.replace(
            "          git -C trusted-control merge-base --is-ancestor \\\n"
            '            "$target_ref" "origin/$DEFAULT_BRANCH"\n',
            "",
            1,
        )
        self.assertIn(
            "validated run commit is not constrained to default-branch history",
            verify_ci_policy.autofix_security_errors(unsafe),
        )

    def test_rejects_candidate_supplied_driver(self) -> None:
        unsafe = self.workflow.replace(
            "TRUSTED_AUTOFIX: "
            "${{ github.workspace }}/trusted-control/scripts/ci/codex_autofix.sh",
            "TRUSTED_AUTOFIX_REMOVED: true",
        )
        unsafe += "\n      - name: Unsafe candidate driver\n"
        unsafe += "        run: scripts/ci/codex_autofix.sh\n"

        errors = verify_ci_policy.autofix_security_errors(unsafe)

        self.assertIn("trusted autofix driver is not selected explicitly", errors)
        self.assertIn(
            "candidate checkout may not supply the executed autofix driver",
            errors,
        )

    def test_rejects_secrets_or_write_credentials_in_execution_job(self) -> None:
        unsafe = self.workflow.replace(
            "      CODEX_AUTOFIX_VERIFY: cargo test --locked --workspace --all-targets\n",
            "      CODEX_AUTOFIX_VERIFY: cargo test --locked --workspace --all-targets\n"
            "      OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}\n",
        ).replace(
            "      contents: read\n    runs-on: ubuntu-latest",
            "      contents: write\n    runs-on: ubuntu-latest",
            1,
        )

        errors = verify_ci_policy.autofix_security_errors(unsafe)

        self.assertIn(
            "OpenAI credentials must be scoped to exactly one execution step",
            errors,
        )
        self.assertIn(
            "autofix execution job may not have write permissions",
            errors,
        )

    def test_rejects_force_push(self) -> None:
        unsafe = self.workflow.replace(
            '          git push -u origin "$branch"\n',
            '          git push --force-with-lease origin "$branch"\n',
        )

        self.assertIn(
            "autofix publication may not force-push",
            verify_ci_policy.autofix_security_errors(unsafe),
        )


class ReleasePermissionPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = (
            verify_ci_policy.WORKFLOW_DIR / "release.yml"
        ).read_text(encoding="utf-8")

    def test_repository_release_permissions_are_least_privilege(self) -> None:
        self.assertEqual(
            verify_ci_policy.release_permission_errors(self.workflow),
            [],
        )

    def test_rejects_workflow_wide_publication_permissions(self) -> None:
        unsafe = self.workflow.replace(
            "permissions:\n  contents: read",
            "permissions:\n  contents: write\n  id-token: write",
            1,
        )

        errors = verify_ci_policy.release_permission_errors(unsafe)

        self.assertIn(
            "workflow-wide release permissions may not grant writes",
            errors,
        )
        self.assertIn(
            "contents write permission must occur only in the publish job",
            errors,
        )
        self.assertIn(
            "OIDC write permission must occur only in the publish job",
            errors,
        )

    def test_rejects_publication_permissions_on_build_job(self) -> None:
        unsafe = self.workflow.replace(
            "  build:\n    needs: release-gates\n    permissions:\n"
            "      contents: read",
            "  build:\n    needs: release-gates\n    permissions:\n"
            "      contents: write\n      id-token: write",
            1,
        )

        self.assertIn(
            "build job may not receive publication permissions",
            verify_ci_policy.release_permission_errors(unsafe),
        )

    def test_rejects_publish_job_without_oidc(self) -> None:
        unsafe = self.workflow.replace("      id-token: write\n", "", 1)

        errors = verify_ci_policy.release_permission_errors(unsafe)

        self.assertIn(
            "publish job lacks scoped id-token: write permission",
            errors,
        )
        self.assertIn(
            "OIDC write permission must occur only in the publish job",
            errors,
        )


class DependabotPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.config = (
            verify_ci_policy.ROOT / ".github" / "dependabot.yml"
        ).read_text(encoding="utf-8")

    def test_repository_dependency_lane_is_reviewed_and_bounded(self) -> None:
        self.assertEqual(
            verify_ci_policy.dependabot_policy_errors(self.config),
            [],
        )

    def test_rejects_missing_ecosystem_or_unbounded_prs(self) -> None:
        unsafe = self.config.replace(
            '- package-ecosystem: "github-actions"',
            '- package-ecosystem: "docker"',
            1,
        ).replace("    open-pull-requests-limit: 5\n", "", 1)

        errors = verify_ci_policy.dependabot_policy_errors(unsafe)

        self.assertIn(
            'Dependabot must configure "github-actions" exactly once',
            errors,
        )
        self.assertIn(
            "every dependency ecosystem must bound open update PRs at five",
            errors,
        )

    def test_rejects_automatic_merge(self) -> None:
        unsafe = self.config + "\nauto-merge: true\n"

        self.assertIn(
            "dependency updates must not bypass review through auto-merge",
            verify_ci_policy.dependabot_policy_errors(unsafe),
        )


class ReleasePackageSmokePolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.build = (verify_ci_policy.WORKFLOW_DIR / "build.yml").read_text(
            encoding="utf-8"
        )
        cls.release = (verify_ci_policy.WORKFLOW_DIR / "release.yml").read_text(
            encoding="utf-8"
        )
        cls.full_gate = (
            verify_ci_policy.ROOT / "scripts" / "validate_full_gate.sh"
        ).read_text(encoding="utf-8")
        cls.package_verifier = (
            verify_ci_policy.ROOT / "scripts" / "verify_release_packages.py"
        ).read_text(encoding="utf-8")

    def errors(
        self,
        *,
        build: str | None = None,
        release: str | None = None,
        full_gate: str | None = None,
        package_verifier: str | None = None,
    ) -> list[str]:
        return verify_ci_policy.release_package_smoke_errors(
            build if build is not None else self.build,
            release if release is not None else self.release,
            full_gate if full_gate is not None else self.full_gate,
            (
                package_verifier
                if package_verifier is not None
                else self.package_verifier
            ),
        )

    def test_repository_release_package_smoke_is_complete(self) -> None:
        self.assertEqual(self.errors(), [])

    def test_rejects_static_musl_target_for_preload_library(self) -> None:
        unsafe = self.release.replace(
            '--target "${{ matrix.shim_target }}" --package context-instruct-shim',
            '--target "${{ matrix.target }}" --package context-instruct-shim',
        )
        self.assertIn(
            "preload shim must be built separately from static musl executables",
            self.errors(release=unsafe),
        )

    def test_rejects_preload_library_staged_from_static_target(self) -> None:
        unsafe = self.release.replace(
            'ARTIFACT_TARGET="${{ matrix.shim_target }}"',
            'ARTIFACT_TARGET="${{ matrix.target }}"',
        )
        self.assertIn(
            "preload shim must be staged from its shared-library target",
            self.errors(release=unsafe),
        )

    def test_rejects_missing_pre_upload_binary_smoke(self) -> None:
        unsafe = self.release.replace(
            "python3 scripts/verify_release_packages.py platform",
            "python3 scripts/removed_release_package_verifier.py platform",
            1,
        )

        self.assertIn(
            "staged platform verifier is not invoked in the build job",
            self.errors(release=unsafe),
        )

    def test_rejects_linux_arm64_without_emulated_execution(self) -> None:
        unsafe = self.release.replace(
            "smoke_mode: qemu-aarch64",
            "smoke_mode: native-or-metadata",
            1,
        )

        errors = self.errors(release=unsafe)

        self.assertTrue(
            any("smoke_mode: qemu-aarch64" in error for error in errors)
        )

    def test_rejects_silent_macos_cross_architecture_skip(self) -> None:
        unsafe = self.release.replace(
            "x86_64 execution requires an Intel runner or Rosetta and remains an "
            "external release check.",
            "cross build",
            1,
        )

        self.assertIn(
            "macOS x86_64 execution limitation is not explicit",
            self.errors(release=unsafe),
        )

    def test_rejects_artifact_transfer_that_loses_executable_modes(self) -> None:
        unsafe = self.release.replace(
            "path: dist/pkg-${{ matrix.platform }}.tar.gz",
            "path: dist/@packet28/${{ matrix.platform }}",
            1,
        )

        self.assertIn(
            "platform artifact does not preserve executable metadata",
            self.errors(release=unsafe),
        )

    def test_rejects_missing_pre_tag_package_dry_run(self) -> None:
        unsafe = self.full_gate.replace(
            "run_cmd python3 scripts/verify_release_packages.py source\n",
            "",
            1,
        )

        self.assertIn(
            "canonical gate lacks the pre-tag npm package dry-run",
            self.errors(full_gate=unsafe),
        )

    def test_rejects_online_npm_publish_dry_run(self) -> None:
        unsafe = self.package_verifier.replace('"--offline",', '"--online",', 1)

        self.assertIn(
            "package verifier does not force npm offline",
            self.errors(package_verifier=unsafe),
        )


if __name__ == "__main__":
    unittest.main()
