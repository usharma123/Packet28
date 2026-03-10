use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    log_path, read_runtime_info, read_socket_message, ready_path, resolve_workspace_root,
    socket_path, write_socket_message, ContextRecallRequest, ContextRecallResponse,
    ContextStoreGetRequest, ContextStoreGetResponse, ContextStoreListRequest,
    ContextStoreListResponse, ContextStorePruneDaemonRequest, ContextStorePruneResponse,
    ContextStoreStatsRequest, ContextStoreStatsResponse, CoverCheckRequest, CoverCheckResponse,
    DaemonEventFrame, DaemonIndexClearRequest, DaemonIndexRebuildRequest, DaemonIndexStatusRequest,
    DaemonRequest, DaemonResponse, PacketFetchRequest, PacketFetchResponse, TaskSubmitSpec,
    TestMapRequest, TestMapResponse, TestShardRequest, TestShardResponse,
};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
const DAEMON_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(unix)]
pub struct PersistentDaemonClient {
    root: PathBuf,
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

#[derive(Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommands,
}

#[derive(Subcommand)]
pub enum DaemonCommands {
    Start(StatusRootArgs),
    Stop(StatusRootArgs),
    Status(JsonRootArgs),
    Task(TaskArgs),
    Watch(WatchArgs),
    Index(IndexArgs),
}

#[derive(Args)]
pub struct StatusRootArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
}

#[derive(Args)]
pub struct JsonRootArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommands,
}

#[derive(Subcommand)]
pub enum TaskCommands {
    Submit(TaskSubmitArgs),
    Status(TaskStatusArgs),
    Cancel(TaskCancelArgs),
    Watch(TaskWatchArgs),
}

#[derive(Args)]
pub struct TaskSubmitArgs {
    #[arg(long)]
    pub spec: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskStatusArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskCancelArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct TaskWatchArgs {
    #[arg(long)]
    pub task_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long, default_value_t = 0)]
    pub replay_last: usize,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct WatchArgs {
    #[command(subcommand)]
    pub command: WatchCommands,
}

#[derive(Args)]
pub struct IndexArgs {
    #[command(subcommand)]
    pub command: IndexCommands,
}

#[derive(Subcommand)]
pub enum IndexCommands {
    Status(JsonRootArgs),
    Rebuild(IndexRebuildArgs),
    Clear(JsonRootArgs),
}

#[derive(Args)]
pub struct IndexRebuildArgs {
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub full: bool,
    #[arg(long = "path")]
    pub paths: Vec<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Subcommand)]
pub enum WatchCommands {
    List(WatchListArgs),
    Remove(WatchRemoveArgs),
}

