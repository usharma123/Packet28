use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use packet28_daemon_core::{DaemonIndexStatusRequest, DaemonRequest, DaemonResponse};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Args)]
pub struct DoctorArgs {
    #[arg(long, default_value = ".")]
    pub root: String,

    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    required: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct McpConfigCheck {
    path: String,
    exists: bool,
    packet28_configured: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    root: String,
    ok: bool,
    daemon: DoctorCheck,
    index: DoctorCheck,
    mcp_config: Vec<McpConfigCheck>,
    handshake: DoctorCheck,
    reducer_round_trip: DoctorCheck,
    push_notifications: DoctorCheck,
    handoff_round_trip: DoctorCheck,
    checks: Vec<DoctorCheck>,
}

struct McpRoundTripChecks {
    handshake: DoctorCheck,
    reducer_round_trip: DoctorCheck,
    push_notifications: DoctorCheck,
    handoff_round_trip: DoctorCheck,
}

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
}

impl McpHarness {
    fn start(root: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("failed to resolve current Packet28 binary")?;
        let mut child = Command::new(exe)
            .current_dir(root)
            .arg("mcp")
            .arg("serve")
            .arg("--root")
            .arg(root.to_str().unwrap_or("."))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start Packet28 MCP server for doctor")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture MCP stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture MCP stdout"))?;
        let responses = spawn_reader(stdout);
        Ok(Self {
            child,
            stdin,
            responses,
        })
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_response(&self, expected_id: u64, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| anyhow!("timed out waiting for MCP response id={expected_id}"))?;
            let value = self
                .responses
                .recv_timeout(remaining)
                .map_err(|_| anyhow!("timed out waiting for MCP response id={expected_id}"))?;
            if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
                return Ok(value);
            }
        }
    }

    fn read_notification(&self, method: &str, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| anyhow!("timed out waiting for MCP notification {method}"))?;
            let value = self
                .responses
                .recv_timeout(remaining)
                .map_err(|_| anyhow!("timed out waiting for MCP notification {method}"))?;
            if value.get("method").and_then(Value::as_str) == Some(method) {
                return Ok(value);
            }
        }
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_claude_hook(root: &Path, payload: &Value) -> Result<i32> {
    let exe = std::env::current_exe().context("failed to resolve current Packet28 binary")?;
    let mut child = Command::new(exe)
        .current_dir(root)
        .arg("hook")
        .arg("claude")
        .arg("--root")
        .arg(root.to_str().unwrap_or("."))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Packet28 Claude hook for doctor")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(serde_json::to_string(payload)?.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() && status.code() != Some(2) {
        return Err(anyhow!(
            "claude hook exited with status {:?}",
            status.code()
        ));
    }
    Ok(status.code().unwrap_or_default())
}

pub fn run(args: DoctorArgs) -> Result<i32> {
    let root = crate::cmd_daemon::resolve_root_arg(&args.root);
    let report = build_report(&root);
    if args.json {
        let text = if args.pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{text}");
    } else {
        print_human_report(&report);
    }
    Ok(if report.ok { 0 } else { 1 })
}

fn build_report(root: &Path) -> DoctorReport {
    let daemon = check_daemon(root);
    let index = check_index(root);
    let mcp_config = collect_mcp_config_checks(root);
    let mcp_config_summary = summarize_mcp_config(root, &mcp_config);
    let mcp_round_trip = check_mcp_round_trip(root);
    let checks = vec![
        daemon.clone(),
        index.clone(),
        mcp_config_summary,
        mcp_round_trip.handshake.clone(),
        mcp_round_trip.reducer_round_trip.clone(),
        mcp_round_trip.push_notifications.clone(),
        mcp_round_trip.handoff_round_trip.clone(),
    ];
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);
    DoctorReport {
        root: root.display().to_string(),
        ok,
        daemon,
        index,
        mcp_config,
        handshake: mcp_round_trip.handshake,
        reducer_round_trip: mcp_round_trip.reducer_round_trip,
        push_notifications: mcp_round_trip.push_notifications,
        handoff_round_trip: mcp_round_trip.handoff_round_trip,
        checks,
    }
}

