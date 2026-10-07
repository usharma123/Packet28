//! Size-bounded, process-owned diagnostic log files.
//!
//! [`RotatingLog`] owns one active log leaf beneath a retained [`StateDir`]
//! and rotates it in-process, while it is running, into numbered backups
//! (`name.1` .. `name.N`). The owning process performs every rename itself and
//! reopens its own handle afterward, so no descriptor is left appending to a
//! renamed generation.
//!
//! Every owner of the same log, in this or any other process, serializes the
//! complete reattach, size check, rotation and write transaction through an
//! advisory lock on a stable sidecar leaf (`name.lock`). The sidecar is opened
//! descriptor-relative without following symlinks, is never renamed or
//! removed, and is re-authenticated against its directory entry after each
//! acquisition. Acquisition is nonblocking with a short bounded retry, so a
//! stalled owner delays diagnostics, never the process: a record that cannot
//! be serialized in time is dropped with [`io::ErrorKind::WouldBlock`].
//!
//! Each write is bounded independently of its input: a record is truncated
//! to the configured record ceiling, and a raw [`Write`] call accepts at most
//! that many bytes. Generations that already exceed the threshold, such as
//! legacy logs or logs written under a larger threshold, are reduced to a
//! marked recent tail when an owner first attaches and after each rotation.
//! The active file and every backup therefore stay within `max_bytes`, and
//! retained logs within `max_bytes * (backups + 1)`, plus an empty sidecar.
//!
//! Logging is best-effort by contract. A failed rotation falls back to
//! truncating the active file in place, and the recorded notice shares the
//! record's byte budget; if that also fails the record is dropped. Errors are
//! returned to the caller but are never retried through another sink.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::{FileAccess, StateDir, StateFile};

/// Default ceiling for one appended record.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024;

const TRUNCATED_MARKER: &[u8] = b" ... [truncated]\n";
/// Longest wait for exclusive access before an ordinary record is dropped.
const WRITE_LOCK_WAIT: Duration = Duration::from_millis(250);
/// Longest wait for exclusive access from a panic hook.
const PANIC_LOCK_WAIT: Duration = Duration::from_millis(50);
const MAX_LOCK_BACKOFF: Duration = Duration::from_millis(1);
/// Bytes examined after a tail cut to resume at the next line boundary.
const TAIL_ALIGN_WINDOW: usize = 4096;

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

    fn file_limit(&self) -> usize {
        usize::try_from(self.max_bytes).unwrap_or(usize::MAX)
    }

    fn record_limit(&self) -> usize {
        self.max_record_bytes.min(self.file_limit()).max(1)
    }
}

/// A process-owned log file rotated by size during the process lifetime.
///
/// Writers in one process are serialized by an internal mutex, and owners in
/// separate processes by the sidecar lock. Before each write the retained
/// handle is checked against the directory entry and reopened if another
/// owner rotated or removed it.
#[derive(Debug)]
pub struct RotatingLog {
    directory: StateDir,
    name: String,
    lock_name: String,
    policy: LogRotation,
    state: Mutex<Handles>,
}

#[derive(Debug, Default)]
struct Handles {
    active: Option<StateFile>,
    lock: Option<StateFile>,
    retention_enforced: bool,
}

impl RotatingLog {
    /// Creates a log owner for `name` beneath `directory`.
    ///
    /// The active file and sidecar lock are opened lazily on the first write,
    /// so construction succeeds even while the leaf is temporarily
    /// unavailable.
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
            lock_name: format!("{name}.lock"),
            policy,
            state: Mutex::new(Handles::default()),
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

    /// Reduces existing generations that exceed the size threshold.
    ///
    /// Each oversized generation keeps only its most recent bytes, starting
    /// at a line boundary where one is near, behind a marker line recording
    /// how much older output was discarded. This also runs before an owner's
    /// first write; calling it at startup bounds a quiet process's logs.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when exclusive access is not
    /// obtained in time, or the first error reducing a generation.
    pub fn enforce_retention(&self) -> io::Result<()> {
        self.transact(Wait::Bounded(WRITE_LOCK_WAIT), |handles| {
            handles.retention_enforced = true;
            self.enforce_generations(handles, true)
        })
    }

