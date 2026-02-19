use colored::Colorize;
use comfy_table::{Cell, Color, ContentArrangement, Table};

use crate::model::{CoverageData, FileDiff, QualityGateResult};

/// Render coverage report to terminal.
pub fn render_terminal(coverage: &CoverageData, show_missing: bool, sort_by: &str) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    let mut headers = vec!["File", "Lines", "Covered", "Coverage"];
    if show_missing {
        headers.push("Missing");
    }
    table.set_header(headers);

    let mut entries: Vec<_> = coverage.files.iter().collect();
    match sort_by {
        "coverage" => entries.sort_by(|a, b| {
            let pa = a.1.line_coverage_pct().unwrap_or(0.0);
            let pb = b.1.line_coverage_pct().unwrap_or(0.0);
            pa.partial_cmp(&pb).unwrap()
        }),
        "name" => entries.sort_by_key(|(k, _)| (*k).clone()),
        _ => entries.sort_by_key(|(k, _)| (*k).clone()),
    }

    for (path, fc) in &entries {
        let pct = fc.line_coverage_pct().unwrap_or(0.0);
        let color = coverage_color(pct);
        let pct_str = format!("{pct:.1}%");

        let mut row = vec![
            Cell::new(path),
            Cell::new(fc.lines_instrumented.len()),
            Cell::new(fc.lines_covered.len()),
            Cell::new(&pct_str).fg(color),
        ];

        if show_missing {
            let missing = &fc.lines_instrumented - &fc.lines_covered;
            let missing_str = format_line_ranges(&missing);
            row.push(Cell::new(missing_str));
        }

        table.add_row(row);
    }

    println!("{table}");

    // Summary
    if let Some(total) = coverage.total_coverage_pct() {
        let color_code = if total >= 80.0 {
            "green"
        } else if total >= 60.0 {
            "yellow"
        } else {
            "red"
        };
        let summary = format!("Total coverage: {total:.1}%");
        match color_code {
            "green" => println!("\n{}", summary.green().bold()),
            "yellow" => println!("\n{}", summary.yellow().bold()),
            _ => println!("\n{}", summary.red().bold()),
        }
    } else {
        println!("\n{}", "No coverage data available.".dimmed());
    }
}

/// Render quality gate result to terminal.
pub fn render_gate_result(result: &QualityGateResult) {
    println!();
    if result.passed {
        println!("{}", "╔══════════════════════════════════╗".green());
        println!("{}", "║      Quality Gate: PASSED        ║".green().bold());
        println!("{}", "╚══════════════════════════════════╝".green());
    } else {
        println!("{}", "╔══════════════════════════════════╗".red());
        println!("{}", "║      Quality Gate: FAILED        ║".red().bold());
        println!("{}", "╚══════════════════════════════════╝".red());
    }

    if let Some(pct) = result.total_coverage_pct {
        println!("  Total coverage:         {pct:.1}%");
    }
    if let Some(pct) = result.changed_coverage_pct {
        println!("  Changed lines coverage: {pct:.1}%");
    }
    if let Some(pct) = result.new_file_coverage_pct {
        println!("  New file coverage:      {pct:.1}%");
    }

    for violation in &result.violations {
        println!("  {} {violation}", "✗".red());
    }
    println!();
}

/// Render coverage data as JSON.
pub fn render_json(coverage: &CoverageData) -> String {
    let mut files = Vec::new();
    for (path, fc) in &coverage.files {
        let covered: Vec<u32> = fc.lines_covered.iter().collect();
        let instrumented: Vec<u32> = fc.lines_instrumented.iter().collect();
        let missing: Vec<u32> = (&fc.lines_instrumented - &fc.lines_covered).iter().collect();
        files.push(serde_json::json!({
            "path": path,
            "lines_covered": covered.len(),
            "lines_instrumented": instrumented.len(),
            "coverage_pct": fc.line_coverage_pct().unwrap_or(0.0),
            "missing_lines": missing,
        }));
    }

    let report = serde_json::json!({
        "total_coverage_pct": coverage.total_coverage_pct().unwrap_or(0.0),
        "files": files,
    });

    serde_json::to_string_pretty(&report).unwrap_or_default()
}

/// Render quality gate result as JSON.
pub fn render_gate_json(result: &QualityGateResult) -> String {
    serde_json::to_string_pretty(result).unwrap_or_default()
}

