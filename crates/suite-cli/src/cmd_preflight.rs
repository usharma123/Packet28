use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use context_memory_core::{PacketCache, PersistConfig, RecallHit, RecallOptions, RecallScope};
use serde::Serialize;
use serde_json::{json, Value};
use suite_foundation_core::config::{GateConfig, IssueGateConfig};
use suite_foundation_core::CovyConfig;
use suite_packet_core::{BudgetCost, EnvelopeV1, PacketWrapperV1};

const PREFLIGHT_SCHEMA_VERSION: &str = "suite.preflight.v1";
const DEFAULT_PREFLIGHT_BUDGET_TOKENS: u64 = 5_000;
const DEFAULT_PREFLIGHT_RECALL_LIMIT: usize = 4;
const DEFAULT_PREFLIGHT_RECALL_WINDOW_SECS: u64 = 7 * 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum PreflightReducer {
    Cover,
    Diff,
    Map,
    Recall,
    Stack,
    Build,
    Impact,
}

impl PreflightReducer {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Diff => "diff",
            Self::Map => "map",
            Self::Recall => "recall",
            Self::Stack => "stack",
            Self::Build => "build",
            Self::Impact => "impact",
        }
    }

    fn planning_cost(self) -> u64 {
        match self {
            Self::Cover => 800,
            Self::Diff => 1_200,
            Self::Map => 2_000,
            Self::Recall => 600,
            Self::Stack => 500,
            Self::Build => 600,
            Self::Impact => 900,
        }
    }

    fn execution_order() -> &'static [Self] {
        &[
            Self::Cover,
            Self::Diff,
            Self::Map,
            Self::Stack,
            Self::Build,
            Self::Impact,
            Self::Recall,
        ]
    }
}

#[derive(Args, Clone)]
pub struct PreflightArgs {
    /// Natural-language task description
    #[arg(long)]
    pub task: String,

    /// Root path for repo-aware reducers and persisted context
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Optional task identifier for recall scoping
    #[arg(long)]
    pub task_id: Option<String>,

    /// Base ref for git-aware reducers (default from config)
    #[arg(long)]
    pub base: Option<String>,

    /// Head ref for git-aware reducers (default from config)
    #[arg(long)]
    pub head: Option<String>,

    /// Overall planning token budget for heuristic reducer selection
    #[arg(long, default_value_t = DEFAULT_PREFLIGHT_BUDGET_TOKENS)]
    pub budget_tokens: u64,

    /// Maximum recall hits to include
    #[arg(long, default_value_t = DEFAULT_PREFLIGHT_RECALL_LIMIT)]
    pub limit_recall: usize,

    /// Explicit focus paths
    #[arg(long = "focus-path")]
    pub focus_paths: Vec<String>,

    /// Explicit focus symbols
    #[arg(long = "focus-symbol")]
    pub focus_symbols: Vec<String>,

    /// Coverage report file paths
    #[arg(long = "coverage")]
    pub coverage: Vec<String>,

    /// Optional stack/log input path
    #[arg(long)]
    pub stack_input: Option<String>,

    /// Optional build/lint log input path
    #[arg(long)]
    pub build_input: Option<String>,

    /// Testmap path for impact analysis
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Force-include reducers
    #[arg(long = "include", value_enum)]
    pub include: Vec<PreflightReducer>,

    /// Force-exclude reducers
    #[arg(long = "exclude", value_enum)]
    pub exclude: Vec<PreflightReducer>,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,
}

