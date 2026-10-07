use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use packet28_daemon_protocol::index::{
    DaemonIndexState, DaemonIndexStatusRequest, DaemonIndexStatusResponse,
};
use packet28_daemon_protocol::message::{DaemonRequest, DaemonResponse};

use crate::cmd_setup_render::{format_setup_badge, SetupBadgeStyle};

#[derive(Debug)]
pub(crate) enum SetupIndexVerification {
    Ready(DaemonIndexStatusResponse),
    Building(DaemonIndexStatusResponse),
    Deferred {
        reason: String,
    },
    Failed {
        response: Option<DaemonIndexStatusResponse>,
        reason: String,
    },
}

const SETUP_INDEX_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const SETUP_INDEX_VERIFY_POLL: Duration = Duration::from_millis(100);
const REGEX_INDEX_DIR_NAME: &str = "regex-v1";
const REGEX_INDEX_MANIFEST_FILE: &str = "manifest.json";

pub(crate) fn verify_setup_index(root: &Path) -> Result<SetupIndexVerification> {
    let start = Instant::now();
    let mut rendered_progress = None::<String>;
    let first_response = fetch_index_status(root)?;
    match classify_setup_index_status(root, &first_response, false) {
        SetupIndexVerification::Ready(_)
        | SetupIndexVerification::Deferred { .. }
        | SetupIndexVerification::Failed { .. } => {
            return Ok(classify_setup_index_status(root, &first_response, false));
        }
        SetupIndexVerification::Building(_) => {
            render_setup_index_progress_line(&first_response, &mut rendered_progress)?;
        }
    }
    let mut last_response = first_response;
    loop {
        if start.elapsed() >= SETUP_INDEX_VERIFY_TIMEOUT {
            break;
        }
        std::thread::sleep(SETUP_INDEX_VERIFY_POLL);
        let response = fetch_index_status(root)?;
        match classify_setup_index_status(root, &response, false) {
            SetupIndexVerification::Ready(_)
            | SetupIndexVerification::Deferred { .. }
            | SetupIndexVerification::Failed { .. } => {
                finish_setup_index_progress_line(&mut rendered_progress)?;
                return Ok(classify_setup_index_status(root, &response, false));
            }
            SetupIndexVerification::Building(_) => {
                render_setup_index_progress_line(&response, &mut rendered_progress)?;
                last_response = response;
            }
        }
    }

    finish_setup_index_progress_line(&mut rendered_progress)?;
    Ok(classify_setup_index_status(root, &last_response, true))
}

fn render_setup_index_progress_line(
    response: &DaemonIndexStatusResponse,
    rendered_progress: &mut Option<String>,
) -> Result<()> {
    let line = format!(
        "    {} {}",
        format_setup_badge("indexing", SetupBadgeStyle::Warning),
        render_setup_index_progress(response)
    );
    if rendered_progress.as_deref() == Some(line.as_str()) {
        return Ok(());
    }

    let padding = rendered_progress
        .as_ref()
        .map(|previous| " ".repeat(previous.len().saturating_sub(line.len())))
        .unwrap_or_default();
    print!("\r{line}{padding}");
    io::stdout().flush()?;
    *rendered_progress = Some(line);
    Ok(())
}

fn finish_setup_index_progress_line(rendered_progress: &mut Option<String>) -> Result<()> {
    if rendered_progress.is_some() {
        println!();
        io::stdout().flush()?;
    }
    *rendered_progress = None;
    Ok(())
}

pub(crate) fn render_setup_index_progress(response: &DaemonIndexStatusResponse) -> String {
    format!(
        "repo {}  regex {}",
        render_index_stage(
            response.manifest.status.as_str(),
            response.manifest.indexed_files,
            response.manifest.total_files,
        ),
        render_index_stage(
            response
                .manifest
                .regex_status
                .as_deref()
                .unwrap_or("missing"),
            response.manifest.regex_indexed_files,
            response.manifest.regex_total_files,
        ),
    )
}

fn render_index_stage(status: &str, indexed_files: usize, total_files: usize) -> String {
    match status {
        "queued" => "queued".to_string(),
        "missing" => "missing".to_string(),
        "ready" => {
            if total_files > 0 {
                format!("ready {}", render_progress_bar(total_files, total_files))
            } else {
                "ready".to_string()
            }
        }
        other => {
            if total_files > 0 {
                format!(
                    "{other} {}",
                    render_progress_bar(indexed_files.min(total_files), total_files)
                )
            } else {
                other.to_string()
            }
        }
    }
}

