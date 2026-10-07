//! Descriptor-anchored I/O for repository-local Packet28 state.
//!
//! [`StateDir`] canonicalizes only the caller-supplied workspace root. Every
//! managed descendant is then opened one component at a time without following
//! symbolic links. Reads admit only single-link regular files and enforce their
//! byte limit both before allocation and while streaming.
//!
//! [`RotatingLog`] is the size-bounded diagnostic sink for long-running
//! Packet28 processes; it rotates its own retained log leaf in-process.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(unix)]
use fs2::FileExt;

const MAX_TEMP_ATTEMPTS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static PROCESS_NONCE: OnceLock<[u64; 2]> = OnceLock::new();

/// Access requested for a retained regular file.
#[derive(Clone, Copy, Debug)]
pub enum FileAccess {
    /// Read without allowing mutation.
    ReadOnly,
    /// Read and write at explicit offsets.
    ReadWrite,
    /// Read and append atomically at the current end.
    Append,
}

/// A retained directory beneath an authenticated workspace root.
#[derive(Clone, Debug)]
pub struct StateDir {
    inner: Arc<platform::RetainedDir>,
}

/// A retained regular file and its expected directory attachment.
#[derive(Debug)]
pub struct StateFile {
    directory: StateDir,
    name: OsString,
    file: File,
    identity: platform::Identity,
}

/// Result of opening or creating a retained regular file.
#[derive(Debug)]
pub struct OpenedStateFile {
    /// Retained file handle.
    pub file: StateFile,
    /// Whether this call created the file.
    pub created: bool,
}

/// An advisory exclusive lease on a retained directory.
///
/// The lease is attached to the retained directory object rather than to a
/// replaceable child entry. It is therefore suitable for serializing
/// cooperative namespace writers even if an attacker renames their
/// conventional lock-file leaf.
#[cfg(unix)]
#[derive(Debug)]
#[must_use = "dropping the directory lock releases the exclusive lease"]
pub struct StateDirLock {
    file: File,
}

impl StateDir {
    /// Opens a managed directory beneath `root`.
    ///
    /// The root itself may be a symlink for compatibility with worktrees and
    /// symlinked workspace aliases. Managed components are never followed.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized, a component is
    /// invalid, a managed ancestor is not a real directory, or a requested
    /// directory cannot be created.
    pub fn open(root: &Path, components: &[&str], create: bool) -> io::Result<Self> {
        platform::RetainedDir::open(root, components, create).map(|inner| Self {
            inner: Arc::new(inner),
        })
    }

    /// Returns the diagnostic path corresponding to this capability.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Reopens every retained ancestor and verifies its identity.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] when any ancestor has been
    /// substituted since this capability was opened.
    pub fn validate(&self) -> io::Result<()> {
        self.inner.validate()
    }

    /// Acquires an advisory exclusive lease on this retained directory.
    ///
    /// Cooperative writers must acquire this lease before any replaceable
    /// child lock. The retained ancestry is revalidated after acquisition.
    ///
    /// # Errors
    ///
    /// Returns an error when the advisory lock cannot be acquired or the
    /// retained ancestry changes while waiting.
    #[cfg(unix)]
    pub fn lock_exclusive(&self) -> io::Result<StateDirLock> {
        self.acquire_exclusive_lock(false)
    }

    /// Attempts to acquire an advisory exclusive lease without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when another descriptor holds an
    /// incompatible lease, or an ancestry error if this capability detached.
    #[cfg(unix)]
    pub fn try_lock_exclusive(&self) -> io::Result<StateDirLock> {
        self.acquire_exclusive_lock(true)
    }

    #[cfg(unix)]
    fn acquire_exclusive_lock(&self, nonblocking: bool) -> io::Result<StateDirLock> {
        self.validate()?;
        let file = self.inner.open_directory_lock_file()?;
        if nonblocking {
            FileExt::try_lock_exclusive(&file)?;
        } else {
            FileExt::lock_exclusive(&file)?;
        }
        if let Err(error) = self.validate() {
            let _ = FileExt::unlock(&file);
            return Err(error);
        }
        Ok(StateDirLock { file })
    }

