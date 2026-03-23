use std::path::Path;

use anyhow::Result;
use context_memory_core::{RecallOptions, RecallScope as MemoryRecallScope};
use serde_json::json;

use crate::cmd_context::{current_unix, emit_json, load_cache, RecallArgs};

pub fn run_recall(args: RecallArgs) -> Result<i32> {
    let cache = load_cache(&args.root)?;
    let now = current_unix();
    let since_default = now.saturating_sub(86_400);
    let scope = args.scope.map(Into::into).unwrap_or_else(|| {
        if args.task_id.is_some() {
            MemoryRecallScope::TaskFirst
        } else {
            MemoryRecallScope::Global
        }
    });
    let hits = cache.recall(
        &args.query,
        &RecallOptions {
            limit: args.limit,
            since_unix: args.since.or(Some(since_default)),
            until_unix: args.until,
            target: args.target,
            task_id: args.task_id,
            scope,
            packet_types: args.packet_types,
            path_filters: args.path_filters,
            symbol_filters: args.symbol_filters,
            mode: args.mode.into(),
            include_debug: args.include_debug,
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
        if let Some(summary) = hit.summary.as_ref() {
            println!("  {summary}");
            if !hit.snippet.is_empty() && hit.snippet != *summary {
                println!("  {}", hit.snippet);
            }
        } else {
            println!("  {}", hit.snippet);
        }
    }

    Ok(0)
}

pub fn run_recall_remote(args: RecallArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let resolved_root = crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd);
    let since_default = current_unix().saturating_sub(86_400);
    let response = crate::cmd_daemon::execute_context_recall(
        daemon_root,
        packet28_daemon_core::ContextRecallRequest {
            query: args.query.clone(),
            root: resolved_root,
            limit: args.limit,
            since: args.since.or(Some(since_default)),
            until: args.until,
            target: args.target.clone(),
            task_id: args.task_id.clone(),
            scope: args.scope.map(|scope| scope.as_policy_scope().to_string()),
            packet_types: args.packet_types.clone(),
            path_filters: args.path_filters.clone(),
            symbol_filters: args.symbol_filters.clone(),
            mode: Some(args.mode.into()),
            include_debug: args.include_debug,
        },
    )?;

    if args.json {
        emit_json(
            &json!({
                "schema_version": "suite.context.recall.v1",
                "query": response.query,
                "hits": response.hits,
            }),
            args.pretty,
        )?;
        return Ok(0);
    }

    if response.hits.is_empty() {
        println!("(no recall hits)");
        return Ok(0);
    }

    for hit in response.hits {
        println!(
            "- score={:.3} age={}s target={} key={}",
            hit.score, hit.age_secs, hit.target, hit.cache_key
        );
        if let Some(summary) = hit.summary.as_ref() {
            println!("  {summary}");
            if !hit.snippet.is_empty() && hit.snippet != *summary {
                println!("  {}", hit.snippet);
            }
        } else {
            println!("  {}", hit.snippet);
        }
    }

    Ok(0)
}
