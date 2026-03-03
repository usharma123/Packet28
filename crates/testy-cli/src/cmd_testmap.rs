use anyhow::Result;

pub type TestmapArgs = testy_cli_common::testmap::TestmapArgs;

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    testy_cli_common::testmap::run_testmap_command(
        args,
        &testy_cli_common::testmap::TestmapRunnerOptions::default(),
    )
}
