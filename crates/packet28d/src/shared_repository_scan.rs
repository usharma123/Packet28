//! One-pass full rebuild composition for the map and regex indexes.
//!
//! The map and regex crates continue to own their filtering, derived data,
//! caches, writer locks, immutable artifacts, and manifests. This module owns
//! only the daemon-level invariant that repository discovery and content reads
//! happen once and that the two prepared generations publish as a pair.
//!
//! This experiment remains behind the non-default `shared-repository-scan`
//! feature until production release benchmarks and parity tests justify making
//! it the default.
//!
//! # Example
//!
//! ```no_run
//! use packet28d::shared_repository_scan::{
//!     rebuild_full_indexes_with_shared_scan, SharedScanProgress,
//! };
//! use std::path::Path;
//!
//! let result = rebuild_full_indexes_with_shared_scan(
//!     Path::new("."),
//!     true,
//!     || false,
//!     |SharedScanProgress { engine, completed, total }| {
//!         eprintln!("{engine:?}: {completed}/{total}");
//!     },
//! )?;
//! assert_eq!(result.telemetry.walk_passes, 1);
//! # Ok::<(), packet28d::shared_repository_scan::SharedScanError>(())
//! ```

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ignore::{DirEntry, Error as WalkError, WalkBuilder};
use mapy_core::shared_scan::RepoIndexScanSession;
use mapy_core::PreparedRepoIndexRuntime;
use packet28_search_core::shared_scan::{PreparedRegexIndexRuntime, RegexIndexScanSession};
use packet28_search_core::SearchError;
use suite_packet_core::CovyError;

/// Identifies the consumer associated with a shared-scan progress checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedScanEngine {
    /// Repository map index.
    Map,
    /// Regex prefilter index.
    Regex,
}

/// One consumer-specific progress checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedScanProgress {
    /// Consumer receiving the checkpoint.
    pub engine: SharedScanEngine,
    /// Number of discovered candidates processed by that consumer.
    pub completed: usize,
    /// Immutable discovery total for that consumer.
    pub total: usize,
}

/// Exact application-level work performed by one shared full scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedScanTelemetry {
    /// Repository walk passes. A successful shared scan always reports one.
    pub walk_passes: u64,
    /// Successful entries yielded by the walker, including directories.
    pub walked_entries: u64,
    /// Walker errors ignored because the standalone map walker would have
    /// pruned their path; the regex scanner historically ignores walk errors.
    pub ignored_walk_errors: u64,
    /// Symlink or unknown-entry type checks that required following the path.
    pub classification_metadata_queries: u64,
    /// Explicit content metadata calls after discovery.
    pub content_metadata_calls: u64,
    /// Successful content reads.
    pub successful_read_calls: u64,
    /// Bytes returned by successful content reads.
    pub bytes_read: u64,
    /// Maximum number of raw content buffers retained at once.
    pub peak_retained_content_files: u64,
    /// Largest raw content buffer retained during the scan.
    pub peak_retained_content_bytes: u64,
}

/// Both validated runtimes produced by one paired publication.
#[derive(Debug)]
pub struct SharedIndexRuntimes {
    /// Published repository map generation.
    pub repo: mapy_core::RepoIndexRuntime,
    /// Published regex index generation.
    pub regex: packet28_search_core::RegexIndexRuntime,
    /// Exact one-pass discovery and content-I/O counters.
    pub telemetry: SharedScanTelemetry,
}

/// Typed failure from shared discovery, ingestion, preparation, or publication.
#[derive(Debug)]
#[non_exhaustive]
pub enum SharedScanError {
    /// The configured repository root is not a directory.
    InvalidRoot {
        /// Rejected root.
        root: PathBuf,
    },
    /// Cancellation was observed at a bounded coordinator checkpoint.
    Cancelled,
    /// The standalone map walk would have surfaced this walker failure.
    Walk {
        /// Repository root being traversed.
        root: PathBuf,
        /// Typed walker failure.
        source: WalkError,
    },
    /// Regex-required metadata could not be loaded.
    Metadata {
        /// Normalized repository-relative path.
        path: String,
        /// Operating-system failure.
        source: io::Error,
    },
    /// Regex-required content could not be read.
    Read {
        /// Normalized repository-relative path.
        path: String,
        /// Operating-system failure.
        source: io::Error,
    },
    /// Map-specific session or generation failure.
    Map {
        /// Typed map failure.
        source: CovyError,
    },
    /// Regex-specific session or generation failure.
    Regex {
        /// Typed regex-index failure.
        source: SearchError,
    },
    /// A second-engine failure was followed by a map-manifest rollback failure.
    PublicationRollback {
        /// Original second-engine failure.
        publication: Box<SharedScanError>,
        /// Failure restoring the exact pre-publication map manifests.
        rollback: CovyError,
    },
}

impl fmt::Display for SharedScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot { root } => {
                write!(formatter, "shared index root is not a directory: {}", root.display())
            }
            Self::Cancelled => formatter.write_str("shared index scan was cancelled"),
            Self::Walk { root, source } => {
                write!(formatter, "failed to walk repository '{}': {source}", root.display())
            }
            Self::Metadata { path, source } => {
                write!(formatter, "failed to read index metadata for '{path}': {source}")
            }
            Self::Read { path, source } => {
                write!(formatter, "failed to read index content for '{path}': {source}")
            }
            Self::Map { source } => write!(formatter, "map index build failed: {source}"),
            Self::Regex { source } => write!(formatter, "regex index build failed: {source}"),
            Self::PublicationRollback {
                publication,
                rollback,
            } => write!(
                formatter,
                "paired index publication failed ({publication}); restoring map manifests also failed ({rollback})"
            ),
        }
    }
}

