use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{load_task_registry, task_state_json_path, BrokerGetContextResponse, BrokerWriteOp, BrokerWriteStateRequest};
use serde::Serialize;
use serde_json::{json, Value};

use crate::route_registry::{build_route_rewrite, decide_command_route, NativeToolKind, RouteKind};

#[derive(Args)]
pub struct CompactArgs {
    #[command(subcommand)]
    pub command: CompactCommands,
}

#[derive(Subcommand)]
pub enum CompactCommands {
    Tree(TreeArgs),
    Read(ReadArgs),
    Grep(GrepArgs),
    Json(JsonArgs),
    Env(EnvArgs),
    Deps(DepsArgs),
    Log(LogArgs),
    Summary(SummaryArgs),
    Err(SummaryArgs),
    Test(SummaryArgs),
    Rewrite(RewriteArgs),
    Gain(AnalyticsArgs),
    Discover(AnalyticsArgs),
    Session(SessionArgs),
    FetchRaw(FetchRawArgs),
}

#[derive(Args, Clone)]
pub struct TreeArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, default_value_t = 3)]
    pub max_depth: usize,
    #[arg(long, default_value_t = 200)]
    pub max_entries: usize,
    #[arg(long, default_value_t = false)]
    pub hidden: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
}

#[derive(Args, Clone)]
pub struct ReadArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long)]
    pub line_start: Option<usize>,
    #[arg(long)]
    pub line_end: Option<usize>,
    #[arg(long)]
    pub last: Option<usize>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    pub path: String,
}

#[derive(Args, Clone)]
pub struct GrepArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, default_value_t = false)]
    pub fixed_string: bool,
    #[arg(long, default_value_t = false)]
    pub ignore_case: bool,
    #[arg(long, default_value_t = false)]
    pub whole_word: bool,
    #[arg(long)]
    pub context_lines: Option<usize>,
    #[arg(long)]
    pub max_matches_per_file: Option<usize>,
    #[arg(long)]
    pub max_total_matches: Option<usize>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    pub query: String,
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
}

#[derive(Args, Clone)]
pub struct JsonArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value_t = 64)]
    pub max_items: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    pub path: String,
}

#[derive(Args, Clone)]
pub struct EnvArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub prefix: Option<String>,
    #[arg(long, default_value_t = false)]
    pub show_values: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args, Clone)]
pub struct DepsArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value_t = 80)]
    pub max_items: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(default_value = ".")]
    pub path: String,
}

#[derive(Args, Clone)]
pub struct LogArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value_t = 80)]
    pub max_lines: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    pub path: String,
}

#[derive(Args, Clone)]
#[command(trailing_var_arg = true)]
pub struct SummaryArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(required = true, allow_hyphen_values = true)]
    pub command_argv: Vec<String>,
}

#[derive(Args, Clone)]
#[command(trailing_var_arg = true)]
pub struct RewriteArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long, default_value = ".")]
    pub cwd: String,
    #[arg(long, default_value = "task-compact-preview")]
    pub task_id: String,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(required = true, allow_hyphen_values = true)]
    pub command_argv: Vec<String>,
}

#[derive(Args, Clone)]
pub struct AnalyticsArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args, Clone)]
pub struct SessionArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args, Clone)]
pub struct FetchRawArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task_id: String,
    #[arg(long)]
    pub handle: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Serialize, Default)]
