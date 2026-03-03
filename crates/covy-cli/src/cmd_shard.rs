use anyhow::Result;

pub type ShardArgs = testy_cli_common::shard::ShardArgs;

pub fn run(args: ShardArgs, config_path: &str) -> Result<i32> {
    testy_cli_common::shard::run_shard_command(args, config_path)
}
