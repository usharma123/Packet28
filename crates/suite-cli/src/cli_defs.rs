use clap::{Args, Parser, Subcommand};

use crate::{
    cmd_agent_prompt, cmd_build, cmd_context, cmd_cover, cmd_daemon, cmd_diff, cmd_doctor,
    cmd_guard, cmd_hook, cmd_impact, cmd_map, cmd_map_repo, cmd_mcp, cmd_packet, cmd_proxy,
    cmd_setup, cmd_shard, cmd_stack,
};

#[derive(Parser)]
#[command(
    name = "Packet28",
    version,
    about = "Umbrella platform CLI for suite domains",
    after_help = "Examples:\n  Packet28 diff analyze --coverage tests/fixtures/lcov/basic.info --base HEAD --head HEAD --json\n  Packet28 agent-prompt --format claude\n  Packet28 daemon status --root . --json\n  Packet28 doctor --root . --json\n  Packet28 context store stats --json\n  Packet28 context recall --query \"missing mappings in parser\" --json"
)]
pub struct Cli {
    /// Path to config file
    #[arg(long, global = true, default_value = "covy.toml")]
    pub config: String,

    /// Write stdout output to a file instead of the terminal
    #[arg(long)]
    pub output: Option<String>,

    /// Route supported command execution through packet28d
    #[arg(long, global = true)]
    pub via_daemon: bool,

    /// Workspace root that owns the packet28d socket/runtime for routed commands
    #[arg(long, global = true)]
    pub daemon_root: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Coverage domain commands
    Cover(CoverArgs),
    /// Diff domain commands
    Diff(DiffArgs),
    /// Test domain commands
    Test(TestArgs),
    /// Guard/policy domain commands
    Guard(GuardArgs),
    /// Context assembly domain commands
    Context(ContextArgs),
    /// Stack trace / failure log reduction commands
    Stack(StackArgs),
    /// Build diagnostics reduction commands
    Build(BuildArgs),
    /// Repo mapping commands
    Map(MapArgs),
    /// Safe command proxy/reduction commands
    Proxy(cmd_proxy::ProxyArgs),
    /// Packet artifact utilities
    Packet(cmd_packet::PacketArgs),
    /// Emit repo-local agent instruction fragments that describe how to use Packet28
    AgentPrompt(cmd_agent_prompt::AgentPromptArgs),
    /// Run Packet28 as an MCP stdio server
    Mcp(cmd_mcp::McpArgs),
    /// Run Packet28-managed Claude hook handlers
    Hook(cmd_hook::HookArgs),
    /// Daemon lifecycle and task commands
    Daemon(cmd_daemon::DaemonArgs),
    /// Verify Packet28 daemon, index, MCP, notifications, and broker round-trip health
    Doctor(cmd_doctor::DoctorArgs),
    /// Configure Packet28 for your agent runtimes (Claude Code, Cursor, Codex)
    Setup(cmd_setup::SetupArgs),
}

#[derive(Args)]
pub struct CoverArgs {
    #[command(subcommand)]
    pub command: CoverCommands,
}

#[derive(Subcommand)]
pub enum CoverCommands {
    /// Analyze coverage quality gate
    Check(cmd_cover::CheckArgs),
}

#[derive(Args)]
pub struct DiffArgs {
    #[command(subcommand)]
    pub command: DiffCommands,
}

#[derive(Subcommand)]
pub enum DiffCommands {
    /// Analyze a git diff and evaluate quality gate
    Analyze(cmd_diff::AnalyzeArgs),
}

#[derive(Args)]
pub struct TestArgs {
    #[command(subcommand)]
    pub command: TestCommands,
}

#[derive(Subcommand)]
pub enum TestCommands {
    /// Compute impacted tests from a git diff
    Impact(cmd_impact::ImpactArgs),
    /// Plan test shard allocations
    Shard(cmd_shard::ShardArgs),
    /// Build test impact map artifacts
    Map(cmd_map::MapArgs),
}

#[derive(Args)]
pub struct GuardArgs {
    #[command(subcommand)]
    pub command: GuardCommands,
}

#[derive(Subcommand)]
pub enum GuardCommands {
    /// Validate guard policy config (context.yaml) shape and rule syntax
    Validate(cmd_guard::ValidateArgs),
    /// Evaluate one packet against guard policy config
    Check(cmd_guard::CheckArgs),
}

#[derive(Args)]
#[command(
    after_help = "Examples:\n  Packet28 context assemble --packet a.json --packet b.json --context-config context.yaml\n  Packet28 context store list --root . --limit 20\n  Packet28 context recall --query \"what changed in parser\" --limit 5\n  Packet28 context manage --task-id task-123 --budget-tokens 4000 --budget-bytes 32000"
)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub command: ContextCommands,
}

#[derive(Subcommand)]
pub enum ContextCommands {
    /// Merge multiple reducer packets into a bounded final packet
    #[command(alias = "merge")]
    Assemble(cmd_context::AssembleArgs),
    /// Correlate multiple packets into a synthesized insight packet
    Correlate(cmd_context::CorrelateArgs),
    /// Produce budget-aware task context management guidance
    Manage(cmd_context::ManageArgs),
    /// Write and inspect agent task state
    State(cmd_context::StateArgs),
    /// Query and manage persisted context store entries
    Store(cmd_context::StoreArgs),
    /// Recall prior context entries by semantic/lexical query
    Recall(cmd_context::RecallArgs),
}

#[derive(Args)]
pub struct StackArgs {
    #[command(subcommand)]
    pub command: StackCommands,
}

#[derive(Subcommand)]
pub enum StackCommands {
    /// Parse stack traces/failing logs into deduped failure packets
    Slice(cmd_stack::SliceArgs),
}

#[derive(Args)]
pub struct BuildArgs {
    #[command(subcommand)]
    pub command: BuildCommands,
}

#[derive(Subcommand)]
pub enum BuildCommands {
    /// Parse compiler/linter output into deduped build diagnostic packets
    Reduce(cmd_build::ReduceArgs),
}

#[derive(Args)]
pub struct MapArgs {
    #[command(subcommand)]
    pub command: MapCommands,
}

#[derive(Subcommand)]
pub enum MapCommands {
    /// Build deterministic repo map packet
    Repo(cmd_map_repo::RepoArgs),
}