fn render_progress_bar(indexed_files: usize, total_files: usize) -> String {
    if total_files == 0 {
        return "[----------------] --% (0/0)".to_string();
    }
    let width = 16usize;
    let completed = indexed_files.min(total_files);
    let filled = completed.saturating_mul(width) / total_files;
    let percent = completed.saturating_mul(100) / total_files;
    format!(
        "[{}{}] {:>3}% ({}/{})",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled)),
        percent,
        completed,
        total_files
    )
}

fn fetch_index_status(root: &Path) -> Result<DaemonIndexStatusResponse> {
    match crate::cmd_daemon::send_request(
        root,
        &DaemonRequest::DaemonIndexStatus {
            request: DaemonIndexStatusRequest {
                root: root.display().to_string(),
            },
        },
    )? {
        DaemonResponse::DaemonIndexStatus { response } => Ok(response),
        DaemonResponse::Error { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected index status response: {other:?}"),
    }
}

pub(crate) fn classify_setup_index_status(
    root: &Path,
    response: &DaemonIndexStatusResponse,
    timed_out: bool,
) -> SetupIndexVerification {
    if setup_index_ready(root, response) {
        return SetupIndexVerification::Ready(response.clone());
    }

    // Full builds attest setup's own working-tree changes. A workspace that
    // still cannot be attested (for example, it changed during the build or
    // exceeds the bounded attestation limits) defers indexing rather than
    // turning a successful installation into an error or claiming readiness.
    if response.manifest.status != DaemonIndexState::Corrupt
        && response.manifest.regex_status.as_deref() != Some("corrupt")
    {
        if let Some(reason) = response
            .manifest
            .last_error
            .as_deref()
            .and_then(deferred_workspace_reason)
        {
            return SetupIndexVerification::Deferred { reason };
        }
    }

    if let Some(reason) = setup_index_failure_reason(root, response, timed_out) {
        return SetupIndexVerification::Failed {
            response: Some(response.clone()),
            reason,
        };
    }

    SetupIndexVerification::Building(response.clone())
}

fn deferred_workspace_reason(error: &str) -> Option<String> {
    const NOT_READY: &str = "regex search index is not ready: ";
    // Daemons that predate dirty-workspace attestation report this reason.
    const LEGACY_DIRTY: &str = "full regex index rebuild requires a clean Git working tree";
    if let Some((_, reason)) = error.split_once(NOT_READY) {
        return Some(reason.to_string());
    }
    error
        .contains(LEGACY_DIRTY)
        .then(|| LEGACY_DIRTY.to_string())
}

fn setup_index_ready(root: &Path, response: &DaemonIndexStatusResponse) -> bool {
    response.ready
        && response.manifest.status == DaemonIndexState::Ready
        && response.manifest.regex_status.as_deref() == Some("ready")
        && response.manifest.regex_generation.is_some()
        && response
            .manifest
            .regex_weight_table_version
            .unwrap_or_default()
            > 0
        && regex_index_artifacts_present(root)
}

fn setup_index_failure_reason(
    root: &Path,
    response: &DaemonIndexStatusResponse,
    timed_out: bool,
) -> Option<String> {
    let manifest = &response.manifest;
    if manifest.status == DaemonIndexState::Corrupt {
        return Some(
            manifest
                .last_error
                .clone()
                .unwrap_or_else(|| "repo index manifest is corrupt".to_string()),
        );
    }
    if manifest.regex_status.as_deref() == Some("corrupt") {
        return Some(
            manifest
                .regex_stale_reason
                .clone()
                .or_else(|| manifest.last_error.clone())
                .unwrap_or_else(|| "regex trigram index is corrupt".to_string()),
        );
    }
    if manifest.status == DaemonIndexState::Ready
        && manifest.regex_status.as_deref() != Some("ready")
    {
        return Some(
            manifest
                .regex_stale_reason
                .clone()
                .or_else(|| manifest.last_error.clone())
                .unwrap_or_else(|| "regex trigram index is not ready".to_string()),
        );
    }
    if timed_out && !regex_index_artifacts_present(root) {
        return Some("regex trigram index artifacts are missing".to_string());
    }
    if timed_out && manifest.status == DaemonIndexState::Missing && manifest.regex_status.is_none()
    {
        return Some("setup did not create the repo or regex search index".to_string());
    }
    None
}

fn regex_index_artifacts_present(root: &Path) -> bool {
    let dir = root
        .join(".packet28")
        .join("index")
        .join(REGEX_INDEX_DIR_NAME);
    dir.join(REGEX_INDEX_MANIFEST_FILE).is_file()
}
