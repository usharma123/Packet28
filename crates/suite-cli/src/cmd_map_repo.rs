use anyhow::{anyhow, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum PacketDetailArg {
    #[default]
    Compact,
    Rich,
}

#[derive(Args)]
pub struct RepoArgs {
    /// Repository root path
    #[arg(long, default_value = ".")]
    pub repo_root: String,

    /// Focus paths for relevance ranking
    #[arg(long = "focus-path")]
    pub focus_paths: Vec<String>,

    /// Focus symbols for relevance ranking
    #[arg(long = "focus-symbol")]
    pub focus_symbols: Vec<String>,

    /// Maximum files in map output
    #[arg(long, default_value_t = 40)]
    pub max_files: usize,

    /// Maximum symbols in map output
    #[arg(long, default_value_t = 120)]
    pub max_symbols: usize,

    /// Include test files
    #[arg(long)]
    pub include_tests: bool,

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

    /// Persist kernel cache on disk under <repo-root>/.packet28
    #[arg(long)]
    pub cache: bool,

    /// Optional task identifier for state-aware focus propagation.
    #[arg(long)]
    pub task_id: Option<String>,

    /// Run governed packet path using this context policy config (context.yaml)
    #[arg(long)]
    pub context_config: Option<String>,

    /// Context assembly token budget for governed mode
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub context_budget_bytes: usize,
}

pub fn run(args: RepoArgs) -> Result<i32> {
    let machine_profile = args
        .json
        .map(|profile| suite_packet_core::JsonProfile::from(profile));
    let detail_mode = if matches!(
        machine_profile,
        Some(suite_packet_core::JsonProfile::Full | suite_packet_core::JsonProfile::Handle)
    ) || args.packet_detail == PacketDetailArg::Rich
    {
        PacketDetailArg::Rich
    } else {
        PacketDetailArg::Compact
    };
    let repo_root = args.repo_root.clone();
    let input = mapy_core::RepoMapRequest {
        repo_root,
        focus_paths: args.focus_paths,
        focus_symbols: args.focus_symbols,
        max_files: args.max_files,
        max_symbols: args.max_symbols,
        include_tests: args.include_tests,
    };

    let use_kernel = args.cache || args.context_config.is_some() || args.task_id.is_some();
    if !use_kernel {
        let envelope = mapy_core::build_repo_map(input)?;
        if let Some(profile) = machine_profile {
            if args.legacy_json {
                let packet = packet_value(&envelope, detail_mode)?;
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.map.repo.v1",
                        "packet": packet,
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_MAP_REPO,
                    &envelope,
                    profile,
                    args.pretty,
                    &PathBuf::from(&args.repo_root),
                    None,
                )?;
            }
            return Ok(0);
        }

        print_text_summary(&envelope);
        return Ok(0);
    }

    let kernel = build_kernel(
        args.cache || args.task_id.is_some(),
        PathBuf::from(&input.repo_root),
    );
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "mapy.repo".to_string(),
        reducer_input: serde_json::to_value(input)?,
        policy_context: match (args.context_config.as_ref(), args.task_id.as_ref()) {
            (Some(path), Some(task_id)) => json!({
                "config_path": path,
                "task_id": task_id,
                "disable_cache": true,
            }),
            (Some(path), None) => json!({"config_path": path}),
            (None, Some(task_id)) => json!({
                "task_id": task_id,
                "disable_cache": true,
            }),
            (None, None) => Value::Null,
        },
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;

    let envelope: suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid mapy output packet: {source}"))?;
    let governed_input_packet = if detail_mode == PacketDetailArg::Rich {
        let mut packet = output_packet.clone();
        packet.body = packet_value(&envelope, PacketDetailArg::Rich)?;
        packet
    } else {
        output_packet.clone()
    };

    let governed_response = if let Some(context_config) = args.context_config {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![governed_input_packet],
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
                "task_id": args.task_id,
                "disable_cache": args.task_id.is_some(),
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if let Some(profile) = machine_profile {
        let packet = packet_value(&envelope, detail_mode)?;
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                args.context_budget_tokens,
                args.context_budget_bytes,
                "Packet28 map repo --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            if args.legacy_json {
                if args.debug {
                    crate::cmd_common::emit_json(
                        &json!({
                            "schema_version": "suite.map.repo.v1",
                            "packet": packet,
                            "final_packet": final_packet.body,
                            "kernel_audit": {
                                "map": response.audit,
                                "governed": governed.audit,
                            },
                            "kernel_metadata": {
                                "map": response.metadata,
                                "governed": governed.metadata,
                            },
                            "cache": {
                                "map": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                            "schema_version": "suite.map.repo.v1",
                            "packet": packet,
                            "final_packet": final_packet.body,
                        }),
                        args.pretty,
                    )?;
                }
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_MAP_REPO,
                    &envelope,
                    profile,
                    args.pretty,
                    &PathBuf::from(&args.repo_root),
                    Some(json!({
                        "kernel_audit": {
                            "map": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "map": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "map": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                        "schema_version": "suite.map.repo.v1",
                        "packet": packet,
                        "cache": {
                            "map": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_MAP_REPO,
                    &envelope,
                    profile,
                    args.pretty,
                    &PathBuf::from(&args.repo_root),
                    Some(json!({
                        "cache": {
                            "map": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    })),
                )?;
            }
        }

        return Ok(0);
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
            "Packet28 map repo --context-config <context.yaml>",
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

    Ok(0)
}

fn packet_value(
    envelope: &suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>,
    detail: PacketDetailArg,
) -> Result<Value> {
    if detail == PacketDetailArg::Rich {
        let mut value = serde_json::to_value(envelope)?;
        value["payload"] = serde_json::to_value(mapy_core::expand_repo_map_payload(envelope))?;
        return Ok(value);
    }
    serde_json::to_value(envelope).map_err(Into::into)
}

fn print_text_summary(envelope: &suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload>) {
    println!("{}", envelope.summary);
    println!("top files:");
    for file in envelope.payload.files_ranked.iter().take(10) {
        if let Some(file_ref) = envelope.files.get(file.file_idx) {
            println!("- {} ({:.3})", file_ref.path, file.score);
        }
    }
    println!("top symbols:");
    for symbol in envelope.payload.symbols_ranked.iter().take(10) {
        if let Some(symbol_ref) = envelope.symbols.get(symbol.symbol_idx) {
            let file = envelope
                .files
                .get(symbol.file_idx)
                .map(|f| f.path.as_str())
                .unwrap_or("unknown");
            println!("- {} :: {} ({:.3})", file, symbol_ref.name, symbol.score);
        }
    }
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }

    context_kernel_core::Kernel::with_v1_reducers()
}
