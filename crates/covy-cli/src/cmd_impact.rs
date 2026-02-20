use anyhow::Result;
use clap::Args;
use covy_core::CovyConfig;
use std::path::Path;

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

pub fn run(_args: ImpactArgs, _config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(_config_path)).unwrap_or_default();
    let base = _args.base.as_deref().unwrap_or(&config.diff.base);
    let head = _args.head.as_deref().unwrap_or(&config.diff.head);

    let bytes = std::fs::read(Path::new(&_args.testmap)).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read testmap at {}: {e}",
            Path::new(&_args.testmap).display()
        )
    })?;
    let map = covy_core::cache::deserialize_testmap(&bytes)?;

    let diffs = covy_core::diff::git_diff(base, head)?;
    let result = covy_core::impact::select_impacted_tests(&map, &diffs);

    if _args.json {
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

    if _args.print_command {
        let command = if result.selected_tests.is_empty() {
            "echo \"no impacted tests\"".to_string()
        } else {
            format!("mvn -Dtest={} test", result.selected_tests.join(","))
        };
        println!("{command}");
    }

    Ok(0)
}
