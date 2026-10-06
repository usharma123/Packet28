//! Size-bounded, process-owned diagnostic log files.
//!
//! [`RotatingLog`] owns one active log leaf beneath a retained [`StateDir`]
//! and rotates it in-process, while it is running, into numbered backups
//! (`name.1` .. `name.N`). The owning process performs every rename itself and
//! reopens its own handle afterward, so no descriptor is left appending to a
//! renamed generation.
//!
//! Each write is bounded independently of its input: a record is truncated
//! to the configured record ceiling, and a raw [`Write`] call accepts at most
//! that many bytes. The active file therefore never exceeds `max_bytes` after
//! this owner writes to it, and retained logs are bounded by
//! `max_bytes * (backups + 1)`.
//!
//! Logging is best-effort by contract. A failed rotation falls back to
//! truncating the active file in place; if that also fails the record is
//! dropped. Errors are returned to the caller but are never retried through
//! another sink.

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard, TryLockError};

use crate::{FileAccess, StateDir, StateFile};

/// Default ceiling for one appended record.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024;

const TRUNCATED_MARKER: &str = " ... [truncated]\n";
const PANIC_LOCK_ATTEMPTS: usize = 1024;

/// Size and retention policy for a [`RotatingLog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRotation {
    /// Largest size, in bytes, of the active file and of each backup.
    pub max_bytes: u64,
    /// Number of numbered backups retained beside the active file.
    pub backups: usize,
    /// Largest number of bytes accepted for one record or raw write.
    pub max_record_bytes: usize,
}

impl LogRotation {
    /// Creates a policy with the default record ceiling.
    pub fn new(max_bytes: u64, backups: usize) -> Self {
        Self {
            max_bytes,
            backups,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
        }
    }

    fn record_limit(&self) -> usize {
        let file_limit = usize::try_from(self.max_bytes).unwrap_or(usize::MAX);
        self.max_record_bytes.min(file_limit).max(1)
    }
}

/// A process-owned log file rotated by size during the process lifetime.
///
/// All writes from one process are serialized by an internal mutex. Before
/// each write the retained handle is checked against the directory entry and
/// reopened if another process rotated or removed it.
#[derive(Debug)]
pub struct RotatingLog {
    directory: StateDir,
    name: String,
    policy: LogRotation,
    active: Mutex<Option<StateFile>>,
}

impl RotatingLog {
    /// Creates a log owner for `name` beneath `directory`.
    ///
    /// The active file is opened lazily on the first write, so construction
    /// succeeds even while the leaf is temporarily unavailable.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] when `name` is not one
    /// non-special path component or the policy retains no bytes.
    pub fn new(directory: StateDir, name: &str, policy: LogRotation) -> io::Result<Self> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log name must be one non-special component",
            ));
        }
        if policy.max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log rotation size must be positive",
            ));
        }
        Ok(Self {
            directory,
            name: name.to_string(),
            policy,
            active: Mutex::new(None),
        })
    }

    /// Returns the rotation policy.
    pub fn policy(&self) -> LogRotation {
        self.policy
    }

    /// Returns the diagnostic path of the active log file.
    pub fn path(&self) -> std::path::PathBuf {
        self.directory.path().join(&self.name)
    }

    /// Appends one newline-terminated record.
    ///
    /// A record longer than the policy's record ceiling is cut at a UTF-8
    /// boundary and marked as truncated.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the record could not be written.
    pub fn append_record(&self, record: &str) -> io::Result<()> {
        let bytes = bounded_record(record, self.policy.record_limit());
        let mut active = lock(&self.active);
        self.write_locked(&mut active, &bytes)
    }

    /// Variant of [`Self::append_record`] for panic hooks.
    ///
    /// The lock is acquired with bounded retries so a panic raised while the
    /// same thread holds the log lock drops the record instead of deadlocking.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when the lock stays unavailable,
    /// or the underlying I/O error.
    pub fn try_append_record(&self, record: &str) -> io::Result<()> {
        let bytes = bounded_record(record, self.policy.record_limit());
        for _ in 0..PANIC_LOCK_ATTEMPTS {
            match self.active.try_lock() {
                Ok(mut active) => return self.write_locked(&mut active, &bytes),
                Err(TryLockError::Poisoned(poisoned)) => {
                    let mut active = poisoned.into_inner();
                    return self.write_locked(&mut active, &bytes);
                }
                Err(TryLockError::WouldBlock) => std::thread::yield_now(),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "log lock remained unavailable",
        ))
    }

    fn write_locked(&self, active: &mut Option<StateFile>, bytes: &[u8]) -> io::Result<()> {
        let file = self.attached(active)?;
        let len = file.len()?;
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > 0 && len.saturating_add(incoming) > self.policy.max_bytes {
            self.rotate(active)?;
        }
        let file = self.attached(active)?;
        let result = file.file_mut().write_all(bytes);
        if result.is_err() {
            *active = None;
        }
        result
    }

    /// Returns the retained handle, reopening it when it is missing or no
    /// longer attached to the active directory entry.
    fn attached<'a>(&self, active: &'a mut Option<StateFile>) -> io::Result<&'a mut StateFile> {
        if active
            .as_ref()
            .is_some_and(|file| file.validate_attachment().is_err())
        {
            *active = None;
        }
        if active.is_none() {
            let opened = self
                .directory
                .open_or_create(&self.name, FileAccess::Append)?;
            *active = Some(opened.file);
        }
        active
            .as_mut()
            .ok_or_else(|| io::Error::other("log handle unavailable after reopen"))
    }

    /// Shifts backups and moves the active file to `.1`, then reopens.
    ///
    /// When the active file cannot be renamed, it is truncated in place so the
    /// size bound still holds; the truncation is recorded in the log itself.
    fn rotate(&self, active: &mut Option<StateFile>) -> io::Result<()> {
        let rotated = self.rotate_entries();
        match rotated {
            Ok(()) => {
                *active = None;
                self.attached(active).map(|_| ())
            }
            Err(error) => {
                let file = self.attached(active)?;
                file.file().set_len(0)?;
                let notice = format!(
                    "[log] rotation of '{}' failed ({error}); truncated in place\n",
                    self.name
                );
                let notice = bounded_record(&notice, self.policy.record_limit());
                file.file_mut().write_all(&notice)
            }
        }
    }

    fn rotate_entries(&self) -> io::Result<()> {
        self.directory.validate()?;
        let inner = &self.directory.inner;
        if self.policy.backups == 0 {
            return inner.remove_file_if_exists(&self.name);
        }
        // The oldest generation is discarded first. Failures shifting older
        // generations only lose history; they never stop the active rename.
        let _ = inner.remove_file_if_exists(&self.backup_name(self.policy.backups));
        for index in (1..self.policy.backups).rev() {
            match inner.rename(&self.backup_name(index), &self.backup_name(index + 1)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = inner.remove_file_if_exists(&self.backup_name(index));
                }
            }
        }
        inner.rename(&self.name, &self.backup_name(1))?;
        let _ = inner.sync();
        Ok(())
    }

    fn backup_name(&self, index: usize) -> String {
        format!("{}.{index}", self.name)
    }
}

