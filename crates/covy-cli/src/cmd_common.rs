use std::path::Path;

use anyhow::{Context, Result};
use covy_core::diagnostics::DiagnosticsData;
use covy_core::CoverageData;

pub fn load_coverage_state(path: &str) -> Result<CoverageData> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read coverage state at {path}"))?;
    covy_core::cache::deserialize_coverage(&bytes).map_err(Into::into)
}

pub fn load_diagnostics_if_present(path: &str) -> Result<Option<DiagnosticsData>> {
    if !Path::new(path).exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let mut data = covy_core::cache::deserialize_diagnostics(&bytes)?;
    covy_core::pathmap::auto_normalize_issue_paths(&mut data, None);
    Ok(Some(data))
}
