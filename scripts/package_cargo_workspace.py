#!/usr/bin/env python3
"""Verify Cargo packages in a disposable mirror without enabling publication."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
from pathlib import PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from typing import Mapping, Sequence


SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import check_cargo_publish_policy as publish_policy


ROOT = SCRIPT_DIR.parent
TARGET_DIRECTORY_VARIABLES = ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR")
PRIVATE_PUBLISH_LINE = re.compile(
    r"(?m)^publish(?:\.workspace)?\s*=\s*(?:true|false|\[\])\s*$"
)


def workspace_inputs(root: Path) -> list[Path]:
    """Return tracked and non-ignored untracked inputs from the worktree."""

    result = subprocess.run(
        [
            "git",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        cwd=root,
        check=False,
        capture_output=True,
    )
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise RuntimeError(f"git ls-files failed: {detail}")
    inputs: list[Path] = []
    for encoded in result.stdout.split(b"\0"):
        if not encoded:
            continue
        relative = Path(os.fsdecode(encoded))
        if (root / relative).exists() or (root / relative).is_symlink():
            inputs.append(relative)
    return inputs


def copy_workspace(root: Path, destination: Path, inputs: Sequence[Path]) -> None:
    """Copy current worktree inputs while preserving symlinks."""

    for relative in inputs:
        source = root / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.symlink_to(os.readlink(source))
        elif source.is_file():
            shutil.copy2(source, target)


def verification_manifest(text: str) -> str:
    """Make one private member package eligible only inside the mirror."""

    matches = list(PRIVATE_PUBLISH_LINE.finditer(text))
    if len(matches) != 1:
        raise ValueError(
            "private member manifest must contain exactly one publish policy line"
        )
    return PRIVATE_PUBLISH_LINE.sub('publish = ["crates-io"]', text, count=1)


def prepare_verification_manifests(
    root: Path,
    mirror: Path,
    packages: Mapping[str, Mapping[str, object]],
    private_packages: set[str],
) -> None:
    """Enable Cargo's temporary-registry verification only in the mirror."""

    for name in sorted(private_packages):
        package = packages[name]
        manifest_path = package.get("manifest_path")
        if not isinstance(manifest_path, str):
            raise ValueError(f"{name}: Cargo metadata has no manifest path")
        relative = Path(manifest_path).relative_to(root)
        mirror_manifest = mirror / relative
        text = mirror_manifest.read_text(encoding="utf-8")
        mirror_manifest.write_text(verification_manifest(text), encoding="utf-8")


def target_dir_arguments(target_dir: Path | None) -> tuple[str, ...]:
    """Bind Cargo output explicitly, overriding inherited environment or config."""

    return () if target_dir is None else ("--target-dir", str(target_dir))


def package_command(target_dir: Path | None = None) -> tuple[str, ...]:
    """Return the exact locked package-assembly command."""

    return (
        "cargo",
        "package",
        "--workspace",
        "--all-features",
        "--locked",
        "--offline",
        "--no-verify",
        "--allow-dirty",
    ) + target_dir_arguments(target_dir)


def packaged_check_command(target_dir: Path | None = None) -> tuple[str, ...]:
    """Return the check that compiles only source recovered from archives."""

    return (
        "cargo",
        "check",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--locked",
        "--offline",
    ) + target_dir_arguments(target_dir)


def archive_relative_path(member_name: str, prefix: str) -> Path | None:
    """Return a safe package-relative archive member path."""

    path = PurePosixPath(member_name)
    if (
        path.is_absolute()
        or ".." in path.parts
        or not path.parts
        or "\\" in member_name
    ):
        raise ValueError(f"unsafe Cargo package archive path: {member_name!r}")
    if path.parts[0] != prefix:
        raise ValueError(
            f"Cargo package archive member is outside {prefix!r}: {member_name!r}"
        )
    if len(path.parts) == 1:
        return None
    return Path(*path.parts[1:])