/// Each call accepts at most one record's worth of bytes, rotating as needed,
/// so arbitrarily large input without newlines is still written in bounded
/// pieces.
impl Write for &RotatingLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let accepted = buffer.len().min(self.policy.record_limit());
        let mut active = lock(&self.active);
        self.write_locked(&mut active, &buffer[..accepted])?;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn lock(mutex: &Mutex<Option<StateFile>>) -> MutexGuard<'_, Option<StateFile>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bounded_record(record: &str, limit: usize) -> Vec<u8> {
    let record = record.strip_suffix('\n').unwrap_or(record);
    if record.len() < limit {
        let mut bytes = Vec::with_capacity(record.len() + 1);
        bytes.extend_from_slice(record.as_bytes());
        bytes.push(b'\n');
        return bytes;
    }
    let marker = if TRUNCATED_MARKER.len() < limit {
        TRUNCATED_MARKER
    } else {
        "\n"
    };
    let mut end = limit.saturating_sub(marker.len());
    while end > 0 && !record.is_char_boundary(end) {
        end -= 1;
    }
    let mut bytes = Vec::with_capacity(end + marker.len());
    bytes.extend_from_slice(&record.as_bytes()[..end]);
    bytes.extend_from_slice(marker.as_bytes());
    bytes.truncate(limit);
    bytes
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Barrier};

    use super::*;
    use tempfile::tempdir;

    fn open_log(root: &std::path::Path, policy: LogRotation) -> RotatingLog {
        let directory = StateDir::open(root, &[".packet28", "daemon"], true).unwrap();
        RotatingLog::new(directory, "service.log", policy).unwrap()
    }

    fn generations(log: &RotatingLog) -> Vec<Option<u64>> {
        let active = log.path();
        let mut sizes = vec![fs::metadata(&active).ok().map(|m| m.len())];
        for index in 1..=log.policy().backups + 1 {
            let mut name = active.file_name().unwrap().to_os_string();
            name.push(format!(".{index}"));
            sizes.push(
                fs::metadata(active.with_file_name(name))
                    .ok()
                    .map(|m| m.len()),
            );
        }
        sizes
    }

    #[test]
    fn rotates_repeatedly_within_one_process_and_keeps_bounded_backups() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(256, 3));
        for index in 0..200 {
            log.append_record(&format!("record {index:04} {}", "x".repeat(40)))
                .unwrap();
        }
        let sizes = generations(&log);
        for size in &sizes[..4] {
            let size = size.expect("active file and three backups exist");
            assert!(size > 0 && size <= 256, "{sizes:?}");
        }
        assert_eq!(sizes[4], None, "a fourth backup must never be kept");
        let newest = fs::read_to_string(log.path()).unwrap();
        assert!(newest.contains("record 0199"), "{newest}");
        let mut backup = log.path().into_os_string();
        backup.push(".1");
        let previous = fs::read_to_string(backup).unwrap();
        assert!(!previous.is_empty() && !previous.contains("record 0199"));
    }

    #[test]
    fn huge_record_without_newline_is_bounded_and_marked() {
        let root = tempdir().unwrap();
        let mut policy = LogRotation::new(4096, 3);
        policy.max_record_bytes = 1024;
        let log = open_log(root.path(), policy);
        let huge = "é".repeat(4 * 1024 * 1024);

        log.append_record(&huge).unwrap();

        let written = fs::read(log.path()).unwrap();
        assert!(written.len() <= 1024, "{}", written.len());
        let text = String::from_utf8(written).expect("truncation keeps UTF-8 boundaries");
        assert!(text.ends_with(TRUNCATED_MARKER), "{text:?}");
    }

    #[test]
    fn raw_writes_of_unbounded_input_are_chunked_through_rotation() {
        let root = tempdir().unwrap();
        let mut policy = LogRotation::new(2048, 2);
        policy.max_record_bytes = 512;
        let log = open_log(root.path(), policy);
        let huge = vec![b'z'; 8 * 1024 * 1024];

        let accepted = (&log).write(&huge).unwrap();
        assert_eq!(accepted, 512);
        (&log).write_all(&huge[..64 * 1024]).unwrap();

        let sizes = generations(&log);
        for size in &sizes[..3] {
            assert!(size.is_some_and(|size| size <= 2048), "{sizes:?}");
        }
        assert_eq!(sizes[3], None, "{sizes:?}");
    }

    #[test]
    fn concurrent_writers_never_interleave_or_exceed_bounds() {
        let root = tempdir().unwrap();
        let log = Arc::new(open_log(root.path(), LogRotation::new(8 * 1024, 3)));
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let workers = (0..threads)
            .map(|thread| {
                let log = Arc::clone(&log);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for index in 0..500 {
                        let body = format!("{thread}:{index}:");
                        log.append_record(&format!("{body}{}", "k".repeat(64 - body.len())))
                            .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let sizes = generations(&log);
        for size in &sizes[..4] {
            assert!(size.is_some_and(|size| size <= 8 * 1024), "{sizes:?}");
        }
        assert_eq!(sizes[4], None);
        for line in fs::read_to_string(log.path()).unwrap().lines() {
            assert_eq!(line.len(), 64, "interleaved record: {line:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rotation_failure_truncates_in_place_and_stays_bounded() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(512, 3));
        log.append_record("before the directory became read-only")
            .unwrap();
        let daemon_dir = log.path().parent().unwrap().to_path_buf();
        fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let outcome = (0..64)
            .map(|index| log.append_record(&format!("blocked rotation record {index:03}")))
            .collect::<Vec<_>>();
        let size = fs::metadata(log.path()).map(|m| m.len());
        let contents = fs::read_to_string(log.path());
        fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(outcome.iter().all(Result::is_ok), "{outcome:?}");
        assert!(size.unwrap() <= 512);
        let contents = contents.unwrap();
        assert!(contents.contains("truncated in place"), "{contents}");
        assert!(
            contents.contains("blocked rotation record 063"),
            "{contents}"
        );
        let mut backup = log.path().into_os_string();
        backup.push(".1");
        assert!(!std::path::Path::new(&backup).exists());

        // Once the directory is writable again, normal rotation resumes.
        for index in 0..64 {
            log.append_record(&format!("recovered record {index:03}"))
                .unwrap();
        }
        assert!(std::path::Path::new(&backup).exists());
    }

    #[test]
    fn externally_removed_active_file_is_recreated_on_next_write() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        log.append_record("first").unwrap();
        fs::remove_file(log.path()).unwrap();

        log.append_record("second").unwrap();

        assert_eq!(fs::read_to_string(log.path()).unwrap(), "second\n");
    }

    #[test]
    fn legacy_oversized_active_file_is_rotated_before_the_first_write() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(1024, 3));
        fs::write(log.path(), vec![b'L'; 64 * 1024]).unwrap();

        log.append_record("fresh").unwrap();

        assert_eq!(fs::read_to_string(log.path()).unwrap(), "fresh\n");
        let mut backup = log.path().into_os_string();
        backup.push(".1");
        assert_eq!(fs::metadata(backup).unwrap().len(), 64 * 1024);
    }

    #[test]
    fn panic_path_does_not_deadlock_while_the_lock_is_held() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        let held = log.active.lock().unwrap();

        let error = log.try_append_record("panic while logging").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        log.try_append_record("panic after release").unwrap();
    }
}
