use std::path::Path;

use anyhow::Result;
use clap::Args;
use suite_foundation_core::CovyConfig;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PlannerAlgorithmArg {
    #[value(name = "lpt")]
    Lpt,
    #[value(name = "whale-lpt")]
    WhaleLpt,
}

#[derive(Args)]
pub struct ShardArgs {
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

    if args.schema {
        println!("{SHARD_PLAN_SCHEMA_EXAMPLES}");
        return Ok(0);
    }

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
        .ok_or_else(|| anyhow::anyhow!("shard response missing shard plan"))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&shard_plan)?);
    } else {
        render_text(&shard_plan);
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
