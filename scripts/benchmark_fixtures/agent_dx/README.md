# Agent DX explicit-CLI fixtures

Frozen command output for `scripts/benchmark_agent_dx.py`. `manifest.json`
records each case's argv, exit code, role, contract, SHA-256 hashes and
provenance. Files under `../python`, `../javascript`, `../go` and `../infra` are
small synthetic samples from the retired hook suite, now replayed through their
real explicit routes.

Keep bytes exact (`.gitattributes`). Replace a fixture only with a real capture,
never padded or repeated text; record its capture time and tool version, update
the hashes, and label constructed input as `reconstruction` or `synthetic`. See
[the benchmark guide](../../../docs/agent-dx-benchmark.md).
