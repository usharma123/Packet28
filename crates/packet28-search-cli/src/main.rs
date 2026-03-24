use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use packet28_reducer_core::SearchRequest;
use packet28_search_core::{
    guarded_fallback_reason, indexed_search, load_runtime, rebuild_full_index,
};

#[derive(Parser)]
#[command(name = "packet28-search")]
#[command(about = "Standalone CLI for the Packet28 indexed regex search engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build(BuildArgs),
    Query(QueryArgs),
    Guard(QueryArgs),
    Bench(QueryArgs),
}

#[derive(Args)]
struct BuildArgs {
    root: PathBuf,
    #[arg(long, default_value_t = true)]
    include_tests: bool,
}

#[derive(Args, Clone)]
struct QueryArgs {
    root: PathBuf,
    pattern: String,
    #[arg(long = "path")]
    paths: Vec<String>,
    #[arg(long)]
    fixed_string: bool,
    #[arg(long)]
    ignore_case: bool,
    #[arg(long)]
    whole_word: bool,
    #[arg(long, default_value_t = 20)]
    max_matches_per_file: usize,
    #[arg(long, default_value_t = 200)]
    max_total_matches: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build(args) => run_build(args),
        Command::Query(args) => run_query(args),
        Command::Guard(args) => run_guard(args),
        Command::Bench(args) => run_bench(args),
    }
}

fn run_build(args: BuildArgs) -> Result<()> {
    let started = Instant::now();
    let runtime = rebuild_full_index(&args.root, args.include_tests)?;
    println!(
        "build_ms={:.3} generation={} files={}",
        started.elapsed().as_secs_f64() * 1000.0,
        runtime.manifest.generation,
        runtime.manifest.indexed_files
    );
    Ok(())
}

fn run_query(args: QueryArgs) -> Result<()> {
    let runtime = load_runtime(&args.root)?;
    let request = search_request(&args);
    let started = Instant::now();
    let result = indexed_search(&args.root, &runtime, &request)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    print_search_result("indexed", elapsed_ms, &result);
    Ok(())
}

fn run_guard(args: QueryArgs) -> Result<()> {
    let runtime = load_runtime(&args.root)?;
    let request = search_request(&args);
    let fallback = guarded_fallback_reason(&args.root, &runtime, &request)?;
    match fallback {
        Some(reason) => {
            println!("mode=fallback");
            println!("reason={reason}");
        }
        None => {
            println!("mode=index");
            println!("reason=selective");
        }
    }
    Ok(())
}

fn run_bench(args: QueryArgs) -> Result<()> {
    let runtime = load_runtime(&args.root)?;
    let request = search_request(&args);

    let guard = guarded_fallback_reason(&args.root, &runtime, &request)?;

    let indexed_started = Instant::now();
    let indexed = indexed_search(&args.root, &runtime, &request)?;
    let indexed_ms = indexed_started.elapsed().as_secs_f64() * 1000.0;

    let reducer_started = Instant::now();
    let reducer = packet28_reducer_core::search(&args.root, &request)?;
    let reducer_ms = reducer_started.elapsed().as_secs_f64() * 1000.0;

    println!("guard={}", guard.unwrap_or_else(|| "index".to_string()));
    print_search_result("indexed", indexed_ms, &indexed);
    print_search_result("legacy_rg", reducer_ms, &reducer);
    Ok(())
}

fn search_request(args: &QueryArgs) -> SearchRequest {
    SearchRequest {
        query: args.pattern.clone(),
        requested_paths: args.paths.clone(),
        fixed_string: args.fixed_string,
        case_sensitive: Some(!args.ignore_case),
        whole_word: args.whole_word,
        context_lines: Some(0),
        max_matches_per_file: Some(args.max_matches_per_file),
        max_total_matches: Some(args.max_total_matches),
    }
}

fn print_search_result(label: &str, elapsed_ms: f64, result: &packet28_reducer_core::SearchResult) {
    let engine = result.engine.clone().unwrap_or_default();
    println!(
        "{label}_ms={elapsed_ms:.3} matches={} files={} candidates={} verified={} lookups={} postings_bytes={}",
        result.match_count,
        result.paths.len(),
        engine.candidates_examined,
        engine.verified_files,
        engine.index_lookups,
        engine.postings_bytes_read
    );
    for group in result.groups.iter().take(3) {
        if let Some(first) = group.matches.first() {
            println!(
                "sample={}#L{} {}",
                group.path,
                first.line,
                first.text.trim()
            );
        }
    }
}

#[allow(dead_code)]
fn ensure_root_exists(root: &PathBuf) -> Result<()> {
    if root.exists() {
        Ok(())
    } else {
        Err(anyhow!("root path '{}' does not exist", root.display()))
    }
}