fn check_daemon(root: &Path) -> DoctorCheck {
    match crate::cmd_daemon::ensure_daemon(root) {
        Ok(_) => match crate::cmd_daemon::send_request(root, &DaemonRequest::Status) {
            Ok(DaemonResponse::Status { status }) => DoctorCheck {
                name: "daemon",
                ok: true,
                required: true,
                detail: format!(
                    "daemon ready pid={} socket={}",
                    status.pid, status.socket_path
                ),
            },
            Ok(other) => DoctorCheck {
                name: "daemon",
                ok: false,
                required: true,
                detail: format!("unexpected daemon status response: {other:?}"),
            },
            Err(err) => DoctorCheck {
                name: "daemon",
                ok: false,
                required: true,
                detail: err.to_string(),
            },
        },
        Err(err) => DoctorCheck {
            name: "daemon",
            ok: false,
            required: true,
            detail: err.to_string(),
        },
    }
}

fn check_index(root: &Path) -> DoctorCheck {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match crate::cmd_daemon::send_request(
            root,
            &DaemonRequest::DaemonIndexStatus {
                request: DaemonIndexStatusRequest {
                    root: root.display().to_string(),
                },
            },
        ) {
            Ok(DaemonResponse::DaemonIndexStatus { response }) => {
                let ok = response.ready && response.manifest.status == "ready";
                if ok || std::time::Instant::now() >= deadline {
                    return DoctorCheck {
                        name: "index",
                        ok,
                        required: true,
                        detail: format!(
                            "ready={} status={} generation={}",
                            response.ready, response.manifest.status, response.manifest.generation
                        ),
                    };
                }
            }
            Ok(other) => {
                return DoctorCheck {
                    name: "index",
                    ok: false,
                    required: true,
                    detail: format!("unexpected index status response: {other:?}"),
                };
            }
            Err(err) => {
                return DoctorCheck {
                    name: "index",
                    ok: false,
                    required: true,
                    detail: err.to_string(),
                };
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn collect_mcp_config_checks(root: &Path) -> Vec<McpConfigCheck> {
    let config_paths = [
        root.join(".mcp.json"),
        root.join(".cursor").join("mcp.json"),
    ];
    config_paths
        .into_iter()
        .map(|path| inspect_mcp_config(&path))
        .collect()
}

fn inspect_mcp_config(path: &Path) -> McpConfigCheck {
    if !path.exists() {
        return McpConfigCheck {
            path: path.display().to_string(),
            exists: false,
            packet28_configured: false,
            detail: "file not found".to_string(),
        };
    }
    match mcp_config_has_packet28(path) {
        Ok(packet28_configured) => McpConfigCheck {
            path: path.display().to_string(),
            exists: true,
            packet28_configured,
            detail: if packet28_configured {
                "packet28 MCP entry present".to_string()
            } else {
                "packet28 MCP entry missing".to_string()
            },
        },
        Err(err) => McpConfigCheck {
            path: path.display().to_string(),
            exists: true,
            packet28_configured: false,
            detail: err.to_string(),
        },
    }
}

fn summarize_mcp_config(root: &Path, entries: &[McpConfigCheck]) -> DoctorCheck {
    let configured = entries
        .iter()
        .filter(|entry| entry.packet28_configured)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let fallback_paths = ["AGENTS.md", "CLAUDE.md", ".cursorrules"]
        .into_iter()
        .map(|relative| root.join(relative))
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let ok = !configured.is_empty();
    let detail = if ok {
        format!("configured MCP entries: {}", configured.join(", "))
    } else if !fallback_paths.is_empty() {
        format!(
            "no MCP config found; fallback guidance files present: {}",
            fallback_paths.join(", ")
        )
    } else {
        "no MCP config or fallback guidance files found".to_string()
    };
    DoctorCheck {
        name: "mcp_config",
        ok,
        required: false,
        detail,
    }
}

fn mcp_config_has_packet28(path: &Path) -> Result<bool> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("invalid MCP config '{}'", path.display()))?;
    Ok(value
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key("packet28")))
}

