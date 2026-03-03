use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::config::GateConfig;
use covy_core::model::CoverageFormat;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct GithubCommentArgs {
    /// Coverage report file paths (supports globs)
    #[arg()]
    paths: Vec<String>,

    /// Coverage format (auto/lcov/cobertura/jacoco/gocov/llvm-cov)
    #[arg(short, long, default_value = "auto")]
    format: String,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Path to cached diagnostics state file (default: .covy/state/issues.bin)
    #[arg(long)]
    issues_state: Option<String>,

    /// Disable automatic diagnostics state loading when --issues is not provided
    #[arg(long)]
    no_issues_state: bool,

    /// Read coverage data from stdin
    #[arg(long)]
    stdin: bool,

    /// Base ref for diff (default: main)
    #[arg(long)]
    base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    head: Option<String>,

    /// Fail if total coverage is below this %
    #[arg(long)]
    fail_under_total: Option<f64>,

    /// Fail if changed lines coverage is below this %
    #[arg(long)]
    fail_under_changed: Option<f64>,

    /// Fail if new file coverage is below this %
    #[arg(long)]
    fail_under_new: Option<f64>,

    /// Prefixes to strip from file paths
    #[arg(long)]
    strip_prefix: Vec<String>,

    /// Source root for resolving relative paths
    #[arg(long)]
    source_root: Option<String>,

    /// Show missing lines in the markdown table
    #[arg(long)]
    show_missing: bool,

    /// Print markdown but don't post to GitHub
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: GithubCommentArgs, config_path: &str) -> Result<i32> {
    crate::cmd_common::maybe_warn_deprecated(
        "warning: `covy github-comment` is deprecated; use `covy comment` + `covy annotate` (or `covy pr`).",
    );

    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: config.gate.issues.clone(),
    };

    let coverage_format = parse_format(&args.format)?;
    let source_root = args.source_root.as_ref().map(PathBuf::from);
    let strip_prefixes: Vec<String> = args
        .strip_prefix
        .iter()
        .cloned()
        .chain(config.ingest.strip_prefixes.iter().cloned())
        .collect();

    let request = covy_core::pipeline::PipelineRequest {
        base: base.to_string(),
        head: head.to_string(),
        source_root,
        coverage: covy_core::pipeline::PipelineCoverageInput {
            paths: args.paths,
            format: coverage_format,
            stdin: args.stdin,
            input_state_path: None,
            default_input_state_path: None,
            strip_prefixes,
            reject_paths_with_input: true,
            no_inputs_error: "No coverage files specified. Provide file paths or use --stdin."
                .to_string(),
        },
        diagnostics: covy_core::pipeline::PipelineDiagnosticsInput {
            issue_patterns: args.issues,
            issues_state_path: args.issues_state,
            no_issues_state: args.no_issues_state,
            default_issues_state_path: ".covy/state/issues.bin".to_string(),
        },
        gate: gate_config,
    };

    let adapters = crate::cmd_common::default_pipeline_ingest_adapters();
    let output = covy_core::pipeline::run_pipeline(request, &adapters)?;

    let markdown = covy_core::report::render_markdown(
        &output.coverage,
        &output.gate_result,
        &output.changed_line_context.diffs,
        args.show_missing,
        output.diagnostics.as_ref(),
    );

    if args.dry_run {
        print!("{markdown}");
        return Ok(if output.gate_result.passed { 0 } else { 1 });
    }

    let token =
        std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN environment variable is required")?;
    let repo = std::env::var("GITHUB_REPOSITORY")
        .context("GITHUB_REPOSITORY environment variable is required (e.g. owner/repo)")?;
    let pr_number = detect_pr_number()
        .context("Could not detect PR number from GITHUB_REF (expected refs/pull/N/merge)")?;

    let api_base =
        std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());

    let comments_url = format!("{api_base}/repos/{repo}/issues/{pr_number}/comments");
    let existing_id = find_existing_comment(&comments_url, &token)?;

    if let Some(comment_id) = existing_id {
        let url = format!("{api_base}/repos/{repo}/issues/comments/{comment_id}");
        let body = serde_json::json!({ "body": markdown });
        ureq::patch(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .send_json(&body)
            .context("Failed to update GitHub comment")?;
        tracing::info!("Updated existing comment #{comment_id}");
    } else {
        let body = serde_json::json!({ "body": markdown });
        ureq::post(&comments_url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .send_json(&body)
            .context("Failed to create GitHub comment")?;
        tracing::info!("Created new PR comment");
    }

    Ok(if output.gate_result.passed { 0 } else { 1 })
}

fn parse_format(s: &str) -> Result<Option<CoverageFormat>> {
    match s {
        "lcov" => Ok(Some(CoverageFormat::Lcov)),
        "cobertura" => Ok(Some(CoverageFormat::Cobertura)),
        "jacoco" => Ok(Some(CoverageFormat::JaCoCo)),
        "gocov" => Ok(Some(CoverageFormat::GoCov)),
        "llvm-cov" => Ok(Some(CoverageFormat::LlvmCov)),
        "auto" => Ok(None),
        other => anyhow::bail!("Unknown format: {other}"),
    }
}

fn detect_pr_number() -> Option<u64> {
    let github_ref = std::env::var("GITHUB_REF").ok()?;
    // GITHUB_REF looks like: refs/pull/123/merge
    let parts: Vec<&str> = github_ref.split('/').collect();
    if parts.len() >= 3 && parts[1] == "pull" {
        parts[2].parse().ok()
    } else {
        None
    }
}

fn find_existing_comment(url: &str, token: &str) -> Result<Option<u64>> {
    let resp: serde_json::Value = ureq::get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("Failed to list GitHub comments")?
        .body_mut()
        .read_json()
        .context("Failed to parse GitHub comments")?;

    if let Some(comments) = resp.as_array() {
        for comment in comments {
            if let Some(body) = comment["body"].as_str() {
                if body.contains("<!-- covy -->") {
                    if let Some(id) = comment["id"].as_u64() {
                        return Ok(Some(id));
                    }
                }
            }
        }
    }

    Ok(None)
}