impl PreflightArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some()
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct PreflightAnchors {
    paths: Vec<String>,
    symbols: Vec<String>,
    terms: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightSkippedReducer {
    reducer: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightSelection {
    tags: Vec<String>,
    anchors: PreflightAnchors,
    selected_reducers: Vec<String>,
    skipped: Vec<PreflightSkippedReducer>,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightPacketResult {
    reducer: String,
    packet_type: String,
    cache_hit: bool,
    packet: Value,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightRecallResult {
    query: String,
    hits: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightResults {
    packets: Vec<PreflightPacketResult>,
    recall: PreflightRecallResult,
}

#[derive(Debug, Clone, Serialize, Default)]
struct PreflightTotals {
    est_tokens: u64,
    est_bytes: usize,
    runtime_ms: u64,
    tool_calls: u64,
    packet_count: usize,
    cache_hits: usize,
    over_budget: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PreflightResponse {
    schema_version: String,
    task: String,
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    profile: String,
    selection: PreflightSelection,
    results: PreflightResults,
    totals: PreflightTotals,
}

#[derive(Debug, Clone)]
struct PacketExecutionResult {
    reducer: PreflightReducer,
    packet_type: String,
    cache_hit: bool,
    packet: Value,
    budget_cost: BudgetCost,
}

#[derive(Debug, Clone, Default)]
struct SelectionPlan {
    tags: Vec<String>,
    anchors: PreflightAnchors,
    selected: Vec<PreflightReducer>,
    skipped: Vec<PreflightSkippedReducer>,
    over_budget: bool,
}

#[derive(Debug, Clone, Copy)]
enum ExecutionMode<'a> {
    Local,
    Remote(&'a Path),
}

pub fn run(args: PreflightArgs, config_path: &str) -> Result<i32> {
    let response = execute(args.clone(), config_path, ExecutionMode::Local)?;
    emit_response(&args, &response)?;
    Ok(0)
}

pub fn run_remote(args: PreflightArgs, config_path: &str, daemon_root: &Path) -> Result<i32> {
    let response = execute(
        args.clone(),
        config_path,
        ExecutionMode::Remote(daemon_root),
    )?;
    emit_response(&args, &response)?;
    Ok(0)
}

fn execute(
    args: PreflightArgs,
    config_path: &str,
    mode: ExecutionMode<'_>,
) -> Result<PreflightResponse> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let root = PathBuf::from(crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd));
    let config_path = crate::cmd_common::resolve_path_from_cwd(config_path, &root);
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let config = CovyConfig::load(Path::new(&config_path)).unwrap_or_default();
    let (base, head) = resolve_git_refs(
        &root,
        args.base.as_deref(),
        args.head.as_deref(),
        &config.diff.base,
        &config.diff.head,
    );

    let coverage_paths = resolve_paths_relative_to_root(&args.coverage, &root);
    let stack_input = resolve_optional_path_relative_to_root(args.stack_input.as_deref(), &root);
    let build_input = resolve_optional_path_relative_to_root(args.build_input.as_deref(), &root);
    let testmap_path = crate::cmd_common::resolve_path_from_cwd(&args.testmap, &root);
    let coverage_state_path = root.join(".covy").join("state").join("latest.bin");
    let issues_state_path = root.join(".covy").join("state").join("issues.bin");

    let availability = Availability {
        has_cover: !coverage_paths.is_empty() || coverage_state_path.exists(),
        has_diff: is_git_repo(&root),
        has_map: true,
        has_recall: true,
        has_stack: stack_input.is_some(),
        has_build: build_input.is_some(),
        has_impact: Path::new(&testmap_path).exists(),
    };

    let selection = plan_selection(&args, &availability);
    let artifact_root = root.clone();
    let mut totals = PreflightTotals {
        over_budget: selection.over_budget,
        ..PreflightTotals::default()
    };
    let mut packets = Vec::new();

    for reducer in &selection.selected {
        let packet = match reducer {
            PreflightReducer::Cover => run_cover(
                mode,
                &root,
                &config_path,
                &base,
                &head,
                &coverage_paths,
                coverage_state_path
                    .exists()
                    .then_some(coverage_state_path.as_path()),
                issues_state_path
                    .exists()
                    .then_some(issues_state_path.as_path()),
                profile,
                &artifact_root,
            )?,
            PreflightReducer::Diff => run_diff(
                mode,
                &root,
                &config_path,
                &base,
                &head,
                &coverage_paths,
                coverage_state_path
                    .exists()
                    .then_some(coverage_state_path.as_path()),
                issues_state_path
                    .exists()
                    .then_some(issues_state_path.as_path()),
                profile,
                &artifact_root,
            )?,
            PreflightReducer::Map => {
                run_map(mode, &root, &selection.anchors, profile, &artifact_root)?
            }
            PreflightReducer::Stack => run_stack(
                mode,
                stack_input
                    .as_deref()
                    .ok_or_else(|| anyhow!("missing stack input after selection"))?,
                profile,
                &artifact_root,
            )?,
            PreflightReducer::Build => run_build(
                mode,
                build_input
                    .as_deref()
                    .ok_or_else(|| anyhow!("missing build input after selection"))?,
                profile,
                &artifact_root,
            )?,
            PreflightReducer::Impact => run_impact(
                mode,
                &root,
                &config_path,
                &base,
                &head,
                &testmap_path,
                profile,
                &artifact_root,
            )?,
            PreflightReducer::Recall => continue,
        };
        totals.est_tokens = totals
            .est_tokens
            .saturating_add(packet.budget_cost.est_tokens);
        totals.est_bytes = totals
            .est_bytes
            .saturating_add(packet.budget_cost.est_bytes);
        totals.runtime_ms = totals
            .runtime_ms
            .saturating_add(packet.budget_cost.runtime_ms);
        totals.tool_calls = totals
            .tool_calls
            .saturating_add(packet.budget_cost.tool_calls);
        totals.packet_count = totals.packet_count.saturating_add(1);
        totals.cache_hits = totals
            .cache_hits
            .saturating_add(usize::from(packet.cache_hit));
        packets.push(PreflightPacketResult {
            reducer: packet.reducer.as_str().to_string(),
            packet_type: packet.packet_type,
            cache_hit: packet.cache_hit,
            packet: packet.packet,
        });
    }

    let recall_query = build_recall_query(&args.task, &selection.anchors);
    let recall_hits = if selection.selected.contains(&PreflightReducer::Recall) {
        let hits = match mode {
            ExecutionMode::Local => {
                run_recall_local(&root, &recall_query, &selection.anchors, &args)?
            }
            ExecutionMode::Remote(daemon_root) => {
                run_recall_remote(daemon_root, &root, &recall_query, &selection.anchors, &args)?
            }
        };
        for hit in &hits {
            totals.est_tokens = totals
                .est_tokens
                .saturating_add(hit.budget_estimate.est_tokens);
            totals.est_bytes = totals
                .est_bytes
                .saturating_add(hit.budget_estimate.est_bytes as usize);
            totals.runtime_ms = totals
                .runtime_ms
                .saturating_add(hit.budget_estimate.runtime_ms);
        }
        hits
    } else {
        Vec::new()
    };

    Ok(PreflightResponse {
        schema_version: PREFLIGHT_SCHEMA_VERSION.to_string(),
        task: args.task.clone(),
        root: root.to_string_lossy().into_owned(),
        task_id: args.task_id.clone(),
        profile: profile.to_string(),
        selection: PreflightSelection {
            tags: selection.tags,
            anchors: selection.anchors.clone(),
            selected_reducers: selection
                .selected
                .iter()
                .map(|reducer| reducer.as_str().to_string())
                .collect(),
            skipped: selection.skipped,
        },
        results: PreflightResults {
            packets,
            recall: PreflightRecallResult {
                query: recall_query,
                hits: profile_recall_hits(&recall_hits, profile),
            },
        },
        totals,
    })
}

#[derive(Debug, Clone, Copy)]
struct Availability {
    has_cover: bool,
    has_diff: bool,
    has_map: bool,
    has_recall: bool,
    has_stack: bool,
    has_build: bool,
    has_impact: bool,
}

fn plan_selection(args: &PreflightArgs, availability: &Availability) -> SelectionPlan {
    let anchors = extract_anchors(&args.task, &args.focus_paths, &args.focus_symbols);
    let tags = classify_tags(&anchors.terms, &args.task);
    let heuristics = heuristic_reducers(&tags);
    let includes = args.include.iter().copied().collect::<HashSet<_>>();
    let excludes = args.exclude.iter().copied().collect::<HashSet<_>>();
    let mut selected = heuristics
        .into_iter()
        .chain(includes.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut skipped = Vec::new();
    let mut kept = Vec::new();
    let mut planned_tokens = 0_u64;

    for reducer in PreflightReducer::execution_order() {
        if !selected.remove(reducer) {
            continue;
        }
        if excludes.contains(reducer) {
            skipped.push(skip(*reducer, "excluded"));
            continue;
        }
        if let Some(reason) = availability_reason(*reducer, availability) {
            skipped.push(skip(*reducer, reason));
            continue;
        }
        let planned_cost = reducer.planning_cost();
        if !includes.contains(reducer)
            && args.budget_tokens > 0
            && planned_tokens.saturating_add(planned_cost) > args.budget_tokens
        {
            skipped.push(skip(*reducer, "budget_trimmed"));
            continue;
        }
        planned_tokens = planned_tokens.saturating_add(planned_cost);
        kept.push(*reducer);
    }

    SelectionPlan {
        tags,
        anchors,
        selected: kept,
        skipped,
        over_budget: planned_tokens > args.budget_tokens,
    }
}

fn availability_reason(
    reducer: PreflightReducer,
    availability: &Availability,
) -> Option<&'static str> {
    match reducer {
        PreflightReducer::Cover if !availability.has_cover => Some("no_coverage_input"),
        PreflightReducer::Diff if !availability.has_diff => Some("not_git_repo"),
        PreflightReducer::Map if !availability.has_map => Some("map_unavailable"),
        PreflightReducer::Recall if !availability.has_recall => Some("recall_unavailable"),
        PreflightReducer::Stack if !availability.has_stack => Some("no_stack_input"),
        PreflightReducer::Build if !availability.has_build => Some("no_build_input"),
        PreflightReducer::Impact if !availability.has_impact => Some("no_testmap"),
        _ => None,
    }
}

fn heuristic_reducers(tags: &[String]) -> BTreeSet<PreflightReducer> {
    let mut reducers = BTreeSet::new();
    for tag in tags {
        match tag.as_str() {
            "coverage" => {
                reducers.insert(PreflightReducer::Cover);
                reducers.insert(PreflightReducer::Diff);
                reducers.insert(PreflightReducer::Map);
                reducers.insert(PreflightReducer::Recall);
            }
            "diff" => {
                reducers.insert(PreflightReducer::Diff);
                reducers.insert(PreflightReducer::Map);
                reducers.insert(PreflightReducer::Recall);
            }
            "build" => {
                reducers.insert(PreflightReducer::Build);
                reducers.insert(PreflightReducer::Diff);
                reducers.insert(PreflightReducer::Recall);
            }
            "stack" => {
                reducers.insert(PreflightReducer::Stack);
                reducers.insert(PreflightReducer::Map);
                reducers.insert(PreflightReducer::Recall);
            }
            "test" => {
                reducers.insert(PreflightReducer::Impact);
                reducers.insert(PreflightReducer::Diff);
                reducers.insert(PreflightReducer::Recall);
            }
            _ => {}
        }
    }
    if reducers.is_empty() {
        reducers.insert(PreflightReducer::Diff);
        reducers.insert(PreflightReducer::Map);
        reducers.insert(PreflightReducer::Recall);
    }
    reducers
}

fn classify_tags(terms: &[String], task: &str) -> Vec<String> {
    let lower_task = task.to_ascii_lowercase();
    let mut tags = Vec::new();

    if has_any(
        terms,
        &["coverage", "cover", "jacoco", "lcov", "cobertura", "gate"],
    ) {
        tags.push("coverage".to_string());
    }
    if has_any(
        terms,
        &[
            "diff",
            "change",
            "changes",
            "changed",
            "regression",
            "review",
            "patch",
            "branch",
            "pr",
        ],
    ) {
        tags.push("diff".to_string());
    }
    if has_any(
        terms,
        &[
            "build",
            "compile",
            "compiler",
            "lint",
            "linter",
            "diagnostic",
            "warning",
            "warnings",
            "error",
            "errors",
        ],
    ) || lower_task.contains("build break")
    {
        tags.push("build".to_string());
    }
    if has_any(
        terms,
        &[
            "stack",
            "trace",
            "exception",
            "failure",
            "failures",
            "crash",
            "panic",
        ],
    ) {
        tags.push("stack".to_string());
    }
    if has_any(
        terms,
        &["test", "tests", "testing", "impact", "flaky", "flake"],
    ) {
        tags.push("test".to_string());
    }

    tags.sort();
    tags.dedup();
    tags
}

fn has_any(terms: &[String], candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| terms.iter().any(|term| term == candidate))
}

fn extract_anchors(
    task: &str,
    focus_paths: &[String],
    focus_symbols: &[String],
) -> PreflightAnchors {
    let mut paths = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let mut terms = BTreeSet::new();

    for value in focus_paths {
        let normalized = clean_token(value);
        if !normalized.is_empty() {
            paths.insert(normalized);
        }
    }
    for value in focus_symbols {
        let normalized = clean_token(value);
        if !normalized.is_empty() {
            symbols.insert(normalized);
        }
    }

    for raw in task.split_whitespace() {
        let token = clean_token(raw);
        if token.is_empty() {
            continue;
        }
        let lowered = token.to_ascii_lowercase();
        if looks_like_path(&token) {
            paths.insert(token.clone());
        } else if looks_like_symbol(&token) {
            symbols.insert(token.clone());
        }
        if lowered.len() >= 3 && !is_stopword(&lowered) {
            terms.insert(lowered);
        }
    }

    PreflightAnchors {
        paths: paths.into_iter().collect(),
        symbols: symbols.into_iter().collect(),
        terms: terms.into_iter().collect(),
    }
}

fn build_recall_query(task: &str, anchors: &PreflightAnchors) -> String {
    let mut parts = vec![task.trim().to_string()];
    parts.extend(anchors.paths.iter().take(3).cloned());
    parts.extend(anchors.symbols.iter().take(3).cloned());
    parts.join(" ")
}

fn run_recall_local(
    root: &Path,
    query: &str,
    anchors: &PreflightAnchors,
    args: &PreflightArgs,
) -> Result<Vec<RecallHit>> {
    let cache = PacketCache::load_from_disk(&PersistConfig::new(root.to_path_buf()));
    let since = current_unix().saturating_sub(DEFAULT_PREFLIGHT_RECALL_WINDOW_SECS);
    Ok(cache.recall(
        query,
        &RecallOptions {
            limit: args.limit_recall.max(1),
            since_unix: Some(since),
            task_id: args.task_id.clone(),
            scope: if args.task_id.is_some() {
                RecallScope::TaskFirst
            } else {
                RecallScope::Global
            },
            path_filters: anchors.paths.clone(),
            symbol_filters: anchors.symbols.clone(),
            ..RecallOptions::default()
        },
    ))
}

fn run_recall_remote(
    daemon_root: &Path,
    root: &Path,
    query: &str,
    anchors: &PreflightAnchors,
    args: &PreflightArgs,
) -> Result<Vec<RecallHit>> {
    let response = crate::cmd_daemon::execute_context_recall(
        daemon_root,
        packet28_daemon_core::ContextRecallRequest {
            query: query.to_string(),
            root: root.to_string_lossy().into_owned(),
            limit: args.limit_recall.max(1),
            since: Some(current_unix().saturating_sub(DEFAULT_PREFLIGHT_RECALL_WINDOW_SECS)),
            until: None,
            target: None,
            task_id: args.task_id.clone(),
            scope: Some(if args.task_id.is_some() {
                "task_first".to_string()
            } else {
                "global".to_string()
            }),
            packet_types: Vec::new(),
            path_filters: anchors.paths.clone(),
            symbol_filters: anchors.symbols.clone(),
        },
    )?;
    Ok(response.hits)
}

fn run_cover(
    mode: ExecutionMode<'_>,
    root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => run_cover_local(
            root,
            config_path,
            base,
            head,
            coverage_paths,
            coverage_state,
            issues_state,
            profile,
            artifact_root,
        ),
        ExecutionMode::Remote(daemon_root) => run_cover_remote(
            daemon_root,
            root,
            config_path,
            base,
            head,
            coverage_paths,
            coverage_state,
            issues_state,
            profile,
            artifact_root,
        ),
    }
}

fn run_cover_local(
    _root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let issue_gate = IssueGateConfig {
        max_new_errors: config.gate.issues.max_new_errors,
        max_new_warnings: config.gate.issues.max_new_warnings,
        max_new_issues: config.gate.issues.max_new_issues,
    };
    let gate_config = GateConfig {
        fail_under_total: config.gate.fail_under_total,
        fail_under_changed: config.gate.fail_under_changed,
        fail_under_new: config.gate.fail_under_new,
        issues: issue_gate,
    };
    let request = diffy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root: None,
        coverage: diffy_core::pipeline::PipelineCoverageInput {
            paths: coverage_paths.to_vec(),
            format: None,
            stdin: false,
            input_state_path: coverage_state.map(|path| path.to_string_lossy().into_owned()),
            default_input_state_path: None,
            strip_prefixes: config.ingest.strip_prefixes.clone(),
            reject_paths_with_input: true,
            no_inputs_error: "No coverage input found for preflight".to_string(),
        },
        diagnostics: diffy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: Vec::new(),
            issues_state_path: issues_state.map(|path| path.to_string_lossy().into_owned()),
            no_issues_state: issues_state.is_none(),
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };
    let output = diffy_core::pipeline::run_analysis(
        request,
        &crate::cmd_common::default_pipeline_ingest_adapters(),
    )?;
    let gate_json = serde_json::to_value(&output.gate_result)?;
    let gate_json_bytes = serde_json::to_vec(&gate_json)?.len();
    let mut changed_paths = output
        .changed_line_context
        .changed_paths
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();
    let envelope = EnvelopeV1 {
        version: "1".to_string(),
        tool: "covy".to_string(),
        kind: "coverage_gate".to_string(),
        hash: String::new(),
        summary: format!(
            "passed={} changed={:?} total={:?} new={:?}",
            output.gate_result.passed,
            output.gate_result.changed_coverage_pct,
            output.gate_result.total_coverage_pct,
            output.gate_result.new_file_coverage_pct
        ),
        files: changed_paths
            .iter()
            .map(|path| suite_packet_core::FileRef {
                path: path.clone(),
                relevance: Some(0.75),
                source: Some("cover.check".to_string()),
            })
            .collect(),
        symbols: Vec::new(),
        risk: None,
        confidence: Some(if output.gate_result.passed { 1.0 } else { 0.8 }),
        budget_cost: BudgetCost {
            est_tokens: 0,
            est_bytes: 0,
            runtime_ms: 0,
            tool_calls: 1,
            payload_est_tokens: Some((gate_json_bytes / 4) as u64),
            payload_est_bytes: Some(gate_json_bytes),
        },
        provenance: suite_packet_core::Provenance {
            inputs: changed_paths,
            git_base: Some(base.to_string()),
            git_head: Some(head.to_string()),
            generated_at_unix: current_unix(),
        },
        payload: gate_json,
    }
    .with_canonical_hash_and_real_budget();
    packet_result(
        PreflightReducer::Cover,
        suite_packet_core::PACKET_TYPE_COVER_CHECK,
        false,
        &envelope,
        profile,
        artifact_root,
    )
}

fn run_cover_remote(
    daemon_root: &Path,
    root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    let response = crate::cmd_daemon::send_cover_check(
        daemon_root,
        packet28_daemon_core::CoverCheckRequest {
            coverage: coverage_paths.to_vec(),
            paths: Vec::new(),
            format: "auto".to_string(),
            issues: Vec::new(),
            issues_state: issues_state.map(|path| path.to_string_lossy().into_owned()),
            no_issues_state: issues_state.is_none(),
            base: Some(base.to_string()),
            head: Some(head.to_string()),
            fail_under_total: None,
            fail_under_changed: None,
            fail_under_new: None,
            max_new_errors: None,
            max_new_warnings: None,
            input: coverage_state.map(|path| path.to_string_lossy().into_owned()),
            strip_prefix: Vec::new(),
            source_root: Some(root.to_string_lossy().into_owned()),
            show_missing: false,
            config_path: config_path.to_string(),
        },
    )?;
    let envelope: EnvelopeV1<Value> =
        serde_json::from_value(serde_json::to_value(response.envelope)?)?;
    packet_result(
        PreflightReducer::Cover,
        &response.packet_type,
        false,
        &envelope,
        profile,
        artifact_root,
    )
}

fn run_diff(
    mode: ExecutionMode<'_>,
    root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => run_diff_local(
            root,
            config_path,
            base,
            head,
            coverage_paths,
            coverage_state,
            issues_state,
            profile,
            artifact_root,
        ),
        ExecutionMode::Remote(daemon_root) => run_diff_remote(
            daemon_root,
            root,
            config_path,
            base,
            head,
            coverage_paths,
            coverage_state,
            issues_state,
            profile,
            artifact_root,
        ),
    }
}

