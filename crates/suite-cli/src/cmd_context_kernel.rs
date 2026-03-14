use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::cmd_context::{build_kernel, AssembleArgs, CorrelateArgs, ManageArgs};

pub fn run_assemble(args: AssembleArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let detail_mode = if profile == suite_packet_core::JsonProfile::Compact {
        "compact"
    } else {
        "rich"
    };
    let compact_assembly = profile == suite_packet_core::JsonProfile::Compact;
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let cwd = std::env::current_dir()?;
    let kernel = build_kernel(args.cache || args.task_id.is_some(), cwd.clone());
    let target = if args.context_config.is_some() {
        "governed.assemble"
    } else {
        "contextq.assemble"
    };
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: target.to_string(),
        input_packets,
        budget: context_kernel_core::ExecutionBudget {
            token_cap: Some(args.budget_tokens),
            byte_cap: Some(args.budget_bytes),
            runtime_ms_cap: None,
        },
        policy_context: match args.context_config.as_ref() {
            Some(config_path) => json!({
                "config_path": config_path,
                "detail_mode": detail_mode,
                "compact_assembly": compact_assembly,
                "task_id": args.task_id,
                "disable_cache": args.task_id.is_some(),
            }),
            None => json!({
                "detail_mode": detail_mode,
                "compact_assembly": compact_assembly,
                "task_id": args.task_id,
                "disable_cache": args.task_id.is_some(),
            }),
        },
        ..context_kernel_core::KernelRequest::default()
    })?;

    let assembled = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<Value> =
        serde_json::from_value(assembled.body.clone())
            .map_err(|source| anyhow!("invalid context output packet: {source}"))?;
    if args.context_config.is_some() {
        let budget_hint = crate::cmd_common::budget_retry_hint(
            &response.metadata,
            args.budget_tokens,
            args.budget_bytes,
            "Packet28 context assemble --context-config <context.yaml>",
        );
        if args.legacy_json {
            crate::cmd_common::emit_json(
                &json!({
                    "schema_version": "suite.context.assemble.v1",
                    "final_packet": assembled.body,
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
                &envelope,
                profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                Some(json!({
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                })),
            )?;
        }
    } else if args.legacy_json {
        crate::cmd_common::emit_json(
            &json!({
                "schema_version": "suite.context.assemble.v1",
                "packet": assembled.body,
                "kernel_audit": {
                    "context": response.audit,
                },
                "kernel_metadata": {
                    "context": response.metadata,
                },
                "cache": {
                    "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            }),
            args.pretty,
        )?;
    } else {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_audit": {
                    "context": response.audit,
                },
                "kernel_metadata": {
                    "context": response.metadata,
                },
                "cache": {
                    "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            })),
        )?;
    }

    Ok(0)
}

pub fn run_assemble_remote(args: AssembleArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let resolved_context_config =
        crate::cmd_common::resolve_optional_path_from_cwd(args.context_config.as_deref(), &cwd);
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let detail_mode = if profile == suite_packet_core::JsonProfile::Compact {
        "compact"
    } else {
        "rich"
    };
    let compact_assembly = profile == suite_packet_core::JsonProfile::Compact;
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let target = if resolved_context_config.is_some() {
        "governed.assemble"
    } else {
        "contextq.assemble"
    };
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: target.to_string(),
            input_packets,
            budget: context_kernel_core::ExecutionBudget {
                token_cap: Some(args.budget_tokens),
                byte_cap: Some(args.budget_bytes),
                runtime_ms_cap: None,
            },
            policy_context: match resolved_context_config.as_ref() {
                Some(config_path) => json!({
                    "config_path": config_path,
                    "detail_mode": detail_mode,
                    "compact_assembly": compact_assembly,
                    "task_id": args.task_id,
                    "disable_cache": args.task_id.is_some(),
                }),
                None => json!({
                    "detail_mode": detail_mode,
                    "compact_assembly": compact_assembly,
                    "task_id": args.task_id,
                    "disable_cache": args.task_id.is_some(),
                }),
            },
            ..context_kernel_core::KernelRequest::default()
        },
    )?;
    let assembled = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<Value> =
        serde_json::from_value(assembled.body.clone())
            .map_err(|source| anyhow!("invalid context output packet: {source}"))?;
    if resolved_context_config.is_some() {
        let budget_hint = crate::cmd_common::budget_retry_hint(
            &response.metadata,
            args.budget_tokens,
            args.budget_bytes,
            "Packet28 context assemble --context-config <context.yaml>",
        );
        if args.legacy_json {
            crate::cmd_common::emit_json(
                &json!({
                    "schema_version": "suite.context.assemble.v1",
                    "final_packet": assembled.body,
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                }),
                args.pretty,
            )?;
        } else {
            crate::cmd_common::emit_machine_envelope(
                suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
                &envelope,
                profile,
                args.pretty,
                &crate::cmd_common::resolve_artifact_root(None),
                Some(json!({
                    "kernel_audit": {
                        "governed": response.audit,
                    },
                    "kernel_metadata": {
                        "governed": response.metadata,
                    },
                    "cache": {
                        "governed": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                    },
                    "hints": {
                        "budget_retry": budget_hint,
                    },
                })),
            )?;
        }
    } else if args.legacy_json {
        crate::cmd_common::emit_json(
            &json!({
                "schema_version": "suite.context.assemble.v1",
                "packet": assembled.body,
                "kernel_audit": {
                    "context": response.audit,
                },
                "kernel_metadata": {
                    "context": response.metadata,
                },
                "cache": {
                    "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            }),
            args.pretty,
        )?;
    } else {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_ASSEMBLE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_audit": {
                    "context": response.audit,
                },
                "kernel_metadata": {
                    "context": response.metadata,
                },
                "cache": {
                    "context": response.metadata.get("cache").cloned().unwrap_or(Value::Null),
                },
            })),
        )?;
    }
    Ok(0)
}

