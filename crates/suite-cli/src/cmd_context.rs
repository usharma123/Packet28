use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use context_memory_core::{
    ContextStoreListFilter, ContextStorePaging, ContextStorePruneRequest, PacketCache,
    PersistConfig, RecallOptions,
};
use serde_json::{json, Value};

#[derive(Args)]
pub struct AssembleArgs {
    /// Path(s) to reducer packet JSON files.
    #[arg(long = "packet", alias = "input", required = true)]
    packets: Vec<String>,

    /// Max approximate token budget for assembled payload.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    budget_tokens: u64,

    /// Max byte budget for assembled payload JSON.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    budget_bytes: usize,

    /// Run governed assembly path using this context policy config (context.yaml).
    #[arg(long)]
    context_config: Option<String>,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    cache: bool,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

#[derive(Args)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommands,
}

#[derive(Subcommand)]
pub enum StoreCommands {
    /// List cached context entries
    #[command(alias = "ls")]
    List(StoreListArgs),
    /// Get one cached context entry by key
    Get(StoreGetArgs),
    /// Prune cached context entries
    #[command(alias = "gc")]
    Prune(StorePruneArgs),
    /// Show context store statistics
    Stats(StoreStatsArgs),
}

#[derive(Args)]
pub struct StoreListArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    root: String,

    /// Optional target substring filter
    #[arg(long)]
    target: Option<String>,

    /// Optional free-text filter over key/target/input hash
    #[arg(long)]
    query: Option<String>,

    /// Optional lower bound for created_at_unix (seconds)
    #[arg(long)]
    created_after: Option<u64>,

    /// Optional upper bound for created_at_unix (seconds)
    #[arg(long)]
    created_before: Option<u64>,

    /// Pagination offset
    #[arg(long, default_value_t = 0)]
    offset: usize,

    /// Maximum entries to return
    #[arg(long, default_value_t = 50)]
    limit: usize,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

#[derive(Args)]
pub struct StoreGetArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    root: String,

    /// Cache key to fetch
    #[arg(long)]
    key: String,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

#[derive(Args)]
pub struct StorePruneArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    root: String,

    /// Remove all entries
    #[arg(long)]
    all: bool,

    /// Remove entries older than this TTL (seconds)
    #[arg(long)]
    ttl_secs: Option<u64>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

#[derive(Args)]
pub struct StoreStatsArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    root: String,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

#[derive(Args)]
pub struct RecallArgs {
    /// Retrieval query text
    #[arg(long)]
    query: String,

    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    root: String,

    /// Maximum recall hits
    #[arg(long, default_value_t = 8)]
    limit: usize,

    /// Optional lower bound for created_at_unix (seconds)
    #[arg(long)]
    since: Option<u64>,

    /// Optional upper bound for created_at_unix (seconds)
    #[arg(long)]
    until: Option<u64>,

