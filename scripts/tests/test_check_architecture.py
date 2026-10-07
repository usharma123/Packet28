from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Callable, Iterable


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check_architecture.py"
REQUIRED_PACKAGES = (
    "packet28-daemon-protocol",
    "packet28-daemon-client",
    "context-instruct-shim",
    "packet28-daemon-core",
    "suite-packet-core",
    "context-kernel-builtins",
    "context-kernel-core",
    "context-kernel-mechanism",
    "context-memory-core",
    "packet28-reducer-core",
    "packet28-search-core",
    "packet28-search-cli",
)
KERNEL_BUILTIN_TARGETS = (
    "agenty.state.snapshot",
    "agenty.state.write",
    "buildy.reduce",
    "contextq.assemble",
    "contextq.correlate",
    "contextq.manage",
    "diffy.analyze",
    "governed.assemble",
    "guardy.check",
    "mapy.query",
    "mapy.repo",
    "packet28.broker_memory.write",
    "packet28.instruction.summarize",
    "proxy.run",
    "stacky.slice",
    "testy.impact",
)
PACKET28D_BROKER_MODULES = (
    "context",
    "handoff",
    "limits",
    "ops",
    "render",
    "search",
    "search_plan",
    "snapshot",
    "support",
)
PROTOCOL_PUBLIC_MODULES = (
    "broker",
    "commands",
    "context_store",
    "frame",
    "hooks",
    "index",
    "logging",
    "message",
    "paths",
    "process",
    "registry",
    "task",
)
CORE_PUBLIC_MODULES = (
    "integrity",
    "retention",
    "storage",
    "task_store_lease",
    "trust",
)
CLIENT_PUBLIC_MODULES = (
    "runtime_discovery",
    "transport",
)


def package(name: str, *, binary: bool = False) -> dict[str, object]:
    kinds = ["bin"] if binary else ["lib"]
    return {
        "id": name,
        "name": name,
        "targets": [{"name": name, "kind": kinds}],
    }


def dependency(name: str, kind: str | None = None) -> dict[str, object]:
    return {
        "name": name.replace("-", "_"),
        "pkg": name,
        "dep_kinds": [{"kind": kind, "target": None}],
    }


def metadata(
    edges: dict[str, Iterable[tuple[str, str | None]]],
    *,
    additional_packages: Iterable[str] = (),
    include_builtin_mechanism_edge: bool = True,
) -> dict[str, object]:
    edges = {source: list(dependencies) for source, dependencies in edges.items()}
    edges.setdefault("context-kernel-core", []).append(
        ("context-kernel-builtins", None)
    )
    if include_builtin_mechanism_edge:
        edges.setdefault("context-kernel-builtins", []).append(
            ("context-kernel-mechanism", None)
        )
    package_names = set(REQUIRED_PACKAGES)
    package_names.update(additional_packages)
    for source, dependencies in edges.items():
        package_names.add(source)
        package_names.update(target for target, _kind in dependencies)

    packages = [
        package(name, binary=name == "packet28-search-cli")
        for name in sorted(package_names)
    ]
    nodes = [
        {
            "id": name,
            "deps": [
                dependency(target, kind)
                for target, kind in edges.get(name, ())
            ],
        }
        for name in sorted(package_names)
    ]
    return {
        "packages": packages,
        "workspace_members": sorted(package_names),
        "resolve": {"nodes": nodes},
    }


def write_kernel_sources(root: Path, mechanism_source: str) -> None:
    mechanism = root / "crates" / "context-kernel-mechanism" / "src"
    builtins = root / "crates" / "context-kernel-builtins" / "src"
    mechanism.mkdir(parents=True)
    builtins.mkdir(parents=True)
    (mechanism / "lib.rs").write_text(mechanism_source, encoding="utf-8")
    (builtins / "kernel_registry.rs").write_text(
        "\n".join(
            f'kernel.register_reducer("{target}");'
            for target in KERNEL_BUILTIN_TARGETS
        ),
        encoding="utf-8",
    )


