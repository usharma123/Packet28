use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, Result};
use packet28_daemon_core::retention::{
    inspect_task_store, retain_task_store, RetentionMode, RetentionOptions, RetentionOutcome,
    RetentionReason, TaskStoreMetrics, TaskStoreReport,
};
use packet28_daemon_core::storage::{
    inspect_offline_corrupt_task_event_logs, inspect_task_watch_registry_checkpoint_repair,
    now_unix, repair_corrupt_task_event_logs, repair_task_watch_registry_checkpoint,
    QuarantinedCorruptTaskEventLog, RegistryCheckpointRepairReport, RegistryCheckpointRepairStatus,
    RegistryRepairFileState,
};
use serde_json::{json, Value};

use crate::cmd_daemon::{
    resolve_root_arg, StorageArgs, StorageCleanupArgs, StorageCommands, StorageInspectArgs,
    StorageRepairArgs,
};

/// Executes the selected daemon storage operation.
///
/// # Returns
///
/// The operation's process exit status, or an error if execution fails.
///
/// # Examples
///
/// ```text
/// Packet28 daemon storage inspect --root /path/to/workspace
/// Packet28 daemon storage repair --root /path/to/workspace
/// ```
pub(crate) fn run_storage(args: StorageArgs) -> Result<i32> {
    match args.command {
        StorageCommands::Inspect(args) => run_inspect(args),
        StorageCommands::Cleanup(args) => run_cleanup(args),
        StorageCommands::Repair(args) => run_repair(args),
    }
}

/// Inspects or repairs canonical task/watch registry images and corrupt task
/// event logs for the selected workspace.
///
/// Registry images are classified first because event-log inspection loads the
/// registry. A canonical image that no exact checkpoint authority can restore
/// is reported, left untouched, and makes the command exit `1`; event logs are
/// then not inspected. Every mode requires nonblocking exclusive task-store
/// access and a daemon-instance gate. A running or starting daemon, or a
/// writer, makes the command fail busy.
///
/// # Examples
///
/// ```text
/// packet28 daemon storage repair --root /path/to/workspace
/// packet28 daemon storage repair --root /path/to/workspace --apply
/// ```
///
/// # Errors
///
/// Returns an error if the daemon is running or if inspection, repair, or report emission fails.
fn run_repair(args: StorageRepairArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    let registry = if args.apply {
        repair_task_watch_registry_checkpoint(&root)?
    } else {
        inspect_task_watch_registry_checkpoint_repair(&root)?
    };
    let records = if registry.startup_registry_ready() {
        Some(if args.apply {
            repair_corrupt_task_event_logs(&root)?
        } else {
            inspect_offline_corrupt_task_event_logs(&root)?
        })
    } else {
        None
    };
    emit_repair(
        &root,
        &registry,
        records.as_deref(),
        args.apply,
        args.json,
        args.pretty,
    )?;
    Ok(i32::from(registry.is_unrecoverable()))
}

