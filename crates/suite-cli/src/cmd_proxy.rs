use anyhow::{anyhow, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum PacketDetailArg {
    #[default]
    Compact,
    Rich,
}

impl From<PacketDetailArg> for suite_proxy_core::PacketDetail {
    fn from(value: PacketDetailArg) -> Self {
        match value {
            PacketDetailArg::Compact => suite_proxy_core::PacketDetail::Compact,
            PacketDetailArg::Rich => suite_proxy_core::PacketDetail::Rich,
        }
    }
}

#[derive(Args)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub command: ProxyCommands,
}

#[derive(clap::Subcommand)]
pub enum ProxyCommands {
    /// Run a safe shell command through deterministic proxy reduction
    #[command(alias = "exec")]
    Run(RunArgs),
}

#[derive(Args)]
pub struct RunArgs {
    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    pub legacy_json: bool,

    /// Packet detail level in JSON mode
    #[arg(long, value_enum, default_value_t = PacketDetailArg::Compact)]
    pub packet_detail: PacketDetailArg,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,

    /// Include kernel metadata/audit in JSON output
    #[arg(long)]
    pub debug: bool,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    pub cache: bool,

    /// Working directory for command execution
    #[arg(long)]
    pub cwd: Option<String>,

    /// Allowed env var names to pass through (PATH is always allowed)
    #[arg(long = "env", value_name = "VAR")]
    pub env_allowlist: Vec<String>,

    /// Maximum reduced output bytes
    #[arg(long)]
    pub max_output_bytes: Option<usize>,

    /// Maximum reduced output lines
    #[arg(long)]
    pub max_lines: Option<usize>,

    /// Maximum serialized packet bytes
    #[arg(long)]
    pub packet_byte_cap: Option<usize>,

    /// Run governed packet path using this context policy config (context.yaml)
    #[arg(long)]
    pub context_config: Option<String>,

    /// Context assembly token budget for governed mode
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub context_budget_bytes: usize,

    /// Command to execute (must be safe list)
    #[arg(required = true, trailing_var_arg = true)]
    pub command_argv: Vec<String>,
}

