//! Bounded Git workspace attestation for full builds, incremental updates, and queries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path};

use crate::error::{Result, SearchError};
use crate::git_process::run_git;
use crate::model::{OverlayState, RegexIndexManifest, MAX_INDEXED_FILE_BYTES};
use crate::paths::{inspect_workspace_path, WorkspacePathInspection, WorkspacePathKind};

const MAX_ATTESTED_WORKSPACE_ENTRIES: usize = 4_096;
const MAX_ATTESTED_WORKSPACE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BUILD_ATTESTATION_ENTRIES: usize = 100_000;
const MAX_BUILD_ATTESTATION_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
struct AttestationLimits {
    entries: usize,
    bytes: u64,
    allow_oversized: bool,
}

const QUERY_ATTESTATION_LIMITS: AttestationLimits = AttestationLimits {
    entries: MAX_ATTESTED_WORKSPACE_ENTRIES,
    bytes: MAX_ATTESTED_WORKSPACE_BYTES,
    allow_oversized: false,
};
const BUILD_ATTESTATION_LIMITS: AttestationLimits = AttestationLimits {
    entries: MAX_BUILD_ATTESTATION_ENTRIES,
    bytes: MAX_BUILD_ATTESTATION_BYTES,
    allow_oversized: true,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePathAttestation {
    digest: String,
    indexable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitWorkspaceSnapshot {
    pub(crate) commit: String,
    clean: bool,
    pub(crate) entries: BTreeMap<String, String>,
    reported: BTreeMap<String, WorkspacePathAttestation>,
}

impl GitWorkspaceSnapshot {
    pub(crate) fn ensure_indexed_bytes(
        &self,
        indexed_fingerprints: &BTreeMap<String, String>,
    ) -> Result<()> {
        let matches = self.reported.iter().all(|(path, attestation)| {
            if attestation.indexable {
                indexed_fingerprints.get(path) == Some(&attestation.digest)
            } else {
                !indexed_fingerprints.contains_key(path)
            }
        });
        if matches
            && indexed_fingerprints.len() == self.reported.values().filter(|v| v.indexable).count()
        {
            return Ok(());
        }
        Err(SearchError::IndexNotReady {
            reason: "regex index build did not index the authenticated workspace bytes".to_string(),
        })
    }
}

pub(crate) fn git_workspace_snapshot(
    root: &Path,
    reported_paths: &[String],
) -> std::result::Result<GitWorkspaceSnapshot, String> {
    git_workspace_snapshot_with_limits(root, reported_paths, QUERY_ATTESTATION_LIMITS)
}

fn git_workspace_snapshot_with_limits(
    root: &Path,
    reported_paths: &[String],
    limits: AttestationLimits,
) -> std::result::Result<GitWorkspaceSnapshot, String> {
    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let mut records = status.stdout.split(|byte| *byte == 0).peekable();
    let commit = records
        .clone()
        .find_map(|record| record.strip_prefix(b"# branch.oid "))
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| *value != "(initial)")
        .map(str::to_string)
        .ok_or_else(|| "git status did not report a HEAD commit".to_string())?;
    let mut dirty_paths = BTreeSet::new();
    while let Some(record) = records.next() {
        let path = match record.first().copied() {
            None | Some(b'#') | Some(b'!') => continue,
            Some(b'1') => status_record_path(record, 9)?,
            Some(b'2') => {
                dirty_paths.insert(status_record_path(record, 10)?.to_string());
                let original = records
                    .next()
                    .ok_or_else(|| "git rename status omitted its original path".to_string())?;
                std::str::from_utf8(original)
                    .map_err(|_| "git status reported a non-UTF-8 path".to_string())?
            }
            Some(b'u') => status_record_path(record, 11)?,
            Some(b'?') if record.get(1) == Some(&b' ') => std::str::from_utf8(&record[2..])
                .map_err(|_| "git status reported a non-UTF-8 path".to_string())?,
            _ => return Err("git status reported an unsupported record".to_string()),
        };
        dirty_paths.insert(path.to_string());
    }
    let reported = reported_paths.iter().cloned().collect::<BTreeSet<_>>();
    if dirty_paths.union(&reported).count() > limits.entries {
        return Err(format!(
            "workspace attestation exceeds the {}-path safety limit",
            limits.entries
        ));
    }
    let tracked_reported = validate_index_flags(root, &reported)?;
    let mut budget = 0_u64;
    let mut states = BTreeMap::new();
    for path in dirty_paths.union(&reported) {
        validate_workspace_path(path)?;
        if !dirty_paths.contains(path) && !tracked_reported.contains(path) {
            return Err(format!(
                "reported workspace path '{path}' is neither Git-dirty nor tracked"
            ));
        }
        states.insert(
            path.clone(),
            attest_workspace_path(
                root,
                path,
                &mut budget,
                limits.bytes,
                limits.allow_oversized,
            )?,
        );
    }
    let entries = dirty_paths
        .iter()
        .map(|path| (path.clone(), states[path].digest.clone()))
        .collect();
    let reported = reported
        .into_iter()
        .map(|path| (path.clone(), states[&path].clone()))
        .collect();
    Ok(GitWorkspaceSnapshot {
        commit,
        clean: dirty_paths.is_empty(),
        entries,
        reported,
    })
}

fn validate_index_flags(
    root: &Path,
    reported: &BTreeSet<String>,
) -> std::result::Result<BTreeSet<String>, String> {
    let output = run_git(root, &["ls-files", "-v", "-z"])?;
    let mut tracked_reported = BTreeSet::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|r| !r.is_empty())
    {
        let Some((&tag, path)) = record.split_first() else {
            continue;
        };
        let Some(path) = path.strip_prefix(b" ") else {
            return Err("git ls-files reported a malformed record".to_string());
        };
        if tag == b'S' || tag.is_ascii_lowercase() {
            return Err(format!(
                "Git index flag on '{}' prevents workspace authentication",
                String::from_utf8_lossy(path)
            ));
        }
        if let Ok(path) = std::str::from_utf8(path) {
            if reported.contains(path) {
                tracked_reported.insert(path.to_string());
            }
        }
    }
    Ok(tracked_reported)
}

