#!/usr/bin/env python3
"""Reject forbidden normal-dependency paths in the Cargo workspace graph."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import Counter, deque
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent

# Keep the explicit list as a backstop for metadata fixtures and packages whose
# binary target is temporarily absent during a refactor. Binary workspace
# packages discovered from Cargo metadata are forbidden as well.
CLI_PACKAGES = frozenset(
    {
        "covy-cli",
        "diffy-cli",
        "packet28-search-cli",
        "suite-cli",
        "testy-cli",
    }
)

# Async orchestration belongs at process boundaries. Keeping Tokio out of the
# reusable core graph prevents synchronous repository scans, SQLite, and
# serialization from being mislabeled as non-blocking merely because a runtime
# happens to be available.
TOKIO_RUNTIME_OWNERS = frozenset({"packet28d", "suite-cli"})

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

KERNEL_CONCRETE_PACKAGES = frozenset(
    {
        "buildy-core",
        "context-kernel-builtins",
        "context-kernel-core",
        "contextq-core",
        "covy-ingest",
        "diffy-core",
        "guardy-core",
        "mapy-core",
        "stacky-core",
        "suite-foundation-core",
        "suite-policy-core",
        "suite-proxy-core",
        "testy-cli-common",
        "testy-core",
    }
)

KERNEL_CONCRETE_SOURCE_NAMES = tuple(
    package.replace("-", "_") for package in sorted(KERNEL_CONCRETE_PACKAGES)
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

PACKET28D_BROKER_ROOT_MODULES = frozenset({"index", "planning", "state"})
PACKET28D_BROKER_ROOT_PORTS = frozenset(
    {
        "context_version_storage_id",
        "daemon_log",
        "fence_task_namespace_admission",
        "lock_err",
        "mark_task_dirty",
        "persist_task",
        "resolve_root",
        "task_storage_id",
    }
)
PACKET28D_BROKER_INDEX_PORTS = frozenset(
    {
        "build_index_status",
        "enqueue_full_index_rebuild",
        "enqueue_incremental_index_paths",
    }
)
PACKET28D_BROKER_PLANNING_PORTS = frozenset(
    {
        "coverage_gap_for_path",
        "current_deleted_paths",
        "estimate_plan_step_tokens",
        "find_candidate_test_paths",
        "is_edit_like_action",
        "is_read_like_action",
        "merged_unique",
        "merged_unique_many",
        "normalize_plan_steps",
        "test_step_covers_path",
        "test_step_targets_mapped_tests",
        "testmap_tests_for_path",
    }
)
PACKET28D_BROKER_STATE_PORTS = frozenset(
    {"CachedSourceFile", "DaemonState", "TaskGenerationId"}
)
PACKET28D_BROKER_MODULE_PORTS = {
    "index": PACKET28D_BROKER_INDEX_PORTS,
    "planning": PACKET28D_BROKER_PLANNING_PORTS,
    "state": PACKET28D_BROKER_STATE_PORTS,
}

PACKET28D_MAIN_MAX_LINES = 80

PACKET28D_PUBLIC_DOC_INVENTORY = (
    ("packet28-daemon-protocol", "broker", "excluded", "wire-dto-json-compat-tests"),
    ("packet28-daemon-protocol", "commands", "excluded", "command-json-dispatch-tests"),
    ("packet28-daemon-protocol", "context_store", "excluded", "context-store-process-tests"),
    ("packet28-daemon-protocol", "frame", "covered", "protocol-frame-runnable"),
    ("packet28-daemon-protocol", "hooks", "excluded", "hook-ingest-json-tests"),
    ("packet28-daemon-protocol", "index", "excluded", "index-state-process-tests"),
    ("packet28-daemon-protocol", "logging", "excluded", "runtime-log-rotation-process-tests"),
    ("packet28-daemon-protocol", "message", "excluded", "request-response-json-tests"),
    ("packet28-daemon-protocol", "paths", "excluded", "path-endpoint-tests"),
    ("packet28-daemon-protocol", "process", "excluded", "session-detach-process-tests"),
    (
        "packet28-daemon-protocol", "registry", "covered",
        "protocol-registry-migration-runnable+json-compat-tests",
    ),
    ("packet28-daemon-protocol", "task", "covered", "protocol-task-lifecycle-runnable+compile_fail"),
    (
        "packet28-daemon-protocol", "root_compatibility", "excluded",
        "exact-two-name-root-allowlist",
    ),
    (
        "packet28-daemon-client", "runtime_discovery", "covered",
        "daemon-client-discovery-runnable",
    ),
    (
        "packet28-daemon-client", "transport", "excluded",
        "authenticated-transport-process-tests",
    ),
    ("packet28-daemon-core", "integrity", "excluded", "integrity-corruption-tests"),
    ("packet28-daemon-core", "retention", "excluded", "retention-recovery-process-tests"),
    ("packet28-daemon-core", "storage", "covered", "daemon-core-storage-runnable"),
    ("packet28-daemon-core", "task_store_lease", "excluded", "lease-authority-process-tests"),
    ("packet28-daemon-core", "trust", "excluded", "trust-platform-tests"),
    (
        "packet28-daemon-core", "root_compatibility", "excluded",
        "exact-182-name-frozen-v0-inventory",
    ),
    ("packet28d", "serve", "excluded", "non-hermetic-process-lifecycle-owner"),
    (
        "packet28d", "serve_with_managed_log", "excluded",
        "non-hermetic-process-lifecycle-owner",
    ),
    ("packet28d", "start", "excluded", "p28-bootstrap-authority-process-tests"),
    ("packet28d", "shared_repository_scan", "covered", "packet28d-shared-scan-no_run+feature-shared-repository-scan"),
)

PACKET28D_DOCTEST_ANCHORS = (
    (
        "protocol-frame-runnable", "crates/packet28-daemon-protocol/src/frame.rs",
        "runnable", "//! ```",
        ("write_frame(", "read_frame(", "DaemonRequest::Status", "DaemonResponse::Ack"),
    ),
    (
        "protocol-registry-migration-runnable",
        "crates/packet28-daemon-protocol/src/registry.rs",
        "runnable",
        "//! ```",
        (
            "DaemonRegistryRequestV1::TaskListPage",
            "TaskListPageRequestV1::default()",
            "serde_json::from_value::<DaemonRequest>",
        ),
    ),
    (
        "protocol-task-lifecycle-runnable", "crates/packet28-daemon-protocol/src/task.rs",
        "runnable", "/// ```",
        ("TaskLifecycle::Idle", "lifecycle.start()?", "lifecycle.finish_run()?"),
    ),
    (
        "protocol-task-lifecycle-compile_fail", "crates/packet28-daemon-protocol/src/task.rs",
        "compile_fail", "/// ```compile_fail",
        ("TaskLifecycle {", "running: true", "cancel_requested: true"),
    ),
    (
        "daemon-client-discovery-runnable",
        "crates/packet28-daemon-client/src/lib.rs",
        "runnable",
        "//! ```",
        ("read_runtime_info_if_present(", "runtime.is_none()"),
    ),
    (
        "daemon-core-storage-runnable", "crates/packet28-daemon-core/src/lib.rs",
        "runnable", "//! ```",
        (
            "save_task_watch_registry_checkpoint(",
            "load_task_registry(",
            "TaskRegistry::default()",
            "WatchRegistry::default()",
        ),
    ),
    (
        "daemon-core-error-source-chain-runnable", "crates/packet28-daemon-core/src/error.rs",
        "runnable", "/// ```",
        ("DaemonCoreError::Frame", "error.source().is_some()"),
    ),
    (
        "daemon-core-root-compatibility-compile_fail", "crates/packet28-daemon-core/src/lib.rs",
        "compile_fail", "//! ```compile_fail",
        ("use packet28_daemon_core::write_frame;",),
    ),
    (
        "packet28d-shared-scan-no_run", "crates/packet28d/src/shared_repository_scan.rs",
        "no_run", "//! ```no_run",
        ("rebuild_full_indexes_with_shared_scan(", "result.telemetry.walk_passes"),
    ),
)

PACKET28D_PUBLIC_DOC_MARKER = re.compile(
    r"^<!-- packet28d-public owner=([a-z0-9-]+) item=([a-z0-9_]+) "
    r"classification=(covered|excluded) evidence=([A-Za-z0-9_+.-]+) -->$",
    re.MULTILINE,
)
PACKET28D_ANCHOR_DOC_MARKER = re.compile(
    r"^<!-- packet28d-anchor id=([A-Za-z0-9_+-]+) "
    r"source=([A-Za-z0-9_./-]+) fence=(runnable|compile_fail|no_run) -->$",
    re.MULTILINE,
)


@dataclass(frozen=True)
class ArchitectureRule:
    source: str
    forbidden: frozenset[str]


BASE_RULES = (
    ArchitectureRule(
        source="context-instruct-shim",
        forbidden=frozenset(
            {
                "packet28-daemon-core",
                "context-kernel-core",
                "context-memory-core",
                "packet28-reducer-core",
                "packet28-search-core",
            }
        ),
    ),
    ArchitectureRule(
        source="packet28-daemon-core",
        forbidden=frozenset(
            {
                "context-kernel-core",
                "context-memory-core",
                "packet28-reducer-core",
                "packet28-search-core",
            }
        ),
    ),
    ArchitectureRule(
        source="packet28-search-cli",
        forbidden=frozenset(
            {
                "packet28-daemon-core",
                "context-kernel-core",
                "context-memory-core",
            }
        ),
    ),
    ArchitectureRule(
        source="suite-packet-core",
        forbidden=frozenset({"packet28-daemon-protocol"}),
    ),
    ArchitectureRule(
        source="context-kernel-mechanism",
        forbidden=KERNEL_CONCRETE_PACKAGES,
    ),
    ArchitectureRule(
        source="context-kernel-builtins",
        forbidden=frozenset({"context-kernel-core"}),
    ),
)


class MetadataError(ValueError):
    """Cargo metadata did not contain the graph required by this check."""


class CargoMetadataGraph:
    """The resolved normal-dependency graph from Cargo metadata."""

    def __init__(self, metadata: dict[str, Any]) -> None:
        packages = metadata.get("packages")
        resolve = metadata.get("resolve")
        workspace_members = metadata.get("workspace_members")
        if not isinstance(packages, list):
            raise MetadataError("metadata is missing the packages array")
        if not isinstance(resolve, dict) or not isinstance(
            resolve.get("nodes"), list
        ):
            raise MetadataError("metadata is missing the resolved dependency graph")
        if not isinstance(workspace_members, list):
            raise MetadataError("metadata is missing the workspace_members array")

        self.names: dict[str, str] = {}
        self.packages: dict[str, dict[str, Any]] = {}
        for package in packages:
            if not isinstance(package, dict):
                raise MetadataError("metadata contains a non-object package")
            package_id = package.get("id")
            name = package.get("name")
            if not isinstance(package_id, str) or not isinstance(name, str):
                raise MetadataError("metadata contains a package without an id or name")
            if package_id in self.packages:
                raise MetadataError(
                    f"metadata contains duplicate package id {package_id}"
                )
            self.packages[package_id] = package
            self.names[package_id] = name

        self.workspace_members = set(workspace_members)
        unknown_members = self.workspace_members.difference(self.packages)
        if unknown_members:
            unknown = ", ".join(sorted(unknown_members))
            raise MetadataError(
                f"workspace members are absent from packages: {unknown}"
            )

        self.edges: dict[str, set[str]] = {
            package_id: set() for package_id in self.packages
        }
        for node in resolve["nodes"]:
            if not isinstance(node, dict):
                raise MetadataError("metadata contains a non-object resolve node")
            package_id = node.get("id")
            deps = node.get("deps")
            if not isinstance(package_id, str) or package_id not in self.packages:
                raise MetadataError("metadata contains an unknown resolve node")
            if not isinstance(deps, list):
                raise MetadataError(
                    f"resolve node {package_id} is missing dependency kinds"
                )
            for dependency in deps:
                if not isinstance(dependency, dict):
                    raise MetadataError(
                        f"resolve node {package_id} contains a malformed dependency"
                    )
                dependency_id = dependency.get("pkg")
                dependency_kinds = dependency.get("dep_kinds")
                if not isinstance(dependency_id, str):
                    raise MetadataError(
                        f"resolve node {package_id} contains a dependency without an id"
                    )
                if dependency_id not in self.packages:
                    raise MetadataError(
                        f"resolve node {package_id} references unknown {dependency_id}"
                    )
                if not isinstance(dependency_kinds, list):
                    raise MetadataError(
                        f"dependency {package_id} -> {dependency_id} lacks dep_kinds"
                    )
                if self._has_normal_kind(dependency_kinds):
                    self.edges[package_id].add(dependency_id)

        self.workspace_package_ids_by_name: dict[str, list[str]] = {}
        for package_id in self.workspace_members:
            self.workspace_package_ids_by_name.setdefault(
                self.names[package_id], []
            ).append(package_id)

    @staticmethod
    def _has_normal_kind(dependency_kinds: list[Any]) -> bool:
        # Cargo represents a normal dependency as kind=null. Accept "normal"
        # too so deterministic fixtures remain readable and future-compatible.
        return any(
            isinstance(kind, dict) and kind.get("kind") in (None, "normal")
            for kind in dependency_kinds
        )

    def workspace_package_id(self, name: str) -> str:
        matches = self.workspace_package_ids_by_name.get(name, [])
        if not matches:
            raise MetadataError(f"required workspace package is missing: {name}")
        if len(matches) > 1:
            raise MetadataError(f"workspace package name is ambiguous: {name}")
        return matches[0]

    def workspace_cli_packages(self) -> set[str]:
        result = set(CLI_PACKAGES)
        for package_id in self.workspace_members:
            package = self.packages[package_id]
            targets = package.get("targets", [])
            if not isinstance(targets, list):
                raise MetadataError(
                    f"package {self.names[package_id]} has malformed targets"
                )
            if any(
                isinstance(target, dict)
                and isinstance(target.get("kind"), list)
                and "bin" in target["kind"]
                for target in targets
            ):
                result.add(self.names[package_id])
        return result

    def direct_dependency_names(self, source_name: str) -> set[str]:
        source = self.workspace_package_id(source_name)
        return {self.names[dependency] for dependency in self.edges[source]}

    def shortest_forbidden_paths(
        self, source_name: str, forbidden_names: set[str] | frozenset[str]
    ) -> dict[str, list[str]]:
        source = self.workspace_package_id(source_name)
        predecessors: dict[str, str | None] = {source: None}
        queue = deque([source])

        while queue:
            current = queue.popleft()
            dependencies = sorted(
                self.edges.get(current, ()),
                key=lambda package_id: (self.names[package_id], package_id),
            )
            for dependency in dependencies:
                if dependency in predecessors:
                    continue
                predecessors[dependency] = current
                queue.append(dependency)

        found: dict[str, list[str]] = {}
        for package_id in predecessors:
            name = self.names[package_id]
            if package_id == source or name not in forbidden_names:
                continue
            path_ids: list[str] = []
            cursor: str | None = package_id
            while cursor is not None:
                path_ids.append(cursor)
                cursor = predecessors[cursor]
            path = [self.names[item] for item in reversed(path_ids)]
            previous = found.get(name)
            if previous is None or len(path) < len(previous):
                found[name] = path
        return found


def architecture_rules(graph: CargoMetadataGraph) -> tuple[ArchitectureRule, ...]:
    protocol_forbidden = {
        "packet28-daemon-core",
        "packet28d",
        "context-kernel-core",
        "context-memory-core",
        "packet28-reducer-core",
        "packet28-search-core",
        *graph.workspace_cli_packages(),
    }
    client_forbidden = {
        "packet28-daemon-core",
        "packet28d",
        "context-kernel-core",
        "context-memory-core",
        "packet28-reducer-core",
        "packet28-search-core",
        *graph.workspace_cli_packages(),
    }
    return (
        ArchitectureRule(
            source="packet28-daemon-protocol",
            forbidden=frozenset(protocol_forbidden),
        ),
        ArchitectureRule(
            source="packet28-daemon-client",
            forbidden=frozenset(client_forbidden),
        ),
        *BASE_RULES,
    )


def check_kernel_source_boundaries(root: Path) -> list[str]:
    errors: list[str] = []
    mechanism_src = root / "crates" / "context-kernel-mechanism" / "src"
    registry = (
        root
        / "crates"
        / "context-kernel-builtins"
        / "src"
        / "kernel_registry.rs"
    )
    if not mechanism_src.is_dir():
        return ["context-kernel-mechanism source directory is missing"]
    if not registry.is_file():
        return ["context-kernel-builtins registry source is missing"]

    forbidden_literals = (*KERNEL_BUILTIN_TARGETS, *KERNEL_CONCRETE_SOURCE_NAMES)
    for path in sorted(mechanism_src.rglob("*.rs")):
        try:
            source = path.read_text(encoding="utf-8")
        except OSError as error:
            errors.append(f"cannot read {path.relative_to(root)}: {error}")
            continue
        for literal in forbidden_literals:
            if literal in source:
                errors.append(
                    "context-kernel-mechanism source contains concrete built-in "
                    f"literal {literal!r}: {path.relative_to(root)}"
                )

    try:
        registry_source = registry.read_text(encoding="utf-8")
    except OSError as error:
        errors.append(f"cannot read {registry.relative_to(root)}: {error}")
        return errors
    for target in KERNEL_BUILTIN_TARGETS:
        if registry_source.count(f'"{target}"') != 1:
            errors.append(
                "context-kernel-builtins registry must own exactly one "
                f"registration literal for {target!r}"
            )
    return errors


def check_packet28d_runtime_documentation(root: Path) -> list[str]:
    """Match the daemon's public source surface to exact documentation markers."""
    runtime_doc = root / "docs" / "daemon-runtime.md"
    try:
        doc_source = runtime_doc.read_text(encoding="utf-8")
    except OSError as error:
        return [
            "packet28d runtime architecture documentation is missing or "
            f"unreadable at {runtime_doc.relative_to(root)}: {error}"
        ]

    errors: list[str] = []
    public_markers = PACKET28D_PUBLIC_DOC_MARKER.findall(doc_source)
    if doc_source.count("<!-- packet28d-public ") != len(public_markers):
        errors.append("packet28d runtime documentation has a malformed public marker")
    expected_public = {
        (owner, item): (classification, evidence)
        for owner, item, classification, evidence in PACKET28D_PUBLIC_DOC_INVENTORY
    }
    public_counts = Counter((owner, item) for owner, item, _, _ in public_markers)
    documented_public = {
        (owner, item): (classification, evidence)
        for owner, item, classification, evidence in public_markers
    }
    for owner, item in sorted(expected_public):
        count = public_counts[(owner, item)]
        if count != 1:
            errors.append(
                "packet28d runtime documentation must contain exactly one "
                f"public marker for {owner}::{item} (found {count})"
            )
        elif documented_public[(owner, item)] != expected_public[(owner, item)]:
            errors.append(
                "packet28d runtime documentation classification/evidence "
                f"mismatch for {owner}::{item}"
            )
    for owner, item in sorted(documented_public.keys() - expected_public.keys()):
        errors.append(
            "packet28d runtime documentation contains unclassified public "
            f"marker {owner}::{item}"
        )

    public_source_paths = {
        "packet28-daemon-protocol": "crates/packet28-daemon-protocol/src/lib.rs",
        "packet28-daemon-client": "crates/packet28-daemon-client/src/lib.rs",
        "packet28-daemon-core": "crates/packet28-daemon-core/src/lib.rs",
        "packet28d": "crates/packet28d/src/lib.rs",
    }
    expected_source_items = {
        owner: {
            item
            for inventory_owner, item, _, _ in PACKET28D_PUBLIC_DOC_INVENTORY
            if inventory_owner == owner and item != "root_compatibility"
        }
        for owner in public_source_paths
    }
    for owner, relative_path in public_source_paths.items():
        source_path = root / relative_path
        try:
            source = source_path.read_text(encoding="utf-8")
        except OSError as error:
            errors.append(
                f"cannot read {source_path.relative_to(root)} for public "
                f"inventory: {error}"
            )
            continue
        items = re.findall(
            r"^pub mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
            source,
            flags=re.MULTILINE,
        )
        if owner == "packet28d":
            exports = re.findall(
                r"^pub use\s+([A-Za-z_][A-Za-z0-9_:]*)"
                r"(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?\s*;",
                source,
                flags=re.MULTILINE,
            )
            if len(exports) != len(re.findall(r"^pub use\s+", source, re.MULTILINE)):
                errors.append("packet28d has an unclassified public re-export form")
            items.extend(alias or target.rsplit("::", 1)[-1] for target, alias in exports)
            if not re.search(
                r'#\[cfg\(feature = "shared-repository-scan"\)\]\s*'
                r"pub mod shared_repository_scan\s*;",
                source,
            ):
                errors.append(
                    "packet28d shared_repository_scan must remain feature-gated "
                    "by 'shared-repository-scan'"
                )
        item_counts = Counter(items)
        for item, count in sorted(item_counts.items()):
            if count != 1:
                errors.append(
                    f"public source item {owner}::{item} is declared {count} times"
                )
        actual_items = set(items)
        for item in sorted(actual_items - expected_source_items[owner]):
            errors.append(
                "packet28d runtime documentation does not classify public "
                f"source item {owner}::{item}"
            )
        for item in sorted(expected_source_items[owner] - actual_items):
            errors.append(
                "packet28d runtime documentation lists missing public source "
                f"item {owner}::{item}"
            )

    anchor_markers = PACKET28D_ANCHOR_DOC_MARKER.findall(doc_source)
    if doc_source.count("<!-- packet28d-anchor ") != len(anchor_markers):
        errors.append("packet28d runtime documentation has a malformed anchor marker")
    expected_anchors = {
        anchor: (path, fence)
        for anchor, path, fence, _, _ in PACKET28D_DOCTEST_ANCHORS
    }
    anchor_counts = Counter(anchor for anchor, _, _ in anchor_markers)
    documented_anchors = {
        anchor: (path, fence) for anchor, path, fence in anchor_markers
    }
    for anchor in sorted(expected_anchors):
        count = anchor_counts[anchor]
        if count != 1:
            errors.append(
                "packet28d runtime documentation must contain exactly one "
                f"anchor marker for {anchor} (found {count})"
            )
        elif documented_anchors[anchor] != expected_anchors[anchor]:
            errors.append(
                "packet28d runtime documentation source/fence mismatch for "
                f"anchor {anchor}"
            )
    for anchor in sorted(documented_anchors.keys() - expected_anchors.keys()):
        errors.append(
            f"packet28d runtime documentation contains unknown anchor {anchor}"
        )

    source_cache: dict[str, str] = {}
    for anchor, relative_path, _, fence_tag, tokens in PACKET28D_DOCTEST_ANCHORS:
        if relative_path not in source_cache:
            try:
                source_cache[relative_path] = (root / relative_path).read_text(
                    encoding="utf-8"
                )
            except OSError as error:
                errors.append(
                    f"cannot read {relative_path} for doctest anchor {anchor}: "
                    f"{error}"
                )
                source_cache[relative_path] = ""
        missing = [
            token
            for token in (fence_tag, *tokens)
            if token not in source_cache[relative_path]
        ]
        if missing:
            errors.append(
                f"packet28d doctest anchor {anchor} is missing source evidence "
                f"{missing!r}"
            )
    return errors


