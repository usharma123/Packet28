use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::{json, Value};

#[derive(Args, Default)]
pub struct ValidateArgs {
    /// Path to guard policy config (context.yaml)
    #[arg(long)]
    context_config: Option<String>,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Path to packet JSON file
    #[arg(long)]
    packet: String,

    /// Path to guard policy config (context.yaml)
    #[arg(long)]
    context_config: Option<String>,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pretty: bool,
}

pub fn run_validate(args: ValidateArgs, config_path: &str) -> Result<i32> {
    let effective_config = resolve_context_config(args.context_config.as_deref(), config_path);
    let result = guardy_core::validate_config_file(Path::new(&effective_config))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(if result.valid { 0 } else { 1 })
}

pub fn run_check(args: CheckArgs, config_path: &str) -> Result<i32> {
    let effective_config = resolve_context_config(args.context_config.as_deref(), config_path);
    let packet = context_kernel_core::load_packet_file(Path::new(&args.packet))?;
    let kernel = context_kernel_core::Kernel::with_v1_reducers();
    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "guardy.check".to_string(),
        input_packets: vec![packet],
        policy_context: json!({
            "config_path": effective_config,
        }),
        ..context_kernel_core::KernelRequest::default()
    })?;

    let audit_packet = response
        .output_packets
        .first()
        .ok_or_else(|| anyhow!("kernel returned no output packets"))?;
    let envelope: suite_packet_core::EnvelopeV1<Value> =
        serde_json::from_value(audit_packet.body.clone())
            .map_err(|source| anyhow!("invalid guard audit envelope: {source}"))?;
    let audit: guardy_core::AuditResult = serde_json::from_value(envelope.payload.clone())
        .map_err(|source| anyhow!("invalid guard audit payload: {source}"))?;
    if args.legacy_json {
        crate::cmd_common::emit_json(&serde_json::to_value(&audit)?, args.pretty)?;
    } else {
        let profile = args
            .json
            .map(suite_packet_core::JsonProfile::from)
            .unwrap_or(suite_packet_core::JsonProfile::Compact);
        crate::cmd_common::emit_machine_envelope(
            suite_packet_core::PACKET_TYPE_GUARD_CHECK,
            &envelope,
            profile,
            args.pretty,
            &crate::cmd_common::resolve_artifact_root(None),
            Some(json!({
                "kernel_metadata": response.metadata,
            })),
        )?;
    }
    Ok(if audit.passed { 0 } else { 1 })
}

fn resolve_context_config(explicit: Option<&str>, legacy_config: &str) -> String {
    if let Some(path) = explicit {
        return path.to_string();
    }

    if legacy_config != "covy.toml" {
        eprintln!(
            "warning: --config for guard commands is deprecated; use --context-config instead"
        );
    }
    legacy_config.to_string()
}
