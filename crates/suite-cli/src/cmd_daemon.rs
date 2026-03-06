use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use packet28_daemon_core::{
    read_runtime_info, read_socket_message, resolve_workspace_root, socket_path,
    write_socket_message, ContextRecallRequest, ContextRecallResponse, ContextStoreGetRequest,
    ContextStoreGetResponse, ContextStoreListRequest, ContextStoreListResponse,
    ContextStorePruneDaemonRequest, ContextStorePruneResponse, ContextStoreStatsRequest,
    ContextStoreStatsResponse, CoverCheckRequest, CoverCheckResponse, DaemonRequest,
    DaemonResponse, PacketFetchRequest, PacketFetchResponse, TaskSubmitSpec, TestMapRequest,
    TestMapResponse, TestShardRequest, TestShardResponse,
};

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
pub struct WatchArgs {
    #[command(subcommand)]
    pub command: WatchCommands,
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

pub fn run(args: DaemonArgs) -> Result<i32> {
    match args.command {
        DaemonCommands::Start(args) => run_start(args),
        DaemonCommands::Stop(args) => run_stop(args),
        DaemonCommands::Status(args) => run_status(args),
        DaemonCommands::Task(args) => run_task(args),
        DaemonCommands::Watch(args) => run_watch(args),
    }
}

pub fn run_via_daemon(cli: crate::Cli, _raw_args: &[String]) -> Result<i32> {
    match cli.command {
        crate::Commands::Cover(cover) => match cover.command {
            crate::CoverCommands::Check(args) => crate::cmd_cover::run_remote(args, &cli.config),
        },
        crate::Commands::Diff(diff) => match diff.command {
            crate::DiffCommands::Analyze(args) => crate::cmd_diff::run_remote(args, &cli.config),
        },
        crate::Commands::Test(test) => match test.command {
            crate::TestCommands::Impact(args) => crate::cmd_impact::run_remote(args, &cli.config),
            crate::TestCommands::Shard(args) => crate::cmd_shard::run_remote(args, &cli.config),
            crate::TestCommands::Map(args) => crate::cmd_map::run_remote(args),
        },
        crate::Commands::Context(context) => match context.command {
            crate::ContextCommands::Assemble(args) => crate::cmd_context::run_assemble_remote(args),
            crate::ContextCommands::Correlate(args) => crate::cmd_context::run_correlate_remote(args),
            crate::ContextCommands::State(args) => crate::cmd_context::run_state_remote(args),
            crate::ContextCommands::Store(args) => crate::cmd_context::run_store_remote(args),
            crate::ContextCommands::Recall(args) => crate::cmd_context::run_recall_remote(args),
        },
        crate::Commands::Stack(stack) => match stack.command {
            crate::StackCommands::Slice(args) => crate::cmd_stack::run_remote(args),
        },
        crate::Commands::Build(build) => match build.command {
            crate::BuildCommands::Reduce(args) => crate::cmd_build::run_remote(args),
        },
        crate::Commands::Map(map) => match map.command {
            crate::MapCommands::Repo(args) => crate::cmd_map_repo::run_remote(args),
        },
        crate::Commands::Proxy(proxy) => match proxy.command {
            crate::cmd_proxy::ProxyCommands::Run(args) => crate::cmd_proxy::run_remote(args),
        },
        crate::Commands::Packet(packet) => match packet.command {
            crate::cmd_packet::PacketCommands::Fetch(args) => crate::cmd_packet::run_fetch_remote(args),
        },
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

pub fn via_daemon_env_enabled() -> bool {
    std::env::var("PACKET28_VIA_DAEMON")
        .ok()
        .map(|value| !matches!(value.trim(), "" | "0" | "false" | "False" | "FALSE"))
        .unwrap_or(false)
}

pub fn execute_kernel_request(root: &Path, request: context_kernel_core::KernelRequest) -> Result<context_kernel_core::KernelResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::Execute { request })? {
        DaemonResponse::Execute { response } => Ok(response),
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

pub fn send_kernel_request(root: &Path, request: context_kernel_core::KernelRequest) -> Result<context_kernel_core::KernelResponse> {
    execute_kernel_request(root, request)
}

pub fn execute_sequence(root: &Path, spec: TaskSubmitSpec) -> Result<packet28_daemon_core::SequenceSubmitResponse> {
    ensure_daemon(root)?;
    match send_request(root, &DaemonRequest::ExecuteSequence { spec })? {
        DaemonResponse::ExecuteSequence { response, task, watches } => {
            Ok(packet28_daemon_core::SequenceSubmitResponse {
                task_id: task.task_id,
                watch_ids: watches.iter().map(|watch| watch.watch_id.clone()).collect(),
                response,
            })
        }
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

pub fn execute_packet_fetch(root: &Path, request: PacketFetchRequest) -> Result<PacketFetchResponse> {
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

pub fn send_request(root: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    let socket = socket_path(root);
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to '{}'", socket.display()))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    write_socket_message(&mut writer, request)?;
    read_socket_message(&mut reader)
}

pub fn ensure_daemon(root: &Path) -> Result<()> {
    if socket_path(root).exists() && UnixStream::connect(socket_path(root)).is_ok() {
        return Ok(());
    }
    start_daemon(root)?;
    wait_for_daemon(root, Duration::from_secs(10))
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
    let response = send_request(&root, &DaemonRequest::Stop)?;
    match response {
        DaemonResponse::Ack { message } => {
            println!("{message}");
            Ok(0)
        }
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn run_status(args: JsonRootArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    ensure_daemon(&root)?;
    match send_request(&root, &DaemonRequest::Status)? {
        DaemonResponse::Status { status } => {
            if args.json {
                crate::cmd_common::emit_json(&serde_json::to_value(status)?, args.pretty)?;
            } else {
                println!("pid={}", status.pid);
                println!("root={}", status.workspace_root);
                println!("socket={}", status.socket_path);
                println!("tasks={}", status.tasks.len());
                println!("watches={}", status.watches.len());
            }
            Ok(0)
        }
        DaemonResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
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
                DaemonResponse::ExecuteSequence { response, task, watches } => {
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
            match send_request(&root, &DaemonRequest::TaskStatus { task_id: args.task_id })? {
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
            match send_request(&root, &DaemonRequest::TaskCancel { task_id: args.task_id })? {
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
    }
}

fn run_watch(args: WatchArgs) -> Result<i32> {
    match args.command {
        WatchCommands::List(args) => {
            let root = resolve_root_arg(&args.root);
            ensure_daemon(&root)?;
            match send_request(&root, &DaemonRequest::WatchList { task_id: args.task_id })? {
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
            match send_request(&root, &DaemonRequest::WatchRemove { watch_id: args.watch_id })? {
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

fn start_daemon(root: &Path) -> Result<()> {
    let binary = packet28d_binary()?;
    let root_arg = root.to_string_lossy().to_string();
    Command::new(binary)
        .arg("serve")
        .arg("--root")
        .arg(root_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn packet28d")?;
    Ok(())
}

fn wait_for_daemon(root: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if socket_path(root).exists() && UnixStream::connect(socket_path(root)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Ok(runtime) = read_runtime_info(root) {
        return Err(anyhow!(
            "packet28d did not become ready; runtime file exists for pid {} at {}",
            runtime.pid,
            runtime.socket_path
        ));
    }
    Err(anyhow!("packet28d did not become ready"))
}

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
