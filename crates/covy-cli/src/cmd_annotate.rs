use std::path::Path;

use anyhow::Result;
use clap::Args;
use covy_core::diagnostics::Severity;
use covy_core::{CoverageData, DiffStatus, FileDiff};

use crate::cmd_common::{
    compute_pr_shared_state, compute_uncovered_blocks_generic, PrSharedState,
};

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
    let shared = compute_pr_shared_state(
        config_path,
        args.base_ref.as_deref(),
        args.head_ref.as_deref(),
    )?;
    render_from_state(&args, &shared)?;
    Ok(0)
}

pub(crate) fn render_from_state(args: &AnnotateArgs, shared: &PrSharedState) -> Result<()> {
    let blocks = uncovered_blocks(&shared.coverage, &shared.diffs);
    let sarif = build_sarif(
        &blocks,
        shared.diagnostics.as_ref(),
        &shared.diffs,
        &shared.gate.violations,
        args.max_findings,
    );

    if let Some(parent) = Path::new(&args.out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&sarif)?)?;
    println!("Wrote SARIF: {}", args.out);

    Ok(())
}

fn uncovered_blocks(coverage: &CoverageData, diffs: &[FileDiff]) -> Vec<BlockFinding> {
    compute_uncovered_blocks_generic(coverage, diffs, |diff, start, end| BlockFinding {
            file: diff.path.clone(),
            start_line: start,
            end_line: end,
            is_new_file: diff.status == DiffStatus::Added,
        })
}

fn build_sarif(
    blocks: &[BlockFinding],
    diagnostics: Option<&covy_core::diagnostics::DiagnosticsData>,
    diffs: &[FileDiff],
    violations: &[String],
    max_findings: usize,
) -> serde_json::Value {
    let mut results = Vec::new();
    'collect: {
        for block in blocks {
            if results.len() >= max_findings {
                break 'collect;
            }
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
                if results.len() >= max_findings {
                    break 'collect;
                }
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
            if results.len() >= max_findings {
                break 'collect;
            }
            results.push(serde_json::json!({
                "ruleId": "covy/policy/threshold-fail",
                "level": "error",
                "message": { "text": violation }
            }));
        }
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
    use roaring::RoaringBitmap;

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

    #[test]
    fn test_build_sarif_limits_findings_without_post_truncate() {
        let blocks = vec![
            BlockFinding {
                file: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                is_new_file: false,
            },
            BlockFinding {
                file: "src/b.rs".to_string(),
                start_line: 2,
                end_line: 2,
                is_new_file: false,
            },
        ];
        let sarif = build_sarif(&blocks, None, &[], &["gate fail".to_string()], 1);
        assert_eq!(sarif["runs"][0]["results"].as_array().unwrap().len(), 1);
    }
}