    /// Appends one newline-terminated record.
    ///
    /// A record longer than the policy's record ceiling is cut at a UTF-8
    /// boundary and marked as truncated.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when exclusive access is not
    /// obtained in time, or the underlying I/O error.
    pub fn append_record(&self, record: &str) -> io::Result<()> {
        let bytes = bounded_record(record, self.policy.record_limit());
        self.transact(Wait::Bounded(WRITE_LOCK_WAIT), |handles| {
            self.write_locked(handles, &bytes)
        })
    }

    /// Variant of [`Self::append_record`] for panic hooks.
    ///
    /// Exclusive access is awaited for a shorter bound, so a panic raised
    /// while the same thread holds the log drops the record instead of
    /// deadlocking.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when the lock stays unavailable,
    /// or the underlying I/O error.
    pub fn try_append_record(&self, record: &str) -> io::Result<()> {
        let bytes = bounded_record(record, self.policy.record_limit());
        self.transact(Wait::Panic(PANIC_LOCK_WAIT), |handles| {
            self.write_locked(handles, &bytes)
        })
    }

    /// Runs `body` while holding both the in-process mutex and the
    /// cross-process sidecar lock.
    ///
    /// Ordinary writers queue on the in-process mutex; every holder leaves
    /// it by its own deadline plus one bounded transaction, so the queue
    /// stays bounded. The panic path only polls the mutex, because the
    /// panicking thread may already hold it.
    fn transact<T>(
        &self,
        wait: Wait,
        body: impl FnOnce(&mut Handles) -> io::Result<T>,
    ) -> io::Result<T> {
        let (mut handles, deadline) = match wait {
            Wait::Bounded(limit) => {
                let handles = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (handles, Instant::now() + limit)
            }
            Wait::Panic(limit) => {
                let deadline = Instant::now() + limit;
                (poll_lock(&self.state, deadline)?, deadline)
            }
        };
        let lock = self.acquire_owner_lock(&mut handles, deadline)?;
        let result = body(&mut handles);
        release_owner_lock(&lock);
        handles.lock = Some(lock);
        result
    }

