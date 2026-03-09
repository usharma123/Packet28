# Context Store

## Overview

The context store persists reducer packets, recall indexes, and task state across process invocations. It supports long-horizon agent workflows where prior context must survive restarts, and enables recall — querying past packets by text, paths, symbols, and tests.

## Storage Layout

Root directory: `.packet28/` under the workspace root.

```
.packet28/
├── packet-cache-v2.bin          Packet cache with indexes (bincode)
├── artifacts/                   Full packet artifacts for --json=handle
│   └── <handle_id>.json
├── agent/
│   └── latest-preflight.json    Last preflight result from packet28-agent
└── daemon/
    ├── packet28d.sock           Unix socket
    ├── runtime.json             Daemon PID and metadata
    ├── packet28d.log            Daemon log
    ├── watch-registry-v1.json   Active file watches
    ├── task-registry-v1.json    Task state
    └── tasks/
        └── <task-id>/
            └── events.jsonl     Per-task event log
```

## Cache Format (V2)

The cache is stored as `packet-cache-v2.bin` using bincode with a versioned envelope:

```rust
struct PersistEnvelopeV2 {
    version: u32,                              // 2
    entries: Vec<PacketCacheEntry>,
    recall_postings: HashMap<String, Vec<(String, usize)>>,  // BM25 term index
    recall_docs: HashMap<String, RecallDocument>,
    file_ref_index: HashMap<String, BTreeSet<String>>,
    basename_alias_index: HashMap<String, BTreeSet<String>>,
    symbol_index: HashMap<String, BTreeSet<String>>,
    test_index: HashMap<String, BTreeSet<String>>,
    task_index: HashMap<String, BTreeSet<String>>,
}
```

The V2 format persists all indexes alongside cache entries, so recall is immediately available on load without reindexing.

### Schema Migration

- V1 files (`packet-cache-v1.bin`) are loaded and indexes rebuilt on first access
- V2 files include pre-built indexes
- Unknown versions are ignored (empty cache, warning logged)
- Corrupt files are ignored (empty cache, no hard failure)

## PacketCacheEntry

Each cached packet stores:

```rust
struct PacketCacheEntry {
    cache_key: String,                 // blake3 hash of canonical packet
    target: String,                    // Reducer target (e.g. "diffy.analyze")
    created_at_unix: u64,
    body: Value,                       // Full EnvelopeV1 JSON
    metadata: Value,                   // Execution metadata (timing, cache info)
}
```

## Recall System

The recall system supports two query modes:

### BM25 Full-Text Search

Tokenized packet content (summaries, paths, symbols, payload text) is indexed in an inverted posting list. Queries are scored using BM25 (k1=1.5, b=0.75):

```
score = IDF * (tf * (k1 + 1)) / (tf + k1 * (1 - b + b * doc_len / avg_doc_len))
```

### Structured Field Matching

In addition to text search, recall matches against structured indexes:
- **Path index**: Canonical file paths and basename aliases
- **Symbol index**: Class/function/method names
- **Test index**: Test names
- **Task index**: Task IDs for scoped recall

### Recall Scopes

```rust
enum RecallScope {
    Global,        // Search all cached entries
    TaskFirst,     // Task-scoped entries ranked first, then global
    TaskOnly,      // Only entries associated with a specific task
}
```

### Recall Options

```rust
RecallOptions {
    limit: 8,                          // Max results
    since_unix: Some(week_ago),        // Time window
    until_unix: None,
    target: Some("diffy.analyze"),     // Filter by reducer
    task_id: Some("task-123"),
    scope: RecallScope::TaskFirst,
    packet_types: vec![],
    path_filters: vec!["src/auth.rs"],
    symbol_filters: vec!["AuthService"],
}
```

### Recall Results

```rust
RecallHit {
    cache_key: String,
    target: String,
    score: f64,
    summary: Option<String>,
    snippet: String,
    matched_paths: Vec<String>,
    matched_symbols: Vec<String>,
    match_reasons: Vec<String>,        // "bm25_text", "path_match", "symbol_match"
    packet_types: Vec<String>,
    task_ids: Vec<String>,
    budget_estimate: RecallBudgetEstimate,
    age_secs: u64,
}
```

## TTL and Eviction

- Default TTL: 86400 seconds (24 hours)
- Pruning runs on load and after cache mutations
- Expired entries are removed from all indexes

## CLI Commands

```bash
# List cached entries
Packet28 context store list --root . --json

# Get a specific entry
Packet28 context store get --root . --key <cache_key> --json

# Prune expired entries
Packet28 context store prune --root . --json

# Show store statistics
Packet28 context store stats --root . --json

# Recall prior context
Packet28 context recall --root . --query "coverage gap" --limit 5 --json
```

## Daemon Integration

When the daemon is running, it owns the persistent cache. Commands routed via `--via-daemon` use the daemon's in-memory cache, which is periodically flushed to disk. Direct CLI commands (without `--via-daemon`) load from disk on each invocation.

## Corruption and Recovery

- If the cache file is unreadable or has an unknown version: start with an empty cache, log a warning
- If the daemon crashes: cache state from the last flush is preserved on disk
- No manual recovery steps required — the system self-heals on the next successful write
