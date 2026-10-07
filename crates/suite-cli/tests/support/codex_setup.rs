use packet28_daemon_protocol::hooks::HookRuntimeConfig;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};

pub fn assert_native_hook_installation(root: &Path) {
    let config: Value =
        serde_json::from_slice(&fs::read(root.join(".codex/hooks.json")).unwrap()).unwrap();
    let hooks = config["hooks"].as_object().unwrap();
    assert_eq!(
        hooks.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PreCompact",
            "Stop",
            "SessionEnd",
        ])
    );
    for entries in hooks.values() {
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let handlers = entries[0]["hooks"].as_array().unwrap();
        assert_eq!(handlers.len(), 1);
        let handler = &handlers[0];
        assert_eq!(handler["type"], "command");
        assert_eq!(handler["timeout"], 10);
        let argv = shell_words::split(handler["command"].as_str().unwrap()).unwrap();
        assert_eq!(argv.len(), 6);
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("exec \"$1\" hook codex --root \"$2\""));
        assert_eq!(argv[3], "packet28-hook");
        assert!(!argv[4].is_empty());
        assert_eq!(
            fs::canonicalize(&argv[5]).unwrap(),
            root.canonicalize().unwrap()
        );
    }
    let runtime: HookRuntimeConfig = serde_json::from_slice(
        &fs::read(packet28_daemon_protocol::paths::hook_runtime_config_path(
            root,
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(runtime.hooks_enabled);
}
