use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct TestmapArgs {
    #[command(subcommand)]
    pub command: TestmapCommands,
}

#[derive(Subcommand)]
pub enum TestmapCommands {
    /// Build test impact map artifacts
    Build(TestmapBuildArgs),
}

#[derive(Args)]
pub struct TestmapBuildArgs {
    /// Input manifest glob(s)
    #[arg(long)]
    pub manifest: Vec<String>,

    /// Output test map path
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub output: String,

    /// Output timing map path
    #[arg(long, default_value = ".covy/state/testtimings.bin")]
    pub timings_output: String,

    /// Emit JSON summary output
    #[arg(long)]
    pub json: bool,

    /// Print input schema/example and exit
    #[arg(long)]
    pub schema: bool,
}

#[derive(serde::Serialize)]
struct TestmapBuildSummary {
    manifest_files: usize,
    records: usize,
    tests: usize,
    files: usize,
    output_testmap_path: String,
    output_timings_path: String,
}

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    match args.command {
        TestmapCommands::Build(build) => run_build(build),
    }
}

fn run_build(build: TestmapBuildArgs) -> Result<i32> {
    if build.schema {
        println!(
            "{}",
            testy_core::pipeline_testmap::TESTMAP_MANIFEST_SCHEMA_EXAMPLE
        );
        return Ok(0);
    }

    let adapters = crate::cmd_common::default_testmap_adapters();
    let response = testy_core::pipeline_testmap::run_testmap(
        testy_core::pipeline_testmap::TestMapRequest {
            manifest_globs: build.manifest,
            output_testmap_path: build.output,
            output_timings_path: build.timings_output,
        },
        &adapters,
    )?;

    for warning in &response.warnings {
        tracing::warn!("{warning}");
    }

    if build.json {
        let summary = TestmapBuildSummary {
            manifest_files: response.stats.manifest_files,
            records: response.stats.records,
            tests: response.stats.tests,
            files: response.stats.files,
            output_testmap_path: response.output_testmap_path,
            output_timings_path: response.output_timings_path,
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        tracing::info!(
            "Built testmap from {} manifest records across {} file(s)",
            response.stats.records,
            response.stats.manifest_files
        );
    }

    Ok(0)
}
