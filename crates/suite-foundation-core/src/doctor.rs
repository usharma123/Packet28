use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub fn ensure_git_available() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .context("git is not available in PATH")?;
    if !output.status.success() {
        anyhow::bail!("git command is unavailable");
    }
    Ok(())
}

pub fn validate_git_refs(base: &str, head: &str) -> Result<()> {
    for r in [base, head] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", r])
            .output()
            .with_context(|| format!("Failed to resolve git ref '{r}'"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to resolve git ref '{r}': {stderr}");
        }
    }
    Ok(())
}

pub fn collect_report_paths<F>(report_files: &[PathBuf], mut extract: F) -> Result<Vec<String>>
where
    F: FnMut(&Path) -> Result<Vec<String>>,
{
    let mut paths = Vec::new();
    for report in report_files {
        paths.extend(extract(report)?);
    }
    Ok(paths)
}
