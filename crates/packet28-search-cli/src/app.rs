use std::ffi::OsString;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Instant as StdInstant;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use packet28_daemon_protocol::frame::{read_frame, write_frame};
use packet28_daemon_protocol::index::{
    DaemonIndexState, DaemonIndexStatusRequest, DaemonIndexStatusResponse,
};
use packet28_daemon_protocol::message::{
    DaemonRequest, DaemonResponse, Packet28SearchGuardResponse,
    Packet28SearchRequest as DaemonPacket28SearchRequest,
};
use packet28_daemon_protocol::paths::resolve_workspace_root;
use packet28_daemon_protocol::registry::{DaemonRegistryRequestV1, DaemonRegistryResponseV1};
use packet28_reducer_core::{parse_region_for_path, SearchRequest, SearchResult};
use packet28_reducer_core::{SearchEngineStats, SearchGroup, SearchMatch};
use packet28_search_core::{
    guarded_fallback_reason, load_and_guarded_indexed_search, load_and_indexed_search,
    load_runtime, rebuild_full_index, SearchError as IndexSearchError,
};
#[cfg(unix)]
const DAEMON_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "p28")]
#[command(about = "Instant indexed regex search for local codebases")]
struct SearchCli {
    pattern: String,
    #[arg(value_name = "path")]
    paths: Vec<String>,
    #[arg(long = "fixed-strings", visible_alias = "fixed-string")]
    fixed_strings: bool,
    #[arg(long)]
    ignore_case: bool,
    #[arg(long = "word-regexp", visible_alias = "whole-word")]
    word_regexp: bool,
    #[arg(long, value_enum, default_value_t = EngineMode::Auto)]
    engine: EngineMode,
    #[arg(long, value_enum, default_value_t = TransportMode::Auto)]
    transport: TransportMode,
    #[arg(long)]
    compact: bool,
    #[arg(long)]
    stats: bool,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value_t = 20)]
    max_matches_per_file: usize,
    #[arg(long, default_value_t = 200)]
    max_total_matches: usize,
}

#[derive(Parser, Debug)]
#[command(name = "p28 debug")]
#[command(about = "Maintenance commands for the p28 search engine")]
struct DebugCli {
    #[command(subcommand)]
    command: DebugCommand,
}

