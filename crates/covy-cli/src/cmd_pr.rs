use anyhow::Result;
use clap::Args;

use crate::cmd_common::compute_pr_shared_state;

#[derive(Args)]
pub struct PrArgs {
    /// Output markdown comment artifact path
    #[arg(long = "output-comment", alias = "out-comment")]
    pub output_comment: String,

    /// Output SARIF artifact path
    #[arg(long = "output-sarif", alias = "out-sarif")]
    pub output_sarif: String,

    /// Base ref for diff
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for diff
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Maximum findings in SARIF output
    #[arg(long, default_value_t = 200)]
    pub max_findings: usize,

    /// Emit JSON summary output
    #[arg(long)]
    pub json: bool,

    /// Path to coverage state file
    #[arg(long, default_value = ".covy/state/latest.bin")]
    pub coverage_state_path: String,

    /// Path to diagnostics state file
    #[arg(long, default_value = ".covy/state/issues.bin")]
    pub diagnostics_state_path: String,
}

pub fn run(args: PrArgs, config_path: &str) -> Result<i32> {
    crate::cmd_common::warn_if_legacy_flags_used(&[
        ("--out-comment", "--output-comment"),
        ("--out-sarif", "--output-sarif"),
    ]);
    let shared = compute_pr_shared_state(
        config_path,
        args.base_ref.as_deref(),
        args.head_ref.as_deref(),
        &args.coverage_state_path,
        &args.diagnostics_state_path,
    )?;

    let comment_args = crate::cmd_comment::CommentArgs {
        base_ref: args.base_ref.clone(),
        head_ref: args.head_ref.clone(),
        format: "markdown".to_string(),
        output: Some(args.output_comment.clone()),
        json: args.json,
        max_uncovered: 5,
        coverage_state_path: args.coverage_state_path.clone(),
        diagnostics_state_path: args.diagnostics_state_path.clone(),
    };
    let comment_summary = crate::cmd_comment::render_from_state(&comment_args, &shared)?;

    let annotate_args = crate::cmd_annotate::AnnotateArgs {
        output: args.output_sarif.clone(),
        base_ref: args.base_ref,
        head_ref: args.head_ref,
        max_findings: args.max_findings,
        json: args.json,
        coverage_state_path: args.coverage_state_path,
        diagnostics_state_path: args.diagnostics_state_path,
    };
    let annotate_summary = crate::cmd_annotate::render_from_state(&annotate_args, &shared)?;

    if args.json {
        #[derive(serde::Serialize)]
        struct PrSummary {
            comment: crate::cmd_comment::CommentRenderSummary,
            sarif: crate::cmd_annotate::AnnotateRenderSummary,
        }
        let summary = PrSummary {
            comment: comment_summary,
            sarif: annotate_summary,
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    Ok(0)
}
