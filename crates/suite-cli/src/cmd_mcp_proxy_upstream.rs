use super::*;

use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

pub(crate) struct UpstreamClient {
    pub(crate) name: String,
    pub(crate) _child: Child,
    pub(crate) stdin: ChildStdin,
    pub(crate) responses: Receiver<Value>,
    pub(crate) request_timeout: Duration,
    pub(crate) command_preview: String,
}

const DEFAULT_UPSTREAM_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn spawn_upstream_clients(
    root: &Path,
    config: &McpProxyConfig,
    writer: Arc<Mutex<io::Stdout>>,
    session: Arc<Mutex<McpSessionState>>,
) -> Result<BTreeMap<String, UpstreamClient>> {
    let mut upstreams = BTreeMap::new();
    for (name, server) in &config.mcp_servers {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        } else {
            command.current_dir(root);
        }
        command.envs(server.env.clone());
        let command_preview = render_command_preview(&server.command, &server.args);
        let timeout = Duration::from_millis(
            server
                .timeout_ms
                .unwrap_or(DEFAULT_UPSTREAM_TIMEOUT_MS)
                .max(1),
        );
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start upstream MCP server '{name}'"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("upstream MCP server '{name}' has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("upstream MCP server '{name}' has no stdout"))?;
        let (tx, rx) = mpsc::channel();
        start_upstream_reader_thread(name.clone(), stdout, tx, writer.clone(), session.clone());
        upstreams.insert(
            name.clone(),
            UpstreamClient {
                name: name.clone(),
                _child: child,
                stdin,
                responses: rx,
                request_timeout: timeout,
                command_preview,
            },
        );
    }
    Ok(upstreams)
}

fn start_upstream_reader_thread(
    upstream_name: String,
    stdout: std::process::ChildStdout,
    responses: Sender<Value>,
    writer: Arc<Mutex<io::Stdout>>,
    session: Arc<Mutex<McpSessionState>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let Ok(Some((message, _framing))) = read_message(&mut reader) else {
                break;
            };
            if message.get("id").is_some() {
                if responses.send(message).is_err() {
                    break;
                }
                continue;
            }
            let framing = session
                .lock()
                .ok()
                .and_then(|guard| guard.framing)
                .unwrap_or(McpMessageFraming::ContentLength);
            let mut notification = message;
            if let Some(params) = notification
                .get_mut("params")
                .and_then(Value::as_object_mut)
            {
                params
                    .entry("upstream".to_string())
                    .or_insert_with(|| Value::String(upstream_name.clone()));
            }
            if let Ok(mut guard) = writer.lock() {
                let _ = write_message(&mut *guard, &notification, framing);
            }
        }
    });
}

pub(crate) fn send_message_to_upstream(upstream: &mut UpstreamClient, request: &Value) -> Result<()> {
    let body = serde_json::to_vec(request)?;
    write!(upstream.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    upstream.stdin.write_all(&body)?;
    upstream.stdin.flush()?;
    Ok(())
}

pub(crate) fn send_request_to_upstream(upstream: &mut UpstreamClient, request: &Value) -> Result<Value> {
    send_message_to_upstream(upstream, request)?;
    upstream
        .responses
        .recv_timeout(upstream.request_timeout)
        .map_err(|_| {
            anyhow!(
                "timed out waiting for upstream '{}' after {}ms while running `{}`",
                upstream.name,
                upstream.request_timeout.as_millis(),
                upstream.command_preview
            )
        })
}
