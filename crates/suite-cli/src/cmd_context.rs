use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use context_memory_core::{
    PacketCache, PersistConfig, RecallMode as MemoryRecallMode, RecallScope as MemoryRecallScope,
};
use serde_json::Value;

pub(crate) use crate::cmd_context_kernel::{
    run_assemble, run_assemble_remote, run_correlate, run_correlate_remote, run_manage,
    run_manage_remote,
};
pub(crate) use crate::cmd_context_recall::{run_recall, run_recall_remote};
pub(crate) use crate::cmd_context_state::{run_state, run_state_remote};
pub(crate) use crate::cmd_context_store::{run_store, run_store_remote};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum RecallScopeArg {
    Global,
    TaskFirst,
    TaskOnly,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum RecallModeArg {
    Auto,
    Conceptual,
    Telemetry,
}

impl From<RecallModeArg> for MemoryRecallMode {
    fn from(value: RecallModeArg) -> Self {
        match value {
            RecallModeArg::Auto => MemoryRecallMode::Auto,
            RecallModeArg::Conceptual => MemoryRecallMode::Conceptual,
            RecallModeArg::Telemetry => MemoryRecallMode::Telemetry,
        }
    }
}

impl From<RecallScopeArg> for MemoryRecallScope {
    fn from(value: RecallScopeArg) -> Self {
        match value {
            RecallScopeArg::Global => MemoryRecallScope::Global,
            RecallScopeArg::TaskFirst => MemoryRecallScope::TaskFirst,
            RecallScopeArg::TaskOnly => MemoryRecallScope::TaskOnly,
        }
    }
}

impl RecallScopeArg {
    pub(crate) fn as_policy_scope(self) -> &'static str {
        match self {
            RecallScopeArg::Global => "global",
            RecallScopeArg::TaskFirst => "task_first",
            RecallScopeArg::TaskOnly => "task_only",
        }
    }
}

#[derive(Args)]
pub struct AssembleArgs {
    /// Path(s) to reducer packet JSON files.
    #[arg(long = "packet", alias = "input", required = true)]
    pub(crate) packets: Vec<String>,

    /// Max approximate token budget for assembled payload.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub(crate) budget_tokens: u64,

    /// Max byte budget for assembled payload JSON.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub(crate) budget_bytes: usize,

    /// Run governed assembly path using this context policy config (context.yaml).
    #[arg(long)]
    pub(crate) context_config: Option<String>,

    /// Persist kernel cache on disk under <cwd>/.packet28
    #[arg(long)]
    pub(crate) cache: bool,

    /// Task identifier for state-aware assembly
    #[arg(long)]
    pub(crate) task_id: Option<String>,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub(crate) json: Option<crate::cmd_common::JsonProfileArg>,

    /// Emit one-release compatibility JSON shape
    #[arg(long)]
    pub(crate) legacy_json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl AssembleArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some() || self.legacy_json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }

    pub(crate) fn governed_requested(&self) -> bool {
        self.context_config.is_some()
    }
}

#[derive(Args)]
pub struct CorrelateArgs {
    /// Path(s) to reducer packet JSON files.
    #[arg(long = "packet", alias = "input", required = true)]
    pub(crate) packets: Vec<String>,

    /// Task identifier for correlation context
    #[arg(long)]
    pub(crate) task_id: Option<String>,

    /// Correlation scope for task-aware cache history
    #[arg(long, value_enum)]
    pub(crate) scope: Option<RecallScopeArg>,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub(crate) json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

#[derive(Args)]
pub struct ManageArgs {
    /// Task identifier for context management
    #[arg(long)]
    pub(crate) task_id: String,

    /// Optional retrieval query; defaults to task snapshot signals
    #[arg(long)]
    pub(crate) query: Option<String>,

    /// Max approximate token budget for the recommended working set.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_TOKENS)]
    pub(crate) budget_tokens: u64,

    /// Max byte budget for the recommended working set.
    #[arg(long, default_value_t = contextq_core::DEFAULT_BUDGET_BYTES)]
    pub(crate) budget_bytes: usize,

    /// Scope for task-aware memory lookup
    #[arg(long, value_enum)]
    pub(crate) scope: Option<RecallScopeArg>,

    /// Optional checkpoint identifier, or omit to use the latest checkpoint
    #[arg(long)]
    pub(crate) checkpoint_id: Option<String>,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub(crate) json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl ManageArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some()
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

impl CorrelateArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some()
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommands,
}

#[derive(Args)]
pub struct StateArgs {
    #[command(subcommand)]
    pub command: StateCommands,
}

#[derive(Subcommand)]
pub enum StateCommands {
    /// Append one task-state event packet
    Append(StateAppendArgs),
    /// Derive the current task-state snapshot
    Snapshot(StateSnapshotArgs),
}

#[derive(Args)]
pub struct StateAppendArgs {
    /// Task identifier for the state event
    #[arg(long)]
    pub(crate) task_id: String,

    /// JSON file describing one task-state event
    #[arg(long)]
    pub(crate) input: String,