fn run_diff_local(
    _root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let output = diffy_core::pipeline::run_analysis(
        context_kernel_core::build_diff_pipeline_request(
            &context_kernel_core::DiffAnalyzeKernelInput {
                base: base.to_string(),
                head: head.to_string(),
                fail_under_changed: config.gate.fail_under_changed,
                fail_under_total: config.gate.fail_under_total,
                fail_under_new: config.gate.fail_under_new,
                max_new_errors: config.gate.issues.max_new_errors,
                max_new_warnings: config.gate.issues.max_new_warnings,
                max_new_issues: config.gate.issues.max_new_issues,
                issues: Vec::new(),
                issues_state: issues_state.map(|path| path.to_string_lossy().into_owned()),
                no_issues_state: issues_state.is_none(),
                coverage: coverage_paths.to_vec(),
                input: coverage_state.map(|path| path.to_string_lossy().into_owned()),
            },
        ),
        &crate::cmd_common::default_pipeline_ingest_adapters(),
    )?;
    let envelope = context_kernel_core::build_diff_analyze_envelope(&output, base, head);
    packet_result(
        PreflightReducer::Diff,
        suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
        false,
        &envelope,
        profile,
        artifact_root,
    )
}

fn run_diff_remote(
    daemon_root: &Path,
    root: &Path,
    _config_path: &str,
    base: &str,
    head: &str,
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    let cache_fingerprint = crate::cmd_common::repo_cache_fingerprint(
        root,
        &diff_fingerprint_paths(coverage_paths, coverage_state, issues_state),
    );
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "diffy.analyze".to_string(),
            reducer_input: serde_json::to_value(context_kernel_core::DiffAnalyzeKernelInput {
                base: base.to_string(),
                head: head.to_string(),
                fail_under_changed: None,
                fail_under_total: None,
                fail_under_new: None,
                max_new_errors: None,
                max_new_warnings: None,
                max_new_issues: None,
                issues: Vec::new(),
                issues_state: issues_state.map(|path| path.to_string_lossy().into_owned()),
                no_issues_state: issues_state.is_none(),
                coverage: coverage_paths.to_vec(),
                input: coverage_state.map(|path| path.to_string_lossy().into_owned()),
            })?,
            policy_context: json!({
                "cache_fingerprint": cache_fingerprint,
            }),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;
    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: EnvelopeV1<Value> = serde_json::from_value(output_packet.body.clone())
        .map_err(|source| anyhow!("invalid diff analyze output packet: {source}"))?;
    packet_result(
        PreflightReducer::Diff,
        suite_packet_core::PACKET_TYPE_DIFF_ANALYZE,
        cache_hit(&response.metadata),
        &envelope,
        profile,
        artifact_root,
    )
}

