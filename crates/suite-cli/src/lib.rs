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
use serde_json::{json, Value};

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

    /// Workspace root that owns the packet28d socket/runtime for routed commands
    #[arg(long, global = true)]
    pub daemon_root: Option<String>,

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
    after_help = "Examples:\n  Packet28 context assemble --packet a.json --packet b.json --context-config context.yaml\n  Packet28 context store list --root . --limit 20\n  Packet28 context recall --query \"what changed in parser\" --limit 5\n  Packet28 context manage --task-id task-123 --budget-tokens 4000 --budget-bytes 32000"
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
    /// Produce budget-aware task context management guidance
    Manage(cmd_context::ManageArgs),
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
    let machine_error = machine_error_context(&cli);
    if let Err(e) = configure_stdout_output(cli.output.as_deref()) {
        display_error(&e);
        std::process::exit(2);
    }

    let result = run_cli(cli, &raw_args);
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            if let Some(context) = machine_error {
                if let Err(emit_err) = crate::cmd_common::emit_machine_error(
                    &context.command,
                    &e,
                    context.pretty,
                    context.target.as_deref(),
                    context.retry_hint,
                ) {
                    display_error(&emit_err);
                }
                std::process::exit(2);
            }
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
            ContextCommands::Manage(args) => cmd_context::run_manage(args),
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

struct MachineErrorContext {
    command: String,
    pretty: bool,
    target: Option<String>,
    retry_hint: Option<Value>,
}

