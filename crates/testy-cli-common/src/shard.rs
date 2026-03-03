use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PlannerAlgorithmArg {
    #[value(name = "lpt")]
    Lpt,
    #[value(name = "whale-lpt")]
    WhaleLpt,
}

impl From<PlannerAlgorithmArg> for testy_core::command_shard::PlannerAlgorithmArg {
    fn from(value: PlannerAlgorithmArg) -> Self {
        match value {
            PlannerAlgorithmArg::Lpt => testy_core::command_shard::PlannerAlgorithmArg::Lpt,
            PlannerAlgorithmArg::WhaleLpt => {
                testy_core::command_shard::PlannerAlgorithmArg::WhaleLpt
            }
        }
    }
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

pub fn run_shard_command(args: ShardArgs, config_path: &str) -> Result<i32> {
    match args.command {
        ShardCommands::Plan(plan) => run_shard_plan_command(plan, config_path),
        ShardCommands::Update(update) => run_shard_update_command(update, config_path),
    }
}

pub fn run_shard_plan_command(args: ShardPlanArgs, config_path: &str) -> Result<i32> {
    if args.schema {
        println!("{}", testy_core::command_shard::SHARD_PLAN_SCHEMA_EXAMPLES);
        return Ok(0);
    }

    let shard_plan = testy_core::command_shard::run_shard_plan_command(
        testy_core::command_shard::ShardPlanArgs {
            shards: args.shards,
            tasks_json: args.tasks_json,
            tier: args.tier,
            include_tag: args.include_tag,
            exclude_tag: args.exclude_tag,
            tests_file: args.tests_file,
            impact_json: args.impact_json,
            timings: args.timings,
            unknown_test_seconds: args.unknown_test_seconds,
            algorithm: args.algorithm.map(Into::into),
            write_files: args.write_files,
        },
        config_path,
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&shard_plan)?);
    } else {
        print!("{}", testy_core::command_shard::render_text(&shard_plan));
    }

    Ok(0)
}

pub fn run_shard_update_command(args: ShardUpdateArgs, config_path: &str) -> Result<i32> {
    let summary = testy_core::command_shard::run_shard_update_command(
        testy_core::command_shard::ShardUpdateArgs {
            junit_xml: args.junit_xml,
            timings_jsonl: args.timings_jsonl,
            timings: args.timings,
            export_json: args.export_json,
            junit_id_granularity: args.junit_id_granularity.into(),
        },
        config_path,
    )?;

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
