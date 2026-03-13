use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::cmd_context::{
    build_persistent_kernel, StateAppendArgs, StateArgs, StateCommands, StateSnapshotArgs,
};

pub(crate) fn run_state(args: StateArgs) -> Result<i32> {
    match args.command {
        StateCommands::Append(args) => run_state_append(args),
        StateCommands::Snapshot(args) => run_state_snapshot(args),
    }
}

pub(crate) fn run_state_remote(args: StateArgs, daemon_root: &Path) -> Result<i32> {
    match args.command {
        StateCommands::Append(args) => run_state_append_remote(args, daemon_root),
        StateCommands::Snapshot(args) => run_state_snapshot_remote(args, daemon_root),
    }
}

fn run_state_append(args: StateAppendArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let input_text = std::fs::read_to_string(&args.input)
        .with_context(|| format!("failed to read state input '{}'", args.input))?;
    let mut input_value: Value = serde_json::from_str(&input_text)
        .with_context(|| format!("invalid JSON in '{}'", args.input))?;
    let object = input_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("state input must be a JSON object"))?;
    match object.get("task_id").and_then(Value::as_str) {
        Some(existing) if existing != args.task_id => {
            anyhow::bail!(
                "state input task_id '{}' does not match --task-id '{}'",
                existing,
                args.task_id
            );
        }
        Some(_) => {}
        None => {
            object.insert("task_id".to_string(), Value::String(args.task_id.clone()));
        }
    }

    let kernel = build_persistent_kernel(PathBuf::from(&args.root));
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "agenty.state.write".to_string(),
        reducer_input: input_value,
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid agent state output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_AGENT_STATE,
            &envelope,
            profile,
            args.pretty,
            &PathBuf::from(&args.root),
            Some(json!({
                "kernel_audit": {
                    "state": response.audit,
                },
                "kernel_metadata": {
                    "state": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} event={} kind={:?}",
        envelope.payload.task_id, envelope.payload.event_id, envelope.payload.kind
    );
    Ok(0)
}

fn run_state_append_remote(args: StateAppendArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let resolved_root = crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd);
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let input_text = std::fs::read_to_string(&args.input)
        .with_context(|| format!("failed to read state input '{}'", args.input))?;
    let mut input_value: Value = serde_json::from_str(&input_text)
        .with_context(|| format!("invalid JSON in '{}'", args.input))?;
    let object = input_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("state input must be a JSON object"))?;
    match object.get("task_id").and_then(Value::as_str) {
        Some(existing) if existing != args.task_id => {
            anyhow::bail!(
                "state input task_id '{}' does not match --task-id '{}'",
                existing,
                args.task_id
            );
        }
        Some(_) => {}
        None => {
            object.insert("task_id".to_string(), Value::String(args.task_id.clone()));
        }
    }

    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "agenty.state.write".to_string(),
            reducer_input: input_value,
            policy_context: json!({
                "persist_root": resolved_root,
            }),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentStateEventPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid agent state output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_AGENT_STATE,
            &envelope,
            profile,
            args.pretty,
            &PathBuf::from(&resolved_root),
            Some(json!({
                "kernel_audit": {
                    "state": response.audit,
                },
                "kernel_metadata": {
                    "state": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} event={} kind={:?}",
        envelope.payload.task_id, envelope.payload.event_id, envelope.payload.kind
    );
    Ok(0)
}

fn run_state_snapshot(args: StateSnapshotArgs) -> Result<i32> {
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let kernel = build_persistent_kernel(PathBuf::from(&args.root));
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "agenty.state.snapshot".to_string(),
        reducer_input: json!({
            "task_id": args.task_id,
        }),
        policy_context: json!({
            "disable_cache": true,
        }),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid agent snapshot output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_AGENT_SNAPSHOT,
            &envelope,
            profile,
            args.pretty,
            &PathBuf::from(&args.root),
            Some(json!({
                "kernel_audit": {
                    "state": response.audit,
                },
                "kernel_metadata": {
                    "state": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} events={} focus_paths={} open_questions={}",
        envelope.payload.task_id,
        envelope.payload.event_count,
        envelope.payload.focus_paths.len(),
        envelope.payload.open_questions.len()
    );
    Ok(0)
}

fn run_state_snapshot_remote(args: StateSnapshotArgs, daemon_root: &Path) -> Result<i32> {
    let cwd = crate::cmd_common::caller_cwd()?;
    let resolved_root = crate::cmd_common::resolve_path_from_cwd(&args.root, &cwd);
    let profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Compact);
    let response = crate::cmd_daemon::send_kernel_request(
        daemon_root,
        context_kernel_core::KernelRequest {
            target: "agenty.state.snapshot".to_string(),
            reducer_input: json!({
                "task_id": args.task_id,
            }),
            policy_context: json!({
                "disable_cache": true,
                "persist_root": resolved_root,
            }),
            ..context_kernel_core::KernelRequest::default()
        },
    )?;

    let output_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<suite_packet_core::AgentSnapshotPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid agent snapshot output packet: {source}"))?;

    if args.json.is_some() {
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_AGENT_SNAPSHOT,
            &envelope,
            profile,
            args.pretty,
            &PathBuf::from(&resolved_root),
            Some(json!({
                "kernel_audit": {
                    "state": response.audit,
                },
                "kernel_metadata": {
                    "state": response.metadata,
                },
            })),
        )?;
        return Ok(0);
    }

    println!(
        "task={} events={} focus_paths={} open_questions={}",
        envelope.payload.task_id,
        envelope.payload.event_count,
        envelope.payload.focus_paths.len(),
        envelope.payload.open_questions.len()
    );
    Ok(0)
}
