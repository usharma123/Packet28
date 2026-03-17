use anyhow::{anyhow, Result};
use clap::{Args, ValueEnum};
use packet28_daemon_core::{resolve_workspace_root, BrokerWriteOp, BrokerWriteStateRequest};
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
    /// Optional Packet28 task to attribute proxy usage to
    #[arg(long)]
    pub task_id: Option<String>,

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
    let caller_cwd = crate::cmd_common::caller_cwd()?;
    let machine_profile = args
        .json
        .map(|profile| suite_packet_core::JsonProfile::from(profile))
        .or(args
            .legacy_json
            .then_some(suite_packet_core::JsonProfile::Compact));
    let resolved_cwd = Some(match args.cwd.as_deref() {
        Some(path) => crate::cmd_common::resolve_path_from_cwd(path, &caller_cwd),
        None => caller_cwd.to_string_lossy().into_owned(),
    });
    let resolved_context_config = crate::cmd_common::resolve_optional_path_from_cwd(
        args.context_config.as_deref(),
        &caller_cwd,
    );
    let detail_mode = if args.packet_detail == PacketDetailArg::Rich {
        PacketDetailArg::Rich
    } else {
        PacketDetailArg::Compact
    };
    let input = suite_proxy_core::ProxyRunRequest {
        argv: args.command_argv.clone(),
        cwd: resolved_cwd.clone(),
        env_allowlist: args.env_allowlist.clone(),
        max_output_bytes: args.max_output_bytes,
        max_lines: args.max_lines,
        packet_byte_cap: args.packet_byte_cap,
        detail: detail_mode.into(),
    };

    let use_kernel = args.cache || resolved_context_config.is_some();
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
        record_proxy_result(&caller_cwd, args.task_id.as_deref(), &args.command_argv, &envelope)?;
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
        policy_context: resolved_context_config
            .as_ref()
            .map(|path| json!({"config_path": path}))
            .unwrap_or(Value::Null),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;

    let governed_response = if let Some(context_config) = resolved_context_config.clone() {
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

    handle_kernel_response(&args, &persist_root, &caller_cwd, response, governed_response)
}

pub fn run_remote(args: RunArgs, daemon_root: &Path) -> Result<i32> {
    let persist_root = persistence_root(&args)?;
    let caller_cwd = crate::cmd_common::caller_cwd()?;
    let detail_mode = if args.packet_detail == PacketDetailArg::Rich {
        PacketDetailArg::Rich
    } else {
        PacketDetailArg::Compact
    };
    let resolved_cwd = Some(match args.cwd.as_deref() {
        Some(path) => crate::cmd_common::resolve_path_from_cwd(path, &caller_cwd),
        None => caller_cwd.to_string_lossy().into_owned(),
    });
    let resolved_context_config = crate::cmd_common::resolve_optional_path_from_cwd(
        args.context_config.as_deref(),
        &caller_cwd,
    );
    let input = suite_proxy_core::ProxyRunRequest {
        argv: args.command_argv.clone(),
        cwd: resolved_cwd,
        env_allowlist: args.env_allowlist.clone(),
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
            policy_context: resolved_context_config
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
    let governed_response = if let Some(context_config) = resolved_context_config {
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
    handle_kernel_response(&args, &persist_root, &caller_cwd, response, governed_response)
}

fn handle_kernel_response(
    args: &RunArgs,
    persist_root: &Path,
    caller_cwd: &Path,
    response: context_kernel_core::KernelResponse,
    governed_response: Option<context_kernel_core::KernelResponse>,
) -> Result<i32> {
    let machine_profile = args
        .json
        .map(|profile| suite_packet_core::JsonProfile::from(profile))
        .or(args
            .legacy_json
            .then_some(suite_packet_core::JsonProfile::Compact));
    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_proxy_core::CommandSummaryPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid proxy output packet: {source}"))?;

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
                    persist_root,
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
        } else if args.legacy_json {
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
                persist_root,
                Some(json!({
                    "cache": {
                        "proxy": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                })),
            )?;
        }

        record_proxy_result(caller_cwd, args.task_id.as_deref(), &args.command_argv, &envelope)?;
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

    record_proxy_result(caller_cwd, args.task_id.as_deref(), &args.command_argv, &envelope)?;
    Ok(if envelope.payload.exit_code == 0 {
        0
    } else {
        1
    })
}

fn record_proxy_result(
    caller_cwd: &Path,
    task_id: Option<&str>,
    argv: &[String],
    envelope: &suite_packet_core::EnvelopeV1<suite_proxy_core::CommandSummaryPayload>,
) -> Result<()> {
    let Some(task_id) = task_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let root = resolve_workspace_root(caller_cwd);
    crate::broker_client::ensure_daemon(&root)?;
    crate::broker_client::write_state(
        &root,
        BrokerWriteStateRequest {
            task_id: task_id.to_string(),
            op: Some(BrokerWriteOp::ToolResult),
            tool_name: Some("packet28.proxy.run".to_string()),
            operation_kind: Some(suite_packet_core::ToolOperationKind::Generic),
            request_summary: Some(argv.join(" ")),
            result_summary: Some(envelope.summary.clone()),
            compact_path: Some("proxy_passthrough".to_string()),
            raw_est_tokens: Some(((envelope.payload.bytes_in as f64) / 4.0).ceil() as u64),
            reduced_est_tokens: Some(((envelope.payload.bytes_out as f64) / 4.0).ceil() as u64),
            paths: envelope.files.iter().map(|file| file.path.clone()).collect(),
            raw_artifact_available: Some(false),
            refresh_context: Some(false),
            ..BrokerWriteStateRequest::default()
        },
    )?;
    Ok(())
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
