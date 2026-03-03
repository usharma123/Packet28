use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};
use suite_foundation_core::CovyConfig;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PlannerAlgorithmArg {
    #[value(name = "lpt")]
    Lpt,
    #[value(name = "whale-lpt")]
    WhaleLpt,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum JunitIdGranularityArg {
    #[value(name = "method")]
    Method,
    #[value(name = "class")]
    Class,
}

impl From<JunitIdGranularityArg> for testy_core::shard_timing::JunitIdGranularity {
    fn from(value: JunitIdGranularityArg) -> Self {
        match value {
            JunitIdGranularityArg::Method => testy_core::shard_timing::JunitIdGranularity::Method,
            JunitIdGranularityArg::Class => testy_core::shard_timing::JunitIdGranularity::Class,
        }
    }
}

#[derive(Args)]
pub struct ShardArgs {
    #[command(subcommand)]
    pub command: ShardCommands,
}

#[derive(Subcommand)]
pub enum ShardCommands {
    /// Plan test shards for CI runners
    Plan(ShardPlanArgs),
    /// Update timing history from runner timing artifacts
    Update(ShardUpdateArgs),
}

#[derive(Args)]
pub struct ShardPlanArgs {
    /// Number of shards
    #[arg(long, required_unless_present = "schema")]
    pub shards: Option<usize>,

    /// Input tasks.json file
    #[arg(long)]
    pub tasks_json: Option<String>,

    /// Planning tier (pr or nightly)
    #[arg(long, default_value = "nightly")]
    pub tier: String,

    /// Include only tasks with at least one of these tags (repeatable)
    #[arg(long)]
    pub include_tag: Vec<String>,

    /// Exclude tasks with any of these tags (repeatable)
    #[arg(long)]
    pub exclude_tag: Vec<String>,

    /// Input tests file
    #[arg(long)]
    pub tests_file: Option<String>,

    /// Impact JSON output file (selected_tests field)
    #[arg(long)]
    pub impact_json: Option<String>,

    /// Timing history path
    #[arg(long)]
    pub timings: Option<String>,

    /// Fallback duration (seconds) for unknown tests
    #[arg(long)]
    pub unknown_test_seconds: Option<f64>,

    /// Planning algorithm (lpt or whale-lpt)
    #[arg(long, value_enum)]
    pub algorithm: Option<PlannerAlgorithmArg>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Directory for shard output files
    #[arg(long)]
    pub write_files: Option<String>,

    /// Print input schema/examples and exit
    #[arg(long)]
    pub schema: bool,
}

#[derive(Args)]
pub struct ShardUpdateArgs {
    /// JUnit XML timing inputs (supports globs)
    #[arg(long)]
    pub junit_xml: Vec<String>,

    /// Generic timing JSONL inputs (supports globs)
    #[arg(long)]
    pub timings_jsonl: Vec<String>,

    /// Timing history path
    #[arg(long)]
    pub timings: Option<String>,

    /// Optional JSON export path for the merged timings snapshot
    #[arg(long)]
    pub export_json: Option<String>,

    /// JUnit test id granularity (method or class)
    #[arg(long, value_enum, default_value = "method")]
    pub junit_id_granularity: JunitIdGranularityArg,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

const SHARD_PLAN_SCHEMA_EXAMPLES: &str = r#"{
  "type": "shard-plan-input-schemas",
  "tasks_json": {
    "schema_version": 1,
    "tasks": [
      {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1200, "tags": ["unit"]}
    ]
  },
  "impact_json": {
    "selected_tests": ["com.foo.BarTest", "tests/test_mod.py::test_one"],
    "smoke_tests": [],
    "missing_mappings": [],
    "stale": false,
    "confidence": 1.0,
    "escalate_full_suite": false
  }
}"#;

pub fn run_shard_command(args: ShardArgs, config_path: &str) -> Result<i32> {
    match args.command {
        ShardCommands::Plan(plan) => run_shard_plan_command(plan, config_path),
        ShardCommands::Update(update) => run_shard_update_command(update, config_path),
    }
}

pub fn run_shard_plan_command(args: ShardPlanArgs, config_path: &str) -> Result<i32> {
    if args.schema {
        println!("{SHARD_PLAN_SCHEMA_EXAMPLES}");
        return Ok(0);
    }

    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let shard_count = args
        .shards
        .ok_or_else(|| anyhow::anyhow!("--shards is required"))?;
    let timings_path = args
        .timings
        .as_deref()
        .unwrap_or(&config.shard.timings_path)
        .to_string();
    let unknown_seconds = args
        .unknown_test_seconds
        .unwrap_or(config.shard.unknown_test_seconds);
    let algorithm = resolve_plan_algorithm(args.algorithm, &config)?;

    let response =
        testy_core::pipeline_shard::run_shard(testy_core::pipeline_shard::ShardRequest {
            mode: testy_core::pipeline_shard::ShardMode::Plan(
                testy_core::pipeline_shard::ShardPlanRequest {
                    shard_count,
                    tasks_json: args.tasks_json,
                    tests_file: args.tests_file,
                    impact_json: args.impact_json,
                    tier: args.tier,
                    include_tag: args.include_tag,
                    exclude_tag: args.exclude_tag,
                    tier_exclude_tags_pr: config.shard.tiers.pr.exclude_tags,
                    tier_exclude_tags_nightly: config.shard.tiers.nightly.exclude_tags,
                    timings_path,
                    unknown_test_seconds: unknown_seconds,
                    algorithm: to_core_algorithm(algorithm),
                    write_files: args.write_files,
                },
            ),
        })?;

    let shard_plan = response
        .shard_plan
        .ok_or_else(|| anyhow::anyhow!("shard plan response missing shard plan"))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&shard_plan)?);
    } else {
        render_text(&shard_plan);
    }

    Ok(0)
}