fn diff_fingerprint_paths(
    coverage_paths: &[String],
    coverage_state: Option<&Path>,
    issues_state: Option<&Path>,
) -> Vec<PathBuf> {
    coverage_paths
        .iter()
        .map(PathBuf::from)
        .chain(coverage_state.iter().map(|path| path.to_path_buf()))
        .chain(issues_state.iter().map(|path| path.to_path_buf()))
        .collect()
}

fn run_map(
    mode: ExecutionMode<'_>,
    root: &Path,
    anchors: &PreflightAnchors,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => {
            let envelope = mapy_core::build_repo_map(mapy_core::RepoMapRequest {
                repo_root: root.to_string_lossy().into_owned(),
                focus_paths: anchors.paths.clone(),
                focus_symbols: anchors.symbols.clone(),
                max_files: 20,
                max_symbols: 60,
                include_tests: false,
            })?;
            packet_result(
                PreflightReducer::Map,
                suite_packet_core::PACKET_TYPE_MAP_REPO,
                false,
                &envelope,
                profile,
                artifact_root,
            )
        }
        ExecutionMode::Remote(daemon_root) => {
            let response = crate::cmd_daemon::send_kernel_request(
                daemon_root,
                context_kernel_core::KernelRequest {
                    target: "mapy.repo".to_string(),
                    reducer_input: serde_json::to_value(mapy_core::RepoMapRequest {
                        repo_root: root.to_string_lossy().into_owned(),
                        focus_paths: anchors.paths.clone(),
                        focus_symbols: anchors.symbols.clone(),
                        max_files: 20,
                        max_symbols: 60,
                        include_tests: false,
                    })?,
                    policy_context: json!({
                        "cache_fingerprint": crate::cmd_common::repo_cache_fingerprint(root, &[]),
                    }),
                    ..context_kernel_core::KernelRequest::default()
                },
            )?;
            let output_packet = response
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
            let envelope: EnvelopeV1<Value> = serde_json::from_value(output_packet.body.clone())
                .map_err(|source| anyhow!("invalid mapy output packet: {source}"))?;
            packet_result(
                PreflightReducer::Map,
                suite_packet_core::PACKET_TYPE_MAP_REPO,
                cache_hit(&response.metadata),
                &envelope,
                profile,
                artifact_root,
            )
        }
    }
}

