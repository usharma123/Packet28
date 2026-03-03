use std::path::Path;

use anyhow::Result;
use clap::Args;
use suite_foundation_core::CovyConfig;

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
    #[arg(long)]
    pub json: bool,

    /// Emit runnable test command
    #[arg(long)]
    pub print_command: bool,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let base = args
        .base
        .as_deref()
        .unwrap_or(&config.diff.base)
        .to_string();
    let head = args
        .head
        .as_deref()
        .unwrap_or(&config.diff.head)
        .to_string();

    let testmap = if args.testmap == ".covy/state/testmap.bin" {
        config.impact.testmap_path
    } else {
        args.testmap
    };

    let adapters = crate::cmd_common::default_impact_adapters();
    let response = testy_core::pipeline::run_impact(
        testy_core::pipeline::ImpactRequest {
            mode: testy_core::pipeline::ImpactMode::LegacySelect(
                testy_core::pipeline::ImpactLegacyRequest {
                    base_ref: base,
                    head_ref: head,
                    testmap,
                    fresh_hours: config.impact.fresh_hours,
                    full_suite_threshold: config.impact.full_suite_threshold,
                    fallback_mode: config.impact.fallback_mode,
                    smoke_always: config.impact.smoke.always,
                    smoke_stale_extra: config.impact.smoke.stale_extra,
                    include_print_command: args.print_command,
                },
            ),
        },
        &adapters,
    )?;

    let result = response
        .impact_result
        .ok_or_else(|| anyhow::anyhow!("impact response missing result"))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(0);
    }

    if result.selected_tests.is_empty() {
        println!("(no impacted tests)");
    } else {
        for test in &result.selected_tests {
            println!("{test}");
        }
    }

    println!(
        "summary: selected={} known={} missing={} confidence={:.2} stale={} escalate_full_suite={}",
        result.selected_tests.len(),
        response.known_tests.unwrap_or(0),
        result.missing_mappings.len(),
        result.confidence,
        result.stale,
        result.escalate_full_suite
    );

    if args.print_command {
        if let Some(command) = response.print_command {
            println!("{command}");
        }
    }

    Ok(0)
}
