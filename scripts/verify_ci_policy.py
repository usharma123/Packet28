#!/usr/bin/env python3
"""Verify immutable CI inputs and locked repository Cargo commands."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW_DIR = ROOT / ".github" / "workflows"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")
USES = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)", re.MULTILINE)
CARGO_GRAPH_COMMAND = re.compile(
    r"""(?x)
    (?:
        ^ | :\s+ | run_cmd\s+ | &&\s+ | \|\|\s+ |
        ["']\s* | :-\s* | ^-\s+
    )
    cargo\s+(?:build|check|clippy|test|doc|package)\b
    """
)
BENCHMARK_CARGO_RUN_COMMAND = re.compile(
    r"""(?x)
    (?:
        ^ | :\s+ | run_cmd\s+ | &&\s+ | \|\|\s+ |
        ["']\s* | :-\s* | ^-\s+
    )
    cargo\s+run\b
    """
)
WORKFLOW_FILES = sorted(
    [*WORKFLOW_DIR.glob("*.yml"), *WORKFLOW_DIR.glob("*.yaml")]
)
ACTION_FILES = [
    *WORKFLOW_FILES,
    ROOT / "scripts" / "ci" / "github-actions.yml",
]

BENCHMARK_LOCKED_COMMAND_FILES = [
    ROOT / "benchmarks" / "per-03-incremental-index" / "README.md",
    ROOT / "benchmarks" / "run.sh",
    ROOT / "benchmarks" / "run_agent_search_bench.sh",
]

LOCKED_COMMAND_FILES = [
    *WORKFLOW_FILES,
    ROOT / "scripts" / "validate_refactor_batch.sh",
    ROOT / "scripts" / "validate_full_gate.sh",
    ROOT / "scripts" / "ci" / "codex_autofix.sh",
    ROOT / "scripts" / "ci" / "github-actions.yml",
    ROOT / "scripts" / "ci" / "gitlab-ci.yml",
    ROOT / "scripts" / "run_packet28_claude_code_experiment.sh",
    ROOT / "scripts" / "bench_apache.sh",
    ROOT / "scripts" / "test_token_usage.py",
    ROOT / "npm" / "build-npm.sh",
    ROOT / "README.md",
    *BENCHMARK_LOCKED_COMMAND_FILES,
]

CROSS_REVISION = "88f49ff79e777bef6d3564531636ee4d3cc2f8d2"
CARGO_DENY_VERSION = "0.20.2"
CARGO_DENY_CHECKSUMS = {
    "aarch64-apple-darwin": (
        "fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3"
    ),
    "aarch64-unknown-linux-musl": (
        "995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d"
    ),
    "x86_64-apple-darwin": (
        "248da7f581724e470071990c088ffc55c811981715f4cbdb258621fb79f8b7a6"
    ),
    "x86_64-unknown-linux-musl": (
        "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
    ),
}
CURRENT_RUST = "1.93.1"
MSRV_RUST = "1.88.0"
APPROVED_ACTION_REVISIONS = {
    "actions/checkout": {
        "11d5960a326750d5838078e36cf38b85af677262",
        "3d3c42e5aac5ba805825da76410c181273ba90b1",
        "fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09",
    },
    "actions/download-artifact": {
        "37930b1c2abaa49bbe596cd826c3c89aef350131",
        "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
    },
    "actions/setup-node": {
        "249970729cb0ef3589644e2896645e5dc5ba9c38",
        "820762786026740c76f36085b0efc47a31fe5020",
        "a0853c24544627f65ddf259abe73b1d18a591444",
    },
    "actions/upload-artifact": {
        "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
        "b7c566a772e6b6bfb58ed0dc250532a479d7789f",
    },
    "dtolnay/rust-toolchain": {
        "4cda84d5c5c54efe2404f9d843567869ab1699d4"
    },
    "github/codeql-action/upload-sarif": {
        "4187e74d05793876e9989daffde9c3e66b4acd07"
    },
    "Swatinem/rust-cache": {
        "f0d9c3887740aee45f6153b24b3a6b815192ec16"
    },
}


def relative(path: Path) -> str:
    return str(path.relative_to(ROOT))


def verify_action_references(errors: list[str]) -> None:
    count = 0
    for workflow in ACTION_FILES:
        text = workflow.read_text(encoding="utf-8")
        for value in USES.findall(text):
            count += 1
            if value.startswith("./"):
                continue
            if "@" not in value:
                errors.append(f"{relative(workflow)}: action has no revision: {value}")
                continue
            revision = value.rsplit("@", 1)[1]
            if not FULL_SHA.fullmatch(revision):
                errors.append(
                    f"{relative(workflow)}: action is not pinned to a full SHA: {value}"
                )
                continue
            action = value.rsplit("@", 1)[0]
            approved = APPROVED_ACTION_REVISIONS.get(action)
            if approved is None:
                errors.append(
                    f"{relative(workflow)}: action lacks an approved revision: {action}"
                )
            elif revision not in approved:
                errors.append(
                    f"{relative(workflow)}: action revision is not approved: {value}"
                )
    if count == 0:
        errors.append("no GitHub Action references were found")


def verify_rust_toolchains(errors: list[str]) -> None:
    msrv_count = 0
    for path in ACTION_FILES:
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        for index, line in enumerate(lines):
            if "uses: dtolnay/rust-toolchain@" not in line:
                continue
            nearby = "\n".join(lines[index + 1 : index + 6])
            matched = re.search(r"^\s*toolchain:\s*(\S+)\s*$", nearby, re.MULTILINE)
            if matched is None:
                errors.append(
                    f"{relative(path)}:{index + 1}: Rust action lacks exact toolchain input"
                )
                continue
            toolchain = matched.group(1)
            if toolchain == MSRV_RUST:
                msrv_count += 1
                if path != WORKFLOW_DIR / "build.yml":
                    errors.append(
                        f"{relative(path)}:{index + 1}: MSRV toolchain is only valid in build.yml"
                    )
            elif toolchain != CURRENT_RUST:
                errors.append(
                    f"{relative(path)}:{index + 1}: unreviewed Rust toolchain {toolchain}"
                )
        errors.extend(msrv_clippy_component_errors(path, text))
    if msrv_count != 1:
        errors.append(
            f"expected exactly one {MSRV_RUST} MSRV action, found {msrv_count}"
        )


def msrv_clippy_component_errors(path: Path, text: str) -> list[str]:
    """Return MSRV Rust actions that do not explicitly install Clippy."""

    errors: list[str] = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if "uses: dtolnay/rust-toolchain@" not in line:
            continue
        nearby = "\n".join(lines[index + 1 : index + 7])
        toolchain = re.search(
            r"^\s*toolchain:\s*(\S+)\s*$", nearby, re.MULTILINE
        )
        if toolchain is None or toolchain.group(1) != MSRV_RUST:
            continue
        components = re.search(
            r"^\s*components:\s*(.+?)\s*$", nearby, re.MULTILINE
        )
        installed = (
            {
                component.strip()
                for component in components.group(1).split(",")
            }
            if components is not None
            else set()
        )
        if "clippy" not in installed:
            errors.append(
                f"{relative(path)}:{index + 1}: MSRV toolchain does not install clippy"
            )
    return errors


def locked_command_errors(path: Path, text: str) -> list[str]:
    """Return unlocked Cargo graph commands from one policy-scanned file."""

    errors: list[str] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("echo "):
            continue
        cargo_graph_command = CARGO_GRAPH_COMMAND.search(stripped) or (
            path in BENCHMARK_LOCKED_COMMAND_FILES
            and BENCHMARK_CARGO_RUN_COMMAND.search(stripped)
        )
        if cargo_graph_command and "--locked" not in line:
            errors.append(
                f"{relative(path)}:{line_number}: Cargo graph command lacks --locked"
            )
        if re.search(r"\bcross\s+build\b", line) and "--locked" not in line:
            errors.append(
                f"{relative(path)}:{line_number}: cross build lacks --locked"
            )
    return errors


def verify_locked_commands(errors: list[str]) -> None:
    for path in LOCKED_COMMAND_FILES:
        if not path.exists():
            errors.append(f"required CI/gate file is missing: {relative(path)}")
            continue
        errors.extend(
            locked_command_errors(path, path.read_text(encoding="utf-8"))
        )


def autofix_security_errors(text: str) -> list[str]:
    """Return violations of the privileged autofix trust boundary."""

    errors: list[str] = []
    required_fragments = {
        "automatic runs are not restricted to the trusted default branch": (
            "github.event.workflow_run.head_branch == "
            "github.event.repository.default_branch"
        ),
        "trusted control checkout is missing": "path: trusted-control",
        "repair target checkout is not isolated": "path: target",
        "trusted autofix driver is not selected explicitly": (
            "TRUSTED_AUTOFIX: "
            "${{ github.workspace }}/trusted-control/scripts/ci/codex_autofix.sh"
        ),
        "trusted run validator is not selected explicitly": (
            "TRUSTED_RUN_VALIDATOR: "
            "${{ github.workspace }}/trusted-control/scripts/ci/validate_autofix_run.py"
        ),
        "manual run metadata is not validated before checkout": (
            'python3 "$TRUSTED_RUN_VALIDATOR"'
        ),
        "repair target is not derived from validated run metadata": (
            "ref: ${{ steps.resolve.outputs.target_ref }}"
        ),
        "validated run commit is not constrained to default-branch history": (
            "git -C trusted-control merge-base --is-ancestor"
        ),
        "repair target root is not bound explicitly": (
            "CODEX_AUTOFIX_ROOT: ${{ env.TARGET_ROOT }}"
        ),
        "candidate patch is not applied in a separate job": (
            "git apply --index --binary .packet28/ci-autofix/diff.patch"
        ),
        "publish job does not depend on the read-only autofix job": (
            "needs: autofix"
        ),
        "Codex CLI is not pinned to the reviewed version": (
            "npm install -g @openai/codex@0.145.0"
        ),
    }
    for message, fragment in required_fragments.items():
        if fragment not in text:
            errors.append(message)

    if text.count("persist-credentials: false") < 3:
        errors.append("every autofix checkout must discard persisted credentials")
    if text.count("OPENAI_API_KEY:") != 1:
        errors.append("OpenAI credentials must be scoped to exactly one execution step")
    if "inputs.ref" in text:
        errors.append("manual autofix may not accept an arbitrary repair ref")
    if re.search(r"^\s*run:\s*scripts/ci/codex_autofix\.sh\s*$", text, re.MULTILINE):
        errors.append("candidate checkout may not supply the executed autofix driver")
    if "git push --force" in text:
        errors.append("autofix publication may not force-push")
    if "pull_request_target" in text:
        errors.append("autofix may not use pull_request_target")

    autofix_job = text.partition("\n  autofix:")[2].partition("\n  publish:")[0]
    if not autofix_job:
        errors.append("read-only autofix job is missing")
    else:
        if "permissions:\n      actions: read\n      contents: read" not in autofix_job:
            errors.append("autofix execution job must have read-only permissions")
        if "contents: write" in autofix_job or "pull-requests: write" in autofix_job:
            errors.append("autofix execution job may not have write permissions")
        if "gh auth setup-git" in autofix_job:
            errors.append("autofix execution job may not configure write credentials")

    publish_job = text.partition("\n  publish:")[2]
    if not publish_job:
        errors.append("credential-isolated publish job is missing")
    else:
        if "contents: write" not in publish_job:
            errors.append("publish job lacks scoped contents permission")
        if "pull-requests: write" not in publish_job:
            errors.append("publish job lacks scoped pull-request permission")
        if "OPENAI_API_KEY" in publish_job:
            errors.append("publish job may not receive OpenAI credentials")

    return errors


def verify_autofix_security(errors: list[str]) -> None:
    autofix = (WORKFLOW_DIR / "codex-autofix.yml").read_text(encoding="utf-8")
    errors.extend(
        f"codex-autofix.yml: {error}" for error in autofix_security_errors(autofix)
    )
    driver = (ROOT / "scripts" / "ci" / "codex_autofix.sh").read_text(
        encoding="utf-8"
    )
    for fragment, message in (
        (
            "shell_environment_policy.ignore_default_excludes=false",
            "Codex subprocesses do not retain the default secret filter",
        ),
        (
            'shell_environment_policy.exclude=["OPENAI_API_KEY","GITHUB_TOKEN",'
            '"GH_TOKEN","ACTIONS_ID_TOKEN_REQUEST_TOKEN"]',
            "Codex subprocesses do not explicitly exclude CI credentials",
        ),
    ):
        if fragment not in driver:
            errors.append(f"codex_autofix.sh: {message}")
    if "CODEX_CMD" in driver:
        errors.append("codex_autofix.sh: arbitrary command override is forbidden")


def release_permission_errors(text: str) -> list[str]:
    """Return violations of the release workflow's least-privilege boundary."""

    errors: list[str] = []
    pre_jobs = text.partition("\njobs:")[0]
    release_gates = text.partition("\n  release-gates:")[2].partition("\n  build:")[0]
    build = text.partition("\n  build:")[2].partition("\n  publish:")[0]
    publish = text.partition("\n  publish:")[2]

    if "permissions:\n  contents: read" not in pre_jobs:
        errors.append("workflow default permission must be contents: read")
    if "contents: write" in pre_jobs or "id-token: write" in pre_jobs:
        errors.append("workflow-wide release permissions may not grant writes")

    for name, job in (("release-gates", release_gates), ("build", build)):
        if not job:
            errors.append(f"{name} job is missing")
            continue
        if "permissions:\n      contents: read" not in job:
            errors.append(f"{name} job must declare read-only contents")
        if "contents: write" in job or "id-token: write" in job:
            errors.append(f"{name} job may not receive publication permissions")

    if not publish:
        errors.append("publish job is missing")
    else:
        required_publish_permissions = (
            "actions: read",
            "contents: write",
            "id-token: write",
        )
        for permission in required_publish_permissions:
            if permission not in publish:
                errors.append(f"publish job lacks scoped {permission} permission")

    if text.count("contents: write") != 1:
        errors.append("contents write permission must occur only in the publish job")
    if text.count("id-token: write") != 1:
        errors.append("OIDC write permission must occur only in the publish job")

    return errors


def verify_release_permissions(errors: list[str]) -> None:
    release = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
    errors.extend(
        f"release.yml: {error}" for error in release_permission_errors(release)
    )


def dependabot_policy_errors(text: str) -> list[str]:
    """Return violations of the reviewed dependency-update lane."""

    errors: list[str] = []
    if not text.startswith("version: 2\n"):
        errors.append("Dependabot schema version must be 2")

    ecosystems = ('"cargo"', '"npm"', '"github-actions"')
    for ecosystem in ecosystems:
        marker = f'package-ecosystem: {ecosystem}'
        if text.count(marker) != 1:
            errors.append(
                f"Dependabot must configure {ecosystem} exactly once"
            )
    if text.count('interval: "weekly"') != len(ecosystems):
        errors.append("every dependency ecosystem must use a weekly review cadence")
    if text.count("open-pull-requests-limit: 5") != len(ecosystems):
        errors.append("every dependency ecosystem must bound open update PRs at five")
    if text.count('rebase-strategy: "auto"') != len(ecosystems):
        errors.append("every dependency ecosystem must refresh its locked update branch")

    required_fragments = {
        "Cargo updates must cover the workspace root": (
            '- package-ecosystem: "cargo"\n    directory: "/"'
        ),
        "npm updates must cover every reviewed package directory": (
            'directories:\n      - "/"\n      - "/npm/*"\n      - "/package"'
        ),
        "GitHub Actions updates must cover workflow files": (
            '- package-ecosystem: "github-actions"\n    directory: "/"'
        ),
        "compatible Rust updates must be grouped": "rust-compatible:",
        "compatible npm updates must be grouped": "npm-compatible:",
        "compatible action updates must be grouped": "actions-compatible:",
        "minor updates must be reviewed": '- "minor"',
        "patch updates must be reviewed": '- "patch"',
    }
    for message, fragment in required_fragments.items():
        if fragment not in text:
            errors.append(message)

    if "auto-merge" in text or "automerge" in text:
        errors.append("dependency updates must not bypass review through auto-merge")
    return errors


def verify_dependabot_policy(errors: list[str]) -> None:
    dependabot = (ROOT / ".github" / "dependabot.yml").read_text(encoding="utf-8")
    errors.extend(
        f"dependabot.yml: {error}"
        for error in dependabot_policy_errors(dependabot)
    )


def release_package_smoke_errors(
    build: str, release: str, full_gate: str, package_verifier: str
) -> list[str]:
    """Return violations of the pre-publish package verification boundary."""

    errors: list[str] = []
    package_job = build.partition("\n  packages:")[2].partition("\n  quality:")[0]
    release_gate_job = release.partition("\n  release-gates:")[2].partition(
        "\n  build:"
    )[0]
    release_build_job = release.partition("\n  build:")[2].partition(
        "\n  publish:"
    )[0]
    release_publish_job = release.partition("\n  publish:")[2]
    node_action = (
        "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020"
    )
    node_jobs = {
        "build package check": package_job,
        "release gate": release_gate_job,
        "release artifact build": release_build_job,
        "release publish": release_publish_job,
    }
    for job_name, job in node_jobs.items():
        version = "24.16.0" if job_name == "release publish" else "20.20.2"
        if node_action not in job or f"node-version: {version}" not in job:
            errors.append(f"{job_name} must pin the reviewed Node {version} action")
    if "NODE_AUTH_TOKEN:" in release_publish_job or "secrets.NPM_TOKEN" in release_publish_job:
        errors.append("release publish must use OIDC without an npm token")

    expected_modes = {
        "smoke_mode: native": 2,
        "smoke_mode: native-or-metadata": 1,
        "smoke_mode: qemu-aarch64": 1,
    }
    for fragment, expected_count in expected_modes.items():
        actual_count = len(
            re.findall(rf"^\s*{re.escape(fragment)}\s*$", release, re.MULTILINE)
        )
        if actual_count != expected_count:
            errors.append(
                f"release matrix expected {expected_count} occurrences of "
                f"{fragment!r}, found {actual_count}"
            )
    if (
        "x86_64 execution requires an Intel runner or Rosetta and remains an "
        "external release check."
        not in release
    ):
        errors.append("macOS x86_64 execution limitation is not explicit")

    required_release_fragments = {
        "Linux x64 preload shim must use the GNU target": (
            "shim_target: x86_64-unknown-linux-gnu"
        ),
        "Linux ARM64 preload shim must use the GNU target": (
            "shim_target: aarch64-unknown-linux-gnu"
        ),
        "preload shim must be built separately from static musl executables": (
            'cross build --release --locked --target "${{ matrix.shim_target }}" --package context-instruct-shim'
        ),
        "preload shim must be staged from its shared-library target": (
            'ARTIFACT_TARGET="${{ matrix.shim_target }}"'
        ),
        "Linux ARM64 emulator package is not installed": (
            "sudo apt-get install -y --no-install-recommends qemu-user"
        ),
        "Linux ARM64 emulator executable is not checked": (
            "command -v qemu-aarch64"
        ),
        "staged package path is not matrix-bound": (
            '--package-dir "dist/@packet28/${{ matrix.platform }}"'
        ),
        "staged package platform is not matrix-bound": (
            '--platform "${{ matrix.platform }}"'
        ),
        "staged package execution mode is not matrix-bound": (
            '--run-mode "${{ matrix.smoke_mode }}"'
        ),
        "staged package skip reason is not matrix-bound": (
            '--skip-reason "${{ matrix.smoke_reason }}"'
        ),
        "verified platform package is not archived": (
            'tar -C "$PKG_DIR" -czf "dist/pkg-${{ matrix.platform }}.tar.gz" .'
        ),
        "platform artifact does not preserve executable metadata": (
            "path: dist/pkg-${{ matrix.platform }}.tar.gz"
        ),
        "downloaded platform archive is not extracted": (
            'tar -xzf "$ARCHIVE" -C "$PKG_DIR"'
        ),
    }
    for message, fragment in required_release_fragments.items():
        if fragment not in release:
            errors.append(message)

    platform_verifier = "python3 scripts/verify_release_packages.py platform"
    if platform_verifier not in release_build_job:
        errors.append("staged platform verifier is not invoked in the build job")
    if platform_verifier not in release_publish_job:
        errors.append("downloaded platform packages are not revalidated")
    npm_verifier = "python3 scripts/verify_release_packages.py npm"
    if npm_verifier not in release_publish_job:
        errors.append("root npm package is not revalidated")

    smoke_position = release.find("- name: Smoke staged platform package")
    archive_position = release.find("- name: Archive verified platform package")
    upload_position = release.find("- name: Upload platform package")
    if (
        smoke_position < 0
        or archive_position < smoke_position
        or upload_position < archive_position
    ):
        errors.append("platform package smoke must run before artifact upload")
    extract_position = release.find(
        "- name: Extract platform packages with executable modes"
    )
    dry_run_position = release.find("- name: Dry-run every npm package")
    publish_position = release.find("- name: Publish platform packages")
    if (
        extract_position < 0
        or dry_run_position < extract_position
        or publish_position < 0
        or dry_run_position > publish_position
    ):
        errors.append("npm package dry-runs must run before publication")
    if "npm publish --dry-run" in release:
        errors.append(
            "release workflow may not bypass the offline package verifier"
        )

    if "run_cmd python3 scripts/verify_release_packages.py source" not in full_gate:
        errors.append("canonical gate lacks the pre-tag npm package dry-run")

    required_verifier_fragments = {
        "package verifier does not force npm offline": '"--offline"',
        "package verifier does not force npm dry-run": '"--dry-run"',
        "package verifier does not inspect binary headers": "def binary_identity(",
        "package verifier does not execute staged binaries": (
            "def smoke_platform_binaries("
        ),
        "package verifier does not check npm package integrity": (
            "pack.get(\"integrity\") == publish.get(\"integrity\")"
        ),
    }
    for message, fragment in required_verifier_fragments.items():
        if fragment not in package_verifier:
            errors.append(message)

    return errors


def verify_release_package_smoke(errors: list[str]) -> None:
    build = (WORKFLOW_DIR / "build.yml").read_text(encoding="utf-8")
    release = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
    full_gate = (ROOT / "scripts" / "validate_full_gate.sh").read_text(
        encoding="utf-8"
    )
    package_verifier = (ROOT / "scripts" / "verify_release_packages.py").read_text(
        encoding="utf-8"
    )
    errors.extend(
        f"release-package-smoke: {error}"
        for error in release_package_smoke_errors(
            build, release, full_gate, package_verifier
        )
    )


def audit_finalization_wiring_errors(full_gate: str) -> list[str]:
    release_gate = re.search(
        r'if \[\[ -n "\$release_tag" \]\]; then(?P<body>.*?)\nfi',
        full_gate,
        re.DOTALL,
    )
    release_body = (
        release_gate.group("body").replace("\\\n", " ")
        if release_gate is not None
        else ""
    )
    if release_gate is not None and re.search(
        r"run_cmd\s+python3\s+scripts/check_architecture_audit_ledger\.py\s+"
        r"--final\s+--source-rev\s+HEAD\^",
        release_body,
    ):
        return []
    return [
        "tag-aware canonical gate does not strictly finalize the audit "
        "ledger against HEAD^"
    ]


def direct_minimum_gate_errors(full_gate: str) -> list[str]:
    """Return violations of the alternate direct-minimum graph gate."""

    if "run_cmd python3 scripts/validate_direct_minimum.py" in full_gate:
        return []
    return [
        "canonical gate does not compile the committed direct-minimum graph"
    ]


def clean_runner_bootstrap_errors(
    full_gate: str,
    workspace_policy: str,
) -> list[str]:
    """Require every discovered workspace to fetch before offline metadata."""

    invocation = "run_cmd scripts/verify_workspace_policy.sh --bootstrap"
    if invocation not in full_gate:
        return [
            "canonical gate does not bootstrap every locked workspace"
        ]
    fetch = workspace_policy.find(
        'cargo fetch \\\n      --locked \\\n      --manifest-path "$manifest"'
    )
    metadata = workspace_policy.find(
        'cargo metadata \\\n    --locked \\\n    --offline \\\n'
        '    --manifest-path "$manifest"'
    )
    if fetch < 0:
        return [
            "workspace policy does not fetch each discovered locked manifest"
        ]
    if metadata < 0 or fetch > metadata:
        return [
            "workspace policy does not fetch each manifest before offline "
            "metadata"
        ]
    return []


def runtime_starvation_evidence_gate_errors(full_gate: str) -> list[str]:
    """Return violations of the checked-in ASY-04 evidence gate."""

    expected = (
        "run_cmd python3 "
        "benchmarks/asy-04-runtime-starvation/verify.py"
    )
    if expected in full_gate:
        return []
    return [
        "canonical gate does not verify the runtime-starvation evidence"
    ]


def incremental_index_evidence_gate_errors(full_gate: str) -> list[str]:
    """Return violations of the checked-in PER-03 evidence gate."""

    expected = (
        "run_cmd python3 "
        "benchmarks/per-03-incremental-index/verify.py"
    )
    if expected in full_gate:
        return []
    return [
        "canonical gate does not verify the incremental-index evidence"
    ]


def split_ci_wiring_errors(build: str) -> list[str]:
    """Keep all validation phases wired while preserving the required check."""
    errors = []
    for phase in ("policy", "lint", "tests", "docs", "audit", "dependencies", "packages"):
        if f"run: scripts/validate_full_gate.sh --phase {phase}" not in build:
            errors.append(f"build workflow lacks the {phase} phase")
    required = (
        "needs: [changes, policy, lint, tests, msrv, audit, dependencies, packages]",
        "run: python3 scripts/ci/select_checks.py --check-results",
        "NEEDS_JSON: ${{ toJSON(needs) }}",
        "  quality:\n    if: always()",
    )
    if any(fragment not in build for fragment in required):
        errors.append("quality must aggregate every job even after failure or cancellation")
    if re.search(r"run: scripts/validate_full_gate\.sh\s*$", build, re.MULTILINE):
        errors.append("PR validation must use phases instead of the serial full gate")
    return errors


def verify_workflow_wiring(errors: list[str]) -> None:
    build = (WORKFLOW_DIR / "build.yml").read_text(encoding="utf-8")
    release = (WORKFLOW_DIR / "release.yml").read_text(encoding="utf-8")
    full_gate = (ROOT / "scripts" / "validate_full_gate.sh").read_text(
        encoding="utf-8"
    )
    workspace_policy = (
        ROOT / "scripts" / "verify_workspace_policy.sh"
    ).read_text(encoding="utf-8")

    errors.extend(split_ci_wiring_errors(build))
    if "scripts/validate_full_gate.sh --msrv" not in build:
        errors.append("build workflow does not invoke the canonical MSRV gate")
    if 'scripts/validate_full_gate.sh --release-tag "$GITHUB_REF_NAME"' not in release:
        errors.append("release workflow does not run the tag-aware canonical gate")
    if "run_cmd python3 scripts/check_architecture.py" not in full_gate:
        errors.append("canonical gate does not run the architecture checker")
    if (
        "run_cmd python3 -m unittest discover -s scripts/tests"
        not in full_gate
    ):
        errors.append("canonical gate does not run the script unit tests")
    if "run_cmd python3 scripts/check_architecture_audit_ledger.py" not in full_gate:
        errors.append("canonical gate does not run the architecture-audit ledger checker")
    errors.extend(audit_finalization_wiring_errors(full_gate))
    if "run_cmd python3 scripts/check_instruction_claims.py" not in full_gate:
        errors.append("canonical gate does not run the instruction-claim checker")
    if "run_cmd python3 scripts/check_rust_hazards.py" not in full_gate:
        errors.append("canonical gate does not run the Rust hazard-policy checker")
    if "run_cmd python3 scripts/check_test_harness.py" not in full_gate:
        errors.append("canonical gate does not run the test-harness policy checker")
    errors.extend(runtime_starvation_evidence_gate_errors(full_gate))
    errors.extend(incremental_index_evidence_gate_errors(full_gate))
    errors.extend(direct_minimum_gate_errors(full_gate))
    errors.extend(clean_runner_bootstrap_errors(full_gate, workspace_policy))
    if "run_cmd cargo deny --locked check" not in full_gate:
        errors.append("canonical gate does not run cargo-deny against the lockfile")

    expected_cross = (
        "cargo install cross --git https://github.com/cross-rs/cross "
        f"--rev {CROSS_REVISION} --locked"
    )
    if expected_cross not in release:
        errors.append("release cross installer is not pinned to the reviewed revision")
    if 'node-version: "22.23.1"' not in (
        WORKFLOW_DIR / "codex-autofix.yml"
    ).read_text(encoding="utf-8"):
        errors.append("Codex autofix Node toolchain is not pinned to 22.23.1")
    if "node-version: 20.20.2" not in release:
        errors.append("release Node toolchain is not pinned to 20.20.2")

    gitlab = (ROOT / "scripts" / "ci" / "gitlab-ci.yml").read_text(
        encoding="utf-8"
    )
    expected_gitlab_image = (
        "image: rust:1.93.1@sha256:"
        "ecbe59a8408895edd02d9ef422504b8501dd9fa1526de27a45b73406d734d659"
    )
    if expected_gitlab_image not in gitlab:
        errors.append("GitLab Rust image is not pinned by exact version and digest")

    for workflow_name in ("build.yml", "release.yml"):
        text = (WORKFLOW_DIR / workflow_name).read_text(encoding="utf-8")
        if 'scripts/install_cargo_deny.sh "$RUNNER_TEMP/cargo-deny-bin"' not in text:
            errors.append(
                f"{workflow_name} does not use the checksum-verifying cargo-deny installer"
            )
        if 'echo "$RUNNER_TEMP/cargo-deny-bin" >> "$GITHUB_PATH"' not in text:
            errors.append(f"{workflow_name} does not expose the verified cargo-deny")

    installer = (ROOT / "scripts" / "install_cargo_deny.sh").read_text(
        encoding="utf-8"
    )
    if f'version="{CARGO_DENY_VERSION}"' not in installer:
        errors.append("cargo-deny installer version is not the reviewed exact version")
    for target, checksum in CARGO_DENY_CHECKSUMS.items():
        if target not in installer or checksum not in installer:
            errors.append(
                f"cargo-deny installer lacks reviewed checksum for {target}"
            )
    required_installer_fragments = (
        "--proto '=https' --tlsv1.2",
        '"$actual_sha256" != "$expected_sha256"',
        '"$destination/cargo-deny" --version',
    )
    for fragment in required_installer_fragments:
        if fragment not in installer:
            errors.append(
                f"cargo-deny installer lacks integrity invariant: {fragment}"
            )


def main() -> int:
    errors: list[str] = []
    verify_action_references(errors)
    verify_rust_toolchains(errors)
    verify_locked_commands(errors)
    verify_autofix_security(errors)
    verify_release_permissions(errors)
    verify_dependabot_policy(errors)
    verify_release_package_smoke(errors)
    verify_workflow_wiring(errors)
    if errors:
        for error in errors:
            print(f"ci policy invariant failed: {error}", file=sys.stderr)
        return 1
    print("ci policy invariant passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
