# Hyperfine Benchmark Suite

## `covy` (Published Binary)

This suite benchmarks `covy` from the public release path (`cargo install covy-cli`), not from local workspace binaries.

### What It Covers

- CLI startup (`--version`, `--help`)
- Ingest speed across all core coverage formats:
  - `lcov`, `cobertura`, `jacoco`, `gocov`, `llvm-cov`
- Scale ingest:
  - `lcov` 100k / 1m lines
  - SARIF 50k / 200k issues
- Reporting paths:
  - terminal/json/below-threshold/summary-only
- Gate paths:
  - coverage-only
  - cached diagnostics state
  - direct SARIF parse
  - strict failing gate timing
- PR artifact paths:
  - `diff`, `comment`, `annotate`, `pr`
- Extended flows (full profile):
  - `impact record`, `impact plan`, `testmap build`, `shard plan`, `merge`

It also runs correctness smoke checks before timing to validate behavior, not just runtime.

### Usage

```bash
./hyperfine/run.sh
```

Optional knobs:

```bash
# Use a specific published version from crates.io
COVY_VERSION=0.2.0 ./hyperfine/run.sh

# Reinstall published binary before running
COVY_FORCE_INSTALL=1 ./hyperfine/run.sh

# Skip install and use a pre-existing binary
COVY_BIN="$HOME/.cargo/bin/covy" ./hyperfine/run.sh

# Keep only core groups (faster run)
HYPERFINE_PROFILE=core ./hyperfine/run.sh

# Tune runs
HYPERFINE_RUNS_SMALL=15 HYPERFINE_RUNS_MEDIUM=8 HYPERFINE_RUNS_LARGE=4 ./hyperfine/run.sh
```

### Output

- JSON timing reports: `hyperfine/results/<timestamp>/*.json`
- Markdown timing tables: `hyperfine/results/<timestamp>/*.md`
- Generated synthetic fixtures: `hyperfine/generated/`
- Isolated benchmark project: `hyperfine/project/`

## `Packet28` On `JavaTest`

Run the Packet28 benchmark harness with:

```bash
./hyperfine/run_packet28_javatest.sh
```

What it does:

- Builds `target/release/Packet28` unless `PACKET28_BIN` is set.
- Creates a dedicated git-backed benchmark repo under `JavaTest/.packet28-bench-repo`.
- Replays a deterministic two-commit Java change, then runs `mvn test` to refresh JaCoCo and Surefire artifacts.
- Benchmarks Packet28 commands with `hyperfine`.
- Emits runtime reports plus compact/full/handle token comparisons for the captured JSON outputs.
- Computes the combined `context assemble` input payload-estimate total from the generated input packets.
- Emits explicit pass/fail checks for:
  - `context assemble` compact containment against the combined input payload estimate
  - compact-vs-handle shrinkage for the bounded Packet28 packet captures
- Compares the current run to the latest previous accepted benchmark baseline when one exists.

Useful knobs:

```bash
# point at an existing binary
PACKET28_BIN=target/release/Packet28 ./hyperfine/run_packet28_javatest.sh

# tune hyperfine runs
PACKET28_WARMUP=2 PACKET28_RUNS=8 ./hyperfine/run_packet28_javatest.sh

# change the benchmark repo location
PACKET28_JAVATEST_REPO_DIR="$PWD/JavaTest/.packet28-bench-repo" ./hyperfine/run_packet28_javatest.sh
```

Outputs:

- Hyperfine JSON: `hyperfine/results/packet28-javatest/<timestamp>/hyperfine.json`
- Hyperfine Markdown: `hyperfine/results/packet28-javatest/<timestamp>/hyperfine.md`
- Combined summary: `hyperfine/results/packet28-javatest/<timestamp>/summary.md`
- Structured summary with acceptance state, checks, ratios, and optional baseline comparison: `hyperfine/results/packet28-javatest/<timestamp>/summary.json`
- Profile summary with per-profile token metrics, ratios, and shrinkage checks: `hyperfine/results/packet28-javatest/<timestamp>/profile-summary.json`
