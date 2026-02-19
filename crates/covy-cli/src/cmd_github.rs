use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use covy_core::config::GateConfig;
use covy_core::diagnostics::DiagnosticsData;
use covy_core::model::CoverageData;
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
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

    // Build a CheckArgs-compatible ingest
    let mut coverage = ingest_coverage(&args, &config)?;

    // Normalize
    let source_root = args.source_root.as_deref().map(Path::new);
    covy_core::pathmap::auto_normalize_paths(&mut coverage, source_root);

    let mut diagnostics = resolve_diagnostics(
        &args.issues,
        args.issues_state.as_deref(),
        args.no_issues_state,
    )?;

    if let Some(diag) = diagnostics.as_mut() {
        covy_core::pathmap::auto_normalize_issue_paths(diag, source_root);
    }

    // Diff
    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    tracing::info!("Computing diff {base}..{head}");
    let diffs = covy_core::diff::git_diff(base, head)?;

    // Gate
    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: config.gate.issues.clone(),
    };

    let gate_result =
        covy_core::gate::evaluate_full_gate(&gate_config, &coverage, diagnostics.as_ref(), &diffs);

    // Render markdown
    let markdown = covy_core::report::render_markdown(
        &coverage,
        &gate_result,
        &diffs,
        args.show_missing,
        diagnostics.as_ref(),
    );

    if args.dry_run {
        print!("{markdown}");
        return Ok(if gate_result.passed { 0 } else { 1 });
    }

    // Post to GitHub
    let token =
        std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN environment variable is required")?;
    let repo = std::env::var("GITHUB_REPOSITORY")
        .context("GITHUB_REPOSITORY environment variable is required (e.g. owner/repo)")?;
    let pr_number = detect_pr_number()
        .context("Could not detect PR number from GITHUB_REF (expected refs/pull/N/merge)")?;

    let api_base =
        std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());

    // Find existing covy comment
    let comments_url = format!("{api_base}/repos/{repo}/issues/{pr_number}/comments");

    let existing_id = find_existing_comment(&comments_url, &token)?;

    if let Some(comment_id) = existing_id {
        // Update existing comment
        let url = format!("{api_base}/repos/{repo}/issues/comments/{comment_id}");
        let body = serde_json::json!({ "body": markdown });
        ureq::patch(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .send_json(&body)
            .context("Failed to update GitHub comment")?;
        tracing::info!("Updated existing comment #{comment_id}");
    } else {
        // Create new comment
        let body = serde_json::json!({ "body": markdown });
        ureq::post(&comments_url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .send_json(&body)
            .context("Failed to create GitHub comment")?;
        tracing::info!("Created new PR comment");
    }

    Ok(if gate_result.passed { 0 } else { 1 })
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

fn ingest_coverage(args: &GithubCommentArgs, config: &CovyConfig) -> Result<CoverageData> {
    let format = match args.format.as_str() {
        "lcov" => Some(covy_core::CoverageFormat::Lcov),
        "cobertura" => Some(covy_core::CoverageFormat::Cobertura),
        "jacoco" => Some(covy_core::CoverageFormat::JaCoCo),
        "gocov" => Some(covy_core::CoverageFormat::GoCov),
        "llvm-cov" => Some(covy_core::CoverageFormat::LlvmCov),
        "auto" => None,
        other => anyhow::bail!("Unknown format: {other}"),
    };

    if args.stdin {
        let fmt = format
            .ok_or_else(|| anyhow::anyhow!("--format is required when reading from --stdin"))?;
        return Ok(covy_ingest::ingest_reader(std::io::stdin().lock(), fmt)?);
    }

    if args.paths.is_empty() {
        anyhow::bail!("No coverage files specified. Provide file paths or use --stdin.");
    }

    let mut files = Vec::new();
    for pattern in &args.paths {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        files.extend(matches);
    }

    if files.is_empty() {
        anyhow::bail!("No coverage files found");
    }

    let strip_prefixes: Vec<&str> = args
        .strip_prefix
        .iter()
        .chain(config.ingest.strip_prefixes.iter())
        .map(|s| s.as_str())
        .collect();

    let mut combined = CoverageData::new();
    for file in &files {
        let data = if let Some(fmt) = format {
            covy_ingest::ingest_path_with_format(file, fmt)?
        } else {
            covy_ingest::ingest_path(file)?
        };

        let data = if strip_prefixes.is_empty() {
            data
        } else {
            let mut result = CoverageData {
                files: std::collections::BTreeMap::new(),
                format: data.format,
                timestamp: data.timestamp,
            };
            for (path, fc) in data.files {
                let mut stripped = path.as_str();
                for prefix in &strip_prefixes {
                    if let Some(rest) = stripped.strip_prefix(prefix) {
                        stripped = rest;
                        break;
                    }
                }
                result.files.insert(stripped.to_string(), fc);
            }
            result
        };

        combined.merge(&data);
    }

    Ok(combined)
}

fn ingest_issues(patterns: &[String]) -> Result<DiagnosticsData> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        files.extend(matches);
    }

    if files.is_empty() {
        anyhow::bail!("No diagnostics files found");
    }

    let mut combined = DiagnosticsData::new();
    for file in &files {
        let data = covy_ingest::ingest_diagnostics_path(file)?;
        combined.merge(&data);
    }

    Ok(combined)
}

fn resolve_diagnostics(
    issues_patterns: &[String],
    issues_state_path: Option<&str>,
    no_issues_state: bool,
) -> Result<Option<DiagnosticsData>> {
    if !issues_patterns.is_empty() {
        return Ok(Some(ingest_issues(issues_patterns)?));
    }

    if no_issues_state {
        return Ok(None);
    }

    let state_path = issues_state_path.unwrap_or(".covy/state/issues.bin");
    let state_path = Path::new(state_path);
    if !state_path.exists() {
        return Ok(None);
    }

    tracing::info!(
        "Loading diagnostics from cached state {}",
        state_path.display()
    );
    let bytes = std::fs::read(state_path)?;
    let diagnostics = covy_core::cache::deserialize_diagnostics(&bytes)?;
    Ok(Some(diagnostics))
}