/// Emits registry and event-log repair results in JSON or human-readable form.
///
/// # Arguments
///
/// * `root` - The workspace root included in the report.
/// * `registry` - Canonical registry classification or repair result.
/// * `records` - Corrupt task event log records, or `None` when the registry
///   must be repaired before event logs can be inspected.
/// * `applied` - Whether the records were quarantined rather than inspected.
/// * `json_output` - Whether to emit JSON output.
/// * `pretty` - Whether to format JSON output with indentation.
fn emit_repair(
    root: &Path,
    registry: &RegistryCheckpointRepairReport,
    records: Option<&[QuarantinedCorruptTaskEventLog]>,
    applied: bool,
    json_output: bool,
    pretty: bool,
) -> Result<()> {
    if json_output {
        let items: Option<Vec<Value>> = records.map(|records| {
            records
                .iter()
                .map(|record| {
                    json!({
                        "task_id": record.task_id,
                        "successor_task_id": if record.successor_task_id.is_empty() { None } else { Some(record.successor_task_id.as_str()) },
                        "event_log_path": record.event_log_path.display().to_string(),
                        "quarantined_path": record
                            .quarantined_path
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        "reason": record.reason,
                    })
                })
                .collect()
        });
        let report = json!({
            "workspace_root": root.display().to_string(),
            "applied": applied,
            "registry_checkpoint": registry,
            "corrupt_task_event_logs": records.map(<[_]>::len),
            "records": items,
        });
        let rendered = if pretty {
            serde_json::to_string_pretty(&report)?
        } else {
            serde_json::to_string(&report)?
        };
        println!("{rendered}");
        return Ok(());
    }

    print!("{}", render_registry_repair(registry));
    let Some(records) = records else {
        println!(
            "task event logs were not inspected: the task/watch registry must be repaired first"
        );
        return Ok(());
    };
    if records.is_empty() {
        println!("no corrupt task event logs found");
        return Ok(());
    }
    let verb = if applied {
        "quarantined"
    } else {
        "found (dry run)"
    };
    println!("{verb} {} corrupt task event log(s):", records.len());
    for record in records {
        println!("  task_id={} reason={}", record.task_id, record.reason);
        println!("    event_log={}", record.event_log_path.display());
        if !record.successor_task_id.is_empty() {
            println!("    continue_as={}", record.successor_task_id);
        }
        if let Some(quarantined) = &record.quarantined_path {
            println!("    moved_to={}", quarantined.display());
        }
    }
    if !applied {
        println!("re-run with --apply to quarantine them (the daemon must be stopped)");
    }
    Ok(())
}

fn render_registry_repair(report: &RegistryCheckpointRepairReport) -> String {
    let mut output = String::new();
    let headline = match report.status {
        RegistryCheckpointRepairStatus::Clean => return output,
        RegistryCheckpointRepairStatus::Repairable => {
            "task/watch registry: exact checkpoint restore available (dry run)"
        }
        RegistryCheckpointRepairStatus::Repaired => {
            "task/watch registry: restored exact checkpoint images"
        }
        RegistryCheckpointRepairStatus::InterruptedRepair => {
            "task/watch registry: an interrupted repair is pending (dry run)"
        }
        RegistryCheckpointRepairStatus::Resumed => {
            "task/watch registry: completed an interrupted repair"
        }
        RegistryCheckpointRepairStatus::Unrecoverable => {
            "task/watch registry: NO SAFE RECOVERY; nothing was changed"
        }
    };
    let _ = writeln!(output, "{headline}");
    if let Some(error) = &report.startup_error {
        let _ = writeln!(output, "  startup_error={error}");
    }
    if let Some(authority) = report.authority {
        let _ = writeln!(
            output,
            "  authority={} generation={} applied_delta_revision={}",
            serde_json::to_value(authority)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            optional_u64(report.generation),
            optional_u64(report.applied_delta_revision)
        );
    }
    for file in &report.files {
        let state = match file.state {
            RegistryRepairFileState::Matches => "matches",
            RegistryRepairFileState::Restore => "would_restore",
            RegistryRepairFileState::Restored => "restored",
            RegistryRepairFileState::Unrecoverable => "unrecoverable",
        };
        let _ = writeln!(
            output,
            "  file={} state={state} path={}",
            file.file,
            file.path.display()
        );
        if file.replaced_bytes_were_reencoding == Some(false) {
            let _ = writeln!(
                output,
                "    note=replaced bytes differ in content from the committed image; they are \
                 archived, not adopted"
            );
        }
        if let Some(detail) = &file.detail {
            let _ = writeln!(output, "    detail={detail}");
        }
    }
    if let Some(wal) = &report.wal {
        let _ = writeln!(
            output,
            "  wal_present={} wal_checkpoint_revision={} wal_replayed_revision={} \
             wal_torn_suffix_bytes={}",
            wal.present, wal.checkpoint_revision, wal.replayed_revision, wal.torn_suffix_bytes
        );
    }
    if let Some(archive) = &report.archive_path {
        let _ = writeln!(output, "  originals_preserved_in={}", archive.display());
    }
    if let Some(reason) = &report.reason {
        let _ = writeln!(output, "  reason={reason}");
    }
    match report.status {
        RegistryCheckpointRepairStatus::Repairable
        | RegistryCheckpointRepairStatus::InterruptedRepair => {
            let _ = writeln!(
                output,
                "re-run with --apply to restore it (the daemon must be stopped)"
            );
        }
        RegistryCheckpointRepairStatus::Unrecoverable => {
            let _ = writeln!(
                output,
                "the daemon will not start until these bytes are restored from a backup; the \
                 current files were left in place"
            );
        }
        RegistryCheckpointRepairStatus::Clean
        | RegistryCheckpointRepairStatus::Repaired
        | RegistryCheckpointRepairStatus::Resumed => {}
    }
    output
}

