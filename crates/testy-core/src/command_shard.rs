use std::path::Path;

use anyhow::Result;
use suite_foundation_core::CovyConfig;
use suite_packet_core::shard::ShardPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerAlgorithmArg {
    Lpt,
    WhaleLpt,
}

#[derive(Debug, Clone)]
pub struct ShardPlanArgs {
    pub shards: Option<usize>,
    pub tasks_json: Option<String>,
    pub tier: String,
    pub include_tag: Vec<String>,
    pub exclude_tag: Vec<String>,
    pub tests_file: Option<String>,
    pub impact_json: Option<String>,
    pub timings: Option<String>,
    pub unknown_test_seconds: Option<f64>,
    pub algorithm: Option<PlannerAlgorithmArg>,
    pub write_files: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShardUpdateArgs {
    pub junit_xml: Vec<String>,
    pub timings_jsonl: Vec<String>,
    pub timings: Option<String>,
    pub export_json: Option<String>,
    pub junit_id_granularity: crate::shard_timing::JunitIdGranularity,
}

pub const SHARD_PLAN_SCHEMA_EXAMPLES: &str = r#"{
  "type": "shard-plan-input-schemas",
  "tasks_json": {
    "schema_version": 1,
    "tasks": [
      {"id": "com.foo.BarTest", "selector": "com.foo.BarTest", "est_ms": 1200, "tags": ["unit"]}
    ]
  },
  "impact_json": {
    "selected_tests": ["com.foo.BarTest", "tests/test_mod.py::test_one"],
    "smoke_tests": [],
    "missing_mappings": [],
    "stale": false,
    "confidence": 1.0,
    "escalate_full_suite": false
  }
}"#;

pub fn run_shard_plan_command(args: ShardPlanArgs, config_path: &str) -> Result<ShardPlan> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let shard_count = args
        .shards
        .ok_or_else(|| anyhow::anyhow!("--shards is required"))?;
    let timings_path = args
        .timings
        .as_deref()
        .unwrap_or(&config.shard.timings_path)
        .to_string();
    let unknown_seconds = args
        .unknown_test_seconds
        .unwrap_or(config.shard.unknown_test_seconds);
    let algorithm = resolve_plan_algorithm(args.algorithm, &config)?;

    let response = crate::pipeline_shard::run_shard(crate::pipeline_shard::ShardRequest {
        mode: crate::pipeline_shard::ShardMode::Plan(crate::pipeline_shard::ShardPlanRequest {
            shard_count,
            tasks_json: args.tasks_json,
            tests_file: args.tests_file,
            impact_json: args.impact_json,
            tier: args.tier,
            include_tag: args.include_tag,
            exclude_tag: args.exclude_tag,
            tier_exclude_tags_pr: config.shard.tiers.pr.exclude_tags,
            tier_exclude_tags_nightly: config.shard.tiers.nightly.exclude_tags,
            timings_path,
            unknown_test_seconds: unknown_seconds,
            algorithm: to_core_algorithm(algorithm),
            write_files: args.write_files,
        }),
    })?;

    response
        .shard_plan
        .ok_or_else(|| anyhow::anyhow!("shard plan response missing shard plan"))
}

pub fn run_shard_update_command(
    args: ShardUpdateArgs,
    config_path: &str,
) -> Result<crate::pipeline_shard::ShardTimingSummary> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let timings_path = args
        .timings
        .as_deref()
        .unwrap_or(&config.shard.timings_path)
        .to_string();

    let response = crate::pipeline_shard::run_shard(crate::pipeline_shard::ShardRequest {
        mode: crate::pipeline_shard::ShardMode::Update(crate::pipeline_shard::ShardUpdateRequest {
            junit_xml: args.junit_xml,
            timings_jsonl: args.timings_jsonl,
            timings_path,
            export_json: args.export_json,
            junit_id_granularity: args.junit_id_granularity,
        }),
    })?;

    response
        .timing_summary
        .ok_or_else(|| anyhow::anyhow!("shard update response missing timing summary"))
}

pub fn resolve_plan_algorithm(
    cli_algorithm: Option<PlannerAlgorithmArg>,
    config: &CovyConfig,
) -> Result<PlannerAlgorithmArg> {
    if let Some(algorithm) = cli_algorithm {
        return Ok(algorithm);
    }

    let configured = config.shard.algorithm.trim();
    if configured.is_empty() {
        return Ok(PlannerAlgorithmArg::Lpt);
    }

    match configured.to_ascii_lowercase().as_str() {
        "lpt" => Ok(PlannerAlgorithmArg::Lpt),
        "whale-lpt" => Ok(PlannerAlgorithmArg::WhaleLpt),
        _ => anyhow::bail!(
            "Unsupported shard algorithm '{}'. Expected 'lpt' or 'whale-lpt'",
            configured
        ),
    }
}

pub fn render_text(plan: &ShardPlan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "shards={} total_ms={} makespan_ms={} imbalance_ratio={:.3} parallel_efficiency={:.3} whale_count={} top_10_share={:.3}\n",
        plan.shards.len(),
        plan.total_predicted_duration_ms,
        plan.makespan_ms,
        plan.imbalance_ratio,
        plan.parallel_efficiency,
        plan.whale_count,
        plan.top_10_share
    ));
    for shard in &plan.shards {
        out.push_str(&format!(
            "shard={} tests={} predicted_ms={}\n",
            shard.id + 1,
            shard.tests.len(),
            shard.predicted_duration_ms
        ));
        for test in &shard.tests {
            out.push_str(test);
            out.push('\n');
        }
    }
    out
}

fn to_core_algorithm(value: PlannerAlgorithmArg) -> crate::pipeline_shard::ShardPlannerAlgorithm {
    match value {
        PlannerAlgorithmArg::Lpt => crate::pipeline_shard::ShardPlannerAlgorithm::Lpt,
        PlannerAlgorithmArg::WhaleLpt => crate::pipeline_shard::ShardPlannerAlgorithm::WhaleLpt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_plan_algorithm_prefers_cli_flag() {
        let cfg = CovyConfig::default();
        let resolved = resolve_plan_algorithm(Some(PlannerAlgorithmArg::WhaleLpt), &cfg).unwrap();
        assert!(matches!(resolved, PlannerAlgorithmArg::WhaleLpt));
    }

    #[test]
    fn test_resolve_plan_algorithm_rejects_invalid_config() {
        let mut cfg = CovyConfig::default();
        cfg.shard.algorithm = "bad".to_string();
        let err = resolve_plan_algorithm(None, &cfg).unwrap_err();
        assert!(err.to_string().contains("Unsupported shard algorithm"));
    }
}
