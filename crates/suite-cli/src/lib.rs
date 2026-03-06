pub mod cmd_build;
pub mod cmd_common;
pub mod cmd_context;
pub mod cmd_cover;
pub mod cmd_daemon;
pub mod cmd_diff;
pub mod cmd_guard;
pub mod cmd_impact;
pub mod cmd_map;
pub mod cmd_map_repo;
pub mod cmd_packet;
pub mod cmd_proxy;
pub mod cmd_shard;
pub mod cmd_stack;

use std::path::Path;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "Packet28",
    version,
    about = "Umbrella platform CLI for suite domains",
    after_help = "Examples:\n  Packet28 diff analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --json\n  Packet28 context store stats --json\n  Packet28 context recall --query \"missing mappings in parser\" --json"
)]
pub struct Cli {
    /// Path to config file
    #[arg(long, global = true, default_value = "covy.toml")]
    pub config: String,

    /// Write stdout output to a file instead of the terminal
    #[arg(long)]
    pub output: Option<String>,

    /// Route supported command execution through packet28d
    #[arg(long, global = true)]
    pub via_daemon: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Coverage domain commands
    Cover(CoverArgs),
    /// Diff domain commands
    Diff(DiffArgs),
    /// Test domain commands
    Test(TestArgs),
    /// Guard/policy domain commands
    Guard(GuardArgs),
    /// Context assembly domain commands
    Context(ContextArgs),
    /// Stack trace / failure log reduction commands
    Stack(StackArgs),
    /// Build diagnostics reduction commands
    Build(BuildArgs),
    /// Repo mapping commands
    Map(MapArgs),
    /// Safe command proxy/reduction commands
    Proxy(cmd_proxy::ProxyArgs),
    /// Packet artifact utilities
    Packet(cmd_packet::PacketArgs),
    /// Daemon lifecycle and task commands
    Daemon(cmd_daemon::DaemonArgs),
}

#[derive(Args)]
pub struct CoverArgs {
    #[command(subcommand)]
    pub command: CoverCommands,
}

#[derive(Subcommand)]
pub enum CoverCommands {
    /// Analyze coverage quality gate
    Check(cmd_cover::CheckArgs),
}

#[derive(Args)]
pub struct DiffArgs {
    #[command(subcommand)]
    pub command: DiffCommands,
}

#[derive(Subcommand)]
pub enum DiffCommands {
    /// Analyze a git diff and evaluate quality gate
    Analyze(cmd_diff::AnalyzeArgs),
}

#[derive(Args)]
pub struct TestArgs {
    #[command(subcommand)]
    pub command: TestCommands,
}

#[derive(Subcommand)]
pub enum TestCommands {
    /// Compute impacted tests from a git diff
    Impact(cmd_impact::ImpactArgs),
    /// Plan test shard allocations
    Shard(cmd_shard::ShardArgs),
    /// Build test impact map artifacts
    Map(cmd_map::MapArgs),
}

#[derive(Args)]
pub struct GuardArgs {
    #[command(subcommand)]
    pub command: GuardCommands,
}

#[derive(Subcommand)]
pub enum GuardCommands {
    /// Validate guard policy config (context.yaml) shape and rule syntax
    Validate(cmd_guard::ValidateArgs),
    /// Evaluate one packet against guard policy config
    Check(cmd_guard::CheckArgs),
}

#[derive(Args)]
#[command(
    after_help = "Examples:\n  Packet28 context assemble --packet a.json --packet b.json --context-config context.yaml\n  Packet28 context store list --root . --limit 20\n  Packet28 context recall --query \"what changed in parser\" --limit 5"
)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub command: ContextCommands,
}

#[derive(Subcommand)]
pub enum ContextCommands {
    /// Merge multiple reducer packets into a bounded final packet
    #[command(alias = "merge")]
    Assemble(cmd_context::AssembleArgs),
    /// Correlate multiple packets into a synthesized insight packet
    Correlate(cmd_context::CorrelateArgs),
    /// Write and inspect agent task state
    State(cmd_context::StateArgs),
    /// Query and manage persisted context store entries
    Store(cmd_context::StoreArgs),
    /// Recall prior context entries by semantic/lexical query
    Recall(cmd_context::RecallArgs),
}

#[derive(Args)]
pub struct StackArgs {
    #[command(subcommand)]
    pub command: StackCommands,
}

#[derive(Subcommand)]
pub enum StackCommands {
    /// Parse stack traces/failing logs into deduped failure packets
    Slice(cmd_stack::SliceArgs),
}