/// Inspects the task store at the selected workspace root and emits its report.
///
/// # Returns
///
/// Returns `0` after the report is emitted.
///
/// # Examples
///
/// ```text
/// Packet28 daemon storage inspect --root /path/to/workspace --json
/// ```
fn run_inspect(args: StorageInspectArgs) -> Result<i32> {
    let root = resolve_root_arg(&args.root);
    let report = inspect_task_store(&root, now_unix())?;
    emit_report(&report, args.json, args.pretty)?;
    Ok(0)
}

fn run_cleanup(args: StorageCleanupArgs) -> Result<i32> {
    if args.max_age_seconds.is_none() && args.max_bytes.is_none() {
        bail!("cleanup requires --max-age-seconds, --max-bytes, or both");
    }

    let root = resolve_root_arg(&args.root);
    let mut options = RetentionOptions::dry_run(args.max_age_seconds, args.max_bytes);
    if args.apply {
        options = options.apply();
    }
    let report = retain_task_store(&root, now_unix(), options)?;
    emit_report(&report, args.json, args.pretty)?;
    Ok(cleanup_exit_code(&report))
}

fn cleanup_exit_code(report: &TaskStoreReport) -> i32 {
    let retention = &report.retention;
    i32::from(
        retention.failed_tasks > 0
            || retention.recovery_conflicted_groups > 0
            || !retention.final_rescan_reliable
            || !retention.action_byte_accounting_reliable,
    )
}

fn emit_report(report: &TaskStoreReport, json: bool, pretty: bool) -> Result<()> {
    if json {
        println!("{}", render_json(report, pretty)?);
    } else {
        print!("{}", render_text(report)?);
    }
    Ok(())
}

fn render_json(report: &TaskStoreReport, pretty: bool) -> Result<String> {
    if pretty {
        Ok(serde_json::to_string_pretty(report)?)
    } else {
        Ok(serde_json::to_string(report)?)
    }
}

