use std::path::Path;

use anyhow::Result;
use clap::Args;
use covy_core::{CoverageData, CovyConfig, FileDiff};

use crate::cmd_common::{compute_pr_shared_state, compute_uncovered_blocks_generic, PrSharedState};

#[derive(Args)]
pub struct CommentArgs {
    /// Base ref for diff
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for diff
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Output format (markdown only)
    #[arg(long, default_value = "markdown")]
    pub format: String,

    /// Output path for comment markdown
    #[arg(long = "output", alias = "out")]
    pub output: Option<String>,

    /// Emit JSON summary output
    #[arg(long)]
    pub json: bool,

    /// Maximum uncovered blocks to show
    #[arg(long, default_value_t = 5)]
    pub max_uncovered: usize,

    /// Path to coverage state file
    #[arg(long, default_value = ".covy/state/latest.bin")]
    pub coverage_state_path: String,

    /// Path to diagnostics state file
    #[arg(long, default_value = ".covy/state/issues.bin")]
    pub diagnostics_state_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineBlock {
    file: String,
    start_line: u32,
    end_line: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CommentRenderSummary {
    pub output_path: Option<String>,
    pub wrote_stdout: bool,
    pub bytes: usize,
    pub gate_passed: bool,
}

pub fn run(args: CommentArgs, config_path: &str) -> Result<i32> {
    crate::cmd_common::warn_if_legacy_flag_used("--out", "--output");
    let shared = compute_pr_shared_state(
        config_path,
        args.base_ref.as_deref(),
        args.head_ref.as_deref(),
        &args.coverage_state_path,
        &args.diagnostics_state_path,
    )?;
    let summary = render_from_state(&args, &shared)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }
    Ok(0)
}

pub(crate) fn render_from_state(
    args: &CommentArgs,
    shared: &PrSharedState,
) -> Result<CommentRenderSummary> {
    if !args.format.eq_ignore_ascii_case("markdown") {
        anyhow::bail!(
            "Unsupported --format '{}'; only markdown is supported",
            args.format
        );
    }

    let uncovered = compute_uncovered_blocks(&shared.coverage, &shared.diffs);
    let suggested_tests = suggested_tests(&shared.diffs, &shared.config)?;
    let markdown = render_comment_markdown(
        &shared.gate,
        uncovered,
        args.max_uncovered,
        &suggested_tests,
        shared.gate.changed_coverage_pct,
    );

    let bytes = markdown.len();
    let wrote_stdout = args.output.is_none() && !args.json;

    if let Some(path) = args.output.as_deref() {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, markdown)?;
    } else if wrote_stdout {
        print!("{markdown}");
    }

    Ok(CommentRenderSummary {
        output_path: args.output.clone(),
        wrote_stdout,
        bytes,
        gate_passed: shared.gate.passed,
    })
}

fn suggested_tests(diffs: &[FileDiff], config: &CovyConfig) -> Result<Vec<String>> {
    let path = Path::new(&config.impact.testmap_path);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let bytes = std::fs::read(path)?;
    let map = covy_core::cache::deserialize_testmap(&bytes)?;
    if map.coverage.is_empty() || map.tests.is_empty() {
        return Ok(Vec::new());
    }

    let plan = covy_core::impact::plan_impacted_tests(
        &map,
        diffs,
        config.impact.max_tests,
        config.impact.target_coverage,
    );

    Ok(plan.tests.into_iter().map(|t| t.id).collect())
}

fn compute_uncovered_blocks(coverage: &CoverageData, diffs: &[FileDiff]) -> Vec<LineBlock> {
    compute_uncovered_blocks_generic(coverage, diffs, |diff, start, end| LineBlock {
        file: diff.path.clone(),
        start_line: start,
        end_line: end,
    })
}

fn render_comment_markdown(
    gate: &covy_core::model::QualityGateResult,
    mut uncovered: Vec<LineBlock>,
    max_uncovered: usize,
    suggested_tests: &[String],
    changed_coverage_pct: Option<f64>,
) -> String {
    uncovered.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.end_line.cmp(&b.end_line))
    });

    let status = if gate.passed { "✅" } else { "❌" };
    let changed_str = changed_coverage_pct
        .map(|p| format!("{p:.1}%"))
        .unwrap_or_else(|| "n/a".to_string());

    let mut out = String::new();
    out.push_str(&format!(
        "{status} gate: {} | changed-lines coverage: {} | uncovered blocks: {}\n\n",
        if gate.passed { "pass" } else { "fail" },
        changed_str,
        uncovered.len()
    ));

    out.push_str("### Top Uncovered Blocks\n\n");
    if uncovered.is_empty() {
        out.push_str("- none\n");
    } else {
        for block in uncovered.iter().take(max_uncovered) {
            out.push_str(&format!(
                "- `{}`:{}-{}\n",
                block.file, block.start_line, block.end_line
            ));
        }
    }

    if !suggested_tests.is_empty() {
        out.push_str("\n### Suggested Tests To Run\n\n");
        for test in suggested_tests {
            out.push_str(&format!("- `{test}`\n"));
        }
    }

    if !gate.violations.is_empty() {
        out.push_str("\n### Gate Violations\n\n");
        for violation in &gate.violations {
            out.push_str(&format!("- {violation}\n"));
        }
    }

    out.push_str("\n<!-- covy -->\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;

    #[test]
    fn test_render_comment_markdown_basic_snapshot_shape() {
        let gate = covy_core::model::QualityGateResult {
            passed: false,
            total_coverage_pct: Some(80.0),
            changed_coverage_pct: Some(75.0),
            new_file_coverage_pct: None,
            violations: vec!["Changed lines coverage 75.0% is below threshold 90.0%".to_string()],
            issue_counts: None,
        };
        let blocks = vec![
            LineBlock {
                file: "src/a.rs".to_string(),
                start_line: 10,
                end_line: 12,
            },
            LineBlock {
                file: "src/b.rs".to_string(),
                start_line: 7,
                end_line: 7,
            },
        ];

        let md = render_comment_markdown(&gate, blocks, 5, &["t1".to_string()], Some(75.0));
        assert!(md.contains("❌ gate: fail"));
        assert!(md.contains("changed-lines coverage: 75.0%"));
        assert!(md.contains("`src/a.rs`:10-12"));
        assert!(md.contains("### Suggested Tests To Run"));
        assert!(md.contains("`t1`"));
        assert!(md.contains("<!-- covy -->"));
    }

    #[test]
    fn test_compute_uncovered_blocks_compacts_ranges() {
        let mut coverage = CoverageData::new();
        let mut fc = covy_core::FileCoverage::new();
        fc.lines_instrumented.insert(1);
        fc.lines_instrumented.insert(2);
        fc.lines_instrumented.insert(3);
        fc.lines_instrumented.insert(5);
        fc.lines_covered.insert(1);
        fc.lines_covered.insert(2);
        coverage.files.insert("src/a.rs".to_string(), fc);

        let mut changed = RoaringBitmap::new();
        changed.insert(1);
        changed.insert(2);
        changed.insert(3);
        changed.insert(5);

        let diffs = vec![FileDiff {
            path: "src/a.rs".to_string(),
            old_path: None,
            status: covy_core::DiffStatus::Modified,
            changed_lines: changed,
        }];

        let blocks = compute_uncovered_blocks(&coverage, &diffs);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].start_line, 3);
        assert_eq!(blocks[1].start_line, 5);
    }
}
