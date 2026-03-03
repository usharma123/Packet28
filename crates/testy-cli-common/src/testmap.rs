use anyhow::Result;
use clap::{Args, Subcommand};

use crate::adapters;

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

#[derive(Clone, Copy)]
pub struct TestmapRunnerOptions {
    pub emit_warning: fn(&str),
    pub emit_text: fn(&str),
}

impl Default for TestmapRunnerOptions {
    fn default() -> Self {
        Self {
            emit_warning: default_emit_warning,
            emit_text: default_emit_text,
        }
    }
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

pub fn run_testmap_command(args: TestmapArgs, options: &TestmapRunnerOptions) -> Result<i32> {
    match args.command {
        TestmapCommands::Build(build) => run_testmap_build(build, options),
    }
}

pub fn run_testmap_build(build: TestmapBuildArgs, options: &TestmapRunnerOptions) -> Result<i32> {
    if build.schema {
        println!(
            "{}",
            testy_core::pipeline_testmap::TESTMAP_MANIFEST_SCHEMA_EXAMPLE
        );
        return Ok(0);
    }

    let response = testy_core::pipeline_testmap::run_testmap(
        testy_core::pipeline_testmap::TestMapRequest {
            manifest_globs: build.manifest,
            output_testmap_path: build.output,
            output_timings_path: build.timings_output,
        },
        &adapters::default_testmap_adapters(),
    )?;

    for warning in &response.warnings {
        (options.emit_warning)(warning);
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
        (options.emit_text)(&format!(
            "Built testmap from {} manifest records across {} file(s)",
            response.stats.records, response.stats.manifest_files
        ));
    }

    Ok(0)
}

fn default_emit_warning(message: &str) {
    eprintln!("warning: {message}");
}

fn default_emit_text(message: &str) {
    println!("{message}");
}
