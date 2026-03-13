use super::*;
use fs2::FileExt;

pub fn ensure_daemon_dir(root: &Path) -> Result<PathBuf> {
    let dir = daemon_dir(root);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon directory '{}'", dir.display()))?;
    let socket_dir = socket_path(root)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&socket_dir).with_context(|| {
        format!(
            "failed to create daemon socket directory '{}'",
            socket_dir.display()
        )
    })?;
    Ok(dir)
}

pub fn write_runtime_info(root: &Path, info: &DaemonRuntimeInfo) -> Result<()> {
    ensure_daemon_dir(root)?;
    fs::write(pid_path(root), format!("{}\n", info.pid))
        .with_context(|| format!("failed to write pid file for '{}'", root.display()))?;
    fs::write(runtime_path(root), serde_json::to_vec_pretty(info)?)
        .with_context(|| format!("failed to write runtime file for '{}'", root.display()))?;
    Ok(())
}

pub fn read_runtime_info(root: &Path) -> Result<DaemonRuntimeInfo> {
    let raw = fs::read(runtime_path(root))
        .with_context(|| format!("failed to read runtime file for '{}'", root.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn remove_runtime_files(root: &Path) -> Result<()> {
    for path in [
        socket_path(root),
        pid_path(root),
        runtime_path(root),
        ready_path(root),
    ] {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove '{}'", path.display()))?;
        }
    }
    Ok(())
}

pub fn load_watch_registry(root: &Path) -> Result<WatchRegistry> {
    let path = watch_registry_path(root);
    if !path.exists() {
        return Ok(WatchRegistry::default());
    }
    let raw = fs::read(&path)
        .with_context(|| format!("failed to read watch registry '{}'", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn save_watch_registry(root: &Path, registry: &WatchRegistry) -> Result<()> {
    ensure_daemon_dir(root)?;
    let path = watch_registry_path(root);
    let bytes = serde_json::to_vec_pretty(registry)?;
    write_atomically(&path, &bytes)
        .with_context(|| format!("failed to write watch registry '{}'", path.display()))?;
    Ok(())
}

pub fn load_task_registry(root: &Path) -> Result<TaskRegistry> {
    let path = task_registry_path(root);
    if !path.exists() {
        return Ok(TaskRegistry::default());
    }
    let raw = fs::read(&path)
        .with_context(|| format!("failed to read task registry '{}'", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn save_task_registry(root: &Path, registry: &TaskRegistry) -> Result<()> {
    ensure_daemon_dir(root)?;
    let path = task_registry_path(root);
    let bytes = serde_json::to_vec_pretty(registry)?;
    write_atomically(&path, &bytes)
        .with_context(|| format!("failed to write task registry '{}'", path.display()))?;
    Ok(())
}

pub fn append_task_event(root: &Path, frame: &DaemonEventFrame) -> Result<()> {
    let dir = task_events_dir(root);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create task events dir '{}'", dir.display()))?;
    let path = task_event_log_path(root, &frame.task_id);
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open task event log '{}'", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("failed to lock task event log '{}'", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to append task event log '{}'", path.display()))?;
    file.unlock()
        .with_context(|| format!("failed to unlock task event log '{}'", path.display()))?;
    Ok(())
}

pub fn load_task_events(root: &Path, task_id: &str) -> Result<Vec<DaemonEventFrame>> {
    let path = task_event_log_path(root, task_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read task event log '{}'", path.display()))?;
    let mut events = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        events.push(serde_json::from_str(line)?);
    }
    Ok(events)
}

pub fn resolve_workspace_root(start: &Path) -> PathBuf {
    let mut dir = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return start.to_path_buf();
        }
    }
}

pub fn write_socket_message<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let len = bytes.len() as u64;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_socket_message<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T> {
    let mut len_bytes = [0_u8; 8];
    reader.read_exact(&mut len_bytes)?;
    let len = usize::try_from(u64::from_be_bytes(len_bytes))
        .context("socket frame length does not fit in usize")?;
    if len == 0 {
        anyhow::bail!("socket frame length must be greater than zero");
    }
    if len > MAX_SOCKET_MESSAGE_BYTES {
        anyhow::bail!(
            "socket frame too large: {len} bytes exceeds limit of {MAX_SOCKET_MESSAGE_BYTES}"
        );
    }
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&temp_path)
        .with_context(|| format!("failed to create temp file '{}'", temp_path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write temp file '{}'", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync temp file '{}'", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to atomically replace '{}' with '{}'",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn appends_and_loads_task_events() {
        let dir = tempdir().unwrap();
        let frame = DaemonEventFrame {
            seq: 1,
            task_id: "task/demo".to_string(),
            event: DaemonEvent {
                kind: "task_started".to_string(),
                occurred_at_unix: 1,
                data: serde_json::json!({"task_id":"task/demo"}),
            },
        };
        append_task_event(dir.path(), &frame).unwrap();
        append_task_event(
            dir.path(),
            &DaemonEventFrame {
                seq: 2,
                task_id: "task/demo".to_string(),
                event: DaemonEvent {
                    kind: "task_completed".to_string(),
                    occurred_at_unix: 2,
                    data: serde_json::json!({"task_id":"task/demo"}),
                },
            },
        )
        .unwrap();

        let loaded = load_task_events(dir.path(), "task/demo").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].seq, 1);
        assert_eq!(loaded[1].event.kind, "task_completed");
    }
}