def mask_rust_comments_and_literals(source: str) -> str:
    """Preserve Rust code tokens while masking comments and string literals."""
    masked = list(source)
    index = 0
    length = len(source)

    def hide(start: int, end: int) -> None:
        for offset in range(start, end):
            if masked[offset] != "\n":
                masked[offset] = " "

    while index < length:
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = length if end == -1 else end
            hide(index, end)
            index = end
            continue
        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < length and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            hide(index, end)
            index = end
            continue
        character = re.match(
            r"(?:b)?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|[^\r\n])|[^'\\\r\n])'",
            source[index:],
        )
        if character is not None:
            end = index + character.end()
            hide(index, end)
            index = end
            continue
        raw = re.match(r"(?:br|cr|r)(?P<hashes>#{0,255})\"", source[index:])
        if raw is not None:
            terminator = '"' + raw.group("hashes")
            content_start = index + raw.end()
            content_end = source.find(terminator, content_start)
            end = length if content_end == -1 else content_end + len(terminator)
            hide(index, end)
            index = end
            continue
        prefix_bytes = 2 if source.startswith(('b"', 'c"'), index) else 1
        if source[index] == '"' or prefix_bytes == 2:
            end = index + prefix_bytes
            escaped = False
            while end < length:
                char = source[end]
                end += 1
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    break
            hide(index, end)
            index = end
            continue
        index += 1
    return "".join(masked)


