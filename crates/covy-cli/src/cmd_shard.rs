use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};
use covy_core::CovyConfig;

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

impl From<JunitIdGranularityArg> for covy_core::shard_timing::JunitIdGranularity {
    fn from(value: JunitIdGranularityArg) -> Self {
        match value {
            JunitIdGranularityArg::Method => covy_core::shard_timing::JunitIdGranularity::Method,
            JunitIdGranularityArg::Class => covy_core::shard_timing::JunitIdGranularity::Class,
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

pub fn run(args: ShardArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    match args.command {
        ShardCommands::Plan(plan) => {
            if plan.schema {
                println!("{SHARD_PLAN_SCHEMA_EXAMPLES}");
                return Ok(0);
            }
            let shard_count = plan
                .shards
                .ok_or_else(|| anyhow::anyhow!("--shards is required"))?;
            let timings_path = plan
                .timings
                .as_deref()
                .unwrap_or(&config.shard.timings_path)
                .to_string();
            let unknown_seconds = plan
                .unknown_test_seconds
                .unwrap_or(config.shard.unknown_test_seconds);
            let algorithm = resolve_plan_algorithm(&plan, &config)?;

            let response =
                covy_core::shard_pipeline::run_shard(covy_core::shard_pipeline::ShardRequest {
                    mode: covy_core::shard_pipeline::ShardMode::Plan(
                        covy_core::shard_pipeline::ShardPlanRequest {
                            shard_count,
                            tasks_json: plan.tasks_json,
                            tests_file: plan.tests_file,
                            impact_json: plan.impact_json,
                            tier: plan.tier,
                            include_tag: plan.include_tag,
                            exclude_tag: plan.exclude_tag,
                            tier_exclude_tags_pr: config.shard.tiers.pr.exclude_tags,
                            tier_exclude_tags_nightly: config.shard.tiers.nightly.exclude_tags,
                            timings_path,
                            unknown_test_seconds: unknown_seconds,
                            algorithm: to_core_algorithm(algorithm),
                            write_files: plan.write_files,
                        },
                    ),
                })?;

            let shard_plan = response
                .shard_plan
                .ok_or_else(|| anyhow::anyhow!("shard plan response missing shard plan"))?;

            if plan.json {
                println!("{}", serde_json::to_string_pretty(&shard_plan)?);
            } else {
                render_text(&shard_plan);
            }

            Ok(0)
        }
        ShardCommands::Update(update) => {
            let timings_path = update
                .timings
                .as_deref()
                .unwrap_or(&config.shard.timings_path)
                .to_string();

            let response =
                covy_core::shard_pipeline::run_shard(covy_core::shard_pipeline::ShardRequest {
                    mode: covy_core::shard_pipeline::ShardMode::Update(
                        covy_core::shard_pipeline::ShardUpdateRequest {
                            junit_xml: update.junit_xml,
                            timings_jsonl: update.timings_jsonl,
                            timings_path,
                            export_json: update.export_json,
                            junit_id_granularity: update.junit_id_granularity.into(),
                        },
                    ),
                })?;

            let summary = response
                .timing_summary
                .ok_or_else(|| anyhow::anyhow!("shard update response missing timing summary"))?;
            if update.json {
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
    }
}

fn to_core_algorithm(
    value: PlannerAlgorithmArg,
) -> covy_core::shard_pipeline::ShardPlannerAlgorithm {
    match value {
        PlannerAlgorithmArg::Lpt => covy_core::shard_pipeline::ShardPlannerAlgorithm::Lpt,
        PlannerAlgorithmArg::WhaleLpt => covy_core::shard_pipeline::ShardPlannerAlgorithm::WhaleLpt,
    }
}

fn resolve_plan_algorithm(
    plan: &ShardPlanArgs,
    config: &CovyConfig,
) -> Result<PlannerAlgorithmArg> {
    if let Some(algorithm) = plan.algorithm {
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

fn render_text(plan: &covy_core::shard::ShardPlan) {
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
        let args = ShardPlanArgs {
            shards: Some(1),
            tasks_json: None,
            tier: "nightly".to_string(),
            include_tag: Vec::new(),
            exclude_tag: Vec::new(),
            tests_file: None,
            impact_json: None,
            timings: None,
            unknown_test_seconds: None,
            algorithm: Some(PlannerAlgorithmArg::WhaleLpt),
            json: false,
            write_files: None,
            schema: false,
        };
        let cfg = CovyConfig::default();
        let resolved = resolve_plan_algorithm(&args, &cfg).unwrap();
        assert!(matches!(resolved, PlannerAlgorithmArg::WhaleLpt));
    }

    #[test]
    fn test_resolve_plan_algorithm_rejects_invalid_config() {
        let args = ShardPlanArgs {
            shards: Some(1),
            tasks_json: None,
            tier: "nightly".to_string(),
            include_tag: Vec::new(),
            exclude_tag: Vec::new(),
            tests_file: None,
            impact_json: None,
            timings: None,
            unknown_test_seconds: None,
            algorithm: None,
            json: false,
            write_files: None,
            schema: false,
        };
        let mut cfg = CovyConfig::default();
        cfg.shard.algorithm = "bad".to_string();
        let err = resolve_plan_algorithm(&args, &cfg).unwrap_err();
        assert!(err.to_string().contains("Unsupported shard algorithm"));
    }
}
