# Development and validation

Packet28 treats reproducibility, architecture, tests, and performance evidence
as part of the implementation. This guide summarizes the local workflow; the
contribution policy is [CONTRIBUTING.md](../CONTRIBUTING.md).

## Toolchains

- Current pinned toolchain: `rust-toolchain.toml`
- Declared MSRV: Rust 1.88.0
- Dependency graph: committed `Cargo.lock`
- Direct-minimum graph: committed `Cargo.direct-minimal.lock`
- Supply-chain tool: checksum-pinned `cargo-deny` 0.20.2

The current toolchain catches new lints and runs the normal development gate.
The exact MSRV lane proves the supported language and dependency floor.

## Fast loop

Run package-specific tests while editing:

```bash
cargo test -p <package> --all-features --locked
cargo clippy -p <package> --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
```

Then run:

```bash
scripts/validate_refactor_batch.sh
```

This is the fast repository batch, not the release gate.

## Canonical gate

Inspect the commands:

```bash
scripts/validate_full_gate.sh --list
```

Run them:

```bash
scripts/validate_full_gate.sh
```

The gate covers:

1. workspace, lockfile, dependency, architecture, hazard, and test-harness
   policy;
2. audit-ledger and generated README integrity;
3. script unit tests and maintained experiment verifiers;
4. formatting, workspace check/build, strict Clippy;
5. all-target/all-feature tests, doctests, and strict rustdoc;
6. `cargo-deny`;
7. source-package, Cargo-package, and publication-policy checks.

PR CI runs policy, lint/docs, workspace tests, dependency auditing, and MSRV
as separate jobs. The required `quality` check succeeds only when every required
job and every selected optional job succeeds. New PR commits cancel obsolete
runs. MSRV runs only the locked workspace check, without repeating policy tests.

Packaging and direct-minimum compilation run on PRs that change manifests,
lockfiles, toolchain/configuration, CI, or validation scripts. Packaging also
runs for npm packages, build scripts, and license changes. Ordinary Rust source
edits defer those two checks to main, the weekly Monday run, or a manual run;
all of those run every phase. Release validation still uses the complete gate,
including explicit workspace check/build and packaged-source compilation.

To rerun an individual CI job locally, use `--phase policy`, `--phase lint`,
`--phase docs`, `--phase tests`, `--phase audit`, `--phase dependencies`, or
`--phase packages`. `--list` works with every mode.

Exact MSRV:

```bash
rustup run 1.88.0 scripts/validate_full_gate.sh --msrv
```

Release candidate:

```bash
scripts/validate_full_gate.sh --release-tag vX.Y.Z
```

## Test strategy

Choose the smallest test that proves the invariant, then cover boundary
failures proportionately.

| Change | Expected coverage |
| --- | --- |
| Pure reducer logic | unit tests, malformed input, property/parity cases |
| Wire or public API | JSON/snapshot compatibility, compile tests, doctests |
| Persistence | restart, partial write, corruption, retry, failure atomicity |
| Lifecycle/concurrency | barriers/channels, cancellation, drain/join, bounded deadlines |
| Filesystem authority | symlink/hard-link/swap, permissions, identity revalidation |
| Architecture | metadata and source guards with synthetic violating fixtures |
| Performance | behavior parity plus versioned before/after measurements |
| CLI/MCP process flow | shared bounded process harness and cleanup assertions |

Avoid timing-only concurrency tests. The shared integration harness owns child
processes, MCP framing, timeouts, and drop cleanup; see
[Integration-test harness](integration-test-harness.md).

## Dependency changes

Workspace dependencies are declared once in `[workspace.dependencies]` and
inherited by members.

```bash
python3 scripts/check_direct_dependencies.py
python3 scripts/validate_direct_minimum.py
cargo deny --locked check
```

When changing the graph:

1. update the workspace dependency source;
2. update `Cargo.lock`;
3. update the direct-minimum manifest/lock evidence;
4. run the exact MSRV lane;
5. explain native, runtime, license, and advisory implications.

The direct-minimum graph intentionally answers a different question from the
normal lockfile. Read [Direct-minimum dependencies](direct-minimum-dependencies.md).

## Architecture changes

Run:

```bash
python3 scripts/check_architecture.py
cargo test --locked -p packet28-search-core --test module_architecture --all-features
```

Update the relevant contract when changing:

- context-kernel composition;
- daemon protocol/runtime/storage direction;
- async-runtime locality;
- public compatibility exports;
- runtime adapter ownership;
- test-harness process ownership.

Do not route around a dependency rule with a re-export or feature that recreates
the prohibited edge.

## Performance and experiments

Evidence belongs in a versioned directory under `benchmarks/` or
`docs/experiments/`. Record:

- source SHA and patch identity;
- toolchain, host, profile, and command;
- fixture identity and operation count;
- raw samples and summary method;
- behavior-parity assertion;
- accepted/rejected decision and scope.

The [agent DX benchmark](agent-dx-benchmark.md) is the required workflow
benchmark. It gates correctness, evidence, recovery and authority, and reports
savings and latency without gating them. Add a hard performance floor only with
a fixed corpus, repeated source-bound measurements and a stated product reason.

Architectural experiments should be feature-gated until their output parity,
failure behavior, and performance justify adoption. Historical or external
measurements remain labeled as such.

The stable-prefix experiment separates:

- stable repository instructions;
- adaptive task/broker brief;
- local hash and cache evidence;
- provider placement/cost/adherence evidence.

Only the first three can be established locally without provider telemetry.

## Documentation and generated files

Run:

```bash
python3 scripts/verify_readme_stats.py --check
python3 scripts/check_architecture_audit_ledger.py
```

Use `--write` for the README stats generator after workspace membership or
tracked Rust files change. Generated audit snapshot/final-gate blocks must be
updated with their owning script rather than hand-edited.

Do not include test-generated `.packet28` caches, local benchmark output,
provider credentials, or user repositories in a commit.

## Release flow

Release validation checks:

- tag, Cargo, npm, platform package, and release-note version agreement;
- immutable CI/action and cross-tool inputs;
- platform package contents, executable modes, headers, and binary identity;
- Cargo package contents and internal dependency publication policy;
- the locked graph, MSRV, rustdoc, supply-chain policy, and canonical tests.

See [Release package verification](release-package-verification.md) and
[Cargo publication policy](cargo-publication-policy.md).
