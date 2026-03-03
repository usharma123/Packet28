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