fn render_text(report: &TaskStoreReport) -> Result<String> {
    let mut output = String::new();
    let _ = writeln!(output, "schema_version={}", report.schema_version);
    let _ = writeln!(output, "observed_at_unix={}", report.observed_at_unix);
    let _ = writeln!(output, "workspace_root={}", report.workspace_root);
    let _ = writeln!(output, "state_root={}", report.state_root);
    let _ = writeln!(output, "mode={}", mode_name(report.mode));
    write_metrics(&mut output, "before", &report.metrics_before);
    write_metrics(&mut output, "after", &report.metrics_after);
    let retention = &report.retention;
    let _ = writeln!(
        output,
        "retention_max_age_seconds={}",
        optional_u64(retention.max_age_seconds)
    );
    let _ = writeln!(
        output,
        "retention_max_bytes={}",
        optional_u64(retention.max_bytes)
    );
    let _ = writeln!(
        output,
        "retention_protected_tasks={}",
        retention.protected_tasks
    );
    let _ = writeln!(
        output,
        "retention_protected_logical_bytes={}",
        retention.protected_logical_bytes
    );
    let _ = writeln!(
        output,
        "retention_planned_tasks={}",
        retention.planned_tasks
    );
    let _ = writeln!(
        output,
        "retention_planned_logical_bytes={}",
        retention.planned_logical_bytes
    );
    let _ = writeln!(
        output,
        "retention_removed_tasks={}",
        retention.removed_tasks
    );
    let _ = writeln!(
        output,
        "retention_removed_logical_bytes={}",
        retention.removed_logical_bytes
    );
    let _ = writeln!(
        output,
        "retention_skipped_tasks={}",
        retention.skipped_tasks
    );
    let _ = writeln!(output, "retention_failed_tasks={}", retention.failed_tasks);
    let _ = writeln!(
        output,
        "retention_failed_logical_bytes={}",
        retention.failed_logical_bytes
    );
    let _ = writeln!(
        output,
        "retention_recovered_precommit_groups={}",
        retention.recovered_precommit_groups
    );
    let _ = writeln!(
        output,
        "retention_recovered_committed_groups={}",
        retention.recovered_committed_groups
    );
    let _ = writeln!(
        output,
        "retention_recovery_conflicted_groups={}",
        retention.recovery_conflicted_groups
    );
    let _ = writeln!(
        output,
        "retention_final_rescan_reliable={}",
        retention.final_rescan_reliable
    );
    let _ = writeln!(
        output,
        "retention_action_byte_accounting_reliable={}",
        retention.action_byte_accounting_reliable
    );
    let _ = writeln!(
        output,
        "retention_remaining_managed_logical_bytes={}",
        retention.remaining_managed_logical_bytes
    );
    let _ = writeln!(
        output,
        "retention_remaining_over_limit_bytes={}",
        retention.remaining_over_limit_bytes
    );
    let _ = writeln!(output, "actions={}", report.actions.len());
    for action in &report.actions {
        let task_ids = action
            .task_ids
            .iter()
            .map(|value| quoted(value))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let reasons = action
            .reasons
            .iter()
            .map(|reason| reason_name(*reason))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            output,
            "action storage_key={} task_ids=[{}] logical_bytes={} removed_logical_bytes={} remaining_logical_bytes={} byte_accounting_reliable={} latest_timestamp_unix={} reasons=[{}] outcome={}",
            quoted(&action.storage_key)?,
            task_ids,
            action.logical_bytes,
            action.removed_logical_bytes,
            action.remaining_logical_bytes,
            action.byte_accounting_reliable,
            optional_u64(action.latest_timestamp_unix),
            reasons,
            outcome_name(action.outcome)
        );
    }
    let _ = writeln!(output, "issues={}", report.issues.len());
    for issue in &report.issues {
        let _ = writeln!(
            output,
            "issue kind={} path={} message={}",
            quoted(&issue.kind)?,
            quoted(&issue.path)?,
            quoted(&issue.message)?
        );
    }
    Ok(output)
}