fn check_mcp_round_trip(root: &Path) -> McpRoundTripChecks {
    let timeout = Duration::from_secs(5);
    let task_id = format!(
        "doctor-smoke-task-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let session_id = format!("{task_id}-session");
    let mut handshake = DoctorCheck {
        name: "handshake",
        ok: false,
        required: true,
        detail: "not attempted".to_string(),
    };
    let mut reducer_round_trip = DoctorCheck {
        name: "reducer_round_trip",
        ok: false,
        required: true,
        detail: "skipped because handshake did not complete".to_string(),
    };
    let mut push_notifications = DoctorCheck {
        name: "push_notifications",
        ok: false,
        required: false,
        detail: "skipped because reducer round trip did not complete".to_string(),
    };
    let mut handoff_round_trip = DoctorCheck {
        name: "handoff_round_trip",
        ok: false,
        required: true,
        detail: "skipped because reducer round trip did not complete".to_string(),
    };

    let mut harness = match McpHarness::start(root) {
        Ok(harness) => harness,
        Err(err) => {
            handshake.detail = err.to_string();
            return McpRoundTripChecks {
                handshake,
                reducer_round_trip,
                push_notifications,
                handoff_round_trip,
            };
        }
    };

    let result = (|| -> Result<()> {
        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"packet28-doctor","version":"1"}}
        }))?;
        let initialize = harness.read_response(1, timeout)?;
        let server_name = initialize["result"]["serverInfo"]["name"]
            .as_str()
            .unwrap_or("unknown");

        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/list"
        }))?;
        let tools = harness.read_response(2, timeout)?;
        let tool_names = tools["result"]["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        for required_tool in [
            "packet28.write_intention",
            "packet28.prepare_handoff",
            "packet28.fetch_context",
        ] {
            if !tool_names.iter().any(|name| *name == required_tool) {
                return Err(anyhow!("{required_tool} missing from tools/list"));
            }
        }
        handshake = DoctorCheck {
            name: "handshake",
            ok: true,
            required: true,
            detail: format!(
                "server={server_name} tools=list ok tool_count={}",
                tool_names.len()
            ),
        };

        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28.write_intention",
                "arguments":{
                    "task_id":task_id,
                    "text": format!("Doctor handoff probe {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()),
                    "step_id":"hooks-first"
                }
            }
        }))?;
        let intention = harness.read_response(3, timeout)?;
        if intention["result"]["structuredContent"]["accepted"] != json!(true) {
            return Err(anyhow!("write_intention was not accepted"));
        }
        let hook_status = run_claude_hook(
            root,
            &json!({
                "hook_event_name":"PostToolUse",
                "task_id": task_id,
                "session_id": session_id,
                "cwd": root.display().to_string(),
                "tool_name":"Bash",
                "tool_input":{"command":"git status --short src/lib.rs"},
                "tool_response":{"stdout":" M src/lib.rs\n","stderr":"","is_error":false}
            }),
        )?;
        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{
                "name":"packet28.task_status",
                "arguments":{"task_id":task_id}
            }
        }))?;
        let task_status = harness.read_response(4, timeout)?;
        let task_status_payload = &task_status["result"]["structuredContent"];
        let task_record = &task_status_payload["task"];
        let hook_tokens = task_record["hook_window_est_tokens"].as_u64().unwrap_or(0);
        let hook_kind = task_record["latest_hook_command_kind"]
            .as_str()
            .unwrap_or_default();
        let reducer_ok = hook_status == 0 && hook_tokens > 0 && !hook_kind.is_empty();
        reducer_round_trip = DoctorCheck {
            name: "reducer_round_trip",
            ok: reducer_ok,
            required: true,
            detail: if reducer_ok {
                format!(
                    "task_id={task_id} hook reducer ingest ok ({hook_kind}, {hook_tokens} tokens)"
                )
            } else {
                format!(
                    "task_id={task_id} reducer ingest missing: {}",
                    serde_json::to_string(task_status_payload).unwrap_or_default()
                )
            },
        };

        push_notifications =
            match harness.read_notification("notifications/packet28.context_updated", timeout) {
                Ok(notification) => {
                    let notified_task_id = notification["params"]["task_id"]
                        .as_str()
                        .unwrap_or("unknown");
                    if notified_task_id != task_id {
                        return Err(anyhow!(
                        "notification task mismatch: expected {task_id}, got {notified_task_id}"
                    ));
                    }
                    DoctorCheck {
                        name: "push_notifications",
                        ok: true,
                        required: false,
                        detail: format!("notification received for task_id={task_id}"),
                    }
                }
                Err(err) => DoctorCheck {
                    name: "push_notifications",
                    ok: false,
                    required: false,
                    detail: format!("notification probe skipped: {err}"),
                },
            };

        drop(harness);
        let mut handoff_harness = McpHarness::start(root)?;
        handoff_harness.send(&json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"packet28-doctor-handoff","version":"1"}}
        }))?;
        let _ = handoff_harness.read_response(1, timeout)?;

        run_claude_hook(
            root,
            &json!({
                "hook_event_name":"Stop",
                "task_id":task_id,
                "session_id":session_id,
                "cwd": root.display().to_string()
            }),
        )?;
        handoff_harness.send(&json!({
            "jsonrpc":"2.0",
            "id":6,
            "method":"tools/call",
            "params":{
                "name":"packet28.prepare_handoff",
                "arguments":{
                    "task_id":task_id,
                    "response_mode":"slim"
                }
            }
        }))?;
        let handoff = handoff_harness.read_response(6, timeout)?;
        let handoff_task_id = handoff["result"]["structuredContent"]["task_id"]
            .as_str()
            .unwrap_or("unknown");
        if handoff_task_id != task_id {
            return Err(anyhow!(
                "prepare_handoff resolved unexpected task_id '{handoff_task_id}'"
            ));
        }
        if handoff["result"]["structuredContent"]["handoff_ready"] != json!(true) {
            return Err(anyhow!("prepare_handoff did not return a ready handoff"));
        }
        handoff_round_trip = DoctorCheck {
            name: "handoff_round_trip",
            ok: true,
            required: true,
            detail: format!("task_id={task_id} checkpointed handoff ok"),
        };

        Ok(())
    })();

    if let Err(err) = result {
        if !handshake.ok {
            handshake.detail = err.to_string();
        } else if !reducer_round_trip.ok {
            reducer_round_trip.detail = err.to_string();
            push_notifications.detail = "skipped because reducer round trip failed".to_string();
            handoff_round_trip.detail = "skipped because reducer round trip failed".to_string();
        } else if !handoff_round_trip.ok {
            handoff_round_trip.detail = err.to_string();
        }
    }

    McpRoundTripChecks {
        handshake,
        reducer_round_trip,
        push_notifications,
        handoff_round_trip,
    }
}

fn spawn_reader(stdout: ChildStdout) -> Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Ok(value) = read_mcp_message(&mut reader) {
            if tx.send(value).is_err() {
                break;
            }
        }
    });
    rx
}

fn read_mcp_message(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length = None::<usize>;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(anyhow!("MCP stream closed"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
    }
    let length = content_length.ok_or_else(|| anyhow!("missing MCP content-length header"))?;
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn print_human_report(report: &DoctorReport) {
    println!("Packet28 doctor");
    println!("root: {}", report.root);
    for check in &report.checks {
        let status = if check.ok { "ok" } else { "fail" };
        let required = if check.required {
            "required"
        } else {
            "advisory"
        };
        println!("- {} [{}]: {}", check.name, required, status);
        println!("  {}", check.detail);
    }
    println!("overall: {}", if report.ok { "ok" } else { "fail" });
}