fn status_record_path(record: &[u8], fields: usize) -> std::result::Result<&str, String> {
    let path = record
        .splitn(fields, |byte| *byte == b' ')
        .nth(fields - 1)
        .ok_or_else(|| "git status record omitted its path".to_string())?;
    std::str::from_utf8(path).map_err(|_| "git status reported a non-UTF-8 path".to_string())
}

fn validate_workspace_path(path: &str) -> std::result::Result<(), String> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("git status reported unsafe path '{path}'"));
    }
    Ok(())
}

fn attest_workspace_path(
    root: &Path,
    path: &str,
    budget: &mut u64,
    max_budget: u64,
    allow_oversized: bool,
) -> std::result::Result<WorkspacePathAttestation, String> {
    let inspection = inspect_workspace_path(root, Path::new(path))
        .map_err(|error| format!("failed to inspect workspace path '{path}': {error}"))?;
    let WorkspacePathInspection { full_path, kind } = inspection;
    let metadata = match kind {
        WorkspacePathKind::Symlink => {
            return Err(format!(
                "workspace symlink '{path}' cannot be authenticated for indexed search"
            ))
        }
        WorkspacePathKind::File(metadata) => metadata,
        WorkspacePathKind::Missing => {
            return Ok(WorkspacePathAttestation {
                digest: "missing".to_string(),
                indexable: false,
            })
        }
        WorkspacePathKind::Directory | WorkspacePathKind::Other => {
            return Err(format!("workspace path '{path}' is not a regular file"))
        }
    };
    if metadata.len() > MAX_INDEXED_FILE_BYTES as u64 {
        if !allow_oversized {
            return Err(format!(
                "workspace file '{path}' exceeds the {MAX_INDEXED_FILE_BYTES}-byte attestation limit"
            ));
        }
        return Ok(WorkspacePathAttestation {
            digest: format!("oversized:{}", metadata.len()),
            indexable: false,
        });
    }
    let mut file = File::open(&full_path)
        .map_err(|error| format!("failed to read workspace file '{path}': {error}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes_read = 0_u64;
    let mut contains_zero = false;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read workspace file '{path}': {error}"))?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        *budget = budget.saturating_add(read as u64);
        if bytes_read > MAX_INDEXED_FILE_BYTES as u64 || *budget > max_budget {
            return Err("workspace content exceeds the bounded attestation byte limit".to_string());
        }
        contains_zero |= buffer[..read].contains(&0);
        hasher.update(&buffer[..read]);
    }
    Ok(WorkspacePathAttestation {
        digest: hasher.finalize().to_hex().to_string(),
        indexable: bytes_read > 0 && !contains_zero,
    })
}

/// Captures the HEAD commit and every Git-dirty path digest before a full build.
///
/// A dirty tree is accepted here; [`authenticate_full_build_workspace`]
/// requires the same commit and dirty digests once scanning has finished.
pub(crate) fn begin_full_build_workspace(root: &Path) -> Result<Option<GitWorkspaceSnapshot>> {
    if !git_metadata_present(root).map_err(index_not_ready)? {
        return Ok(None);
    }
    git_workspace_snapshot(root, &[])
        .map(Some)
        .map_err(|reason| SearchError::IndexNotReady { reason })
}

/// Authenticates the scanned bytes and records the full-build attestation.
///
/// HEAD and every dirty-path digest must match the pre-build snapshot, and
/// every discovered path must have been indexed from its final bytes. A clean
/// tree keeps the legacy `workspace_clean_commit` label; a dirty tree records
/// `workspace_attested_commit` and returns its digests as overlay state.
pub(crate) fn authenticate_full_build_workspace(
    root: &Path,
    before: Option<&GitWorkspaceSnapshot>,
    reported_paths: &[String],
    indexed_fingerprints: &BTreeMap<String, String>,
    manifest: &mut RegexIndexManifest,
) -> Result<OverlayState> {
    let Some(before) = before else {
        if git_metadata_present(root).map_err(index_not_ready)? {
            return Err(SearchError::IndexNotReady {
                reason: "a Git repository appeared while rebuilding the regex index".to_string(),
            });
        }
        return Ok(OverlayState::default());
    };
    let after = git_workspace_snapshot_with_limits(root, reported_paths, BUILD_ATTESTATION_LIMITS)
        .map_err(|reason| SearchError::IndexNotReady { reason })?;
    if before.commit != after.commit || before.entries != after.entries {
        return Err(SearchError::IndexNotReady {
            reason:
                "Git workspace changed while rebuilding the full regex index; retry once HEAD and the working tree are stable"
                    .to_string(),
        });
    }
    after.ensure_indexed_bytes(indexed_fingerprints)?;
    manifest.base_commit = Some(after.commit.clone());
    let mut overlay_state = OverlayState::default();
    if after.clean {
        manifest.workspace_clean_commit = Some(after.commit);
    } else {
        manifest.workspace_attested_commit = Some(after.commit);
        overlay_state.workspace_entries = after.entries;
    }
    Ok(overlay_state)
}

fn git_metadata_present(root: &Path) -> std::result::Result<bool, String> {
    let git_path = root.join(".git");
    match fs::symlink_metadata(&git_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err("repository Git metadata cannot be a symlink".to_string())
        }
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(metadata) if metadata.is_dir() => match fs::symlink_metadata(git_path.join("HEAD")) {
            Ok(head) if head.is_file() => Ok(true),
            Ok(_) => Err("repository Git HEAD is not a regular file".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let empty = fs::read_dir(&git_path)
                    .map_err(|error| format!("failed to inspect repository Git metadata: {error}"))?
                    .next()
                    .is_none();
                if empty {
                    Ok(false)
                } else {
                    Err("repository Git metadata is missing HEAD".to_string())
                }
            }
            Err(error) => Err(format!("failed to inspect repository Git HEAD: {error}")),
        },
        Ok(_) => Err("repository Git metadata is not a file or directory".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "failed to inspect repository Git metadata: {error}"
        )),
    }
}

