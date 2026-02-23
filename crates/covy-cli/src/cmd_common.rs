use std::path::Path;

use anyhow::{Context, Result};
use covy_core::diagnostics::DiagnosticsData;
use covy_core::{CoverageData, FileDiff};
use roaring::RoaringBitmap;

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

pub fn compute_uncovered_blocks_generic<T, F>(
    coverage: &CoverageData,
    diffs: &[FileDiff],
    mut make_block: F,
) -> Vec<T>
where
    F: FnMut(&FileDiff, u32, u32) -> T,
{
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
                blocks.push(make_block(diff, start, end));
                start = *line;
                end = *line;
            }
        }
        blocks.push(make_block(diff, start, end));
    }

    blocks
}