def write_packet28d_sources(
    root: Path,
    *,
    main_source: str = "fn main() { packet28d::serve(Default::default()); }\n",
    broker_child_source: str = "use crate::state::DaemonState;\n",
) -> None:
    source = root / "crates" / "packet28d" / "src"
    broker = source / "broker"
    broker.mkdir(parents=True)
    (source / "main.rs").write_text(main_source, encoding="utf-8")
    (source / "application.rs").write_text(
        "pub fn serve() {}\npub fn serve_with_managed_log() {}\n",
        encoding="utf-8",
    )
    (source / "bootstrap.rs").write_text(
        "pub fn start() {}\n", encoding="utf-8"
    )
    (source / "lib.rs").write_text(
        "mod application;\n"
        "mod bootstrap;\n"
        "mod broker;\n"
        "pub use application::serve;\n"
        "pub use application::serve_with_managed_log;\n"
        "pub use bootstrap::start;\n"
        '#[cfg(feature = "shared-repository-scan")]\n'
        "pub mod shared_repository_scan;\n",
        encoding="utf-8",
    )
    (source / "shared_repository_scan.rs").write_text(
        "//! ```no_run\n"
        "//! rebuild_full_indexes_with_shared_scan(());\n"
        "//! let _ = result.telemetry.walk_passes;\n"
        "//! ```\n",
        encoding="utf-8",
    )
    (broker / "mod.rs").write_text(
        "\n".join(f"mod {module};" for module in PACKET28D_BROKER_MODULES)
        + "\npub(crate) use context::broker_get_context;\n",
        encoding="utf-8",
    )
    for module in PACKET28D_BROKER_MODULES:
        (broker / f"{module}.rs").write_text(
            broker_child_source, encoding="utf-8"
        )

    protocol = root / "crates" / "packet28-daemon-protocol" / "src"
    protocol.mkdir(parents=True)
    (protocol / "lib.rs").write_text(
        "\n".join(f"pub mod {module};" for module in PROTOCOL_PUBLIC_MODULES),
        encoding="utf-8",
    )
    (protocol / "frame.rs").write_text(
        "//! ```\n"
        "//! write_frame(()); read_frame(());\n"
        "//! let _ = DaemonRequest::Status;\n"
        "//! let _ = DaemonResponse::Ack;\n"
        "//! ```\n",
        encoding="utf-8",
    )
    (protocol / "registry.rs").write_text(
        "//! ```\n"
        "//! let request = DaemonRegistryRequestV1::TaskListPage {\n"
        "//!     request: TaskListPageRequestV1::default(),\n"
        "//! };\n"
        "//! let _ = serde_json::from_value::<DaemonRequest>(request);\n"
        "//! ```\n",
        encoding="utf-8",
    )
    (protocol / "task.rs").write_text(
        "/// ```\n"
        "/// let mut lifecycle = TaskLifecycle::Idle;\n"
        "/// lifecycle.start()?;\n"
        "/// lifecycle.finish_run()?;\n"
        "/// ```\n"
        "/// ```compile_fail\n"
        "/// let _ = TaskLifecycle {\n"
        "///     running: true, cancel_requested: true,\n"
        "/// };\n"
        "/// ```\n",
        encoding="utf-8",
    )

    core = root / "crates" / "packet28-daemon-core" / "src"
    core.mkdir(parents=True)
    (core / "lib.rs").write_text(
        "//! ```\n"
        "//! save_task_watch_registry_checkpoint(\n"
        "//!     (),\n"
        "//!     &TaskRegistry::default(),\n"
        "//!     &WatchRegistry::default(),\n"
        "//! );\n"
        "//! load_task_registry(());\n"
        "//! ```\n"
        "//! ```compile_fail\n"
        "//! use packet28_daemon_core::write_frame;\n"
        "//! ```\n"
        + "\n".join(f"pub mod {module};" for module in CORE_PUBLIC_MODULES),
        encoding="utf-8",
    )
    (core / "error.rs").write_text(
        "/// ```\n"
        "/// let _ = DaemonCoreError::Frame;\n"
        "/// assert!(error.source().is_some());\n"
        "/// ```\n",
        encoding="utf-8",
    )

    client = root / "crates" / "packet28-daemon-client" / "src"
    client.mkdir(parents=True)
    (client / "lib.rs").write_text(
        "//! ```\n"
        "//! let runtime = read_runtime_info_if_present(());\n"
        "//! assert!(runtime.is_none());\n"
        "//! ```\n"
        + "\n".join(f"pub mod {module};" for module in CLIENT_PUBLIC_MODULES),
        encoding="utf-8",
    )

    docs = root / "docs"
    docs.mkdir()
    (docs / "daemon-runtime.md").write_text(
        (ROOT / "docs" / "daemon-runtime.md").read_text(encoding="utf-8"),
        encoding="utf-8",
    )


