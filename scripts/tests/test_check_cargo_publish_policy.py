import copy
import importlib.util
import io
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check_cargo_publish_policy.py"
SPEC = importlib.util.spec_from_file_location("check_cargo_publish_policy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
publish_policy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publish_policy)

PACKAGE_SCRIPT = ROOT / "scripts" / "package_cargo_workspace.py"
PACKAGE_SPEC = importlib.util.spec_from_file_location(
    "package_cargo_workspace", PACKAGE_SCRIPT
)
assert PACKAGE_SPEC is not None and PACKAGE_SPEC.loader is not None
package_workspace = importlib.util.module_from_spec(PACKAGE_SPEC)
PACKAGE_SPEC.loader.exec_module(package_workspace)


def package(
    name,
    *,
    publish=None,
    description="Private Packet28 workspace component",
    dependencies=None,
):
    return {
        "id": name,
        "name": name,
        "version": "0.2.63",
        "publish": [] if publish is None else publish,
        "license": "MIT",
        "repository": "https://github.com/usharma123/Covy",
        "homepage": "https://github.com/usharma123/Covy",
        "description": description,
        "rust_version": "1.88.0",
        "readme": "README.md",
        "keywords": ["developer-tools"],
        "categories": ["development-tools"],
        "manifest_path": f"/tmp/{name}/Cargo.toml",
        "dependencies": [] if dependencies is None else dependencies,
    }


def metadata(*packages):
    return {
        "workspace_members": [candidate["id"] for candidate in packages],
        "packages": list(packages),
    }


def policy(*, published=(), private=("private-core",), order=()):
    return {
        "schema_version": 1,
        "registry": "crates-io",
        "metadata": {
            "license": "MIT",
            "repository": "https://github.com/usharma123/Covy",
            "homepage": "https://github.com/usharma123/Covy",
            "generic_descriptions": [
                "Fast Rust CLI for coverage and diagnostics gating"
            ],
        },
        "publish": {
            "packages": sorted(published),
            "order": list(order),
        },
        "private": {"packages": sorted(private)},
        "package_files": {
            "forbidden_components": [".git", ".packet28", "target"],
            "forbidden_names": [".env", "credentials.json", "id_rsa"],
            "forbidden_suffixes": [".key", ".p12", ".pem"],
        },
    }


def internal_dependency(name, requirement="^0.2.0", kind=None):
    return {
        "name": name,
        "path": f"/tmp/{name}",
        "req": requirement,
        "kind": kind,
    }


