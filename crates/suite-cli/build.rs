use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace_manifest = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .join("Cargo.toml");

    println!("cargo:rerun-if-changed={}", workspace_manifest.display());

    let version = workspace_version(&workspace_manifest)
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").expect("package version"));
    println!("cargo:rustc-env=PACKET28_VERSION={version}");
}

fn workspace_version(path: &PathBuf) -> Option<String> {
    let manifest = fs::read_to_string(path).ok()?;
    let mut in_workspace_package = false;

    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_workspace_package = line == "[workspace.package]";
            continue;
        }
        if !in_workspace_package || line.starts_with('#') || !line.starts_with("version") {
            continue;
        }
        let (_, value) = line.split_once('=')?;
        return Some(value.trim().trim_matches('"').to_string());
    }

    None
}