    /// State store root directory
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub(crate) json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StateAppendArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some()
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct StateSnapshotArgs {
    /// Task identifier to snapshot
    #[arg(long)]
    pub(crate) task_id: String,

    /// State store root directory
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Emit JSON output profile
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "compact")]
    pub(crate) json: Option<crate::cmd_common::JsonProfileArg>,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StateSnapshotArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json.is_some()
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Subcommand)]
pub enum StoreCommands {
    /// List cached context entries
    #[command(alias = "ls")]
    List(StoreListArgs),
    /// Get one cached context entry by key
    Get(StoreGetArgs),
    /// Prune cached context entries
    #[command(alias = "gc")]
    Prune(StorePruneArgs),
    /// Show context store statistics
    Stats(StoreStatsArgs),
}

#[derive(Args)]
pub struct StoreListArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Optional target substring filter
    #[arg(long)]
    pub(crate) target: Option<String>,

    /// Optional free-text filter over key/target/input hash
    #[arg(long)]
    pub(crate) query: Option<String>,

    /// Optional lower bound for created_at_unix (seconds)
    #[arg(long)]
    pub(crate) created_after: Option<u64>,

    /// Optional upper bound for created_at_unix (seconds)
    #[arg(long)]
    pub(crate) created_before: Option<u64>,

    /// Pagination offset
    #[arg(long, default_value_t = 0)]
    pub(crate) offset: usize,

    /// Maximum entries to return
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: usize,

    /// Emit JSON output
    #[arg(long)]
    pub(crate) json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StoreListArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct StoreGetArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Cache key to fetch
    #[arg(long)]
    pub(crate) key: String,

    /// Emit JSON output
    #[arg(long)]
    pub(crate) json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StoreGetArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct StorePruneArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Remove all entries
    #[arg(long)]
    pub(crate) all: bool,

    /// Remove entries older than this TTL (seconds)
    #[arg(long)]
    pub(crate) ttl_secs: Option<u64>,

    /// Emit JSON output
    #[arg(long)]
    pub(crate) json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StorePruneArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct StoreStatsArgs {
    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Emit JSON output
    #[arg(long)]
    pub(crate) json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl StoreStatsArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

#[derive(Args)]
pub struct RecallArgs {
    /// Retrieval query text
    #[arg(long)]
    pub(crate) query: String,

    /// Store root directory (uses <root>/.packet28/packet-cache-v1.bin)
    #[arg(long, default_value = ".")]
    pub(crate) root: String,

    /// Maximum recall hits
    #[arg(long, default_value_t = 8)]
    pub(crate) limit: usize,

    /// Optional lower bound for created_at_unix (seconds)
    #[arg(long)]
    pub(crate) since: Option<u64>,

    /// Optional upper bound for created_at_unix (seconds)
    #[arg(long)]
    pub(crate) until: Option<u64>,

    /// Optional target substring filter
    #[arg(long)]
    pub(crate) target: Option<String>,

    /// Optional task identifier for task-first memory
    #[arg(long)]
    pub(crate) task_id: Option<String>,

    /// Recall scope; defaults to task-first when task_id is present
    #[arg(long, value_enum)]
    pub(crate) scope: Option<RecallScopeArg>,

    /// Optional packet-type substring filter
    #[arg(long = "packet-type")]
    pub(crate) packet_types: Vec<String>,

    /// Optional path substring filter
    #[arg(long = "path")]
    pub(crate) path_filters: Vec<String>,

    /// Optional symbol substring filter
    #[arg(long = "symbol")]
    pub(crate) symbol_filters: Vec<String>,

    /// Recall scoring lane
    #[arg(long, value_enum, default_value_t = RecallModeArg::Auto)]
    pub(crate) mode: RecallModeArg,

    /// Include superseded or stale curated memory in results
    #[arg(long)]
    pub(crate) include_debug: bool,

    /// Emit JSON output
    #[arg(long)]
    pub(crate) json: bool,

    /// Pretty-print JSON output
    #[arg(long)]
    pub(crate) pretty: bool,
}

impl RecallArgs {
    pub(crate) fn machine_output_requested(&self) -> bool {
        self.json
    }

    pub(crate) fn pretty_output(&self) -> bool {
        self.pretty
    }
}

pub(crate) fn load_cache(root: &str) -> Result<PacketCache> {
    let root_path = PathBuf::from(root);
    let config = PersistConfig::new(root_path.clone());
    Ok(PacketCache::load_from_disk(&config))
}

pub(crate) fn emit_json(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

pub(crate) fn build_kernel(cache: bool, root_dir: PathBuf) -> context_kernel_core::Kernel {
    if cache {
        return context_kernel_core::Kernel::with_v1_reducers_and_persistence(
            context_kernel_core::PersistConfig::new(root_dir),
        );
    }

    context_kernel_core::Kernel::with_v1_reducers()
}

pub(crate) fn build_persistent_kernel(root_dir: PathBuf) -> context_kernel_core::Kernel {
    context_kernel_core::Kernel::with_v1_reducers_and_persistence(
        context_kernel_core::PersistConfig::new(root_dir),
    )
}

pub(crate) fn current_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
