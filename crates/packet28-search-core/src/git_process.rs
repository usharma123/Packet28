//! Time- and output-bounded Git subprocess execution.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_GIT_METADATA_BYTES: usize = 32 * 1024 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[cfg(test)]
thread_local! {
    // Per-thread so concurrently running Git-backed tests cannot skew counts.
    static GIT_COMMAND_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn run_git(root: &Path, args: &[&str]) -> std::result::Result<Output, String> {
    #[cfg(test)]
    GIT_COMMAND_COUNT.set(GIT_COMMAND_COUNT.get() + 1);
    let operation = args.first().copied().unwrap_or("command");
    let label = format!("git {operation}");
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args);
    let output = run_command_bounded(
        &mut command,
        &label,
        GIT_COMMAND_TIMEOUT,
        MAX_GIT_METADATA_BYTES,
    )?;
    if !output.status.success() {
        return Err(format!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn run_command_bounded(
    command: &mut Command,
    label: &str,
    timeout: Duration,
    max_output_bytes: usize,
) -> std::result::Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run {label}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("failed to capture {label} stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("failed to capture {label} stderr"))?;
    let stream_limit = max_output_bytes.saturating_add(1);
    let stdout_reader = thread::spawn(move || read_bounded(stdout, stream_limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, stream_limit));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{label} timed out after {} milliseconds",
                    timeout.as_millis()
                ));
            }
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting for {label}: {error}"));
            }
        }
    };
    let stdout = join_reader(stdout_reader, label, "stdout")?;
    let stderr = join_reader(stderr_reader, label, "stderr")?;
    if stdout.len().saturating_add(stderr.len()) > max_output_bytes {
        return Err(format!("{label} output exceeds the bounded metadata limit"));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded(stream: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    label: &str,
    stream: &str,
) -> std::result::Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| format!("{label} {stream} reader panicked"))?
        .map_err(|error| format!("failed to read {label} {stream}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    #[cfg(unix)]
    fn fixture_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_runtime_is_bounded() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do :; done"]);
        let started = Instant::now();

        let error = run_command_bounded(
            &mut command,
            "timeout fixture",
            Duration::from_millis(25),
            1_024,
        )
        .expect_err("non-terminating subprocess unexpectedly completed");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn command_output_is_bounded() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "i=0; while [ \"$i\" -lt 1000 ]; do printf 0123456789; i=$((i + 1)); done",
        ]);

        let error = run_command_bounded(
            &mut command,
            "output fixture",
            Duration::from_secs(1),
            1_024,
        )
        .expect_err("oversized subprocess output unexpectedly succeeded");

        assert!(error.contains("output exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn guarded_query_runs_one_workspace_attestation() {
        use packet28_reducer_core::SearchRequest;

        use crate::broker_internal_guarded_indexed_search_batch as run_batch;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn unique_attestation_needle() {}\npub fn second_attestation_needle() {}\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".packet28/\n").unwrap();
        fixture_git(root, &["init", "--quiet"]);
        fixture_git(root, &["config", "user.name", "Packet28 Test"]);
        fixture_git(root, &["config", "user.email", "packet28@example.invalid"]);
        fixture_git(root, &["add", "."]);
        fixture_git(
            root,
            &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
        );
        crate::generation::rebuild_full_index(root, true).unwrap();
        let request = |query: &str| SearchRequest {
            query: query.to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        };
        GIT_COMMAND_COUNT.set(0);

        crate::query::load_and_guarded_indexed_search(root, &request("unique_attestation_needle"))
            .unwrap();
        assert_eq!(GIT_COMMAND_COUNT.get(), 2);
        GIT_COMMAND_COUNT.set(0);
        let error = crate::query::load_and_guarded_indexed_search(
            root,
            &SearchRequest {
                query: ".+".to_string(),
                ..SearchRequest::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, crate::SearchError::IndexNotReady { .. }));
        assert_eq!(GIT_COMMAND_COUNT.get(), 2);

        let runtime = crate::generation::load_runtime(root).unwrap();
        GIT_COMMAND_COUNT.set(0);
        let primary = [request("unique_attestation_needle")];
        let deferred = [request("second_attestation_needle"), request("x")];
        let mut session = crate::BrokerInternalGuardedIndexedSearchSession::new();
        let primary_results = run_batch(root, &runtime, &primary, &mut session).unwrap();
        let deferred_results = run_batch(root, &runtime, &deferred, &mut session).unwrap();
        assert_eq!(
            primary_results
                .iter()
                .chain(&deferred_results)
                .map(Option::is_some)
                .collect::<Vec<_>>(),
            [true, true, false],
            "one-byte query must not verify every indexed file"
        );
        assert_eq!(
            GIT_COMMAND_COUNT.get(),
            4,
            "each exposed result batch must perform one two-command attestation"
        );
    }
}
