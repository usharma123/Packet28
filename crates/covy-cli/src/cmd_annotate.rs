use std::path::Path;

use anyhow::Result;
use clap::Args;
use covy_core::config::GateConfig;
use covy_core::diagnostics::Severity;
use covy_core::{CoverageData, CovyConfig, DiffStatus, FileDiff};
use roaring::RoaringBitmap;

use crate::cmd_common::{load_coverage_state, load_diagnostics_if_present};

#[derive(Args)]
pub struct AnnotateArgs {
    /// Output SARIF path
    #[arg(long)]
    pub out: String,

    /// Base ref for diff
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for diff
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Maximum findings to emit
    #[arg(long, default_value_t = 200)]
    pub max_findings: usize,
}

#[derive(Debug, Clone)]
struct BlockFinding {
    file: String,
    start_line: u32,
    end_line: u32,
    is_new_file: bool,
}

pub fn run(args: AnnotateArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path))?;
    let base = args.base_ref.as_deref().unwrap_or(&config.diff.base);
    let head = args.head_ref.as_deref().unwrap_or(&config.diff.head);

    let mut coverage = load_coverage_state(".covy/state/latest.bin")?;
    covy_core::pathmap::auto_normalize_paths(&mut coverage, None);

    let diffs = covy_core::diff::git_diff(base, head)?;
    let diagnostics = load_diagnostics_if_present(".covy/state/issues.bin")?;

    let gate = covy_core::gate::evaluate_full_gate(
        &GateConfig {
            fail_under_total: config.gate.fail_under_total,
            fail_under_changed: config.gate.fail_under_changed,
            fail_under_new: config.gate.fail_under_new,
            issues: config.gate.issues.clone(),
        },
        &coverage,
        diagnostics.as_ref(),
        &diffs,
    );

    let blocks = uncovered_blocks(&coverage, &diffs);
    let sarif = build_sarif(
        &blocks,
        diagnostics.as_ref(),
        &diffs,
        &gate.violations,
        args.max_findings,
    );

    if let Some(parent) = Path::new(&args.out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&sarif)?)?;
    println!("Wrote SARIF: {}", args.out);

    Ok(0)
}

fn uncovered_blocks(coverage: &CoverageData, diffs: &[FileDiff]) -> Vec<BlockFinding> {
    let mut blocks = Vec::new();

    for diff in diffs {
        let mut uncovered = RoaringBitmap::new();
        if let Some(fc) = coverage.files.get(&diff.path) {
            let missing = &fc.lines_instrumented - &fc.lines_covered;
            uncovered |= &(&diff.changed_lines & &missing);
        } else {
            uncovered |= &diff.changed_lines;
        }

        let lines: Vec<u32> = uncovered.iter().collect();
        if lines.is_empty() {
            continue;
        }

        let mut start = lines[0];
        let mut end = lines[0];
        for line in lines.iter().skip(1) {
            if *line == end + 1 {
                end = *line;
            } else {
                blocks.push(BlockFinding {
                    file: diff.path.clone(),
                    start_line: start,
                    end_line: end,
                    is_new_file: diff.status == DiffStatus::Added,
                });
                start = *line;
                end = *line;
            }
        }
        blocks.push(BlockFinding {
            file: diff.path.clone(),
            start_line: start,
            end_line: end,
            is_new_file: diff.status == DiffStatus::Added,
        });
    }

    blocks
}

fn build_sarif(
    blocks: &[BlockFinding],
    diagnostics: Option<&covy_core::diagnostics::DiagnosticsData>,
    diffs: &[FileDiff],
    violations: &[String],
    max_findings: usize,
) -> serde_json::Value {
    let mut results = Vec::new();

    for block in blocks {
        let rule_id = if block.is_new_file {
            "covy/coverage/new-file-uncovered"
        } else {
            "covy/coverage/changed-line-uncovered"
        };
        results.push(sarif_result_with_location(
            rule_id,
            "warning",
            format!(
                "Uncovered changed lines in {}:{}-{}",
                block.file, block.start_line, block.end_line
            ),
            &block.file,
            block.start_line,
            block.end_line,
        ));
    }

    if let Some(diag) = diagnostics {
        for issue in diag.issues_on_changed_lines(diffs) {
            let level = match issue.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Note => "note",
            };
            results.push(sarif_result_with_location(
                "covy/issues/new-on-changed-lines",
                level,
                format!("[{}:{}] {}", issue.source, issue.rule_id, issue.message),
                &issue.path,
                issue.line,
                issue.end_line.unwrap_or(issue.line),
            ));
        }
    }

    for violation in violations {
        results.push(serde_json::json!({
            "ruleId": "covy/policy/threshold-fail",
            "level": "error",
            "message": { "text": violation }
        }));
    }

    if results.len() > max_findings {
        results.truncate(max_findings);
    }

    serde_json::json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "covy",
                    "rules": [
                        {"id": "covy/coverage/changed-line-uncovered"},
                        {"id": "covy/coverage/new-file-uncovered"},
                        {"id": "covy/issues/new-on-changed-lines"},
                        {"id": "covy/policy/threshold-fail"}
                    ]
                }
            },
            "results": results
        }]
    })
}

fn sarif_result_with_location(
    rule_id: &str,
    level: &str,
    message: String,
    file: &str,
    start_line: u32,
    end_line: u32,
) -> serde_json::Value {
    serde_json::json!({
        "ruleId": rule_id,
        "level": level,
        "message": { "text": message },
        "locations": [{
            "physicalLocation": {
                "artifactLocation": { "uri": file },
                "region": {
                    "startLine": start_line,
                    "endLine": end_line
                }
            }
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sarif_has_required_top_level_shape() {
        let blocks = vec![BlockFinding {
            file: "src/a.rs".to_string(),
            start_line: 1,
            end_line: 2,
            is_new_file: false,
        }];
        let sarif = build_sarif(&blocks, None, &[], &[], 200);
        assert_eq!(sarif["version"], "2.1.0");
        assert!(sarif["runs"].is_array());
        assert!(sarif["runs"][0]["tool"]["driver"]["rules"].is_array());
        assert!(sarif["runs"][0]["results"].is_array());
    }

    #[test]
    fn test_uncovered_blocks_generates_exact_locations() {
        let mut coverage = CoverageData::new();
        let mut fc = covy_core::FileCoverage::new();
        fc.lines_instrumented.insert(1);
        fc.lines_instrumented.insert(2);
        fc.lines_instrumented.insert(3);
        fc.lines_covered.insert(1);
        coverage.files.insert("src/a.rs".to_string(), fc);

        let mut changed = RoaringBitmap::new();
        changed.insert(1);
        changed.insert(2);
        changed.insert(3);

        let diffs = vec![FileDiff {
            path: "src/a.rs".to_string(),
            old_path: None,
            status: DiffStatus::Modified,
            changed_lines: changed,
        }];

        let blocks = uncovered_blocks(&coverage, &diffs);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_line, 2);
        assert_eq!(blocks[0].end_line, 3);
    }
}