/// Render a markdown coverage report suitable for PR comments.
pub fn render_markdown(
    coverage: &CoverageData,
    gate_result: &QualityGateResult,
    diffs: &[FileDiff],
    show_missing: bool,
) -> String {
    let mut out = String::new();
    out.push_str("## Coverage Report\n\n");

    // Summary table
    out.push_str("| Metric | Value | Threshold | Status |\n");
    out.push_str("|--------|-------|-----------|--------|\n");

    if let Some(total) = gate_result.total_coverage_pct {
        let threshold = gate_result
            .violations
            .iter()
            .find(|v| v.contains("Total coverage"));
        let (thresh_str, status) = if let Some(v) = threshold {
            // Parse threshold from violation message
            let t = extract_threshold(v).unwrap_or_default();
            (format!("{t:.1}%"), "failed")
        } else {
            ("—".into(), "passed")
        };
        let icon = if status == "passed" { "pass" } else { "fail" };
        out.push_str(&format!(
            "| Total | {total:.1}% | {thresh_str} | {icon} |\n"
        ));
    }

    if let Some(changed) = gate_result.changed_coverage_pct {
        let threshold = gate_result
            .violations
            .iter()
            .find(|v| v.contains("Changed lines"));
        let (thresh_str, status) = if let Some(v) = threshold {
            let t = extract_threshold(v).unwrap_or_default();
            (format!("{t:.1}%"), "failed")
        } else {
            ("—".into(), "passed")
        };
        let icon = if status == "passed" { "pass" } else { "fail" };
        out.push_str(&format!(
            "| Changed Lines | {changed:.1}% | {thresh_str} | {icon} |\n"
        ));
    }

    if let Some(new_file) = gate_result.new_file_coverage_pct {
        let threshold = gate_result
            .violations
            .iter()
            .find(|v| v.contains("New file"));
        let (thresh_str, status) = if let Some(v) = threshold {
            let t = extract_threshold(v).unwrap_or_default();
            (format!("{t:.1}%"), "failed")
        } else {
            ("—".into(), "passed")
        };
        let icon = if status == "passed" { "pass" } else { "fail" };
        out.push_str(&format!(
            "| New Files | {new_file:.1}% | {thresh_str} | {icon} |\n"
        ));
    }

    // Changed files detail
    let changed_files: Vec<_> = diffs
        .iter()
        .filter(|d| coverage.files.contains_key(&d.path))
        .collect();

    if !changed_files.is_empty() {
        out.push_str(&format!(
            "\n<details><summary>Changed Files ({})</summary>\n\n",
            changed_files.len()
        ));

        let mut headers = "| File | Coverage | Lines |".to_string();
        if show_missing {
            headers.push_str(" Missing |");
        }
        out.push_str(&headers);
        out.push('\n');

        let mut sep = "|------|----------|-------|".to_string();
        if show_missing {
            sep.push_str("---------|");
        }
        out.push_str(&sep);
        out.push('\n');

        for diff in &changed_files {
            if let Some(fc) = coverage.files.get(&diff.path) {
                let pct = fc.line_coverage_pct().unwrap_or(0.0);
                let changed_covered = (&diff.changed_lines & &fc.lines_covered).len();
                let changed_total = (&diff.changed_lines & &fc.lines_instrumented).len();
                let mut row = format!(
                    "| {} | {pct:.1}% | {changed_covered}/{changed_total} |",
                    diff.path
                );
                if show_missing {
                    let missing = &fc.lines_instrumented - &fc.lines_covered;
                    let changed_missing = &diff.changed_lines & &missing;
                    row.push_str(&format!(" {} |", format_line_ranges(&changed_missing)));
                }
                out.push_str(&row);
                out.push('\n');
            }
        }

        out.push_str("\n</details>\n");
    }

    // Footer
    out.push_str("\n<!-- covy -->\n");
    out
}

/// Render GitHub Actions annotations for uncovered changed lines.
pub fn render_github_annotations(
    coverage: &CoverageData,
    diffs: &[FileDiff],
    gate_result: &QualityGateResult,
) {
    for diff in diffs {
        if let Some(fc) = coverage.files.get(&diff.path) {
            let missing = &fc.lines_instrumented - &fc.lines_covered;
            let uncovered_changed = &diff.changed_lines & &missing;

            for line in uncovered_changed.iter() {
                println!(
                    "::warning file={},line={line}::Line not covered by tests",
                    diff.path
                );
            }
        }
    }

    if !gate_result.passed {
        for violation in &gate_result.violations {
            println!("::error::Quality gate failed: {violation}");
        }
    }
}

/// Extract the threshold number from a violation message like "... below threshold 90.0%"
fn extract_threshold(violation: &str) -> Option<f64> {
    violation
        .rsplit("threshold ")
        .next()
        .and_then(|s| s.trim_end_matches('%').parse().ok())
}

fn coverage_color(pct: f64) -> Color {
    if pct >= 80.0 {
        Color::Green
    } else if pct >= 60.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Format a roaring bitmap as compact line ranges (e.g., "1-3, 7, 10-15").
fn format_line_ranges(bitmap: &roaring::RoaringBitmap) -> String {
    if bitmap.is_empty() {
        return String::new();
    }
    let lines: Vec<u32> = bitmap.iter().collect();
    let mut ranges = Vec::new();
    let mut start = lines[0];
    let mut end = lines[0];

    for &line in &lines[1..] {
        if line == end + 1 {
            end = line;
        } else {
            if start == end {
                ranges.push(format!("{start}"));
            } else {
                ranges.push(format!("{start}-{end}"));
            }
            start = line;
            end = line;
        }
    }
    if start == end {
        ranges.push(format!("{start}"));
    } else {
        ranges.push(format!("{start}-{end}"));
    }

    ranges.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;

    #[test]
    fn test_format_line_ranges() {
        let mut bm = RoaringBitmap::new();
        bm.insert(1);
        bm.insert(2);
        bm.insert(3);
        bm.insert(7);
        bm.insert(10);
        bm.insert(11);
        assert_eq!(format_line_ranges(&bm), "1-3, 7, 10-11");
    }

    #[test]
    fn test_format_line_ranges_empty() {
        let bm = RoaringBitmap::new();
        assert_eq!(format_line_ranges(&bm), "");
    }

    #[test]
    fn test_render_json() {
        let mut coverage = CoverageData::new();
        let mut fc = crate::model::FileCoverage::new();
        fc.lines_covered.insert(1);
        fc.lines_covered.insert(2);
        fc.lines_instrumented.insert(1);
        fc.lines_instrumented.insert(2);
        fc.lines_instrumented.insert(3);
        coverage.files.insert("test.rs".to_string(), fc);

        let json = render_json(&coverage);
        assert!(json.contains("test.rs"));
        assert!(json.contains("66."));
    }
}
