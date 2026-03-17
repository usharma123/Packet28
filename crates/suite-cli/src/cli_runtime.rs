use std::path::Path;

use anyhow::Result;
use clap::Parser;
use serde_json::{json, Value};

use crate::{
    cmd_agent_prompt, cmd_build, cmd_common, cmd_compact, cmd_context, cmd_cover, cmd_daemon,
    cmd_diff, cmd_discover, cmd_doctor, cmd_guard, cmd_hook, cmd_impact, cmd_learn, cmd_map,
    cmd_map_repo, cmd_mcp, cmd_packet, cmd_proxy, cmd_setup, cmd_shard, cmd_stack, BuildCommands,
    Cli, Commands, ContextCommands, CoverCommands, DiffCommands, GuardCommands, MapCommands,
    StackCommands, TestCommands,
};

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
                if let Err(emit_err) = cmd_common::emit_machine_error(
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
        Commands::Compact(args) => cmd_compact::run(args),
        Commands::Packet(packet) => match packet.command {
            cmd_packet::PacketCommands::Fetch(args) => cmd_packet::run_fetch(args),
        },
        Commands::AgentPrompt(args) => cmd_agent_prompt::run(args),
        Commands::Mcp(args) => cmd_mcp::run(args),
        Commands::Hook(args) => cmd_hook::run(args),
        Commands::Daemon(daemon) => cmd_daemon::run(daemon),
        Commands::Doctor(args) => cmd_doctor::run(args),
        Commands::Setup(args) => cmd_setup::run(args),
        Commands::Discover(args) => cmd_discover::run(args),
        Commands::Learn(args) => cmd_learn::run(args),
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
        Commands::Compact(_) | Commands::Discover(_) | Commands::Learn(_) => None,
        Commands::Daemon(_)
        | Commands::Guard(_)
        | Commands::AgentPrompt(_)
        | Commands::Mcp(_)
        | Commands::Hook(_)
        | Commands::Setup(_)
        | Commands::Doctor(_) => None,
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