    /// Acquires the sidecar lock and authenticates it against its entry.
    ///
    /// The returned handle is locked. A panic in the transaction drops it,
    /// closing the descriptor and releasing the lock.
    fn acquire_owner_lock(
        &self,
        handles: &mut Handles,
        deadline: Instant,
    ) -> io::Result<StateFile> {
        let mut backoff = Duration::from_millis(1);
        loop {
            let lock = match handles.lock.take() {
                Some(lock) => lock,
                None => {
                    self.directory
                        .open_or_create(&self.lock_name, FileAccess::ReadWrite)?
                        .file
                }
            };
            match try_lock_owner(&lock) {
                Ok(()) => {
                    // A lock on a leaf no longer attached to the sidecar
                    // name serializes nothing; reopen the current entry.
                    if lock.validate_attachment().is_ok() {
                        return Ok(lock);
                    }
                    release_owner_lock(&lock);
                    drop(lock);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    handles.lock = Some(lock);
                }
                Err(error) => {
                    handles.lock = Some(lock);
                    return Err(error);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "log owner lock remained unavailable",
                ));
            }
            std::thread::sleep(backoff.min(deadline - now));
            backoff = (backoff * 2).min(MAX_LOCK_BACKOFF);
        }
    }

    fn write_locked(&self, handles: &mut Handles, bytes: &[u8]) -> io::Result<()> {
        if !handles.retention_enforced {
            // Best-effort: an unreducible generation never blocks logging.
            handles.retention_enforced = true;
            let _ = self.enforce_generations(handles, true);
        }
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let mut len = self.attached(&mut handles.active)?.len()?;
        if len > self.policy.max_bytes {
            // Another writer (an earlier binary or a larger threshold) left
            // the active file oversized; keep its tail before rotating it.
            if self.reduce_to_tail(&self.name).is_ok() {
                handles.active = None;
                len = self.attached(&mut handles.active)?.len()?;
            }
        }
        let mut rotation_error = None;
        if len > 0 && len.saturating_add(incoming) > self.policy.max_bytes {
            match self.rotate_entries() {
                Ok(()) => {
                    handles.active = None;
                    let _ = self.enforce_generations(handles, false);
                    len = self.attached(&mut handles.active)?.len()?;
                }
                Err(error) => rotation_error = Some(error),
            }
        }
        let file = self.attached(&mut handles.active)?;
        let result = if len.saturating_add(incoming) > self.policy.max_bytes {
            // Rotation failed (or the fresh file is unexpectedly non-empty):
            // truncate in place, keeping notice and record within budget.
            let reason = rotation_error
                .map_or_else(|| "active file not empty".to_string(), |e| e.to_string());
            let notice = format!(
                "[log] rotation of '{}' failed ({reason}); truncated in place\n",
                self.name
            );
            let payload = fit_with_notice(notice.as_bytes(), bytes, self.policy.file_limit());
            file.file()
                .set_len(0)
                .and_then(|()| file.file_mut().write_all(&payload))
        } else {
            file.file_mut().write_all(bytes)
        };
        if result.is_err() {
            handles.active = None;
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

    /// Reduces every oversized backup and, when `include_active`, the active
    /// file. Returns the first failure after attempting every generation.
    fn enforce_generations(&self, handles: &mut Handles, include_active: bool) -> io::Result<()> {
        let mut first_error = None;
        let backups = (1..=self.policy.backups).map(|index| self.backup_name(index));
        let names = include_active
            .then(|| self.name.clone())
            .into_iter()
            .chain(backups);
        for name in names {
            match self.reduce_to_tail(&name) {
                Ok(true) if name == self.name => handles.active = None,
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Atomically replaces `name` with its marked recent tail when it exceeds
    /// the threshold. Copies at most `max_bytes` through a fixed buffer, so a
    /// multi-gigabyte legacy log costs no proportional memory or reading.
    ///
    /// Returns whether the generation was replaced.
    fn reduce_to_tail(&self, name: &str) -> io::Result<bool> {
        let Some(mut source) = self.directory.open_existing(name, FileAccess::ReadOnly)? else {
            return Ok(false);
        };
        let len = source.len()?;
        if len <= self.policy.max_bytes {
            return Ok(false);
        }
        let limit = self.policy.max_bytes;
        let mut header = format!(
            "[log] older diagnostics discarded: kept at most the last {limit} of {len} bytes\n"
        )
        .into_bytes();
        if u64::try_from(header.len()).unwrap_or(u64::MAX) >= limit / 2 {
            header = b"[log truncated]\n".to_vec();
            if u64::try_from(header.len()).unwrap_or(u64::MAX) >= limit / 2 {
                header.clear();
            }
        }
        let budget = limit - u64::try_from(header.len()).unwrap_or(0);
        source.file_mut().seek(SeekFrom::Start(len - budget))?;
        // Resume at the next line when one starts close to the cut.
        let window = usize::try_from(budget)
            .unwrap_or(usize::MAX)
            .min(TAIL_ALIGN_WINDOW);
        let mut head = vec![0; window];
        let mut filled = 0;
        while filled < window {
            match source.file_mut().read(&mut head[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        head.truncate(filled);
        let skip = match head.iter().position(|&byte| byte == b'\n') {
            Some(newline) if newline + 1 < head.len() => newline + 1,
            _ => 0,
        };
        let remaining = budget - u64::try_from(filled).unwrap_or(budget);
        self.directory.write_atomic_stream(name, |target| {
            target.write_all(&header)?;
            target.write_all(&head[skip..])?;
            let copied = io::copy(&mut source.file_mut().take(remaining), target)?;
            debug_assert!(copied <= remaining);
            Ok(())
        })?;
        Ok(true)
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
        self.transact(Wait::Bounded(WRITE_LOCK_WAIT), |handles| {
            self.write_locked(handles, &buffer[..accepted])
        })?;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Wait {
    /// Block on the in-process mutex, then wait this long for the owner lock.
    Bounded(Duration),
    /// Poll both locks for at most this long in total.
    Panic(Duration),
}

/// Acquires `mutex` by polling until `deadline`, recovering from poisoning.
fn poll_lock<T>(mutex: &Mutex<T>, deadline: Instant) -> io::Result<MutexGuard<'_, T>> {
    let mut backoff = Duration::from_micros(100);
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "log lock remained unavailable",
            ));
        }
        std::thread::sleep(backoff.min(deadline - now));
        backoff = (backoff * 2).min(MAX_LOCK_BACKOFF);
    }
}

#[cfg(unix)]
fn try_lock_owner(lock: &StateFile) -> io::Result<()> {
    fs2::FileExt::try_lock_exclusive(lock.file()).map_err(|error| {
        if error.kind() == fs2::lock_contended_error().kind() {
            io::Error::new(io::ErrorKind::WouldBlock, error)
        } else {
            error
        }
    })
}

#[cfg(unix)]
fn release_owner_lock(lock: &StateFile) {
    let _ = fs2::FileExt::unlock(lock.file());
}

/// Platforms without advisory file locks serialize only within a process.
#[cfg(not(unix))]
fn try_lock_owner(_lock: &StateFile) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn release_owner_lock(_lock: &StateFile) {}

/// Builds the in-place truncation payload within `limit` bytes.
///
/// The record keeps priority: it is shortened (and marked) only to make room
/// for the notice, and the notice is omitted when both cannot be legible.
fn fit_with_notice(notice: &[u8], record: &[u8], limit: usize) -> Vec<u8> {
    if notice.len().saturating_add(record.len()) <= limit {
        return [notice, record].concat();
    }
    let room = limit.saturating_sub(notice.len());
    if room > TRUNCATED_MARKER.len() && notice.len() <= limit / 2 {
        let mut payload = notice.to_vec();
        payload.extend_from_slice(&truncate_marked(record, room));
        return payload;
    }
    truncate_marked(record, limit)
}

fn bounded_record(record: &str, limit: usize) -> Vec<u8> {
    let record = record.strip_suffix('\n').unwrap_or(record);
    if record.len() < limit {
        let mut bytes = Vec::with_capacity(record.len() + 1);
        bytes.extend_from_slice(record.as_bytes());
        bytes.push(b'\n');
        return bytes;
    }
    cut_marked(record.as_bytes(), limit)
}

/// Returns `bytes` unchanged when they fit in `limit`, or [`cut_marked`].
fn truncate_marked(bytes: &[u8], limit: usize) -> Vec<u8> {
    if bytes.len() <= limit {
        bytes.to_vec()
    } else {
        cut_marked(bytes, limit)
    }
}

/// Cuts `bytes` to at most `limit`, ending with the truncation marker (or a
/// newline when the marker does not fit). The cut never splits a UTF-8
/// sequence.
fn cut_marked(bytes: &[u8], limit: usize) -> Vec<u8> {
    let marker: &[u8] = if TRUNCATED_MARKER.len() < limit {
        TRUNCATED_MARKER
    } else {
        b"\n"
    };
    let mut end = limit.saturating_sub(marker.len());
    while end > 0 && bytes.get(end).copied().is_some_and(is_utf8_continuation) {
        end -= 1;
    }
    let mut cut = Vec::with_capacity(end + marker.len());
    cut.extend_from_slice(&bytes[..end]);
    cut.extend_from_slice(marker);
    cut.truncate(limit);
    cut
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

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
        assert!(text.ends_with(" ... [truncated]\n"), "{text:?}");
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

    fn backup_path(log: &RotatingLog, index: usize) -> std::path::PathBuf {
        let mut backup = log.path().into_os_string();
        backup.push(format!(".{index}"));
        backup.into()
    }

    fn assert_generations_within(log: &RotatingLog, limit: u64) {
        let sizes = generations(log);
        for size in &sizes[..=log.policy().backups] {
            assert!(size.is_none_or(|size| size <= limit), "{sizes:?} > {limit}");
        }
        assert_eq!(sizes[log.policy().backups + 1], None, "{sizes:?}");
    }

    #[cfg(unix)]
    #[test]
    fn rotation_failure_budgets_the_notice_with_a_full_capacity_record() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(64, 3));
        log.append_record("seed").unwrap();
        let daemon_dir = log.path().parent().unwrap().to_path_buf();
        fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let full = "F".repeat(200);
        let outcome = log.append_record(&full);
        let size = fs::metadata(log.path()).map(|m| m.len());
        let contents = fs::read(log.path());
        (&log).write_all(&[b'r'; 64]).unwrap_or(());
        let raw_size = fs::metadata(log.path()).map(|m| m.len());
        fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();

        outcome.unwrap();
        assert!(size.unwrap() <= 64, "notice plus record: {contents:?}");
        assert!(raw_size.unwrap() <= 64, "notice plus raw write");
        let contents = String::from_utf8(contents.unwrap()).unwrap();
        assert!(
            contents.contains("[truncated]") || contents.contains("truncated in place"),
            "{contents:?}"
        );
    }

    /// Samples every generation until `done` returns true and returns the
    /// largest size seen, so a transient overshoot is still observed.
    fn peak_while(log: &RotatingLog, mut done: impl FnMut() -> bool) -> u64 {
        let mut peak = 0;
        loop {
            let finished = done();
            for size in generations(log).into_iter().flatten() {
                peak = peak.max(size);
            }
            if finished {
                return peak;
            }
        }
    }

    #[test]
    fn independent_owners_in_one_process_never_exceed_the_bound() {
        let root = tempdir().unwrap();
        let owners = 6;
        let barrier = Arc::new(Barrier::new(owners + 1));
        let workers = (0..owners)
            .map(|owner| {
                let root = root.path().to_path_buf();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    // Each owner has its own directory capability, handle and
                    // mutex, as separate daemons would.
                    let log = open_log(&root, LogRotation::new(1024, 3));
                    barrier.wait();
                    let mut failures = 0;
                    for index in 0..400 {
                        let body = format!("owner {owner} record {index:04} ");
                        let record = format!("{body}{}", "o".repeat(100 - body.len()));
                        if let Err(error) = log.append_record(&record) {
                            // Only bounded lock waits may drop a record.
                            assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
                            failures += 1;
                        }
                    }
                    failures
                })
            })
            .collect::<Vec<_>>();
        let log = open_log(root.path(), LogRotation::new(1024, 3));
        barrier.wait();
        let peak = peak_while(&log, || workers.iter().all(|worker| worker.is_finished()));
        let failures = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum::<usize>();
        eprintln!("independent owners: {failures} of 2400 records timed out");

        assert!(peak <= 1024, "observed a {peak}-byte generation");
        assert_generations_within(&log, 1024);
        // Writers here loop without pause, so the bounded nonblocking wait
        // can rarely time out; it must stay a rare, explicit drop.
        assert!(failures * 50 < owners * 400, "{failures} records dropped");
        for index in 0..=3 {
            let path = if index == 0 {
                log.path()
            } else {
                backup_path(&log, index)
            };
            for line in fs::read_to_string(path).unwrap().lines() {
                assert_eq!(line.len(), 100, "torn record: {line:?}");
            }
        }
    }

    const CHILD_ROOT_ENV: &str = "PACKET28_STATE_FS_LOG_CHILD_ROOT";

    /// Writer body for the independent-process regression; inert otherwise.
    #[test]
    fn independent_process_writer_child() {
        let Some(root) = std::env::var_os(CHILD_ROOT_ENV) else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let log = open_log(&root, LogRotation::new(1024, 3));
        let start = std::time::Instant::now();
        while !root.join("go").exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(30));
            std::thread::yield_now();
        }
        let pid = std::process::id();
        let mut failures = 0;
        for index in 0..1500 {
            let body = format!("pid {pid} record {index:04} ");
            let record = format!("{body}{}", "p".repeat(100 - body.len()));
            if let Err(error) = log.append_record(&record) {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
                failures += 1;
            }
        }
        fs::write(root.join(format!("failures-{pid}")), failures.to_string()).unwrap();
    }

    #[test]
    fn independent_processes_never_exceed_the_bound() {
        let root = tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = (0..6)
            .map(|_| {
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "rotating_log::tests::independent_process_writer_child",
                        "--test-threads=1",
                        "--quiet",
                    ])
                    .env(CHILD_ROOT_ENV, root.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let log = open_log(root.path(), LogRotation::new(1024, 3));
        std::thread::sleep(std::time::Duration::from_millis(300));
        fs::write(root.path().join("go"), b"").unwrap();
        let mut statuses = Vec::new();
        let peak = peak_while(&log, || {
            statuses = children
                .iter_mut()
                .filter_map(|child| child.try_wait().unwrap())
                .collect();
            statuses.len() == children.len()
        });

        assert!(peak <= 1024, "observed a {peak}-byte generation");
        assert_generations_within(&log, 1024);
        assert!(
            statuses.iter().all(|status| status.success()),
            "{statuses:?}"
        );
        let mut failures = 0;
        for entry in fs::read_dir(root.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with("failures-") {
                failures += fs::read_to_string(entry.path())
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
            }
        }
        assert!(failures * 50 < 6 * 1500, "{failures} records dropped");
        eprintln!("independent processes: {failures} of 9000 records timed out");
        let newest = fs::read_to_string(log.path()).unwrap();
        assert!(newest.contains("record 1499"), "{newest}");
    }

    #[test]
    fn preexisting_oversized_generations_keep_only_a_bounded_recent_tail() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(1024, 3));
        let mut legacy = String::new();
        for index in 0..4000 {
            legacy.push_str(&format!("legacy active line {index:05}\n"));
        }
        fs::write(log.path(), &legacy).unwrap();
        fs::write(backup_path(&log, 2), vec![b'B'; 256 * 1024]).unwrap();

        log.append_record("fresh").unwrap();

        assert_generations_within(&log, 1024);
        let active = fs::read_to_string(log.path()).unwrap();
        assert!(active.ends_with("fresh\n"), "{active}");
        // The marked recent tail is retained ahead of the new record, either
        // in the active file or rotated into `.1`.
        let tail = if active == "fresh\n" {
            fs::read_to_string(backup_path(&log, 1)).unwrap()
        } else {
            active.strip_suffix("fresh\n").unwrap().to_string()
        };
        assert!(
            tail.starts_with("[log] older diagnostics discarded"),
            "{tail}"
        );
        assert!(tail.ends_with("legacy active line 03999\n"), "{tail}");
        let body = tail.split_once('\n').unwrap().1;
        assert!(
            body.starts_with("legacy active line "),
            "tail starts at a line: {body}"
        );
        let backup = fs::read_to_string(backup_path(&log, 2)).unwrap();
        assert!(backup.starts_with("[log] older diagnostics discarded"));
        assert!(
            backup.ends_with("BBBB"),
            "most recent backup bytes retained"
        );
    }

    #[test]
    fn a_lowered_threshold_reduces_existing_generations() {
        let root = tempdir().unwrap();
        let wide = open_log(root.path(), LogRotation::new(8 * 1024, 3));
        for index in 0..600 {
            wide.append_record(&format!("wide record {index:04} {}", "w".repeat(48)))
                .unwrap();
        }
        assert!(generations(&wide)[3].is_some_and(|size| size > 1024));
        drop(wide);

        let narrow = open_log(root.path(), LogRotation::new(1024, 3));
        narrow.append_record("narrow").unwrap();

        assert_generations_within(&narrow, 1024);
    }

    #[test]
    fn retention_reduces_a_quiet_log_without_writing_a_record() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        fs::write(log.path(), vec![b'A'; 3 * 1024 * 1024]).unwrap();
        fs::write(backup_path(&log, 3), vec![b'C'; 5000]).unwrap();

        log.enforce_retention().unwrap();

        assert_generations_within(&log, 4096);
        let active = fs::read(log.path()).unwrap();
        assert!(active.starts_with(b"[log] older diagnostics discarded"));
        assert!(active.ends_with(b"AAAA"));
        assert_eq!(fs::read(backup_path(&log, 1)).ok(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_owner_delays_diagnostics_only_for_a_bounded_time() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        log.append_record("before the stall").unwrap();
        let mut sidecar = log.path().into_os_string();
        sidecar.push(".lock");
        let stalled = fs::File::open(&sidecar).unwrap();
        fs2::FileExt::lock_exclusive(&stalled).unwrap();

        let started = Instant::now();
        let error = log.append_record("during the stall").unwrap_err();
        let panic_error = log.try_append_record("panic during the stall").unwrap_err();
        let waited = started.elapsed();
        fs2::FileExt::unlock(&stalled).unwrap();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(panic_error.kind(), io::ErrorKind::WouldBlock);
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        log.append_record("after the stall").unwrap();
        let contents = fs::read_to_string(log.path()).unwrap();
        assert_eq!(contents, "before the stall\nafter the stall\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_substituted_sidecar_is_never_followed() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        let elsewhere = root.path().join("elsewhere");
        fs::write(&elsewhere, b"").unwrap();
        let mut sidecar = log.path().into_os_string();
        sidecar.push(".lock");
        std::os::unix::fs::symlink(&elsewhere, &sidecar).unwrap();

        assert!(log.append_record("refused").is_err());

        assert_eq!(fs::read(&elsewhere).unwrap(), b"");
        assert!(fs::symlink_metadata(&sidecar)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::read(log.path()).is_err(), "no unserialized write");
    }

    #[test]
    fn panic_path_does_not_deadlock_while_the_lock_is_held() {
        let root = tempdir().unwrap();
        let log = open_log(root.path(), LogRotation::new(4096, 3));
        let held = log.state.lock().unwrap();

        let error = log.try_append_record("panic while logging").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        log.try_append_record("panic after release").unwrap();
    }
}