fn run_stack(
    mode: ExecutionMode<'_>,
    input: &str,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => {
            let log_text = fs::read_to_string(input)
                .with_context(|| format!("failed to read stack input '{input}'"))?;
            let envelope = stacky_core::slice_to_envelope(stacky_core::StackSliceRequest {
                log_text,
                source: Some(input.to_string()),
                max_failures: None,
            });
            packet_result(
                PreflightReducer::Stack,
                suite_packet_core::PACKET_TYPE_STACK_SLICE,
                false,
                &envelope,
                profile,
                artifact_root,
            )
        }
        ExecutionMode::Remote(daemon_root) => {
            let log_text = fs::read_to_string(input)
                .with_context(|| format!("failed to read stack input '{input}'"))?;
            let response = crate::cmd_daemon::send_kernel_request(
                daemon_root,
                context_kernel_core::KernelRequest {
                    target: "stacky.slice".to_string(),
                    reducer_input: serde_json::to_value(stacky_core::StackSliceRequest {
                        log_text,
                        source: Some(input.to_string()),
                        max_failures: None,
                    })?,
                    ..context_kernel_core::KernelRequest::default()
                },
            )?;
            let output_packet = response
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
            let envelope: EnvelopeV1<Value> = serde_json::from_value(output_packet.body.clone())
                .map_err(|source| anyhow!("invalid stacky output packet: {source}"))?;
            packet_result(
                PreflightReducer::Stack,
                suite_packet_core::PACKET_TYPE_STACK_SLICE,
                cache_hit(&response.metadata),
                &envelope,
                profile,
                artifact_root,
            )
        }
    }
}

