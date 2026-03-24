//! Tee output recovery: save raw command output for post-mortem retrieval.
//!
//! When a command fails, the reducer may discard raw output in favor of a
//! compact summary. Tee captures the raw output to disk so it can be
//! retrieved later via `compact fetch-raw`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Controls when raw output is tee'd to disk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TeeMode {
    /// Never tee output.
    #[default]
    Never,
    /// Tee only when the command fails (non-zero exit code).
    Failures,
    /// Always tee raw output.
    Always,
}

/// Configuration for tee output capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TeeConfig {
    pub enabled: bool,
    pub mode: TeeMode,
    pub directory: String,
    pub max_files: usize,
    pub max_file_size: usize,
}

impl Default for TeeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TeeMode::Failures,
            directory: default_tee_directory(),
            max_files: 20,
            max_file_size: 1_048_576, // 1MB
        }
    }
}

fn default_tee_directory() -> String {
    dirs_or_fallback("packet28/tee")
}

fn dirs_or_fallback(suffix: &str) -> String {
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/.local/share/{suffix}");
        }
    }
    format!("/tmp/{suffix}")
}

/// Should tee be performed for the given exit code?
pub fn should_tee(config: &TeeConfig, exit_code: i32) -> bool {
    if !config.enabled {
        return false;
    }
    match config.mode {
        TeeMode::Never => false,
        TeeMode::Failures => exit_code != 0,
        TeeMode::Always => true,
    }
}

/// Write raw output to tee directory and return the file path.
///
/// Skips small outputs (<500 chars) to avoid noise.
/// Enforces max_file_size by truncating.
/// Rotates old files to stay within max_files.
pub fn tee_raw(config: &TeeConfig, raw: &str, slug: &str, exit_code: i32) -> Option<PathBuf> {
    if !should_tee(config, exit_code) {
        return None;
    }

    // Skip small outputs
    if raw.len() < 500 {
        return None;
    }

    let dir = PathBuf::from(&config.directory);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("tee: failed to create directory '{}': {e}", dir.display());
        return None;
    }

    // Rotate old files
    rotate_tee_files(&dir, config.max_files.saturating_sub(1));

    // Sanitize slug for filename
    let safe_slug: String = slug
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .take(60)
        .collect();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let filename = format!("{timestamp}-{safe_slug}-exit{exit_code}.txt");
    let path = dir.join(&filename);

    // Truncate to max_file_size
    let content = if raw.len() > config.max_file_size {
        let truncated = &raw[..config.max_file_size.saturating_sub(50)];
        format!(
            "{truncated}\n\n... (truncated at {} bytes)",
            config.max_file_size
        )
    } else {
        raw.to_string()
    };

    if let Err(e) = fs::write(&path, &content) {
        eprintln!("tee: failed to write '{}': {e}", path.display());
        return None;
    }

    Some(path)
}

/// Tee raw output and return a hint string for the LLM.
pub fn tee_and_hint(config: &TeeConfig, raw: &str, slug: &str, exit_code: i32) -> Option<String> {
    let path = tee_raw(config, raw, slug, exit_code)?;
    Some(format!(
        "Full raw output saved to: {} ({} bytes). Use `compact fetch-raw` to retrieve.",
        path.display(),
        raw.len()
    ))
}

fn rotate_tee_files(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((path, modified))
        })
        .collect();

    if files.len() <= keep {
        return;
    }

    // Sort by modification time, oldest first
    files.sort_by_key(|(_, time)| *time);

    let to_remove = files.len() - keep;
    for (path, _) in files.into_iter().take(to_remove) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_tee_respects_mode() {
        let config = TeeConfig {
            enabled: true,
            mode: TeeMode::Failures,
            ..TeeConfig::default()
        };
        assert!(should_tee(&config, 1));
        assert!(!should_tee(&config, 0));

        let always = TeeConfig {
            enabled: true,
            mode: TeeMode::Always,
            ..TeeConfig::default()
        };
        assert!(should_tee(&always, 0));
        assert!(should_tee(&always, 1));

        let never = TeeConfig {
            enabled: true,
            mode: TeeMode::Never,
            ..TeeConfig::default()
        };
        assert!(!should_tee(&never, 0));
        assert!(!should_tee(&never, 1));
    }

    #[test]
    fn should_tee_disabled() {
        let config = TeeConfig {
            enabled: false,
            mode: TeeMode::Always,
            ..TeeConfig::default()
        };
        assert!(!should_tee(&config, 1));
    }

    #[test]
    fn tee_raw_skips_small_output() {
        let config = TeeConfig {
            enabled: true,
            mode: TeeMode::Always,
            directory: "/tmp/packet28-tee-test".to_string(),
            ..TeeConfig::default()
        };
        let small = "short";
        assert!(tee_raw(&config, small, "test", 1).is_none());
    }

    #[test]
    fn tee_raw_writes_large_output() {
        let dir = format!("/tmp/packet28-tee-test-{}", std::process::id());
        let config = TeeConfig {
            enabled: true,
            mode: TeeMode::Always,
            directory: dir.clone(),
            max_files: 5,
            max_file_size: 1_048_576,
        };
        let large = "x".repeat(600);
        let result = tee_raw(&config, &large, "my-test-cmd", 1);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.len() >= 600);

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }
}
