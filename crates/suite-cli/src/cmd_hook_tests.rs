use super::*;

#[test]
fn pretool_hook_output_surfaces_action_critic_without_rewrite() {
    let body = render_hook_output(
        HookEventKind::PreToolUse,
        &packet28_daemon_protocol::hooks::HookIngestResponse::default(),
        None,
        &["destructive_command: inspect scope first".to_string()],
    )
    .unwrap()
    .unwrap();
    let payload: Value = serde_json::from_str(&body).unwrap();
    let output = &payload["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "PreToolUse");
    assert!(output.get("updatedInput").is_none());
    assert!(output.get("permissionDecision").is_none());
    assert!(output["additionalContext"]
        .as_str()
        .unwrap()
        .contains("Packet28 action critic"));
}

#[test]
fn grep_hook_packet_preserves_actionable_regions_and_preview() {
    let input = json!({
        "pattern": r"fn classify\|Mutation",
        "include": ["crates/packet28-reducer-core/src/command.rs"]
    });
    let response = json!({
        "output": "crates/packet28-reducer-core/src/command.rs:16:pub fn classify_command(command: &str) {}\ncrates/packet28-reducer-core/src/command.rs:34:pub fn classify_command_argv(command: &str) {}\n"
    });

    let packet = build_grep_packet(&input, &response).unwrap();

    assert_eq!(
        packet.search_query.as_deref(),
        Some(r"fn classify\|Mutation")
    );
    assert!(packet
        .regions
        .contains(&"crates/packet28-reducer-core/src/command.rs:16-16".to_string()));
    assert!(packet
        .regions
        .contains(&"crates/packet28-reducer-core/src/command.rs:34-34".to_string()));
    let preview = packet.compact_preview.unwrap();
    assert!(preview.contains("Grep found 2 matches"));
    assert!(preview.contains("crates/packet28-reducer-core/src/command.rs:16:"));
}

#[test]
fn bash_grep_post_capture_preserves_actionable_regions_without_pretool_rewrite() {
    let input = json!({
        "command": r"grep -n 'fn classify\|Mutation\|fn classify_command' crates/packet28-reducer-core/src/command.rs"
    });
    let response = json!({
        "stdout": "16:pub fn classify_command(command: &str) -> Option<CommandReducerSpec> {\n34:pub fn classify_command_argv(command: &str, argv: &[String]) -> Option<CommandReducerSpec> {\n"
    });

    let packet = build_bash_packet(&input, &response).unwrap();

    assert_eq!(packet.tool_name, "Bash");
    assert_eq!(packet.packet_type, "packet28.hook.bash.grep.v1");
    assert_eq!(
        packet.search_query.as_deref(),
        Some(r"fn classify\|Mutation\|fn classify_command")
    );
    assert!(packet
        .regions
        .contains(&"crates/packet28-reducer-core/src/command.rs:16-16".to_string()));
    assert!(packet
        .regions
        .contains(&"crates/packet28-reducer-core/src/command.rs:34-34".to_string()));
    let preview = packet.compact_preview.unwrap();
    assert!(preview.contains("Grep found 2 matches"));
    assert!(preview.contains("crates/packet28-reducer-core/src/command.rs:16:"));
}

#[test]
fn runtime_capture_outputs_preserve_host_permissions_and_commands() {
    for runtime in [
        ExternalHookRuntime::Copilot,
        ExternalHookRuntime::Cursor,
        ExternalHookRuntime::Gemini,
        ExternalHookRuntime::Windsurf,
    ] {
        let output = render_runtime_hook_output(runtime, HookEventKind::PreToolUse).unwrap();
        if let Some(output) = output {
            let output: Value = serde_json::from_str(&output).unwrap();
            assert!(output.get("permissionDecision").is_none());
            assert!(output.get("permission").is_none());
            assert!(output.get("updatedInput").is_none());
            assert!(output.get("updated_input").is_none());
            assert!(output.get("hookSpecificOutput").is_none());
        }
    }
}

#[test]
fn runtime_posttool_capture_keeps_original_command_and_output() {
    for (runtime, payload, expected_kind) in [
        (
            ExternalHookRuntime::Cursor,
            json!({"command":"cat src/alpha.rs", "output":"fresh completion"}),
            "cursor_native",
        ),
        (
            ExternalHookRuntime::Copilot,
            json!({"toolName":"bash", "toolArgs": "{\"command\":\"cat src/alpha.rs\"}", "output":"fresh completion"}),
            "copilot_native",
        ),
        (
            ExternalHookRuntime::Gemini,
            json!({"tool_name":"run_shell_command", "tool_input":{"command":"cat src/alpha.rs"}, "output":"fresh completion"}),
            "gemini_native",
        ),
        (
            ExternalHookRuntime::Windsurf,
            json!({"tool_info":{"command_line":"cat src/alpha.rs"}}),
            "windsurf_native",
        ),
    ] {
        assert!(
            build_runtime_reducer_packet(runtime, &payload, HookEventKind::PreToolUse).is_none()
        );
        let packet =
            build_runtime_reducer_packet(runtime, &payload, HookEventKind::PostToolUse).unwrap();
        assert_eq!(packet.command.as_deref(), Some("cat src/alpha.rs"));
        assert_eq!(packet.reducer_family.as_deref(), Some(expected_kind));
        if expected_kind != "windsurf_native" {
            assert_eq!(packet.summary, "fresh completion");
        }
        assert_eq!(packet.canonical_command_kind.as_deref(), Some("shell"));
        assert_eq!(packet.artifact, Some(payload));
    }
}

#[test]
fn rust_workspace_fingerprint_changes_for_out_of_band_source_edit() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    let spec = classify_command("cargo test --lib").unwrap();

    let before = workspace_cache_fingerprint(dir.path(), dir.path(), &spec);
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 2 }\n",
    )
    .unwrap();
    let after = workspace_cache_fingerprint(dir.path(), dir.path(), &spec);

    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn rust_workspace_fingerprint_skips_symlink_cycles() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    symlink(dir.path(), dir.path().join("src/loop")).unwrap();
    let spec = classify_command("cargo test --lib").unwrap();

    let fingerprint = workspace_cache_fingerprint(dir.path(), dir.path(), &spec);
    assert!(!fingerprint.is_empty());
}