fn index_not_ready(reason: String) -> SearchError {
    SearchError::IndexNotReady { reason }
}

pub(crate) fn workspace_freshness_reason(
    root: &Path,
    manifest: &RegexIndexManifest,
    expected_entries: &BTreeMap<String, String>,
) -> Option<String> {
    let expected_base = manifest.base_commit.as_deref()?;
    let expected_commit = match manifest.authenticated_workspace_commit(expected_base) {
        Ok(commit) => commit,
        Err(reason) => return Some(reason),
    };
    let reported = expected_entries.keys().cloned().collect::<Vec<_>>();
    match git_workspace_snapshot(root, &reported) {
        Ok(workspace) if workspace.commit != expected_commit => Some(format!(
            "regex index base commit changed (indexed={expected_commit}, current={})",
            workspace.commit
        )),
        Ok(workspace) if workspace.entries != *expected_entries => Some(
            "workspace freshness could not be authenticated because the Git working tree changed after the indexed publication"
                .to_string(),
        ),
        Ok(_) => None,
        Err(error) => Some(format!(
            "workspace freshness could not be authenticated: {error}"
        )),
    }
}

pub(crate) fn authenticate_incremental_workspace(
    root: &Path,
    manifest: &RegexIndexManifest,
    previous: &BTreeMap<String, String>,
    changed_paths: &[String],
) -> Result<Option<GitWorkspaceSnapshot>> {
    let Some(base_commit) = manifest.base_commit.as_deref() else {
        return Ok(None);
    };
    let snapshot = git_workspace_snapshot(root, changed_paths)
        .map_err(|reason| SearchError::IndexNotReady { reason })?;
    let changes_are_reported = previous
        .keys()
        .chain(snapshot.entries.keys())
        .all(|candidate| {
            previous.get(candidate) == snapshot.entries.get(candidate)
                || changed_paths.iter().any(|path| candidate == path)
        });
    if manifest
        .authenticated_workspace_commit(base_commit)
        .is_err()
        || snapshot.commit != base_commit
        || !changes_are_reported
    {
        return Err(SearchError::IndexNotReady {
            reason: "incremental regex update could not authenticate every workspace change path"
                .to_string(),
        });
    }
    Ok(Some(snapshot))
}

