use regex::Regex;
use std::sync::LazyLock;

use covy_core::model::{CoverageData, CoverageFormat, FileCoverage};
use covy_core::CovyError;

use crate::Ingestor;

pub struct GoCovIngestor;

// Go coverprofile line: file:startLine.startCol,endLine.endCol numStatements count
static LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+):(\d+)\.\d+,(\d+)\.\d+\s+\d+\s+(\d+)$").unwrap()
});

impl Ingestor for GoCovIngestor {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::GoCov
    }

    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError> {
        let text = std::str::from_utf8(data)
            .map_err(|e| CovyError::Parse {
                format: "gocov".into(),
                detail: format!("Invalid UTF-8: {e}"),
            })?;
        parse_gocov(text)
    }
}

fn parse_gocov(text: &str) -> Result<CoverageData, CovyError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(gocov input)".into(),
        });
    }
    if trimmed.starts_with('<') {
        return Err(CovyError::Parse {
            format: "gocov".into(),
            detail: "Input looks like XML — did you mean --format cobertura or --format jacoco?".into(),
        });
    }

    let mut result = CoverageData::new();
    result.format = Some(CoverageFormat::GoCov);

    // Detect module prefix from the first data line for stripping
    let mut module_prefix: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();

        if line.starts_with("mode:") || line.is_empty() {
            continue;
        }

        if let Some(caps) = LINE_RE.captures(line) {
            let raw_path = &caps[1];
            let start_line: u32 = caps[2].parse().unwrap_or(0);
            let end_line: u32 = caps[3].parse().unwrap_or(0);
            let count: u64 = caps[4].parse().unwrap_or(0);

            // Detect module prefix from first line
            if module_prefix.is_none() {
                // Go paths look like: github.com/user/repo/pkg/file.go
                // We want to strip the module prefix (e.g. "github.com/user/repo/")
                if let Some(idx) = find_go_module_end(raw_path) {
                    module_prefix = Some(raw_path[..idx].to_string());
                } else {
                    module_prefix = Some(String::new());
                }
            }

            // Strip module prefix
            let path = if let Some(ref prefix) = module_prefix {
                if !prefix.is_empty() && raw_path.starts_with(prefix.as_str()) {
                    &raw_path[prefix.len()..]
                } else {
                    raw_path
                }
            } else {
                raw_path
            };

            let fc = result
                .files
                .entry(path.to_string())
                .or_insert_with(FileCoverage::new);

            // Expand line range into bitmap
            for line_no in start_line..=end_line {
                fc.lines_instrumented.insert(line_no);
                if count > 0 {
                    fc.lines_covered.insert(line_no);
                }
            }
        }
    }

    Ok(result)
}

/// Find the end index of the Go module prefix.
/// Go paths: "github.com/user/repo/pkg/file.go"
/// Module is typically 3 segments for github: "github.com/user/repo/"
fn find_go_module_end(path: &str) -> Option<usize> {
    let mut slash_count = 0;
    for (i, c) in path.char_indices() {
        if c == '/' {
            slash_count += 1;
            if slash_count == 3 {
                return Some(i + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gocov_basic() {
        let input = r#"mode: set
github.com/user/repo/pkg/handler.go:10.30,15.2 3 1
github.com/user/repo/pkg/handler.go:17.30,20.2 2 0
github.com/user/repo/main.go:5.13,8.2 2 1
"#;

        let result = parse_gocov(input).unwrap();
        assert_eq!(result.files.len(), 2);

        let handler = &result.files["pkg/handler.go"];
        // Lines 10-15 covered, 17-20 not covered
        assert!(handler.lines_covered.contains(10));
        assert!(handler.lines_covered.contains(15));
        assert!(!handler.lines_covered.contains(17));
        assert!(handler.lines_instrumented.contains(17));

        let main = &result.files["main.go"];
        assert_eq!(main.lines_covered.len(), 4); // lines 5,6,7,8
    }

    #[test]
    fn test_parse_gocov_empty() {
        let input = "mode: atomic\n";
        let result = parse_gocov(input).unwrap();
        assert!(result.files.is_empty());
    }

    #[test]
    fn test_find_module_end() {
        assert_eq!(
            find_go_module_end("github.com/user/repo/pkg/file.go"),
            Some(21)
        );
        assert_eq!(find_go_module_end("main.go"), None);
    }
}
