use anyhow::{anyhow, Result};
use clap::Args;
use serde_json::{json, Value};

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
    #[arg(long, default_value_t = 80)]
    pub max_files: usize,

    /// Maximum symbols in map output
    #[arg(long, default_value_t = 300)]
    pub max_symbols: usize,

    /// Include test files
    #[arg(long)]
    pub include_tests: bool,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

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
    let kernel = context_kernel_core::Kernel::with_v1_reducers();

    let response = kernel.execute(context_kernel_core::KernelRequest {
        target: "mapy.repo".to_string(),
        reducer_input: serde_json::to_value(mapy_core::RepoMapRequest {
            repo_root: args.repo_root,
            focus_paths: args.focus_paths,
            focus_symbols: args.focus_symbols,
            max_files: args.max_files,
            max_symbols: args.max_symbols,
            include_tests: args.include_tests,
        })?,
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

    let envelope: suite_packet_core::EnvelopeV1<mapy_core::RepoMapPayload> =
        serde_json::from_value(output_packet.body.clone())
            .map_err(|source| anyhow!("invalid mapy output packet: {source}"))?;

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
            }),
            ..context_kernel_core::KernelRequest::default()
        })?)
    } else {
        None
    };

    if args.json {
        if let Some(governed) = governed_response {
            let final_packet = governed
                .output_packets
                .first()
                .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.map.repo.v1",
                    "packet": envelope,
                    "final_packet": final_packet.body,
                    "kernel_audit": {
                        "map": response.audit,
                        "governed": governed.audit,
                    },
                    "kernel_metadata": {
                        "map": response.metadata,
                        "governed": governed.metadata,
                    },
                }))?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": "suite.map.repo.v1",
                    "packet": envelope,
                    "kernel_audit": {
                        "map": response.audit,
                    },
                    "kernel_metadata": {
                        "map": response.metadata,
                    },
                }))?
            );
        }

        return Ok(0);
    }

    println!("{}", envelope.summary);
    println!("top files:");
    for file in envelope.payload.files_ranked.iter().take(10) {
        println!("- {} ({:.3})", file.path, file.score);
    }
    println!("top symbols:");
    for symbol in envelope.payload.symbols_ranked.iter().take(10) {
        println!("- {} :: {} ({:.3})", symbol.file, symbol.name, symbol.score);
    }

    if let Some(governed) = governed_response {
        let final_packet = governed
            .output_packets
            .first()
            .ok_or_else(|| anyhow!("kernel returned no output packets for governed flow"))?;
        let sections = final_packet
            .body
            .get("assembly")
            .and_then(|assembly| assembly.get("sections_kept"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        println!(
            "governed packet assembled: packet_id={} sections_kept={sections}",
            final_packet.packet_id.as_deref().unwrap_or("unknown")
        );
    }

    Ok(0)
}
