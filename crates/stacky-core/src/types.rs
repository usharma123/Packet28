use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct StackSliceRequest {
    pub log_text: String,
    pub source: Option<String>,
    pub max_failures: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct StackFrame {
    pub raw: String,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub normalized: String,
    pub actionable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FailureSummary {
    pub fingerprint: String,
    pub title: String,
    pub message: String,
    pub occurrences: usize,
    pub frames: Vec<StackFrame>,
    pub first_actionable_frame: Option<StackFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StackSliceOutput {
    pub schema_version: String,
    pub source: Option<String>,
    pub total_failures: usize,
    pub unique_failures: usize,
    pub duplicates_removed: usize,
    pub failures: Vec<FailureSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StackPacket {
    pub packet_id: Option<String>,
    pub tool: Option<String>,
    pub tools: Vec<String>,
    pub reducer: Option<String>,
    pub reducers: Vec<String>,
    pub paths: Vec<String>,
    pub payload: serde_json::Value,
    pub sections: Vec<serde_json::Value>,
    pub refs: Vec<serde_json::Value>,
    pub text_blobs: Vec<String>,
}
