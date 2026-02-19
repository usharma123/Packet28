use covy_core::diff::parse_diff_output;
use covy_core::model::DiffStatus;

#[test]
fn test_parse_multi_file_diff() {
    let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc..def 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -5,0 +6,3 @@
+fn new_func() {
+    println!("hello");
+}
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,2 +10,4 @@
+    let x = 1;
+    let y = 2;
"#;

    let diffs = parse_diff_output(diff).unwrap();
    assert_eq!(diffs.len(), 2);

    assert_eq!(diffs[0].path, "src/main.rs");
    assert_eq!(diffs[0].status, DiffStatus::Modified);
    assert!(diffs[0].changed_lines.contains(6));
    assert!(diffs[0].changed_lines.contains(8));

    assert_eq!(diffs[1].path, "src/lib.rs");
    assert!(diffs[1].changed_lines.contains(10));
}

#[test]
fn test_parse_new_file_diff() {
    let diff = r#"diff --git a/new_file.rs b/new_file.rs
new file mode 100644
index 0000000..abc1234
--- /dev/null
+++ b/new_file.rs
@@ -0,0 +1,5 @@
+fn main() {
+    let x = 1;
+    let y = 2;
+    println!("{}", x + y);
+}
"#;

    let diffs = parse_diff_output(diff).unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].status, DiffStatus::Added);
    assert_eq!(diffs[0].changed_lines.len(), 5);
}

#[test]
fn test_parse_rename_diff() {
    let diff = r#"diff --git a/old_name.rs b/new_name.rs
similarity index 95%
rename from old_name.rs
rename to new_name.rs
index abc..def 100644
--- a/old_name.rs
+++ b/new_name.rs
@@ -3 +3 @@
-old content
+new content
"#;

    let diffs = parse_diff_output(diff).unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].status, DiffStatus::Renamed);
    assert_eq!(diffs[0].path, "new_name.rs");
    assert_eq!(diffs[0].old_path.as_deref(), Some("old_name.rs"));
}

#[test]
fn test_parse_empty_diff() {
    let diffs = parse_diff_output("").unwrap();
    assert!(diffs.is_empty());
}

#[test]
fn test_parse_multiple_hunks() {
    let diff = r#"diff --git a/file.rs b/file.rs
index abc..def 100644
--- a/file.rs
+++ b/file.rs
@@ -5 +5 @@
-old line 5
+new line 5
@@ -20,3 +20,5 @@
+added line 1
+added line 2
"#;

    let diffs = parse_diff_output(diff).unwrap();
    assert_eq!(diffs.len(), 1);
    assert!(diffs[0].changed_lines.contains(5));
    assert!(diffs[0].changed_lines.contains(20));
    assert!(diffs[0].changed_lines.contains(24));
}
