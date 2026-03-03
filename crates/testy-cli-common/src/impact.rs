use anyhow::Result;
use clap::{Args, Subcommand};

use crate::adapters;
use crate::support;

#[derive(Debug, Clone, Copy)]
pub struct ImpactRunnerOptions {
    pub binary_name: &'static str,
    pub warn_on_legacy_mode: bool,
}

impl ImpactRunnerOptions {
    pub const fn for_binary(binary_name: &'static str) -> Self {
        Self {
            binary_name,
            warn_on_legacy_mode: true,
        }
    }
}

#[derive(Args)]
pub struct ImpactArgs {
    #[command(subcommand)]
    pub command: Option<ImpactCommand>,

    #[command(flatten)]
    pub legacy: LegacyImpactArgs,
}

#[derive(Subcommand)]
pub enum ImpactCommand {
    /// Build or update per-test impact map
    Record(ImpactRecordArgs),
    /// Plan tests for a git diff
    Plan(ImpactPlanArgs),
    /// Execute a previously generated impact plan
    Run(ImpactRunArgs),
}

#[derive(Args, Default)]
pub struct LegacyImpactArgs {
    /// Base ref for diff (default: main)
    #[arg(long)]
    pub base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    pub head: Option<String>,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Emit runnable test command
    #[arg(long)]
    pub print_command: bool,
}

#[derive(Args, Default)]
pub struct ImpactRecordArgs {
    /// Base ref used for metadata tagging (default: main)
    #[arg(long, default_value = "main")]
    pub base_ref: String,

    /// Output testmap path
    #[arg(
        long = "output",
        default_value = ".covy/state/testmap.bin",
        alias = "out"
    )]
    pub output: String,

    /// Directory containing per-test LCOV reports
    #[arg(long)]
    pub per_test_lcov_dir: Option<String>,

    /// Directory containing per-test JaCoCo reports
    #[arg(long)]
    pub per_test_jacoco_dir: Option<String>,

    /// Directory containing per-test Cobertura reports
    #[arg(long)]
    pub per_test_cobertura_dir: Option<String>,

    /// JSONL manifest with test_id + coverage_report(s)
    #[arg(long)]
    pub test_report: Option<String>,

    /// Optional summary json output path
    #[arg(long)]
    pub summary_json: Option<String>,

    /// Print input schema/example and exit
    #[arg(long)]
    pub schema: bool,
}

#[derive(Args, Default)]
pub struct ImpactPlanArgs {
    /// Base ref for diff
    #[arg(long, default_value = "origin/main")]
    pub base_ref: String,

    /// Head ref for diff
    #[arg(long, default_value = "HEAD")]
    pub head_ref: String,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Maximum number of tests to select
    #[arg(long)]
    pub max_tests: Option<usize>,

    /// Target changed-lines coverage as a ratio in [0,1]
    #[arg(long)]
    pub target_coverage: Option<f64>,

    /// Output format (json only for now)
    #[arg(long, default_value = "json")]
    pub format: String,
}

#[derive(Args, Default)]
pub struct ImpactRunArgs {
    /// Path to impact plan json
    #[arg(long)]
    pub plan: Option<String>,

    /// Print input schema/example and exit
    #[arg(long)]
    pub schema: bool,

    /// Command template to execute (provide after --)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run_impact_command(
    args: ImpactArgs,
    config_path: &str,
    options: &ImpactRunnerOptions,
) -> Result<i32> {
    match args.command {
        Some(ImpactCommand::Record(record)) => run_record(record),
        Some(ImpactCommand::Plan(plan)) => run_plan(plan, config_path),
        Some(ImpactCommand::Run(run)) => run_impact_run(run, options.binary_name),
        None => {
            if options.warn_on_legacy_mode {
                support::maybe_warn_deprecated(&format!(
                    "warning: `{} impact` legacy mode is deprecated; use `{} impact plan` and `{} impact run`.",
                    options.binary_name, options.binary_name, options.binary_name
                ));
            }
            run_legacy_impact(args.legacy, config_path)
        }
    }
}

pub fn run_legacy_impact(args: LegacyImpactArgs, config_path: &str) -> Result<i32> {
    let adapters = adapters::default_impact_adapters();
    let output = testy_core::command_impact::run_legacy_impact(
        testy_core::command_impact::LegacyImpactArgs {
            base: args.base,
            head: args.head,
            testmap: args.testmap,
            print_command: args.print_command,
        },
        config_path,
        &adapters,
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output.result)?);
        return Ok(0);
    }

    if output.result.selected_tests.is_empty() {
        println!("(no impacted tests)");
    } else {
        for test in &output.result.selected_tests {
            println!("{test}");
        }
    }
    println!(
        "summary: selected={} known={} missing={} confidence={:.2} stale={} escalate_full_suite={}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite
    );

    if args.print_command {
        if let Some(command) = output.print_command {
            println!("{command}");
        }
    }

    Ok(0)
}

fn run_record(args: ImpactRecordArgs) -> Result<i32> {
    support::warn_if_legacy_flag_used("--out", "--output");
    if args.schema {
        println!(
            "{}",
            testy_core::command_impact::IMPACT_RECORD_MANIFEST_EXAMPLE
        );
        return Ok(0);
    }

    let adapters = adapters::default_impact_adapters();
    let summary = testy_core::command_impact::run_record(
        testy_core::command_impact::ImpactRecordArgs {
            base_ref: args.base_ref,
            output: args.output,
            per_test_lcov_dir: args.per_test_lcov_dir,
            per_test_jacoco_dir: args.per_test_jacoco_dir,
            per_test_cobertura_dir: args.per_test_cobertura_dir,
            test_report: args.test_report,
            summary_json: args.summary_json,
        },
        &adapters,
    )?;

    println!(
        "Recorded testmap: tests={} files={} cells={} out={}",
        summary.tests_total, summary.files_total, summary.non_empty_cells, summary.output
    );
    Ok(0)
}

fn run_plan(args: ImpactPlanArgs, config_path: &str) -> Result<i32> {
    let adapters = adapters::default_impact_adapters();
    let plan = testy_core::command_impact::run_plan(
        testy_core::command_impact::ImpactPlanArgs {
            base_ref: args.base_ref,
            head_ref: args.head_ref,
            testmap: args.testmap,
            max_tests: args.max_tests,
            target_coverage: args.target_coverage,
            format: args.format,
        },
        config_path,
        &adapters,
    )?;

    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(0)
}

fn run_impact_run(args: ImpactRunArgs, binary_name: &str) -> Result<i32> {
    if args.schema {
        println!(
            "{}",
            testy_core::command_impact::impact_plan_example(binary_name)
        );
        return Ok(0);
    }

    let plan_path = args.plan.ok_or_else(|| {
        anyhow::anyhow!(
            "Missing --plan. Use: {binary_name} impact run --plan plan.json -- <command>"
        )
    })?;

    let outcome = testy_core::command_impact::run_impact_run(
        testy_core::command_impact::ImpactRunArgs {
            plan_path,
            command: args.command,
        },
        binary_name,
    )?;

    match outcome {
        testy_core::command_impact::ImpactRunOutcome::SkippedEmptyPlan => {
            println!("No tests selected in plan; skipping execution.");
            Ok(0)
        }
        testy_core::command_impact::ImpactRunOutcome::ExitCode(code) => Ok(code),
    }
}
