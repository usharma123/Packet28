use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use context_memory_core::{
    ContextStoreListFilter, ContextStorePaging, ContextStorePruneRequest, PacketCache,
    PersistConfig,
};
use serde_json::json;

use crate::cmd_context::{
    emit_json, load_cache, StoreArgs, StoreCommands, StoreGetArgs, StoreListArgs, StorePruneArgs,
    StoreStatsArgs,
};

pub(crate) fn run_store(args: StoreArgs) -> Result<i32> {
    match args.command {
        StoreCommands::List(args) => run_store_list(args),
        StoreCommands::Get(args) => run_store_get(args),
        StoreCommands::Prune(args) => run_store_prune(args),
        StoreCommands::Stats(args) => run_store_stats(args),
    }
}

pub(crate) fn run_store_remote(args: StoreArgs, daemon_root: &Path) -> Result<i32> {
    match args.command {
        StoreCommands::List(args) => run_store_list_remote(args, daemon_root),
        StoreCommands::Get(args) => run_store_get_remote(args, daemon_root),
        StoreCommands::Prune(args) => run_store_prune_remote(args, daemon_root),
        StoreCommands::Stats(args) => run_store_stats_remote(args, daemon_root),
    }
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

fn run_store_list_remote(args: StoreListArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let response = crate::cmd_daemon::execute_context_store_list(
        daemon_root,
        packet28_daemon_core::ContextStoreListRequest {
            root: crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd),
            target: args.target.clone(),
            query: args.query.clone(),
            created_after: args.created_after,
            created_before: args.created_before,
            offset: args.offset,
            limit: args.limit,
        },
    )?;

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.list.v1",
                "entries": response.entries,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    if response.entries.is_empty() {
        println!("(empty context store)");
        return Ok(0);
    }

    for entry in response.entries {
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

fn run_store_get_remote(args: StoreGetArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let response = crate::cmd_daemon::execute_context_store_get(
        daemon_root,
        packet28_daemon_core::ContextStoreGetRequest {
            root: crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd),
            key: args.key.clone(),
        },
    )?;
    let Some(detail) = response.entry else {
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

fn run_store_prune_remote(args: StorePruneArgs, daemon_root: &Path) -> Result<i32> {
    if !args.all && args.ttl_secs.is_none() {
        anyhow::bail!("set --all or --ttl-secs for prune");
    }
    let cwd = crate::cmd_common::caller_cwd()?;

    let response = crate::cmd_daemon::execute_context_store_prune(
        daemon_root,
        packet28_daemon_core::ContextStorePruneDaemonRequest {
            root: crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd),
            all: args.all,
            ttl_secs: args.ttl_secs,
        },
    )?;

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.prune.v1",
                "report": response.report,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    println!(
        "pruned={} remaining={} manual={} expired={}",
        response.report.removed,
        response.report.remaining,
        response.report.reasons.manual_prune,
        response.report.reasons.expired_ttl
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

fn run_store_stats_remote(args: StoreStatsArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let response = crate::cmd_daemon::execute_context_store_stats(
        daemon_root,
        packet28_daemon_core::ContextStoreStatsRequest {
            root: crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd),
        },
    )?;

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.store.stats.v1",
                "stats": response.stats,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    println!(
        "entries={} oldest={:?} newest={:?}",
        response.stats.entries,
        response.stats.oldest_created_at_unix,
        response.stats.newest_created_at_unix
    );
    println!(
        "evictions: expired={} manual={} version_mismatch={} corrupt_load={}",
        response.stats.evictions.expired_ttl,
        response.stats.evictions.manual_prune,
        response.stats.evictions.version_mismatch,
        response.stats.evictions.corrupt_load_recovery
    );

    Ok(0)
}
