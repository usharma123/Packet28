use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::search::{
    format_region, infer_symbols_from_lines, normalize_capture_path, parse_region_for_path,
};
use crate::types::{ReadLine, ReadRegionsRequest, ReadRegionsResult};

pub fn read_regions(root: &Path, request: &ReadRegionsRequest) -> Result<ReadRegionsResult> {
    let path = normalize_capture_path(root, &request.path);
    anyhow::ensure!(!path.is_empty(), "read_regions requires a valid path");
    let contents = fs::read_to_string(root.join(&path))
        .with_context(|| format!("failed to read '{}'", root.join(&path).display()))?;
    let all_lines = contents
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let mut ranges = request
        .regions
        .iter()
        .filter_map(|region| parse_region_for_path(region, &path))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        match (request.line_start, request.line_end) {
            (Some(start), Some(end)) if start > 0 && end > 0 => {
                ranges.push((start.min(end), start.max(end)));
            }
            (Some(start), None) if start > 0 => ranges.push((start, start)),
            (None, Some(end)) if end > 0 => ranges.push((end, end)),
            _ => ranges.push((1, all_lines.len().min(120).max(1))),
        }
    }
    let mut rendered = Vec::new();
    let mut selected_text = Vec::new();
    let mut normalized_regions = Vec::new();
    for (start, end) in ranges {
        let start = start.min(all_lines.len().max(1));
        let end = end.min(all_lines.len().max(1)).max(start);
        normalized_regions.push(format_region(&path, start, end));
        for line_no in start..=end {
            if let Some(line) = all_lines.get(line_no - 1) {
                rendered.push(ReadLine {
                    line: line_no,
                    text: line.clone(),
                });
                selected_text.push(line.clone());
            }
        }
    }
    Ok(ReadRegionsResult {
        path: path.clone(),
        regions: normalized_regions,
        symbols: infer_symbols_from_lines(&selected_text),
        compact_preview: render_read_compact_preview(&path, &rendered),
        lines: rendered,
    })
}

pub(crate) fn render_read_compact_preview(path: &str, lines: &[ReadLine]) -> String {
    let mut rendered = vec![format!("Read {} line(s) from {}.", lines.len(), path)];
    for item in lines.iter().take(8) {
        rendered.push(format!("{}: {}", item.line, compact_line(&item.text, 120)));
    }
    if lines.len() > 8 {
        rendered.push(format!("+{} more line(s)", lines.len() - 8));
    }
    rendered.join("\n")
}

fn compact_line(line: &str, max_len: usize) -> String {
    let trimmed = line.trim();
    let char_count = trimmed.chars().count();
    if char_count <= max_len {
        return trimmed.to_string();
    }
    let budget = max_len.saturating_sub(3);
    let shortened = trimmed.chars().take(budget).collect::<String>();
    format!("{shortened}...")
}
