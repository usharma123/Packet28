use anyhow::Result;
use clap::{Args, Subcommand};

#[cfg(not(unix))]
use crate::cmd_daemon_client::daemon_not_supported;
#[cfg(unix)]
pub(crate) use crate::cmd_daemon_client::subscribe_task;
pub use crate::cmd_daemon_client::{
    daemon_root_env, daemon_workspace_root, execute_context_recall, execute_context_store_get,
    execute_context_store_list, execute_context_store_prune, execute_context_store_stats,
    execute_cover_check, execute_kernel_request, execute_packet_fetch, execute_sequence,
    execute_test_map, execute_test_shard, send_cover_check, send_kernel_request, send_packet_fetch,
    send_request, via_daemon_env_enabled, PersistentDaemonClient,
};
pub(crate) use crate::cmd_daemon_client::{ensure_daemon, resolve_root_arg, restart_daemon};
pub(crate) use crate::cmd_daemon_commands::{
    run_index, run_start, run_status, run_stop, run_task, run_watch,
};

#[derive(Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommands,
}

#[derive(Subcommand)]
pub enum DaemonCommands {
    Start(StatusRootArgs),
    Stop(StatusRootArgs),
    Status(JsonRootArgs),
    Task(TaskArgs),
    Watch(WatchArgs),
    Index(IndexArgs),
}

#[derive(Args)]
pub struct StatusRootArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
}

#[derive(Args)]
pub struct JsonRootArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommands,
}

#[derive(Subcommand)]
pub enum TaskCommands {
    Submit(TaskSubmitArgs),
    Status(TaskStatusArgs),
    AwaitHandoff(TaskAwaitHandoffArgs),
    LaunchAgent(TaskLaunchAgentArgs),
    Cancel(TaskCancelArgs),
    Watch(TaskWatchArgs),
}

#[derive(Args)]
pub struct TaskSubmitArgs {
    #[arg(long)]
    pub spec: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskStatusArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskAwaitHandoffArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub after_context_version: Option<String>,
    #[arg(long, default_value_t = 300_000)]
    pub timeout_ms: u64,
    #[arg(long, default_value_t = 250)]
    pub poll_ms: u64,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
#[command(trailing_var_arg = true)]
pub struct TaskLaunchAgentArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long, default_value_t = false)]
    pub wait_for_handoff: bool,
    #[arg(long, default_value_t = 300_000)]
    pub handoff_timeout_ms: u64,
    #[arg(long, default_value_t = 250)]
    pub handoff_poll_ms: u64,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
    #[arg(required = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct TaskCancelArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskWatchArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long, default_value_t = 0)]
    pub replay_last: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct WatchArgs {
    #[command(subcommand)]
    pub command: WatchCommands,
}

#[derive(Args)]
pub struct IndexArgs {
    #[command(subcommand)]
    pub command: IndexCommands,
}

#[derive(Subcommand)]
pub enum IndexCommands {
    Status(JsonRootArgs),
    Rebuild(IndexRebuildArgs),
    Clear(JsonRootArgs),
}

#[derive(Args)]
pub struct IndexRebuildArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub full: bool,
    #[arg(long = "path")]
    pub paths: Vec<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Subcommand)]
pub enum WatchCommands {
    List(WatchListArgs),
    Remove(WatchRemoveArgs),
}

#[derive(Args)]
pub struct WatchListArgs {
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct WatchRemoveArgs {
    #[arg(long)]
    pub watch_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[cfg(unix)]
pub fn run(args: DaemonArgs) -> Result<i32> {
    match args.command {
        DaemonCommands::Start(args) => run_start(args),
        DaemonCommands::Stop(args) => run_stop(args),
        DaemonCommands::Status(args) => run_status(args),
        DaemonCommands::Task(args) => run_task(args),
        DaemonCommands::Watch(args) => run_watch(args),
        DaemonCommands::Index(args) => run_index(args),
    }
}

#[cfg(not(unix))]
pub fn run(_args: DaemonArgs) -> Result<i32> {
    daemon_not_supported()
}

#[cfg(unix)]
pub fn run_via_daemon(cli: crate::Cli, _raw_args: &[String]) -> Result<i32> {
    let daemon_root = daemon_workspace_root(cli.daemon_root.as_deref())?;
    match cli.command {
        crate::Commands::Cover(cover) => match cover.command {
            crate::CoverCommands::Check(args) => {
                crate::cmd_cover::run_remote(args, &cli.config, &daemon_root)
            }
        },
        crate::Commands::Diff(diff) => match diff.command {
            crate::DiffCommands::Analyze(args) => {
                crate::cmd_diff::run_remote(args, &cli.config, &daemon_root)
            }
        },
        crate::Commands::Test(test) => match test.command {
            crate::TestCommands::Impact(args) => {
                crate::cmd_impact::run_remote(args, &cli.config, &daemon_root)
            }
            crate::TestCommands::Shard(args) => {
                crate::cmd_shard::run_remote(args, &cli.config, &daemon_root)
            }
            crate::TestCommands::Map(args) => crate::cmd_map::run_remote(args, &daemon_root),
        },
        crate::Commands::Context(context) => match context.command {
            crate::ContextCommands::Assemble(args) => {
                crate::cmd_context::run_assemble_remote(args, &daemon_root)
            }
            crate::ContextCommands::Correlate(args) => {
                crate::cmd_context::run_correlate_remote(args, &daemon_root)
            }
            crate::ContextCommands::Manage(args) => {
                crate::cmd_context::run_manage_remote(args, &daemon_root)
            }
            crate::ContextCommands::State(args) => {
                crate::cmd_context::run_state_remote(args, &daemon_root)
            }
            crate::ContextCommands::Store(args) => {
                crate::cmd_context::run_store_remote(args, &daemon_root)
            }
            crate::ContextCommands::Recall(args) => {
                crate::cmd_context::run_recall_remote(args, &daemon_root)
            }
        },
        crate::Commands::Stack(stack) => match stack.command {
            crate::StackCommands::Slice(args) => crate::cmd_stack::run_remote(args, &daemon_root),
        },
        crate::Commands::Build(build) => match build.command {
            crate::BuildCommands::Reduce(args) => crate::cmd_build::run_remote(args, &daemon_root),
        },
        crate::Commands::Map(map) => match map.command {
            crate::MapCommands::Repo(args) => crate::cmd_map_repo::run_remote(args, &daemon_root),
        },
        crate::Commands::Proxy(proxy) => match proxy.command {
            crate::cmd_proxy::ProxyCommands::Run(args) => {
                crate::cmd_proxy::run_remote(args, &daemon_root)
            }
        },
        crate::Commands::Compact(args) => crate::cmd_compact::run(args),
        crate::Commands::Packet(packet) => match packet.command {
            crate::cmd_packet::PacketCommands::Fetch(args) => {
                crate::cmd_packet::run_fetch_remote(args, &daemon_root)
            }
        },
        other => {
            let cli = crate::Cli {
                command: other,
                via_daemon: false,
                ..cli
            };
            crate::run_cli_local(cli)
        }
    }
}

#[cfg(not(unix))]
pub fn run_via_daemon(_cli: crate::Cli, _raw_args: &[String]) -> Result<i32> {
    daemon_not_supported()
}
