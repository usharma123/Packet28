use anyhow::Result;
use clap::Args;

use crate::cmd_common::compute_pr_shared_state;

#[derive(Args)]
pub struct PrArgs {
    /// Output markdown comment artifact path
    #[arg(long)]
    pub out_comment: String,

    /// Output SARIF artifact path
    #[arg(long)]
    pub out_sarif: String,

    /// Base ref for diff
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for diff
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Maximum findings in SARIF output
    #[arg(long, default_value_t = 200)]
    pub max_findings: usize,
}

pub fn run(args: PrArgs, config_path: &str) -> Result<i32> {
    let shared = compute_pr_shared_state(
        config_path,
        args.base_ref.as_deref(),
        args.head_ref.as_deref(),
    )?;

    let comment_args = crate::cmd_comment::CommentArgs {
        base_ref: args.base_ref.clone(),
        head_ref: args.head_ref.clone(),
        format: "markdown".to_string(),
        out: Some(args.out_comment.clone()),
        max_uncovered: 5,
    };
    crate::cmd_comment::render_from_state(&comment_args, &shared)?;

    let annotate_args = crate::cmd_annotate::AnnotateArgs {
        out: args.out_sarif.clone(),
        base_ref: args.base_ref,
        head_ref: args.head_ref,
        max_findings: args.max_findings,
    };
    crate::cmd_annotate::render_from_state(&annotate_args, &shared)?;

    Ok(0)
}
