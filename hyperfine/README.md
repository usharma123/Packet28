# Hyperfine Benchmark Suite (Published `covy`)

This suite benchmarks `covy` from the public release path (`cargo install covy-cli`), not from local workspace binaries.

## What This Covers

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

## Usage

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

## Output

- JSON timing reports: `hyperfine/results/<timestamp>/*.json`
- Markdown timing tables: `hyperfine/results/<timestamp>/*.md`
- Generated synthetic fixtures: `hyperfine/generated/`
- Isolated benchmark project: `hyperfine/project/`

