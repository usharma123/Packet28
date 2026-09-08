use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::DoctorCheck;

pub(super) struct McpRoundTripChecks {
    pub(super) handshake: DoctorCheck,
    pub(super) reducer_round_trip: DoctorCheck,
    pub(super) push_notifications: DoctorCheck,
    pub(super) handoff_round_trip: DoctorCheck,
}

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<McpHarnessEvent>,
    stderr: Arc<Mutex<String>>,
    buffered: VecDeque<Value>,
}

enum McpHarnessEvent {
    Message(Value),
    StreamError(String),
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
            .stderr(Stdio::piped())
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
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture MCP stderr"))?;
        let responses = spawn_reader(stdout);
        let stderr = capture_stderr(stderr);
        Ok(Self {
            child,
            stdin,
            responses,
            stderr,
            buffered: VecDeque::new(),
        })
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_response(&mut self, expected_id: u64, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(index) = self
                .buffered
                .iter()
                .position(|value| value.get("id").and_then(Value::as_u64) == Some(expected_id))
            {
                if let Some(value) = self.buffered.remove(index) {
                    return Ok(value);
                }
                return Err(self.diagnostic_error(format!(
                    "MCP response buffer changed while reading id={expected_id}"
                )));
            }
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    self.diagnostic_error(format!(
                        "timed out waiting for MCP response id={expected_id}"
                    ))
                })?;
            let event = self.responses.recv_timeout(remaining).map_err(|_| {
                self.diagnostic_error(format!(
                    "timed out waiting for MCP response id={expected_id}"
                ))
            })?;
            match event {
                McpHarnessEvent::Message(value) => {
                    if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(value);
                    }
                    self.buffered.push_back(value);
                }
                McpHarnessEvent::StreamError(error) => {
                    return Err(self.diagnostic_error(format!(
                        "MCP stream closed before response id={expected_id}: {error}"
                    )));
                }
            }
        }
    }

    fn read_notification(&mut self, method: &str, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(index) = self
                .buffered
                .iter()
                .position(|value| value.get("method").and_then(Value::as_str) == Some(method))
            {
                if let Some(value) = self.buffered.remove(index) {
                    return Ok(value);
                }
                return Err(self.diagnostic_error(format!(
                    "MCP notification buffer changed while reading {method}"
                )));
            }
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    self.diagnostic_error(format!(
                        "timed out waiting for MCP notification {method}"
                    ))
                })?;
            let event = self.responses.recv_timeout(remaining).map_err(|_| {
                self.diagnostic_error(format!("timed out waiting for MCP notification {method}"))
            })?;
            match event {
                McpHarnessEvent::Message(value) => {
                    if value.get("method").and_then(Value::as_str) == Some(method) {
                        return Ok(value);
                    }
                    self.buffered.push_back(value);
                }
                McpHarnessEvent::StreamError(error) => {
                    return Err(self.diagnostic_error(format!(
                        "MCP stream closed before notification {method}: {error}"
                    )));
                }
            }
        }
    }

    fn diagnostic_error(&mut self, prefix: String) -> anyhow::Error {
        let child_state = match self.child.try_wait() {
            Ok(Some(status)) => format!(" child_exit={:?}", status.code()),
            Ok(None) => " child_running=true".to_string(),
            Err(error) => format!(" child_status_error={error}"),
        };
        let stderr = self
            .stderr
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let stderr = stderr.trim();
        if stderr.is_empty() {
            anyhow!("{prefix}.{child_state}")
        } else {
            anyhow!(
                "{prefix}.{child_state} stderr={}",
                compact_text(stderr, 400)
            )
        }
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns the hook runtime config path when durable hook ingest is disabled
/// (`hooks_enabled: false`).
///
/// In that state packet28d rejects every hook ingest with `accepted: false`,
/// so the reducer and handoff doctor probes cannot succeed. Surfacing the
/// config path lets the doctor report the real cause instead of an opaque
/// "reducer ingest missing" payload dump. A missing or unreadable config is
/// treated as "not disabled" so the normal smoke path still runs and reports
/// any other failure.
fn disabled_hook_runtime_config(root: &Path) -> Option<std::path::PathBuf> {
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(root);
    let raw = std::fs::read_to_string(&path).ok()?;
    let config =
        serde_json::from_str::<packet28_daemon_protocol::hooks::HookRuntimeConfig>(&raw).ok()?;
    (!config.hooks_enabled).then_some(path)
}

fn run_claude_hook_with_output(root: &Path, payload: &Value) -> Result<(i32, String)> {
    let exe = std::env::current_exe().context("failed to resolve current Packet28 binary")?;
    let mut child = Command::new(exe)
        .current_dir(root)
        .arg("hook")
        .arg("claude")
        .arg("--root")
        .arg(root.to_str().unwrap_or("."))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Packet28 Claude hook for doctor")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(serde_json::to_string(payload)?.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    let status = output.status;
    if !status.success() && status.code() != Some(2) {
        return Err(anyhow!(
            "claude hook exited with status {:?}",
            status.code()
        ));
    }
    Ok((
        status.code().unwrap_or_default(),
        String::from_utf8_lossy(&output.stdout).to_string(),
    ))
}

fn run_claude_hook(root: &Path, payload: &Value) -> Result<i32> {
    Ok(run_claude_hook_with_output(root, payload)?.0)
}

fn wait_for_handoff_ready(
    harness: &mut McpHarness,
    task_id: &str,
    timeout: Duration,
    request_id_base: u64,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    let mut request_id = request_id_base;
    loop {
        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":request_id,
            "method":"tools/call",
            "params":{
                "name":"packet28_task_status",
                "arguments":{"task_id":task_id}
            }
        }))?;
        let status = harness.read_response(request_id, timeout)?;
        let payload = &status["result"]["structuredContent"];
        let ready = payload["handoff_ready"] == json!(true);
        let has_artifact = payload["latest_handoff_artifact_id"]
            .as_str()
            .is_some_and(|value| !value.trim().is_empty());
        if ready && has_artifact {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(status);
        }
        request_id = request_id.saturating_add(1);
        thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn check_mcp_round_trip(root: &Path) -> McpRoundTripChecks {
    let timeout = Duration::from_secs(10);
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
        required: true,
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
            "packet28_write_intention",
            "packet28_prepare_handoff",
            "packet28_fetch_context",
        ] {
            if !tool_names.contains(&required_tool) {
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

        // A durably disabled hook runtime (`hooks_enabled: false`, typically a
        // stale kill switch from a prior `packet28 uninstall`) makes packet28d
        // reject every hook ingest with `accepted: false`. Without this early
        // check the reducer/handoff smoke would fail with an opaque
        // "reducer ingest missing" dump. Report the real cause explicitly and
        // skip the dependent hook probes.
        if let Some(config_path) = disabled_hook_runtime_config(root) {
            reducer_round_trip = DoctorCheck {
                name: "reducer_round_trip",
                ok: false,
                required: true,
                detail: format!(
                    "hook ingest is disabled: {} has hooks_enabled=false, so packet28d rejects every hook ingest. Re-run `packet28 setup` for your agent runtime to re-enable hook ingest.",
                    config_path.display()
                ),
            };
            push_notifications = DoctorCheck {
                name: "push_notifications",
                ok: false,
                required: true,
                detail: "skipped because hook ingest is disabled (hooks_enabled=false)".to_string(),
            };
            handoff_round_trip = DoctorCheck {
                name: "handoff_round_trip",
                ok: false,
                required: true,
                detail: "skipped because hook ingest is disabled (hooks_enabled=false)".to_string(),
            };
            return Ok(());
        }

        harness.send(&json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"packet28_write_intention",
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
                "name":"packet28_task_status",
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
                        required: true,
                        detail: format!("notification received for task_id={task_id}"),
                    }
                }
                Err(err) => DoctorCheck {
                    name: "push_notifications",
                    ok: false,
                    required: true,
                    detail: format!("notification probe failed: {err}"),
                },
            };
        if !push_notifications.ok {
            return Err(anyhow!(push_notifications.detail.clone()));
        }

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
        let handoff_status = wait_for_handoff_ready(&mut handoff_harness, &task_id, timeout, 6)?;
        let handoff_status_payload = &handoff_status["result"]["structuredContent"];
        if handoff_status_payload["handoff"]["handoff_id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .is_none()
        {
            return Err(anyhow!("handoff descriptor id was not recorded"));
        }
        let handoff_artifact_id = handoff_status_payload["latest_handoff_artifact_id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("handoff artifact id was not recorded"))?
            .to_string();

        handoff_harness.send(&json!({
            "jsonrpc":"2.0",
            "id":12,
            "method":"tools/call",
            "params":{
                "name":"packet28_prepare_handoff",
                "arguments":{
                    "task_id":task_id,
                    "response_mode":"slim"
                }
            }
        }))?;
        let handoff = handoff_harness.read_response(12, timeout)?;
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
        if handoff["result"]["structuredContent"]["latest_handoff_artifact_id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .is_none()
        {
            return Err(anyhow!(
                "prepare_handoff did not return a handoff artifact id"
            ));
        }
        if handoff["result"]["structuredContent"]["context"].is_null() {
            return Err(anyhow!(
                "prepare_handoff did not return a resumable context payload"
            ));
        }
        if handoff["result"]["structuredContent"]["handoff"]["handoff_id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .is_none()
        {
            return Err(anyhow!(
                "prepare_handoff did not return a handoff descriptor"
            ));
        }

        let resume_session_id = format!("{task_id}-resume");
        let (resume_status, resume_stdout) = run_claude_hook_with_output(
            root,
            &json!({
                "hook_event_name":"SessionStart",
                "task_id":task_id,
                "session_id":resume_session_id,
                "cwd": root.display().to_string()
            }),
        )?;
        if resume_status != 0 {
            return Err(anyhow!("resume SessionStart hook did not succeed"));
        }
        let resume_payload: Value = serde_json::from_str(&resume_stdout)
            .context("resume SessionStart hook did not emit valid JSON output")?;
        let additional_context = resume_payload["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("resume SessionStart hook did not return additionalContext"))?;
        if !additional_context.contains("Packet28 Context v") {
            return Err(anyhow!(
                "resume SessionStart hook returned unexpected additionalContext for handoff artifact {handoff_artifact_id}"
            ));
        }
        handoff_round_trip = DoctorCheck {
            name: "handoff_round_trip",
            ok: true,
            required: true,
            detail: format!(
                "task_id={task_id} checkpointed handoff ok artifact={handoff_artifact_id}"
            ),
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
        } else if !push_notifications.ok {
            push_notifications.detail = err.to_string();
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

fn spawn_reader(stdout: ChildStdout) -> Receiver<McpHarnessEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_mcp_message(&mut reader) {
                Ok(value) => {
                    if tx.send(McpHarnessEvent::Message(value)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(McpHarnessEvent::StreamError(error.to_string()));
                    break;
                }
            }
        }
    });
    rx
}

fn capture_stderr(stderr: ChildStderr) -> Arc<Mutex<String>> {
    let captured = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&captured);
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buffer = String::new();
        let _ = reader.read_to_string(&mut buffer);
        if let Ok(mut guard) = sink.lock() {
            *guard = buffer;
        }
    });
    captured
}

fn compact_text(value: &str, limit: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = compact.chars().count();
    if count <= limit {
        compact
    } else if limit <= 3 {
        "...".to_string()
    } else {
        let shortened = compact
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        format!("{shortened}...")
    }
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
