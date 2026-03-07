# Packet Envelope V1

## Purpose

`EnvelopeV1<T>` is the canonical internal packet frame for all Packet28 reducers.

Design rule: canonical internally, bounded externally.

Every reducer constructs and returns `EnvelopeV1<T>` before any wire-profile projection.

## Structure

```rust
struct EnvelopeV1<T> {
    version: String,           // Always "1"
    tool: String,              // Reducer tool name (e.g. "diffy", "stacky", "mapy")
    kind: String,              // Packet kind (e.g. "diff_analyze", "coverage_gate")
    hash: String,              // Canonical blake3 hash
    summary: String,           // Human-readable summary of the packet content
    files: Vec<FileRef>,       // File references with relevance scores
    symbols: Vec<SymbolRef>,   // Symbol references with kinds and relevance
    risk: Option<RiskLevel>,   // Low / Medium / High / Critical
    confidence: Option<f64>,   // 0.0 to 1.0
    budget_cost: BudgetCost,   // Token/byte/runtime estimates
    provenance: Provenance,    // Input tracking and git context
    payload: T,                // Reducer-specific payload
}
```

## Required Fields

Always present in every packet:

- `version`, `tool`, `kind`, `hash`, `summary`
- `budget_cost`, `provenance`, `payload`

## Optional Fields

May be omitted when empty or not applicable:

- `files`, `symbols`, `risk`, `confidence`

## FileRef and SymbolRef

```rust
struct FileRef {
    path: String,
    relevance: Option<f64>,
    source: Option<String>,    // Which reducer produced this ref
}

struct SymbolRef {
    name: String,
    file: Option<String>,
    kind: Option<String>,      // "class", "method", "function", etc.
    relevance: Option<f64>,
}
```

These structured references enable cross-packet correlation. The `context correlate` command uses `files` and `symbols` to find `shared_file`, `shared_symbol`, and `map_edge_connects` relationships across packets.

## Budget Cost

```rust
struct BudgetCost {
    est_tokens: u64,               // Estimated token count of the full packet
    est_bytes: usize,              // Estimated byte size
    runtime_ms: u64,               // Wall-clock time to produce this packet
    tool_calls: u64,               // Number of tool invocations
    payload_est_tokens: Option<u64>,  // Payload-only token estimate
    payload_est_bytes: Option<usize>, // Payload-only byte estimate
}
```

`with_canonical_hash_and_real_budget()` computes the budget estimates by serializing the packet, measuring its size, and then re-sealing the hash. This is called after construction to ensure estimates reflect the actual serialized size.

## Provenance

```rust
struct Provenance {
    inputs: Vec<String>,           // Input file paths that produced this packet
    git_base: Option<String>,      // Git base ref (e.g. "origin/main")
    git_head: Option<String>,      // Git head ref (e.g. "HEAD")
    generated_at_unix: u64,        // Unix timestamp of generation
}
```

## Canonical Hash Rules

`EnvelopeV1::canonical_hash()` uses canonical JSON key ordering and excludes volatile runtime values.

Excluded from hash computation:

- `hash` (self-referential)
- `budget_cost.est_tokens`, `est_bytes`, `payload_est_tokens`, `payload_est_bytes`
- `budget_cost.runtime_ms`
- `provenance.generated_at_unix`

Hash algorithm: blake3 over canonicalized JSON bytes.

Invariant: same semantic packet produces the same hash across runs and JSON key order differences.

## Canonical Serialization Rules

- Object keys are sorted lexically before hashing
- Arrays preserve order (order is semantic)
- Required fields are always present
- Optional vectors are omitted when empty (where configured with `skip_serializing_if`)
- Optional scalars are omitted for `None`

## Registered Packet Types

| Packet Type | Tool | Kind |
| --- | --- | --- |
| `suite.cover.check.v1` | `covy` | `coverage_gate` |
| `suite.diff.analyze.v1` | `diffy` | `diff_analyze` |
| `suite.test.impact.v1` | `testy` | `test_impact` |
| `suite.stack.slice.v1` | `stacky` | `stack_slice` |
| `suite.build.reduce.v1` | `buildy` | `build_reduce` |
| `suite.map.repo.v1` | `mapy` | `repo_map` |
| `suite.proxy.run.v1` | `proxy` | `command_summary` |
| `suite.context.assemble.v1` | `contextq` | `assembled_context` |
| `suite.context.correlate.v1` | `contextq` | `correlation` |
| `suite.context.manage.v1` | `contextq` | `context_manage` |
| `suite.guard.check.v1` | `guardy` | `guard_check` |