fn run_build(
    mode: ExecutionMode<'_>,
    input: &str,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => {
            let log_text = fs::read_to_string(input)
                .with_context(|| format!("failed to read build input '{input}'"))?;
            let envelope = buildy_core::reduce_to_envelope(buildy_core::BuildReduceRequest {
                log_text,
                source: Some(input.to_string()),
                max_diagnostics: None,
            });
            packet_result(
                PreflightReducer::Build,
                suite_packet_core::PACKET_TYPE_BUILD_REDUCE,
                false,
                &envelope,
                profile,
                artifact_root,
            )
        }
        ExecutionMode::Remote(daemon_root) => {
            let log_text = fs::read_to_string(input)
                .with_context(|| format!("failed to read build input '{input}'"))?;
            let response = crate::cmd_daemon::send_kernel_request(
                daemon_root,
                context_kernel_core::KernelRequest {
                    target: "buildy.reduce".to_string(),
                    reducer_input: serde_json::to_value(buildy_core::BuildReduceRequest {
                        log_text,
                        source: Some(input.to_string()),
                        max_diagnostics: None,
                    })?,
                    ..context_kernel_core::KernelRequest::default()
                },
            )?;
            let output_packet = response
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
            let envelope: EnvelopeV1<Value> = serde_json::from_value(output_packet.body.clone())
                .map_err(|source| anyhow!("invalid buildy output packet: {source}"))?;
            packet_result(
                PreflightReducer::Build,
                suite_packet_core::PACKET_TYPE_BUILD_REDUCE,
                cache_hit(&response.metadata),
                &envelope,
                profile,
                artifact_root,
            )
        }
    }
}