#[derive(Subcommand, Debug)]
enum DebugCommand {
    Build(BuildArgs),
    Guard(DebugSearchArgs),
    Bench(DebugSearchArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum EngineMode {
    Auto,
    Indexed,
    Legacy,
    Fff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TransportMode {
    Auto,
    Inproc,
    Daemon,
}

#[derive(Args, Debug)]
struct BuildArgs {
    root: PathBuf,
    #[arg(long, default_value_t = true)]
    include_tests: bool,
}

#[derive(Args, Clone, Debug)]
struct CommonSearchArgs {
    #[arg(long = "path")]
    paths: Vec<String>,
    #[arg(long = "fixed-strings", visible_alias = "fixed-string")]
    fixed_strings: bool,
    #[arg(long)]
    ignore_case: bool,
    #[arg(long = "word-regexp", visible_alias = "whole-word")]
    word_regexp: bool,
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

#[derive(Args, Clone, Debug)]
struct DebugSearchArgs {
    root: PathBuf,
    pattern: String,
    #[command(flatten)]
    options: CommonSearchArgs,
}

pub(crate) fn run() -> Result<()> {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_else(|| OsString::from("p28"));
    let collected = args.collect::<Vec<_>>();
    if matches!(
        collected.first().and_then(|arg| arg.to_str()),
        Some("debug")
    ) {
        let cli =
            DebugCli::parse_from(std::iter::once(program).chain(collected.into_iter().skip(1)));
        run_debug(cli.command)
    } else {
        let cli = SearchCli::parse_from(std::iter::once(program).chain(collected));
        run_search(cli)
    }
}

fn run_search(args: SearchCli) -> Result<()> {
    let root = std::env::current_dir().context("failed to resolve current directory")?;
    let request = search_request(
        &args.pattern,
        &args.paths,
        SearchOptions {
            fixed_strings: args.fixed_strings,
            ignore_case: args.ignore_case,
            word_regexp: args.word_regexp,
            max_matches_per_file: args.max_matches_per_file,
            max_total_matches: args.max_total_matches,
        },
    );
    let started = Instant::now();
    let (result, transport) = execute_search(&root, &request, args.engine, args.transport)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    if args.json {
        emit_json(elapsed_ms, transport, result)?;
        return Ok(());
    }
    emit_human_hits(&root, &result, args.compact)?;
    if args.stats {
        emit_stats("p28", transport, elapsed_ms, &result);
    }
    Ok(())
}

fn run_debug(command: DebugCommand) -> Result<()> {
    match command {
        DebugCommand::Build(args) => run_build(args),
        DebugCommand::Guard(args) => run_guard(args),
        DebugCommand::Bench(args) => run_bench(args),
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

fn run_guard(args: DebugSearchArgs) -> Result<()> {
    let request = debug_search_request(&args);
    match args.options.engine {
        EngineMode::Legacy => {
            println!("mode=fallback");
            println!("reason=forced legacy backend");
        }
        EngineMode::Indexed => {
            println!("mode=index");
            println!("reason=forced indexed backend");
        }
        EngineMode::Fff => {
            println!("mode=fff");
            println!("reason=forced fff MCP backend");
        }
        EngineMode::Auto => match guard_reason(&args.root, &request, args.options.transport)? {
            Some(reason) => {
                println!("mode=fallback");
                println!("reason={reason}");
            }
            None => {
                println!("mode=index");
                println!("reason=selective");
            }
        },
    }
    Ok(())
}

fn run_bench(args: DebugSearchArgs) -> Result<()> {
    let request = debug_search_request(&args);
    let guard = match args.options.engine {
        EngineMode::Legacy => Some("forced legacy backend".to_string()),
        EngineMode::Indexed => None,
        EngineMode::Fff => Some("forced fff MCP backend".to_string()),
        EngineMode::Auto => guard_reason(&args.root, &request, args.options.transport)?,
    };

    let packet28_started = Instant::now();
    let (packet28, transport) = execute_search(
        &args.root,
        &request,
        args.options.engine,
        args.options.transport,
    )?;
    let packet28_ms = packet28_started.elapsed().as_secs_f64() * 1000.0;

    let reducer_started = Instant::now();
    let reducer = packet28_reducer_core::search(&args.root, &request)?;
    let reducer_ms = reducer_started.elapsed().as_secs_f64() * 1000.0;

    let packet28_hits = collect_hits(&packet28);
    let reducer_hits = collect_hits(&reducer);

    println!("guard={}", guard.unwrap_or_else(|| "index".to_string()));
    println!(
        "parity={}",
        if packet28_hits == reducer_hits {
            "exact"
        } else {
            "mismatch"
        }
    );
    if packet28_hits != reducer_hits {
        for missing in reducer_hits
            .iter()
            .filter(|hit| !packet28_hits.contains(*hit))
        {
            println!("missing={missing}");
        }
        for extra in packet28_hits
            .iter()
            .filter(|hit| !reducer_hits.contains(*hit))
        {
            println!("extra={extra}");
        }
    }
    print_debug_search_result(
        "p28",
        transport,
        packet28_ms,
        &packet28,
        args.options.compact,
    );
    print_debug_search_result(
        "legacy_rg",
        TransportMode::Inproc,
        reducer_ms,
        &reducer,
        args.options.compact,
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct SearchOptions {
    fixed_strings: bool,
    ignore_case: bool,
    word_regexp: bool,
    max_matches_per_file: usize,
    max_total_matches: usize,
}

fn debug_search_request(args: &DebugSearchArgs) -> SearchRequest {
    search_request(
        &args.pattern,
        &args.options.paths,
        SearchOptions {
            fixed_strings: args.options.fixed_strings,
            ignore_case: args.options.ignore_case,
            word_regexp: args.options.word_regexp,
            max_matches_per_file: args.options.max_matches_per_file,
            max_total_matches: args.options.max_total_matches,
        },
    )
}

fn search_request(pattern: &str, paths: &[String], options: SearchOptions) -> SearchRequest {
    SearchRequest {
        query: pattern.to_string(),
        requested_paths: paths.to_vec(),
        fixed_string: options.fixed_strings,
        case_sensitive: Some(!options.ignore_case),
        whole_word: options.word_regexp,
        context_lines: Some(0),
        max_matches_per_file: Some(options.max_matches_per_file),
        max_total_matches: Some(options.max_total_matches),
    }
}

fn execute_search(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
    transport: TransportMode,
) -> Result<(SearchResult, TransportMode)> {
    if matches!(engine, EngineMode::Fff) {
        return execute_fff_search(root, request).map(|result| (result, TransportMode::Inproc));
    }
    let preferred_fff_error = if matches!(engine, EngineMode::Auto) && fff_auto_prefer_requested() {
        match execute_fff_search(root, request) {
            Ok(result) => return Ok((result, TransportMode::Inproc)),
            Err(error) => Some(format!("fff auto preferred backend failed: {error}")),
        }
    } else {
        None
    };
    let (mut result, transport) = match transport {
        TransportMode::Inproc => execute_search_inproc(root, request, engine)
            .map(|result| (result, TransportMode::Inproc)),
        TransportMode::Daemon => {
            #[cfg(unix)]
            ensure_daemon(&resolve_workspace_root(root))?;
            execute_search_daemon(root, request, engine)
                .map(|result| (result, TransportMode::Daemon))
        }
        TransportMode::Auto => execute_search_auto(root, request, engine),
    }?;
    if let Some(error) = preferred_fff_error {
        annotate_reason(&mut result, error);
    }
    Ok((result, transport))
}

#[cfg(unix)]
fn execute_search_auto(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<(SearchResult, TransportMode)> {
    let workspace_root = resolve_workspace_root(root);
    if let Err(err) = ensure_daemon(&workspace_root) {
        let mut result = execute_search_inproc(root, request, engine)?;
        annotate_reason(&mut result, format!("daemon unavailable: {err}"));
        return Ok((result, TransportMode::Inproc));
    }
    if !matches!(engine, EngineMode::Legacy) {
        let _ = wait_for_daemon_index_ready(&workspace_root, default_index_wait_timeout());
    }
    match execute_search_daemon(root, request, engine) {
        Ok(result) => {
            if let Some(fff_result) = select_fff_for_auto_fallback(root, request, engine, &result) {
                Ok((fff_result, TransportMode::Inproc))
            } else {
                Ok((result, TransportMode::Daemon))
            }
        }
        Err(err) => {
            let mut result =
                execute_search_inproc(root, request, engine).map_err(|fallback_err| {
                    anyhow!(
                    "daemon search failed: {err}; in-process fallback also failed: {fallback_err}"
                )
                })?;
            annotate_reason(&mut result, format!("daemon search failed: {err}"));
            Ok((result, TransportMode::Inproc))
        }
    }
}

#[cfg(not(unix))]
fn execute_search_auto(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<(SearchResult, TransportMode)> {
    execute_search_inproc(root, request, engine).map(|result| (result, TransportMode::Inproc))
}

fn execute_search_inproc(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<SearchResult> {
    match engine {
        EngineMode::Legacy => {
            let mut result = packet28_reducer_core::search(root, request)?;
            annotate_fallback(&mut result, "forced legacy backend".to_string());
            Ok(result)
        }
        EngineMode::Fff => execute_fff_search(root, request),
        EngineMode::Indexed => Ok(load_and_indexed_search(root, request)?),
        EngineMode::Auto => match load_and_guarded_indexed_search(root, request) {
            Ok(result) => Ok(result),
            Err(IndexSearchError::IndexNotReady { reason }) => {
                let mut result = packet28_reducer_core::search(root, request)?;
                annotate_fallback(&mut result, reason);
                Ok(select_fff_for_auto_fallback(root, request, engine, &result).unwrap_or(result))
            }
            Err(error) => Err(error.into()),
        },
    }
}

fn execute_search_daemon(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
) -> Result<SearchResult> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
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
        EngineMode::Fff => execute_fff_search(root, request),
    }
}

fn execute_fff_search(root: &Path, request: &SearchRequest) -> Result<SearchResult> {
    let mut client = FffMcpClient::spawn(root)?;
    let tool_text = client.call_grep(request)?;
    Ok(parse_fff_grep_text(root, request, &tool_text))
}

fn select_fff_for_auto_fallback(
    root: &Path,
    request: &SearchRequest,
    engine: EngineMode,
    result: &SearchResult,
) -> Option<SearchResult> {
    if !matches!(engine, EngineMode::Auto) || !should_try_fff_for_auto_result(result) {
        return None;
    }
    if !fff_auto_enabled() {
        return None;
    }
    if !fff_backend_available() {
        return None;
    }
    let prior_reason = result
        .engine
        .as_ref()
        .and_then(|engine| engine.fallback_reason.clone())
        .unwrap_or_else(|| "Packet28 indexed search selected legacy fallback".to_string());
    match execute_fff_search(root, request) {
        Ok(mut fff_result) => {
            fff_result.diagnostics.push(format!(
                "auto selected fff_mcp after indexed fallback: {prior_reason}"
            ));
            Some(fff_result)
        }
        Err(error) => {
            let mut result = result.clone();
            annotate_reason(&mut result, format!("fff auto backend failed: {error}"));
            Some(result)
        }
    }
}

fn should_try_fff_for_auto_result(result: &SearchResult) -> bool {
    let Some(engine) = result.engine.as_ref() else {
        return false;
    };
    if engine.engine != "legacy_rg" {
        return false;
    }
    let Some(reason) = engine.fallback_reason.as_deref() else {
        return false;
    };
    reason.contains("weak/common literals")
        || reason.contains("candidate set remained too broad")
        || reason.contains("planner could not derive a selective index plan")
}

fn fff_auto_enabled() -> bool {
    !matches!(
        std::env::var("P28_FFF_AUTO").ok().as_deref(),
        Some("0" | "false" | "FALSE" | "off" | "OFF")
    )
}

fn fff_auto_prefer_requested() -> bool {
    matches!(
        std::env::var("P28_FFF_AUTO").ok().as_deref(),
        Some("prefer" | "PREFER" | "first" | "FIRST" | "1" | "true" | "TRUE" | "on" | "ON")
    )
}

fn fff_backend_available() -> bool {
    match std::env::var("P28_FFF_MCP_BIN") {
        Ok(value) => return !value.trim().is_empty(),
        Err(std::env::VarError::NotUnicode(_)) => return false,
        Err(std::env::VarError::NotPresent) => {}
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join("fff-mcp"))
                .find(|path| path.is_file())
        })
        .is_some()
}

struct FffMcpClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl FffMcpClient {
    fn spawn(root: &Path) -> Result<Self> {
        let bin = std::env::var("P28_FFF_MCP_BIN").unwrap_or_else(|_| "fff-mcp".to_string());
        let mut child = ProcessCommand::new(&bin)
            .arg(root)
            .arg("--no-update-check")
            .arg("--no-watch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to launch fff MCP backend '{bin}'; install fff-mcp or set P28_FFF_MCP_BIN"
                )
            })?;
        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("fff MCP backend did not expose stdin"))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("fff MCP backend did not expose stdout"))?,
        );
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<()> {
        let id = self.next_request_id();
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "packet28-p28", "version": env!("CARGO_PKG_VERSION")}
            }
        }))?;
        let _ = self.read_response(id)?;
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))?;
        Ok(())
    }

    fn call_grep(&mut self, request: &SearchRequest) -> Result<String> {
        let query = fff_query(request);
        let max_results = request.max_total_matches.unwrap_or(200).max(1);
        let mut last_text = String::new();
        for attempt in 0..10 {
            let id = self.next_request_id();
            self.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "grep",
                    "arguments": {
                        "query": query,
                        "maxResults": max_results,
                        "output_mode": "content"
                    }
                }
            }))?;
            let response = self.read_response(id)?;
            if let Some(error) = response.get("error") {
                return Err(anyhow!("fff MCP grep failed: {error}"));
            }
            let result = response
                .get("result")
                .ok_or_else(|| anyhow!("fff MCP grep response missing result"))?;
            let mut chunks = Vec::new();
            if let Some(content) = result.get("content").and_then(|value| value.as_array()) {
                for item in content {
                    if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                        chunks.push(text.to_string());
                    }
                }
            }
            last_text = chunks.join("\n");
            if !is_fff_empty_result(&last_text) || attempt == 9 {
                return Ok(last_text);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(last_text)
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_response(&mut self, id: u64) -> Result<serde_json::Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = self.stdout.read_line(&mut line)?;
            if bytes == 0 {
                return Err(anyhow!("fff MCP backend exited before response id {id}"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(trimmed)
                .with_context(|| format!("failed to parse fff MCP JSON line: {trimmed}"))?;
            if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                return Ok(value);
            }
        }
    }
}

