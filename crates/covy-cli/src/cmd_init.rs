use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

#[derive(Args)]
pub struct InitArgs {
    /// Overwrite existing covy.toml
    #[arg(long)]
    pub force: bool,

    /// Print config to stdout without writing
    #[arg(long)]
    pub dry_run: bool,

    /// Initialize at the git repository root instead of current directory
    #[arg(long)]
    pub repo_root: bool,

    /// Emit JSON summary output
    #[arg(long)]
    pub json: bool,
}

/// Well-known coverage report globs to scan for during auto-discovery.
const DISCOVERY_GLOBS: &[&str] = &[
    "**/jacoco.xml",
    "**/jacoco-report.xml",
    "**/coverage.xml",
    "**/cobertura.xml",
    "**/lcov.info",
    "**/coverage.lcov",
    "coverage/*.info",
    "**/cover.out",
    "**/llvm-cov-export.json",
];

/// Directories to exclude from discovery results.
const EXCLUDE_DIRS: &[&str] = &["/.git/", "/node_modules/", "/bazel-out/"];

#[derive(Debug, serde::Serialize)]
struct InitSummary {
    config_path: String,
    state_dir: String,
    cache_dir: String,
    project_name: String,
    discovered_reports: Vec<String>,
    dry_run: bool,
}

pub fn run(args: InitArgs, config_path: &str) -> Result<i32> {
    let config_path = resolve_init_config_path(config_path, args.repo_root)?;
    let target_root = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);

    if config_path.exists() && !args.force {
        anyhow::bail!(
            "covy.toml already exists at {}. Use --force to overwrite.",
            config_path.display()
        );
    }

    let discovered = discover_coverage_reports(&target_root);
    let project_name = detect_project_name(&target_root);

    let report_paths: Vec<String> = discovered
        .iter()
        .map(|p| {
            p.strip_prefix(&target_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let config_content = generate_config(&project_name, &report_paths);

    let state_dir = target_root.join(".covy/state");
    let cache_dir = target_root.join(".covy/cache");

    let summary = InitSummary {
        config_path: config_path.display().to_string(),
        state_dir: state_dir.display().to_string(),
        cache_dir: cache_dir.display().to_string(),
        project_name,
        discovered_reports: report_paths,
        dry_run: args.dry_run,
    };

    if args.dry_run {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            println!("{config_content}");
        }
        return Ok(0);
    }

    std::fs::write(&config_path, &config_content)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    // Create state and cache directories
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Failed to create {}", cache_dir.display()))?;

    // Print summary
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("Created {}", config_path.display());
        println!("Created {}", state_dir.display());
        println!("Created {}", cache_dir.display());

        if summary.discovered_reports.is_empty() {
            println!("\nNo coverage reports were auto-discovered.");
            println!("Edit covy.toml and add your report paths to [ingest].report_paths.");
        } else {
            println!(
                "\nDiscovered {} coverage report(s):",
                summary.discovered_reports.len()
            );
            for rp in &summary.discovered_reports {
                println!("  - {rp}");
            }
        }

        println!("\nNext steps:");
        println!("  1. Review covy.toml and adjust as needed");
        println!("  2. Run `covy doctor` to validate your setup");
        println!("  3. Run `covy check <report>` to see coverage results");
    }

    Ok(0)
}

fn resolve_init_config_path(config_path: &str, use_repo_root: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("Failed to resolve current directory")?;
    let cfg = Path::new(config_path);

    let default_cfg_name = "covy.toml";
    if cfg.is_absolute() || config_path != default_cfg_name {
        return Ok(if cfg.is_absolute() {
            cfg.to_path_buf()
        } else {
            cwd.join(cfg)
        });
    }

    if use_repo_root {
        return Ok(crate::cmd_common::detect_repo_root()?.join(default_cfg_name));
    }

    Ok(cwd.join(default_cfg_name))
}

fn discover_coverage_reports(repo_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();

    for pattern in DISCOVERY_GLOBS {
        let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
        if let Ok(entries) = glob::glob(&full_pattern) {
            for entry in entries.flatten() {
                let rel = entry.to_string_lossy().replace('\\', "/");
                if EXCLUDE_DIRS.iter().any(|d| rel.contains(d)) {
                    continue;
                }
                if !found.contains(&entry) {
                    found.push(entry);
                }
            }
        }
    }

    found.sort();
    found
}

