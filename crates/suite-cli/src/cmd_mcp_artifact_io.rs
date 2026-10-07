use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use anyhow::{anyhow, Context, Result};
use packet28_daemon_protocol::paths::{task_artifact_dir, ContextVersionStorageId, TaskStorageId};
use serde_json::Value;

/// Maximum artifact body that can be returned in one MCP response.
pub(super) const MAX_MCP_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn encode_json_artifact(payload: &Value) -> Result<Vec<u8>> {
    let mut writer = BoundedArtifactWriter::default();
    serde_json::to_writer_pretty(&mut writer, payload)
        .context("artifact JSON exceeds the bounded MCP artifact envelope")?;
    Ok(writer.bytes)
}

#[derive(Default)]
struct BoundedArtifactWriter {
    bytes: Vec<u8>,
}

impl io::Write for BoundedArtifactWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let new_len = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("artifact JSON length overflow"))?;
        if new_len > MAX_MCP_ARTIFACT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("artifact JSON exceeds the {MAX_MCP_ARTIFACT_BYTES} byte limit"),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ArtifactLocation {
    TaskRoot,
    HookSpool,
    HookArtifacts,
    ToolEvidence,
    Versions,
}

impl ArtifactLocation {
    fn component(self) -> Option<&'static str> {
        match self {
            Self::TaskRoot => None,
            Self::HookSpool => Some("hook-spool"),
            Self::HookArtifacts => Some("hook-artifacts"),
            Self::ToolEvidence => Some("tool-evidence"),
            Self::Versions => Some("versions"),
        }
    }
}

/// Validated opaque filename accepted by MCP artifact readers and writers.
///
/// A handle is a lowercase portable stem with either no extension or one
/// controlled `.json`/`.log`/`.md` extension. It is never trimmed or interpreted as
/// a path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ArtifactHandle(String);

impl ArtifactHandle {
    pub(super) fn from_json_stem(stem: &str) -> Result<Self> {
        let stem = ContextVersionStorageId::try_from(stem)?;
        Self::try_from(format!("{stem}.json"))
    }