impl Drop for FffMcpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fff_query(request: &SearchRequest) -> String {
    if request.requested_paths.is_empty() {
        return request.query.clone();
    }
    let mut parts = request
        .requested_paths
        .iter()
        .map(|path| path.trim_start_matches("./").to_string())
        .collect::<Vec<_>>();
    parts.push(request.query.clone());
    parts.join(" ")
}

fn is_fff_empty_result(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "0 matches." || trimmed.starts_with("0 results ")
}

fn parse_fff_grep_text(root: &Path, request: &SearchRequest, text: &str) -> SearchResult {
    let mut groups = std::collections::BTreeMap::<String, Vec<SearchMatch>>::new();
    let mut current_path: Option<String> = None;
    let max_per_file = request.max_matches_per_file.unwrap_or(usize::MAX);
    let max_total = request.max_total_matches.unwrap_or(usize::MAX);
    let mut total_matches = 0usize;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty()
            || line.starts_with('→')
            || line.starts_with("cursor:")
            || line.contains(" matches shown")
            || line.starts_with("0 ")
        {
            continue;
        }
        if let Some((line_no, body)) = parse_fff_match_line(line) {
            if let Some(path) = current_path.clone() {
                if !fff_path_allowed(&path, &request.requested_paths) {
                    continue;
                }
                let file_matches = groups.entry(path.clone()).or_default();
                if file_matches.len() < max_per_file && total_matches < max_total {
                    file_matches.push(SearchMatch {
                        path,
                        line: line_no,
                        text: body.to_string(),
                    });
                    total_matches += 1;
                }
            }
            continue;
        }
        if root.join(line).exists() || line.contains('/') || line.contains('\\') {
            current_path = Some(line.to_string());
        }
    }

    let match_count = total_group_matches(&groups);
    let paths = groups.keys().cloned().collect::<Vec<_>>();
    let mut regions = Vec::new();
    let mut compact_preview = String::new();
    let groups = groups
        .into_iter()
        .map(|(path, matches)| {
            for item in &matches {
                regions.push(format!("{}:{}-{}", item.path, item.line, item.line));
                compact_preview.push_str(&format!(
                    "{}:{}:{}\n",
                    item.path,
                    item.line,
                    item.text.trim()
                ));
            }
            SearchGroup {
                path,
                match_count: matches.len(),
                displayed_match_count: matches.len(),
                truncated: false,
                matches,
            }
        })
        .collect::<Vec<_>>();
    let returned_match_count = regions.len();
    SearchResult {
        query: request.query.clone(),
        requested_paths: request.requested_paths.clone(),
        resolved_paths: paths.clone(),
        match_count,
        returned_match_count,
        truncated: match_count > returned_match_count,
        paths,
        regions,
        symbols: Vec::new(),
        groups,
        compact_preview,
        diagnostics: vec!["experimental fff MCP backend".to_string()],
        engine: Some(SearchEngineStats {
            engine: "fff_mcp".to_string(),
            ..SearchEngineStats::default()
        }),
    }
}

