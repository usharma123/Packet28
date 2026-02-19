use std::process::Command;

use roaring::RoaringBitmap;

use crate::error::CovyError;
use crate::model::{DiffStatus, FileDiff};

/// Parse git diff output to extract changed files and line numbers.
pub fn git_diff(base: &str, head: &str) -> Result<Vec<FileDiff>, CovyError> {
    let output = Command::new("git")
        .args([
            "diff",
            "--unified=0",
            "--no-color",
            "--diff-filter=ACMR",
            &format!("{base}..{head}"),
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CovyError::GitNotFound
            } else {
                CovyError::Git(format!("Failed to run git diff: {e}"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CovyError::Git(format!("git diff failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_diff_output(&stdout)
}

/// Parse the raw output of `git diff --unified=0`.
pub fn parse_diff_output(diff_text: &str) -> Result<Vec<FileDiff>, CovyError> {
    let mut diffs = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_old_path: Option<String> = None;
    let mut current_status = DiffStatus::Modified;
    let mut current_lines = RoaringBitmap::new();

    for line in diff_text.lines() {
        if line.starts_with("diff --git") {
            // Flush previous file
            if let Some(path) = current_path.take() {
                diffs.push(FileDiff {
                    path,
                    old_path: current_old_path.take(),
                    status: current_status,
                    changed_lines: std::mem::take(&mut current_lines),
                });
            }
            current_status = DiffStatus::Modified;
            current_old_path = None;
        } else if line.starts_with("+++ b/") {
            current_path = Some(line[6..].to_string());
        } else if line.starts_with("+++ /dev/null") {
            // File was deleted — we skip deleted files (filtered by --diff-filter)
        } else if line.starts_with("new file") {
            current_status = DiffStatus::Added;
        } else if line.starts_with("rename from ") {
            current_old_path = Some(line["rename from ".len()..].to_string());
            current_status = DiffStatus::Renamed;
        } else if line.starts_with("@@") {
            // Parse @@ -old,count +new,count @@ ...
            if let Some(new_range) = parse_hunk_header(line) {
                for line_no in new_range.0..=new_range.1 {
                    current_lines.insert(line_no);
                }
            }
        }
    }

    // Flush last file
    if let Some(path) = current_path {
        diffs.push(FileDiff {
            path,
            old_path: current_old_path,
            status: current_status,
            changed_lines: current_lines,
        });
    }

    Ok(diffs)
}

/// Parse a hunk header like `@@ -10,3 +20,5 @@` and return (start, end) for the new side.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    // Find the +N,M or +N part
    let plus_idx = line.find('+').unwrap_or(0);
    let rest = &line[plus_idx + 1..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let range_str = &rest[..end];

    if let Some(comma) = range_str.find(',') {
        let start: u32 = range_str[..comma].parse().ok()?;
        let count: u32 = range_str[comma + 1..].parse().ok()?;
        if count == 0 {
            return None; // Pure deletion in old, no new lines
        }
        Some((start, start + count - 1))
    } else {
        let start: u32 = range_str.parse().ok()?;
        Some((start, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_single_line() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
    }

    #[test]
    fn test_parse_hunk_range() {
        assert_eq!(
            parse_hunk_header("@@ -10,3 +20,5 @@ fn foo"),
            Some((20, 24))
        );
    }

    #[test]
    fn test_parse_hunk_deletion_only() {
        assert_eq!(parse_hunk_header("@@ -10,3 +9,0 @@"), None);
    }

    #[test]
    fn test_parse_diff_output_basic() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,5 @@
+use std::io;
+
 fn main() {
-    println!("old");
+    println!("new");
+    io::stdout().flush().unwrap();
 }
"#;
        let result = parse_diff_output(diff).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "src/main.rs");
        assert_eq!(result[0].status, DiffStatus::Modified);
        assert!(result[0].changed_lines.contains(1));
        assert!(result[0].changed_lines.contains(5));
    }

    #[test]
    fn test_parse_diff_new_file() {
        let diff = r#"diff --git a/new.rs b/new.rs
new file mode 100644
index 0000000..abc123
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,3 @@
+fn new_func() {
+    todo!()
+}
"#;
        let result = parse_diff_output(diff).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, DiffStatus::Added);
        assert_eq!(result[0].changed_lines.len(), 3);
    }

    #[test]
    fn test_parse_diff_rename() {
        let diff = r#"diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1 +1 @@
-old line
+new line
"#;
        let result = parse_diff_output(diff).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, DiffStatus::Renamed);
        assert_eq!(result[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(result[0].path, "new.rs");
    }
}
