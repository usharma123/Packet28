use anyhow::{bail, Result};
use clap::Args;
use serde_json::json;
use std::path::Path;

#[derive(Args, Clone)]
pub struct QueryArgs {
    /// Repository root path
    #[arg(long, default_value = ".")]
    pub repo_root: String,

    /// Symbol name to query
    #[arg(long, conflicts_with = "pattern")]
    pub symbol: Option<String>,

    /// Structural pattern query
    #[arg(long, conflicts_with = "symbol")]
    pub pattern: Option<String>,

    /// Query language for pattern mode
    #[arg(long, requires = "pattern")]
    pub lang: Option<String>,

    /// Optional tree-sitter selector or comma-separated selectors
    #[arg(long, requires = "pattern")]
    pub selector: Option<String>,

    /// Maximum number of matches
    #[arg(long, default_value_t = 10)]
    pub max_results: usize,

    /// Include test files
    #[arg(long)]
    pub include_tests: bool,

    /// Require exact symbol match
    #[arg(long, conflicts_with = "pattern")]
    pub exact: bool,

    /// Print only file paths with matches
    #[arg(long)]
    pub files_with_matches: bool,

    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    pub legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,
}

pub fn run(args: QueryArgs) -> Result<i32> {
    if args.symbol.is_none() && args.pattern.is_none() {
        bail!("either --symbol or --pattern is required");
    }
    if args.pattern.is_some() && args.lang.is_none() {
        bail!("--lang is required with --pattern");
    }

    let input = mapy_core::RepoQueryRequest {
        repo_root: args.repo_root.clone(),
        symbol_query: args.symbol.clone().unwrap_or_default(),
        pattern_query: args.pattern.clone().unwrap_or_default(),
        language: args.lang.clone().unwrap_or_default(),
        selector: args.selector.clone().unwrap_or_default(),
        max_results: args.max_results,
        include_tests: args.include_tests,
        exact: args.exact,
        files_only: args.files_with_matches,
    };
    let envelope = mapy_core::build_repo_query(input)?;

    if let Some(profile) = args.json.map(suite_packet_core::JsonProfile::from) {
        if args.legacy_json {
            crate::cmd_common::emit_json(
                &json!({
                    "schema_version": "suite.map.query.v1",
                    "packet": envelope,
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_MAP_QUERY,
                &envelope,
                profile,
                args.pretty,
                Path::new(&args.repo_root),
                None,
            )?;
        }
        return Ok(0);
    }

    print_text_results(&envelope, args.files_with_matches);
    Ok(if envelope.payload.matches.is_empty() {
        1
    } else {
        0
    })
}

pub fn run_remote(args: QueryArgs, _daemon_root: &Path) -> Result<i32> {
    run(args)
}

fn print_text_results(
    envelope: &suite_packet_core::EnvelopeV1<mapy_core::RepoQueryPayload>,
    files_only: bool,
) {
    if files_only {
        for matched in &envelope.payload.matches {
            if let Some(file) = envelope.files.get(matched.file_idx) {
                println!("{}", file.path);
            }
        }
        return;
    }

    for matched in &envelope.payload.matches {
        let Some(file) = envelope.files.get(matched.file_idx) else {
            continue;
        };
        let Some(symbol) = envelope.symbols.get(matched.symbol_idx) else {
            continue;
        };
        let kind = symbol.kind.as_deref().unwrap_or("symbol");
        println!(
            "{}:{}:{}:{}:{:.3}",
            file.path, matched.line, kind, symbol.name, matched.score
        );
    }
}