class CargoPublishPolicyTests(unittest.TestCase):
    def test_repository_publish_decision_and_package_files_are_valid(self):
        cargo_metadata = publish_policy.load_metadata(ROOT)
        current_policy = publish_policy.load_policy(ROOT)
        packages = publish_policy.workspace_packages(cargo_metadata)
        package_files = publish_policy.collect_package_files(ROOT, packages)

        self.assertEqual(
            publish_policy.policy_errors(
                cargo_metadata,
                current_policy,
                package_files,
            ),
            [],
        )

    def test_every_workspace_dependency_requires_local_path_and_bounded_version(self):
        cargo_metadata = publish_policy.load_metadata(ROOT)
        current_policy = publish_policy.load_policy(ROOT)
        packages = publish_policy.workspace_packages(cargo_metadata)
        locations = {}
        for owner, candidate in packages.items():
            for index, dependency in enumerate(candidate["dependencies"]):
                dependency_name = dependency.get("name")
                if dependency_name in packages:
                    locations.setdefault(dependency_name, (owner, index))

        self.assertIn("packet28-binary-codec", locations)
        self.assertIn("packet28-daemon-protocol", locations)

        for dependency_name, (owner, index) in sorted(locations.items()):
            with self.subTest(dependency=dependency_name, mutation="missing-path"):
                mutated = copy.deepcopy(cargo_metadata)
                mutated_packages = publish_policy.workspace_packages(mutated)
                mutated_packages[owner]["dependencies"][index]["path"] = None
                errors = publish_policy.policy_errors(mutated, current_policy)
                self.assertIn(
                    f"{owner}: internal dependency {dependency_name} must resolve "
                    "through its workspace path",
                    errors,
                )

            with self.subTest(dependency=dependency_name, mutation="wildcard"):
                mutated = copy.deepcopy(cargo_metadata)
                mutated_packages = publish_policy.workspace_packages(mutated)
                mutated_packages[owner]["dependencies"][index]["req"] = "*"
                errors = publish_policy.policy_errors(mutated, current_policy)
                self.assertIn(
                    f"{owner}: internal dependency {dependency_name} requirement "
                    "'*' is unconstrained",
                    errors,
                )

    def test_packaged_dashboard_fixtures_match_documentation_evidence(self):
        names = (
            "history.jsonl",
            "hidden-samples-delimiters.json",
            "hidden-samples-delimiters.summary",
        )
        for name in names:
            with self.subTest(name=name):
                self.assertEqual(
                    (
                        ROOT
                        / "crates"
                        / "suite-cli"
                        / "tests"
                        / "fixtures"
                        / "context_anomalies"
                        / name
                    ).read_bytes(),
                    (ROOT / "docs" / "context-anomalies" / name).read_bytes(),
                )

    def test_missing_public_metadata_is_rejected(self):
        public = package(
            "public-api",
            publish=["crates-io"],
            description="Fast Rust CLI for coverage and diagnostics gating",
        )
        public["readme"] = None
        public["keywords"] = []
        public["categories"] = []

        errors = publish_policy.policy_errors(
            metadata(public),
            policy(published=("public-api",), private=(), order=("public-api",)),
        )

        self.assertIn("public-api: public description is generic, not crate-specific", errors)
        self.assertIn("public-api: public package readme metadata is missing", errors)
        self.assertIn("public-api: public package needs one to five keywords", errors)
        self.assertIn("public-api: public package needs one to five categories", errors)

    def test_accidental_private_publish_enablement_is_rejected(self):
        private = package("private-core")
        private["publish"] = None

        self.assertIn(
            "private-core: private package must set publish = false",
            publish_policy.policy_errors(metadata(private), policy()),
        )

    def test_unpublished_dependency_and_wrong_order_are_rejected(self):
        core = package("private-core")
        application = package(
            "public-app",
            publish=["crates-io"],
            dependencies=[internal_dependency("private-core")],
        )
        errors = publish_policy.policy_errors(
            metadata(application, core),
            policy(
                published=("public-app",),
                private=("private-core",),
                order=("public-app",),
            ),
        )
        self.assertIn(
            "public-app: public package has unpublished normal dependency private-core",
            errors,
        )

        core["publish"] = ["crates-io"]
        errors = publish_policy.policy_errors(
            metadata(application, core),
            policy(
                published=("private-core", "public-app"),
                private=(),
                order=("public-app", "private-core"),
            ),
        )
        self.assertIn(
            "publish.order places public-app before its private-core dependency",
            errors,
        )

    def test_incompatible_internal_dependency_version_is_rejected(self):
        core = package("private-core")
        application = package(
            "private-app",
            dependencies=[internal_dependency("private-core", "^0.3.0")],
        )

        self.assertIn(
            "private-app: internal dependency private-core requirement "
            "'^0.3.0' excludes workspace version '0.2.63'",
            publish_policy.policy_errors(
                metadata(application, core),
                policy(private=("private-app", "private-core")),
            ),
        )

    def test_malformed_internal_dependency_version_is_rejected(self):
        self.assertFalse(
            publish_policy.requirement_allows("^0.2.0garbage", "0.2.63")
        )
        self.assertFalse(
            publish_policy.requirement_allows("^0.2.0", "0.2.63garbage")
        )

    def test_sensitive_or_escaping_package_files_are_rejected(self):
        errors = publish_policy.package_file_errors(
            "private-core",
            (
                "src/lib.rs",
                ".env",
                "certificates/release.pem",
                "target/debug/private-core",
                "../outside.txt",
            ),
            policy()["package_files"],
        )

        self.assertTrue(any("sensitive file would enter package: .env" in error for error in errors))
        self.assertTrue(
            any("sensitive file would enter package: certificates/release.pem" in error for error in errors)
        )
        self.assertTrue(
            any("forbidden component 'target'" in error for error in errors)
        )
        self.assertTrue(any("package path escapes its crate" in error for error in errors))

    def test_unclassified_and_unknown_packages_are_rejected(self):
        current = package("private-core")
        invalid_policy = copy.deepcopy(policy())
        invalid_policy["private"]["packages"] = ["removed-core"]

        errors = publish_policy.policy_errors(metadata(current), invalid_policy)

        self.assertIn("workspace packages lack a publish decision: private-core", errors)
        self.assertIn("publish policy names unknown packages: removed-core", errors)

        invalid_policy = policy(
            published=("removed-core",),
            private=("private-core",),
            order=("removed-core",),
        )
        errors = publish_policy.policy_errors(metadata(current), invalid_policy)
        self.assertIn("publish policy names unknown packages: removed-core", errors)

    def test_public_readme_cannot_escape_crate(self):
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "crate"
            crate.mkdir()
            (Path(directory) / "README.md").write_text("outside\n", encoding="utf-8")
            public = package("public-api", publish=["crates-io"])
            public["manifest_path"] = str(crate / "Cargo.toml")
            public["readme"] = "../README.md"

            errors = publish_policy.policy_errors(
                metadata(public),
                policy(
                    published=("public-api",),
                    private=(),
                    order=("public-api",),
                ),
            )

        self.assertIn(
            "public-api: public package readme escapes its crate: ../README.md",
            errors,
        )

    def test_package_symlink_cannot_escape_crate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "crate"
            crate.mkdir()
            outside = root / "outside.txt"
            outside.write_text("secret\n", encoding="utf-8")
            (crate / "linked.txt").symlink_to(outside)
            candidate = package("private-core")
            candidate["manifest_path"] = str(crate / "Cargo.toml")

            errors = publish_policy.package_symlink_errors(
                candidate,
                ("linked.txt",),
            )

        self.assertEqual(
            errors,
            ["private-core: package symlink escapes its crate: linked.txt"],
        )

    def test_disposable_manifest_enables_only_package_verification(self):
        source = (
            "[package]\n"
            'name = "private-core"\n'
            "version.workspace = true\n"
            "publish.workspace = true\n"
        )

        transformed = package_workspace.verification_manifest(source)

        self.assertIn('publish = ["crates-io"]', transformed)
        self.assertNotIn("publish.workspace = true", transformed)
        command = package_workspace.package_command()
        self.assertEqual(command[:2], ("cargo", "package"))
        self.assertIn("--workspace", command)
        self.assertIn("--all-features", command)
        self.assertIn("--locked", command)
        self.assertIn("--offline", command)
        self.assertIn("--no-verify", command)
        self.assertNotIn("publish", command)
        packaged_check = package_workspace.packaged_check_command()
        self.assertEqual(packaged_check[:2], ("cargo", "check"))
        self.assertIn("--workspace", packaged_check)
        self.assertIn("--all-targets", packaged_check)
        self.assertIn("--all-features", packaged_check)
        self.assertIn("--locked", packaged_check)
        self.assertIn("--offline", packaged_check)

    def test_disposable_manifest_rejects_missing_or_ambiguous_guard(self):
        with self.assertRaisesRegex(ValueError, "exactly one publish policy line"):
            package_workspace.verification_manifest(
                '[package]\nname = "private-core"\n'
            )
        with self.assertRaisesRegex(ValueError, "exactly one publish policy line"):
            package_workspace.verification_manifest(
                "[package]\n"
                'name = "private-core"\n'
                "publish.workspace = true\n"
                "publish = false\n"
            )

    def test_archive_paths_cannot_escape_destination(self):
        with self.assertRaisesRegex(ValueError, "unsafe Cargo package archive path"):
            package_workspace.archive_relative_path(
                "private-core-0.2.63/../secret.txt",
                "private-core-0.2.63",
            )
        with self.assertRaisesRegex(ValueError, "unsafe Cargo package archive path"):
            package_workspace.archive_relative_path(
                r"private-core-0.2.63\\..\\secret.txt",
                "private-core-0.2.63",
            )

    def test_preparing_mirror_does_not_change_guarded_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source"
            mirror = Path(directory) / "mirror"
            source_manifest = root / "crates" / "private-core" / "Cargo.toml"
            mirror_manifest = mirror / "crates" / "private-core" / "Cargo.toml"
            source_manifest.parent.mkdir(parents=True)
            mirror_manifest.parent.mkdir(parents=True)
            manifest = (
                "[package]\n"
                'name = "private-core"\n'
                "publish.workspace = true\n"
            )
            source_manifest.write_text(manifest, encoding="utf-8")
            mirror_manifest.write_text(manifest, encoding="utf-8")
            packages = {
                "private-core": {
                    "name": "private-core",
                    "manifest_path": str(source_manifest),
                }
            }

            package_workspace.prepare_verification_manifests(
                root,
                mirror,
                packages,
                {"private-core"},
            )

            self.assertEqual(source_manifest.read_text(encoding="utf-8"), manifest)
            self.assertIn(
                'publish = ["crates-io"]',
                mirror_manifest.read_text(encoding="utf-8"),
            )

    def test_inherited_target_directory_cannot_reroute_package_archives(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            root = base / "source"
            inherited_target = base / "inherited-target"
            manifest = (
                "[package]\n"
                'name = "private-core"\n'
                'version = "0.2.63"\n'
                "publish.workspace = true\n"
            )
            files = {
                Path("Cargo.toml"): '[workspace]\nmembers = ["crates/private-core"]\n',
                Path("Cargo.lock"): "version = 4\n",
                Path("crates/private-core/Cargo.toml"): manifest,
                Path("crates/private-core/src/lib.rs"): "pub fn core() {}\n",
            }
            for relative, text in files.items():
                (root / relative).parent.mkdir(parents=True, exist_ok=True)
                (root / relative).write_text(text, encoding="utf-8")
            source_before = {
                path.relative_to(root): path.read_bytes()
                for path in root.rglob("*")
                if path.is_file()
            }
            candidate = package("private-core")
            candidate["manifest_path"] = str(root / "crates/private-core/Cargo.toml")
            packages = {"private-core": candidate}
            calls = []
            recovered = {}

            def fake_cargo(command, *, cwd, env, check):
                command = tuple(command)
                calls.append((command, Path(cwd), dict(env)))
                # Mirror Cargo's precedence: flag, then environment, then default.
                if "--target-dir" in command:
                    target = Path(command[command.index("--target-dir") + 1])
                elif "CARGO_TARGET_DIR" in env:
                    target = Path(env["CARGO_TARGET_DIR"])
                else:
                    target = Path(cwd) / "target"
                if command[:2] == ("cargo", "package"):
                    mirror_manifest = Path(cwd) / "crates/private-core/Cargo.toml"
                    original = mirror_manifest.read_bytes()
                    archive = target / "package" / "private-core-0.2.63.crate"
                    archive.parent.mkdir(parents=True)
                    with tarfile.open(archive, mode="w:gz") as crate:
                        for name, data in (
                            ("Cargo.toml", b"# normalized\n"),
                            ("Cargo.toml.orig", original),
                            ("src/lib.rs", b"pub fn core() {}\n"),
                        ):
                            member = tarfile.TarInfo(f"private-core-0.2.63/{name}")
                            member.size = len(data)
                            member.mode = 0o644
                            crate.addfile(member, io.BytesIO(data))
                else:
                    crate_root = Path(cwd) / "crates/private-core"
                    recovered.update(
                        manifest=(crate_root / "Cargo.toml").read_text(encoding="utf-8"),
                        leftover_orig=(crate_root / "Cargo.toml.orig").exists(),
                    )
                return mock.Mock(returncode=0)

            policy_module = package_workspace.publish_policy
            environment = {
                "CARGO_TARGET_DIR": str(inherited_target),
                "CARGO_BUILD_TARGET_DIR": str(inherited_target),
                "CARGO_REGISTRY_TOKEN": "secret",
                "CARGO_REGISTRIES_CRATES_IO_TOKEN": "secret",
            }
            with (
                mock.patch.dict(os.environ, environment),
                mock.patch.object(policy_module, "load_metadata", return_value={}),
                mock.patch.object(
                    policy_module, "load_policy", return_value=policy()
                ),
                mock.patch.object(
                    policy_module, "workspace_packages", return_value=packages
                ),
                mock.patch.object(policy_module, "collect_package_files"),
                mock.patch.object(policy_module, "policy_errors", return_value=[]),
                mock.patch.object(
                    package_workspace,
                    "workspace_inputs",
                    return_value=sorted(files),
                ),
                mock.patch.object(
                    package_workspace.subprocess, "run", side_effect=fake_cargo
                ),
            ):
                package_workspace.verify_packages(root)

            self.assertEqual([call[0][:2] for call in calls], [
                ("cargo", "package"),
                ("cargo", "check"),
            ])
            (package_call, mirror, _), (check_call, packaged, _) = calls
            package_target = Path(package_call[package_call.index("--target-dir") + 1])
            self.assertEqual(package_target.parent, mirror.parent)
            self.assertNotEqual(package_target, inherited_target)
            self.assertEqual(
                Path(check_call[check_call.index("--target-dir") + 1]),
                root / "target" / "cargo-package-archive-check",
            )
            self.assertEqual(packaged, mirror.parent / "packaged-workspace")
            self.assertEqual(
                recovered,
                {
                    "manifest": package_workspace.verification_manifest(manifest),
                    "leftover_orig": False,
                },
            )
            for _command, _cwd, child_environment in calls:
                for variable in environment:
                    self.assertNotIn(variable, child_environment)
            self.assertFalse(inherited_target.exists())
            self.assertFalse(mirror.parent.exists())
            self.assertEqual(
                {
                    path.relative_to(root): path.read_bytes()
                    for path in root.rglob("*")
                    if path.is_file()
                },
                source_before,
            )

if __name__ == "__main__":
    unittest.main()