fn machine_error_context(cli: &Cli) -> Option<MachineErrorContext> {
    match &cli.command {
        Commands::Cover(cover) => match &cover.command {
            CoverCommands::Check(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 cover check".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("cover.check".to_string()),
                    retry_hint: None,
                })
            }
            _ => None,
        },
        Commands::Diff(diff) => match &diff.command {
            DiffCommands::Analyze(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 diff analyze".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("diffy.analyze".to_string()),
                    retry_hint: governed_retry_hint(
                        args.governed_requested(),
                        "Packet28 diff analyze --context-config <context.yaml>",
                    ),
                })
            }
            _ => None,
        },
        Commands::Test(test) => match &test.command {
            TestCommands::Impact(args) if args.json.is_some() || args.legacy_json => {
                Some(MachineErrorContext {
                    command: "Packet28 test impact".to_string(),
                    pretty: args.pretty,
                    target: Some("testy.impact".to_string()),
                    retry_hint: governed_retry_hint(
                        args.context_config.is_some(),
                        "Packet28 test impact --context-config <context.yaml>",
                    ),
                })
            }
            TestCommands::Shard(args) if args.json => Some(MachineErrorContext {
                command: "Packet28 test shard".to_string(),
                pretty: false,
                target: Some("testy.shard".to_string()),
                retry_hint: None,
            }),
            TestCommands::Map(args) if args.json => Some(MachineErrorContext {
                command: "Packet28 test map".to_string(),
                pretty: false,
                target: Some("testy.map".to_string()),
                retry_hint: None,
            }),
            _ => None,
        },
        Commands::Context(context) => match &context.command {
            ContextCommands::Assemble(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 context assemble".to_string(),
                    pretty: args.pretty_output(),
                    target: Some(if args.governed_requested() {
                        "governed.assemble".to_string()
                    } else {
                        "contextq.assemble".to_string()
                    }),
                    retry_hint: governed_retry_hint(
                        args.governed_requested(),
                        "Packet28 context assemble --context-config <context.yaml>",
                    ),
                })
            }
            ContextCommands::Correlate(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 context correlate".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("contextq.correlate".to_string()),
                    retry_hint: None,
                })
            }
            ContextCommands::Manage(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 context manage".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("contextq.manage".to_string()),
                    retry_hint: None,
                })
            }
            ContextCommands::State(state) => match &state.command {
                cmd_context::StateCommands::Append(args) if args.machine_output_requested() => {
                    Some(MachineErrorContext {
                        command: "Packet28 context state append".to_string(),
                        pretty: args.pretty_output(),
                        target: Some("agenty.state.write".to_string()),
                        retry_hint: None,
                    })
                }
                cmd_context::StateCommands::Snapshot(args) if args.machine_output_requested() => {
                    Some(MachineErrorContext {
                        command: "Packet28 context state snapshot".to_string(),
                        pretty: args.pretty_output(),
                        target: Some("agenty.state.snapshot".to_string()),
                        retry_hint: None,
                    })
                }
                _ => None,
            },
            ContextCommands::Store(store) => match &store.command {
                cmd_context::StoreCommands::List(args) if args.machine_output_requested() => {
                    Some(machine_error(
                        "Packet28 context store list",
                        args.pretty_output(),
                        "context.store.list",
                    ))
                }
                cmd_context::StoreCommands::Get(args) if args.machine_output_requested() => {
                    Some(machine_error(
                        "Packet28 context store get",
                        args.pretty_output(),
                        "context.store.get",
                    ))
                }
                cmd_context::StoreCommands::Prune(args) if args.machine_output_requested() => {
                    Some(machine_error(
                        "Packet28 context store prune",
                        args.pretty_output(),
                        "context.store.prune",
                    ))
                }
                cmd_context::StoreCommands::Stats(args) if args.machine_output_requested() => {
                    Some(machine_error(
                        "Packet28 context store stats",
                        args.pretty_output(),
                        "context.store.stats",
                    ))
                }
                _ => None,
            },
            ContextCommands::Recall(args) if args.machine_output_requested() => {
                Some(machine_error(
                    "Packet28 context recall",
                    args.pretty_output(),
                    "context.recall",
                ))
            }
            _ => None,
        },
        Commands::Stack(stack) => match &stack.command {
            StackCommands::Slice(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 stack slice".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("stacky.slice".to_string()),
                    retry_hint: governed_retry_hint(
                        args.governed_requested(),
                        "Packet28 stack slice --context-config <context.yaml>",
                    ),
                })
            }
            _ => None,
        },
        Commands::Build(build) => match &build.command {
            BuildCommands::Reduce(args) if args.machine_output_requested() => {
                Some(MachineErrorContext {
                    command: "Packet28 build reduce".to_string(),
                    pretty: args.pretty_output(),
                    target: Some("buildy.reduce".to_string()),
                    retry_hint: governed_retry_hint(
                        args.governed_requested(),
                        "Packet28 build reduce --context-config <context.yaml>",
                    ),
                })
            }
            _ => None,
        },
        Commands::Map(map) => match &map.command {
            MapCommands::Repo(args) if args.json.is_some() || args.legacy_json => {
                Some(MachineErrorContext {
                    command: "Packet28 map repo".to_string(),
                    pretty: args.pretty,
                    target: Some("mapy.repo".to_string()),
                    retry_hint: governed_retry_hint(
                        args.context_config.is_some(),
                        "Packet28 map repo --context-config <context.yaml>",
                    ),
                })
            }
            _ => None,
        },
        Commands::Proxy(proxy) => match &proxy.command {
            cmd_proxy::ProxyCommands::Run(args) if args.json.is_some() || args.legacy_json => {
                Some(MachineErrorContext {
                    command: "Packet28 proxy run".to_string(),
                    pretty: args.pretty,
                    target: Some("proxy.run".to_string()),
                    retry_hint: governed_retry_hint(
                        args.context_config.is_some(),
                        "Packet28 proxy run --context-config <context.yaml> -- <command>",
                    ),
                })
            }
            _ => None,
        },
        Commands::Packet(packet) => {
            match &packet.command {
                cmd_packet::PacketCommands::Fetch(args) if args.json.is_some() => Some(
                    machine_error("Packet28 packet fetch", args.pretty, "packet.fetch"),
                ),
                _ => None,
            }
        }
        Commands::Daemon(_) | Commands::Guard(_) => None,
    }
}

fn machine_error(command: &str, pretty: bool, target: &str) -> MachineErrorContext {
    MachineErrorContext {
        command: command.to_string(),
        pretty,
        target: Some(target.to_string()),
        retry_hint: None,
    }
}

fn governed_retry_hint(enabled: bool, command: &str) -> Option<Value> {
    enabled.then(|| {
        json!({
            "retry_command": command
        })
    })
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
