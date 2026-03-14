use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchRequest {
    pub query: String,
    pub requested_paths: Vec<String>,
    pub fixed_string: bool,
    pub case_sensitive: Option<bool>,
    pub whole_word: bool,
    pub context_lines: Option<usize>,
    pub max_matches_per_file: Option<usize>,
    pub max_total_matches: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchGroup {
    pub path: String,
    pub match_count: usize,
    pub displayed_match_count: usize,
    pub truncated: bool,
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchResult {
    pub query: String,
    pub requested_paths: Vec<String>,
    pub resolved_paths: Vec<String>,
    pub match_count: usize,
    pub returned_match_count: usize,
    pub truncated: bool,
    pub paths: Vec<String>,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub groups: Vec<SearchGroup>,
    pub compact_preview: String,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadRegionsRequest {
    pub path: String,
    pub regions: Vec<String>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadLine {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ReadRegionsResult {
    pub path: String,
    pub regions: Vec<String>,
    pub symbols: Vec<String>,
    pub lines: Vec<ReadLine>,
    pub compact_preview: String,
}