#[derive(Args)]
pub struct BuildArgs {
    #[command(subcommand)]
    pub command: BuildCommands,
}

#[derive(Subcommand)]
pub enum BuildCommands {
    /// Parse compiler/linter output into deduped build diagnostic packets
    Reduce(cmd_build::ReduceArgs),
}

#[derive(Args)]
pub struct MapArgs {
    #[command(subcommand)]
    pub command: MapCommands,
}

#[derive(Subcommand)]
pub enum MapCommands {
    /// Build deterministic repo map packet
    Repo(cmd_map_repo::RepoArgs),
}

pub fn main_entry() {
    let raw_args = std::env::args().collect::<Vec<_>>();
    let cli = Cli::parse();
    if let Err(e) = configure_stdout_output(cli.output.as_deref()) {
        display_error(&e);
        std::process::exit(2);
    }

    let result = run_cli(cli, &raw_args);
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            display_error(&e);
            std::process::exit(2);
        }
    }
}

pub fn run_cli(mut cli: Cli, raw_args: &[String]) -> Result<i32> {
    if !matches!(cli.command, Commands::Daemon(_))
        && (cli.via_daemon || cmd_daemon::via_daemon_env_enabled())
    {
        cli.via_daemon = true;
        return cmd_daemon::run_via_daemon(cli, raw_args);
    }

    run_cli_local(cli)
}

pub fn run_cli_local(cli: Cli) -> Result<i32> {
    match cli.command {
        Commands::Cover(cover) => match cover.command {
            CoverCommands::Check(args) => cmd_cover::run(args, &cli.config),
        },
        Commands::Diff(diff) => match diff.command {
            DiffCommands::Analyze(args) => cmd_diff::run(args, &cli.config),
        },
        Commands::Test(test) => match test.command {
            TestCommands::Impact(args) => cmd_impact::run(args, &cli.config),
            TestCommands::Shard(args) => cmd_shard::run(args, &cli.config),
            TestCommands::Map(args) => cmd_map::run(args),
        },
        Commands::Guard(guard) => match guard.command {
            GuardCommands::Validate(args) => cmd_guard::run_validate(args, &cli.config),
            GuardCommands::Check(args) => cmd_guard::run_check(args, &cli.config),
        },
        Commands::Context(context) => match context.command {
            ContextCommands::Assemble(args) => cmd_context::run_assemble(args),
            ContextCommands::Correlate(args) => cmd_context::run_correlate(args),
            ContextCommands::State(args) => cmd_context::run_state(args),
            ContextCommands::Store(args) => cmd_context::run_store(args),
            ContextCommands::Recall(args) => cmd_context::run_recall(args),
        },
        Commands::Stack(stack) => match stack.command {
            StackCommands::Slice(args) => cmd_stack::run(args),
        },
        Commands::Build(build) => match build.command {
            BuildCommands::Reduce(args) => cmd_build::run(args),
        },
        Commands::Map(map) => match map.command {
            MapCommands::Repo(args) => cmd_map_repo::run(args),
        },
        Commands::Proxy(proxy) => match proxy.command {
            cmd_proxy::ProxyCommands::Run(args) => cmd_proxy::run(args),
        },
        Commands::Packet(packet) => match packet.command {
            cmd_packet::PacketCommands::Fetch(args) => cmd_packet::run_fetch(args),
        },
        Commands::Daemon(daemon) => cmd_daemon::run(daemon),
    }
}

pub fn display_error(err: &anyhow::Error) {
    use colored::Colorize;

    if let Some(covy_err) = err.downcast_ref::<suite_packet_core::CovyError>() {
        eprintln!("{} {covy_err}", "error:".red().bold());
        if let Some(hint) = covy_err.hint() {
            eprintln!("  {} {hint}", "hint:".cyan().bold());
        }
    } else {
        eprintln!("{} {err}", "error:".red().bold());
        for cause in err.chain().skip(1) {
            eprintln!("  {} {cause}", "caused by:".dimmed());
        }
    }
}

#[cfg(unix)]
fn configure_stdout_output(path: Option<&str>) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let Some(path) = path else {
        return Ok(());
    };

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(Path::new(path))?;

    let ret = unsafe { libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(())
}

#[cfg(not(unix))]
fn configure_stdout_output(path: Option<&str>) -> anyhow::Result<()> {
    if path.is_some() {
        anyhow::bail!("--output is currently supported only on Unix targets");
    }
    Ok(())
}