fn run_impact(
    mode: ExecutionMode<'_>,
    root: &Path,
    config_path: &str,
    base: &str,
    head: &str,
    testmap_path: &str,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    match mode {
        ExecutionMode::Local => {
            let adapters = testy_cli_common::adapters::default_impact_adapters();
            let output = testy_core::command_impact::run_legacy_impact(
                testy_core::command_impact::LegacyImpactArgs {
                    base: Some(base.to_string()),
                    head: Some(head.to_string()),
                    testmap: testmap_path.to_string(),
                    print_command: false,
                },
                config_path,
                &adapters,
            )?;
            let envelope = context_kernel_core::build_test_impact_envelope(
                &output,
                testmap_path,
                Some(base),
                Some(head),
            );
            packet_result(
                PreflightReducer::Impact,
                suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                false,
                &envelope,
                profile,
                artifact_root,
            )
        }
        ExecutionMode::Remote(daemon_root) => {
            let cache_fingerprint =
                crate::cmd_common::repo_cache_fingerprint(root, &[PathBuf::from(testmap_path)]);
            let response = crate::cmd_daemon::send_kernel_request(
                daemon_root,
                context_kernel_core::KernelRequest {
                    target: "testy.impact".to_string(),
                    reducer_input: serde_json::to_value(context_kernel_core::ImpactKernelInput {
                        base: Some(base.to_string()),
                        head: Some(head.to_string()),
                        testmap: testmap_path.to_string(),
                        print_command: false,
                        config_path: config_path.to_string(),
                    })?,
                    policy_context: json!({
                        "cache_fingerprint": cache_fingerprint,
                    }),
                    ..context_kernel_core::KernelRequest::default()
                },
            )?;
            let output_packet = response
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
            let envelope: EnvelopeV1<Value> = serde_json::from_value(output_packet.body.clone())
                .map_err(|source| anyhow!("invalid test impact output packet: {source}"))?;
            packet_result(
                PreflightReducer::Impact,
                suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                cache_hit(&response.metadata),
                &envelope,
                profile,
                artifact_root,
            )
        }
    }
}

fn packet_result<T: Serialize + Clone>(
    reducer: PreflightReducer,
    packet_type: &str,
    cache_hit: bool,
    envelope: &EnvelopeV1<T>,
    profile: suite_packet_core::JsonProfile,
    artifact_root: &Path,
) -> Result<PacketExecutionResult> {
    let mut wrapper = PacketWrapperV1::new(packet_type.to_string(), envelope.clone());
    wrapper.cache_hit = cache_hit;
    let packet = crate::cmd_common::machine_wrapper_value(&wrapper, profile, artifact_root, None)?;
    Ok(PacketExecutionResult {
        reducer,
        packet_type: packet_type.to_string(),
        cache_hit,
        packet,
        budget_cost: envelope.budget_cost.clone(),
    })
}

fn emit_response(args: &PreflightArgs, response: &PreflightResponse) -> Result<()> {
    if args.machine_output_requested() {
        crate::cmd_common::emit_json(&serde_json::to_value(response)?, args.pretty)?;
        return Ok(());
    }

    println!(
        "task={} reducers={} total_est_tokens={} runtime_ms={}",
        response.task,
        response.selection.selected_reducers.join(","),
        response.totals.est_tokens,
        response.totals.runtime_ms
    );

    for skipped in &response.selection.skipped {
        println!("skip {} ({})", skipped.reducer, skipped.reason);
    }

    for packet in &response.results.packets {
        let summary = packet
            .packet
            .get("packet")
            .and_then(|packet| packet.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or("no summary");
        println!("- {} [{}] {}", packet.reducer, packet.packet_type, summary);
    }

    if response.results.recall.hits.is_empty() {
        println!("recall: (no hits)");
    } else {
        println!("recall query: {}", response.results.recall.query);
        for hit in &response.results.recall.hits {
            let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let target = hit
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let summary = hit
                .get("summary")
                .and_then(Value::as_str)
                .or_else(|| hit.get("snippet").and_then(Value::as_str))
                .unwrap_or("no summary");
            println!("  - score={score:.3} target={target} {summary}");
        }
    }

    Ok(())
}

fn profile_recall_hits(hits: &[RecallHit], profile: suite_packet_core::JsonProfile) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            let mut value = serde_json::to_value(hit).unwrap_or(Value::Null);
            if matches!(
                profile,
                suite_packet_core::JsonProfile::Compact | suite_packet_core::JsonProfile::Handle
            ) {
                if let Some(map) = value.as_object_mut() {
                    map.remove("created_at_unix");
                    map.remove("matched_tokens");
                }
            }
            value
        })
        .collect()
}

