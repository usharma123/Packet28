#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/validate_full_gate.sh [--list] [--msrv | --phase PHASE | --release-tag TAG]

Runs the canonical repository gate. The default mode fetches the locked
dependency graph, then verifies repository policy, formatting, locked workspace
check/build, strict Clippy, all tests and doctests, strict rustdoc, cargo-deny
policy, offline npm package dry-runs, and Cargo package assembly.

Pass --msrv to run only the locked workspace check on the minimum Rust version.
Pass --phase to run one CI phase: policy, lint, tests, docs, audit,
dependencies, or packages. The default still runs the complete release gate.
Pass --release-tag to additionally verify the tag, Cargo version, npm package
versions, and release-note filename before any release work.
Pass --list to print the selected commands without executing them.
USAGE
}

list_only=false
msrv_only=false
release_tag=""
phase="full"

while (($#)); do
  case "$1" in
    --list)
      list_only=true
      shift
      ;;
    --msrv)
      msrv_only=true
      shift
      ;;
    --phase)
      [[ $# -ge 2 ]] || { echo "--phase requires a value" >&2; exit 2; }
      phase="$2"
      case "$phase" in
        policy|lint|tests|docs|audit|dependencies|packages) ;;
        *) echo "unknown phase: $phase" >&2; exit 2 ;;
      esac
      shift 2
      ;;
    --release-tag)
      [[ $# -ge 2 ]] || {
        echo "--release-tag requires a value" >&2
        exit 2
      }
      release_tag="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if { [[ "$msrv_only" == true ]] && { [[ -n "$release_tag" ]] || [[ "$phase" != full ]]; }; } ||
   { [[ -n "$release_tag" ]] && [[ "$phase" != full ]]; }; then
  echo "--msrv, --phase and --release-tag cannot be combined" >&2
  exit 2
fi

run_cmd() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  if [[ "$list_only" == false ]]; then
    "$@"
  fi
}

# MSRV uses its own toolchain; policy already runs once in the policy job.
if [[ "$msrv_only" == true ]]; then
  run_cmd cargo check --workspace --all-targets --all-features --locked
  exit 0
fi

selected() {
  [[ "$phase" == full || "$phase" == "$1" ]]
}

if selected policy; then
  # Workspace policy discovers every tracked Cargo workspace, fetches each exact
  # lockfile graph, then resolves the same manifests offline. Keeping discovery
  # and bootstrap in one loop prevents auxiliary lockfiles from being masked by a
  # warm cache.
  run_cmd scripts/verify_workspace_policy.sh --bootstrap
  run_cmd python3 scripts/check_direct_dependencies.py
  run_cmd python3 scripts/check_architecture.py
  run_cmd python3 scripts/check_architecture_audit_ledger.py
  run_cmd python3 scripts/check_instruction_claims.py
  run_cmd python3 scripts/check_test_harness.py
  run_cmd python3 benchmarks/asy-04-runtime-starvation/verify.py
  run_cmd python3 benchmarks/per-03-incremental-index/verify.py
  run_cmd python3 scripts/verify_ci_policy.py
  run_cmd python3 scripts/verify_tooling.py
  run_cmd python3 scripts/verify_readme_stats.py --check
  run_cmd python3 -m unittest discover -s scripts/tests -p 'test_*.py'
fi

if [[ -n "$release_tag" ]]; then
  # A release tag must point at the ledger-only finalization commit. The
  # immediately preceding revision is the exact source snapshot recorded by
  # that ledger commit.
  run_cmd python3 scripts/check_architecture_audit_ledger.py \
    --final --source-rev HEAD^
  run_cmd python3 scripts/verify_release_version.py --root . --tag "$release_tag"
fi

if selected dependencies; then
  run_cmd python3 scripts/validate_direct_minimum.py
fi

# Full/release validation retains explicit build coverage. PRs compile through
# Clippy and tests, avoiding two extra passes over the same workspace.
if [[ "$phase" == full ]]; then
  run_cmd cargo check --workspace --all-targets --all-features --locked
  run_cmd cargo build --workspace --all-targets --all-features --locked
fi
if selected lint; then
  # `cargo fmt` does not resolve dependencies and does not accept `--locked`.
  run_cmd cargo fmt --all -- --check
  # Hazard checks already run strict all-target Clippy, plus production panic lints.
  run_cmd python3 scripts/check_rust_hazards.py
fi
if selected tests; then
  run_cmd cargo test --workspace --all-targets --all-features --locked
  run_cmd cargo test --workspace --doc --all-features --locked
fi
if selected docs; then
  run_cmd env RUSTDOCFLAGS="-D warnings -D rustdoc::broken_intra_doc_links" \
    cargo doc --workspace --all-features --no-deps --locked
fi
if selected audit; then
  run_cmd cargo deny --locked check
fi
if selected packages; then
  # The full gate fetched every workspace during policy. An isolated package
  # job must populate its own registry cache before archive checks go offline.
  if [[ "$phase" == packages ]]; then
    run_cmd cargo fetch --locked
  fi
  run_cmd python3 scripts/verify_release_packages.py source
  run_cmd python3 scripts/package_cargo_workspace.py
fi
