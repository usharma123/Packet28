//! Language-aware content filtering for read operations.
//!
//! Three filter levels:
//! - `None`: No filtering, pass through as-is.
//! - `Minimal`: Strip blank lines and trailing whitespace.
//! - `Aggressive`: Strip comments, collapse function bodies, excessive whitespace.

use serde::{Deserialize, Serialize};

/// Filter intensity level for file content reduction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilterLevel {
    #[default]
    None,
    Minimal,
    Aggressive,
}

/// Programming language for comment/syntax awareness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Ruby,
    Shell,
    Data,
    #[default]
    Unknown,
}

/// Comment syntax patterns for a language.
pub struct CommentPatterns {
    pub line_prefix: &'static [&'static str],
    pub block_start: Option<&'static str>,
    pub block_end: Option<&'static str>,
}

impl Language {
    /// Detect language from file extension.
    pub fn from_extension(ext: &str) -> Self {
        match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" | "pyw" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
            "go" => Language::Go,
            "java" | "kt" | "kts" | "scala" => Language::Java,
            "rb" | "rake" | "gemspec" => Language::Ruby,
            "sh" | "bash" | "zsh" | "fish" => Language::Shell,
            "json" | "yaml" | "yml" | "toml" | "xml" | "csv" | "tsv" => Language::Data,
            _ => Language::Unknown,
        }
    }

    /// Get comment patterns for this language.
    pub fn comment_patterns(&self) -> CommentPatterns {
        match self {
            Language::Rust | Language::Go | Language::Java => CommentPatterns {
                line_prefix: &["//"],
                block_start: Some("/*"),
                block_end: Some("*/"),
            },
            Language::Python => CommentPatterns {
                line_prefix: &["#"],
                block_start: Some("\"\"\""),
                block_end: Some("\"\"\""),
            },
            Language::JavaScript | Language::TypeScript => CommentPatterns {
                line_prefix: &["//"],
                block_start: Some("/*"),
                block_end: Some("*/"),
            },
            Language::Ruby => CommentPatterns {
                line_prefix: &["#"],
                block_start: Some("=begin"),
                block_end: Some("=end"),
            },
            Language::Shell => CommentPatterns {
                line_prefix: &["#"],
                block_start: None,
                block_end: None,
            },
            Language::Data | Language::Unknown => CommentPatterns {
                line_prefix: &[],
                block_start: None,
                block_end: None,
            },
        }
    }
}

/// Apply filter to file content lines.
pub fn apply_filter(lines: &[String], level: FilterLevel, language: Language) -> Vec<String> {
    match level {
        FilterLevel::None => lines.to_vec(),
        FilterLevel::Minimal => apply_minimal_filter(lines),
        FilterLevel::Aggressive => apply_aggressive_filter(lines, language),
    }
}

fn apply_minimal_filter(lines: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut prev_blank = false;

    for line in lines {
        let trimmed_end = line.trim_end();
        let is_blank = trimmed_end.trim().is_empty();

        if is_blank && prev_blank {
            continue;
        }

        result.push(trimmed_end.to_string());
        prev_blank = is_blank;
    }

    // Remove trailing blank lines
    while result.last().is_some_and(|line| line.trim().is_empty()) {
        result.pop();
    }

    result
}

fn apply_aggressive_filter(lines: &[String], language: Language) -> Vec<String> {
    let patterns = language.comment_patterns();
    let mut result = Vec::new();
    let mut in_block_comment = false;
    let mut prev_blank = false;

    for line in lines {
        let trimmed = line.trim();

        // Handle block comments
        if let Some(block_end) = patterns.block_end {
            if in_block_comment {
                if trimmed.contains(block_end) {
                    in_block_comment = false;
                }
                continue;
            }
        }
        if let Some(block_start) = patterns.block_start {
            if trimmed.starts_with(block_start) {
                if let Some(block_end) = patterns.block_end {
                    if !trimmed[block_start.len()..].contains(block_end) {
                        in_block_comment = true;
                    }
                }
                continue;
            }
        }

        // Strip line comments (only pure comment lines, not inline)
        let is_comment = patterns
            .line_prefix
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if is_comment {
            continue;
        }

        let trimmed_end = line.trim_end();
        let is_blank = trimmed_end.trim().is_empty();

        // Collapse consecutive blank lines
        if is_blank && prev_blank {
            continue;
        }

        result.push(trimmed_end.to_string());
        prev_blank = is_blank;
    }

    // Remove trailing blank lines
    while result.last().is_some_and(|line| line.trim().is_empty()) {
        result.pop();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension(".py"), Language::Python);
        assert_eq!(Language::from_extension("tsx"), Language::TypeScript);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("json"), Language::Data);
        assert_eq!(Language::from_extension("xyz"), Language::Unknown);
    }

    #[test]
    fn minimal_filter_collapses_blanks() {
        let lines = vec![
            "fn main() {".to_string(),
            "".to_string(),
            "".to_string(),
            "    println!(\"hello\");".to_string(),
            "".to_string(),
            "}".to_string(),
            "".to_string(),
            "".to_string(),
        ];
        let result = apply_filter(&lines, FilterLevel::Minimal, Language::Rust);
        assert_eq!(
            result,
            vec!["fn main() {", "", "    println!(\"hello\");", "", "}",]
        );
    }

    #[test]
    fn aggressive_filter_strips_comments() {
        let lines = vec![
            "// This is a comment".to_string(),
            "fn main() {".to_string(),
            "    // another comment".to_string(),
            "    println!(\"hello\");".to_string(),
            "}".to_string(),
        ];
        let result = apply_filter(&lines, FilterLevel::Aggressive, Language::Rust);
        assert_eq!(
            result,
            vec!["fn main() {", "    println!(\"hello\");", "}",]
        );
    }

    #[test]
    fn aggressive_filter_strips_block_comments() {
        let lines = vec![
            "/* start".to_string(),
            "middle".to_string(),
            "end */".to_string(),
            "fn main() {}".to_string(),
        ];
        let result = apply_filter(&lines, FilterLevel::Aggressive, Language::Rust);
        assert_eq!(result, vec!["fn main() {}"]);
    }

    #[test]
    fn none_filter_preserves_all() {
        let lines = vec![
            "// comment".to_string(),
            "".to_string(),
            "".to_string(),
            "code".to_string(),
        ];
        let result = apply_filter(&lines, FilterLevel::None, Language::Rust);
        assert_eq!(result, lines);
    }
}
