use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PacketDetail {
    #[default]
    Compact,
    Rich,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProxyRunRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env_allowlist: Vec<String>,
    pub max_output_bytes: Option<usize>,
    pub max_lines: Option<usize>,
    pub packet_byte_cap: Option<usize>,
    pub detail: PacketDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SummaryGroup {
    pub name: String,
    pub count: usize,
    pub example_line_indexes: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DroppedSummary {
    pub reason: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CommandSummaryPayload {
    pub command: String,
    pub exit_code: i32,
    pub lines_in: usize,
    pub lines_out: usize,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub bytes_saved: usize,
    pub token_saved_est: u64,
    pub groups: Vec<SummaryGroup>,
    pub dropped: Vec<DroppedSummary>,
    pub highlights: Vec<String>,
    pub output_lines: Vec<String>,
}