def check_packet28d_source_boundaries(root: Path) -> list[str]:
    """Keep daemon composition in the library and broker coupling explicit."""
    source_root = root / "crates" / "packet28d" / "src"
    if not source_root.exists():
        # Source-only metadata fixtures may intentionally omit packet28d.
        return []

    errors = check_packet28d_runtime_documentation(root)
    main_path = source_root / "main.rs"
    application_path = source_root / "application.rs"
    library_path = source_root / "lib.rs"
    broker_root = source_root / "broker"
    broker_facade_path = broker_root / "mod.rs"

    required_files = (main_path, application_path, library_path, broker_facade_path)
    missing_required = False
    for path in required_files:
        if not path.is_file():
            missing_required = True
            errors.append(
                f"packet28d architecture source is missing: {path.relative_to(root)}"
            )
    if missing_required:
        return errors

    try:
        main_source = main_path.read_text(encoding="utf-8")
        library_source = library_path.read_text(encoding="utf-8")
        broker_facade = broker_facade_path.read_text(encoding="utf-8")
    except OSError as error:
        return [f"cannot read packet28d architecture source: {error}"]

    main_lines = len(main_source.splitlines())
    if main_lines > PACKET28D_MAIN_MAX_LINES:
        errors.append(
            "packet28d executable entrypoint exceeds its "
            f"{PACKET28D_MAIN_MAX_LINES}-line composition budget: {main_lines} lines"
        )
    if "packet28d::serve" not in main_source:
        errors.append(
            "packet28d executable entrypoint must delegate to packet28d::serve"
        )
    forbidden_entrypoint_literals = (
        "packet28_daemon_core",
        "packet28_daemon_protocol",
        "context_kernel_core",
        "tokio::",
        "DaemonState",
        "TcpListener",
        "UnixListener",
        "Mutex",
        "broker::",
        "persistence::",
    )
    for literal in forbidden_entrypoint_literals:
        if literal in main_source:
            errors.append(
                "packet28d executable entrypoint owns daemon internals "
                f"{literal!r}; move lifecycle composition into application.rs"
            )

    required_library_seams = (
        "mod application;",
        "mod broker;",
        "pub use application::serve;",
    )
    for seam in required_library_seams:
        if seam not in library_source:
            errors.append(
                f"packet28d library must own the application seam {seam!r}"
            )

    for module in PACKET28D_BROKER_MODULES:
        child = broker_root / f"{module}.rs"
        if not child.is_file():
            errors.append(
                f"packet28d broker module is missing: {child.relative_to(root)}"
            )
        module_declaration = re.compile(
            rf"^\s*(?P<visibility>pub(?:\s*\([^)]*\))?\s+)?"
            rf"mod\s+{re.escape(module)}\s*;\s*$",
            re.MULTILINE,
        )
        declarations = list(module_declaration.finditer(broker_facade))
        if len(declarations) != 1:
            errors.append(
                "packet28d broker facade must declare exactly one private "
                f"module {module!r}"
            )
        elif declarations[0].group("visibility") is not None:
            errors.append(
                "packet28d broker facade must keep implementation module "
                f"{module!r} private"
            )

    legacy_modules = sorted(source_root.glob("broker_*.rs"))
    for path in legacy_modules:
        errors.append(
            "packet28d broker implementation must live under src/broker: "
            f"{path.relative_to(root)}"
        )

    # Rust permits globs at any depth of a grouped use tree, for example
    # `use super::{context as imported_context, *};`. Broker children must
    # name every dependency explicitly, regardless of the use-tree root.
    wildcard_import = re.compile(
        r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\b[^;]*\*[^;]*;",
        re.MULTILINE,
    )
    for path in sorted(broker_root.rglob("*.rs")):
        try:
            source = mask_rust_comments_and_literals(
                path.read_text(encoding="utf-8")
            )
        except OSError as error:
            errors.append(f"cannot read {path.relative_to(root)}: {error}")
            continue
        if wildcard_import.search(source):
            errors.append(
                "packet28d broker modules must use explicit imports: "
                f"{path.relative_to(root)}"
            )
        if path != broker_facade_path and "crate::application" in source:
            errors.append(
                "packet28d broker implementation must not depend on the "
                f"application lifecycle: {path.relative_to(root)}"
            )
        allowed_root_ports = (
            PACKET28D_BROKER_ROOT_MODULES | PACKET28D_BROKER_ROOT_PORTS
        )
        direct_root_route = re.compile(
            r"\bcrate\s*::\s*(?P<port>[A-Za-z_]\w*)"
        )
        for route in direct_root_route.finditer(source):
            if route.group("port") not in allowed_root_ports:
                errors.append(
                    "packet28d broker modules may consume the crate root only "
                    "through reviewed ports, not "
                    f"{route.group('port')!r}: {path.relative_to(root)}"
                )
        grouped_root_route = re.compile(
            r"\buse\s+crate\s*::\s*\{(?P<ports>[^}]*)\}\s*;",
            re.DOTALL,
        )
        for route in grouped_root_route.finditer(source):
            ports = [
                port.strip()
                for port in route.group("ports").split(",")
                if port.strip()
            ]
            forbidden = [
                port for port in ports if port not in PACKET28D_BROKER_ROOT_PORTS
            ]
            if forbidden:
                errors.append(
                    "packet28d broker modules may consume grouped crate-root "
                    f"ports only from the reviewed allowlist, not {forbidden!r}: "
                    f"{path.relative_to(root)}"
                )
        if re.search(
            r"\b(?:use\s+crate|extern\s+crate\s+self)\s+as\b",
            source,
        ) or re.search(
            r"\buse\s*\{[^{}]*\bcrate\s+as\s+\w+[^{}]*\}\s*;",
            source,
            re.DOTALL,
        ):
            errors.append(
                "packet28d broker modules must not alias the packet28d crate "
                f"owner: {path.relative_to(root)}"
            )
        for module, allowed_ports in PACKET28D_BROKER_MODULE_PORTS.items():
            direct_module_route = re.compile(
                rf"\bcrate\s*::\s*{re.escape(module)}\s*::\s*"
                r"(?P<port>[A-Za-z_]\w*)"
            )
            for route in direct_module_route.finditer(source):
                if route.group("port") not in allowed_ports:
                    errors.append(
                        f"packet28d broker modules may consume crate::{module} "
                        "only through reviewed ports, not "
                        f"{route.group('port')!r}: {path.relative_to(root)}"
                    )
            grouped_module_route = re.compile(
                rf"\buse\s+crate\s*::\s*{re.escape(module)}\s*::\s*"
                r"\{(?P<ports>[^}]*)\}\s*;",
                re.DOTALL,
            )
            for route in grouped_module_route.finditer(source):
                ports = [
                    port.strip()
                    for port in route.group("ports").split(",")
                    if port.strip()
                ]
                forbidden = [port for port in ports if port not in allowed_ports]
                if forbidden:
                    errors.append(
                        "packet28d broker modules may consume grouped "
                        f"crate::{module} ports only from the reviewed allowlist, "
                        f"not {forbidden!r}: {path.relative_to(root)}"
                    )
            if re.search(
                rf"\buse\s+crate\s*::\s*{re.escape(module)}"
                r"\s*(?:;|\s+as\b)",
                source,
            ) or re.search(
                rf"\buse\s*\{{[^{{}}]*\bcrate\s*::\s*{re.escape(module)}\b"
                r"\s*(?:as\s+\w+\s*)?(?=,|\})[^{}]*\}\s*;",
                source,
                re.DOTALL,
            ):
                errors.append(
                    f"packet28d broker modules must not import the crate::{module} "
                    f"owner module: {path.relative_to(root)}"
                )
        forbidden_mapy_entrypoints = (
            "build_repo_map",
            "build_repo_query",
            "build_repo_map_from_index",
            "build_repo_index",
            "build_repo_index_with_progress",
            "update_repo_index",
            "rebuild_repo_index_runtime",
            "rebuild_repo_index_runtime_with_progress",
            "update_repo_index_runtime",
            "load_repo_index_runtime",
            "clear_repo_index_runtime",
            "RepoIndexScanSession",
            "PreparedRepoIndexRuntime",
        )
        forbidden_search_entrypoints = (
            "guarded_fallback_reason",
            "indexed_search",
            "guarded_indexed_search",
            "guarded_indexed_search_batch",
            "load_and_indexed_search",
            "load_and_guarded_indexed_search",
            "load_runtime",
            "rebuild_full_index",
            "rebuild_full_index_with_progress",
            "update_overlay_index",
            "clear_index",
            "RegexIndexScanSession",
            "PreparedRegexIndexRuntime",
        )
        forbidden_repository_rescans = (
            (
                re.compile(
                    r"\bmapy_core\s*::\s*(?:"
                    rf"(?:{'|'.join(map(re.escape, forbidden_mapy_entrypoints))})\b"
                    r"|shared_scan\b|\{[^}]*\b(?:"
                    rf"{'|'.join(map(re.escape, forbidden_mapy_entrypoints))}"
                    r"|shared_scan)\b)"
                    r"|\b(?:use|extern\s+crate)\s+(?:::)?mapy_core\s+as\b"
                    r"|\buse\s+(?:::)?mapy_core\s*::\s*\{[^}]*\bself\b"
                    r"(?:\s+as\s+\w+)?"
                    r"|\buse\s*\{[^{}]*(?<![\w:])(?:::)?mapy_core\s+as\b"
                    r"[^{}]*\}\s*;",
                    re.DOTALL,
                ),
                "mapy_core repository scan or runtime ownership",
            ),
            (
                re.compile(
                    r"\bpacket28_reducer_core\s*::\s*"
                    r"(?:search\b|\{[^}]*\bsearch\b)"
                    r"|\b(?:use|extern\s+crate)\s+(?:::)?packet28_reducer_core\s+as\b"
                    r"|\buse\s+(?:::)?packet28_reducer_core\s*::\s*\{[^}]*\bself\b"
                    r"(?:\s+as\s+\w+)?"
                    r"|\buse\s*\{[^{}]*(?<![\w:])(?:::)?packet28_reducer_core"
                    r"\s+as\b[^{}]*\}\s*;",
                    re.DOTALL,
                ),
                "packet28_reducer_core repository scan",
            ),
            (
                re.compile(
                    r"\bpacket28_search_core\s*::\s*(?:"
                    rf"(?:{'|'.join(map(re.escape, forbidden_search_entrypoints))})\b"
                    r"|shared_scan\b|\{[^}]*\b(?:"
                    rf"{'|'.join(map(re.escape, forbidden_search_entrypoints))}"
                    r"|shared_scan)\b)"
                    r"|\b(?:use|extern\s+crate)\s+(?:::)?packet28_search_core\s+as\b"
                    r"|\buse\s+(?:::)?packet28_search_core\s*::\s*\{[^}]*\bself\b"
                    r"(?:\s+as\s+\w+)?"
                    r"|\buse\s*\{[^{}]*(?<![\w:])(?:::)?packet28_search_core"
                    r"\s+as\b[^{}]*\}\s*;",
                    re.DOTALL,
                ),
                "packet28_search_core query or runtime ownership bypass",
            ),
        )
        for pattern, label in forbidden_repository_rescans:
            if pattern.search(source):
                errors.append(
                    "packet28d broker repository consumers must use authenticated "
                    "daemon index runtimes, not rescan entrypoint "
                    f"{label!r}: {path.relative_to(root)}"
                )

    broker_child_names = "|".join(map(re.escape, PACKET28D_BROKER_MODULES))
    direct_child_route = re.compile(
        rf"(?<![\w:])(?:crate::)?broker::(?:{broker_child_names})\b"
    )
    grouped_child_route = re.compile(
        rf"(?<![\w:])(?:crate::)?broker::\{{[^;}}]*"
        rf"\b(?:{broker_child_names})\b\s*(?:::|,|}})",
        re.DOTALL,
    )
    for path in sorted(source_root.rglob("*.rs")):
        if path == broker_facade_path or broker_root in path.parents:
            continue
        try:
            source = path.read_text(encoding="utf-8")
        except OSError as error:
            errors.append(f"cannot read {path.relative_to(root)}: {error}")
            continue
        if direct_child_route.search(source) or grouped_child_route.search(source):
            errors.append(
                "packet28d modules must consume broker ports through the owning "
                f"facade: {path.relative_to(root)}"
            )

    return errors