    /// Reads an optional regular file through a hard byte limit.
    ///
    /// The leaf is opened nonblocking and without following symlinks, then
    /// admitted by type and size before storage is reserved.
    ///
    /// # Errors
    ///
    /// Returns an error for special files, symlinks, hard-link aliases,
    /// substitution, or content larger than `max_bytes`.
    pub fn read_bounded(&self, name: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        let Some(mut retained) = self.open_bounded(name, FileAccess::ReadOnly, max_bytes)? else {
            return Ok(None);
        };
        retained.file.seek(SeekFrom::Start(0))?;
        let capacity = usize::try_from(retained.len()?).map_err(|_| oversized(name, max_bytes))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|source| io::Error::new(io::ErrorKind::OutOfMemory, source))?;
        Read::by_ref(&mut retained.file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(oversized(name, max_bytes));
        }
        retained.validate_attachment()?;
        Ok(Some(bytes))
    }

    /// Opens an optional regular file after bounded admission.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe leaves, replaced ancestors, or a logical
    /// size greater than `max_bytes`.
    pub fn open_bounded(
        &self,
        name: &str,
        access: FileAccess,
        max_bytes: u64,
    ) -> io::Result<Option<StateFile>> {
        self.validate()?;
        let Some((file, identity)) = self.inner.open_existing(name, access)? else {
            return Ok(None);
        };
        let retained = StateFile {
            directory: self.clone(),
            name: OsString::from(name),
            file,
            identity,
        };
        if retained.len()? > max_bytes {
            return Err(oversized(name, max_bytes));
        }
        retained.validate_attachment()?;
        Ok(Some(retained))
    }

    /// Opens an existing regular file without a size ceiling.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe leaves or replaced ancestors.
    pub fn open_existing(&self, name: &str, access: FileAccess) -> io::Result<Option<StateFile>> {
        self.open_bounded(name, access, u64::MAX)
    }

    /// Creates a regular file or opens the existing regular file.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing leaf is unsafe, creation fails, or
    /// the retained ancestry changes.
    pub fn open_or_create(&self, name: &str, access: FileAccess) -> io::Result<OpenedStateFile> {
        self.validate()?;
        let (file, identity, created) = self.inner.open_or_create(name, access)?;
        let retained = StateFile {
            directory: self.clone(),
            name: OsString::from(name),
            file,
            identity,
        };
        retained.validate_attachment()?;
        if created {
            self.inner.sync()?;
            self.validate()?;
        }
        Ok(OpenedStateFile {
            file: retained,
            created,
        })
    }

    /// Creates and durably writes a new immutable regular file.
    ///
    /// # Errors
    ///
    /// Returns an error if the leaf already exists or ancestry changes.
    pub fn write_immutable(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.validate()?;
        let (mut file, identity) = self.inner.create_new(name, FileAccess::ReadWrite)?;
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(error) = result {
            let _ = self.inner.remove_if_identity(name, identity);
            return Err(error);
        }
        let retained = StateFile {
            directory: self.clone(),
            name: OsString::from(name),
            file,
            identity,
        };
        retained.validate_attachment()?;
        self.inner.sync()?;
        self.validate()
    }

    /// Atomically replaces a regular file using a unique same-directory file.
    ///
    /// Existing symlink, special-file, and hard-link leaves are rejected.
    /// A substitution after admission is replaced as a directory entry and is
    /// never followed.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination is unsafe, the temporary cannot be
    /// written, publication fails, or ancestry changes.
    pub fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.write_atomic_stream(name, |file| file.write_all(bytes))
    }

    /// Atomically replaces a regular file with streamed content.
    ///
    /// The writer receives a newly created same-directory temporary. On
    /// success the file is synced and published descriptor-relative.
    ///
    /// # Errors
    ///
    /// Returns an error from `write` or if temporary creation, synchronization,
    /// publication, or ancestry validation fails.
    pub fn write_atomic_stream(
        &self,
        name: &str,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        self.write_atomic_stream_with_observers(name, write, |_| Ok(()), |_| Ok(()), |_, _| Ok(()))
    }

    /// Variant of [`Self::write_atomic`] with a testable pre-publication hook.
    ///
    /// # Errors
    ///
    /// Propagates the same errors as [`Self::write_atomic`] and errors from
    /// `before_publish`.
    pub fn write_atomic_with(
        &self,
        name: &str,
        bytes: &[u8],
        before_publish: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.write_atomic_with_observers(
            name,
            bytes,
            |_| Ok(()),
            |_| Ok(()),
            |_, _| before_publish(),
        )
    }

    /// Atomically replaces a file with fault-injection observers.
    ///
    /// This is intended for persistence protocol tests. Publication itself
    /// always remains descriptor-relative; observers receive diagnostic paths
    /// only so tests can simulate namespace substitution.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::write_atomic`] plus observer errors.
    #[doc(hidden)]
    pub fn write_atomic_with_observers<B, A, P>(
        &self,
        name: &str,
        bytes: &[u8],
        before_temp_open: B,
        after_temp_sync: A,
        before_publish: P,
    ) -> io::Result<()>
    where
        B: FnMut(&Path) -> io::Result<()>,
        A: FnOnce(&Path) -> io::Result<()>,
        P: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        self.write_atomic_stream_with_observers(
            name,
            |file| file.write_all(bytes),
            before_temp_open,
            after_temp_sync,
            before_publish,
        )
    }

    fn write_atomic_stream_with_observers<W, B, A, P>(
        &self,
        name: &str,
        write: W,
        mut before_temp_open: B,
        after_temp_sync: A,
        before_publish: P,
    ) -> io::Result<()>
    where
        W: FnOnce(&mut File) -> io::Result<()>,
        B: FnMut(&Path) -> io::Result<()>,
        A: FnOnce(&Path) -> io::Result<()>,
        P: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        self.validate()?;
        self.inner.validate_replace_target(name)?;
        let mut created = None;
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let temporary = temporary_name(name);
            before_temp_open(&self.path().join(&temporary))?;
            match self.inner.create_new(&temporary, FileAccess::ReadWrite) {
                Ok((file, identity)) => {
                    created = Some((temporary, file, identity));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (temporary, mut file, identity) = created.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "state temporary namespace exhausted",
            )
        })?;
        let result = (|| {
            write(&mut file)?;
            file.sync_all()?;
            after_temp_sync(&self.path().join(&temporary))?;
            self.inner
                .validate_entry(std::ffi::OsStr::new(&temporary), identity)?;
            self.validate()?;
            before_publish(&self.path().join(&temporary), &self.path().join(name))?;
            self.inner.rename(&temporary, name)?;
            self.inner.sync()?;
            self.validate()?;
            self.inner
                .validate_entry(std::ffi::OsStr::new(name), identity)
        })();
        if result.is_err() {
            let _ = self.inner.remove_if_identity(&temporary, identity);
        }
        result
    }

    /// Returns sorted names beneath this retained directory.
    ///
    /// # Errors
    ///
    /// Returns an error after 100,000 entries or if ancestry changes.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        self.validate()?;
        let names = self.inner.names(MAX_DIRECTORY_ENTRIES)?;
        self.validate()?;
        Ok(names)
    }

    /// Removes one directory entry without following it.
    ///
    /// # Errors
    ///
    /// Returns an error when removal or ancestry validation fails.
    pub fn remove_file_if_exists(&self, name: &str) -> io::Result<()> {
        self.validate()?;
        self.inner.remove_file_if_exists(name)?;
        self.inner.sync()?;
        self.validate()
    }

    /// Recursively removes a child directory without following symlinks.
    ///
    /// # Errors
    ///
    /// Returns an error when traversal, removal, or ancestry validation fails.
    pub fn remove_tree_if_exists(&self, name: &str) -> io::Result<()> {
        self.remove_tree_if_exists_with(name, || Ok(()))
    }

    /// Variant of [`Self::remove_tree_if_exists`] with a testable
    /// pre-removal hook.
    ///
    /// The hook runs after ancestry validation and immediately before
    /// descriptor-relative removal.
    ///
    /// # Errors
    ///
    /// Propagates the same errors as [`Self::remove_tree_if_exists`] and errors
    /// from `before_remove`.
    #[doc(hidden)]
    pub fn remove_tree_if_exists_with(
        &self,
        name: &str,
        before_remove: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.validate()?;
        before_remove()?;
        self.inner.remove_tree_if_exists(name)?;
        self.inner.sync()?;
        self.validate()
    }

    /// Flushes directory metadata.
    ///
    /// # Errors
    ///
    /// Returns an operating-system sync error.
    pub fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

