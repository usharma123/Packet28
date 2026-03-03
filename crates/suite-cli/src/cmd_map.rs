use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct MapArgs {
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
struct MapSummary {
    manifest_files: usize,
    records: usize,
    tests: usize,
    files: usize,
    output_testmap_path: String,
    output_timings_path: String,
}

pub fn run(args: MapArgs) -> Result<i32> {
    if args.schema {
        println!(
            "{}",
            testy_core::pipeline_testmap::TESTMAP_MANIFEST_SCHEMA_EXAMPLE
        );
        return Ok(0);
    }

    let adapters = crate::cmd_common::default_testmap_adapters();
    let response = testy_core::pipeline_testmap::run_testmap(
        testy_core::pipeline_testmap::TestMapRequest {
            manifest_globs: args.manifest,
            output_testmap_path: args.output,
            output_timings_path: args.timings_output,
        },
        &adapters,
    )?;

    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }

    if args.json {
        let summary = MapSummary {
            manifest_files: response.stats.manifest_files,
            records: response.stats.records,
            tests: response.stats.tests,
            files: response.stats.files,
            output_testmap_path: response.output_testmap_path,
            output_timings_path: response.output_timings_path,
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "Built testmap from {} manifest records across {} file(s)",
            response.stats.records, response.stats.manifest_files
        );
    }

    Ok(0)
}
