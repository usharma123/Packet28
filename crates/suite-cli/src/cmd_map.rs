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

pub fn run(args: MapArgs) -> Result<i32> {
    testy_cli_common::testmap::run_testmap_build(
        testy_cli_common::testmap::TestmapBuildArgs {
            manifest: args.manifest,
            output: args.output,
            timings_output: args.timings_output,
            json: args.json,
            schema: args.schema,
        },
        &testy_cli_common::testmap::TestmapRunnerOptions::default(),
    )
}

pub fn run_remote(args: MapArgs) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let response = crate::cmd_daemon::execute_test_map(
        &cwd,
        packet28_daemon_core::TestMapRequest {
            manifest: args.manifest,
            output: args.output,
            timings_output: args.timings_output,
            schema: args.schema,
        },
    )?;

    if let Some(schema) = response.schema {
        println!("{schema}");
        return Ok(0);
    }

    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }

    let summary = response
        .summary
        .ok_or_else(|| anyhow::anyhow!("daemon returned no testmap summary"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "Built testmap from {} manifest records across {} file(s)",
            summary.records, summary.manifest_files
        );
    }
    Ok(0)
}