fn cache_hit(metadata: &Value) -> bool {
    metadata
        .get("cache")
        .and_then(|cache| cache.get("hit"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn resolve_paths_relative_to_root(values: &[String], root: &Path) -> Vec<String> {
    values
        .iter()
        .map(|value| crate::cmd_common::resolve_path_from_cwd(value, root))
        .collect()
}

fn resolve_optional_path_relative_to_root(value: Option<&str>, root: &Path) -> Option<String> {
    value.map(|value| crate::cmd_common::resolve_path_from_cwd(value, root))
}

fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn resolve_git_refs(
    root: &Path,
    explicit_base: Option<&str>,
    explicit_head: Option<&str>,
    default_base: &str,
    default_head: &str,
) -> (String, String) {
    let head = explicit_head.unwrap_or(default_head).to_string();
    let base_candidate = explicit_base.unwrap_or(default_base);
    let base = if is_git_repo(root) {
        if git_ref_exists(root, base_candidate) {
            base_candidate.to_string()
        } else if git_ref_exists(root, "HEAD~1") {
            "HEAD~1".to_string()
        } else {
            "HEAD".to_string()
        }
    } else {
        base_candidate.to_string()
    };
    (base, head)
}

fn git_ref_exists(root: &Path, reference: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn clean_token(value: &str) -> String {
    value
        .trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | '.' | ':' | ';' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
            )
        })
        .to_string()
}

fn looks_like_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || [
            ".rs", ".java", ".kt", ".scala", ".py", ".js", ".jsx", ".ts", ".tsx", ".go", ".c",
            ".cc", ".cpp", ".h", ".hpp",
        ]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn looks_like_symbol(value: &str) -> bool {
    (value.contains("::") || value.contains('_') || value.contains('.'))
        || value
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
}

fn is_stopword(value: &str) -> bool {
    matches!(
        value,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "into"
            | "this"
            | "that"
            | "what"
            | "when"
            | "where"
            | "which"
            | "need"
            | "want"
            | "help"
            | "please"
            | "about"
            | "understand"
            | "investigate"
    )
}

fn skip(reducer: PreflightReducer, reason: &str) -> PreflightSkippedReducer {
    PreflightSkippedReducer {
        reducer: reducer.as_str().to_string(),
        reason: reason.to_string(),
    }
}

fn current_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args(task: &str) -> PreflightArgs {
        PreflightArgs {
            task: task.to_string(),
            root: ".".to_string(),
            task_id: None,
            base: None,
            head: None,
            budget_tokens: DEFAULT_PREFLIGHT_BUDGET_TOKENS,
            limit_recall: DEFAULT_PREFLIGHT_RECALL_LIMIT,
            focus_paths: Vec::new(),
            focus_symbols: Vec::new(),
            coverage: Vec::new(),
            stack_input: None,
            build_input: None,
            testmap: ".covy/state/testmap.bin".to_string(),
            include: Vec::new(),
            exclude: Vec::new(),
            json: Some(crate::cmd_common::JsonProfileArg::Compact),
            pretty: false,
        }
    }

    #[test]
    fn coverage_task_prefers_cover_diff_map_recall() {
        let plan = plan_selection(
            &base_args("fix coverage gap in FooService"),
            &Availability {
                has_cover: true,
                has_diff: true,
                has_map: true,
                has_recall: true,
                has_stack: false,
                has_build: false,
                has_impact: false,
            },
        );
        assert_eq!(
            plan.selected,
            vec![
                PreflightReducer::Cover,
                PreflightReducer::Diff,
                PreflightReducer::Map,
                PreflightReducer::Recall,
            ]
        );
        assert!(plan
            .anchors
            .symbols
            .iter()
            .any(|symbol| symbol == "FooService"));
    }

    #[test]
    fn generic_task_defaults_to_diff_map_recall() {
        let plan = plan_selection(
            &base_args("understand parser changes"),
            &Availability {
                has_cover: false,
                has_diff: true,
                has_map: true,
                has_recall: true,
                has_stack: false,
                has_build: false,
                has_impact: false,
            },
        );
        assert_eq!(
            plan.selected,
            vec![
                PreflightReducer::Diff,
                PreflightReducer::Map,
                PreflightReducer::Recall,
            ]
        );
    }

    #[test]
    fn unavailable_inputs_are_skipped() {
        let mut args = base_args("debug stack failure");
        args.include = vec![PreflightReducer::Stack, PreflightReducer::Build];
        let plan = plan_selection(
            &args,
            &Availability {
                has_cover: false,
                has_diff: true,
                has_map: true,
                has_recall: true,
                has_stack: false,
                has_build: false,
                has_impact: false,
            },
        );
        assert!(plan
            .skipped
            .iter()
            .any(|item| item.reducer == "stack" && item.reason == "no_stack_input"));
        assert!(plan
            .skipped
            .iter()
            .any(|item| item.reducer == "build" && item.reason == "no_build_input"));
    }

    #[test]
    fn exclude_wins_over_selection() {
        let mut args = base_args("fix coverage gap in FooService");
        args.exclude = vec![PreflightReducer::Map];
        let plan = plan_selection(
            &args,
            &Availability {
                has_cover: true,
                has_diff: true,
                has_map: true,
                has_recall: true,
                has_stack: false,
                has_build: false,
                has_impact: false,
            },
        );
        assert!(!plan.selected.contains(&PreflightReducer::Map));
        assert!(plan
            .skipped
            .iter()
            .any(|item| item.reducer == "map" && item.reason == "excluded"));
    }

    #[test]
    fn budget_trimming_is_deterministic() {
        let mut args = base_args("fix coverage gap in FooService");
        args.budget_tokens = 2_500;
        let plan = plan_selection(
            &args,
            &Availability {
                has_cover: true,
                has_diff: true,
                has_map: true,
                has_recall: true,
                has_stack: false,
                has_build: false,
                has_impact: false,
            },
        );
        assert_eq!(
            plan.selected,
            vec![PreflightReducer::Cover, PreflightReducer::Diff]
        );
        assert!(plan
            .skipped
            .iter()
            .any(|item| item.reducer == "map" && item.reason == "budget_trimmed"));
        assert!(plan
            .skipped
            .iter()
            .any(|item| item.reducer == "recall" && item.reason == "budget_trimmed"));
    }
}
