use anyhow::{anyhow, Result};
use clap::{Args, Subcommand};
use serde_json::Value;

#[derive(Args)]
pub struct PacketArgs {
    #[command(subcommand)]
    pub command: PacketCommands,
}

#[derive(Subcommand)]
pub enum PacketCommands {
    /// Fetch a full packet artifact by handle
    Fetch(FetchArgs),
}

#[derive(Args)]
pub struct FetchArgs {
    /// Handle id returned by --json=handle
    #[arg(long)]
    pub handle: String,

    /// Root directory containing .packet28/artifacts
    #[arg(long, default_value = ".")]
    pub root: String,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "full")]
    pub json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub pretty: bool,
}

pub fn run_fetch(args: FetchArgs) -> Result<i32> {
    let root = std::path::PathBuf::from(&args.root);
    let value = suite_packet_core::read_packet_artifact(&root, &args.handle)
        .map_err(|source| anyhow!(source.to_string()))?;
    let wrapper: suite_packet_core::PacketWrapperV1<suite_packet_core::EnvelopeV1<Value>> =
        serde_json::from_value(value)
            .map_err(|source| anyhow!("invalid packet artifact: {source}"))?;

    let mut profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Full);
    if profile == suite_packet_core::JsonProfile::Handle {
        profile = suite_packet_core::JsonProfile::Full;
    }

    crate::cmd_common::emit_machine_envelope(
        &wrapper.packet_type,
        &wrapper.packet,
        profile,
        args.pretty,
        &root,
        None,
    )?;

    Ok(0)
}

pub fn run_fetch_remote(args: FetchArgs) -> Result<i32> {
    let root = std::path::PathBuf::from(&args.root);
    let response = crate::cmd_daemon::send_packet_fetch(
        &root,
        packet28_daemon_core::PacketFetchRequest {
            handle: args.handle.clone(),
            root: args.root.clone(),
        },
    )?;
    let mut profile = args
        .json
        .map(suite_packet_core::JsonProfile::from)
        .unwrap_or(suite_packet_core::JsonProfile::Full);
    if profile == suite_packet_core::JsonProfile::Handle {
        profile = suite_packet_core::JsonProfile::Full;
    }
    crate::cmd_common::emit_machine_envelope(
        &response.wrapper.packet_type,
        &response.wrapper.packet,
        profile,
        args.pretty,
        &root,
        None,
    )?;
    Ok(0)
}
