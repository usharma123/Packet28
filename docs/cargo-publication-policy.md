# Cargo publication policy

Packet28's supported release channel is the versioned npm package assembled by
`.github/workflows/release.yml`. The current workflow has no Cargo publication
job, registry credential, or documented `cargo install` path. Every Rust
workspace member is therefore private and inherits `publish = false`.

This is an explicit decision, not an inference from missing metadata:

- `scripts/cargo_publish_policy.toml` partitions every workspace member into
  the crates.io allowlist or the private list. The crates.io allowlist is empty.
- `scripts/check_cargo_publish_policy.py` rejects an unclassified member,
  accidental publication enablement, an incomplete publish dependency closure,
  incompatible internal version requirements, invalid publication order, and
  sensitive files in Cargo package contents.
- `scripts/package_cargo_workspace.py` remains in the canonical gate. It copies
  the worktree into a temporary directory, enables Cargo's local package
  assembly only in that disposable mirror, removes registry tokens from the
  subprocess environment, and runs the locked, offline, all-feature workspace
  package command. Compilation is already covered by the preceding workspace
  check, build, Clippy, test, doctest, and rustdoc gates; the mirror uses
  `--no-verify` only to avoid resolving private internal crates from crates.io.
  It then reconstructs a second workspace exclusively from the generated
  archives and runs a locked, offline, all-target, all-feature `cargo check`.
  Both Cargo commands receive an explicit `--target-dir`: archives go to a
  disposable directory beside the mirror, and the packaged-source check uses
  `target/cargo-package-archive-check` under the source root, so an inherited
  `CARGO_TARGET_DIR` or Cargo `build.target-dir` cannot reroute either step.
  The guarded source manifests remain non-publishable.

The repository history records a narrower, historical crates.io experiment:
commits `15cf4bef` and `90677838` prepared `covy-core`, `covy-ingest`, and
`covy-cli` for versions `0.1.0` and `0.2.0`. That three-crate graph is no
longer the current workspace graph and does not authorize publishing later
workspace versions.

## Reopening crates.io publication

A future change must be reviewed as one release-policy unit:

1. Add every intended crate and its complete internal dependency closure to the
   `publish.packages` allowlist.
2. Give each public crate an accurate crate-specific description, README,
   keywords, and categories. The old workspace-wide generic description is
   deliberately rejected for public crates.
3. Declare a dependency-first `publish.order` and keep every internal version
   requirement compatible with the version being packaged.
4. Add a credential-isolated Cargo publication job and registry dry-run to the
   release workflow.
5. Verify crates.io name ownership, current registry state, and credentials at
   release time. Those external facts are intentionally outside the offline
   repository gate.

Run the local policy and packaging checks with:

```bash
python3 scripts/check_cargo_publish_policy.py
python3 scripts/package_cargo_workspace.py
```