    pub(super) fn from_invocation(invocation_id: &str, suffix: &str) -> Result<Self> {
        let invocation_id = ContextVersionStorageId::try_from(invocation_id)?;
        let suffix = ContextVersionStorageId::try_from(suffix)?;
        Self::from_json_stem(&format!("{invocation_id}-{suffix}"))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn json_file_name(&self) -> Result<Self> {
        if self.0.ends_with(".json") {
            return Ok(self.clone());
        }
        if self.0.contains('.') {
            return Err(anyhow!(
                "artifact handle {:?} cannot name a JSON artifact",
                self.0
            ));
        }
        Self::try_from(format!("{}.json", self.0))
    }
}

impl TryFrom<&str> for ArtifactHandle {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        let stem = value
            .strip_suffix(".json")
            .or_else(|| value.strip_suffix(".log"))
            .or_else(|| value.strip_suffix(".md"))
            .unwrap_or(value);
        if stem.len() == value.len() && value.contains('.') {
            return Err(anyhow!(
                "artifact handle {value:?} has an unsupported extension"
            ));
        }
        ContextVersionStorageId::try_from(stem)
            .with_context(|| format!("invalid artifact handle {value:?}"))?;
        if value.len() > 255 {
            return Err(anyhow!(
                "artifact handle is {} bytes; maximum supported size is 255 bytes",
                value.len()
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for ArtifactHandle {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::try_from(value.as_str())
    }
}

pub(super) fn write_task_artifact(
    root: &Path,
    task_id: &TaskStorageId,
    location: ArtifactLocation,
    handle: &ArtifactHandle,
    bytes: &[u8],
) -> Result<PathBuf> {
    if bytes.len() > MAX_MCP_ARTIFACT_BYTES {
        return Err(anyhow!(
            "artifact body is {} bytes; maximum supported size is {} bytes",
            bytes.len(),
            MAX_MCP_ARTIFACT_BYTES
        ));
    }
    platform::write_task_artifact(root, task_id, location, handle, bytes)
}

pub(super) fn read_task_artifact(
    root: &Path,
    task_id: &TaskStorageId,
    location: ArtifactLocation,
    handle: &ArtifactHandle,
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    platform::read_task_artifact(root, task_id, location, handle)
}

fn display_path(
    root: &Path,
    task_id: &TaskStorageId,
    location: ArtifactLocation,
    handle: &ArtifactHandle,
) -> PathBuf {
    let mut path = task_artifact_dir(root, task_id);
    if let Some(component) = location.component() {
        path.push(component);
    }
    path.join(handle.as_str())
}

fn read_bounded(mut file: File, path: &Path) -> Result<Vec<u8>> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect artifact '{}'", path.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "artifact '{}' is not a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(anyhow!(
                "artifact '{}' has {} hard links; exactly one is required",
                path.display(),
                metadata.nlink()
            ));
        }
    }
    if metadata.len() > MAX_MCP_ARTIFACT_BYTES as u64 {
        return Err(anyhow!(
            "artifact '{}' is {} bytes; maximum supported size is {} bytes",
            path.display(),
            metadata.len(),
            MAX_MCP_ARTIFACT_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_MCP_ARTIFACT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read artifact '{}'", path.display()))?;
    if bytes.len() > MAX_MCP_ARTIFACT_BYTES {
        return Err(anyhow!(
            "artifact '{}' grew beyond the {} byte limit while being read",
            path.display(),
            MAX_MCP_ARTIFACT_BYTES
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
mod platform {
    use std::ffi::{CStr, CString};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use anyhow::{anyhow, Context, Result};
    use packet28_daemon_protocol::paths::{TaskStorageId, MAX_TASK_STORAGE_ID_BYTES};

    use super::{display_path, read_bounded, ArtifactHandle, ArtifactLocation, TEMP_SEQUENCE};
    use std::sync::atomic::Ordering;

    const MAX_ANCHORED_DIRECTORY_ENTRIES: usize = 4096;

    struct AnchoredDir {
        file: File,
    }

    impl AnchoredDir {
        fn open_root(path: &Path) -> Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(path)
                .with_context(|| format!("failed to open artifact root '{}'", path.display()))?;
            if !file.metadata()?.is_dir() {
                return Err(anyhow!(
                    "artifact root '{}' is not a directory",
                    path.display()
                ));
            }
            Ok(Self { file })
        }

        fn open_child(&self, name: &str, create: bool) -> Result<Option<Self>> {
            let name = c_string(name)?;
            match open_directory_at(self.file.as_raw_fd(), &name) {
                Ok(file) => {
                    validate_exact_child_attachment(self.file.as_raw_fd(), &name, &file)?;
                    Ok(Some(Self { file }))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound && !create => Ok(None),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    mkdir_at(self.file.as_raw_fd(), &name)?;
                    let file = open_directory_at(self.file.as_raw_fd(), &name)?;
                    validate_exact_child_attachment(self.file.as_raw_fd(), &name, &file)?;
                    self.file.sync_all()?;
                    Ok(Some(Self { file }))
                }
                Err(error) => Err(error.into()),
            }
        }

        fn open_read(&self, name: &str) -> Result<Option<File>> {
            let name = c_string(name)?;
            let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
            // SAFETY: `self.file` owns a live directory descriptor and `name`
            // is a NUL-terminated single component for the duration of the call.
            let descriptor = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(error.into());
            }
            // SAFETY: a successful `openat` returns a new owned descriptor.
            let file = unsafe { File::from_raw_fd(descriptor) };
            validate_exact_child_attachment(self.file.as_raw_fd(), &name, &file)?;
            Ok(Some(file))
        }

        /// Durably publishes a new artifact. Evidence is immutable: an
        /// existing entry of any kind is never replaced, followed or modified,
        /// including one created concurrently after the first check.
        fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
            let destination = c_string(name)?;
            if stat_at_nofollow(self.file.as_raw_fd(), &destination)?.is_some() {
                return Err(existing_artifact_error());
            }
            let temporary_name = format!(
                ".p28-artifact-{}-{}.tmp",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let temporary = c_string(&temporary_name)?;
            let flags =
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
            // SAFETY: the directory descriptor and NUL-terminated component
            // are valid, and mode is supplied because `O_CREAT` is set.
            let descriptor =
                unsafe { libc::openat(self.file.as_raw_fd(), temporary.as_ptr(), flags, 0o600) };
            if descriptor < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: a successful `openat` returns a new owned descriptor.
            let mut file = unsafe { File::from_raw_fd(descriptor) };
            let write_result = (|| -> Result<()> {
                file.write_all(bytes)?;
                file.sync_all()?;
                match rename_no_replace(self.file.as_raw_fd(), &temporary, &destination) {
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        return Err(existing_artifact_error());
                    }
                    result => result?,
                }
                self.file.sync_all()?;
                Ok(())
            })();
            if write_result.is_err() {
                // SAFETY: the directory descriptor is live and the temporary
                // name is a NUL-terminated single component. Failure to remove
                // an already-renamed or absent temporary is harmless.
                unsafe {
                    libc::unlinkat(self.file.as_raw_fd(), temporary.as_ptr(), 0);
                }
            }
            write_result
        }
    }

    fn existing_artifact_error() -> anyhow::Error {
        anyhow!("refusing to replace existing task artifact; artifacts are immutable evidence")
    }

    /// Atomically renames `source` to `destination` within one directory,
    /// failing with `AlreadyExists` instead of replacing an existing entry.
    #[cfg(target_vendor = "apple")]
    fn rename_no_replace(directory: RawFd, source: &CStr, destination: &CStr) -> io::Result<()> {
        // SAFETY: the directory descriptor is live and both names are
        // NUL-terminated single components for the duration of the call.
        let result = unsafe {
            libc::renameatx_np(
                directory,
                source.as_ptr(),
                directory,
                destination.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // Filesystems without RENAME_EXCL keep the create-only contract
            // through an exclusive hard link.
            Some(libc::ENOTSUP | libc::EINVAL) => link_no_replace(directory, source, destination),
            _ => Err(error),
        }
    }

    /// Atomically renames `source` to `destination` within one directory,
    /// failing with `AlreadyExists` instead of replacing an existing entry.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn rename_no_replace(directory: RawFd, source: &CStr, destination: &CStr) -> io::Result<()> {
        // SAFETY: the directory descriptor is live and both names are
        // NUL-terminated single components for the duration of the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                directory,
                source.as_ptr(),
                directory,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // Kernels or filesystems without RENAME_NOREPLACE keep the
            // create-only contract through an exclusive hard link.
            Some(libc::EINVAL | libc::ENOSYS) => link_no_replace(directory, source, destination),
            _ => Err(error),
        }
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    fn rename_no_replace(directory: RawFd, source: &CStr, destination: &CStr) -> io::Result<()> {
        link_no_replace(directory, source, destination)
    }

    /// Publishes by exclusive hard link, then removes the temporary name.
    /// Readers require exactly one link, so a failed removal is reported.
    fn link_no_replace(directory: RawFd, source: &CStr, destination: &CStr) -> io::Result<()> {
        // SAFETY: the directory descriptor is live and both names are
        // NUL-terminated single components for the duration of the call.
        if unsafe {
            libc::linkat(
                directory,
                source.as_ptr(),
                directory,
                destination.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; the published name keeps the inode alive.
        if unsafe { libc::unlinkat(directory, source.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn write_task_artifact(
        root: &Path,
        task_id: &TaskStorageId,
        location: ArtifactLocation,
        handle: &ArtifactHandle,
        bytes: &[u8],
    ) -> Result<PathBuf> {
        let directory = task_location(root, task_id, location, true)?
            .ok_or_else(|| anyhow!("failed to create anchored task artifact directory"))?;
        directory
            .write_new(handle.as_str(), bytes)
            .with_context(|| {
                format!(
                    "failed to write task artifact '{}'",
                    display_path(root, task_id, location, handle).display()
                )
            })?;
        Ok(display_path(root, task_id, location, handle))
    }

    pub(super) fn read_task_artifact(
        root: &Path,
        task_id: &TaskStorageId,
        location: ArtifactLocation,
        handle: &ArtifactHandle,
    ) -> Result<Option<(PathBuf, Vec<u8>)>> {
        let Some(directory) = task_location(root, task_id, location, false)? else {
            return Ok(None);
        };
        let path = display_path(root, task_id, location, handle);
        let Some(file) = directory
            .open_read(handle.as_str())
            .with_context(|| format!("failed to open artifact '{}'", path.display()))?
        else {
            return Ok(None);
        };
        Ok(Some((path.clone(), read_bounded(file, &path)?)))
    }

    fn task_location(
        root: &Path,
        task_id: &TaskStorageId,
        location: ArtifactLocation,
        create: bool,
    ) -> Result<Option<AnchoredDir>> {
        debug_assert!(task_id.as_str().len() <= MAX_TASK_STORAGE_ID_BYTES);
        let mut directory = AnchoredDir::open_root(root)?;
        for component in [".packet28", "task", task_id.as_str()] {
            let Some(child) = directory.open_child(component, create)? else {
                return Ok(None);
            };
            directory = child;
        }
        if let Some(component) = location.component() {
            let Some(child) = directory.open_child(component, create)? else {
                return Ok(None);
            };
            directory = child;
        }
        Ok(Some(directory))
    }

    fn c_string(value: &str) -> Result<CString> {
        if value.as_bytes().contains(&b'/') {
            return Err(anyhow!("artifact component contains a path separator"));
        }
        CString::new(value).map_err(|_| anyhow!("artifact component contains NUL"))
    }

    fn open_directory_at(parent: RawFd, name: &CStr) -> io::Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_DIRECTORY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK;
        // SAFETY: `parent` is a live directory descriptor and `name` is a
        // NUL-terminated single component for the duration of the call.
        let descriptor = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful `openat` returns a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn mkdir_at(parent: RawFd, name: &CStr) -> io::Result<()> {
        // SAFETY: `parent` is a live directory descriptor and `name` is a
        // NUL-terminated single component for the duration of the call.
        let result = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Ok(());
        }
        Err(error)
    }

    fn validate_exact_child_attachment(parent: RawFd, name: &CStr, child: &File) -> Result<()> {
        if !directory_contains_exact_name(parent, name)? {
            return Err(anyhow!(
                "artifact component {:?} does not have exact filesystem spelling",
                name.to_string_lossy()
            ));
        }
        let linked = stat_at_nofollow(parent, name)?.ok_or_else(|| {
            anyhow!(
                "artifact component {:?} disappeared during anchored open",
                name.to_string_lossy()
            )
        })?;
        let linked_device = stat_device(&linked)?;
        let linked_inode = linked.st_ino;
        let opened = child.metadata()?;
        if opened.dev() != linked_device || opened.ino() != linked_inode {
            return Err(anyhow!(
                "artifact component {:?} changed during anchored open",
                name.to_string_lossy()
            ));
        }
        Ok(())
    }

    fn directory_contains_exact_name(parent: RawFd, expected: &CStr) -> io::Result<bool> {
        // SAFETY: `parent` is a live descriptor. `F_DUPFD_CLOEXEC` returns a
        // new descriptor owned by this function.
        let duplicate = unsafe { libc::fcntl(parent, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `duplicate` is an owned directory descriptor. On success,
        // `fdopendir` transfers ownership to the returned stream.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: ownership was not transferred when `fdopendir` failed.
            unsafe {
                libc::close(duplicate);
            }
            return Err(error);
        }

        // The duplicate shares its offset with `parent`; rewind so an
        // earlier scan of the same descriptor cannot hide entries.
        // SAFETY: `stream` is a live directory stream owned by this function.
        unsafe {
            libc::rewinddir(stream);
        }
        let mut entries = 0_usize;
        let mut found = false;
        loop {
            // SAFETY: `stream` remains live until the `closedir` below.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            entries = entries.saturating_add(1);
            if entries > MAX_ANCHORED_DIRECTORY_ENTRIES {
                // SAFETY: `stream` is live and owned by this function.
                unsafe {
                    libc::closedir(stream);
                }
                return Err(io::Error::other(format!(
                    "artifact directory exceeds the {MAX_ANCHORED_DIRECTORY_ENTRIES}-entry safety limit"
                )));
            }
            // SAFETY: POSIX guarantees that a non-null directory entry has a
            // NUL-terminated `d_name` valid until the next `readdir` call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == expected {
                found = true;
                break;
            }
        }
        // SAFETY: `stream` is live and owned by this function.
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(found)
    }

    #[cfg(target_vendor = "apple")]
    fn stat_device(stat: &libc::stat) -> Result<u64> {
        u64::try_from(stat.st_dev)
            .map_err(|_| anyhow!("artifact component device identifier is invalid"))
    }

    #[cfg(not(target_vendor = "apple"))]
    fn stat_device(stat: &libc::stat) -> Result<u64> {
        Ok(stat.st_dev)
    }

    fn stat_at_nofollow(parent: RawFd, name: &CStr) -> io::Result<Option<libc::stat>> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `parent` is a live directory descriptor, `name` is
        // NUL-terminated, and `stat` points to writable uninitialized storage.
        let result = unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: successful `fstatat` initialized the output structure.
            return Ok(Some(unsafe { stat.assume_init() }));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(error)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hard_link_fallback_publishes_create_only() {
            let root = tempfile::tempdir().unwrap();
            let directory = AnchoredDir::open_root(root.path()).unwrap();
            let descriptor = directory.file.as_raw_fd();
            let (temporary, destination) = (c_string("t.tmp").unwrap(), c_string("a").unwrap());

            std::fs::write(root.path().join("t.tmp"), b"first").unwrap();
            link_no_replace(descriptor, &temporary, &destination).unwrap();
            assert!(!root.path().join("t.tmp").exists());
            let published = std::fs::metadata(root.path().join("a")).unwrap();
            assert_eq!(published.nlink(), 1);

            std::fs::write(root.path().join("t.tmp"), b"second").unwrap();
            let error = link_no_replace(descriptor, &temporary, &destination).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"first");
        }

        #[test]
        fn exact_name_scan_rewinds_a_shared_directory_offset() {
            let root = tempfile::tempdir().unwrap();
            for name in ["a.json", "b.json", "c.json"] {
                std::fs::write(root.path().join(name), b"{}").unwrap();
            }
            let directory = AnchoredDir::open_root(root.path()).unwrap();
            for name in ["a.json", "b.json", "c.json", "a.json"] {
                let name = c_string(name).unwrap();
                for _ in 0..2 {
                    assert!(
                        directory_contains_exact_name(directory.file.as_raw_fd(), &name).unwrap()
                    );
                }
            }
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use anyhow::{anyhow, Context, Result};
    use packet28_daemon_protocol::paths::{task_artifact_dir, TaskStorageId};

    use super::{display_path, read_bounded, ArtifactHandle, ArtifactLocation, TEMP_SEQUENCE};
    use std::sync::atomic::Ordering;

    pub(super) fn write_task_artifact(
        root: &Path,
        task_id: &TaskStorageId,
        location: ArtifactLocation,
        handle: &ArtifactHandle,
        bytes: &[u8],
    ) -> Result<PathBuf> {
        let directory = location_path(root, task_id, location);
        ensure_directory_chain(root, &directory)?;
        let path = directory.join(handle.as_str());
        reject_unsafe_component(&path)?;
        let temporary = directory.join(format!(
            ".p28-artifact-{}-{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        // A hard link publishes create-only: an existing artifact is never
        // replaced. The temporary name is removed either way.
        let published = written.and_then(|()| fs::hard_link(&temporary, &path));
        let _ = fs::remove_file(&temporary);
        match published {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                "refusing to replace existing task artifact; artifacts are immutable evidence"
            )),
            result => Ok(result.map(|()| path)?),
        }
    }

    pub(super) fn read_task_artifact(
        root: &Path,
        task_id: &TaskStorageId,
        location: ArtifactLocation,
        handle: &ArtifactHandle,
    ) -> Result<Option<(PathBuf, Vec<u8>)>> {
        let path = display_path(root, task_id, location, handle);
        if !path.exists() {
            return Ok(None);
        }
        reject_unsafe_component(&path)?;
        let file = File::open(&path)
            .with_context(|| format!("failed to open artifact '{}'", path.display()))?;
        Ok(Some((path.clone(), read_bounded(file, &path)?)))
    }

    fn location_path(root: &Path, task_id: &TaskStorageId, location: ArtifactLocation) -> PathBuf {
        let mut path = task_artifact_dir(root, task_id);
        if let Some(component) = location.component() {
            path.push(component);
        }
        path
    }

    fn ensure_directory_chain(root: &Path, destination: &Path) -> Result<()> {
        let relative = destination.strip_prefix(root)?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(anyhow!(
                        "artifact directory '{}' is not a real directory",
                        current.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn reject_unsafe_component(path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
                anyhow!("artifact '{}' is not a real regular file", path.display()),
            ),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use packet28_daemon_protocol::paths::TaskStorageId;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn artifact_handles_reject_normalization_and_path_syntax() {
        for value in [
            "",
            " result.json",
            "result.json ",
            "Result.json",
            ".",
            "..",
            "../result.json",
            r"..\result.json",
            "/tmp/result.json",
            "result.txt",
            "result.extra.json",
            "λ.json",
            "con.json",
            "lpt1.log",
        ] {
            assert!(
                ArtifactHandle::try_from(value).is_err(),
                "handle {value:?} should be rejected"
            );
        }
    }

    #[test]
    fn artifact_handles_enforce_the_exact_filename_budget() {
        let accepted = format!("{}.json", "a".repeat(250));
        let rejected = format!("{}.json", "a".repeat(251));

        assert_eq!(
            ArtifactHandle::try_from(accepted.as_str())
                .unwrap()
                .as_str()
                .len(),
            255
        );
        assert!(ArtifactHandle::try_from(rejected.as_str()).is_err());
    }

    #[test]
    fn anchored_artifact_roundtrip_is_bounded_and_confined() {
        let root = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let handle = ArtifactHandle::from_invocation("invocation-1", "result").unwrap();

        let path = write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
            br#"{"ok":true}"#,
        )
        .unwrap();
        let (read_path, bytes) = read_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
        )
        .unwrap()
        .unwrap();

        assert_eq!(path, read_path);
        assert_eq!(bytes, br#"{"ok":true}"#);
        assert!(path.starts_with(task_artifact_dir(root.path(), &task_id)));
    }

    #[test]
    fn existing_artifact_is_never_replaced() {
        let root = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let handle = ArtifactHandle::from_invocation("invocation-1", "result").unwrap();
        let path = write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
            br#"{"first":true}"#,
        )
        .unwrap();

        let error = write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
            br#"{"second":true}"#,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("refusing to replace existing task artifact"),
            "{error:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), br#"{"first":true}"#);
        let entries = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from(handle.as_str())]);
    }

    #[test]
    fn concurrent_writers_of_one_handle_publish_exactly_one_artifact() {
        const WRITERS: usize = 8;
        let root = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let handle = ArtifactHandle::from_invocation("invocation-1", "result").unwrap();
        let barrier = std::sync::Barrier::new(WRITERS);
        let results = std::thread::scope(|scope| {
            let workers = (0..WRITERS)
                .map(|writer| {
                    let (root, task_id, handle, barrier) =
                        (root.path(), &task_id, &handle, &barrier);
                    scope.spawn(move || {
                        let bytes = format!(r#"{{"writer":{writer}}}"#).into_bytes();
                        barrier.wait();
                        let result = write_task_artifact(
                            root,
                            task_id,
                            ArtifactLocation::ToolEvidence,
                            handle,
                            &bytes,
                        );
                        (bytes, result)
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });

        let mut winners = results.iter().filter(|(_, result)| result.is_ok());
        let (winner_bytes, winner) = winners.next().expect("one writer publishes");
        assert!(winners.next().is_none(), "only one writer may publish");
        for (_, result) in &results {
            if let Err(error) = result {
                assert!(
                    format!("{error:#}").contains("refusing to replace existing task artifact"),
                    "{error:#}"
                );
            }
        }
        let path = winner.as_ref().unwrap();
        assert_eq!(&fs::read(path).unwrap(), winner_bytes);
        let entries = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from(handle.as_str())]);
    }

    #[test]
    fn oversized_artifact_write_is_rejected_before_creating_task_storage() {
        let root = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let handle = ArtifactHandle::from_invocation("invocation-1", "result").unwrap();
        let bytes = vec![0; MAX_MCP_ARTIFACT_BYTES + 1];

        assert!(write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
            &bytes,
        )
        .is_err());
        assert!(!task_artifact_dir(root.path(), &task_id).exists());
    }

    #[test]
    fn artifact_json_encoding_stops_at_the_shared_byte_budget() {
        let payload = Value::String("x".repeat(MAX_MCP_ARTIFACT_BYTES));

        assert!(encode_json_artifact(&payload).is_err());
    }

    #[test]
    fn oversized_artifact_read_is_rejected_before_body_allocation() {
        let root = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let seed = ArtifactHandle::from_invocation("seed", "result").unwrap();
        let path = write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &seed,
            b"seed",
        )
        .unwrap();
        fs::write(&path, vec![0; MAX_MCP_ARTIFACT_BYTES + 1]).unwrap();

        assert!(
            read_task_artifact(root.path(), &task_id, ArtifactLocation::ToolEvidence, &seed,)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn anchored_artifact_read_rejects_symlinks_and_hard_links() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let seed = ArtifactHandle::from_invocation("seed", "result").unwrap();
        write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &seed,
            b"seed",
        )
        .unwrap();
        let directory = task_artifact_dir(root.path(), &task_id).join("tool-evidence");
        let outside_path = outside.path().join("outside.json");
        fs::write(&outside_path, b"outside").unwrap();

        let symlink_handle = ArtifactHandle::try_from("linked.json").unwrap();
        symlink(&outside_path, directory.join(symlink_handle.as_str())).unwrap();
        assert!(read_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &symlink_handle,
        )
        .is_err());

        let hardlink_handle = ArtifactHandle::try_from("hardlinked.json").unwrap();
        fs::hard_link(&outside_path, directory.join(hardlink_handle.as_str())).unwrap();
        assert!(read_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &hardlink_handle,
        )
        .is_err());
        assert_eq!(fs::read(&outside_path).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn anchored_artifact_write_rejects_existing_symlink_without_mutating_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let task_id = TaskStorageId::try_from("task").unwrap();
        let seed = ArtifactHandle::from_invocation("seed", "result").unwrap();
        write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &seed,
            b"seed",
        )
        .unwrap();
        let handle = ArtifactHandle::try_from("linked.json").unwrap();
        let outside_path = outside.path().join("outside.json");
        fs::write(&outside_path, b"outside").unwrap();
        symlink(
            &outside_path,
            task_artifact_dir(root.path(), &task_id)
                .join("tool-evidence")
                .join(handle.as_str()),
        )
        .unwrap();

        assert!(write_task_artifact(
            root.path(),
            &task_id,
            ArtifactLocation::ToolEvidence,
            &handle,
            b"replacement",
        )
        .is_err());
        assert_eq!(fs::read(&outside_path).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn anchored_artifacts_reject_casefolded_task_spelling() {
        let root = tempdir().unwrap();
        let historical = root.path().join(".packet28/task/LIVE/tool-evidence");
        fs::create_dir_all(&historical).unwrap();
        let historical_artifact = historical.join("result.json");
        fs::write(&historical_artifact, b"historical").unwrap();
        let task_id = TaskStorageId::try_from("live").unwrap();
        if !task_artifact_dir(root.path(), &task_id).exists() {
            eprintln!("filesystem is case-sensitive");
            return;
        }
        let handle = ArtifactHandle::try_from("result.json").unwrap();

        assert!(
            read_task_artifact(
                root.path(),
                &task_id,
                ArtifactLocation::ToolEvidence,
                &handle,
            )
            .is_err(),
            "typed lowercase task read adopted historical uppercase task root"
        );
        assert!(
            write_task_artifact(
                root.path(),
                &task_id,
                ArtifactLocation::ToolEvidence,
                &handle,
                b"replacement",
            )
            .is_err(),
            "typed lowercase task write adopted historical uppercase task root"
        );
        assert_eq!(fs::read(historical_artifact).unwrap(), b"historical");
    }
}