#[derive(Args)]
pub struct WatchListArgs {
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct WatchRemoveArgs {
    #[arg(long)]
    pub watch_id: String,
    #[arg(long, default_value = ".")]
    pub root: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub pretty: bool,
}

#[cfg(unix)]
pub fn run(args: DaemonArgs) -> Result<i32> {
    match args.command {
        DaemonCommands::Start(args) => run_start(args),
        DaemonCommands::Stop(args) => run_stop(args),
        DaemonCommands::Status(args) => run_status(args),
        DaemonCommands::Task(args) => run_task(args),
        DaemonCommands::Watch(args) => run_watch(args),
        DaemonCommands::Index(args) => run_index(args),
    }
}

#[cfg(not(unix))]
pub fn run(_args: DaemonArgs) -> Result<i32> {
    daemon_not_supported()
}

#[cfg(unix)]
pub fn run_via_daemon(cli: crate::Cli, _raw_args: &[String]) -> Result<i32> {
    let daemon_root = daemon_workspace_root(cli.daemon_root.as_deref())?;
    match cli.command {
        crate::Commands::Cover(cover) => match cover.command {
            crate::CoverCommands::Check(args) => {
                crate::cmd_cover::run_remote(args, &cli.config, &daemon_root)
            }
        },
        crate::Commands::Diff(diff) => match diff.command {
            crate::DiffCommands::Analyze(args) => {
                crate::cmd_diff::run_remote(args, &cli.config, &daemon_root)
            }
        },
        crate::Commands::Test(test) => match test.command {
            crate::TestCommands::Impact(args) => {
                crate::cmd_impact::run_remote(args, &cli.config, &daemon_root)
            }
            crate::TestCommands::Shard(args) => {
                crate::cmd_shard::run_remote(args, &cli.config, &daemon_root)
            }
            crate::TestCommands::Map(args) => crate::cmd_map::run_remote(args, &daemon_root),
        },
        crate::Commands::Context(context) => match context.command {
            crate::ContextCommands::Assemble(args) => {
                crate::cmd_context::run_assemble_remote(args, &daemon_root)
            }
            crate::ContextCommands::Correlate(args) => {
                crate::cmd_context::run_correlate_remote(args, &daemon_root)
            }
            crate::ContextCommands::Manage(args) => {
                crate::cmd_context::run_manage_remote(args, &daemon_root)
            }
            crate::ContextCommands::State(args) => {
                crate::cmd_context::run_state_remote(args, &daemon_root)
            }
            crate::ContextCommands::Store(args) => {
                crate::cmd_context::run_store_remote(args, &daemon_root)
            }
            crate::ContextCommands::Recall(args) => {
                crate::cmd_context::run_recall_remote(args, &daemon_root)
            }
        },
        crate::Commands::Stack(stack) => match stack.command {
            crate::StackCommands::Slice(args) => crate::cmd_stack::run_remote(args, &daemon_root),
        },
        crate::Commands::Build(build) => match build.command {
            crate::BuildCommands::Reduce(args) => crate::cmd_build::run_remote(args, &daemon_root),
        },
        crate::Commands::Map(map) => match map.command {
            crate::MapCommands::Repo(args) => crate::cmd_map_repo::run_remote(args, &daemon_root),
        },
        crate::Commands::Proxy(proxy) => match proxy.command {
            crate::cmd_proxy::ProxyCommands::Run(args) => {
                crate::cmd_proxy::run_remote(args, &daemon_root)
            }
        },
        crate::Commands::Packet(packet) => match packet.command {
            crate::cmd_packet::PacketCommands::Fetch(args) => {
                crate::cmd_packet::run_fetch_remote(args, &daemon_root)
            }
        },
        crate::Commands::Preflight(args) => {
            crate::cmd_preflight::run_remote(args, &cli.config, &daemon_root)
        }
        other => {
            let cli = crate::Cli {
                command: other,
                via_daemon: false,
                ..cli
            };
            crate::run_cli_local(cli)
        }
    }
}

#[cfg(not(unix))]
pub fn run_via_daemon(_cli: crate::Cli, _raw_args: &[String]) -> Result<i32> {
    daemon_not_supported()
}

pub fn via_daemon_env_enabled() -> bool {
    crate::cmd_common::parse_daemon_env_flag(std::env::var("PACKET28_VIA_DAEMON").ok().as_deref())
}

pub fn daemon_root_env() -> Option<String> {
    std::env::var("PACKET28_DAEMON_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn daemon_workspace_root(explicit_root: Option<&str>) -> Result<PathBuf> {
    let start = if let Some(root) = explicit_root {
        PathBuf::from(root)
    } else if let Some(root) = daemon_root_env() {
        PathBuf::from(root)
    } else {
        std::env::current_dir().context("failed to resolve current directory")?
    };
    Ok(resolve_workspace_root(&start))
}

fn normalize_daemon_root(root: &Path) -> PathBuf {
    resolve_workspace_root(root)
}

#[cfg(not(unix))]
fn daemon_not_supported<T>() -> Result<T> {
    Err(anyhow!(
        "packet28 daemon commands are only supported on Unix targets"
    ))
}

pub fn execute_kernel_request(
    root: &Path,
    request: context_kernel_core::KernelRequest,
) -> Result<context_kernel_core::KernelResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::Execute { request })? {
        DaemonResponse::Execute { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn send_kernel_request(
    root: &Path,
    request: context_kernel_core::KernelRequest,
) -> Result<context_kernel_core::KernelResponse> {
    execute_kernel_request(root, request)
}

pub fn execute_sequence(
    root: &Path,
    spec: TaskSubmitSpec,
) -> Result<packet28_daemon_core::SequenceSubmitResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ExecuteSequence { spec })? {
        DaemonResponse::ExecuteSequence {
            response,
            task,
            watches,
        } => Ok(packet28_daemon_core::SequenceSubmitResponse {
            task_id: task.task_id,
            watch_ids: watches.iter().map(|watch| watch.watch_id.clone()).collect(),
            response,
        }),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_cover_check(root: &Path, request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::CoverCheck { request })? {
        DaemonResponse::CoverCheck { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_packet_fetch(
    root: &Path,
    request: PacketFetchRequest,
) -> Result<PacketFetchResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::PacketFetch { request })? {
        DaemonResponse::PacketFetch { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn send_cover_check(root: &Path, request: CoverCheckRequest) -> Result<CoverCheckResponse> {
    execute_cover_check(root, request)
}

pub fn send_packet_fetch(root: &Path, request: PacketFetchRequest) -> Result<PacketFetchResponse> {
    execute_packet_fetch(root, request)
}

pub fn execute_test_shard(root: &Path, request: TestShardRequest) -> Result<TestShardResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::TestShard { request })? {
        DaemonResponse::TestShard { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_test_map(root: &Path, request: TestMapRequest) -> Result<TestMapResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::TestMap { request })? {
        DaemonResponse::TestMap { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_list(
    root: &Path,
    request: ContextStoreListRequest,
) -> Result<ContextStoreListResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreList { request })? {
        DaemonResponse::ContextStoreList { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_get(
    root: &Path,
    request: ContextStoreGetRequest,
) -> Result<ContextStoreGetResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreGet { request })? {
        DaemonResponse::ContextStoreGet { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_prune(
    root: &Path,
    request: ContextStorePruneDaemonRequest,
) -> Result<ContextStorePruneResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStorePrune { request })? {
        DaemonResponse::ContextStorePrune { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_store_stats(
    root: &Path,
    request: ContextStoreStatsRequest,
) -> Result<ContextStoreStatsResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextStoreStats { request })? {
        DaemonResponse::ContextStoreStats { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn execute_context_recall(
    root: &Path,
    request: ContextRecallRequest,
) -> Result<ContextRecallResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ContextRecall { request })? {
        DaemonResponse::ContextRecall { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(unix)]
pub fn send_request(root: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let mut client = PersistentDaemonClient::connect(root)?;
    client.send_request(request)
}

#[cfg(unix)]
fn subscribe_task(root: &Path, task_id: &str, replay_last: usize) -> Result<(UnixStream, usize)> {
    let socket = socket_path(root);
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to daemon socket '{}'", socket.display()))?;
    stream
        .set_read_timeout(None)
        .context("failed to configure subscribe read timeout")?;
    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut reader = BufReader::new(stream.try_clone()?);
    write_socket_message(
        &mut writer,
        &DaemonRequest::TaskSubscribe {
            task_id: task_id.to_string(),
            replay_last,
        },
    )?;
    match read_socket_message(&mut reader)? {
        DaemonResponse::TaskSubscribeAck { replayed, .. } => Ok((stream, replayed)),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

#[cfg(not(unix))]
pub fn send_request(_root: &Path, _request: &DaemonRequest) -> Result<DaemonResponse> {
    daemon_not_supported()
}

#[cfg(unix)]
impl PersistentDaemonClient {
    pub fn connect(root: &Path) -> Result<Self> {
        let root = normalize_daemon_root(root);
        ensure_daemon(&root)?;
        let socket = socket_path(&root);
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("failed to connect to '{}'", socket.display()))?;
        stream
            .set_read_timeout(Some(DAEMON_SOCKET_TIMEOUT))
            .with_context(|| {
                format!(
                    "failed to configure read timeout for '{}'",
                    socket.display()
                )
            })?;
        stream
            .set_write_timeout(Some(DAEMON_SOCKET_TIMEOUT))
            .with_context(|| {
                format!(
                    "failed to configure write timeout for '{}'",
                    socket.display()
                )
            })?;
        let reader_stream = stream.try_clone()?;
        reader_stream
            .set_read_timeout(Some(DAEMON_SOCKET_TIMEOUT))
            .with_context(|| {
                format!(
                    "failed to configure cloned read timeout for '{}'",
                    socket.display()
                )
            })?;
        reader_stream
            .set_write_timeout(Some(DAEMON_SOCKET_TIMEOUT))
            .with_context(|| {
                format!(
                    "failed to configure cloned write timeout for '{}'",
                    socket.display()
                )
            })?;
        Ok(Self {
            root,
            reader: BufReader::new(reader_stream),
            writer: BufWriter::new(stream),
        })
    }

    pub fn send_request(&mut self, request: &DaemonRequest) -> Result<DaemonResponse> {
        write_socket_message(&mut self.writer, request)?;
        read_socket_message(&mut self.reader)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(unix)]
pub fn ensure_daemon(root: &Path) -> Result<()> {
    let root = normalize_daemon_root(root);
    if socket_path(&root).exists() && UnixStream::connect(socket_path(&root)).is_ok() {
        return Ok(());
    }
    start_daemon(&root)?;
    wait_for_daemon(&root, Duration::from_secs(10))
}

#[cfg(not(unix))]
pub fn ensure_daemon(_root: &Path) -> Result<()> {
    daemon_not_supported()
}

pub fn resolve_root_arg(root: &str) -> PathBuf {
    let cwd = PathBuf::from(root);
    resolve_workspace_root(&cwd)
}

fn run_start(args: StatusRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    ensure_daemon(&root)?;
    println!("daemon_started root={}", root.display());
    Ok(0)
}

fn run_stop(args: StatusRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    match send_request(&root, &DaemonRequest::Stop) {
        Ok(DaemonResponse::Ack { message }) => {
            println!("{message}");
            Ok(0)
        }
        Ok(DaemonResponse::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!("unexpected daemon response: {other:?}")),
        Err(_) => {
            // The daemon may exit before sending the response — treat as success
            println!("stopping");
            Ok(0)
        }
    }
}

fn run_status(args: JsonRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    match send_request(&root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => {
            if args.json {
                crate::cmd_common::emit_json(&serde_json::to_value(status)?, args.pretty)?;
            } else {
                println!("pid={}", status.pid);
                println!("root={}", status.workspace_root);
                println!("socket={}", status.socket_path);
                println!("log={}", status.log_path);
                println!("tasks={}", status.tasks.len());
                println!("watches={}", status.watches.len());
                if let Some(index) = status.index {
                    println!(
                        "index={} generation={} ready={} dirty={}",
                        index.manifest.status,
                        index.manifest.generation,
                        index.ready,
                        index.dirty_file_count
                    );
                }
            }
            Ok(0)
        }
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn run_index(args: IndexArgs) -> Result<i32> {
    match args.command {
        IndexCommands::Status(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexStatus {
                    request: DaemonIndexStatusRequest {
                        root: root.to_string_lossy().to_string(),
                    },
                },
            )? {
                DaemonResponse::DaemonIndexStatus { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("status={}", response.manifest.status);
                        println!("generation={}", response.manifest.generation);
                        println!("ready={}", response.ready);
                        println!("fallback_mode={}", response.fallback_mode);
                        println!("dirty_files={}", response.dirty_file_count);
                        println!("queued_files={}", response.queued_file_count);
                        println!("indexed_files={}", response.manifest.indexed_files);
                        println!("total_files={}", response.manifest.total_files);
                        if let Some(err) = response.manifest.last_error {
                            println!("last_error={err}");
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        IndexCommands::Rebuild(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            let full = args.full || args.paths.is_empty();
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexRebuild {
                    request: DaemonIndexRebuildRequest {
                        root: root.to_string_lossy().to_string(),
                        full,
                        paths: args.paths,
                    },
                },
            )? {
                DaemonResponse::DaemonIndexRebuild { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("accepted={}", response.accepted);
                        println!("full={}", response.full);
                        if let Some(generation) = response.generation {
                            println!("generation={generation}");
                        }
                        if !response.queued_paths.is_empty() {
                            println!("queued_paths={}", response.queued_paths.join(","));
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        IndexCommands::Clear(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::DaemonIndexClear {
                    request: DaemonIndexClearRequest {
                        root: root.to_string_lossy().to_string(),
                    },
                },
            )? {
                DaemonResponse::DaemonIndexClear { response } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::to_value(response)?,
                            args.pretty,
                        )?;
                    } else {
                        println!("cleared={}", response.cleared);
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
    }
}

fn run_task(args: TaskArgs) -> Result<i32> {
    match args.command {
        TaskCommands::Submit(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            let raw = std::fs::read_to_string(&args.spec)
                .with_context(|| format!("failed to read task spec '{}'", args.spec))?;
            let spec: TaskSubmitSpec = serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in '{}'", args.spec))?;
            match send_request(&root, &DaemonRequest::ExecuteSequence { spec })? {
                DaemonResponse::ExecuteSequence {
                    response,
                    task,
                    watches,
                } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::json!({
                                "task": task,
                                "watches": watches,
                                "response": response,
                            }),
                            args.pretty,
                        )?;
                    } else {
                        let ids = watches
                            .iter()
                            .map(|watch| watch.watch_id.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                        println!(
                            "task={} request_id={} watch_ids={}",
                            task.task_id, response.request_id, ids
                        );
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Status(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskStatus {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::TaskStatus { task } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(task)?, args.pretty)?;
                    } else if let Some(task) = task {
                        println!("task={}", task.task_id);
                        println!("running={}", task.running);
                        println!("watch_ids={}", task.watch_ids.join(","));
                    } else {
                        println!("task not found");
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Cancel(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::TaskCancel {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::TaskCancel {
                    task,
                    removed_watch_ids,
                } => {
                    if args.json {
                        crate::cmd_common::emit_json(
                            &serde_json::json!({
                                "task": task,
                                "removed_watch_ids": removed_watch_ids,
                            }),
                            args.pretty,
                        )?;
                    } else {
                        println!("removed_watch_ids={}", removed_watch_ids.join(","));
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        TaskCommands::Watch(args) => {
            #[cfg(not(unix))]
            {
                let _ = args;
                return daemon_not_supported();
            }
            #[cfg(unix)]
            {
                let root = resolve_root_arg(&args.root);
                ensure_daemon(&root)?;
                let (stream, replayed) = subscribe_task(&root, &args.task_id, args.replay_last)?;
                let mut reader = BufReader::new(stream);
                if !args.json {
                    println!("task={} replayed={}", args.task_id, replayed);
                }
                loop {
                    let frame: DaemonEventFrame = match read_socket_message(&mut reader) {
                        Ok(frame) => frame,
                        Err(err) => {
                            if args.json {
                                return Err(err);
                            }
                            println!("stream closed");
                            return Ok(0);
                        }
                    };
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(frame)?, args.pretty)?;
                        continue;
                    }
                    println!(
                        "[{}] seq={} kind={}",
                        frame.event.occurred_at_unix, frame.seq, frame.event.kind
                    );
                    if let Some(text) = frame
                        .event
                        .data
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                    {
                        println!("  {text}");
                    } else if let Some(step_id) = frame
                        .event
                        .data
                        .get("step_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        println!("  step={step_id}");
                    } else if let Some(paths) = frame
                        .event
                        .data
                        .get("paths")
                        .and_then(serde_json::Value::as_array)
                    {
                        let joined = paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",");
                        if !joined.is_empty() {
                            println!("  paths={joined}");
                        }
                    }
                }
            }
        }
    }
}

fn run_watch(args: WatchArgs) -> Result<i32> {
    match args.command {
        WatchCommands::List(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::WatchList {
                    task_id: args.task_id,
                },
            )? {
                DaemonResponse::WatchList { watches } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(watches)?, args.pretty)?;
                    } else {
                        for watch in watches {
                            println!(
                                "watch_id={} task_id={} kind={:?} paths={}",
                                watch.watch_id,
                                watch.spec.task_id,
                                watch.spec.kind,
                                watch.spec.paths.join(",")
                            );
                        }
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
        WatchCommands::Remove(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(
                &root,
                &DaemonRequest::WatchRemove {
                    watch_id: args.watch_id,
                },
            )? {
                DaemonResponse::WatchRemove { removed } => {
                    if args.json {
                        crate::cmd_common::emit_json(&serde_json::to_value(removed)?, args.pretty)?;
                    } else if let Some(watch) = removed {
                        println!("removed watch_id={}", watch.watch_id);
                    } else {
                        println!("watch not found");
                    }
                    Ok(0)
                }
                DaemonResponse::Error { message } => Err(anyhow!(message)),
                other => Err(anyhow!("unexpected daemon response: {other:?}")),
            }
        }
    }
}

#[cfg(unix)]
fn start_daemon(root: &Path) -> Result<()> {
    let binary = packet28d_binary()?;
    let root_arg = root.to_string_lossy().to_string();
    let log_path = log_path(root);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create daemon log dir '{}'", parent.display()))?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log '{}'", log_path.display()))?;
    Command::new(binary)
        .arg("serve")
        .arg("--root")
        .arg(root_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn packet28d")?;
    Ok(())
}

#[cfg(unix)]
fn wait_for_daemon(root: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if ready_path(root).exists()
            && socket_path(root).exists()
            && UnixStream::connect(socket_path(root)).is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Ok(runtime) = read_runtime_info(root) {
        return Err(anyhow!(
            "packet28d did not become ready; runtime file exists for pid {} at {} (log: {})",
            runtime.pid,
            runtime.socket_path,
            runtime.log_path
        ));
    }
    Err(anyhow!("packet28d did not become ready"))
}

#[cfg(unix)]
fn packet28d_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_packet28d") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let candidate = current
        .parent()
        .ok_or_else(|| anyhow!("missing executable parent"))?
        .join("packet28d");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(anyhow!(
        "could not locate packet28d next to '{}'",
        current.display()
    ))
}