pub(crate) fn authenticate_indexed_workspace_after(
    root: &Path,
    reported_paths: &[String],
    before: Option<GitWorkspaceSnapshot>,
    indexed_fingerprints: &BTreeMap<String, String>,
) -> Result<Option<BTreeMap<String, String>>> {
    let Some(before) = before else {
        return Ok(None);
    };
    let after = git_workspace_snapshot(root, reported_paths)
        .map_err(|reason| SearchError::IndexNotReady { reason })?;
    if after != before {
        return Err(SearchError::IndexNotReady {
            reason: "Git workspace changed while publishing the incremental regex index"
                .to_string(),
        });
    }
    after.ensure_indexed_bytes(indexed_fingerprints)?;
    Ok(Some(after.entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_bytes_must_match_the_attested_snapshot() {
        let path = "src/lib.rs".to_string();
        let snapshot = GitWorkspaceSnapshot {
            commit: "base".to_string(),
            clean: false,
            entries: BTreeMap::from([(path.clone(), "after".to_string())]),
            reported: BTreeMap::from([(
                path.clone(),
                WorkspacePathAttestation {
                    digest: "after".to_string(),
                    indexable: true,
                },
            )]),
        };

        let error = snapshot
            .ensure_indexed_bytes(&BTreeMap::from([(path, "intermediate".to_string())]))
            .expect_err("intermediate bytes matched the final workspace attestation");

        assert!(matches!(error, SearchError::IndexNotReady { .. }));
    }
}
