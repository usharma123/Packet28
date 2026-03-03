use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct AssembleArgs {
    /// Path(s) to reducer packet JSON files.
    #[arg(long = "packet", alias = "input", required = true)]
    packets: Vec<String>,

    /// Max approximate token budget for assembled payload.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    budget_tokens: u64,

    /// Max byte budget for assembled payload JSON.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    budget_bytes: usize,
}

pub fn run_assemble(args: AssembleArgs) -> Result<i32> {
    let packet_paths: Vec<PathBuf> = args.packets.into_iter().map(PathBuf::from).collect();

    let assembled = contextq_core::assemble_packet_files(
        &packet_paths,
        contextq_core::AssembleOptions {
            budget_tokens: args.budget_tokens,
            budget_bytes: args.budget_bytes,
        },
    )?;

    println!("{}", serde_json::to_string_pretty(&assembled)?);
    Ok(0)
}