fn total_group_matches(groups: &std::collections::BTreeMap<String, Vec<SearchMatch>>) -> usize {
    groups.values().map(Vec::len).sum()
}

fn parse_fff_match_line(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let (number, body) = trimmed.split_once(':')?;
    let line_no = number.trim().parse::<usize>().ok()?;
    Some((line_no, body.trim_start()))
}

fn fff_path_allowed(path: &str, requested_paths: &[String]) -> bool {
    if requested_paths.is_empty() {
        return true;
    }
    let normalized = normalize_fff_path(path);
    requested_paths.iter().any(|requested| {
        let requested = normalize_fff_path(requested);
        requested == "."
            || normalized == requested
            || normalized.starts_with(&format!("{requested}/"))
    })
}

fn normalize_fff_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn guard_reason(
    root: &Path,
    request: &SearchRequest,
    transport: TransportMode,
) -> Result<Option<String>> {
    match transport {
        TransportMode::Daemon => {
            #[cfg(unix)]
            {
                ensure_daemon(&resolve_workspace_root(root))?;
            }
            daemon_guard_reason(root, request)
        }
        TransportMode::Inproc => {
            let runtime = load_runtime(root)?;
            Ok(guarded_fallback_reason(root, &runtime, request)?)
        }
        TransportMode::Auto => guard_reason_auto(root, request),
    }
}

