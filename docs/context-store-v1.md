# Context Store V1

## Goals
- Persist reducer packet cache across process invocations.
- Reuse prior context safely for long-horizon agent workflows.
- Keep default runtime behavior backward compatible when persistence is disabled.

## Scope
- Applies to `context-memory-core`, `context-kernel-core`, and `Packet28` CLI integration.
- Runtime persistence is now implemented behind explicit CLI opt-in flags.

## Storage Model
- Root directory: `.packet28/` under workspace root (or explicit root path).
- Data file: `.packet28/packet-cache-v1.bin`.
- Encoding: `bincode` over a versioned envelope:
  - `version: u32` (initial value `1`)
  - `entries: Vec<PacketCacheEntry>`

## Persistence Types
- `PersistConfig`
  - `root_dir: PathBuf`
  - `ttl_secs: u64` (default `86400`, 24h)
- `PersistEnvelope`
  - `version: u32`
  - `entries: Vec<PacketCacheEntry>`

## Lifecycle
1. Kernel startup with persistence enabled:
   - Load cache file if present.
   - On parse/corruption failure: log warning, continue with empty cache.
   - Apply TTL pruning immediately after load.
2. Reducer execution cache mutation:
   - After `put_with_hooks`, prune expired entries.
   - Serialize and write atomically to `packet-cache-v1.bin`.
3. Cache read path:
   - Regular in-memory lookup behavior remains unchanged.

## TTL and Eviction
- Entry expiration computed as `now_unix - created_at_unix > ttl_secs`.
- Pruning removes expired entries from both:
  - `entries_by_hash`
  - `latest_request_index`

## Kernel Integration
- Add `persist_config: Option<PersistConfig>` to `Kernel`.
- New constructor:
  - `Kernel::with_v1_reducers_and_persistence(config: PersistConfig) -> Self`
- Existing constructor remains:
  - `Kernel::with_v1_reducers()` (no persistence, current behavior)

## CLI Integration
- Opt-in `--cache` is available on:
  - `Packet28 map repo`
  - `Packet28 proxy run`
- When enabled:
  - `map repo` uses `--repo-root` as `PersistConfig.root_dir`.
  - `proxy run` uses `--cwd` when provided, otherwise process current working directory.
  - CLI instantiates `Kernel::with_v1_reducers_and_persistence(...)`.
- Default behavior remains non-persistent unless `--cache` is set.

## Corruption and Recovery
- If cache file is unreadable or invalid:
  - Do not fail command execution.
  - Start with empty cache.
  - Overwrite with fresh cache on next successful save.

## Compatibility
- Binary format versioned (`version=1`) to support forward migration.
- If version mismatch:
  - Ignore old file and continue with empty cache (plus warning).
- No migration logic required in V1.

## Test Coverage
- Roundtrip: save/load preserves entry lookup hits.
- TTL: expired entries are removed during load and save.
- Corrupt file: load returns empty cache, no hard failure.
- Kernel integration: cache survives kernel instance restart and serves a cache hit.
