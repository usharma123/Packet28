use anyhow::Result;

pub type TestmapArgs = testy_cli_common::testmap::TestmapArgs;

const TESTMAP_RUNNER_OPTIONS: testy_cli_common::testmap::TestmapRunnerOptions =
    testy_cli_common::testmap::TestmapRunnerOptions {
        emit_warning,
        emit_text,
    };

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    testy_cli_common::testmap::run_testmap_command(args, &TESTMAP_RUNNER_OPTIONS)
}

fn emit_warning(message: &str) {
    tracing::warn!("{message}");
}

fn emit_text(message: &str) {
    tracing::info!("{message}");
}