#[cfg(unix)]
impl Drop for StateDirLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl StateFile {
    /// Borrows the retained file.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Mutably borrows the retained file.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Returns the admitted logical size.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read.
    pub fn len(&self) -> io::Result<u64> {
        self.file.metadata().map(|metadata| metadata.len())
    }

    /// Returns whether the retained file is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read.
    pub fn is_empty(&self) -> io::Result<bool> {
        self.len().map(|len| len == 0)
    }

    /// Revalidates the file and every retained ancestor.
    ///
    /// # Errors
    ///
    /// Returns an error when the leaf or ancestry has been substituted.
    pub fn validate_attachment(&self) -> io::Result<()> {
        self.directory.validate()?;
        self.directory
            .inner
            .validate_entry(&self.name, self.identity)
    }

    /// Verifies that two retained handles refer to the same file.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::PermissionDenied`] on identity mismatch.
    pub fn ensure_same_file(&self, other: &Self) -> io::Result<()> {
        if self.identity == other.identity {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "retained state files do not have the same identity",
            ))
        }
    }

    /// Flushes the retained parent directory.
    ///
    /// # Errors
    ///
    /// Returns an operating-system sync error or an ancestry validation error.
    pub fn sync_parent(&self) -> io::Result<()> {
        self.directory.sync()?;
        self.directory.validate()
    }
}