pub fn run(args: RunArgs) -> Result<i32> {
    let persist_root = persistence_root(&args)?;
    let machine_profile = args
        .json
        .map(|profile| suite_packet_core::JsonProfile::from(profile));
    let detail_mode = if args.packet_detail == PacketDetailArg::Rich {
        PacketDetailArg::Rich
    } else {
        PacketDetailArg::Compact
    };
    let input = suite_proxy_core::ProxyRunRequest {
        argv: args.command_argv,
        cwd: args.cwd,
        env_allowlist: args.env_allowlist,
        max_output_bytes: args.max_output_bytes,
        max_lines: args.max_lines,
        packet_byte_cap: args.packet_byte_cap,
        detail: detail_mode.into(),
    };

    let use_kernel = args.cache || args.context_config.is_some();
    if !use_kernel {
        let envelope = suite_proxy_core::run_and_reduce(input)?;
        if let Some(profile) = machine_profile {
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.proxy.run.v1",
                        "packet": envelope,
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_PROXY_RUN,
                    &envelope,
                    profile,
                    args.pretty,
                    &persist_root,
                    None,
                )?;
            }
        } else {
            print_text_summary(&envelope);
        }
        return Ok(if envelope.payload.exit_code == 0 {
            0
        } else {
            1
        });
    }

    let kernel = build_kernel(args.cache, persist_root.clone());
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "proxy.run".to_string(),
        reducer_input: serde_json::to_value(input)?,
        policy_context: args
            .context_config
            .as_ref()
            .map(|path| json!({"config_path": path}))
            .unwrap_or(Value::Null),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;

    let envelope: suite_packet_core::EnvelopeV1<suite_proxy_core::CommandSummaryPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid proxy output packet: {source}"))?;

    let governed_response = if let Some(context_config) = args.context_config {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![output_packet.clone()],
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(args.context_budget_tokens),
                byte_cap: Some(args.context_budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context: json!({
                "config_path": context_config,
                "detail_mode": match detail_mode {
                    PacketDetailArg::Rich => "rich",
                    PacketDetailArg::Compact => "compact",
                },
                "compact_assembly": detail_mode == PacketDetailArg::Compact,
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if let Some(profile) = machine_profile {
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                args.context_budget_tokens,
                args.context_budget_bytes,
                "Packet28 proxy run --context-config <context.yaml> -- <command>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            if args.legacy_json {
                if args.debug {
                    crate::cmd_common::emit_json(
                        &json!({
                            "schema_version": "suite.proxy.run.v1",
                            "packet": envelope,
                            "final_packet": final_packet.body,
                            "kernel_audit": {
                                "proxy": response.audit,
                                "governed": governed.audit,
                            },
                            "kernel_metadata": {
                                "proxy": response.metadata,
                                "governed": governed.metadata,
                            },
                            "cache": {
                                "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                                "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            },
                            "hints": {
                                "budget_retry": budget_hint,
                            },
                        }),
                        args.pretty,
                    )?;
                } else {
                    crate::cmd_common::emit_json(
                        &json!({
                            "schema_version": "suite.proxy.run.v1",
                            "packet": envelope,
                            "final_packet": final_packet.body,
                        }),
                        args.pretty,
                    )?;
                }
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_PROXY_RUN,
                    &envelope,
                    profile,
                    args.pretty,
                    &persist_root,
                    Some(json!({
                        "kernel_audit": {
                            "proxy": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "proxy": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                        "hints": {
                            "budget_retry": budget_hint,
                        },
                        "governed_packet": final_packet.body,
                    })),
                )?;
            }
        } else {
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.proxy.run.v1",
                        "packet": envelope,
                        "cache": {
                            "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_PROXY_RUN,
                    &envelope,
                    profile,
                    args.pretty,
                    &persist_root,
                    Some(json!({
                        "cache": {
                            "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    })),
                )?;
            }
        }

        return Ok(if envelope.payload.exit_code == 0 {
            0
        } else {
            1
        });
    }

    print_text_summary(&envelope);
    if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
        println!("{summary}");
    }

    if let Some(governed) = governed_response {
        if let Some(summary) = crate::cmd_common::cache_summary_line(&governed.metadata) {
            println!("{summary}");
        }
        if let Some(hint) = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            args.context_budget_tokens,
            args.context_budget_bytes,
            "Packet28 proxy run --context-config <context.yaml> -- <command>",
        ) {
            if let Some(retry) = hint.get("retry_command").and_then(Value::as_str) {
                println!("hint: high truncation detected; retry with: {retry}");
            }
        }
        let final_packet = governed
            .output_packets
            .first()
            .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
        let sections = final_packet
            .body
            .get("payload")
            .and_then(|payload| payload.get("assembly"))
            .and_then(|assembly| assembly.get("sections_kept"))
            .or_else(|| {
                final_packet
                    .body
                    .get("assembly")
                    .and_then(|assembly| assembly.get("sections_kept"))
            })
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        println!(
            "governed packet assembled: packet_id={} sections_kept={sections}",
            final_packet.packet_id.as_deref().unwrap_or("unknown")
        );
    }

    Ok(if envelope.payload.exit_code == 0 {
        0
    } else {
        1
    })
}

pub fn run_remote(args: RunArgs, daemon_root: &Path) -> Result<i32> {
    let persist_root = persistence_root(&args)?;
    let machine_profile = args
        .json
        .map(|profile| suite_packet_core::JsonProfile::from(profile));
    if machine_profile.is_none() || args.legacy_json {
        return run(args);
    }

    let detail_mode = if args.packet_detail == PacketDetailArg::Rich {
        PacketDetailArg::Rich
    } else {
        PacketDetailArg::Compact
    };
    let input = suite_proxy_core::ProxyRunRequest {
        argv: args.command_argv,
        cwd: args.cwd,
        env_allowlist: args.env_allowlist,
        max_output_bytes: args.max_output_bytes,
        max_lines: args.max_lines,
        packet_byte_cap: args.packet_byte_cap,
        detail: detail_mode.into(),
    };
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "proxy.run".to_string(),
            reducer_input: serde_json::to_value(input)?,
            policy_context: args
                .context_config
                .as_ref()
                .map(|path| json!({"config_path": path}))
                .unwrap_or(Value::Null),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;
    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_proxy_core::CommandSummaryPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid proxy output packet: {source}"))?;
    let governed_response = if let Some(context_config) = args.context_config {
        Some(crate::cmd_daemon::send_kernel_request(
            daemon_root,
            context_kernel_core::KernelRequest {
                target: "governed.assemble".to_string(),
                input_packets: vec![output_packet.clone()],
                budget: context_kernel_core::ExecutionBudget {
                    token_cap: Some(args.context_budget_tokens),
                    byte_cap: Some(args.context_budget_bytes),
                    runtime_ms_cap: None,
                },
                policy_context: json!({
                    "config_path": context_config,
                    "detail_mode": match detail_mode {
                        PacketDetailArg::Rich => "rich",
                        PacketDetailArg::Compact => "compact",
                    },
                    "compact_assembly": detail_mode == PacketDetailArg::Compact,
                }),
                ..context_kernel_core::KernelRequest::default()
            },
        )?)
    } else {
        None
    };
    let profile = machine_profile.unwrap_or(suite_packet_core::JsonProfile::Compact);
    if let Some(governed) = governed_response {
        let budget_hint = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            args.context_budget_tokens,
            args.context_budget_bytes,
            "Packet28 proxy run --context-config <context.yaml> -- <command>",
        );
        let final_packet = governed
            .output_packets
            .first()
            .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_PROXY_RUN,
            &envelope,
            profile,
            args.pretty,
            &persist_root,
            Some(json!({
                "kernel_audit": {
                    "proxy": response.audit,
                    "governed": governed.audit,
                },
                "kernel_metadata": {
                    "proxy": response.metadata,
                    "governed": governed.metadata,
                },
                "cache": {
                    "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
                "hints": {
                    "budget_retry": budget_hint,
                },
                "governed_packet": final_packet.body,
            })),
        )?;
    } else {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_PROXY_RUN,
            &envelope,
            profile,
            args.pretty,
            &persist_root,
            Some(json!({
                "kernel_audit": {
                    "proxy": response.audit,
                },
                "kernel_metadata": {
                    "proxy": response.metadata,
                },
                "cache": {
                    "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            })),
        )?;
    }
    Ok(if envelope.payload.exit_code == 0 { 0 } else { 1 })
}

fn print_text_summary(
    envelope: &suite_packet_core::EnvelopeV1<suite_proxy_core::CommandSummaryPayload>,
) {
    println!("{}", envelope.summary);
    let lines = if envelope.payload.output_lines.is_empty() {
        &envelope.payload.highlights
    } else {
        &envelope.payload.output_lines
    };
    for line in lines {
        println!("{line}");
    }
}

fn persistence_root(args: &RunArgs) -> Result<PathBuf> {
    let root = if let Some(cwd) = args.cwd.as_deref() {
        PathBuf::from(cwd)
    } else {
        std::env::current_dir()?
    };

    Ok(root)
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }

    context_kernel_core::Kernel::with_v1_reducers()
}