def check_architecture(
    metadata: dict[str, Any], source_root: Path | None = None
) -> list[str]:
    graph = CargoMetadataGraph(metadata)
    errors: list[str] = []
    for rule in architecture_rules(graph):
        paths = graph.shortest_forbidden_paths(rule.source, rule.forbidden)
        for forbidden, path in sorted(paths.items()):
            errors.append(
                f"{rule.source} reaches forbidden normal dependency {forbidden}: "
                f"{' -> '.join(path)}"
            )
    if "tokio" in graph.names.values():
        for package_id in sorted(
            graph.workspace_members,
            key=lambda candidate: graph.names[candidate],
        ):
            source = graph.names[package_id]
            if source in TOKIO_RUNTIME_OWNERS:
                continue
            paths = graph.shortest_forbidden_paths(source, {"tokio"})
            if path := paths.get("tokio"):
                errors.append(
                    f"{source} reaches async runtime outside an orchestration "
                    f"boundary: {' -> '.join(path)}"
                )
    core_dependencies = graph.direct_dependency_names("context-kernel-core")
    if core_dependencies != {"context-kernel-builtins"}:
        errors.append(
            "context-kernel-core compatibility facade must have exactly one "
            "normal dependency, context-kernel-builtins; found "
            f"{', '.join(sorted(core_dependencies)) or '<none>'}"
        )
    builtins_dependencies = graph.direct_dependency_names(
        "context-kernel-builtins"
    )
    if "context-kernel-mechanism" not in builtins_dependencies:
        errors.append(
            "context-kernel-builtins must depend directly on "
            "context-kernel-mechanism"
        )
    if source_root is not None:
        errors.extend(check_kernel_source_boundaries(source_root))
        errors.extend(check_packet28d_source_boundaries(source_root))
    return errors