#[cfg(unix)]
fn guard_reason_auto(root: &Path, request: &SearchRequest) -> Result<Option<String>> {
    let workspace_root = resolve_workspace_root(root);
    if let Err(err) = ensure_daemon(&workspace_root) {
        let runtime = load_runtime(root)?;
        let reason = guarded_fallback_reason(root, &runtime, request)?;
        return Ok(Some(
            reason.unwrap_or_else(|| format!("daemon unavailable: {err}")),
        ));
    }
    daemon_guard_reason(root, request)
}

#[cfg(not(unix))]
fn guard_reason_auto(root: &Path, request: &SearchRequest) -> Result<Option<String>> {
    let runtime = load_runtime(root)?;
    Ok(guarded_fallback_reason(root, &runtime, request)?)
}

fn daemon_search_request(
    root: &Path,
    workspace_root: &Path,
    request: &SearchRequest,
) -> Result<SearchRequest> {
    if root == workspace_root {
        return Ok(request.clone());
    }
    let relative_root = root
        .strip_prefix(workspace_root)
        .map_err(|_| {
            anyhow!(
                "root '{}' is not inside workspace '{}'",
                root.display(),
                workspace_root.display()
            )
        })?
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
    root: &Path,
    workspace_root: &Path,
    mut result: SearchResult,
) -> Result<SearchResult> {
    if root == workspace_root {
        return Ok(result);
    }
    let prefix = root
        .strip_prefix(workspace_root)
        .map_err(|_| {
            anyhow!(
                "root '{}' is not inside workspace '{}'",
                root.display(),
                workspace_root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let path_prefix = format!("{prefix}/");
    let strip = |value: String| -> String {
        value
            .strip_prefix(&path_prefix)
            .map(ToString::to_string)
            .unwrap_or(value)
    };
    result.resolved_paths = result.resolved_paths.into_iter().map(strip).collect();
    result.paths = result.paths.into_iter().map(strip).collect();
    result.regions = result.regions.into_iter().map(strip).collect();
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
fn daemon_guard_reason(root: &Path, request: &SearchRequest) -> Result<Option<String>> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let workspace_root = resolve_workspace_root(&canonical_root);
    let daemon_request = daemon_search_request(&canonical_root, &workspace_root, request)?;
    let response = send_daemon_guard(&workspace_root, daemon_request)?;
    Ok(response.fallback_reason)
}

#[cfg(not(unix))]
fn daemon_guard_reason(_root: &Path, _request: &SearchRequest) -> Result<Option<String>> {
    Err(anyhow!(
        "daemon transport is only supported on unix platforms"
    ))
}

#[cfg(unix)]
fn send_daemon_search(
    root: &Path,
    request: SearchRequest,
    force_indexed: bool,
) -> Result<SearchResult> {
    let mut stream = packet28_daemon_client::transport::connect(root, DAEMON_SOCKET_TIMEOUT)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(&mut stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(
        &mut writer,
        &DaemonRequest::Packet28Search {
            request: DaemonPacket28SearchRequest {
                request,
                force_indexed,
            },
        },
    )?;
    match read_frame(&mut reader)? {
        DaemonResponse::Packet28Search { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
fn send_daemon_search(
    _root: &Path,
    _request: SearchRequest,
    _force_indexed: bool,
) -> Result<SearchResult> {
    Err(anyhow!(
        "daemon transport is only supported on unix platforms"
    ))
}

#[cfg(unix)]
fn send_daemon_guard(root: &Path, request: SearchRequest) -> Result<Packet28SearchGuardResponse> {
    let mut stream = packet28_daemon_client::transport::connect(root, DAEMON_SOCKET_TIMEOUT)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(&mut stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(
        &mut writer,
        &DaemonRequest::Packet28SearchGuard {
            request: DaemonPacket28SearchRequest {
                request,
                force_indexed: false,
            },
        },
    )?;
    match read_frame(&mut reader)? {
        DaemonResponse::Packet28SearchGuard { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
fn send_daemon_guard(_root: &Path, _request: SearchRequest) -> Result<Packet28SearchGuardResponse> {
    Err(anyhow!(
        "daemon transport is only supported on unix platforms"
    ))
}

#[cfg(unix)]
fn send_daemon_index_status(root: &Path) -> Result<DaemonIndexStatusResponse> {
    let mut stream = packet28_daemon_client::transport::connect(root, DAEMON_SOCKET_TIMEOUT)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(&mut stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(
        &mut writer,
        &DaemonRequest::DaemonIndexStatus {
            request: DaemonIndexStatusRequest {
                root: root.to_string_lossy().to_string(),
            },
        },
    )?;
    match read_frame(&mut reader)? {
        DaemonResponse::DaemonIndexStatus { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(unix)]
fn wait_for_daemon_index_ready(root: &Path, timeout: Duration) -> Result<bool> {
    if timeout.is_zero() {
        return Ok(false);
    }
    let start = StdInstant::now();
    loop {
        let status = send_daemon_index_status(root)?;
        if status.ready
            && status.manifest.status == DaemonIndexState::Ready
            && status.manifest.regex_status.as_deref() == Some("ready")
        {
            return Ok(true);
        }
        if status.manifest.status == DaemonIndexState::Corrupt
            || status.manifest.regex_status.as_deref() == Some("corrupt")
        {
            return Err(anyhow!(
                "{}",
                status
                    .manifest
                    .last_error
                    .or(status.manifest.regex_stale_reason)
                    .unwrap_or_else(|| "regex search index is corrupt".to_string())
            ));
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn default_index_wait_timeout() -> Duration {
    const DEFAULT_WAIT_MS: u64 = 60_000;
    let wait_ms = std::env::var("P28_INDEX_WAIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WAIT_MS);
    Duration::from_millis(wait_ms)
}

fn emit_human_hits(root: &Path, result: &SearchResult, compact: bool) -> Result<()> {
    let mut line_cache = std::collections::BTreeMap::<String, Vec<String>>::new();
    for region in &result.regions {
        let Some((path, line_no)) = parse_single_line_region(region) else {
            continue;
        };
        if compact {
            println!("{path}:{line_no}");
            continue;
        }
        let line = cached_line(root, &path, line_no, &mut line_cache)?;
        println!("{path}:{line_no}:{line}");
    }
    Ok(())
}

fn parse_single_line_region(region: &str) -> Option<(String, usize)> {
    let (path, _) = region.rsplit_once(':')?;
    let (start, end) = parse_region_for_path(region, path)?;
    (start == end).then(|| (path.to_string(), start))
}

fn cached_line(
    root: &Path,
    path: &str,
    line_no: usize,
    line_cache: &mut std::collections::BTreeMap<String, Vec<String>>,
) -> Result<String> {
    if !line_cache.contains_key(path) {
        let content = std::fs::read_to_string(root.join(path))
            .with_context(|| format!("failed to read '{}'", root.join(path).display()))?;
        line_cache.insert(
            path.to_string(),
            content.lines().map(str::to_string).collect(),
        );
    }
    let lines = line_cache
        .get(path)
        .ok_or_else(|| anyhow!("missing cached lines for '{path}'"))?;
    Ok(lines
        .get(line_no.saturating_sub(1))
        .cloned()
        .unwrap_or_default())
}

fn emit_stats(label: &str, transport: TransportMode, elapsed_ms: f64, result: &SearchResult) {
    let engine = result.engine.clone().unwrap_or_default();
    eprintln!(
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
        eprintln!("fallback_reason={reason}");
    }
}

fn emit_json(elapsed_ms: f64, transport: TransportMode, result: SearchResult) -> Result<()> {
    let payload = serde_json::json!({
        "elapsed_ms": elapsed_ms,
        "transport": transport.as_str(),
        "result": result,
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn annotate_reason(result: &mut SearchResult, reason: String) {
    let engine = result.engine.get_or_insert_with(Default::default);
    match engine.fallback_reason.take() {
        Some(existing) if !existing.is_empty() => {
            engine.fallback_reason = Some(format!("{existing}; {reason}"));
        }
        _ => engine.fallback_reason = Some(reason),
    }
}

fn annotate_fallback(result: &mut SearchResult, reason: String) {
    let engine = result.engine.get_or_insert_with(Default::default);
    engine.engine = "legacy_rg".to_string();
    annotate_reason(result, reason);
}

fn compact_token_estimate(result: &SearchResult) -> usize {
    result.compact_preview.len().div_ceil(4)
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

fn print_debug_search_result(
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

#[cfg(unix)]
fn ensure_daemon(root: &Path) -> Result<()> {
    use packet28_daemon_client::runtime_discovery::read_runtime_info_if_present;
    use packet28_daemon_client::transport::workspace_root_matches;

    let root = resolve_workspace_root(root);
    // A starting daemon accepts connections but answers only once ready, so
    // `packet28d start` awaits it under its startup-readiness deadline rather
    // than this fast path's socket timeout. Metadata naming another workspace
    // is never contacted; `packet28d start` fails closed on it unless this
    // workspace's authority was released. Read failures are left to
    // `packet28d start`, which fails closed on them.
    let fast_path = match read_runtime_info_if_present(&root) {
        Ok(Some(runtime)) => {
            runtime.ready_at_unix.is_some()
                && workspace_root_matches(&root, &runtime.workspace_root)
        }
        Ok(None) | Err(_) => true,
    };
    if fast_path && daemon_status_existing(&root).is_ok() {
        return Ok(());
    }
    // An unreachable endpoint does not mean the previous daemon has exited: a
    // stopping daemon withdraws its endpoint before it finishes persistence and
    // cleanup. Stale-file cleanup and spawn need the daemon startup and
    // instance leases, which p28 does not link, so `packet28d start` performs
    // them under the same authority as the Packet28 CLI.
    start_daemon(&root)?;
    wait_for_daemon(&root, Duration::from_secs(10))
}

/// Runs the lease-guarded `packet28d start` bootstrap for `root`.
///
/// It returns once a daemon answers status, or fails without touching the
/// runtime files of a daemon that has not released workspace authority in
/// time. A daemon that is still starting gets the longer startup-readiness
/// allowance and is left running if it misses it.
#[cfg(unix)]
fn start_daemon(root: &Path) -> Result<()> {
    let binary = packet28d_binary()?;
    let output = Command::new(binary)
        .arg("start")
        .arg("--root")
        .arg(root)
        .stdin(Stdio::null())
        .output()
        .context("failed to run packet28d start")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!(
        "packet28d start failed with {}: {}",
        output.status,
        stderr.trim()
    ))
}

/// Confirms that the daemon selected by `packet28d start` answers status for
/// `root` with its published pid.
///
/// Each status request is bounded by the time remaining before `timeout`.
/// Unauthentic runtime discovery, or metadata naming another workspace, fails
/// immediately.
#[cfg(unix)]
fn wait_for_daemon(root: &Path, timeout: Duration) -> Result<()> {
    use packet28_daemon_client::runtime_discovery::read_runtime_info_if_present;
    use packet28_daemon_client::transport::{
        discover_endpoint, request_status_v1, verify_runtime_workspace, workspace_root_matches,
        DaemonEndpoint,
    };

    let deadline = StdInstant::now() + timeout;
    let mut last_error = None;
    while StdInstant::now() < deadline {
        let runtime = read_runtime_info_if_present(root)?;
        let endpoint = match &runtime {
            Some(runtime) => {
                verify_runtime_workspace(root, runtime)?;
                DaemonEndpoint::from_runtime(root, runtime)?
            }
            None => discover_endpoint(root)?,
        };
        match request_status_v1(&endpoint, deadline) {
            Ok(status)
                if workspace_root_matches(root, &status.workspace_root)
                    && runtime
                        .as_ref()
                        .is_none_or(|runtime| runtime.pid == status.pid) =>
            {
                return Ok(());
            }
            Ok(status) => {
                last_error = Some(format!(
                    "status identity pid {} root '{}' does not match workspace '{}'",
                    status.pid,
                    status.workspace_root,
                    root.display()
                ));
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(10));
    }
    let last_error = last_error.unwrap_or_else(|| "none".to_string());
    if let Ok(Some(runtime)) =
        packet28_daemon_client::runtime_discovery::read_runtime_info_if_present(root)
    {
        return Err(anyhow!(
            "packet28d did not answer status; runtime file exists for pid {} at {} (log: {}; \
             last probe: {last_error})",
            runtime.pid,
            runtime.socket_path,
            runtime.log_path
        ));
    }
    Err(anyhow!(
        "packet28d did not answer status (last probe: {last_error})"
    ))
}

/// Confirms that an existing daemon answers status for `root`.
#[cfg(unix)]
fn daemon_status_existing(root: &Path) -> Result<()> {
    let serves_root = |workspace_root: &str| {
        if packet28_daemon_client::transport::workspace_root_matches(root, workspace_root) {
            Ok(())
        } else {
            Err(anyhow!(
                "daemon status names workspace '{workspace_root}', not '{}'",
                root.display()
            ))
        }
    };
    match send_registry_request_existing_daemon(root, &DaemonRegistryRequestV1::Status) {
        Ok(DaemonRegistryResponseV1::Status { status }) => serves_root(&status.workspace_root),
        Ok(DaemonRegistryResponseV1::Error { message })
            if daemon_error_indicates_protocol_mismatch(&message) =>
        {
            match send_request_existing_daemon(root, &DaemonRequest::Status) {
                Ok(DaemonResponse::Status { status }) => serves_root(&status.workspace_root),
                Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
                Ok(other) => Err(anyhow!(
                    "unexpected legacy daemon status response: {other:?}"
                )),
                Err(error) => Err(error),
            }
        }
        Ok(DaemonRegistryResponseV1::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!(
            "unexpected daemon registry status response: {other:?}"
        )),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn daemon_error_indicates_protocol_mismatch(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unknown variant") && lower.contains("expected one of")
}

#[cfg(unix)]
fn send_registry_request_existing_daemon(
    root: &Path,
    request: &DaemonRegistryRequestV1,
) -> Result<DaemonRegistryResponseV1> {
    let stream = packet28_daemon_client::transport::connect(root, DAEMON_SOCKET_TIMEOUT)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(&mut writer, request)?;
    Ok(read_frame(&mut reader)?)
}

#[cfg(unix)]
fn send_request_existing_daemon(root: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let stream = packet28_daemon_client::transport::connect(root, DAEMON_SOCKET_TIMEOUT)?;
    let reader_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(reader_stream);
    write_frame(&mut writer, request)?;
    Ok(read_frame(&mut reader)?)
}

#[cfg(unix)]
fn packet28d_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_packet28d") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let parent = current
        .parent()
        .ok_or_else(|| anyhow!("missing executable parent"))?;
    for candidate in [
        parent.join("packet28d"),
        parent
            .parent()
            .map(|dir| dir.join("packet28d"))
            .unwrap_or_default(),
    ] {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "could not locate packet28d next to '{}'",
        current.display()
    ))
}
