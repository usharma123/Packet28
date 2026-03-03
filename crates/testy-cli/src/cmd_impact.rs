use anyhow::Result;

pub type ImpactArgs = testy_cli_common::impact::ImpactArgs;

const IMPACT_RUNNER_OPTIONS: testy_cli_common::impact::ImpactRunnerOptions =
    testy_cli_common::impact::ImpactRunnerOptions::for_binary("testy");

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    testy_cli_common::impact::run_impact_command(args, config_path, &IMPACT_RUNNER_OPTIONS)
}