pub fn run_manage(args: ManageArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let cwd = std::env::current_dir()?;
    let kernel = build_kernel(true, cwd);
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "contextq.manage".to_string(),
        reducer_input: json!({
            "task_id": args.task_id,
            "query": args.query,
            "budget_tokens": args.budget_tokens,
            "budget_bytes": args.budget_bytes,
            "scope": args
                .scope
                .map(|scope| scope.as_policy_scope().to_string())
                .unwrap_or_else(|| "task_first".to_string()),
            "checkpoint_id": args.checkpoint_id,
        }),
        policy_context: json!({
            "task_id": args.task_id,
        }),
        ..context_kernel_core::KernelRequest::default()
    })?;
    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid manage output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_MANAGE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_metadata": {
                    "manage": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} working_set={} evictions={} headroom_tokens={}",
        envelope.payload.task_id,
        envelope.payload.working_set.len(),
        envelope.payload.eviction_candidates.len(),
        envelope.payload.budget.reserved_headroom_tokens
    );
    for packet in envelope.payload.working_set.iter().take(5) {
        println!(
            "- keep score={:.3} target={} key={} tokens={}",
            packet.score, packet.target, packet.cache_key, packet.est_tokens
        );
        if let Some(summary) = packet.summary.as_ref() {
            println!("  {summary}");
        }
    }
    for action in &envelope.payload.recommended_actions {
        println!("* {}: {}", action.kind, action.summary);
    }
    Ok(0)
}

pub fn run_manage_remote(args: ManageArgs, daemon_root: &Path) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "contextq.manage".to_string(),
            reducer_input: json!({
                "task_id": args.task_id,
                "query": args.query,
                "budget_tokens": args.budget_tokens,
                "budget_bytes": args.budget_bytes,
                "scope": args
                    .scope
                    .map(|scope| scope.as_policy_scope().to_string())
                    .unwrap_or_else(|| "task_first".to_string()),
                "checkpoint_id": args.checkpoint_id,
            }),
            policy_context: json!({
                "task_id": args.task_id,
            }),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;
    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextManagePayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid manage output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_MANAGE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_metadata": {
                    "manage": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} working_set={} evictions={} headroom_tokens={}",
        envelope.payload.task_id,
        envelope.payload.working_set.len(),
        envelope.payload.eviction_candidates.len(),
        envelope.payload.budget.reserved_headroom_tokens
    );
    for packet in envelope.payload.working_set.iter().take(5) {
        println!(
            "- keep score={:.3} target={} key={} tokens={}",
            packet.score, packet.target, packet.cache_key, packet.est_tokens
        );
        if let Some(summary) = packet.summary.as_ref() {
            println!("  {summary}");
        }
    }
    for action in &envelope.payload.recommended_actions {
        println!("* {}: {}", action.kind, action.summary);
    }
    Ok(0)
}

pub fn run_correlate(args: CorrelateArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let cwd = std::env::current_dir()?;
    let kernel = build_kernel(true, cwd.clone());
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| anyhow!("{source}"))?;

    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "contextq.correlate".to_string(),
        input_packets,
        policy_context: json!({
            "task_id": args.task_id,
            "disable_cache": false,
            "scope": args
                .scope
                .map(|scope| scope.as_policy_scope().to_string())
                .unwrap_or_else(|| "task_first".to_string()),
        }),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid correlation output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_CORRELATE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_metadata": {
                    "correlate": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!("findings: {}", envelope.payload.finding_count);
    for finding in &envelope.payload.findings {
        println!(
            "- [{}] {} ({:.2})",
            finding.relation, finding.summary, finding.confidence
        );
    }
    Ok(0)
}

pub fn run_correlate_remote(args: CorrelateArgs, daemon_root: &Path) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let input_packets = args
        .packets
        .iter()
        .map(|path| context_kernel_core::load_packet_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| anyhow!("{source}"))?;

    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "contextq.correlate".to_string(),
            input_packets,
            policy_context: json!({
                "task_id": args.task_id,
                "disable_cache": false,
                "scope": args
                    .scope
                    .map(|scope| scope.as_policy_scope().to_string())
                    .unwrap_or_else(|| "task_first".to_string()),
            }),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::ContextCorrelationPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid correlation output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_CONTEXT_CORRELATE,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_metadata": {
                    "correlate": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!("findings: {}", envelope.payload.finding_count);
    for finding in &envelope.payload.findings {
        println!(
            "- [{}] {} ({:.2})",
            finding.relation, finding.summary, finding.confidence
        );
    }
    Ok(0)
}