fn detect_project_name(repo_root: &Path) -> String {
    // Try package.json
    let package_json = repo_root.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&package_json) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(name) = parsed.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }

    // Try Cargo.toml
    let cargo_toml = repo_root.join("Cargo.toml");
    if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
        if let Ok(parsed) = content.parse::<toml::Value>() {
            if let Some(name) = parsed
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
            // Try workspace package name
            if let Some(name) = parsed
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }

    // Try pom.xml artifactId
    let pom_xml = repo_root.join("pom.xml");
    if let Ok(content) = std::fs::read_to_string(&pom_xml) {
        if let Some(start) = content.find("<artifactId>") {
            let rest = &content[start + "<artifactId>".len()..];
            if let Some(end) = rest.find("</artifactId>") {
                let artifact_id = rest[..end].trim();
                if !artifact_id.is_empty() {
                    return artifact_id.to_string();
                }
            }
        }
    }

    // Fall back to directory name
    repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("my-project")
        .to_string()
}

fn generate_config(project_name: &str, report_paths: &[String]) -> String {
    let mut config = String::new();

    config.push_str("[project]\n");
    config.push_str(&format!("name = \"{project_name}\"\n"));
    config.push('\n');

    config.push_str("[ingest]\n");
    if report_paths.is_empty() {
        config.push_str("# Add your coverage report globs here:\n");
        config.push_str("# report_paths = [\"**/jacoco.xml\", \"**/lcov.info\"]\n");
        config.push_str("report_paths = []\n");
    } else {
        config.push_str("report_paths = [\n");
        for rp in report_paths {
            config.push_str(&format!("  \"{rp}\",\n"));
        }
        config.push_str("]\n");
    }
    config.push('\n');

    config.push_str("[diff]\n");
    config.push_str("base = \"origin/main\"\n");
    config.push_str("head = \"HEAD\"\n");

    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_project_name_from_package_json() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "my-app", "version": "1.0.0"}"#,
        )
        .unwrap();
        assert_eq!(detect_project_name(dir.path()), "my-app");
    }

    #[test]
    fn test_detect_project_name_from_cargo_toml() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(detect_project_name(dir.path()), "my-crate");
    }

    #[test]
    fn test_detect_project_name_from_pom_xml() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("pom.xml"),
            "<project><artifactId>my-java-app</artifactId></project>",
        )
        .unwrap();
        assert_eq!(detect_project_name(dir.path()), "my-java-app");
    }

    #[test]
    fn test_detect_project_name_fallback_to_dir() {
        let dir = TempDir::new().unwrap();
        let name = detect_project_name(dir.path());
        assert!(!name.is_empty());
    }

    #[test]
    fn test_generate_config_with_reports() {
        let config = generate_config("demo", &["src/jacoco.xml".to_string()]);
        assert!(config.contains("name = \"demo\""));
        assert!(config.contains("\"src/jacoco.xml\""));
        assert!(config.contains("[ingest]"));
        assert!(config.contains("[diff]"));
    }

    #[test]
    fn test_generate_config_without_reports() {
        let config = generate_config("demo", &[]);
        assert!(config.contains("report_paths = []"));
        assert!(config.contains("# Add your coverage report globs here:"));
    }

    #[test]
    fn test_discover_excludes_node_modules_dir() {
        let dir = TempDir::new().unwrap();
        let nm_dir = dir.path().join("node_modules/pkg");
        std::fs::create_dir_all(&nm_dir).unwrap();
        std::fs::write(nm_dir.join("coverage.xml"), "<report/>").unwrap();

        let found = discover_coverage_reports(dir.path());
        assert!(found.is_empty());
    }

    #[test]
    fn test_discover_finds_reports() {
        let dir = TempDir::new().unwrap();
        let report_dir = dir.path().join("reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        std::fs::write(report_dir.join("lcov.info"), "TN:\n").unwrap();

        let found = discover_coverage_reports(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("lcov.info"));
    }

    #[test]
    fn test_resolve_init_config_path_defaults_to_cwd() {
        let _guard = crate::cmd_common::cwd_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let path = resolve_init_config_path("covy.toml", false).unwrap();
        std::env::set_current_dir(old).unwrap();
        let actual = path.to_string_lossy().replace("/private", "");
        let expected = dir
            .path()
            .join("covy.toml")
            .to_string_lossy()
            .replace("/private", "");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_resolve_init_config_path_honors_explicit_relative_config() {
        let _guard = crate::cmd_common::cwd_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let path = resolve_init_config_path("configs/custom.toml", false).unwrap();
        std::env::set_current_dir(old).unwrap();
        let actual = path.to_string_lossy().replace("/private", "");
        let expected = dir
            .path()
            .join("configs/custom.toml")
            .to_string_lossy()
            .replace("/private", "");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_resolve_init_config_path_repo_root_mode() {
        let _guard = crate::cmd_common::cwd_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init"])
            .status()
            .unwrap();
        assert!(status.success());

        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(&sub).unwrap();
        let path = resolve_init_config_path("covy.toml", true).unwrap();
        std::env::set_current_dir(old).unwrap();

        let actual = path.to_string_lossy().replace("/private", "");
        let expected = dir
            .path()
            .join("covy.toml")
            .to_string_lossy()
            .replace("/private", "");
        assert_eq!(actual, expected);
    }
}