impl Error for SharedScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Walk { source, .. } => Some(source),
            Self::Metadata { source, .. } | Self::Read { source, .. } => Some(source),
            Self::Map { source } => Some(source),
            Self::Regex { source } => Some(source),
            Self::PublicationRollback { publication, .. } => Some(publication.as_ref()),
            Self::InvalidRoot { .. } | Self::Cancelled => None,
        }
    }
}

impl From<CovyError> for SharedScanError {
    fn from(source: CovyError) -> Self {
        Self::Map { source }
    }
}

impl From<SearchError> for SharedScanError {
    fn from(source: SearchError) -> Self {
        Self::Regex { source }
    }
}

#[derive(Debug)]
struct PlannedPath {
    path: PathBuf,
    relative: String,
    map: bool,
    regex: bool,
}

struct PreparedSharedIndexes {
    repo: PreparedRepoIndexRuntime,
    regex: PreparedRegexIndexRuntime,
    telemetry: SharedScanTelemetry,
}

impl PreparedSharedIndexes {
    fn publish(self) -> Result<SharedIndexRuntimes, SharedScanError> {
        self.publish_with(PreparedRegexIndexRuntime::publish)
    }

    fn publish_with(
        mut self,
        publish_regex: impl FnOnce(&mut PreparedRegexIndexRuntime) -> Result<(), SearchError>,
    ) -> Result<SharedIndexRuntimes, SharedScanError> {
        self.repo.publish()?;
        if let Err(source) = publish_regex(&mut self.regex) {
            let publication = SharedScanError::Regex { source };
            return match self.repo.rollback() {
                Ok(()) => Err(publication),
                Err(rollback) => Err(SharedScanError::PublicationRollback {
                    publication: Box::new(publication),
                    rollback,
                }),
            };
        }
        let repo = self.repo.commit()?;
        let regex = self.regex.commit()?;
        Ok(SharedIndexRuntimes {
            repo,
            regex,
            telemetry: self.telemetry,
        })
    }
}

/// Rebuilds and pair-publishes both full indexes from one repository scan.
///
/// `is_cancelled` is checked during discovery, after every candidate, between
/// preparation phases, and before publication. The progress callback receives
/// independent `0..=total` sequences for map and regex candidates.
///
/// The coordinator retains no repository-sized content collection: each
/// successful `fs::read` buffer is borrowed by interested sessions and dropped
/// before the next candidate.
///
/// # Errors
///
/// Returns [`SharedScanError::Cancelled`] when cancellation is requested,
/// [`SharedScanError::Walk`] for a walker failure the standalone map scan would
/// observe, a typed metadata/read failure required by the regex scan, or the
/// underlying typed map/regex preparation and publication failure.
pub fn rebuild_full_indexes_with_shared_scan<C, P>(
    root: &Path,
    include_tests: bool,
    is_cancelled: C,
    on_progress: P,
) -> Result<SharedIndexRuntimes, SharedScanError>
where
    C: FnMut() -> bool,
    P: FnMut(SharedScanProgress),
{
    rebuild_full_indexes_with_io(
        root,
        include_tests,
        is_cancelled,
        on_progress,
        |path| fs::metadata(path),
        |path| fs::read(path),
    )
}