pub fn run_shard_update_command(args: ShardUpdateArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let timings_path = args
        .timings
        .as_deref()
        .unwrap_or(&config.shard.timings_path)
        .to_string();

    let response =
        testy_core::pipeline_shard::run_shard(testy_core::pipeline_shard::ShardRequest {
            mode: testy_core::pipeline_shard::ShardMode::Update(
                testy_core::pipeline_shard::ShardUpdateRequest {
                    junit_xml: args.junit_xml,
                    timings_jsonl: args.timings_jsonl,
                    timings_path,
                    export_json: args.export_json,
                    junit_id_granularity: args.junit_id_granularity.into(),
                },
            ),
        })?;

    let summary = response
        .timing_summary
        .ok_or_else(|| anyhow::anyhow!("shard update response missing timing summary"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "timings updated: observations={} tests_updated={} timings_path={}",
            summary.observations_ingested, summary.tests_updated, summary.timings_path
        );
        if let Some(path) = &summary.exported_json {
            println!("timings exported: {path}");
        }
    }

    Ok(0)
}

fn to_core_algorithm(
    value: PlannerAlgorithmArg,
) -> testy_core::pipeline_shard::ShardPlannerAlgorithm {
    match value {
        PlannerAlgorithmArg::Lpt => testy_core::pipeline_shard::ShardPlannerAlgorithm::Lpt,
        PlannerAlgorithmArg::WhaleLpt => {
            testy_core::pipeline_shard::ShardPlannerAlgorithm::WhaleLpt
        }
    }
}

fn resolve_plan_algorithm(
    cli_algorithm: Option<PlannerAlgorithmArg>,
    config: &CovyConfig,
) -> Result<PlannerAlgorithmArg> {
    if let Some(algorithm) = cli_algorithm {
        return Ok(algorithm);
    }

    let configured = config.shard.algorithm.trim();
    if configured.is_empty() {
        return Ok(PlannerAlgorithmArg::Lpt);
    }

    match configured.to_ascii_lowercase().as_str() {
        "lpt" => Ok(PlannerAlgorithmArg::Lpt),
        "whale-lpt" => Ok(PlannerAlgorithmArg::WhaleLpt),
        _ => anyhow::bail!(
            "Unsupported shard algorithm '{}'. Expected 'lpt' or 'whale-lpt'",
            configured
        ),
    }
}

fn render_text(plan: &testy_core::shard::ShardPlan) {
    println!(
        "shards={} total_ms={} makespan_ms={} imbalance_ratio={:.3} parallel_efficiency={:.3} whale_count={} top_10_share={:.3}",
        plan.shards.len(),
        plan.total_predicted_duration_ms,
        plan.makespan_ms,
        plan.imbalance_ratio,
        plan.parallel_efficiency,
        plan.whale_count,
        plan.top_10_share
    );
    for shard in &plan.shards {
        println!(
            "shard={} tests={} predicted_ms={}",
            shard.id + 1,
            shard.tests.len(),
            shard.predicted_duration_ms
        );
        for test in &shard.tests {
            println!("{test}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_plan_algorithm_prefers_cli_flag() {
        let cfg = CovyConfig::default();
        let resolved = resolve_plan_algorithm(Some(PlannerAlgorithmArg::WhaleLpt), &cfg).unwrap();
        assert!(matches!(resolved, PlannerAlgorithmArg::WhaleLpt));
    }

    #[test]
    fn test_resolve_plan_algorithm_rejects_invalid_config() {
        let mut cfg = CovyConfig::default();
        cfg.shard.algorithm = "bad".to_string();
        let err = resolve_plan_algorithm(None, &cfg).unwrap_err();
        assert!(err.to_string().contains("Unsupported shard algorithm"));
    }
}