impl Read for StateFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

fn temporary_name(target: &str) -> String {
    let nonce = PROCESS_NONCE.get_or_init(|| {
        use std::collections::hash_map::RandomState;
        use std::hash::BuildHasher;

        [
            RandomState::new().hash_one(0_u8),
            RandomState::new().hash_one(1_u8),
        ]
    });
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".{target}.tmp-{:016x}{:016x}-{}-{counter:016x}",
        nonce[0],
        nonce[1],
        std::process::id()
    )
}

fn oversized(name: &str, max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("state file '{name}' exceeds {max_bytes} bytes"),
    )
}

mod rotating_log;

pub use rotating_log::{LogRotation, RotatingLog, DEFAULT_MAX_RECORD_BYTES};

#[cfg(unix)]
mod platform;
#[cfg(not(unix))]
#[path = "platform_fallback.rs"]
mod platform;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn bounded_read_rejects_an_oversized_sparse_file_before_allocation() {
        let root = tempdir().unwrap();
        let state = StateDir::open(root.path(), &[".packet28"], true).unwrap();
        let path = state.path().join("oversized");
        let file = File::create(path).unwrap();
        file.set_len(1_048_577).unwrap();

        let error = state.read_bounded("oversized", 1_048_576).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn retained_directory_leases_contend_even_between_clones() {
        let root = tempdir().unwrap();
        let state = StateDir::open(root.path(), &[".packet28"], true).unwrap();
        let lease = state.lock_exclusive().unwrap();

        let error = state
            .clone()
            .try_lock_exclusive()
            .expect_err("a clone must open an independently contending lock description");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(lease);
        let _lease = state.try_lock_exclusive().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_symlink_parent_never_redirects_an_atomic_write() {
        let root = tempdir().unwrap();
        let victim = tempdir().unwrap();
        fs::write(victim.path().join("manifest"), b"victim").unwrap();
        symlink(victim.path(), root.path().join(".packet28")).unwrap();

        let error = StateDir::open(root.path(), &[".packet28"], true).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(fs::read(victim.path().join("manifest")).unwrap(), b"victim");
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_swap_after_open_never_redirects_an_atomic_write() {
        let root = tempdir().unwrap();
        let victim = tempdir().unwrap();
        let state = StateDir::open(root.path(), &[".packet28", "index"], true).unwrap();
        let held = root.path().join("held-packet28");
        fs::write(victim.path().join("manifest"), b"victim").unwrap();
        fs::rename(root.path().join(".packet28"), &held).unwrap();
        symlink(victim.path(), root.path().join(".packet28")).unwrap();

        let error = state.write_atomic("manifest", b"replacement").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(fs::read(victim.path().join("manifest")).unwrap(), b"victim");
        assert!(!held.join("index/manifest").exists());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_a_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let root = tempdir().unwrap();
        let state = StateDir::open(root.path(), &[".packet28"], true).unwrap();
        let path = state.path().join("fifo");
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a live NUL-terminated pathname and no pointer is retained.
        assert_eq!(unsafe { libc_for_test::mkfifo(path.as_ptr(), 0o600) }, 0);

        let error = state.read_bounded("fifo", 1024).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    mod libc_for_test {
        use std::os::raw::{c_char, c_int, c_uint};

        extern "C" {
            pub fn mkfifo(path: *const c_char, mode: c_uint) -> c_int;
        }
    }
}