fn write_metrics(output: &mut String, prefix: &str, metrics: &TaskStoreMetrics) {
    let _ = writeln!(
        output,
        "{prefix}_state_logical_bytes={}",
        metrics.state_logical_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_state_allocated_bytes={}",
        metrics.state_allocated_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_allocated_bytes_supported={}",
        metrics.allocated_bytes_supported
    );
    let _ = writeln!(output, "{prefix}_state_files={}", metrics.state_files);
    let _ = writeln!(
        output,
        "{prefix}_state_directories={}",
        metrics.state_directories
    );
    let _ = writeln!(output, "{prefix}_state_symlinks={}", metrics.state_symlinks);
    let _ = writeln!(
        output,
        "{prefix}_task_registry_file_bytes={}",
        metrics.task_registry_file_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_registry_allocated_bytes={}",
        metrics.task_registry_allocated_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_registry_records={}",
        metrics.task_registry_records
    );
    let _ = writeln!(
        output,
        "{prefix}_task_registry_reliable={}",
        metrics.task_registry_reliable
    );
    let _ = writeln!(
        output,
        "{prefix}_task_artifact_logical_bytes={}",
        metrics.task_artifact_logical_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_artifact_allocated_bytes={}",
        metrics.task_artifact_allocated_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_artifact_files={}",
        metrics.task_artifact_files
    );
    let _ = writeln!(
        output,
        "{prefix}_task_artifact_directories={}",
        metrics.task_artifact_directories
    );
    let _ = writeln!(
        output,
        "{prefix}_task_event_logical_bytes={}",
        metrics.task_event_logical_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_event_allocated_bytes={}",
        metrics.task_event_allocated_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_task_event_files={}",
        metrics.task_event_files
    );
    let _ = writeln!(
        output,
        "{prefix}_retention_quarantine_logical_bytes={}",
        metrics.retention_quarantine_logical_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_retention_quarantine_allocated_bytes={}",
        metrics.retention_quarantine_allocated_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_retention_quarantine_groups={}",
        metrics.retention_quarantine_groups
    );
    let _ = writeln!(
        output,
        "{prefix}_managed_task_logical_bytes={}",
        metrics.managed_task_logical_bytes
    );
    let _ = writeln!(
        output,
        "{prefix}_managed_task_allocated_bytes={}",
        metrics.managed_task_allocated_bytes
    );
    let _ = writeln!(output, "{prefix}_active_tasks={}", metrics.active_tasks);
    let _ = writeln!(
        output,
        "{prefix}_oldest_task_timestamp_unix={}",
        optional_u64(metrics.oldest_task_timestamp_unix)
    );
    let _ = writeln!(
        output,
        "{prefix}_newest_task_timestamp_unix={}",
        optional_u64(metrics.newest_task_timestamp_unix)
    );
}

const fn mode_name(mode: RetentionMode) -> &'static str {
    match mode {
        RetentionMode::Inspect => "inspect",
        RetentionMode::DryRun => "dry_run",
        RetentionMode::Apply => "apply",
    }
}

const fn reason_name(reason: RetentionReason) -> &'static str {
    match reason {
        RetentionReason::AgeLimit => "age_limit",
        RetentionReason::SizeLimit => "size_limit",
    }
}

