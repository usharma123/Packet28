use std::io::Read;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use suite_packet_core::EnvelopeV1;

#[derive(Args)]
pub struct SliceArgs {
    /// Input stack trace/log file path (reads stdin when omitted)
    #[arg(long)]
    input: Option<String>,

    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,

    /// Optional cap on number of unique failures in output
    #[arg(long)]
    max_failures: Option<usize>,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    cache: bool,

    /// Optional task identifier for state-aware failure classification.
    #[arg(long)]
    task_id: Option<String>,

    /// Run governed packet path using this context policy config (context.yaml).
    #[arg(long)]
    context_config: Option<String>,

    /// Context assembly token budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    context_budget_bytes: usize,
}

impl SliceArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some() || self.legacy_json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }

    pub(crate) fn governed_requested(&self) -> bool {
        self.context_config.is_some()
    }
}

pub fn run(args: SliceArgs) -> Result<i32> {
    let input_text = read_input_text(args.input.as_deref())?;

    let kernel = build_kernel(
        args.cache || args.task_id.is_some(),
        std::env::current_dir()?,
    );
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "stacky.slice".to_string(),
        reducer_input: serde_json::to_value(stacky_core::StackSliceRequest {
            log_text: input_text,
            source: args.input.clone(),
            max_failures: args.max_failures,
        })?,
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
    let packet: EnvelopeV1<stacky_core::StackSliceOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid stacky output packet: {source}"))?;

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
                "task_id": args.task_id,
                "disable_cache": args.task_id.is_some(),
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if let Some(profile_arg) = args.json {
        let mut profile: suite_packet_core::JsonProfile = profile_arg.into();
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                args.context_budget_tokens,
                args.context_budget_bytes,
                "Packet28 stack slice --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.stack.slice.v1",
                        "packet": packet,
                        "final_packet": final_packet.body,
                        "kernel_audit": {
                            "stack": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                        "hints": {
                            "budget_retry": budget_hint,
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                if profile == suite_packet_core::JsonProfile::Compact {
                    profile = suite_packet_core::JsonProfile::Compact;
                }
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_STACK_SLICE,
                    &packet,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "stack": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                        "schema_version": "suite.stack.slice.v1",
                        "packet": packet,
                        "kernel_audit": {
                            "stack": response.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_STACK_SLICE,
                    &packet,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "stack": response.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    })),
                )?;
            }
        }
        return Ok(0);
    }

    let payload = packet.payload.clone();
    println!(
        "summary: total={} unique={} duplicates_removed={}",
        payload.total_failures, payload.unique_failures, payload.duplicates_removed
    );
    for failure in payload.failures {
        let actionable = failure
            .first_actionable_frame
            .as_ref()
            .and_then(|frame| frame.file.as_deref())
            .unwrap_or("unknown");
        println!(
            "- [{}] {} occurrences={} actionable={}",
            failure.fingerprint, failure.title, failure.occurrences, actionable
        );
    }
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
            "Packet28 stack slice --context-config <context.yaml>",
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

pub fn run_remote(args: SliceArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let resolved_context_config =
        crate::cmd_common::resolve_optional_path_from_cwd(args.context_config.as_deref(), &cwd);
    let input_text = read_input_text(args.input.as_deref())?;
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "stacky.slice".to_string(),
            reducer_input: serde_json::to_value(stacky_core::StackSliceRequest {
                log_text: input_text,
                source: args.input.clone(),
                max_failures: args.max_failures,
            })?,
            policy_context: match (resolved_context_config.as_ref(), args.task_id.as_ref()) {
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
        },
    )?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let packet: EnvelopeV1<stacky_core::StackSliceOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid stacky output packet: {source}"))?;

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
                    "task_id": args.task_id,
                    "disable_cache": args.task_id.is_some(),
                }),
                ..context_kernel_core::KernelRequest::default()
            },
        )?)
    } else {
        None
    };

    if let Some(profile_arg) = args.json {
        let mut profile: suite_packet_core::JsonProfile = profile_arg.into();
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                args.context_budget_tokens,
                args.context_budget_bytes,
                "Packet28 stack slice --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.stack.slice.v1",
                        "packet": packet,
                        "final_packet": final_packet.body,
                        "kernel_audit": {
                            "stack": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                        "hints": {
                            "budget_retry": budget_hint,
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                if profile == suite_packet_core::JsonProfile::Compact {
                    profile = suite_packet_core::JsonProfile::Compact;
                }
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_STACK_SLICE,
                    &packet,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "stack": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "stack": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                    "schema_version": "suite.stack.slice.v1",
                    "packet": packet,
                    "kernel_audit": {
                        "stack": response.audit,
                    },
                    "kernel_metadata": {
                        "stack": response.metadata,
                    },
                    "cache": {
                        "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_STACK_SLICE,
                &packet,
                profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                Some(json!({
                    "kernel_audit": {
                        "stack": response.audit,
                    },
                    "kernel_metadata": {
                        "stack": response.metadata,
                    },
                    "cache": {
                        "stack": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                })),
            )?;
        }
        return Ok(0);
    }

    let payload = packet.payload.clone();
    println!(
        "summary: total={} unique={} duplicates_removed={}",
        payload.total_failures, payload.unique_failures, payload.duplicates_removed
    );
    for failure in payload.failures {
        let actionable = failure
            .first_actionable_frame
            .as_ref()
            .and_then(|frame| frame.file.as_deref())
            .unwrap_or("unknown");
        println!(
            "- [{}] {} occurrences={} actionable={}",
            failure.fingerprint, failure.title, failure.occurrences, actionable
        );
    }
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
            "Packet28 stack slice --context-config <context.yaml>",
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

fn read_input_text(path: Option<&str>) -> Result<String> {
    match path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read input file '{path}'")),
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("failed to read stack input from stdin")?;
            Ok(buffer)
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