class ArchitectureDependencyTests(unittest.TestCase):
    def run_checker(
        self, fixture: dict[str, object], *, source_root: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metadata.json"
            path.write_text(json.dumps(fixture), encoding="utf-8")
            command = [sys.executable, str(SCRIPT), "--metadata", str(path)]
            if source_root is not None:
                command.extend(["--source-root", str(source_root)])
            return subprocess.run(
                command,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

    def run_packet28d_mutation(
        self,
        relative_path: str,
        mutate: Callable[[str], str],
    ) -> subprocess.CompletedProcess[str]:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(root)
            path = root / relative_path
            path.write_text(mutate(path.read_text(encoding="utf-8")), encoding="utf-8")
            return self.run_checker(fixture, source_root=root)

    def test_good_graph_passes_and_ignores_non_normal_edges(self) -> None:
        fixture = metadata(
            {
                "packet28-daemon-protocol": [
                    ("suite-packet-core", None),
                    ("packet28-daemon-core", "dev"),
                    ("context-kernel-core", "build"),
                ],
                "context-instruct-shim": [
                    ("packet28-daemon-client", None),
                ],
                "packet28-daemon-client": [
                    ("packet28-daemon-protocol", None),
                ],
                "packet28-daemon-core": [
                    ("packet28-daemon-protocol", None),
                ],
                "packet28-search-cli": [
                    ("packet28-daemon-client", None),
                ],
            }
        )

        result = self.run_checker(fixture)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("architecture dependency invariant passed", result.stdout)

    def test_forbidden_direct_dependency_fails(self) -> None:
        fixture = metadata(
            {
                "packet28-daemon-protocol": [
                    ("packet28-daemon-core", None),
                ],
            }
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28-daemon-protocol reaches forbidden normal dependency "
            "packet28-daemon-core",
            result.stderr,
        )
        self.assertIn(
            "packet28-daemon-protocol -> packet28-daemon-core",
            result.stderr,
        )

    def test_daemon_client_must_not_reach_daemon_runtime(self) -> None:
        fixture = metadata(
            {
                "packet28-daemon-client": [
                    ("packet28-daemon-protocol", None),
                    ("adapter-bridge", None),
                ],
                "adapter-bridge": [("packet28-daemon-core", None)],
            },
            additional_packages=("adapter-bridge",),
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28-daemon-client reaches forbidden normal dependency "
            "packet28-daemon-core",
            result.stderr,
        )
        self.assertIn(
            "packet28-daemon-client -> adapter-bridge -> packet28-daemon-core",
            result.stderr,
        )

    def test_forbidden_transitive_dependency_fails_with_full_path(self) -> None:
        fixture = metadata(
            {
                "context-instruct-shim": [("adapter-bridge", None)],
                "adapter-bridge": [("context-memory-core", None)],
            },
            additional_packages=("adapter-bridge",),
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-instruct-shim reaches forbidden normal dependency "
            "context-memory-core",
            result.stderr,
        )
        self.assertIn(
            "context-instruct-shim -> adapter-bridge -> context-memory-core",
            result.stderr,
        )

    def test_packet28_search_cli_may_depend_on_reducer_and_search(self) -> None:
        fixture = metadata(
            {
                "packet28-daemon-protocol": [
                    ("suite-packet-core", None),
                ],
                "packet28-search-cli": [
                    ("packet28-daemon-protocol", None),
                    ("packet28-reducer-core", None),
                    ("packet28-search-core", None),
                ],
                "packet28-reducer-core": [
                    ("suite-packet-core", None),
                ],
                "packet28-search-core": [
                    ("suite-packet-core", None),
                ],
            }
        )

        result = self.run_checker(fixture)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("architecture dependency invariant passed", result.stdout)

    def test_packet28_search_cli_must_not_depend_on_daemon_runtime(self) -> None:
        fixture = metadata(
            {
                "packet28-search-cli": [
                    ("packet28-daemon-protocol", None),
                    ("packet28-daemon-core", None),
                ],
                "packet28-daemon-core": [
                    ("packet28-daemon-protocol", None),
                ],
            }
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28-search-cli reaches forbidden normal dependency "
            "packet28-daemon-core",
            result.stderr,
        )

    def test_daemon_core_must_not_reach_search_implementation(self) -> None:
        fixture = metadata(
            {
                "packet28-daemon-core": [
                    ("packet28-daemon-protocol", None),
                    ("adapter-bridge", None),
                ],
                "adapter-bridge": [("packet28-search-core", None)],
            },
            additional_packages=("adapter-bridge",),
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28-daemon-core -> adapter-bridge -> packet28-search-core",
            result.stderr,
        )

    def test_kernel_mechanism_must_not_reach_concrete_builtins_transitively(
        self,
    ) -> None:
        fixture = metadata(
            {
                "context-kernel-mechanism": [("adapter-bridge", None)],
                "adapter-bridge": [("guardy-core", None)],
            },
            additional_packages=("adapter-bridge", "guardy-core"),
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-mechanism -> adapter-bridge -> guardy-core",
            result.stderr,
        )

    def test_kernel_mechanism_must_not_depend_on_concrete_builtins_directly(
        self,
    ) -> None:
        fixture = metadata(
            {"context-kernel-mechanism": [("guardy-core", None)]},
            additional_packages=("guardy-core",),
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-mechanism -> guardy-core",
            result.stderr,
        )

    def test_kernel_core_must_remain_a_single_edge_compatibility_facade(
        self,
    ) -> None:
        fixture = metadata(
            {
                "context-kernel-core": [
                    ("context-kernel-mechanism", None),
                ],
            }
        )

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-core compatibility facade must have exactly one "
            "normal dependency",
            result.stderr,
        )

    def test_kernel_builtins_must_depend_directly_on_mechanism(self) -> None:
        fixture = metadata({}, include_builtin_mechanism_edge=False)

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-builtins must depend directly on "
            "context-kernel-mechanism",
            result.stderr,
        )

    def test_kernel_mechanism_source_rejects_builtin_target_literals(
        self,
    ) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(
                root,
                'const TARGET: &str = "guardy.check";\n',
            )

            result = self.run_checker(fixture, source_root=root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-mechanism source contains concrete built-in "
            "literal 'guardy.check'",
            result.stderr,
        )

    def test_kernel_mechanism_source_rejects_concrete_crate_identifiers(
        self,
    ) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "use guardy_core::ContextConfig;\n")

            result = self.run_checker(fixture, source_root=root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-mechanism source contains concrete built-in "
            "literal 'guardy_core'",
            result.stderr,
        )

    def test_kernel_registry_must_own_each_target_exactly_once(self) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            registry = (
                root
                / "crates"
                / "context-kernel-builtins"
                / "src"
                / "kernel_registry.rs"
            )
            registry.write_text(
                registry.read_text(encoding="utf-8")
                + '\nconst DUPLICATE: &str = "guardy.check";\n',
                encoding="utf-8",
            )

            result = self.run_checker(fixture, source_root=root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-builtins registry must own exactly one "
            "registration literal for 'guardy.check'",
            result.stderr,
        )

    def test_packet28d_thin_entrypoint_and_explicit_broker_facade_pass(
        self,
    ) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(root)

            result = self.run_checker(fixture, source_root=root)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_packet28d_broker_child_rejects_wildcard_import(self) -> None:
        cases = (
            "use super::*;\n",
            "use super::{context as imported_context, *};\n",
            "use crate::broker::{\n    context,\n    *,\n};\n",
            "use std::prelude::rust_2021::*;\n",
            "pub(crate) use super::{context, *};\n",
        )
        for broker_child_source in cases:
            with self.subTest(source=broker_child_source):
                fixture = metadata({})
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    write_kernel_sources(root, "")
                    write_packet28d_sources(
                        root,
                        broker_child_source=broker_child_source,
                    )

                    result = self.run_checker(fixture, source_root=root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "packet28d broker modules must use explicit imports: "
                    "crates/packet28d/src/broker/context.rs",
                    result.stderr,
                )

    def test_packet28d_broker_facade_rejects_visible_child_module(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28d/src/broker/mod.rs",
            lambda source: source.replace(
                "mod context;",
                "pub(crate) mod context;",
                1,
            ),
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28d broker facade must keep implementation module "
            "'context' private",
            result.stderr,
        )

    def test_packet28d_consumer_rejects_broker_facade_bypass(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28d/src/application.rs",
            lambda source: (
                "use crate::broker::context::broker_get_context;\n" + source
            ),
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28d modules must consume broker ports through the owning "
            "facade: crates/packet28d/src/application.rs",
            result.stderr,
        )

    def test_packet28d_broker_child_rejects_repository_rescan_entrypoints(self) -> None:
        cases = (
            "fn rescan() { let _ = mapy_core::build_repo_map(Default::default()); }\n",
            "fn rescan() { let _ = mapy_core::build_repo_query(Default::default()); }\n",
            "fn rescan() { let _ = mapy_core::build_repo_map_from_index(todo!(), todo!()); }\n",
            "fn rescan() { let _ = mapy_core::build_repo_index(todo!(), true); }\n",
            "use mapy_core::build_repo_index_with_progress as rescan;\n",
            "fn rescan() { let _ = mapy_core::update_repo_index(todo!(), todo!(), todo!(), true); }\n",
            "fn rescan() { let _ = mapy_core::rebuild_repo_index_runtime(todo!(), true); }\n",
            "use mapy_core::rebuild_repo_index_runtime_with_progress as rescan;\n",
            "fn rescan() { let _ = mapy_core::update_repo_index_runtime(todo!(), todo!(), todo!(), true); }\n",
            "fn rescan() { let _ = mapy_core::load_repo_index_runtime(todo!()); }\n",
            "fn rescan() { let _ = mapy_core::clear_repo_index_runtime(todo!()); }\n",
            "use mapy_core::shared_scan::RepoIndexScanSession;\n",
            "use mapy_core::{shared_scan as scan, RepoIndexRuntime};\n",
            "use mapy_core::PreparedRepoIndexRuntime as Prepared;\n",
            "fn rescan() { let _ = packet28_reducer_core::search(todo!(), todo!()); }\n",
            "use mapy_core :: build_repo_map as rescan;\n",
            "use mapy_core::{build_repo_query as rescan};\n",
            "use mapy_core::{self as mapy, RepoIndexRuntime};\n",
            "use mapy_core as mapy;\n",
            "use ::mapy_core as mapy;\n",
            "use packet28_reducer_core :: search as rescan;\n",
            "use packet28_reducer_core::{SearchRequest, search as rescan};\n",
            "use packet28_reducer_core as reducer;\nfn rescan() { reducer::search(todo!(), todo!()); }\n",
            "use ::packet28_reducer_core as reducer;\n",
            "use packet28_reducer_core::{self as reducer, SearchRequest};\n",
            "use {packet28_reducer_core as reducer};\nfn rescan() { reducer::search(todo!(), todo!()); }\n",
            "use {mapy_core as mapy};\nfn rescan() { mapy::build_repo_map(Default::default()); }\n",
            "fn rescan() { let _ = packet28_search_core::indexed_search(todo!(), todo!(), todo!()); }\n",
            "use packet28_search_core :: guarded_indexed_search as query;\n",
            "use packet28_search_core::guarded_indexed_search;\nfn wrapper() { guarded_indexed_search(todo!(), todo!(), todo!()); }\n",
            "use packet28_search_core::guarded_fallback_reason as route;\n",
            "use packet28_search_core::guarded_indexed_search_batch as batch;\n",
            "use packet28_search_core::{RegexIndexRuntime, load_and_indexed_search as query};\n",
            "use packet28_search_core::load_and_guarded_indexed_search as query;\n",
            "use packet28_search_core::load_runtime as load;\n",
            "use packet28_search_core::rebuild_full_index as rebuild;\n",
            "use packet28_search_core::rebuild_full_index_with_progress as rebuild;\n",
            "use packet28_search_core::update_overlay_index as update;\n",
            "use packet28_search_core::clear_index as clear;\n",
            "use packet28_search_core::shared_scan::RegexIndexScanSession;\n",
            "use packet28_search_core::shared_scan::PreparedRegexIndexRuntime;\n",
            "use packet28_search_core::{shared_scan as scan, RegexIndexRuntime};\n",
            "use packet28_search_core::{self as search, RegexIndexRuntime};\n",
            "use packet28_search_core as search;\n",
            "use ::packet28_search_core as search;\n",
            "use {packet28_search_core as search};\nfn query() { search::indexed_search(todo!(), todo!(), todo!()); }\n",
            "const QUOTE: char = '\"';\nfn query() { packet28_search_core::indexed_search(todo!(), todo!(), todo!()); }\n",
            "const BYTE_QUOTE: u8 = b'\"';\nfn query() { packet28_search_core::indexed_search(todo!(), todo!(), todo!()); }\n",
        )
        for broker_child_source in cases:
            with self.subTest(source=broker_child_source):
                fixture = metadata({})
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    write_kernel_sources(root, "")
                    write_packet28d_sources(
                        root,
                        broker_child_source=broker_child_source,
                    )

                    result = self.run_checker(fixture, source_root=root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "packet28d broker repository consumers must use authenticated "
                    "daemon index runtimes",
                    result.stderr,
                )

    def test_packet28d_broker_nested_module_rejects_repository_ownership(self) -> None:
        cases = (
            "fn bypass() { let _ = packet28_search_core::indexed_search(todo!(), todo!(), todo!()); }\n",
            "fn bypass() { let _ = mapy_core::shared_scan::RepoIndexScanSession::begin(todo!(), true, todo!()); }\n",
        )
        for nested_source in cases:
            with self.subTest(source=nested_source):
                fixture = metadata({})
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    write_kernel_sources(root, "")
                    write_packet28d_sources(root)
                    broker = root / "crates" / "packet28d" / "src" / "broker"
                    (broker / "search.rs").write_text(
                        "mod raw;\n", encoding="utf-8"
                    )
                    nested = broker / "search"
                    nested.mkdir()
                    (nested / "raw.rs").write_text(
                        nested_source, encoding="utf-8"
                    )

                    result = self.run_checker(fixture, source_root=root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "packet28d broker repository consumers must use authenticated "
                    "daemon index runtimes",
                    result.stderr,
                )
                self.assertIn(
                    "crates/packet28d/src/broker/search/raw.rs",
                    result.stderr,
                )

    def test_packet28d_broker_rejects_repository_owner_wrapper_routes(self) -> None:
        cases = (
            "use crate::shared_repository_scan::rebuild_full_indexes_with_shared_scan as rescan;\n",
            "use crate::runtime_files::load_index_runtime_files as load;\n",
            "use crate::index::daemon_packet28_search as query;\n",
            "use crate::index::run_index_worker as own;\n",
            "use crate::index::{build_index_status, daemon_packet28_search};\n",
            "use crate::index as index_owner;\n",
            "use crate::state::raw_query;\n",
            "use crate::planning::{merged_unique, raw_map};\n",
            "use crate as daemon_owner;\n",
            "extern crate self as daemon_owner;\n",
            "use {crate::index as owner};\nfn query() { owner::daemon_packet28_search(todo!()); }\n",
            "use {crate::planning as owner};\nfn map() { owner::raw_map(todo!()); }\n",
            "use {crate::state as owner};\nfn query() { owner::raw_query(todo!()); }\n",
            "use {crate as daemon_owner};\n",
        )
        for broker_child_source in cases:
            with self.subTest(source=broker_child_source):
                fixture = metadata({})
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    write_kernel_sources(root, "")
                    write_packet28d_sources(
                        root,
                        broker_child_source=broker_child_source,
                    )

                    result = self.run_checker(fixture, source_root=root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("packet28d broker modules", result.stderr)

    def test_packet28d_broker_rejects_crate_root_reexport_aliases(self) -> None:
        cases = (
            (
                "pub(crate) use packet28_search_core::indexed_search as raw_query;\n",
                "use crate::raw_query;\n",
            ),
            (
                "pub(crate) use mapy_core::build_repo_map as raw_map;\n",
                "use crate::raw_map;\n",
            ),
        )
        for reexport, broker_child_source in cases:
            with self.subTest(reexport=reexport):
                fixture = metadata({})
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    write_kernel_sources(root, "")
                    write_packet28d_sources(
                        root,
                        broker_child_source=broker_child_source,
                    )
                    library = root / "crates" / "packet28d" / "src" / "lib.rs"
                    library.write_text(
                        library.read_text(encoding="utf-8") + reexport,
                        encoding="utf-8",
                    )

                    result = self.run_checker(fixture, source_root=root)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("reviewed ports", result.stderr)

    def test_packet28d_broker_repository_guard_ignores_lexical_decoys(self) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(
                root,
                broker_child_source=(
                    "// packet28_search_core::guarded_indexed_search_batch\n"
                    'const NOTE: &str = "mapy_core::build_repo_map";\n'
                    'const RAW: &str = r#"RegexIndexScanSession"#;\n'
                    "fn guarded_indexed_search_batch() {}\n"
                ),
            )

            result = self.run_checker(fixture, source_root=root)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_packet28d_broker_child_allows_authenticated_runtime_consumers(self) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(
                root,
                broker_child_source=(
                    "use mapy_core::{build_repo_map_from_runtime, RepoIndexRuntime};\n"
                    "use packet28_search_core::{"
                    "broker_internal_guarded_indexed_search_batch, "
                    "BrokerInternalGuardedIndexedSearchSession};\n"
                    "fn consume() {\n"
                    "  let _: RepoIndexRuntime = todo!();\n"
                    "  let _ = build_repo_map_from_runtime(todo!(), todo!());\n"
                    "  let mut session = BrokerInternalGuardedIndexedSearchSession::new();\n"
                    "  let _ = broker_internal_guarded_indexed_search_batch("
                    "todo!(), todo!(), todo!(), &mut session);\n"
                    "}\n"
                ),
            )

            result = self.run_checker(fixture, source_root=root)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_packet28d_entrypoint_rejects_runtime_ownership(self) -> None:
        fixture = metadata({})
        thick_main = "\n".join(
            (
                "use std::sync::Mutex;",
                "use packet28_daemon_protocol::message::DaemonRequest;",
                "fn main() {",
                "    let _state: Option<DaemonState> = None;",
                "    packet28d::serve(Default::default());",
                "}",
            )
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(root, main_source=thick_main)

            result = self.run_checker(fixture, source_root=root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28d executable entrypoint owns daemon internals 'Mutex'",
            result.stderr,
        )
        self.assertIn(
            "packet28d executable entrypoint owns daemon internals "
            "'packet28_daemon_protocol'",
            result.stderr,
        )

    def test_packet28d_runtime_doc_is_required_without_a_docs_directory(
        self,
    ) -> None:
        fixture = metadata({})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_kernel_sources(root, "")
            write_packet28d_sources(root)
            (root / "docs" / "daemon-runtime.md").unlink()
            (root / "docs").rmdir()

            result = self.run_checker(fixture, source_root=root)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "packet28d runtime architecture documentation is missing or unreadable",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_missing_public_marker(self) -> None:
        marker = (
            "<!-- packet28d-public owner=packet28-daemon-protocol item=broker "
            "classification=excluded evidence=wire-dto-json-compat-tests -->\n"
        )
        result = self.run_packet28d_mutation(
            "docs/daemon-runtime.md",
            lambda source: source.replace(marker, "", 1),
        )

        self.assertIn(
            "exactly one public marker for packet28-daemon-protocol::broker "
            "(found 0)",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_duplicate_public_marker(self) -> None:
        marker = (
            "<!-- packet28d-public owner=packet28d item=serve "
            "classification=excluded "
            "evidence=non-hermetic-process-lifecycle-owner -->"
        )
        result = self.run_packet28d_mutation(
            "docs/daemon-runtime.md",
            lambda source: source + f"\n{marker}\n",
        )

        self.assertIn(
            "exactly one public marker for packet28d::serve (found 2)",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_classification_mutation(self) -> None:
        original = (
            "owner=packet28d item=serve classification=excluded "
            "evidence=non-hermetic-process-lifecycle-owner"
        )
        mutated = original.replace("classification=excluded", "classification=covered")
        result = self.run_packet28d_mutation(
            "docs/daemon-runtime.md",
            lambda source: source.replace(original, mutated, 1),
        )

        self.assertIn(
            "classification/evidence mismatch for packet28d::serve",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_unclassified_public_source(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28-daemon-protocol/src/lib.rs",
            lambda source: source + "\npub mod surprise;\n",
        )

        self.assertIn(
            "does not classify public source item "
            "packet28-daemon-protocol::surprise",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_unclassified_client_module(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28-daemon-client/src/lib.rs",
            lambda source: source + "\npub mod surprise;\n",
        )

        self.assertIn(
            "does not classify public source item "
            "packet28-daemon-client::surprise",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_missing_public_source(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28-daemon-core/src/lib.rs",
            lambda source: source.replace("pub mod trust;", "", 1),
        )

        self.assertIn(
            "lists missing public source item packet28-daemon-core::trust",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_duplicate_public_source(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28-daemon-protocol/src/lib.rs",
            lambda source: source + "\npub mod broker;\n",
        )

        self.assertIn(
            "public source item packet28-daemon-protocol::broker is declared 2 times",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_duplicate_anchor_marker(self) -> None:
        marker = (
            "<!-- packet28d-anchor id=protocol-frame-runnable "
            "source=crates/packet28-daemon-protocol/src/frame.rs "
            "fence=runnable -->"
        )
        result = self.run_packet28d_mutation(
            "docs/daemon-runtime.md",
            lambda source: source + f"\n{marker}\n",
        )

        self.assertIn(
            "exactly one anchor marker for protocol-frame-runnable (found 2)",
            result.stderr,
        )

    def test_packet28d_runtime_doc_rejects_source_anchor_mutation(self) -> None:
        result = self.run_packet28d_mutation(
            "crates/packet28d/src/shared_repository_scan.rs",
            lambda source: source.replace("//! ```no_run", "//! ```ignore", 1),
        )

        self.assertIn(
            "doctest anchor packet28d-shared-scan-no_run is missing source evidence",
            result.stderr,
        )

    def test_tokio_is_restricted_to_process_orchestration_boundaries(self) -> None:
        fixture = metadata(
            {
                "context-kernel-core": [("tokio", None)],
                "packet28d": [("tokio", None)],
                "suite-cli": [("tokio", None)],
            },
            additional_packages=("packet28d", "suite-cli", "tokio"),
        )
        fixture["workspace_members"].remove("tokio")

        result = self.run_checker(fixture)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "context-kernel-core reaches async runtime outside an "
            "orchestration boundary: context-kernel-core -> tokio",
            result.stderr,
        )
        self.assertNotIn(
            "packet28d reaches async runtime outside", result.stderr
        )
        self.assertNotIn(
            "suite-cli reaches async runtime outside", result.stderr
        )


if __name__ == "__main__":
    unittest.main()
