#!/usr/bin/env python3
"""Required outcomes for the Packet28 agent-DX workflow benchmark.

Every scenario below is required. A scenario is either *measured* (the
benchmark runs it against the real binaries and records evidence for every
check) or *delegated* (its contract needs fault injection or seeded authority
state that only the Rust process tests can create; the canonical full gate runs
them). Delegated contracts are bound to exact test functions, and validation
fails if one is renamed or deleted.

Budgets that gate a result come from documented product contracts. Token
savings, byte counts, latency and round trips are reported as diagnostics.
"""

from __future__ import annotations

BENCHMARK = "packet28-agent-dx"
SCHEMA_VERSION = 1
# docs/operations.md: managed logs keep the active file and three backups,
# each within PACKET28_DAEMON_LOG_MAX_BYTES.
LOG_MAX_BYTES = 4096
LOG_GENERATIONS = 4
AUTHORITY_FIELDS = ("updatedInput", "permissionDecision", "decision")

MEASURED = {
    "setup_fresh_index": {
        "intent": "A fresh committed repository is usable by an agent right after setup: "
                  "setup's own files do not block the index and indexed search finds user code.",
        "checks": {
            "setup_succeeded": "setup exits 0 and reports `index ready`, not `index deferred`",
            "index_attests_setup_changes": "the regex index attests HEAD plus setup's dirty files",
            "user_source_unchanged": "setup leaves user source bytes unchanged",
            "indexed_search_finds_user_code": "MCP search answers from the index and finds the user symbol",
            "indexed_search_finds_generated_guidance": "MCP search finds the setup-generated agent guidance",
        },
    },
    "hooks_disabled_honesty": {
        "intent": "With hooks disabled, doctor says so and generated handlers capture nothing; "
                  "explicit setup re-enables capture without losing configuration.",
        "checks": {
            "disabled_doctor_reports_reason": "doctor exits non-zero and names disabled hook ingest",
            "disabled_config_unchanged": "doctor does not rewrite the disabled runtime config",
            "disabled_handler_captures_nothing": "the generated handler exits 0 and records no event",
            "setup_reactivates_hooks": "setup sets hooks_enabled=true",
            "reactivated_handler_captures_once": "the same generated handler records exactly one event",
            "enabled_doctor_passes": "doctor exits 0 once hooks are enabled",
        },
    },
    "hooks_capture_only": {
        "intent": "Hooks observe; they never rewrite tool input or decide permissions, "
                  "even when a legacy rewrite flag is stored.",
        "checks": {
            "pretool_never_rewrites_or_decides": "Claude and Codex PreToolUse output has no updatedInput, "
                                                 "permissionDecision or decision for any probe command",
            "legacy_rewrite_flag_inactive": "a stored rewrite_enabled=true changes no hook output and is not rewritten",
        },
    },
    "native_retrieval": {
        "intent": "An agent finds code through slim MCP results and retrieves the exact preserved "
                  "evidence when it needs it.",
        "checks": {
            "search_returns_fetchable_artifact": "slim search names matching regions and a fetchable artifact",
            "fetched_matches_equal_source": "every fetched match line equals the source file line",
            "read_regions_exact": "read_regions returns the exact requested source lines",
            "glob_paths_exist": "glob returns existing files that match the pattern",
        },
    },
    "same_task_sessions": {
        "intent": "Fresh and concurrent MCP processes can work on the same task without "
                  "colliding or replacing earlier evidence.",
        "checks": {
            "sequential_fresh_sessions_succeed": "two sequential fresh processes search the same task",
            "concurrent_sessions_succeed": "two concurrent processes search the same task",
            "artifact_ids_distinct": "every call publishes a distinct artifact",
            "earlier_evidence_retrievable": "a later process fetches every earlier artifact unchanged",
        },
    },
    "handoff_cold_restart": {
        "intent": "Work continues across a host stop boundary and a true daemon restart.",
        "checks": {
            "handoff_ready_after_stop_boundary": "after intention + Stop hook, prepare_handoff is ready",
            "stop_releases_authority": "when `daemon stop` returns the instance lock is free, "
                                       "readiness is withdrawn and the process has exited",
            "restart_without_retry": "one `daemon start` succeeds with a new process",
            "fresh_session_resumes_handoff": "a new MCP process fetches the handoff with the latest intention",
            "index_serves_after_restart": "indexed search still answers after restart",
        },
    },
    "corrupt_history_recovery": {
        "intent": "A task whose event history was damaged while stopped continues through an "
                  "authenticated successor; damaged evidence is preserved, never rewritten.",
        "checks": {
            "first_call_after_corruption_succeeds": "the first tool call on the original task id succeeds",
            "successor_lineage_recorded": "the predecessor names a distinct successor that records its origin",
            "damaged_history_quarantined_exactly": "the quarantined log equals the damaged bytes",
            "inherited_context_available": "the predecessor's handoff and intention are readable after recovery",
            "successor_history_starts_at_one": "the successor's first event has sequence 1",
        },
    },
    "runtime_log_bounds": {
        "intent": "A running daemon keeps its log bounded while it serves errors, without restarting.",
        "checks": {
            "same_daemon_process": "the daemon PID is unchanged across the error burst",
            "generations_within_limit": f"at most {LOG_GENERATIONS} generations, each at most the threshold",
            "diagnostics_retained": "the latest error diagnostic is retained",
        },
    },
    "explicit_cli_reduction": {
        "intent": "Explicit reduction keeps identity, URL, body preview, errors, exit codes and "
                  "omission notices, and verbose output gets smaller.",
        "checks": {
            "every_frozen_case_replayed": "every manifest case rendered its frozen bytes through one stub call",
            "derived_contracts_hold": "every case keeps its derived semantic contract and output budget",
            "verbose_cases_reduced": "every verbose case has fewer visible bytes than raw bytes",
        },
    },
    "cleanup": {
        "intent": "The benchmark leaves no owned daemon or hook server running.",
        "checks": {
            "authority_released": "the daemon instance lock is free",
            "owned_processes_exited": "no process references the benchmark workspace",
        },
    },
}