fn rebuild_full_indexes_with_io<C, P, M, R>(
    root: &Path,
    include_tests: bool,
    mut is_cancelled: C,
    mut on_progress: P,
    mut metadata_for: M,
    mut read_file: R,
) -> Result<SharedIndexRuntimes, SharedScanError>
where
    C: FnMut() -> bool,
    P: FnMut(SharedScanProgress),
    M: FnMut(&Path) -> io::Result<fs::Metadata>,
    R: FnMut(&Path) -> io::Result<Vec<u8>>,
{
    let (plan, mut telemetry) = discover_paths(root, include_tests, &mut is_cancelled)?;
    let repo_paths = plan
        .iter()
        .filter(|entry| entry.map)
        .map(|entry| entry.relative.clone())
        .collect::<Vec<_>>();
    let regex_paths = plan
        .iter()
        .filter(|entry| entry.regex)
        .map(|entry| entry.relative.clone())
        .collect::<Vec<_>>();
    let mut repo = RepoIndexScanSession::begin(root, include_tests, &repo_paths)?;
    let mut regex = RegexIndexScanSession::begin(root, include_tests, &regex_paths)?;
    let repo_total = repo.total_files();
    let regex_total = regex.total_files();
    let mut repo_completed = 0;
    let mut regex_completed = 0;
    on_progress(SharedScanProgress {
        engine: SharedScanEngine::Map,
        completed: 0,
        total: repo_total,
    });
    on_progress(SharedScanProgress {
        engine: SharedScanEngine::Regex,
        completed: 0,
        total: regex_total,
    });
    check_cancelled(&mut is_cancelled)?;

    let mut regex_failure = None;
    for entry in plan {
        let regex_active = entry.regex && regex_failure.is_none();
        if !entry.map && !regex_active {
            continue;
        }
        telemetry.content_metadata_calls = telemetry.content_metadata_calls.saturating_add(1);
        let metadata = match metadata_for(&entry.path) {
            Ok(metadata) => metadata,
            Err(source) => {
                if entry.map {
                    repo_completed += 1;
                    report_progress(
                        &mut on_progress,
                        SharedScanEngine::Map,
                        repo_completed,
                        repo_total,
                    );
                }
                if regex_active {
                    regex_failure = Some(SharedScanError::Metadata {
                        path: entry.relative,
                        source,
                    });
                }
                check_cancelled(&mut is_cancelled)?;
                continue;
            }
        };

        let regex_needs_content =
            regex_active && packet28_search_core::shared_scan::wants_content(&metadata);
        let needs_content = entry.map || regex_needs_content;
        let bytes = if needs_content {
            match read_file(&entry.path) {
                Ok(bytes) => {
                    telemetry.successful_read_calls =
                        telemetry.successful_read_calls.saturating_add(1);
                    telemetry.bytes_read = telemetry
                        .bytes_read
                        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    telemetry.peak_retained_content_files = 1;
                    telemetry.peak_retained_content_bytes = telemetry
                        .peak_retained_content_bytes
                        .max(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    Some(bytes)
                }
                Err(source) => {
                    if entry.map {
                        repo_completed += 1;
                        report_progress(
                            &mut on_progress,
                            SharedScanEngine::Map,
                            repo_completed,
                            repo_total,
                        );
                    }
                    if regex_needs_content {
                        regex_failure = Some(SharedScanError::Read {
                            path: entry.relative,
                            source,
                        });
                    } else if regex_active {
                        match regex.ingest(&entry.relative, &metadata, &[]) {
                            Ok(()) => {
                                regex_completed += 1;
                                report_progress(
                                    &mut on_progress,
                                    SharedScanEngine::Regex,
                                    regex_completed,
                                    regex_total,
                                );
                            }
                            Err(source) => {
                                regex_failure = Some(SharedScanError::Regex { source });
                            }
                        }
                    }
                    check_cancelled(&mut is_cancelled)?;
                    continue;
                }
            }
        } else {
            None
        };

        if entry.map {
            if let Some(bytes) = bytes.as_deref() {
                repo.ingest(&entry.relative, &metadata, bytes)?;
            }
            repo_completed += 1;
            report_progress(
                &mut on_progress,
                SharedScanEngine::Map,
                repo_completed,
                repo_total,
            );
        }
        if regex_active {
            let ingest_result = if regex_needs_content {
                bytes.as_deref().map_or(Ok(()), |bytes| {
                    regex.ingest(&entry.relative, &metadata, bytes)
                })
            } else {
                regex.ingest(&entry.relative, &metadata, &[])
            };
            match ingest_result {
                Ok(()) => {
                    regex_completed += 1;
                    report_progress(
                        &mut on_progress,
                        SharedScanEngine::Regex,
                        regex_completed,
                        regex_total,
                    );
                }
                Err(source) => {
                    regex_failure = Some(SharedScanError::Regex { source });
                }
            }
        }
        check_cancelled(&mut is_cancelled)?;
    }

    let repo = repo.prepare()?;
    check_cancelled(&mut is_cancelled)?;
    if let Some(error) = regex_failure {
        return Err(error);
    }
    let regex = regex.prepare()?;
    check_cancelled(&mut is_cancelled)?;
    PreparedSharedIndexes {
        repo,
        regex,
        telemetry,
    }
    .publish()
}

fn report_progress(
    on_progress: &mut impl FnMut(SharedScanProgress),
    engine: SharedScanEngine,
    completed: usize,
    total: usize,
) {
    on_progress(SharedScanProgress {
        engine,
        completed,
        total,
    });
}

fn check_cancelled(is_cancelled: &mut impl FnMut() -> bool) -> Result<(), SharedScanError> {
    if is_cancelled() {
        Err(SharedScanError::Cancelled)
    } else {
        Ok(())
    }
}

fn discover_paths(
    root: &Path,
    include_tests: bool,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(Vec<PlannedPath>, SharedScanTelemetry), SharedScanError> {
    if !root.is_dir() {
        return Err(SharedScanError::InvalidRoot {
            root: root.to_path_buf(),
        });
    }
    let mut telemetry = SharedScanTelemetry {
        walk_passes: 1,
        ..SharedScanTelemetry::default()
    };
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    let mut plan = Vec::new();
    for result in walker.build() {
        check_cancelled(is_cancelled)?;
        let entry = match result {
            Ok(entry) => entry,
            Err(source) if map_would_observe_walk_error(&source, root) => {
                return Err(SharedScanError::Walk {
                    root: root.to_path_buf(),
                    source,
                });
            }
            Err(_) => {
                telemetry.ignored_walk_errors = telemetry.ignored_walk_errors.saturating_add(1);
                continue;
            }
        };
        telemetry.walked_entries = telemetry.walked_entries.saturating_add(1);
        if let Some(planned) = plan_entry(root, entry, include_tests, &mut telemetry) {
            plan.push(planned);
        }
    }
    plan.sort_by(|left, right| {
        left.relative
            .cmp(&right.relative)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok((plan, telemetry))
}

fn plan_entry(
    root: &Path,
    entry: DirEntry,
    include_tests: bool,
    telemetry: &mut SharedScanTelemetry,
) -> Option<PlannedPath> {
    let relative_path = entry.path().strip_prefix(root).ok()?;
    if relative_path.as_os_str().is_empty() {
        return None;
    }
    let relative = relative_path.to_string_lossy().replace('\\', "/");
    let (is_file, is_dir) = match entry.file_type() {
        Some(file_type) if file_type.is_file() => (true, false),
        Some(file_type) if file_type.is_dir() => (false, true),
        _ => {
            telemetry.classification_metadata_queries =
                telemetry.classification_metadata_queries.saturating_add(2);
            (entry.path().is_file(), entry.path().is_dir())
        }
    };
    let map = is_file && map_wants_path(relative_path, &relative, include_tests);
    let regex = !is_dir
        && relative_path.to_str().is_some()
        && packet28_search_core::shared_scan::wants_path(&relative);
    (map || regex).then(|| PlannedPath {
        path: entry.into_path(),
        relative,
        map,
        regex,
    })
}

fn map_wants_path(relative_path: &Path, normalized: &str, include_tests: bool) -> bool {
    relative_path.to_str().is_some()
        && mapy_core::shared_scan::wants_path(normalized, include_tests)
}

fn map_would_observe_walk_error(error: &WalkError, root: &Path) -> bool {
    match error {
        WalkError::Partial(errors) => errors
            .iter()
            .any(|error| map_would_observe_walk_error(error, root)),
        WalkError::WithLineNumber { err, .. } | WalkError::WithDepth { err, .. } => {
            map_would_observe_walk_error(err, root)
        }
        WalkError::WithPath { path, err } => {
            path_is_map_traversable(path, root) && map_would_observe_walk_error(err, root)
        }
        WalkError::Loop { child, .. } => path_is_map_traversable(child, root),
        WalkError::Io(_)
        | WalkError::Glob { .. }
        | WalkError::UnrecognizedFileType(_)
        | WalkError::InvalidDefinition => true,
    }
}

fn path_is_map_traversable(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    if relative.as_os_str().is_empty() {
        return true;
    }
    mapy_core::shared_scan::wants_traversal(&relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use packet28_reducer_core::SearchRequest;

    #[cfg(unix)]
    #[test]
    fn map_eligibility_rejects_lossy_non_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let mut invalid_name = b"collision_".to_vec();
        invalid_name.push(0xff);
        invalid_name.extend_from_slice(b".rs");
        let invalid = Path::new("src").join(OsString::from_vec(invalid_name));
        let invalid_normalized = invalid.to_string_lossy().replace('\\', "/");

        assert!(!map_wants_path(&invalid, &invalid_normalized, true));
        assert!(map_wants_path(
            Path::new("src/collision_\u{fffd}.rs"),
            "src/collision_\u{fffd}.rs",
            true
        ));
    }

    fn write_fixture(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            b"pub fn shared_visible_symbol() -> usize { 7 }\n",
        )
        .unwrap();
        fs::write(root.join("tests/case.rs"), b"#[test] fn shared_case() {}\n").unwrap();
        fs::write(
            root.join("build/generated.rs"),
            b"pub fn regex_only_generated_symbol() {}\n",
        )
        .unwrap();
        fs::write(root.join("docs/guide.md"), b"shared documentation needle\n").unwrap();
        fs::write(root.join(".hidden.rs"), b"pub struct HiddenShared;\n").unwrap();
        fs::write(root.join("src/empty.rs"), b"").unwrap();
        fs::write(root.join("src/nul.rs"), b"pub fn before() {}\0after\n").unwrap();
        fs::write(root.join("src/invalid.rs"), [0xff, 0xfe, b'a']).unwrap();
        fs::write(root.join(".gitignore"), b"ignored.rs\n.packet28/\n").unwrap();
        fs::write(root.join("ignored.rs"), b"pub fn ignored_symbol() {}\n").unwrap();
        fs::write(
            root.join("src/regex_oversize.rs"),
            vec![b'x'; packet28_search_core::shared_scan::MAX_SHARED_SCAN_CONTENT_BYTES + 1],
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt as _;

            let mut invalid_name = b"collision_".to_vec();
            invalid_name.push(0xff);
            invalid_name.extend_from_slice(b".rs");
            fs::write(
                root.join("src").join(OsString::from_vec(invalid_name)),
                b"pub fn non_utf8_filename_symbol() {}\n",
            )
            .unwrap();
            fs::write(
                root.join("src/collision_\u{fffd}.rs"),
                b"pub fn utf8_replacement_filename_symbol() {}\n",
            )
            .unwrap();
        }
        set_fixture_mtimes(root);
        fixture_git(root, &["init", "--quiet"]);
        fixture_git(root, &["config", "user.name", "Packet28 Test"]);
        fixture_git(root, &["config", "user.email", "packet28@example.invalid"]);
        fixture_git(root, &["add", "."]);
        fixture_git(
            root,
            &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
        );
    }

    const FIXTURE_MTIME_SECONDS: u64 = 1_700_000_000;

    fn set_fixture_mtimes(root: &Path) {
        set_fixture_mtimes_with(root, root, &mut |_| {});
    }

    fn set_fixture_mtimes_with(root: &Path, path: &Path, before_open: &mut dyn FnMut(&Path)) {
        let modified = UNIX_EPOCH + Duration::from_secs(FIXTURE_MTIME_SECONDS);
        let entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read fixture directory {}: {error}", path.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| {
                panic!("read fixture entry in {}: {error}", path.display())
            });
            let entry_path = entry.path();
            // Git metadata is irrelevant to source parity, and Git may prune
            // control files at any time, so only fixture content is touched.
            if path == root && entry.file_name() == ".git" {
                continue;
            }
            let file_type = entry.file_type().unwrap_or_else(|error| {
                panic!("read fixture entry type {}: {error}", entry_path.display())
            });
            if file_type.is_dir() {
                set_fixture_mtimes_with(root, &entry_path, before_open);
            } else if file_type.is_file() {
                before_open(&entry_path);
                fs::File::open(&entry_path)
                    .unwrap_or_else(|error| {
                        panic!("open fixture file {}: {error}", entry_path.display())
                    })
                    .set_modified(modified)
                    .unwrap_or_else(|error| {
                        panic!("set fixture file mtime {}: {error}", entry_path.display())
                    });
            }
        }
    }

    fn fixture_mtime(path: &Path) -> std::time::SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn fixture_mtimes_skip_git_metadata_even_when_git_removes_control_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_fixture(root);
        let git = root.join(".git");
        let control = git.join("packet28-transient-control");
        fs::write(&control, b"transient\n").unwrap();
        let head_mtime = fixture_mtime(&git.join("HEAD"));
        let modified = UNIX_EPOCH + Duration::from_secs(FIXTURE_MTIME_SECONDS);
        let source = root.join("src/lib.rs");
        fs::write(&source, b"pub fn shared_visible_symbol() -> usize { 9 }\n").unwrap();
        assert_ne!(fixture_mtime(&source), modified);

        let mut visited = Vec::new();
        set_fixture_mtimes_with(root, root, &mut |path| {
            // Model Git pruning a control file after the walk enumerated it
            // and before it is opened.
            if path == control {
                fs::remove_file(path).unwrap();
            }
            visited.push(path.to_path_buf());
        });

        assert!(control.is_file(), "Git control file was touched");
        assert!(
            visited.iter().all(|path| !path.starts_with(&git)),
            "fixture mtime walk visited Git metadata: {visited:?}"
        );
        assert!(visited.contains(&source));
        assert_eq!(fixture_mtime(&source), modified);
        assert_eq!(fixture_mtime(&git.join("HEAD")), head_mtime);
    }

    #[test]
    fn fixture_mtimes_fail_with_the_path_of_a_vanished_source_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_fixture(root);
        let vanished = root.join("docs/guide.md");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set_fixture_mtimes_with(root, root, &mut |path| {
                if path == vanished {
                    fs::remove_file(path).unwrap();
                }
            });
        }))
        .expect_err("a vanished fixture source file must fail the walk");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .expect("formatted panic message");

        assert!(
            message.starts_with(&format!("open fixture file {}:", vanished.display())),
            "{message}"
        );
    }

    fn fixture_git(root: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .env("GIT_AUTHOR_DATE", "2023-11-14T22:13:20Z")
            .env("GIT_COMMITTER_DATE", "2023-11-14T22:13:20Z")
            .output()
            .expect("run fixture git");
        let rendered_arguments = arguments.join(" ");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "git {rendered_arguments} failed: {stderr}"
        );
    }

    fn manifest_bytes(root: &Path, engine: &str) -> BTreeMap<String, Vec<u8>> {
        ["manifest.json", "manifest.previous.json"]
            .into_iter()
            .map(|name| {
                let path = root.join(".packet28/index").join(engine).join(name);
                (name.to_string(), fs::read(path).unwrap())
            })
            .collect()
    }

    #[test]
    fn shared_scan_matches_production_snapshots_queries_and_progress() {
        let separate_dir = tempfile::tempdir().unwrap();
        let shared_dir = tempfile::tempdir().unwrap();
        write_fixture(separate_dir.path());
        write_fixture(shared_dir.path());

        let expected_repo =
            mapy_core::rebuild_repo_index_runtime(separate_dir.path(), true).unwrap();
        let expected_regex =
            packet28_search_core::rebuild_full_index(separate_dir.path(), true).unwrap();
        let mut progress = Vec::new();
        let shared = rebuild_full_indexes_with_shared_scan(
            shared_dir.path(),
            true,
            || false,
            |event| progress.push(event),
        )
        .unwrap();

        assert_eq!(
            shared.repo.materialize_snapshot(),
            expected_repo.materialize_snapshot()
        );
        assert_eq!(
            shared.regex.shared_scan_content_digests(),
            expected_regex.shared_scan_content_digests()
        );
        #[cfg(target_os = "linux")]
        {
            let snapshot = shared.repo.materialize_snapshot().unwrap();
            let collision_file = snapshot
                .files
                .get("src/collision_\u{fffd}.rs")
                .expect("UTF-8 replacement-character path should remain indexed");
            assert!(collision_file
                .symbols
                .iter()
                .any(|symbol| symbol.name == "utf8_replacement_filename_symbol"));
            assert!(collision_file
                .symbols
                .iter()
                .all(|symbol| symbol.name != "non_utf8_filename_symbol"));
            let regex_paths = shared.regex.shared_scan_document_paths().unwrap();
            assert_eq!(
                regex_paths
                    .iter()
                    .filter(|path| path.as_str() == "src/collision_\u{fffd}.rs")
                    .count(),
                1,
                "regex indexing must reject a non-UTF-8 path that aliases a UTF-8 path"
            );
        }
        assert_eq!(shared.telemetry.walk_passes, 1);
        assert!(shared.telemetry.successful_read_calls > 0);
        assert_eq!(shared.telemetry.peak_retained_content_files, 1);

        for engine in [SharedScanEngine::Map, SharedScanEngine::Regex] {
            let checkpoints = progress
                .iter()
                .filter(|event| event.engine == engine)
                .copied()
                .collect::<Vec<_>>();
            let total = checkpoints.first().unwrap().total;
            assert_eq!(
                checkpoints
                    .iter()
                    .map(|event| event.completed)
                    .collect::<Vec<_>>(),
                (0..=total).collect::<Vec<_>>()
            );
            assert!(checkpoints.iter().all(|event| event.total == total));
        }

        for query in [
            "shared_visible_symbol",
            "regex_only_generated_symbol",
            "shared documentation needle",
            "HiddenShared",
            "utf8_replacement_filename_symbol",
            "non_utf8_filename_symbol",
        ] {
            let request = SearchRequest {
                query: query.to_string(),
                fixed_string: true,
                ..SearchRequest::default()
            };
            let expected = packet28_search_core::indexed_search(
                separate_dir.path(),
                &expected_regex,
                &request,
            )
            .unwrap();
            let actual =
                packet28_search_core::indexed_search(shared_dir.path(), &shared.regex, &request)
                    .unwrap();
            assert_eq!(actual, expected, "query parity failed for {query}");
        }

        let ignored = packet28_search_core::indexed_search(
            shared_dir.path(),
            &shared.regex,
            &SearchRequest {
                query: "ignored_symbol".to_string(),
                fixed_string: true,
                ..SearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(
            ignored.returned_match_count, 0,
            "an active .gitignore must exclude ignored.rs"
        );
        #[cfg(target_os = "linux")]
        {
            let replacement = packet28_search_core::indexed_search(
                shared_dir.path(),
                &shared.regex,
                &SearchRequest {
                    query: "utf8_replacement_filename_symbol".to_string(),
                    fixed_string: true,
                    ..SearchRequest::default()
                },
            )
            .unwrap();
            assert!(replacement.returned_match_count > 0);
            let non_utf8 = packet28_search_core::indexed_search(
                shared_dir.path(),
                &shared.regex,
                &SearchRequest {
                    query: "non_utf8_filename_symbol".to_string(),
                    fixed_string: true,
                    ..SearchRequest::default()
                },
            )
            .unwrap();
            assert_eq!(
                non_utf8.returned_match_count, 0,
                "lossy verification must retain standalone regex result behavior"
            );
        }
    }

    #[test]
    fn include_tests_false_preserves_each_engine_policy() {
        let separate_dir = tempfile::tempdir().unwrap();
        let shared_dir = tempfile::tempdir().unwrap();
        write_fixture(separate_dir.path());
        write_fixture(shared_dir.path());
        let expected_repo =
            mapy_core::rebuild_repo_index_runtime(separate_dir.path(), false).unwrap();
        let expected_regex =
            packet28_search_core::rebuild_full_index(separate_dir.path(), false).unwrap();

        let shared =
            rebuild_full_indexes_with_shared_scan(shared_dir.path(), false, || false, |_| {})
                .unwrap();

        assert_eq!(
            shared.repo.materialize_snapshot(),
            expected_repo.materialize_snapshot()
        );
        assert!(shared.repo.file("tests/case.rs").is_none());
        assert_eq!(
            shared.regex.shared_scan_content_digests(),
            expected_regex.shared_scan_content_digests()
        );
        let request = SearchRequest {
            query: "shared_case".to_string(),
            fixed_string: true,
            ..SearchRequest::default()
        };
        assert_eq!(
            packet28_search_core::indexed_search(shared_dir.path(), &shared.regex, &request)
                .unwrap(),
            packet28_search_core::indexed_search(separate_dir.path(), &expected_regex, &request)
                .unwrap()
        );
    }

    fn dirty_fixture(root: &Path) {
        write_fixture(root);
        fs::write(
            root.join("src/lib.rs"),
            b"pub fn shared_dirty_tracked_symbol() -> usize { 8 }\n",
        )
        .unwrap();
        fs::write(
            root.join("docs/setup.md"),
            b"shared dirty untracked needle\n",
        )
        .unwrap();
        fs::remove_file(root.join("docs/guide.md")).unwrap();
        set_fixture_mtimes(root);
    }

    #[test]
    fn shared_scan_attests_a_stable_dirty_workspace_like_the_separate_rebuild() {
        let separate_dir = tempfile::tempdir().unwrap();
        let shared_dir = tempfile::tempdir().unwrap();
        dirty_fixture(separate_dir.path());
        dirty_fixture(shared_dir.path());

        let expected = packet28_search_core::rebuild_full_index(separate_dir.path(), true).unwrap();
        let shared =
            rebuild_full_indexes_with_shared_scan(shared_dir.path(), true, || false, |_| {})
                .unwrap();

        assert!(expected.manifest.workspace_attested_commit.is_some());
        assert_eq!(expected.manifest.workspace_clean_commit, None);
        for (actual, expected) in [
            (
                &shared.regex.manifest.base_commit,
                &expected.manifest.base_commit,
            ),
            (
                &shared.regex.manifest.workspace_clean_commit,
                &expected.manifest.workspace_clean_commit,
            ),
            (
                &shared.regex.manifest.workspace_attested_commit,
                &expected.manifest.workspace_attested_commit,
            ),
            (
                &shared.regex.manifest.overlay_state_digest,
                &expected.manifest.overlay_state_digest,
            ),
        ] {
            assert_eq!(actual, expected);
        }
        assert_eq!(
            shared.regex.shared_scan_content_digests(),
            expected.shared_scan_content_digests()
        );
        let reloaded = packet28_search_core::load_runtime(shared_dir.path()).unwrap();
        assert!(reloaded.is_loaded(), "{:?}", reloaded.manifest.stale_reason);
        for (query, matches) in [
            ("shared_dirty_tracked_symbol", 1),
            ("shared dirty untracked needle", 1),
            ("shared documentation needle", 0),
            ("shared_visible_symbol", 0),
        ] {
            let request = SearchRequest {
                query: query.to_string(),
                fixed_string: true,
                ..SearchRequest::default()
            };
            let actual =
                packet28_search_core::indexed_search(shared_dir.path(), &reloaded, &request)
                    .unwrap();
            let separate =
                packet28_search_core::indexed_search(separate_dir.path(), &expected, &request)
                    .unwrap();
            assert_eq!(actual, separate, "query parity failed for {query}");
            assert_eq!(actual.match_count, matches, "{query}");
            if matches > 0 {
                packet28_search_core::guarded_indexed_search(
                    shared_dir.path(),
                    &reloaded,
                    &request,
                )
                .unwrap();
            }
        }

        fs::write(shared_dir.path().join("docs/setup.md"), b"unreported\n").unwrap();
        assert!(!packet28_search_core::load_runtime(shared_dir.path())
            .unwrap()
            .is_loaded());
    }

    #[test]
    fn shared_scan_rejects_a_dirty_workspace_change_during_the_scan() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        dirty_fixture(root);
        let ready = packet28_search_core::rebuild_full_index(root, true).unwrap();
        let manifest = root.join(".packet28/index/regex-v1/manifest.json");
        let expected_regex = fs::read(&manifest).unwrap();
        let paths = vec!["src/lib.rs".to_string()];
        let mut session = RegexIndexScanSession::begin(root, true, &paths).unwrap();
        let changed = b"pub fn shared_dirty_changed_mid_scan() {}\n";
        fs::write(root.join("src/lib.rs"), changed).unwrap();
        let metadata = fs::metadata(root.join("src/lib.rs")).unwrap();
        session.ingest(&paths[0], &metadata, changed).unwrap();

        let error = session
            .prepare()
            .err()
            .expect("dirty workspace changed during the shared scan");

        assert!(matches!(error, SearchError::IndexNotReady { .. }));
        assert_eq!(fs::read(&manifest).unwrap(), expected_regex);
        assert!(ready.manifest.workspace_attested_commit.is_some());
    }

    #[test]
    fn second_engine_rejection_restores_both_map_manifests_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_fixture(root);
        mapy_core::rebuild_repo_index_runtime(root, true).unwrap();
        packet28_search_core::rebuild_full_index(root, true).unwrap();
        mapy_core::rebuild_repo_index_runtime(root, true).unwrap();
        packet28_search_core::rebuild_full_index(root, true).unwrap();
        let expected_map = manifest_bytes(root, "mapy-v1");
        let expected_regex = manifest_bytes(root, "regex-v1");

        let (plan, telemetry) = discover_paths(root, true, &mut || false).unwrap();
        let map_paths = plan
            .iter()
            .filter(|entry| entry.map)
            .map(|entry| entry.relative.clone())
            .collect::<Vec<_>>();
        let regex_paths = plan
            .iter()
            .filter(|entry| entry.regex)
            .map(|entry| entry.relative.clone())
            .collect::<Vec<_>>();
        let mut map = RepoIndexScanSession::begin(root, true, &map_paths).unwrap();
        let mut regex = RegexIndexScanSession::begin(root, true, &regex_paths).unwrap();
        for entry in plan {
            let metadata = fs::metadata(&entry.path).unwrap();
            let needs_regex =
                entry.regex && packet28_search_core::shared_scan::wants_content(&metadata);
            let bytes = (entry.map || needs_regex).then(|| fs::read(&entry.path).unwrap());
            if entry.map {
                map.ingest(&entry.relative, &metadata, bytes.as_deref().unwrap())
                    .unwrap();
            }
            if entry.regex {
                regex
                    .ingest(
                        &entry.relative,
                        &metadata,
                        bytes.as_deref().unwrap_or_default(),
                    )
                    .unwrap();
            }
        }
        let prepared = PreparedSharedIndexes {
            repo: map.prepare().unwrap(),
            regex: regex.prepare().unwrap(),
            telemetry,
        };
        let error = prepared
            .publish_with(|_| {
                Err(SearchError::InvalidChangedPath {
                    path: "injected second-engine rejection".to_string(),
                })
            })
            .unwrap_err();

        assert!(matches!(error, SharedScanError::Regex { .. }));
        assert_eq!(manifest_bytes(root, "mapy-v1"), expected_map);
        assert_eq!(manifest_bytes(root, "regex-v1"), expected_regex);
    }

    #[test]
    fn walk_error_policy_preserves_map_pruning_boundary() {
        let root = Path::new("/repo");
        let ignored = WalkError::WithPath {
            path: root.join("target/debug/object"),
            err: Box::new(WalkError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected",
            ))),
        };
        let observed = WalkError::WithPath {
            path: root.join("src/private"),
            err: Box::new(WalkError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected",
            ))),
        };
        let top_level_target = WalkError::WithPath {
            path: root.join("target"),
            err: Box::new(WalkError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected",
            ))),
        };

        assert!(!map_would_observe_walk_error(&ignored, root));
        assert!(map_would_observe_walk_error(&observed, root));
        assert!(map_would_observe_walk_error(&top_level_target, root));
    }

    #[test]
    fn cancellation_before_publication_leaves_existing_manifests_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        write_fixture(directory.path());
        mapy_core::rebuild_repo_index_runtime(directory.path(), true).unwrap();
        packet28_search_core::rebuild_full_index(directory.path(), true).unwrap();
        let expected_map = fs::read(
            directory
                .path()
                .join(".packet28/index/mapy-v1/manifest.json"),
        )
        .unwrap();
        let expected_regex = fs::read(
            directory
                .path()
                .join(".packet28/index/regex-v1/manifest.json"),
        )
        .unwrap();
        let mut checks = 0usize;

        let error = rebuild_full_indexes_with_shared_scan(
            directory.path(),
            true,
            || {
                checks += 1;
                checks > 4
            },
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, SharedScanError::Cancelled));
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(".packet28/index/mapy-v1/manifest.json")
            )
            .unwrap(),
            expected_map
        );
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(".packet28/index/regex-v1/manifest.json")
            )
            .unwrap(),
            expected_regex
        );
    }

    #[test]
    fn map_read_failure_for_regex_oversize_file_is_skipped_once() {
        let directory = tempfile::tempdir().unwrap();
        write_fixture(directory.path());
        let mut progress = Vec::new();

        let result = rebuild_full_indexes_with_io(
            directory.path(),
            true,
            || false,
            |event| progress.push(event),
            |path| fs::metadata(path),
            |path| {
                if path.ends_with("regex_oversize.rs") {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected map-only read failure",
                    ))
                } else {
                    fs::read(path)
                }
            },
        )
        .unwrap();

        assert!(result.repo.file("src/regex_oversize.rs").is_none());
        for engine in [SharedScanEngine::Map, SharedScanEngine::Regex] {
            let checkpoints = progress
                .iter()
                .filter(|event| event.engine == engine)
                .copied()
                .collect::<Vec<_>>();
            let total = checkpoints.first().unwrap().total;
            assert_eq!(
                checkpoints
                    .iter()
                    .map(|event| event.completed)
                    .collect::<Vec<_>>(),
                (0..=total).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn regex_required_metadata_failure_aborts_without_publication() {
        let directory = tempfile::tempdir().unwrap();
        write_fixture(directory.path());
        mapy_core::rebuild_repo_index_runtime(directory.path(), true).unwrap();
        packet28_search_core::rebuild_full_index(directory.path(), true).unwrap();
        let expected_map = fs::read(
            directory
                .path()
                .join(".packet28/index/mapy-v1/manifest.json"),
        )
        .unwrap();
        let expected_regex = fs::read(
            directory
                .path()
                .join(".packet28/index/regex-v1/manifest.json"),
        )
        .unwrap();
        let mut progress = Vec::new();

        let error = rebuild_full_indexes_with_io(
            directory.path(),
            true,
            || false,
            |event| progress.push(event),
            |path| {
                if path.ends_with("guide.md") {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected regex metadata failure",
                    ))
                } else {
                    fs::metadata(path)
                }
            },
            |path| fs::read(path),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SharedScanError::Metadata { ref path, .. } if path == "docs/guide.md"
        ));
        let map_checkpoints = progress
            .iter()
            .filter(|event| event.engine == SharedScanEngine::Map)
            .copied()
            .collect::<Vec<_>>();
        let map_total = map_checkpoints.first().unwrap().total;
        assert_eq!(
            map_checkpoints
                .iter()
                .map(|event| event.completed)
                .collect::<Vec<_>>(),
            (0..=map_total).collect::<Vec<_>>()
        );
        let regex_checkpoints = progress
            .iter()
            .filter(|event| event.engine == SharedScanEngine::Regex)
            .copied()
            .collect::<Vec<_>>();
        assert!(
            regex_checkpoints.last().unwrap().completed < regex_checkpoints.last().unwrap().total
        );
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(".packet28/index/mapy-v1/manifest.json")
            )
            .unwrap(),
            expected_map
        );
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(".packet28/index/regex-v1/manifest.json")
            )
            .unwrap(),
            expected_regex
        );
    }
}