const fn outcome_name(outcome: RetentionOutcome) -> &'static str {
    match outcome {
        RetentionOutcome::WouldRemove => "would_remove",
        RetentionOutcome::Removed => "removed",
        RetentionOutcome::Skipped => "skipped",
        RetentionOutcome::Failed => "failed",
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn quoted(value: &str) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

#[cfg(test)]
mod tests {
    use packet28_daemon_core::retention::{
        RetentionAccounting, RetentionAction, RetentionOutcome, RetentionReason, TaskStoreIssue,
        TaskStoreMetrics, TaskStoreReport, TASK_STORE_REPORT_SCHEMA_VERSION,
    };

    use super::{cleanup_exit_code, render_json, render_text};

    fn fixture_report() -> TaskStoreReport {
        let before = TaskStoreMetrics {
            state_logical_bytes: 120,
            state_allocated_bytes: 160,
            allocated_bytes_supported: true,
            state_files: 4,
            state_directories: 3,
            state_symlinks: 0,
            task_registry_file_bytes: 20,
            task_registry_allocated_bytes: 24,
            task_registry_records: 2,
            task_registry_reliable: true,
            task_artifact_logical_bytes: 70,
            task_artifact_allocated_bytes: 80,
            task_artifact_files: 2,
            task_artifact_directories: 2,
            task_event_logical_bytes: 30,
            task_event_allocated_bytes: 32,
            task_event_files: 1,
            retention_quarantine_logical_bytes: 0,
            retention_quarantine_allocated_bytes: 0,
            retention_quarantine_groups: 0,
            managed_task_logical_bytes: 110,
            managed_task_allocated_bytes: 136,
            active_tasks: 1,
            oldest_task_timestamp_unix: Some(800),
            newest_task_timestamp_unix: Some(990),
        };
        let mut after = before.clone();
        after.state_logical_bytes = 75;
        after.state_allocated_bytes = 128;
        after.state_files = 2;
        after.task_registry_file_bytes = 15;
        after.task_registry_allocated_bytes = 16;
        after.task_registry_records = 1;
        after.task_artifact_logical_bytes = 40;
        after.task_artifact_allocated_bytes = 48;
        after.task_artifact_files = 1;
        after.task_event_logical_bytes = 20;
        after.task_event_allocated_bytes = 24;
        after.task_event_files = 0;
        after.managed_task_logical_bytes = 65;
        after.managed_task_allocated_bytes = 88;
        TaskStoreReport {
            schema_version: TASK_STORE_REPORT_SCHEMA_VERSION,
            observed_at_unix: 1_000,
            workspace_root: "/fixture/workspace".to_string(),
            state_root: "/fixture/workspace/.packet28".to_string(),
            mode: packet28_daemon_core::retention::RetentionMode::Apply,
            metrics_before: before,
            metrics_after: after,
            retention: RetentionAccounting {
                max_age_seconds: Some(100),
                max_bytes: Some(80),
                protected_tasks: 1,
                protected_logical_bytes: 65,
                planned_tasks: 1,
                planned_logical_bytes: 45,
                removed_tasks: 1,
                removed_logical_bytes: 45,
                skipped_tasks: 0,
                failed_tasks: 0,
                failed_logical_bytes: 0,
                recovered_precommit_groups: 0,
                recovered_committed_groups: 0,
                recovery_conflicted_groups: 0,
                final_rescan_reliable: true,
                action_byte_accounting_reliable: true,
                remaining_managed_logical_bytes: 65,
                remaining_over_limit_bytes: 0,
            },
            actions: vec![RetentionAction {
                storage_key: "task-old".to_string(),
                task_ids: vec!["task/old".to_string()],
                logical_bytes: 45,
                removed_logical_bytes: 45,
                remaining_logical_bytes: 0,
                byte_accounting_reliable: true,
                latest_timestamp_unix: Some(800),
                reasons: vec![RetentionReason::AgeLimit, RetentionReason::SizeLimit],
                outcome: RetentionOutcome::Removed,
            }],
            issues: vec![TaskStoreIssue {
                kind: "state_entry".to_string(),
                path: "/fixture/workspace/.packet28/unknown".to_string(),
                message: "unsupported special file".to_string(),
            }],
        }
    }

    #[test]
    fn text_output_matches_fixture() {
        assert_eq!(
            render_text(&fixture_report()).unwrap(),
            include_str!("../tests/fixtures/daemon_storage/report.txt")
        );
    }

    #[test]
    fn pretty_json_output_matches_fixture() {
        let actual = render_json(&fixture_report(), true).unwrap();
        let expected =
            include_str!("../tests/fixtures/daemon_storage/report.json").trim_end_matches('\n');
        assert_eq!(actual, expected);
    }

    #[test]
    fn cleanup_returns_failure_when_reported_progress_is_unreliable() {
        let mut report = fixture_report();
        report.retention.failed_tasks = 1;
        report.retention.action_byte_accounting_reliable = false;

        assert_eq!(cleanup_exit_code(&report), 1);
    }

    #[test]
    fn cleanup_returns_failure_for_a_failed_action_with_reliable_rescan() {
        let mut report = fixture_report();
        report.actions[0].outcome = RetentionOutcome::Failed;
        report.actions[0].removed_logical_bytes = 0;
        report.actions[0].remaining_logical_bytes = report.actions[0].logical_bytes;
        report.retention.removed_tasks = 0;
        report.retention.removed_logical_bytes = 0;
        report.retention.failed_tasks = 1;
        report.retention.failed_logical_bytes = report.actions[0].logical_bytes;
        assert!(report.retention.final_rescan_reliable);
        assert!(report.retention.action_byte_accounting_reliable);

        assert_eq!(cleanup_exit_code(&report), 1);
    }
}