# Contracts proven by Rust process tests run in the canonical full gate.
DELEGATED = {
    "dormant_oversized_archival": {
        "intent": "Oversized dormant records are diagnosed before the page limit and archived "
                  "online without touching healthy tasks.",
        "why_delegated": "a record above 512 KiB cannot be created through supported writes; "
                         "the tests seed one with the daemon-core storage API",
        "tests": [
            "crates/packet28d/tests/task_record_archive.rs::oversized_dormant_record_is_archived_online_and_survives_restart",
            "crates/packet28d/tests/task_record_archive.rs::archive_refuses_active_pointer_and_recovery_owners",
            "crates/packet28d/tests/task_record_archive.rs::forward_fields_are_archived_sized_and_never_lost_through_restart",
            "crates/packet28d/tests/status_pagination.rs::oversized_records_before_between_and_after_healthy_pages_are_all_reported",
        ],
    },
    "large_store_paging": {
        "intent": "A 5,000-task store becomes ready and pages every task within the 5-second query bound.",
        "why_delegated": "seeding 5,000 records needs the storage API and minutes of debug startup",
        "tests": [
            "crates/packet28d/tests/status_pagination.rs::seeded_five_thousand_task_daemon_keeps_status_live_and_pages_every_task",
        ],
    },
    "registry_journal_repair": {
        "intent": "Registry/journal mismatches are repaired only from authenticated images, "
                  "with backups; ambiguous authority fails closed.",
        "why_delegated": "needs crash-phase injection inside checkpoint publication",
        "tests": [
            "crates/suite-cli/tests/daemon_storage_repair_e2e.rs::storage_repair_restores_a_whitespace_edited_registry_exactly",
            "crates/suite-cli/tests/daemon_storage_repair_e2e.rs::storage_repair_reports_no_safe_recovery_and_changes_nothing",
            "crates/suite-cli/tests/daemon_storage_repair_e2e.rs::storage_repair_completes_after_a_process_interruption",
            "crates/packet28d/tests/registry_repair_startup.rs::repair_refuses_while_the_daemon_is_live",
        ],
    },
    "lifecycle_deadlines": {
        "intent": "Stop and start honor authority release and bounded waits under contention.",
        "why_delegated": "needs held locks, slow starters and TERM-resistant children",
        "tests": [
            "crates/suite-cli/tests/daemon_lifecycle_e2e.rs::stop_waits_for_daemon_authority_release_and_concurrent_start_succeeds",
            "crates/suite-cli/tests/daemon_lifecycle_e2e.rs::stop_timeout_fails_without_touching_runtime_files_of_a_live_owner",
            "crates/suite-cli/tests/daemon_lifecycle_e2e.rs::start_timeout_for_existing_starting_daemon_is_bounded_and_leaves_it_running",
            "crates/packet28d/tests/daemon_lifecycle.rs::stop_terminates_term_resistant_process_group_before_releasing_leases",
        ],
    },
    "fresh_command_execution": {
        "intent": "Every explicit command executes freshly; capture failures never replace the original result.",
        "why_delegated": "needs injected capture rejection and signal exits",
        "tests": [
            "crates/suite-cli/tests/hook_runner_e2e.rs::test_hook_runner_cli_executes_every_explicit_request",
            "crates/suite-cli/tests/hook_runner_e2e.rs::test_hook_runner_capture_rejection_executes_original_command",
            "crates/suite-cli/tests/hook_runner_e2e.rs::test_hook_runner_completion_capture_failure_preserves_result_without_rerun",
        ],
    },
    "pr_view_renderer_edges": {
        "intent": "PR previews keep UTF-8 boundaries, malformed headers and filtered lines safe.",
        "why_delegated": "edge inputs are unit-sized and covered directly at the reducer",
        "tests": [
            "crates/packet28-reducer-core/src/github.rs::pr_view_preview_bounds_long_unicode_body_without_cutting_characters",
            "crates/packet28-reducer-core/src/github.rs::pr_view_preview_keeps_metadata_when_header_separator_is_absent",
            "crates/packet28-reducer-core/src/github.rs::pr_view_preview_bounds_body_lines_and_discloses_filtered_content",
            "crates/packet28-reducer-core/src/github.rs::pr_view_reduction_preserves_complete_failed_output_and_exit",
        ],
    },
    "running_log_rotation": {
        "intent": "Hook servers and contending log owners stay within the log bound while running.",
        "why_delegated": "needs contending owners and legacy oversized logs",
        "tests": [
            "crates/suite-cli/tests/runtime_log_rotation_e2e.rs::background_hook_server_rotates_its_own_log_after_setup_exits",
            "crates/suite-cli/tests/runtime_log_rotation_e2e.rs::managed_daemon_reduces_legacy_logs_and_serializes_contending_owners",
        ],
    },
}

# Diagnostics are recorded and shown, never gated.
DIAGNOSTIC_METRICS = (
    "elapsed_ms",
    "mcp_round_trips",
    "acquisition_tokens",
    "required_retrieval_tokens",
    "verification_retrieval_tokens",
    "all_full_retrieval_tokens",
    "token_reduction_pct",
    "raw_est_tokens",
    "reduced_est_tokens",
)


def required_scenarios() -> list[str]:
    return list(MEASURED)


def check_statement(scenario: str, check: str) -> str:
    return MEASURED.get(scenario, {}).get("checks", {}).get(check, check)
