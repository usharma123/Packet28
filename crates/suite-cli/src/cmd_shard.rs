use anyhow::Result;
use clap::Args;

pub use testy_cli_common::shard::PlannerAlgorithmArg;

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

pub fn run(args: ShardArgs, config_path: &str) -> Result<i32> {
    testy_cli_common::shard::run_shard_plan_command(
        testy_cli_common::shard::ShardPlanArgs {
            shards: args.shards,
            tasks_json: args.tasks_json,
            tier: args.tier,
            include_tag: args.include_tag,
            exclude_tag: args.exclude_tag,
            tests_file: args.tests_file,
            impact_json: args.impact_json,
            timings: args.timings,
            unknown_test_seconds: args.unknown_test_seconds,
            algorithm: args.algorithm,
            json: args.json,
            write_files: args.write_files,
            schema: args.schema,
        },
        config_path,
    )
}