#[test]
fn post_tool_skips_reducer_runner_command() {
    let packet = build_reducer_packet(
        &HookRuntimeConfig::default(),
        &json!({
            "tool_name":"Bash",
            "tool_input":{"command":"Packet28 hook reducer-runner --root . -- task"},
            "tool_response":{"stdout":"done"}
        }),
        HookEventKind::PostToolUse,
    );
    assert!(packet.is_none());
}

#[test]
fn post_tool_failure_captures_failed_bash_packet() {
    let packet = build_reducer_packet(
        &HookRuntimeConfig::default(),
        &json!({
            "tool_name":"Bash",
            "tool_input":{"command":"git status --short src/lib.rs"},
            "error":"fatal: not a git repository"
        }),
        HookEventKind::PostToolUseFailure,
    )
    .unwrap();
    assert!(packet.failed);
    assert_eq!(packet.reducer_family.as_deref(), Some("git"));
    assert_eq!(packet.canonical_command_kind.as_deref(), Some("git_status"));
    assert!(packet.summary.contains("fatal: not a git repository"));
}

#[test]
fn read_reducer_marks_read_operation() {
    let packet = build_read_packet(
        &json!({"file_path":"src/lib.rs","offset":10,"limit":5}),
        &json!({"content":"demo"}),
    )
    .unwrap();
    assert_eq!(
        packet.operation_kind,
        suite_packet_core::ToolOperationKind::Read
    );
    assert_eq!(packet.paths, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        packet.cache_fingerprint.as_deref(),
        Some("read:src/lib.rs:10:14")
    );
}