    /// Optional target substring filter
    #[arg(long)]
    target: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

pub fn run_assemble(args: AssembleArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let detail_mode = if profile == suite_packet_core::JsonProfile::Compact {
        "compact"
    } else {
        "rich"
    };
    let compact_assembly = profile == suite_packet_core::JsonProfile::Compact;
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let kernel = build_kernel(args.cache, std::env::current_dir()?);
    let target = if args.context_config.is_some() {
        "governed.assemble"
    } else {
        "contextq.assemble"
    };
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: target.to_string(),
        input_packets,
        budget: context_kernel_core::ExecutionBudget {
            token_cap: Some(args.budget_tokens),
            byte_cap: Some(args.budget_bytes),
            runtime_ms_cap: None,
        },
        policy_context: match args.context_config.as_ref() {
            Some(config_path) => json!({
                "config_path": config_path,
                "detail_mode": detail_mode,
                "compact_assembly": compact_assembly,
            }),
            None => json!({
                "detail_mode": detail_mode,
                "compact_assembly": compact_assembly,
            }),
        },
        ..context_kernel_core::KernelRequest::default()
    })?;

    let assembled = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<Value> =
        serde_json::from_value(assembled.body.clone())
            .map_err(|source| anyhow!("invalid context output packet: {source}"))?;
    if args.context_config.is_some() {
        let budget_hint = crate::cmd_common::budget_retry_hint(
            &response.metadata,
            args.budget_tokens,
            args.budget_bytes,
            "Packet28 context assemble --context-config <context.yaml>",
        );
        if args.legacy_json {
            crate::cmd_common::emit_json(
                &json!({
                    "schema_version": "suite.context.assemble.v1",
                    "final_packet": assembled.body,
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
                &envelope,
                profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                Some(json!({
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                })),
            )?;
        }
    } else {
        if args.legacy_json {
            crate::cmd_common::emit_json(
                &json!({
                    "schema_version": "suite.context.assemble.v1",
                    "packet": assembled.body,
                    "kernel_audit": {
                        "context": response.audit,
                    },
                    "kernel_metadata": {
                        "context": response.metadata,
                    },
                    "cache": {
                        "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
                &envelope,
                profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                Some(json!({
                    "kernel_audit": {
                        "context": response.audit,
                    },
                    "kernel_metadata": {
                        "context": response.metadata,
                    },
                    "cache": {
                        "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                })),
            )?;
        }
    }

    Ok(0)
}

pub fn run_store(args: StoreArgs) -> Result<i32> {
    match args.command {
        StoreCommands::List(args) => run_store_list(args),
        StoreCommands::Get(args) => run_store_get(args),
        StoreCommands::Prune(args) => run_store_prune(args),
        StoreCommands::Stats(args) => run_store_stats(args),
    }
}

pub fn run_recall(args: RecallArgs) -> Result<i32> {
    let cache = load_cache(&args.root)?;
    let now = current_unix();
    let since_default = now.saturating_sub(86_400);
    let hits = cache.recall(
        &args.query,
        &RecallOptions {
            limit: args.limit,
            since_unix: args.since.or(Some(since_default)),
            until_unix: args.until,
            target: args.target,
        },
    );

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.recall.v1",
                "query": args.query,
                "hits": hits,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    if hits.is_empty() {
        println!("(no recall hits)");
        return Ok(0);
    }

    for hit in hits {
        println!(
            "- score={:.3} age={}s target={} key={}",
            hit.score, hit.age_secs, hit.target, hit.cache_key
        );
        println!("  {}", hit.snippet);
    }

    Ok(0)
}

fn run_store_list(args: StoreListArgs) -> Result<i32> {
    let cache = load_cache(&args.root)?;
    let entries = cache.list_entries(
        &ContextStoreListFilter {
            target: args.target,
            contains_query: args.query,
            created_after_unix: args.created_after,
            created_before_unix: args.created_before,
        },
        &ContextStorePaging {
            offset: args.offset,
            limit: args.limit,
        },
    );

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.list.v1",
                "entries": entries,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    if entries.is_empty() {
        println!("(empty context store)");
        return Ok(0);
    }

    for entry in entries {
        println!(
            "- key={} target={} age={}s packets={}",
            entry.cache_key, entry.target, entry.age_secs, entry.packet_count
        );
    }

    Ok(0)
}

fn run_store_get(args: StoreGetArgs) -> Result<i32> {
    let cache = load_cache(&args.root)?;
    let Some(detail) = cache.get_entry(&args.key) else {
        anyhow::bail!("cache entry not found for key '{}'", args.key);
    };

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.get.v1",
                "entry": detail,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    println!(
        "key={} target={} age={}s packets={}",
        detail.entry.cache_key,
        detail.entry.target,
        detail.age_secs,
        detail.entry.packets.len()
    );

    Ok(0)
}

fn run_store_prune(args: StorePruneArgs) -> Result<i32> {
    if !args.all && args.ttl_secs.is_none() {
        anyhow::bail!("set --all or --ttl-secs for prune");
    }

    let config = PersistConfig::new(PathBuf::from(&args.root));
    let mut cache = PacketCache::load_from_disk(&config);
    let report = cache.prune(ContextStorePruneRequest {
        all: args.all,
        ttl_secs: args.ttl_secs,
    });
    cache
        .save_to_disk(&config)
        .with_context(|| format!("failed to save context store at '{}'", args.root))?;

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.prune.v1",
                "report": report,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    println!(
        "pruned={} remaining={} manual={} expired={}",
        report.removed, report.remaining, report.reasons.manual_prune, report.reasons.expired_ttl
    );
    Ok(0)
}

fn run_store_stats(args: StoreStatsArgs) -> Result<i32> {
    let cache = load_cache(&args.root)?;
    let stats = cache.stats();

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.stats.v1",
                "stats": stats,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    println!(
        "entries={} oldest={:?} newest={:?}",
        stats.entries, stats.oldest_created_at_unix, stats.newest_created_at_unix
    );
    println!(
        "evictions: expired={} manual={} version_mismatch={} corrupt_load={}",
        stats.evictions.expired_ttl,
        stats.evictions.manual_prune,
        stats.evictions.version_mismatch,
        stats.evictions.corrupt_load_recovery
    );

    Ok(0)
}

fn load_cache(root: &str) -> Result<PacketCache> {
    let root_path = PathBuf::from(root);
    let config = PersistConfig::new(root_path.clone());
    Ok(PacketCache::load_from_disk(&config))
}

fn emit_json(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }

    context_kernel_core::Kernel::with_v1_reducers()
}

fn current_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