def read_metadata(path: Path | None) -> dict[str, Any]:
    if path is not None:
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except OSError as error:
            raise MetadataError(
                f"cannot read metadata fixture {path}: {error}"
            ) from error
        except json.JSONDecodeError as error:
            raise MetadataError(
                f"metadata fixture {path} is not valid JSON: {error}"
            ) from error
    else:
        command = ["cargo", "metadata", "--locked", "--format-version", "1"]
        result = subprocess.run(
            command,
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode != 0:
            detail = result.stderr.strip() or "cargo metadata failed without output"
            raise MetadataError(detail)
        try:
            value = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise MetadataError(
                f"cargo metadata emitted invalid JSON: {error}"
            ) from error

    if not isinstance(value, dict):
        raise MetadataError("metadata root must be a JSON object")
    return value


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="check Cargo normal-dependency architecture boundaries"
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        metavar="PATH",
        help=(
            "read Cargo metadata JSON from PATH instead of invoking "
            "'cargo metadata --locked --format-version 1'"
        ),
    )
    parser.add_argument(
        "--source-root",
        type=Path,
        default=ROOT,
        metavar="PATH",
        help="repository root used for source-boundary checks",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        errors = check_architecture(read_metadata(args.metadata), args.source_root)
    except MetadataError as error:
        print(f"architecture dependency invariant failed: {error}", file=sys.stderr)
        return 1

    if errors:
        for error in errors:
            print(f"architecture dependency invariant failed: {error}", file=sys.stderr)
        return 1

    print("architecture dependency invariant passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