def unpack_package_archive(archive: Path, destination: Path, prefix: str) -> None:
    """Extract regular package files without trusting tar paths or links."""

    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, mode="r:gz") as package:
        for member in package:
            relative = archive_relative_path(member.name, prefix)
            if relative is None:
                continue
            target = destination / relative
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(
                    f"Cargo package archive contains unsupported link or device: "
                    f"{member.name}"
                )
            source = package.extractfile(member)
            if source is None:
                raise ValueError(f"Cargo package archive member is unreadable: {member.name}")
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("wb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(member.mode & 0o777)


def prepare_packaged_workspace(
    source_root: Path,
    mirror: Path,
    destination: Path,
    packages: Mapping[str, Mapping[str, object]],
    archive_directory: Path,
) -> None:
    """Rebuild a workspace exclusively from the generated package archives."""

    destination.mkdir(parents=True)
    for workspace_file in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
        source = mirror / workspace_file
        if source.is_file():
            shutil.copy2(source, destination / workspace_file)

    for name, package in sorted(packages.items()):
        version = package.get("version")
        manifest_path = package.get("manifest_path")
        if not isinstance(version, str) or not isinstance(manifest_path, str):
            raise ValueError(f"{name}: Cargo metadata lacks version or manifest path")
        relative_manifest = Path(manifest_path).relative_to(source_root)
        crate_destination = destination / relative_manifest.parent
        prefix = f"{name}-{version}"
        archive = archive_directory / f"{prefix}.crate"
        if not archive.is_file():
            raise ValueError(f"{name}: Cargo package archive is missing")
        unpack_package_archive(archive, crate_destination, prefix)

        original_manifest = crate_destination / "Cargo.toml.orig"
        if not original_manifest.is_file():
            raise ValueError(f"{name}: package archive lacks Cargo.toml.orig")
        shutil.copy2(original_manifest, crate_destination / "Cargo.toml")
        original_manifest.unlink()


def verify_packages(root: Path) -> None:
    """Check policy, build disposable package archives, and verify their contents."""

    metadata = publish_policy.load_metadata(root)
    policy = publish_policy.load_policy(root)
    packages = publish_policy.workspace_packages(metadata)
    package_files = publish_policy.collect_package_files(root, packages)
    errors = publish_policy.policy_errors(metadata, policy, package_files)
    if errors:
        detail = "\n".join(f"- {error}" for error in errors)
        raise RuntimeError(f"Cargo publication policy failed:\n{detail}")

    private_section = policy.get("private")
    if not isinstance(private_section, dict):
        raise ValueError("policy is missing [private]")
    private_values = private_section.get("packages")
    if not isinstance(private_values, list) or not all(
        isinstance(value, str) for value in private_values
    ):
        raise ValueError("private.packages must be a string array")
    private_packages = set(private_values)

    with tempfile.TemporaryDirectory(prefix="packet28-cargo-package-") as directory:
        mirror = Path(directory) / "workspace"
        mirror.mkdir()
        copy_workspace(root, mirror, workspace_inputs(root))
        prepare_verification_manifests(
            root,
            mirror,
            packages,
            private_packages,
        )

        environment = os.environ.copy()
        environment.pop("CARGO_REGISTRY_TOKEN", None)
        environment.pop("CARGO_REGISTRIES_CRATES_IO_TOKEN", None)
        for variable in TARGET_DIRECTORY_VARIABLES:
            environment.pop(variable, None)
        package_target = Path(directory) / "package-target"
        result = subprocess.run(
            package_command(package_target),
            cwd=mirror,
            env=environment,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"disposable Cargo package assembly exited {result.returncode}"
            )

        packaged_workspace = Path(directory) / "packaged-workspace"
        prepare_packaged_workspace(
            root,
            mirror,
            packaged_workspace,
            packages,
            package_target / "package",
        )
        result = subprocess.run(
            packaged_check_command(root / "target" / "cargo-package-archive-check"),
            cwd=packaged_workspace,
            env=environment,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"packaged-source workspace check exited {result.returncode}"
            )


def main(argv: Sequence[str] | None = None) -> int:
    """Run disposable package verification."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        verify_packages(args.root.resolve())
    except (OSError, RuntimeError, ValueError) as error:
        print(f"Cargo package verification failed: {error}", file=sys.stderr)
        return 1
    print(
        "Cargo package assembly and packaged-source check passed "
        "in a disposable publication mirror"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