#[test]
fn hook_runtime_config_defaults_to_capture_only_when_file_is_missing() {
    let root = tempfile::tempdir().unwrap();

    let config = load_hook_runtime_config(root.path()).unwrap();

    assert_eq!(
        (
            config.hooks_enabled,
            config.rewrite_enabled,
            config.fallback_post_tool_capture,
        ),
        (true, false, true)
    );
}

#[test]
fn hook_runtime_config_rejects_malformed_json_without_replacing_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = hook_runtime_config_path(root.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = b"{\"hooks_enabled\": tru".to_vec();
    fs::write(&path, &original).unwrap();

    let error = load_hook_runtime_config(root.path()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to parse hook runtime config"),
        "{error:#}"
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn hook_runtime_config_rejects_invalid_utf8_without_replacing_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = hook_runtime_config_path(root.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = vec![b'{', b'}', 0xff];
    fs::write(&path, &original).unwrap();

    let error = load_hook_runtime_config(root.path()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to read hook runtime config"),
        "{error:#}"
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn hook_runtime_config_rejects_non_file_path_as_unreadable() {
    let root = tempfile::tempdir().unwrap();
    let path = hook_runtime_config_path(root.path());
    fs::create_dir_all(&path).unwrap();

    let error = load_hook_runtime_config(root.path()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to read hook runtime config"),
        "{error:#}"
    );
    assert!(path.is_dir());
}

#[test]
fn disabled_hooks_never_bootstrap_background_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = packet28_daemon_protocol::paths::hook_runtime_config_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let config = HookRuntimeConfig {
        hooks_enabled: false,
        ..HookRuntimeConfig::default()
    };
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    for event in ["SessionStart", "SubagentStart", "SubagentStop", "Stop"] {
        let outcome =
            process_claude_hook_payload(dir.path(), None, &json!({"hook_event_name": event}), true)
                .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.body.is_none());
    }
    assert!(!dir.path().join(".packet28/daemon/runtime.json").exists());
    assert!(!dir.path().join(".packet28/daemon/packet28d.log").exists());
    assert!(!dir
        .path()
        .join(".packet28/daemon/packet28-hook-http.log")
        .exists());
}

#[test]
fn codex_pretool_output_preserves_host_permission_and_command_identity() {
    let body = render_hook_output(
        HookEventKind::PreToolUse,
        &packet28_daemon_protocol::hooks::HookIngestResponse::default(),
        None,
        &["Review current Alpha before editing".to_string()],
    )
    .unwrap()
    .unwrap();
    let output: Value = serde_json::from_str(&body).unwrap();
    let specific = &output["hookSpecificOutput"];
    assert!(specific.get("updatedInput").is_none());
    assert!(specific.get("permissionDecision").is_none());
    assert!(specific["additionalContext"]
        .as_str()
        .unwrap()
        .contains("Review current Alpha"));
}

#[test]
fn fresh_claude_and_codex_sessions_use_distinct_task_namespaces() {
    let claude = tempfile::tempdir().unwrap();
    let codex = tempfile::tempdir().unwrap();
    let session = "same-host-session-id";
    let claude_id = resolve_task_id(claude.path(), &json!({}), Some(session), "claude").unwrap();
    let codex_id = resolve_task_id(codex.path(), &json!({}), Some(session), "codex").unwrap();
    assert_ne!(claude_id, codex_id);
    assert_eq!(
        claude_id,
        crate::task_runtime::derive_claude_task_id(session)
    );
    assert_eq!(
        resolve_task_id(codex.path(), &json!({}), Some(session), "codex").unwrap(),
        codex_id
    );
    assert_eq!(
        resolve_task_id(
            codex.path(),
            &json!({"task_id":"explicit-continuation"}),
            Some(session),
            "codex"
        )
        .unwrap(),
        "explicit-continuation"
    );
    assert_eq!(
        resolve_task_id(codex.path(), &json!({}), Some(session), "codex").unwrap(),
        "explicit-continuation"
    );
}