struct GainSummary {
    task_count: usize,
    invocation_count: usize,
    raw_est_tokens: u64,
    reduced_est_tokens: u64,
    saved_est_tokens: u64,
    savings_pct: f64,
    by_route: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct DiscoverItem {
    task_id: String,
    tool_name: String,
    request: String,
    route: String,
    reason: Option<String>,
    raw_est_tokens: u64,
    reduced_est_tokens: u64,
    raw_artifact_available: bool,
}

#[derive(Debug, Serialize)]
struct SessionItem {
    task_id: String,
    running: bool,
    latest_context_version: Option<String>,
    latest_hook_command_kind: Option<String>,
    latest_hook_handoff_reason: Option<String>,
    recent_invocation_count: usize,
    changed_paths_since_checkpoint: usize,
}

pub fn run(args: CompactArgs) -> Result<i32> {
    match args.command {
        CompactCommands::Tree(args) => run_tree(args),
        CompactCommands::Read(args) => run_read(args),
        CompactCommands::Grep(args) => run_grep(args),
        CompactCommands::Json(args) => run_json(args),
        CompactCommands::Env(args) => run_env(args),
        CompactCommands::Deps(args) => run_deps(args),
        CompactCommands::Log(args) => run_log(args),
        CompactCommands::Summary(args) => run_summary(args, "summary"),
        CompactCommands::Err(args) => run_summary(args, "err"),
        CompactCommands::Test(args) => run_summary(args, "test"),
        CompactCommands::Rewrite(args) => run_rewrite(args),
        CompactCommands::Gain(args) => run_gain(args),
        CompactCommands::Discover(args) => run_discover(args),
        CompactCommands::Session(args) => run_session(args),
        CompactCommands::FetchRaw(args) => run_fetch_raw(args),
    }
}

fn run_tree(args: TreeArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let mut rendered = Vec::<String>::new();
    let mut paths = Vec::<String>::new();
    for raw_path in &args.paths {
        let resolved = resolve_repo_path(&root, &cwd, raw_path);
        walk_tree(
            &root,
            &resolved,
            0,
            args.max_depth,
            args.max_entries,
            args.hidden,
            &mut rendered,
            &mut paths,
        )?;
    }
    paths.sort();
    paths.dedup();
    let preview = rendered.join("\n");
    let payload = json!({
        "paths": paths,
        "entry_count": paths.len(),
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.tree",
        "native_tool",
        "tree".to_string(),
        preview.clone(),
        Some(estimate_tokens_for_strings(&paths)),
        Some(estimate_tokens_str(&preview)),
        paths.clone(),
        Vec::new(),
    )?;
    Ok(0)
}

fn run_read(args: ReadArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let resolved_path = resolve_repo_path(&root, &cwd, &args.path);
    let relative = packet28_reducer_core::normalize_capture_path(&root, &resolved_path.display().to_string());
    let text = fs::read_to_string(&resolved_path)
        .with_context(|| format!("failed to read '{}'", resolved_path.display()))?;
    let lines = text.lines().collect::<Vec<_>>();
    let (start, end, rendered_lines) = select_read_lines(&lines, args.line_start, args.line_end, args.last);
    let preview = rendered_lines
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{}|{}", start + idx, line))
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({
        "path": relative,
        "line_start": start,
        "line_end": end,
        "line_count": rendered_lines.len(),
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.read",
        "native_tool",
        format!("read {}", relative),
        preview.clone(),
        Some(estimate_tokens_str(&text)),
        Some(estimate_tokens_str(&preview)),
        vec![relative],
        Vec::new(),
    )?;
    Ok(0)
}

fn run_grep(args: GrepArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let requested_paths = args
        .paths
        .iter()
        .map(|path| resolve_repo_path(&root, &cwd, path))
        .map(|path| packet28_reducer_core::normalize_capture_path(&root, &path.display().to_string()))
        .collect::<Vec<_>>();
    let result = packet28_reducer_core::search(
        &root,
        &packet28_reducer_core::SearchRequest {
            query: args.query.clone(),
            requested_paths,
            fixed_string: args.fixed_string,
            case_sensitive: Some(!args.ignore_case),
            whole_word: args.whole_word,
            context_lines: args.context_lines,
            max_matches_per_file: args.max_matches_per_file,
            max_total_matches: args.max_total_matches,
        },
    )?;
    let preview = result.compact_preview.clone();
    let payload = json!({
        "query": result.query,
        "match_count": result.match_count,
        "paths": result.paths,
        "regions": result.regions,
        "symbols": result.symbols,
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.grep",
        "native_tool",
        format!("grep {}", args.query),
        preview.clone(),
        Some(estimate_tokens_for_value(&payload)),
        Some(estimate_tokens_str(&preview)),
        result.paths,
        result.symbols,
    )?;
    Ok(0)
}

fn run_json(args: JsonArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = crate::cmd_common::caller_cwd()?;
    let path = PathBuf::from(crate::cmd_common::resolve_path_from_cwd(&args.path, &cwd));
    let relative = packet28_reducer_core::normalize_capture_path(&root, &path.display().to_string());
    let raw = fs::read_to_string(&path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse JSON '{}'", path.display()))?;
    let mut lines = vec![format!("JSON {}", relative)];
    describe_json(&value, "$", &mut lines, args.max_items);
    let preview = lines.join("\n");
    let payload = json!({
        "path": relative,
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.json",
        "native_tool",
        format!("json {}", relative),
        preview.clone(),
        Some(estimate_tokens_str(&raw)),
        Some(estimate_tokens_str(&preview)),
        vec![relative],
        Vec::new(),
    )?;
    Ok(0)
}

fn run_env(args: EnvArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let prefix = args.prefix.clone().unwrap_or_default();
    let mut entries = std::env::vars()
        .filter(|(key, _)| prefix.is_empty() || key.starts_with(&prefix))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let preview = entries
        .iter()
        .map(|(key, value)| {
            if args.show_values {
                format!("{key}={value}")
            } else {
                format!("{key}=<redacted:{}>", value.len())
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({
        "prefix": args.prefix,
        "count": entries.len(),
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.env",
        "native_tool",
        if prefix.is_empty() { "env".to_string() } else { format!("env prefix={prefix}") },
        preview.clone(),
        None,
        Some(estimate_tokens_str(&preview)),
        Vec::new(),
        Vec::new(),
    )?;
    Ok(0)
}

fn run_deps(args: DepsArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = crate::cmd_common::caller_cwd()?;
    let start = PathBuf::from(crate::cmd_common::resolve_path_from_cwd(&args.path, &cwd));
    let manifests = collect_dependency_manifests(&start)?;
    let mut lines = Vec::<String>::new();
    for manifest in manifests.iter().take(args.max_items) {
        lines.extend(render_manifest_dependencies(manifest)?);
    }
    if lines.is_empty() {
        lines.push("No dependency manifests found.".to_string());
    }
    let preview = lines.join("\n");
    let payload = json!({
        "manifest_count": manifests.len(),
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.deps",
        "native_tool",
        "deps".to_string(),
        preview.clone(),
        None,
        Some(estimate_tokens_str(&preview)),
        manifests
            .iter()
            .map(|path| packet28_reducer_core::normalize_capture_path(&root, &path.display().to_string()))
            .collect(),
        Vec::new(),
    )?;
    Ok(0)
}

fn run_log(args: LogArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = crate::cmd_common::caller_cwd()?;
    let path = PathBuf::from(crate::cmd_common::resolve_path_from_cwd(&args.path, &cwd));
    let relative = packet28_reducer_core::normalize_capture_path(&root, &path.display().to_string());
    let raw = fs::read_to_string(&path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let lines = raw.lines().rev().take(args.max_lines).collect::<Vec<_>>();
    let mut grouped = Vec::<(String, usize)>::new();
    for line in lines.into_iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((last, count)) = grouped.last_mut() {
            if last == trimmed {
                *count += 1;
                continue;
            }
        }
        grouped.push((trimmed.to_string(), 1));
    }
    let preview = grouped
        .into_iter()
        .map(|(line, count)| {
            if count > 1 {
                format!("[x{count}] {line}")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({
        "path": relative,
        "compact_preview": preview,
    });
    emit_or_print(&payload, &preview, args.json, args.pretty)?;
    record_tool_result(
        &root,
        args.task_id.as_deref(),
        "packet28.compact.log",
        "native_tool",
        format!("log {}", relative),
        preview.clone(),
        Some(estimate_tokens_str(&raw)),
        Some(estimate_tokens_str(&preview)),
        vec![relative],
        Vec::new(),
    )?;
    Ok(0)
}

fn run_summary(args: SummaryArgs, label: &str) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let request = suite_proxy_core::ProxyRunRequest {
        argv: args.command_argv.clone(),
        cwd: Some(cwd.display().to_string()),
        ..suite_proxy_core::ProxyRunRequest::default()
    };
    match suite_proxy_core::run_and_reduce(request) {
        Ok(envelope) => {
            let preview = envelope.payload.highlights.join("\n");
            if args.json {
                crate::cmd_common::emit_json(&serde_json::to_value(&envelope)?, args.pretty)?;
            } else if preview.is_empty() {
                println!("{}", envelope.summary);
            } else {
                println!("{preview}");
            }
            record_tool_result(
                &root,
                args.task_id.as_deref(),
                &format!("packet28.compact.{label}"),
                "proxy_passthrough",
                args.command_argv.join(" "),
                envelope.summary.clone(),
                Some(((envelope.payload.bytes_in as f64) / 4.0).ceil() as u64),
                Some(((envelope.payload.bytes_out as f64) / 4.0).ceil() as u64),
                envelope
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect(),
                Vec::new(),
            )?;
            Ok(if envelope.payload.exit_code == 0 { 0 } else { 1 })
        }
        Err(error) => {
            record_tool_failure(
                &root,
                args.task_id.as_deref(),
                &format!("packet28.compact.{label}"),
                "proxy_passthrough",
                args.command_argv.join(" "),
                error.to_string(),
            )?;
            Err(anyhow!(error.to_string()))
        }
    }
}

fn run_rewrite(args: RewriteArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let command = args.command_argv.join(" ");
    let decision = decide_command_route(&command);
    let rewritten = build_route_rewrite(
        &root,
        &args.task_id,
        args.session_id.as_deref(),
        &args.cwd,
        &decision,
    );
    let native_kind = decision.native_tool.as_ref().map(|tool| match tool.kind {
        NativeToolKind::Tree => "tree",
        NativeToolKind::Read => "read",
        NativeToolKind::Grep => "grep",
        NativeToolKind::Env => "env",
    });
    let payload = json!({
        "command": command,
        "route": match decision.kind {
            RouteKind::ReducerRewrite => "reducer_rewrite",
            RouteKind::NativeTool => "native_tool",
            RouteKind::ProxyPassthrough => "proxy_passthrough",
            RouteKind::RawPassthrough => "raw_passthrough",
        },
        "reason": decision.reason,
        "env_assignments": decision.env_assignments,
        "native_tool": native_kind,
        "rewritten_command": rewritten,
        "reducer_family": decision.reducer_spec.as_ref().map(|spec| spec.family.clone()),
        "reducer_kind": decision
            .reducer_spec
            .as_ref()
            .map(|spec| spec.canonical_kind.clone()),
    });
    emit_or_print(
        &payload,
        payload["rewritten_command"]
            .as_str()
            .unwrap_or("command would not be rewritten"),
        args.json,
        args.pretty,
    )?;
    Ok(0)
}

fn run_gain(args: AnalyticsArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let states = load_task_states(&root, args.task_id.as_deref(), args.limit)?;
    let mut summary = GainSummary::default();
    summary.task_count = states.len();
    for (_, state) in states {
        for invocation in state.recent_tool_invocations {
            summary.invocation_count += 1;
            summary.raw_est_tokens += invocation.raw_est_tokens.unwrap_or(0);
            summary.reduced_est_tokens += invocation.reduced_est_tokens.unwrap_or(0);
            let route = invocation
                .compact_path
                .unwrap_or_else(|| "unknown".to_string());
            *summary.by_route.entry(route).or_insert(0) += 1;
        }
    }
    summary.saved_est_tokens = summary
        .raw_est_tokens
        .saturating_sub(summary.reduced_est_tokens);
    summary.savings_pct = pct_saved(summary.raw_est_tokens, summary.reduced_est_tokens);
    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(summary)?, args.pretty)?;
    } else {
        println!("tasks={}", summary.task_count);
        println!("invocations={}", summary.invocation_count);
        println!("raw_est_tokens={}", summary.raw_est_tokens);
        println!("reduced_est_tokens={}", summary.reduced_est_tokens);
        println!("saved_est_tokens={}", summary.saved_est_tokens);
        println!("savings_pct={:.1}", summary.savings_pct);
        for (route, count) in summary.by_route {
            println!("route.{route}={count}");
        }
    }
    Ok(0)
}

fn run_discover(args: AnalyticsArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let states = load_task_states(&root, args.task_id.as_deref(), args.limit)?;
    let mut items = Vec::<DiscoverItem>::new();
    for (task_id, state) in states {
        for invocation in state.recent_tool_invocations {
            let route = invocation
                .compact_path
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let raw = invocation.raw_est_tokens.unwrap_or(0);
            let reduced = invocation.reduced_est_tokens.unwrap_or(0);
            let missed = route == "raw_passthrough"
                || invocation.passthrough_reason.is_some()
                || (raw > 0 && pct_saved(raw, reduced) < 50.0);
            if missed {
                items.push(DiscoverItem {
                    task_id: task_id.clone(),
                    tool_name: invocation.tool_name,
                    request: invocation
                        .request_summary
                        .unwrap_or_else(|| "no request summary".to_string()),
                    route,
                    reason: invocation.passthrough_reason,
                    raw_est_tokens: raw,
                    reduced_est_tokens: reduced,
                    raw_artifact_available: invocation.raw_artifact_available,
                });
            }
        }
    }
    items.sort_by(|a, b| b.raw_est_tokens.cmp(&a.raw_est_tokens));
    items.truncate(args.limit.max(1));
    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(items)?, args.pretty)?;
    } else if items.is_empty() {
        println!("No missed-savings candidates found.");
    } else {
        for item in items {
            println!(
                "{} {} route={} raw={} reduced={} reason={}",
                item.task_id,
                item.tool_name,
                item.route,
                item.raw_est_tokens,
                item.reduced_est_tokens,
                item.reason.unwrap_or_else(|| "n/a".to_string())
            );
        }
    }
    Ok(0)
}

fn run_session(args: SessionArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let registry = load_task_registry(&root)?;
    let mut sessions = Vec::<SessionItem>::new();
    for (task_id, task) in registry.tasks {
        if args
            .task_id
            .as_deref()
            .is_some_and(|wanted| wanted != task_id.as_str())
        {
            continue;
        }
        let state = load_task_state(&root, &task_id).ok();
        sessions.push(SessionItem {
            task_id: task_id.clone(),
            running: task.running,
            latest_context_version: task.latest_context_version,
            latest_hook_command_kind: task.latest_hook_command_kind,
            latest_hook_handoff_reason: task.latest_hook_handoff_reason,
            recent_invocation_count: state
                .as_ref()
                .map(|state| state.recent_tool_invocations.len())
                .unwrap_or(0),
            changed_paths_since_checkpoint: state
                .as_ref()
                .map(|state| state.changed_paths_since_checkpoint.len())
                .unwrap_or(0),
        });
    }
    sessions.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    if args.json {
        crate::cmd_common::emit_json(&serde_json::to_value(sessions)?, args.pretty)?;
    } else {
        for session in sessions {
            println!(
                "task={} running={} recent_invocations={} changed_paths={} hook_kind={}",
                session.task_id,
                session.running,
                session.recent_invocation_count,
                session.changed_paths_since_checkpoint,
                session.latest_hook_command_kind.unwrap_or_else(|| "n/a".to_string())
            );
        }
    }
    Ok(0)
}

fn run_fetch_raw(args: FetchRawArgs) -> Result<i32> {
    let root = resolve_root(&args.root)?;
    let handle = PathBuf::from(&args.handle);
    let path = if handle.is_absolute() {
        handle
    } else {
        let candidate = root.join(handle);
        if candidate.exists() {
            candidate
        } else {
            task_state_json_path(&root, &args.task_id).parent().unwrap_or(&root).join(&args.handle)
        }
    };
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read raw artifact '{}'", path.display()))?;
    if args.json {
        crate::cmd_common::emit_json(
            &json!({
                "task_id": args.task_id,
                "handle": args.handle,
                "path": path.display().to_string(),
                "content": text,
            }),
            args.pretty,
        )?;
    } else {
        print!("{text}");
    }
    Ok(0)
}

fn emit_or_print(payload: &Value, preview: &str, json: bool, pretty: bool) -> Result<()> {
    if json {
        crate::cmd_common::emit_json(payload, pretty)
    } else {
        println!("{preview}");
        Ok(())
    }
}

fn resolve_root(root: &str) -> Result<PathBuf> {
    let cwd = crate::cmd_common::caller_cwd()?;
    Ok(PathBuf::from(crate::cmd_common::resolve_path_from_cwd(root, &cwd)))
}

fn resolve_cwd(cwd: Option<&str>) -> Result<PathBuf> {
    let caller_cwd = crate::cmd_common::caller_cwd()?;
    Ok(match cwd {
        Some(path) => PathBuf::from(crate::cmd_common::resolve_path_from_cwd(path, &caller_cwd)),
        None => caller_cwd,
    })
}

fn resolve_repo_path(_root: &Path, cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    let absolute = if path.is_absolute() { path } else { cwd.join(path) };
    absolute.canonicalize().unwrap_or(absolute)
}

fn walk_tree(
    root: &Path,
    path: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    hidden: bool,
    rendered: &mut Vec<String>,
    paths: &mut Vec<String>,
) -> Result<()> {
    if rendered.len() >= max_entries {
        return Ok(());
    }
    let relative = packet28_reducer_core::normalize_capture_path(root, &path.display().to_string());
    if !relative.is_empty() {
        let name = if depth == 0 { relative.clone() } else { format!("{}{}", "  ".repeat(depth), relative) };
        rendered.push(name);
        paths.push(relative.clone());
    }
    if depth >= max_depth || !path.is_dir() {
        return Ok(());
    }
    let mut children = fs::read_dir(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        let name = child.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if (!hidden && name.starts_with('.')) || name == ".git" || name == ".packet28" {
            continue;
        }
        walk_tree(root, &child, depth + 1, max_depth, max_entries, hidden, rendered, paths)?;
        if rendered.len() >= max_entries {
            break;
        }
    }
    Ok(())
}

fn select_read_lines<'a>(
    lines: &'a [&'a str],
    line_start: Option<usize>,
    line_end: Option<usize>,
    last: Option<usize>,
) -> (usize, usize, Vec<&'a str>) {
    if let Some(last) = last.filter(|value| *value > 0) {
        let start_idx = lines.len().saturating_sub(last);
        let rendered = lines[start_idx..].to_vec();
        return (start_idx + 1, lines.len(), rendered);
    }
    let start = line_start.unwrap_or(1).max(1);
    let end = line_end.unwrap_or(start + 79).max(start);
    let start_idx = start.saturating_sub(1).min(lines.len());
    let end_idx = end.min(lines.len());
    (start, end_idx, lines[start_idx..end_idx].to_vec())
}

fn describe_json(value: &Value, path: &str, lines: &mut Vec<String>, max_items: usize) {
    if lines.len() >= max_items {
        return;
    }
    match value {
        Value::Object(map) => {
            lines.push(format!("{path}: object({})", map.len()));
            for (key, child) in map.iter().take(max_items.saturating_sub(lines.len())) {
                describe_json(child, &format!("{path}.{key}"), lines, max_items);
            }
        }
        Value::Array(items) => {
            lines.push(format!("{path}: array({})", items.len()));
            if let Some(first) = items.first() {
                describe_json(first, &format!("{path}[0]"), lines, max_items);
            }
        }
        Value::String(_) => lines.push(format!("{path}: string")),
        Value::Number(_) => lines.push(format!("{path}: number")),
        Value::Bool(_) => lines.push(format!("{path}: bool")),
        Value::Null => lines.push(format!("{path}: null")),
    }
}

fn collect_dependency_manifests(start: &Path) -> Result<Vec<PathBuf>> {
    let mut manifests = Vec::<PathBuf>::new();
    walk_manifests(start, 0, 4, &mut manifests)?;
    manifests.sort();
    manifests.dedup();
    Ok(manifests)
}

fn walk_manifests(path: &Path, depth: usize, max_depth: usize, manifests: &mut Vec<PathBuf>) -> Result<()> {
    if depth > max_depth {
        return Ok(());
    }
    if path.is_file() {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml") {
            manifests.push(path.to_path_buf());
        }
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read '{}'", path.display()))? {
        let child = entry?.path();
        let name = child.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if matches!(name, ".git" | ".packet28" | "node_modules" | "target") {
            continue;
        }
        walk_manifests(&child, depth + 1, max_depth, manifests)?;
    }
    Ok(())
}

fn render_manifest_dependencies(path: &Path) -> Result<Vec<String>> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let raw = fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut lines = vec![format!("manifest {}", path.display())];
    match name {
        "package.json" => {
            let value: Value = serde_json::from_str(&raw)?;
            for section in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(map) = value.get(section).and_then(Value::as_object) {
                    let deps = map
                        .iter()
                        .take(12)
                        .map(|(key, value)| format!("- {section}: {key}@{}", value.as_str().unwrap_or("?")))
                        .collect::<Vec<_>>();
                    lines.extend(deps);
                }
            }
        }
        "Cargo.toml" | "pyproject.toml" => {
            let value: toml::Value = toml::from_str(&raw)?;
            for section in ["dependencies", "dev-dependencies", "build-dependencies", "project"] {
                if let Some(table) = value.get(section).and_then(toml::Value::as_table) {
                    for (key, value) in table.iter().take(12) {
                        lines.push(format!("- {section}: {key}={}", render_toml_value(value)));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(lines)
}

fn render_toml_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        toml::Value::Table(table) => table
            .iter()
            .take(3)
            .map(|(key, value)| format!("{key}={}", render_toml_value(value)))
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

fn load_task_state(root: &Path, task_id: &str) -> Result<BrokerGetContextResponse> {
    let bytes = fs::read(task_state_json_path(root, task_id))
        .with_context(|| format!("failed to read task state for '{}'", task_id))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn load_task_states(
    root: &Path,
    task_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, BrokerGetContextResponse)>> {
    let registry = load_task_registry(root)?;
    let mut task_ids = registry.tasks.into_keys().collect::<Vec<_>>();
    task_ids.sort();
    if let Some(task_id) = task_id {
        task_ids.retain(|candidate| candidate == task_id);
    }
    let mut states = Vec::<(String, BrokerGetContextResponse)>::new();
    for task_id in task_ids.into_iter().take(limit.max(1)) {
        if let Ok(state) = load_task_state(root, &task_id) {
            states.push((task_id, state));
        }
    }
    Ok(states)
}

fn record_tool_result(
    root: &Path,
    task_id: Option<&str>,
    tool_name: &str,
    compact_path: &str,
    request_summary: String,
    result_summary: String,
    raw_est_tokens: Option<u64>,
    reduced_est_tokens: Option<u64>,
    paths: Vec<String>,
    symbols: Vec<String>,
) -> Result<()> {
    let Some(task_id) = task_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    crate::broker_client::write_state(
        root,
        BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolResult),
            tool_name: Some(tool_name.to_string()),
            request_summary: Some(request_summary),
            result_summary: Some(result_summary),
            compact_path: Some(compact_path.to_string()),
            raw_est_tokens,
            reduced_est_tokens,
            paths,
            symbols,
            raw_artifact_available: Some(false),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )?;
    Ok(())
}

fn record_tool_failure(
    root: &Path,
    task_id: Option<&str>,
    tool_name: &str,
    compact_path: &str,
    request_summary: String,
    error_message: String,
) -> Result<()> {
    let Some(task_id) = task_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    crate::broker_client::write_state(
        root,
        BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolInvocationFailed),
            tool_name: Some(tool_name.to_string()),
            request_summary: Some(request_summary),
            compact_path: Some(compact_path.to_string()),
            error_class: Some("command_failed".to_string()),
            error_message: Some(error_message),
            raw_artifact_available: Some(false),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )?;
    Ok(())
}

fn estimate_tokens_for_value(value: &Value) -> u64 {
    let bytes = serde_json::to_vec(value).unwrap_or_default().len() as f64;
    (bytes / 4.0).ceil() as u64
}

fn estimate_tokens_for_strings(values: &[String]) -> u64 {
    estimate_tokens_str(&values.join("\n"))
}

fn estimate_tokens_str(value: &str) -> u64 {
    ((value.len() as f64) / 4.0).ceil() as u64
}

fn pct_saved(raw: u64, reduced: u64) -> f64 {
    if raw == 0 {
        0.0
    } else {
        ((raw.saturating_sub(reduced)) as f64 / raw as f64) * 100.0
    }
}
