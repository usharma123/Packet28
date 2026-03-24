use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use packet28_daemon_core::{
    read_socket_message, resolve_workspace_root, socket_path, write_socket_message, DaemonRequest,
    DaemonResponse, Packet28SearchGuardResponse, Packet28SearchRequest as DaemonPacket28SearchRequest,
};
use packet28_reducer_core::{SearchRequest, SearchResult};
use packet28_search_core::{
    guarded_fallback_reason, indexed_search, load_runtime, rebuild_full_index,
};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum EngineMode {
    Auto,
    Indexed,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TransportMode {
    Auto,
    Inproc,
    Daemon,
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
    #[arg(long, value_enum, default_value_t = EngineMode::Auto)]
    engine: EngineMode,
    #[arg(long, value_enum, default_value_t = TransportMode::Auto)]
    transport: TransportMode,
    #[arg(long)]
    compact: bool,
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
    let request = search_request(&args);
    let started = Instant::now();
    let (result, transport) = execute_search(&args.root, &request, args.engine, args.transport)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    print_search_result("packet28", transport, elapsed_ms, &result, args.compact);
    Ok(())
}

fn run_guard(args: QueryArgs) -> Result<()> {
    let request = search_request(&args);
    match args.engine {
        EngineMode::Legacy => {
            println!("mode=fallback");
            println!("reason=forced legacy backend");
        }
        EngineMode::Indexed => {
            println!("mode=index");
            println!("reason=forced indexed backend");
        }
        EngineMode::Auto => {
            match guard_reason(&args.root, &request, args.transport)? {
                Some(reason) => {
                    println!("mode=fallback");
                    println!("reason={reason}");
                }
                None => {
                    println!("mode=index");
                    println!("reason=selective");
                }
            }
        }
    }
    Ok(())
}

fn run_bench(args: QueryArgs) -> Result<()> {
    let request = search_request(&args);
    let guard = match args.engine {
        EngineMode::Legacy => Some("forced legacy backend".to_string()),
        EngineMode::Indexed => None,
        EngineMode::Auto => guard_reason(&args.root, &request, args.transport)?,
    };

    let packet28_started = Instant::now();
    let (packet28, transport) = execute_search(&args.root, &request, args.engine, args.transport)?;
    let packet28_ms = packet28_started.elapsed().as_secs_f64() * 1000.0;

    let reducer_started = Instant::now();
    let reducer = packet28_reducer_core::search(&args.root, &request)?;
    let reducer_ms = reducer_started.elapsed().as_secs_f64() * 1000.0;

    let packet28_hits = collect_hits(&packet28);
    let reducer_hits = collect_hits(&reducer);

    println!("guard={}", guard.clone().unwrap_or_else(|| "index".to_string()));
    println!("parity={}", if packet28_hits == reducer_hits { "exact" } else { "mismatch" });
    if packet28_hits != reducer_hits {
        for missing in reducer_hits.iter().filter(|hit| !packet28_hits.contains(*hit)) {
            println!("missing={missing}");
        }
        for extra in packet28_hits.iter().filter(|hit| !reducer_hits.contains(*hit)) {
            println!("extra={extra}");
        }
    }
    print_search_result("packet28", transport, packet28_ms, &packet28, args.compact);
    print_search_result("legacy_rg", TransportMode::Inproc, reducer_ms, &reducer, args.compact);
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

fn execute_search(
    root: &PathBuf,
    request: &SearchRequest,
    engine: EngineMode,
    transport: TransportMode,
) -> Result<(SearchResult, TransportMode)> {
    match transport {
        TransportMode::Inproc => execute_search_inproc(root, request, engine).map(|result| (result, TransportMode::Inproc)),
        TransportMode::Daemon => execute_search_daemon(root, request, engine).map(|result| (result, TransportMode::Daemon)),
        TransportMode::Auto => {
            let workspace_root = resolve_workspace_root(root);
            if daemon_available(&workspace_root) {
                execute_search_daemon(root, request, engine).map(|result| (result, TransportMode::Daemon))
            } else {
                execute_search_inproc(root, request, engine).map(|result| (result, TransportMode::Inproc))
            }
        }
    }
}

fn execute_search_inproc(
    root: &PathBuf,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<SearchResult> {
    match engine {
        EngineMode::Legacy => {
            let mut result = packet28_reducer_core::search(root, request)?;
            annotate_fallback(&mut result, "forced legacy backend".to_string());
            Ok(result)
        }
        EngineMode::Indexed => {
            let runtime = load_runtime(root)?;
            indexed_search(root, &runtime, request)
        }
        EngineMode::Auto => match load_runtime(root) {
            Ok(runtime) => match guarded_fallback_reason(root, &runtime, request)? {
                Some(reason) => {
                    let mut result = packet28_reducer_core::search(root, request)?;
                    annotate_fallback(&mut result, reason);
                    Ok(result)
                }
                None => indexed_search(root, &runtime, request),
            },
            Err(err) => {
                let mut result = packet28_reducer_core::search(root, request)?;
                annotate_fallback(&mut result, format!("regex index load failed: {err}"));
                Ok(result)
            }
        },
    }
}

fn execute_search_daemon(
    root: &PathBuf,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<SearchResult> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let workspace_root = resolve_workspace_root(&canonical_root);
    let daemon_request = daemon_search_request(&canonical_root, &workspace_root, request)?;
    match engine {
        EngineMode::Indexed | EngineMode::Auto => {
            let result = send_daemon_search(
                &workspace_root,
                daemon_request,
                matches!(engine, EngineMode::Indexed),
            )?;
            normalize_daemon_result(&canonical_root, &workspace_root, result)
        }
        EngineMode::Legacy => {
            let mut result = packet28_reducer_core::search(root, request)?;
            annotate_fallback(&mut result, "forced legacy backend".to_string());
            Ok(result)
        }
    }
}

fn guard_reason(
    root: &PathBuf,
    request: &SearchRequest,
    transport: TransportMode,
) -> Result<Option<String>> {
    match transport {
        TransportMode::Daemon => daemon_guard_reason(root, request),
        TransportMode::Inproc => {
            let runtime = load_runtime(root)?;
            guarded_fallback_reason(root, &runtime, request)
        }
        TransportMode::Auto => {
            let workspace_root = resolve_workspace_root(root);
            if daemon_available(&workspace_root) {
                daemon_guard_reason(root, request)
            } else {
                let runtime = load_runtime(root)?;
                guarded_fallback_reason(root, &runtime, request)
            }
        }
    }
}

fn daemon_search_request(
    root: &PathBuf,
    workspace_root: &PathBuf,
    request: &SearchRequest,
) -> Result<SearchRequest> {
    if root == workspace_root {
        return Ok(request.clone());
    }
    let relative_root = root
        .strip_prefix(workspace_root)
        .map_err(|_| anyhow!("root '{}' is not inside workspace '{}'", root.display(), workspace_root.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let mut adjusted = request.clone();
    adjusted.requested_paths = if request.requested_paths.is_empty() {
        vec![relative_root]
    } else {
        request
            .requested_paths
            .iter()
            .map(|path| format!("{}/{}", relative_root, path.trim_start_matches("./")))
            .collect()
    };
    Ok(adjusted)
}

fn normalize_daemon_result(
    root: &PathBuf,
    workspace_root: &PathBuf,
    mut result: SearchResult,
) -> Result<SearchResult> {
    if root == workspace_root {
        return Ok(result);
    }
    let prefix = root
        .strip_prefix(workspace_root)
        .map_err(|_| anyhow!("root '{}' is not inside workspace '{}'", root.display(), workspace_root.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let path_prefix = format!("{prefix}/");
    let strip = |value: String| -> String {
        value.strip_prefix(&path_prefix)
            .map(ToString::to_string)
            .unwrap_or(value)
    };
    result.resolved_paths = result.resolved_paths.into_iter().map(strip).collect();
    result.paths = result.paths.into_iter().map(strip).collect();
    result.regions = result
        .regions
        .into_iter()
        .map(|region| strip(region))
        .collect();
    for group in &mut result.groups {
        group.path = strip(group.path.clone());
        for item in &mut group.matches {
            item.path = strip(item.path.clone());
        }
    }
    result.compact_preview = result.compact_preview.replace(&path_prefix, "");
    Ok(result)
}

#[cfg(unix)]
fn daemon_guard_reason(root: &PathBuf, request: &SearchRequest) -> Result<Option<String>> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let workspace_root = resolve_workspace_root(&canonical_root);
    let daemon_request = daemon_search_request(&canonical_root, &workspace_root, request)?;
    let response = send_daemon_guard(&workspace_root, daemon_request)?;
    Ok(response.fallback_reason)
}

#[cfg(not(unix))]
fn daemon_guard_reason(_root: &PathBuf, _request: &SearchRequest) -> Result<Option<String>> {
    Err(anyhow!("daemon transport is only supported on unix platforms"))
}

#[cfg(unix)]
fn send_daemon_search(
    root: &PathBuf,
    request: SearchRequest,
    force_indexed: bool,
) -> Result<SearchResult> {
    let socket = socket_path(root);
    let stream = UnixStream::connect(&socket)
        .map_err(|err| anyhow!("failed to connect to daemon socket '{}': {err}", socket.display()))?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_socket_message(
        &mut writer,
        &DaemonRequest::Packet28Search {
            request: DaemonPacket28SearchRequest {
                request,
                force_indexed,
            },
        },
    )?;
    match read_socket_message(&mut reader)? {
        DaemonResponse::Packet28Search { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
fn send_daemon_search(
    _root: &PathBuf,
    _request: SearchRequest,
    _force_indexed: bool,
) -> Result<SearchResult> {
    Err(anyhow!("daemon transport is only supported on unix platforms"))
}

#[cfg(unix)]
fn send_daemon_guard(
    root: &PathBuf,
    request: SearchRequest,
) -> Result<Packet28SearchGuardResponse> {
    let socket = socket_path(root);
    let stream = UnixStream::connect(&socket)
        .map_err(|err| anyhow!("failed to connect to daemon socket '{}': {err}", socket.display()))?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_socket_message(
        &mut writer,
        &DaemonRequest::Packet28SearchGuard {
            request: DaemonPacket28SearchRequest {
                request,
                force_indexed: false,
            },
        },
    )?;
    match read_socket_message(&mut reader)? {
        DaemonResponse::Packet28SearchGuard { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
fn send_daemon_guard(
    _root: &PathBuf,
    _request: SearchRequest,
) -> Result<Packet28SearchGuardResponse> {
    Err(anyhow!("daemon transport is only supported on unix platforms"))
}

#[cfg(unix)]
fn daemon_available(root: &PathBuf) -> bool {
    UnixStream::connect(socket_path(root)).is_ok()
}

#[cfg(not(unix))]
fn daemon_available(_root: &PathBuf) -> bool {
    false
}

fn annotate_fallback(result: &mut SearchResult, reason: String) {
    let engine = result.engine.get_or_insert_with(Default::default);
    engine.engine = "legacy_rg".to_string();
    engine.fallback_reason = Some(reason);
}

fn compact_token_estimate(result: &SearchResult) -> usize {
    result.compact_preview.as_bytes().len().div_ceil(4)
}

fn collect_hits(result: &SearchResult) -> Vec<String> {
    let mut hits = Vec::new();
    for group in &result.groups {
        for item in &group.matches {
            hits.push(format!("{}:{}", item.path, item.line));
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

fn print_search_result(
    label: &str,
    transport: TransportMode,
    elapsed_ms: f64,
    result: &SearchResult,
    compact: bool,
) {
    let engine = result.engine.clone().unwrap_or_default();
    println!(
        "{label}_ms={elapsed_ms:.3} transport={} backend={} matches={} files={} returned={} compact_tokens={} candidates={} verified={} lookups={} postings_bytes={}",
        transport.as_str(),
        engine.engine,
        result.match_count,
        result.paths.len(),
        result.returned_match_count,
        compact_token_estimate(result),
        engine.candidates_examined,
        engine.verified_files,
        engine.index_lookups,
        engine.postings_bytes_read
    );
    if let Some(reason) = engine.fallback_reason.as_deref() {
        println!("fallback_reason={reason}");
    }
    for group in &result.groups {
        for item in &group.matches {
            if compact {
                println!("hit={}#L{}", item.path, item.line);
            } else {
                println!("hit={}#L{} {}", item.path, item.line, item.text.trim());
            }
        }
    }
    if !compact {
        for region in &result.regions {
            println!("region={region}");
        }
    }
    println!(
        "compact_preview={}",
        result.compact_preview.replace('\n', "\\n")
    );
}

impl TransportMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Inproc => "inproc",
            Self::Daemon => "daemon",
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
