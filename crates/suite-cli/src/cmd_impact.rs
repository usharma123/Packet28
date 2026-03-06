use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct ImpactArgs {
    /// Base ref for diff (default: main)
    #[arg(long)]
    pub base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    pub head: Option<String>,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Emit JSON output
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    pub legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,

    /// Emit runnable test command
    #[arg(long)]
    pub print_command: bool,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    pub cache: bool,

    /// Optional task identifier for state-aware flows.
    #[arg(long)]
    pub task_id: Option<String>,

    /// Run governed packet path using this context policy config (context.yaml).
    #[arg(long)]
    pub context_config: Option<String>,

    /// Context assembly token budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub context_budget_tokens: u64,

    /// Context assembly byte budget for governed mode.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub context_budget_bytes: usize,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    let governed_context_config = args.context_config.clone();
    let governed_budget_tokens = args.context_budget_tokens;
    let governed_budget_bytes = args.context_budget_bytes;
    let cwd = std::env::current_dir()?;
    let cache_fingerprint = crate::cmd_common::repo_cache_fingerprint(
        &cwd,
        &[cwd.join(&args.testmap)],
    );
    let policy_context = match (governed_context_config.as_ref(), args.task_id.as_ref()) {
        (Some(config_path), Some(task_id)) => json!({
            "config_path": config_path,
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (Some(config_path), None) => json!({
            "config_path": config_path,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, Some(task_id)) => json!({
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, None) => json!({
            "cache_fingerprint": cache_fingerprint,
        }),
    };

    if args.json.is_some()
        && !args.legacy_json
        && !args.cache
        && args.task_id.is_none()
        && governed_context_config.is_none()
    {
        let adapters = testy_cli_common::adapters::default_impact_adapters();
        let output = testy_core::command_impact::run_legacy_impact(
            testy_core::command_impact::LegacyImpactArgs {
                base: args.base.clone(),
                head: args.head.clone(),
                testmap: args.testmap.clone(),
                print_command: args.print_command,
            },
            config_path,
            &adapters,
        )?;
        let envelope = context_kernel_core::build_test_impact_envelope(
            &output,
            &args.testmap,
            args.base.as_deref(),
            args.head.as_deref(),
        );
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_TEST_IMPACT,
            &envelope,
            args.json
                .map(suite_packet_core::JsonProfile::from)
                .unwrap_or(suite_packet_core::JsonProfile::Compact),
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            None,
        )?;
        return Ok(0);
    }

    let kernel = build_kernel(args.cache || args.task_id.is_some(), cwd);
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "testy.impact".to_string(),
        reducer_input: serde_json::to_value(context_kernel_core::ImpactKernelInput {
            base: args.base,
            head: args.head,
            testmap: args.testmap,
            print_command: args.print_command,
            config_path: config_path.to_string(),
        })?,
        policy_context: policy_context.clone(),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<context_kernel_core::ImpactKernelOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid impact output packet: {source}"))?;
    let output = envelope.payload.clone();

    let governed_response = if governed_context_config.is_some() {
        Some(kernel.execute(context_kernel_core::KernelRequest {
            target: "governed.assemble".to_string(),
            input_packets: vec![output_packet.clone()],
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(governed_budget_tokens),
                byte_cap: Some(governed_budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context: match (governed_context_config.as_ref(), args.task_id.as_ref()) {
                (Some(config_path), Some(task_id)) => json!({
                    "config_path": config_path,
                    "task_id": task_id,
                    "disable_cache": true,
                }),
                (Some(config_path), None) => json!({
                    "config_path": config_path,
                }),
                _ => Value::Null,
            },
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if let Some(profile_arg) = args.json {
        let profile: suite_packet_core::JsonProfile = profile_arg.into();
        if let Some(governed) = governed_response {
            let budget_hint = crate::cmd_common::budget_retry_hint(
                &governed.metadata,
                governed_budget_tokens,
                governed_budget_bytes,
                "Packet28 test impact --context-config <context.yaml>",
            );
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            if args.legacy_json {
                crate::cmd_common::emit_json(
                    &json!({
                        "schema_version": "suite.test.impact.v1",
                        "impact_result": output.result,
                        "known_tests": output.known_tests,
                        "print_command": output.print_command,
                        "final_packet": final_packet.body,
                        "kernel_audit": {
                            "impact": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "impact": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                            "governed": governed.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                        "hints": {
                            "budget_retry": budget_hint,
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                    &envelope,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "impact": response.audit,
                            "governed": governed.audit,
                        },
                        "kernel_metadata": {
                            "impact": response.metadata,
                            "governed": governed.metadata,
                        },
                        "cache": {
                            "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
                        "schema_version": "suite.test.impact.v1",
                        "impact_result": output.result,
                        "known_tests": output.known_tests,
                        "print_command": output.print_command,
                        "kernel_audit": {
                            "impact": response.audit,
                        },
                        "kernel_metadata": {
                            "impact": response.metadata,
                        },
                        "cache": {
                            "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    }),
                    args.pretty,
                )?;
            } else {
                crate::cmd_common::emit_machine_envelope(
                    suite_packet_core::PACKET_TYPE_TEST_IMPACT,
                    &envelope,
                    profile,
                    args.pretty,
                    &crate::cmd_common::resolve_artifact_root(None),
                    Some(json!({
                        "kernel_audit": {
                            "impact": response.audit,
                        },
                        "kernel_metadata": {
                            "impact": response.metadata,
                        },
                        "cache": {
                            "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                        },
                    })),
                )?;
            }
        }
        return Ok(0);
    }

    if output.result.selected_tests.is_empty() {
        println!("(no impacted tests)");
    } else {
        for test in &output.result.selected_tests {
            println!("{test}");
        }
    }
    println!(
        "summary: selected={} known={} missing={} confidence={:.2} stale={} escalate_full_suite={}",
        output.result.selected_tests.len(),
        output.known_tests,
        output.result.missing_mappings.len(),
        output.result.confidence,
        output.result.stale,
        output.result.escalate_full_suite
    );

    if args.print_command {
        if let Some(command) = output.print_command {
            println!("{command}");
        }
    }

    if let Some(governed) = governed_response {
        if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
            println!("{summary}");
        }
        if let Some(summary) = crate::cmd_common::cache_summary_line(&governed.metadata) {
            println!("{summary}");
        }
        if let Some(hint) = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            governed_budget_tokens,
            governed_budget_bytes,
            "Packet28 test impact --context-config <context.yaml>",
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
    } else if let Some(summary) = crate::cmd_common::cache_summary_line(&response.metadata) {
        println!("{summary}");
    }

    Ok(0)
}

pub fn run_remote(args: ImpactArgs, config_path: &str, daemon_root: &Path) -> Result<i32> {
    if args.json.is_none() || args.legacy_json {
        return run(args, config_path);
    }

    let governed_context_config = args.context_config.clone();
    let governed_budget_tokens = args.context_budget_tokens;
    let governed_budget_bytes = args.context_budget_bytes;
    let cwd = std::env::current_dir()?;
    let cache_fingerprint = crate::cmd_common::repo_cache_fingerprint(&cwd, &[cwd.join(&args.testmap)]);
    let policy_context = match (governed_context_config.as_ref(), args.task_id.as_ref()) {
        (Some(config_path), Some(task_id)) => json!({
            "config_path": config_path,
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (Some(config_path), None) => json!({
            "config_path": config_path,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, Some(task_id)) => json!({
            "task_id": task_id,
            "cache_fingerprint": cache_fingerprint,
        }),
        (None, None) => json!({
            "cache_fingerprint": cache_fingerprint,
        }),
    };

    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "testy.impact".to_string(),
            reducer_input: serde_json::to_value(context_kernel_core::ImpactKernelInput {
                base: args.base.clone(),
                head: args.head.clone(),
                testmap: args.testmap.clone(),
                print_command: args.print_command,
                config_path: config_path.to_string(),
            })?,
            policy_context: policy_context.clone(),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<context_kernel_core::ImpactKernelOutput> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid impact output packet: {source}"))?;

    let governed_response = if governed_context_config.is_some() {
        Some(crate::cmd_daemon::send_kernel_request(
            daemon_root,
            context_kernel_core::KernelRequest {
                target: "governed.assemble".to_string(),
                input_packets: vec![output_packet.clone()],
                budget: context_kernel_core::ExecutionBudget {
                    token_cap: Some(governed_budget_tokens),
                    byte_cap: Some(governed_budget_bytes),
                    runtime_ms_cap: None,
                },
                policy_context: match (governed_context_config.as_ref(), args.task_id.as_ref()) {
                    (Some(config_path), Some(task_id)) => json!({
                        "config_path": config_path,
                        "task_id": task_id,
                        "disable_cache": true,
                    }),
                    (Some(config_path), None) => json!({
                        "config_path": config_path,
                    }),
                    _ => Value::Null,
                },
                ..context_kernel_core::KernelRequest::default()
            },
        )?)
    } else {
        None
    };

    let profile: suite_packet_core::JsonProfile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    if let Some(governed) = governed_response {
        let budget_hint = crate::cmd_common::budget_retry_hint(
            &governed.metadata,
            governed_budget_tokens,
            governed_budget_bytes,
            "Packet28 test impact --context-config <context.yaml>",
        );
        let final_packet = governed
            .output_packets
            .first()
            .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_TEST_IMPACT,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_audit": {
                    "impact": response.audit,
                    "governed": governed.audit,
                },
                "kernel_metadata": {
                    "impact": response.metadata,
                    "governed": governed.metadata,
                },
                "cache": {
                    "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
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
            suite_packet_core::PACKET_TYPE_TEST_IMPACT,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_audit": {
                    "impact": response.audit,
                },
                "kernel_metadata": {
                    "impact": response.metadata,
                },
                "cache": {
                    "impact": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            })),
        )?;
    }

    Ok(0)
}

fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }
    context_kernel_core::Kernel::with_v1_reducers()
}
