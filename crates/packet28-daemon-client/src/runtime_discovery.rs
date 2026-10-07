//! Authenticated, bounded discovery of daemon runtime metadata.
//!
//! Runtime metadata may contain the capability that protects loopback TCP
//! transports. Unix readers therefore authenticate the workspace namespace,
//! retain no-follow directory descriptors for every state lookup, and reject
//! metadata that another effective user could replace while keeping published
//! transport capabilities owner-private.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use packet28_daemon_protocol::message::DaemonRuntimeInfo;
use packet28_daemon_protocol::paths::runtime_path;

const MAX_DAEMON_RUNTIME_INFO_BYTES: usize = 64 * 1024;

/// Error returned while locating or authenticating daemon runtime metadata.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RuntimeDiscoveryError {
    /// A filesystem operation failed or encountered unauthentic state.
    #[error("{operation} `{path}`: {source}")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Runtime or workspace path involved in the failure.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The authenticated runtime file did not contain valid metadata.
    #[error("failed to decode daemon runtime metadata from `{path}`: {source}")]
    Json {
        /// Authenticated runtime metadata path.
        path: PathBuf,
        /// JSON decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// A caller required runtime metadata, but no real state entry existed.
    #[error("daemon runtime metadata does not exist: `{path}`")]
    Missing {
        /// Expected runtime metadata path.
        path: PathBuf,
    },
    /// The authenticated runtime leaf was unlinked while it was being read.
    ///
    /// Bytes read from the detached leaf are discarded, never decoded. A
    /// caller already waiting for a daemon to release workspace authority may
    /// treat this as metadata withdrawn by a stopping owner; it is not
    /// published metadata.
    #[error("daemon runtime metadata was withdrawn during discovery: `{path}`")]
    Withdrawn {
        /// Authenticated runtime metadata path.
        path: PathBuf,
    },
}

impl RuntimeDiscoveryError {
    fn io(
        operation: &'static str,
        path: impl Into<PathBuf>,
        source: io::Error,
    ) -> RuntimeDiscoveryError {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

struct RuntimeRead {
    bytes: Vec<u8>,
    #[cfg(unix)]
    mode: libc::mode_t,
}

/// Outcome of reading the runtime leaf through the platform layer.
enum RuntimeLeaf {
    /// No real runtime state entry exists.
    Absent,
    /// An authenticated leaf was unlinked during the read.
    #[cfg(unix)]
    Withdrawn,
    /// Authenticated bytes of the attached leaf.
    Read(RuntimeRead),
}

/// Loads authenticated daemon runtime metadata when real state entries exist.
///
/// On Unix, the workspace, `.packet28`, `daemon`, and `runtime.json` entries
/// are authenticated through retained no-follow descriptors. A genuinely
/// absent state directory or runtime leaf returns `Ok(None)`. Symlinks,
/// special files, unsafe permissions or ACLs, and state that changes identity
/// during the read return an error and are never treated as absence.
///
/// Non-Unix platforms perform a bounded best-effort regular-file read.
///
/// # Errors
///
/// Returns [`RuntimeDiscoveryError::Io`] when the workspace or runtime state
/// cannot be authenticated, or [`RuntimeDiscoveryError::Json`] when
/// authenticated bytes are not valid [`DaemonRuntimeInfo`]. Returns
/// [`RuntimeDiscoveryError::Withdrawn`] when the authenticated runtime leaf
/// was unlinked during the read, without decoding the detached bytes.
pub fn read_runtime_info_if_present(
    root: &Path,
) -> Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError> {
    decode_runtime(runtime_path(root), platform::read_runtime(root))
}

fn decode_runtime(
    path: PathBuf,
    leaf: io::Result<RuntimeLeaf>,
) -> Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError> {
    let read = match leaf.map_err(|source| {
        RuntimeDiscoveryError::io(
            "failed to read authenticated daemon runtime metadata",
            &path,
            source,
        )
    })? {
        RuntimeLeaf::Absent => return Ok(None),
        #[cfg(unix)]
        RuntimeLeaf::Withdrawn => return Err(RuntimeDiscoveryError::Withdrawn { path }),
        RuntimeLeaf::Read(read) => read,
    };

    let runtime = serde_json::from_slice::<DaemonRuntimeInfo>(&read.bytes).map_err(|source| {
        RuntimeDiscoveryError::Json {
            path: path.clone(),
            source,
        }
    })?;

    #[cfg(unix)]
    if runtime.transport_auth.is_some() && (read.mode & 0o077) != 0 {
        return Err(RuntimeDiscoveryError::io(
            "refused non-owner-private daemon transport capability",
            path,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "runtime metadata containing transport authentication has mode {:o}; \
                     group and other permission bits must be zero",
                    read.mode
                ),
            ),
        ));
    }

    Ok(Some(runtime))
}

/// Loads authenticated daemon runtime metadata and requires it to exist.
///
/// # Errors
///
/// Returns [`RuntimeDiscoveryError::Missing`] when no real state directory or
/// runtime leaf exists. Authentication, I/O, and decoding failures are
/// returned unchanged from [`read_runtime_info_if_present`].
pub fn read_runtime_info(root: &Path) -> Result<DaemonRuntimeInfo, RuntimeDiscoveryError> {
    read_runtime_info_if_present(root)?.ok_or_else(|| RuntimeDiscoveryError::Missing {
        path: runtime_path(root),
    })
}

#[cfg(unix)]
mod platform {
    use std::ffi::{c_int, CString, OsStr};
    use std::fs::File;
    use std::io::Read as _;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};

    use super::{io, RuntimeLeaf, RuntimeRead, MAX_DAEMON_RUNTIME_INFO_BYTES};

    const STATE_DIRECTORY_NAME: &str = ".packet28";
    const DAEMON_DIRECTORY_NAME: &str = "daemon";
    const RUNTIME_FILE_NAME: &str = "runtime.json";
    const DIRECTORY_OPEN_FLAGS: c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    const FILE_OPEN_FLAGS: c_int =
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FileIdentity {
        device: libc::dev_t,
        inode: libc::ino_t,
    }

    struct RetainedDirectory {
        fd: OwnedFd,
        identity: FileIdentity,
        path: PathBuf,
    }

    /// Guarded steps of the runtime leaf read, in order.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum LeafPhase {
        /// Before the preflight name lookup.
        Preflight,
        /// After preflight validation, before the no-follow open.
        Open,
        /// After the open, before the descriptor is validated.
        OpenedValidation,
        /// After the bounded read, before the descriptor is revalidated.
        FinalValidation,
        /// After the final descriptor validation, before the name is
        /// revalidated in the retained daemon directory.
        AttachedValidation,
    }

    /// Link state of a runtime inode that passed every static check.
    enum RuntimeLinks {
        Attached,
        Detached,
    }

    /// Name lookups of the runtime leaf in the retained daemon directory.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum NameLookup {
        /// Preflight lookup before the open.
        Preflight,
        /// Final lookup proving the read leaf is still attached.
        Attached,
        /// Lookup proving a zero-linked descriptor's name was withdrawn.
        Withdrawal,
    }

    /// Observation points around the runtime leaf read.
    ///
    /// Production uses [`Unobserved`]. Tests change the leaf between steps
    /// and model kernel unlink windows, in which a name still resolves to a
    /// zero-linked inode, that userspace cannot hold open.
    pub(super) trait LeafSeam {
        /// Runs before each guarded step.
        fn before(&mut self, phase: LeafPhase, path: &Path) -> io::Result<()>;
        /// Observes the result of a name lookup.
        fn looked_up(&mut self, lookup: NameLookup, stat: &mut Option<libc::stat>);
    }

    struct Unobserved;

    impl LeafSeam for Unobserved {
        fn before(&mut self, _phase: LeafPhase, _path: &Path) -> io::Result<()> {
            Ok(())
        }

        fn looked_up(&mut self, _lookup: NameLookup, _stat: &mut Option<libc::stat>) {}
    }

    /// Validated state of the opened runtime descriptor.
    enum Descriptor {
        Attached(libc::stat),
        Withdrawn,
    }

    pub(super) fn read_runtime(root: &Path) -> io::Result<RuntimeLeaf> {
        read_runtime_with_seam(root, &mut Unobserved)
    }

    #[cfg(test)]
    pub(super) fn read_runtime_with_seam_for_test(
        root: &Path,
        seam: &mut impl LeafSeam,
    ) -> io::Result<RuntimeLeaf> {
        read_runtime_with_seam(root, seam)
    }

    fn read_runtime_with_seam(root: &Path, seam: &mut impl LeafSeam) -> io::Result<RuntimeLeaf> {
        let workspace = open_authenticated_workspace(root)?;
        let Some(state) = open_authenticated_child_directory(&workspace, STATE_DIRECTORY_NAME)?
        else {
            return Ok(RuntimeLeaf::Absent);
        };
        let Some(daemon) = open_authenticated_child_directory(&state, DAEMON_DIRECTORY_NAME)?
        else {
            return Ok(RuntimeLeaf::Absent);
        };
        read_authenticated_runtime_file(&daemon, seam)
    }

    fn open_authenticated_workspace(root: &Path) -> io::Result<RetainedDirectory> {
        let canonical = std::fs::canonicalize(root)?;
        if !canonical.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "canonical workspace path is not absolute: {}",
                    canonical.display()
                ),
            ));
        }

        let mut ancestry = vec![open_absolute_root()?];
        let mut display_path = PathBuf::from("/");
        for component in canonical.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    display_path.push(name);
                    let parent = ancestry.last().ok_or_else(|| {
                        io::Error::other("workspace ancestry unexpectedly became empty")
                    })?;
                    let fd = openat_directory(parent.fd.as_raw_fd(), name)?;
                    let stat = fstat(fd.as_raw_fd())?;
                    require_directory(&stat, &display_path, "workspace namespace component")?;
                    ancestry.push(RetainedDirectory {
                        fd,
                        identity: identity(&stat),
                        path: display_path.clone(),
                    });
                }
                Component::CurDir => {}
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "canonical workspace contains an invalid component: {}",
                            canonical.display()
                        ),
                    ));
                }
            }
        }

        let workspace = ancestry.pop().ok_or_else(|| {
            io::Error::other("canonical workspace traversal produced no directory")
        })?;
        let workspace_stat = fstat(workspace.fd.as_raw_fd())?;
        require_owned_directory(
            &workspace_stat,
            workspace.identity.device,
            &workspace.path,
            "workspace",
        )?;
        require_no_non_owner_write(&workspace_stat, &workspace.path, "workspace")?;
        require_no_namespace_acl(&workspace.fd, &workspace.path, "workspace")?;

        // Validate from the workspace parent toward `/`: sticky directories
        // are safe only while the next child is owned by this effective user.
        let effective_uid = effective_uid();
        let mut child_uid = workspace_stat.st_uid;
        for ancestor in ancestry.iter().rev() {
            let stat = fstat(ancestor.fd.as_raw_fd())?;
            require_directory(&stat, &ancestor.path, "workspace namespace ancestor")?;
            if !namespace_owner_is_trusted(stat.st_uid, effective_uid) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace namespace ancestor is owned by untrusted uid {} and permits \
                         owner replacement of descendants: {}",
                        stat.st_uid,
                        ancestor.path.display()
                    ),
                ));
            }
            require_no_active_namespace_acl(
                &ancestor.fd,
                &ancestor.path,
                "workspace namespace ancestor",
            )?;
            let non_owner_writable = (stat.st_mode & 0o022) != 0;
            let sticky = (stat.st_mode & libc::S_ISVTX) != 0;
            if non_owner_writable && !(sticky && child_uid == effective_uid) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "workspace namespace ancestor permits replacement without safe sticky \
                         ownership semantics: {}",
                        ancestor.path.display()
                    ),
                ));
            }
            child_uid = stat.st_uid;
        }

        Ok(workspace)
    }

    pub(super) fn namespace_owner_is_trusted(
        owner_uid: libc::uid_t,
        effective_uid: libc::uid_t,
    ) -> bool {
        owner_uid == 0 || owner_uid == effective_uid
    }

    fn open_absolute_root() -> io::Result<RetainedDirectory> {
        let root = CString::new("/").map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "root path contains an interior NUL",
            )
        })?;
        // SAFETY: `root` is a live NUL-terminated path, the flags request no
        // creation operation (and therefore require no variadic mode
        // argument), and the returned descriptor is checked before ownership
        // is transferred to `OwnedFd`.
        let raw_fd = unsafe { libc::open(root.as_ptr(), DIRECTORY_OPEN_FLAGS) };
        let fd = owned_fd(raw_fd)?;
        let stat = fstat(fd.as_raw_fd())?;
        require_directory(&stat, Path::new("/"), "filesystem root")?;
        Ok(RetainedDirectory {
            fd,
            identity: identity(&stat),
            path: PathBuf::from("/"),
        })
    }

    fn open_authenticated_child_directory(
        parent: &RetainedDirectory,
        name: &str,
    ) -> io::Result<Option<RetainedDirectory>> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime directory name contains an interior NUL",
            )
        })?;
        let path = parent.path.join(OsStr::from_bytes(name.as_bytes()));
        let Some(preflight) = fstatat_nofollow_if_present(parent.fd.as_raw_fd(), &name)? else {
            return Ok(None);
        };
        validate_state_directory(&preflight, parent.identity.device, &path)?;
        let expected = identity(&preflight);

        let fd = openat_directory_cstr(parent.fd.as_raw_fd(), &name)?;
        let opened = fstat(fd.as_raw_fd())?;
        validate_state_directory(&opened, parent.identity.device, &path)?;
        if identity(&opened) != expected {
            return Err(identity_changed(&path));
        }
        require_empty_acl(&fd, &path, "runtime state directory")?;

        let attached = fstatat_nofollow(parent.fd.as_raw_fd(), &name)?;
        validate_state_directory(&attached, parent.identity.device, &path)?;
        if identity(&attached) != expected {
            return Err(identity_changed(&path));
        }

        Ok(Some(RetainedDirectory {
            fd,
            identity: expected,
            path,
        }))
    }

    fn read_authenticated_runtime_file(
        daemon: &RetainedDirectory,
        seam: &mut impl LeafSeam,
    ) -> io::Result<RuntimeLeaf> {
        let name = CString::new(RUNTIME_FILE_NAME).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime file name contains an interior NUL",
            )
        })?;
        let path = daemon.path.join(RUNTIME_FILE_NAME);
        seam.before(LeafPhase::Preflight, &path)?;
        let Some(preflight) = lookup_leaf(seam, daemon, &name, NameLookup::Preflight)? else {
            return Ok(RuntimeLeaf::Absent);
        };
        // A name can still resolve to an inode whose unlink is in progress.
        // Such a preflight must pass every static check, and the descriptor
        // checks below decide whether it was withdrawn.
        validate_runtime_inode(&preflight, daemon.identity.device, &path)?;
        require_bounded_size(&preflight, &path)?;
        let expected = identity(&preflight);

        seam.before(LeafPhase::Open, &path)?;
        // SAFETY: `name` is live and NUL-terminated, `daemon` retains a valid
        // directory descriptor, the flags request no creation operation (and
        // therefore require no variadic mode argument), and the returned fd is
        // checked before ownership is transferred to `OwnedFd`.
        let raw_fd = unsafe { libc::openat(daemon.fd.as_raw_fd(), name.as_ptr(), FILE_OPEN_FLAGS) };
        if raw_fd < 0 {
            let source = io::Error::last_os_error();
            // The no-follow lookup in the retained daemon directory proves the
            // preflight-authenticated leaf was unlinked; nothing was opened.
            if source.raw_os_error() == Some(libc::ENOENT) {
                return Ok(RuntimeLeaf::Withdrawn);
            }
            return Err(source);
        }
        let fd = owned_fd(raw_fd)?;
        seam.before(LeafPhase::OpenedValidation, &path)?;
        let opened = match validate_runtime_descriptor(seam, &fd, daemon, &name, &path, expected)? {
            Descriptor::Attached(stat) => stat,
            Descriptor::Withdrawn => return Ok(RuntimeLeaf::Withdrawn),
        };

        let initial_capacity = usize::try_from(opened.st_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata length does not fit in memory: {}",
                    path.display()
                ),
            )
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(initial_capacity)
            .map_err(|source| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!(
                        "failed to reserve bounded runtime metadata storage for {}: {source}",
                        path.display()
                    ),
                )
            })?;
        let mut reader = File::from(fd).take(
            u64::try_from(MAX_DAEMON_RUNTIME_INFO_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        );
        reader.read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DAEMON_RUNTIME_INFO_BYTES {
            return Err(oversized(&path));
        }

        let fd = reader.into_inner();
        seam.before(LeafPhase::FinalValidation, &path)?;
        let after = match validate_runtime_descriptor(seam, &fd, daemon, &name, &path, expected)? {
            Descriptor::Attached(stat) => stat,
            Descriptor::Withdrawn => return Ok(RuntimeLeaf::Withdrawn),
        };
        seam.before(LeafPhase::AttachedValidation, &path)?;
        if let Some(attached) = lookup_leaf(seam, daemon, &name, NameLookup::Attached)? {
            let links = validate_runtime_inode(&attached, daemon.identity.device, &path)?;
            require_bounded_size(&attached, &path)?;
            if identity(&attached) != expected {
                return Err(identity_changed(&path));
            }
            if let RuntimeLinks::Attached = links {
                return Ok(RuntimeLeaf::Read(RuntimeRead {
                    bytes,
                    mode: after.st_mode & 0o777,
                }));
            }
        }
        // The name is gone or resolves to the inode whose unlink is in
        // progress. Only an inode unlinked everywhere was withdrawn; one moved
        // to another name still fails closed.
        match validate_runtime_descriptor(seam, &fd, daemon, &name, &path, expected)? {
            Descriptor::Withdrawn => Ok(RuntimeLeaf::Withdrawn),
            Descriptor::Attached(_) => Err(entry_disappeared()),
        }
    }

    fn lookup_leaf(
        seam: &mut impl LeafSeam,
        daemon: &RetainedDirectory,
        name: &CString,
        lookup: NameLookup,
    ) -> io::Result<Option<libc::stat>> {
        let mut stat = fstatat_nofollow_if_present(daemon.fd.as_raw_fd(), name)?;
        seam.looked_up(lookup, &mut stat);
        Ok(stat)
    }

    /// Validates the opened runtime inode against its preflight identity.
    ///
    /// An inode with no remaining links is withdrawn only after it passes
    /// every static check and the retained daemon directory proves the name
    /// absent or still resolving to that same zero-linked inode, so unsafe
    /// metadata or a replacement still fails closed.
    fn validate_runtime_descriptor(
        seam: &mut impl LeafSeam,
        fd: &impl AsRawFd,
        daemon: &RetainedDirectory,
        name: &CString,
        path: &Path,
        expected: FileIdentity,
    ) -> io::Result<Descriptor> {
        let stat = fstat(fd.as_raw_fd())?;
        let links = validate_runtime_inode(&stat, daemon.identity.device, path)?;
        require_bounded_size(&stat, path)?;
        if identity(&stat) != expected {
            return Err(identity_changed(path));
        }
        require_empty_acl(fd, path, "daemon runtime metadata")?;
        match links {
            RuntimeLinks::Attached => Ok(Descriptor::Attached(stat)),
            RuntimeLinks::Detached => {
                match lookup_leaf(seam, daemon, name, NameLookup::Withdrawal)? {
                    None => Ok(Descriptor::Withdrawn),
                    Some(named) if identity(&named) == expected && named.st_nlink == 0 => {
                        Ok(Descriptor::Withdrawn)
                    }
                    Some(_) => Err(identity_changed(path)),
                }
            }
        }
    }

    fn validate_state_directory(
        stat: &libc::stat,
        expected_device: libc::dev_t,
        path: &Path,
    ) -> io::Result<()> {
        require_owned_directory(stat, expected_device, path, "runtime state directory")?;
        require_no_non_owner_write(stat, path, "runtime state directory")
    }

    /// Checks a runtime inode, classifying its link count last.
    fn validate_runtime_inode(
        stat: &libc::stat,
        expected_device: libc::dev_t,
        path: &Path,
    ) -> io::Result<RuntimeLinks> {
        if file_kind(stat) != libc::S_IFREG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata is not a regular file: {}",
                    path.display()
                ),
            ));
        }
        let effective_uid = effective_uid();
        if stat.st_uid != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "daemon runtime metadata is owned by uid {}; expected uid {effective_uid}: {}",
                    stat.st_uid,
                    path.display()
                ),
            ));
        }
        if stat.st_dev != expected_device {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "daemon runtime metadata is on a different filesystem: {}",
                    path.display()
                ),
            ));
        }
        let mode = stat.st_mode & 0o777;
        if (mode & 0o400) == 0 || (mode & 0o022) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "daemon runtime metadata mode {mode:o} is not owner-readable with no \
                     non-owner write authority: {}",
                    path.display()
                ),
            ));
        }
        match stat.st_nlink {
            1 => Ok(RuntimeLinks::Attached),
            0 => Ok(RuntimeLinks::Detached),
            _ => Err(unexpected_link_count(stat, path)),
        }
    }

    fn unexpected_link_count(stat: &libc::stat, path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon runtime metadata has {} links; expected exactly one: {}",
                stat.st_nlink,
                path.display()
            ),
        )
    }

    fn require_bounded_size(stat: &libc::stat, path: &Path) -> io::Result<()> {
        let size = usize::try_from(stat.st_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata has an invalid negative or unaddressable size: {}",
                    path.display()
                ),
            )
        })?;
        if size > MAX_DAEMON_RUNTIME_INFO_BYTES {
            return Err(oversized(path));
        }
        Ok(())
    }

    fn oversized(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "daemon runtime metadata exceeds {MAX_DAEMON_RUNTIME_INFO_BYTES} bytes: {}",
                path.display()
            ),
        )
    }

    fn require_owned_directory(
        stat: &libc::stat,
        expected_device: libc::dev_t,
        path: &Path,
        description: &str,
    ) -> io::Result<()> {
        require_directory(stat, path, description)?;
        let effective_uid = effective_uid();
        if stat.st_uid != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} is owned by uid {}; expected uid {effective_uid}: {}",
                    stat.st_uid,
                    path.display()
                ),
            ));
        }
        if stat.st_dev != expected_device {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} is on a different filesystem: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn require_directory(stat: &libc::stat, path: &Path, description: &str) -> io::Result<()> {
        if file_kind(stat) != libc::S_IFDIR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} is not a directory: {}", path.display()),
            ));
        }
        Ok(())
    }

    fn require_no_non_owner_write(
        stat: &libc::stat,
        path: &Path,
        description: &str,
    ) -> io::Result<()> {
        if (stat.st_mode & 0o022) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} has non-owner write authority: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn require_empty_acl(fd: &impl AsRawFd, path: &Path, description: &str) -> io::Result<()> {
        if has_extended_acl(fd.as_raw_fd())? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} has an extended ACL and cannot be authenticated: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn require_no_namespace_acl(
        fd: &impl AsRawFd,
        path: &Path,
        description: &str,
    ) -> io::Result<()> {
        if has_namespace_authority_acl(fd.as_raw_fd())? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} has extended ACL namespace authority: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn require_no_active_namespace_acl(
        fd: &impl AsRawFd,
        path: &Path,
        description: &str,
    ) -> io::Result<()> {
        if has_active_namespace_authority_acl(fd.as_raw_fd())? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} has extended ACL namespace authority: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` points to writable storage and `fd` is borrowed from
        // a live owned descriptor.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful `fstat` initialized the complete output value.
        Ok(unsafe { stat.assume_init() })
    }

    fn fstatat_nofollow_if_present(
        parent: RawFd,
        name: &CString,
    ) -> io::Result<Option<libc::stat>> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is live and NUL-terminated, `parent` is a live
        // directory descriptor, and `stat` points to writable storage.
        if unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            // SAFETY: successful `fstatat` initialized the output value.
            return Ok(Some(unsafe { stat.assume_init() }));
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(source)
        }
    }

    fn fstatat_nofollow(parent: RawFd, name: &CString) -> io::Result<libc::stat> {
        fstatat_nofollow_if_present(parent, name)?.ok_or_else(entry_disappeared)
    }

    fn entry_disappeared() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authenticated runtime entry disappeared during discovery",
        )
    }

    fn openat_directory(parent: RawFd, name: &OsStr) -> io::Result<OwnedFd> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace path component contains an interior NUL",
            )
        })?;
        openat_directory_cstr(parent, &name)
    }

    fn openat_directory_cstr(parent: RawFd, name: &CString) -> io::Result<OwnedFd> {
        // SAFETY: `name` is live and NUL-terminated, `parent` is a retained
        // directory descriptor, the flags request no creation operation (and
        // therefore require no variadic mode argument), and the returned
        // descriptor is immediately transferred to `OwnedFd`.
        let raw_fd = unsafe { libc::openat(parent, name.as_ptr(), DIRECTORY_OPEN_FLAGS) };
        owned_fd(raw_fd)
    }

    fn owned_fd(raw_fd: RawFd) -> io::Result<OwnedFd> {
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful `open`/`openat` returns one new descriptor that
        // has not been transferred elsewhere.
        Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    }

    fn identity(stat: &libc::stat) -> FileIdentity {
        FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }

    fn identity_changed(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "authenticated runtime entry changed identity during discovery: {}",
                path.display()
            ),
        )
    }

    fn file_kind(stat: &libc::stat) -> libc::mode_t {
        stat.st_mode & libc::S_IFMT
    }

    pub(super) fn effective_uid() -> libc::uid_t {
        // SAFETY: `geteuid` has no preconditions and retains no pointers.
        unsafe { libc::geteuid() }
    }

    #[cfg(target_vendor = "apple")]
    mod acl {
        use std::ffi::c_void;
        use std::ptr;

        use super::{c_int, io, RawFd};

        const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
        const ACL_FIRST_ENTRY: c_int = 0;
        const ACL_NEXT_ENTRY: c_int = -1;
        const ACL_EXTENDED_ALLOW: c_int = 1;
        const ACL_ENTRY_ONLY_INHERIT: c_int = 1 << 8;
        const DANGEROUS_PERMISSIONS: [c_int; 8] = [
            1 << 2,
            1 << 4,
            1 << 5,
            1 << 6,
            1 << 8,
            1 << 10,
            1 << 12,
            1 << 13,
        ];

        #[repr(C)]
        struct AclOpaque {
            _private: [u8; 0],
        }

        #[repr(C)]
        struct AclEntryOpaque {
            _private: [u8; 0],
        }

        #[repr(C)]
        struct AclPermsetOpaque {
            _private: [u8; 0],
        }

        #[repr(C)]
        struct AclFlagsetOpaque {
            _private: [u8; 0],
        }

        unsafe extern "C" {
            fn acl_free(object: *mut c_void) -> c_int;
            fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut AclOpaque;
            fn acl_get_entry(
                acl: *mut AclOpaque,
                entry_id: c_int,
                entry: *mut *mut AclEntryOpaque,
            ) -> c_int;
            fn acl_get_perm_np(permset: *mut AclPermsetOpaque, permission: c_int) -> c_int;
            fn acl_get_permset(
                entry: *mut AclEntryOpaque,
                permset: *mut *mut AclPermsetOpaque,
            ) -> c_int;
            fn acl_get_flag_np(flagset: *mut AclFlagsetOpaque, flag: c_int) -> c_int;
            fn acl_get_flagset_np(
                object: *mut c_void,
                flagset: *mut *mut AclFlagsetOpaque,
            ) -> c_int;
            fn acl_get_tag_type(entry: *mut AclEntryOpaque, tag: *mut c_int) -> c_int;
        }

        struct OwnedAcl(*mut AclOpaque);

        impl Drop for OwnedAcl {
            fn drop(&mut self) {
                // SAFETY: each `OwnedAcl` wraps one non-null pointer returned
                // by `acl_get_fd_np` and frees it exactly once.
                let _ = unsafe { acl_free(self.0.cast()) };
            }
        }

        pub(super) fn has_extended_acl(fd: RawFd) -> io::Result<bool> {
            let Some(acl) = descriptor_acl(fd)? else {
                return Ok(false);
            };
            let mut entry = ptr::null_mut();
            // SAFETY: `acl` remains live and `entry` points to writable
            // storage for a borrowed ACL entry.
            match unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &mut entry) } {
                0 => Ok(true),
                _ => acl_iteration_end_or_error(),
            }
        }

        pub(super) fn has_namespace_authority_acl(fd: RawFd) -> io::Result<bool> {
            let Some(acl) = descriptor_acl(fd)? else {
                return Ok(false);
            };
            let mut entry_id = ACL_FIRST_ENTRY;
            loop {
                let mut entry = ptr::null_mut();
                // SAFETY: `acl` remains live and `entry` points to writable
                // storage for a borrowed entry owned by `acl`.
                match unsafe { acl_get_entry(acl.0, entry_id, &mut entry) } {
                    0 => {}
                    _ => return acl_iteration_end_or_error(),
                }
                entry_id = ACL_NEXT_ENTRY;

                let mut tag = 0;
                // SAFETY: `entry` is live and `tag` points to writable
                // storage for the entry tag.
                if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                if tag != ACL_EXTENDED_ALLOW {
                    continue;
                }

                let mut flags = ptr::null_mut();
                // SAFETY: `entry` remains live and `flags` receives a flagset
                // borrowed from that entry.
                if unsafe { acl_get_flagset_np(entry.cast(), &mut flags) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: `flags` remains borrowed from the live ACL entry.
                match unsafe { acl_get_flag_np(flags, ACL_ENTRY_ONLY_INHERIT) } {
                    1 => continue,
                    0 => {}
                    _ => return Err(io::Error::last_os_error()),
                }

                let mut permissions = ptr::null_mut();
                // SAFETY: `entry` remains live and `permissions` receives a
                // permset borrowed from that entry.
                if unsafe { acl_get_permset(entry, &mut permissions) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                for permission in DANGEROUS_PERMISSIONS {
                    // SAFETY: `permissions` is borrowed from a live ACL entry.
                    match unsafe { acl_get_perm_np(permissions, permission) } {
                        0 => {}
                        1 => return Ok(true),
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
            }
        }

        pub(super) fn has_active_namespace_authority_acl(fd: RawFd) -> io::Result<bool> {
            has_namespace_authority_acl(fd)
        }

        fn descriptor_acl(fd: RawFd) -> io::Result<Option<OwnedAcl>> {
            // SAFETY: `fd` is borrowed from a live descriptor. The returned
            // ACL is independently allocated and immediately owned.
            let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
            if !acl.is_null() {
                return Ok(Some(OwnedAcl(acl)));
            }
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::ENOENT) {
                Ok(None)
            } else {
                Err(source)
            }
        }

        fn acl_iteration_end_or_error() -> io::Result<bool> {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::EINVAL) {
                Ok(false)
            } else {
                Err(source)
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod acl {
        use std::ffi::{c_char, c_void};

        use super::{io, RawFd};

        const ACCESS_ACL: &[u8] = b"system.posix_acl_access\0";
        const DEFAULT_ACL: &[u8] = b"system.posix_acl_default\0";

        pub(super) fn has_extended_acl(fd: RawFd) -> io::Result<bool> {
            Ok(acl_present(fd, ACCESS_ACL)? || acl_present(fd, DEFAULT_ACL)?)
        }

        pub(super) fn has_namespace_authority_acl(fd: RawFd) -> io::Result<bool> {
            // Access-ACL write authority is reflected in the directory's
            // group/other mode bits through the POSIX ACL mask and is checked
            // separately. A default ACL matters here because it can grant
            // authority to newly created runtime-state children.
            acl_present(fd, DEFAULT_ACL)
        }

        pub(super) fn has_active_namespace_authority_acl(_fd: RawFd) -> io::Result<bool> {
            // Ancestor default ACLs affect only future direct children, not
            // the already-opened workspace component. Active access-ACL write
            // authority is represented by mode bits and checked by the caller.
            Ok(false)
        }

        fn acl_present(fd: RawFd, name: &'static [u8]) -> io::Result<bool> {
            // SAFETY: `name` is static and NUL-terminated, `fd` is borrowed
            // from a live descriptor, and a null buffer requests only length.
            let size = unsafe {
                libc::fgetxattr(
                    fd,
                    name.as_ptr().cast::<c_char>(),
                    std::ptr::null_mut::<c_void>(),
                    0,
                )
            };
            if size >= 0 {
                return Ok(true);
            }
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::ENODATA) {
                Ok(false)
            } else {
                Err(source)
            }
        }
    }

    #[cfg(all(
        not(target_vendor = "apple"),
        not(any(target_os = "linux", target_os = "android"))
    ))]
    mod acl {
        use super::{io, RawFd};

        pub(super) fn has_extended_acl(_fd: RawFd) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor ACL verification is unavailable on this Unix platform",
            ))
        }

        pub(super) fn has_namespace_authority_acl(_fd: RawFd) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor namespace ACL verification is unavailable on this Unix platform",
            ))
        }

        pub(super) fn has_active_namespace_authority_acl(_fd: RawFd) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor active namespace ACL verification is unavailable on this Unix platform",
            ))
        }
    }

    use acl::{has_active_namespace_authority_acl, has_extended_acl, has_namespace_authority_acl};
}

#[cfg(not(unix))]
mod platform {
    use std::fs::{self, File};
    use std::io::Read as _;

    use super::*;

    pub(super) fn read_runtime(root: &Path) -> io::Result<RuntimeLeaf> {
        let path = runtime_path(root);
        let preflight = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(RuntimeLeaf::Absent)
            }
            Err(source) => return Err(source),
        };
        if !preflight.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata is not a regular file: {}",
                    path.display()
                ),
            ));
        }
        if preflight.len() > u64::try_from(MAX_DAEMON_RUNTIME_INFO_BYTES).unwrap_or(u64::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata exceeds {MAX_DAEMON_RUNTIME_INFO_BYTES} bytes: {}",
                    path.display()
                ),
            ));
        }

        let file = File::open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata changed type during discovery: {}",
                    path.display()
                ),
            ));
        }
        let mut bytes = Vec::new();
        file.take(
            u64::try_from(MAX_DAEMON_RUNTIME_INFO_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DAEMON_RUNTIME_INFO_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "daemon runtime metadata exceeds {MAX_DAEMON_RUNTIME_INFO_BYTES} bytes: {}",
                    path.display()
                ),
            ));
        }
        Ok(RuntimeLeaf::Read(RuntimeRead { bytes }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod unix {
        use std::ffi::CString;
        use std::fs;
        #[cfg(target_os = "linux")]
        use std::os::fd::AsRawFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::{symlink, PermissionsExt as _};
        #[cfg(target_os = "macos")]
        use std::process::Command;

        use tempfile::TempDir;

        use super::*;
        use packet28_daemon_protocol::message::DaemonTransportAuth;
        use packet28_daemon_protocol::paths::daemon_dir;
        use platform::{LeafPhase, NameLookup};

        #[test]
        fn missing_real_state_directory_returns_none() {
            let root = TempDir::new().unwrap();

            let runtime = read_runtime_info_if_present(root.path()).unwrap();

            assert!(runtime.is_none());
        }

        #[test]
        fn missing_real_daemon_directory_returns_none() {
            let root = TempDir::new().unwrap();
            fs::create_dir(root.path().join(".packet28")).unwrap();
            fs::set_permissions(
                root.path().join(".packet28"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();

            let runtime = read_runtime_info_if_present(root.path()).unwrap();

            assert!(runtime.is_none());
        }

        #[test]
        fn missing_real_runtime_leaf_returns_none() {
            let root = TempDir::new().unwrap();
            create_state_directories(root.path());

            let runtime = read_runtime_info_if_present(root.path()).unwrap();

            assert!(runtime.is_none());
        }

        #[test]
        fn valid_unix_runtime_metadata_is_loaded() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);

            let runtime = read_runtime_info_if_present(root.path()).unwrap().unwrap();

            assert_eq!(runtime.pid, 0);
        }

        #[test]
        fn owner_private_capability_metadata_is_loaded() {
            let root = TempDir::new().unwrap();
            let runtime = DaemonRuntimeInfo {
                transport_auth: Some(DaemonTransportAuth::from_secret_bytes([7_u8; 32])),
                ..DaemonRuntimeInfo::default()
            };
            write_runtime(root.path(), &runtime, 0o600);

            let loaded = read_runtime_info_if_present(root.path()).unwrap().unwrap();

            assert!(loaded.transport_auth.is_some());
        }

        #[test]
        fn capability_metadata_with_group_permissions_is_rejected() {
            let root = TempDir::new().unwrap();
            let runtime = DaemonRuntimeInfo {
                transport_auth: Some(DaemonTransportAuth::from_secret_bytes([9_u8; 32])),
                ..DaemonRuntimeInfo::default()
            };
            write_runtime(root.path(), &runtime, 0o640);

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error
                .to_string()
                .contains("group and other permission bits"));
        }

        #[test]
        fn state_parent_symlink_with_present_target_is_rejected() {
            let root = TempDir::new().unwrap();
            let target = TempDir::new().unwrap();
            fs::create_dir(target.path().join("daemon")).unwrap();
            fs::set_permissions(
                target.path().join("daemon"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            fs::write(
                target.path().join("daemon/runtime.json"),
                serde_json::to_vec(&DaemonRuntimeInfo::default()).unwrap(),
            )
            .unwrap();
            symlink(target.path(), root.path().join(".packet28")).unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("not a directory"));
        }

        #[test]
        fn state_parent_symlink_with_missing_target_is_rejected() {
            let root = TempDir::new().unwrap();
            symlink(
                root.path().join("missing-state-target"),
                root.path().join(".packet28"),
            )
            .unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("not a directory"));
        }

        #[test]
        fn runtime_leaf_symlink_is_rejected() {
            let root = TempDir::new().unwrap();
            let external = root.path().join("external-runtime.json");
            fs::write(
                &external,
                serde_json::to_vec(&DaemonRuntimeInfo::default()).unwrap(),
            )
            .unwrap();
            create_state_directories(root.path());
            symlink(&external, runtime_path(root.path())).unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("not a regular file"));
        }

        #[test]
        fn runtime_fifo_is_rejected_without_opening_it() {
            let root = TempDir::new().unwrap();
            create_state_directories(root.path());
            let runtime_path = runtime_path(root.path());
            let runtime_c = CString::new(runtime_path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `runtime_c` is live and NUL-terminated for this call.
            let result = unsafe { libc::mkfifo(runtime_c.as_ptr(), 0o600) };
            assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("not a regular file"));
        }

        #[test]
        fn oversized_runtime_metadata_is_rejected() {
            let root = TempDir::new().unwrap();
            create_state_directories(root.path());
            fs::write(
                runtime_path(root.path()),
                vec![b' '; MAX_DAEMON_RUNTIME_INFO_BYTES + 1],
            )
            .unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("exceeds"));
        }

        #[test]
        fn hard_linked_runtime_metadata_is_rejected() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            fs::hard_link(
                runtime_path(root.path()),
                root.path().join("runtime-hard-link.json"),
            )
            .unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("expected exactly one"));
        }

        #[test]
        fn group_writable_runtime_state_directory_is_rejected() {
            let root = TempDir::new().unwrap();
            create_state_directories(root.path());
            fs::set_permissions(daemon_dir(root.path()), fs::Permissions::from_mode(0o775))
                .unwrap();

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error.to_string().contains("non-owner write authority"));
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn macos_runtime_acl_is_rejected_without_changing_runtime_bytes() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let path = runtime_path(root.path());
            let bytes_before = fs::read(&path).unwrap();
            add_macos_acl(&path, "everyone allow read");

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error
                .to_string()
                .contains("daemon runtime metadata has an extended ACL"));
            assert_eq!(fs::read(path).unwrap(), bytes_before);
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn macos_workspace_namespace_acl_is_rejected_without_changing_runtime_bytes() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let path = runtime_path(root.path());
            let bytes_before = fs::read(&path).unwrap();
            add_macos_acl(root.path(), "everyone allow add_file");

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error
                .to_string()
                .contains("workspace has extended ACL namespace authority"));
            assert_eq!(fs::read(path).unwrap(), bytes_before);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_runtime_access_acl_is_rejected_without_changing_runtime_bytes() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let path = runtime_path(root.path());
            let bytes_before = fs::read(&path).unwrap();
            let runtime = fs::File::open(&path).unwrap();
            set_linux_acl_xattr(&runtime, LINUX_ACCESS_ACL, 0o644);

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error
                .to_string()
                .contains("daemon runtime metadata has an extended ACL"));
            assert_eq!(fs::read(path).unwrap(), bytes_before);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_workspace_default_acl_is_rejected_without_changing_runtime_bytes() {
            let root = TempDir::new().unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let path = runtime_path(root.path());
            let bytes_before = fs::read(&path).unwrap();
            let workspace = fs::File::open(root.path()).unwrap();
            set_linux_acl_xattr(&workspace, LINUX_DEFAULT_ACL, 0o755);

            let error = read_runtime_info_if_present(root.path()).unwrap_err();

            assert!(error
                .to_string()
                .contains("workspace has extended ACL namespace authority"));
            assert_eq!(fs::read(path).unwrap(), bytes_before);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_ancestor_default_acl_does_not_taint_existing_workspace() {
            let root = TempDir::new().unwrap();
            let ancestor = root.path().join("acl-parent");
            let workspace = ancestor.join("workspace");
            fs::create_dir(&ancestor).unwrap();
            fs::create_dir(&workspace).unwrap();
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
            let ancestor_fd = fs::File::open(&ancestor).unwrap();
            set_linux_acl_xattr(&ancestor_fd, LINUX_DEFAULT_ACL, 0o755);
            write_runtime(&workspace, &DaemonRuntimeInfo::default(), 0o600);

            let runtime = read_runtime_info_if_present(&workspace)
                .unwrap()
                .expect("authenticated runtime metadata");

            assert_eq!(runtime.pid, 0);
            assert!(runtime.transport_auth.is_none());
        }

        #[test]
        fn runtime_identity_change_after_authenticated_read_is_rejected() {
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let path = runtime_path(root.path());
            let authenticated_path = daemon_dir(root.path()).join("authenticated-runtime.json");
            let replacement_path = daemon_dir(root.path()).join("replacement-runtime.json");
            let bytes_before = fs::read(&path).unwrap();
            fs::write(&replacement_path, &bytes_before).unwrap();
            fs::set_permissions(&replacement_path, fs::Permissions::from_mode(0o644)).unwrap();

            let error = platform::read_runtime_with_seam_for_test(
                root.path(),
                &mut TestSeam::changing_at(LeafPhase::AttachedValidation, |runtime_path| {
                    fs::rename(runtime_path, &authenticated_path).unwrap();
                    fs::rename(&replacement_path, runtime_path).unwrap();
                }),
            )
            .err()
            .expect("runtime identity replacement unexpectedly authenticated");

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error
                .to_string()
                .contains("changed identity during discovery"));
            assert_eq!(fs::read(authenticated_path).unwrap(), bytes_before);
        }

        const DESCRIPTOR_PHASES: [LeafPhase; 3] = [
            LeafPhase::OpenedValidation,
            LeafPhase::FinalValidation,
            LeafPhase::AttachedValidation,
        ];
        /// Phases after the preflight lookup authenticated the leaf.
        const AUTHENTICATED_PHASES: [LeafPhase; 4] = [
            LeafPhase::Open,
            LeafPhase::OpenedValidation,
            LeafPhase::FinalValidation,
            LeafPhase::AttachedValidation,
        ];
        const LEAF_PHASES: [LeafPhase; 5] = [
            LeafPhase::Preflight,
            LeafPhase::Open,
            LeafPhase::OpenedValidation,
            LeafPhase::FinalValidation,
            LeafPhase::AttachedValidation,
        ];

        /// Changes the leaf once when the read reaches `phase`, and lets a
        /// test rewrite name lookups to model kernel unlink windows.
        struct TestSeam<C, L> {
            phase: LeafPhase,
            change: Option<C>,
            looked_up: L,
            preflight: Option<libc::stat>,
        }

        impl<C: FnOnce(&Path)> TestSeam<C, fn(NameLookup, &mut Option<libc::stat>, Option<libc::stat>)> {
            fn changing_at(phase: LeafPhase, change: C) -> Self {
                TestSeam {
                    phase,
                    change: Some(change),
                    looked_up: |_, _, _| {},
                    preflight: None,
                }
            }
        }

        impl<C, L> platform::LeafSeam for TestSeam<C, L>
        where
            C: FnOnce(&Path),
            L: FnMut(NameLookup, &mut Option<libc::stat>, Option<libc::stat>),
        {
            fn before(&mut self, phase: LeafPhase, path: &Path) -> io::Result<()> {
                if phase == self.phase {
                    if let Some(change) = self.change.take() {
                        change(path);
                    }
                }
                Ok(())
            }

            fn looked_up(&mut self, lookup: NameLookup, stat: &mut Option<libc::stat>) {
                if self.preflight.is_none() {
                    self.preflight = *stat;
                }
                (self.looked_up)(lookup, stat, self.preflight);
            }
        }

        fn discover_with<C, L>(
            root: &Path,
            seam: &mut TestSeam<C, L>,
        ) -> Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError>
        where
            C: FnOnce(&Path),
            L: FnMut(NameLookup, &mut Option<libc::stat>, Option<libc::stat>),
        {
            let leaf = platform::read_runtime_with_seam_for_test(root, seam);
            decode_runtime(runtime_path(root), leaf)
        }

        /// Reads runtime metadata, changing the leaf once when the read
        /// reaches `phase`.
        fn discover_changing_leaf_at(
            root: &Path,
            phase: LeafPhase,
            change: impl FnOnce(&Path),
        ) -> Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError> {
            let mut seam = TestSeam::changing_at(phase, change);
            let result = discover_with(root, &mut seam);
            assert!(seam.change.is_none(), "read never reached {phase:?}");
            result
        }

        fn zero_linked(mut stat: libc::stat) -> Option<libc::stat> {
            stat.st_nlink = 0;
            Some(stat)
        }

        fn assert_withdrawn(
            root: &Path,
            context: &str,
            result: Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError>,
        ) {
            match result {
                Err(RuntimeDiscoveryError::Withdrawn { path }) => {
                    assert_eq!(path, runtime_path(root), "{context}");
                }
                other => panic!("{context}: expected withdrawal, got {other:?}"),
            }
        }

        fn io_error_at(
            phase: LeafPhase,
            result: Result<Option<DaemonRuntimeInfo>, RuntimeDiscoveryError>,
        ) -> io::Error {
            match result {
                Err(RuntimeDiscoveryError::Io { source, .. }) => source,
                other => panic!("{phase:?}: expected an authentication error, got {other:?}"),
            }
        }

        #[test]
        fn runtime_unlinked_at_each_leaf_phase_is_withdrawn_without_decoding() {
            for phase in LEAF_PHASES {
                let root = TempDir::new().unwrap();
                create_state_directories(root.path());
                // Malformed bytes would fail decoding if they were ever used.
                fs::write(runtime_path(root.path()), b"{").unwrap();
                fs::set_permissions(runtime_path(root.path()), fs::Permissions::from_mode(0o644))
                    .unwrap();

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::remove_file(path).unwrap();
                });

                if phase == LeafPhase::Preflight {
                    // Unlinked before the lookup: genuinely absent.
                    assert!(result.unwrap().is_none());
                } else {
                    assert_withdrawn(root.path(), &format!("{phase:?}"), result);
                }
                assert!(read_runtime_info_if_present(root.path()).unwrap().is_none());
            }
        }

        /// Models the Linux unlink window: the inode's link count is already
        /// zero while lookups through the retained directory still resolve
        /// the name to it, at every lookup the read performs.
        #[test]
        fn zero_linked_name_during_unlink_is_withdrawn_at_each_window() {
            // Windows 1, 3, 4, and 5: the unlink completes before the descriptor
            // checks, but each lookup still reports the zero-linked inode.
            for phase in DESCRIPTOR_PHASES {
                let root = TempDir::new().unwrap();
                create_state_directories(root.path());
                fs::write(runtime_path(root.path()), b"{").unwrap();
                fs::set_permissions(runtime_path(root.path()), fs::Permissions::from_mode(0o644))
                    .unwrap();
                let mut unlinked = false;
                let mut seam = TestSeam {
                    phase,
                    change: Some(|path: &Path| fs::remove_file(path).unwrap()),
                    looked_up: |lookup: NameLookup,
                                stat: &mut Option<libc::stat>,
                                preflight: Option<libc::stat>| {
                        if lookup == NameLookup::Preflight {
                            *stat = zero_linked(preflight.unwrap());
                        } else if stat.is_none() {
                            unlinked = true;
                            *stat = zero_linked(preflight.unwrap());
                        }
                    },
                    preflight: None,
                };

                let result = discover_with(root.path(), &mut seam);

                assert_withdrawn(root.path(), &format!("{phase:?}"), result);
                assert!(unlinked, "{phase:?}: no lookup observed the unlinked name");
            }

            // Window 1 then 2: a zero-linked preflight whose name is gone by
            // the open.
            let root = TempDir::new().unwrap();
            write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
            let mut seam = TestSeam {
                phase: LeafPhase::Open,
                change: Some(|path: &Path| fs::remove_file(path).unwrap()),
                looked_up: |lookup: NameLookup,
                            stat: &mut Option<libc::stat>,
                            preflight: Option<libc::stat>| {
                    if lookup == NameLookup::Preflight {
                        *stat = zero_linked(preflight.unwrap());
                    }
                },
                preflight: None,
            };
            assert_withdrawn(
                root.path(),
                "zero-linked preflight",
                discover_with(root.path(), &mut seam),
            );
        }

        #[test]
        fn zero_linked_name_still_requires_static_authentication() {
            type Corrupt = fn(&mut libc::stat);
            let cases: [(&str, Corrupt); 5] = [
                ("mode 666 is not owner-readable", |stat| {
                    stat.st_mode = libc::S_IFREG | 0o666;
                }),
                ("is not a regular file", |stat| {
                    stat.st_mode = libc::S_IFIFO | 0o644;
                }),
                ("is owned by uid", |stat| {
                    stat.st_uid = stat.st_uid.wrapping_add(1);
                }),
                ("is on a different filesystem", |stat| {
                    stat.st_dev = stat.st_dev.wrapping_add(1);
                }),
                ("daemon runtime metadata exceeds", |stat| {
                    stat.st_size = (MAX_DAEMON_RUNTIME_INFO_BYTES + 1) as libc::off_t;
                }),
            ];
            for lookup_at in [NameLookup::Preflight, NameLookup::Attached] {
                for (expected, corrupt) in cases {
                    let root = TempDir::new().unwrap();
                    write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                    let mut seam = TestSeam {
                        phase: LeafPhase::AttachedValidation,
                        change: Some(|path: &Path| fs::remove_file(path).unwrap()),
                        looked_up:
                            |lookup: NameLookup,
                             stat: &mut Option<libc::stat>,
                             preflight: Option<libc::stat>| {
                                if lookup == lookup_at {
                                    let mut zero = zero_linked(preflight.unwrap());
                                    corrupt(zero.as_mut().unwrap());
                                    *stat = zero;
                                }
                            },
                        preflight: None,
                    };

                    let error = io_error_at(
                        LeafPhase::AttachedValidation,
                        discover_with(root.path(), &mut seam),
                    );

                    assert!(
                        error.to_string().contains(expected),
                        "{lookup_at:?}: {error}"
                    );
                }
            }
        }

        #[test]
        fn zero_linked_replacement_during_unlink_is_not_withdrawn() {
            for lookup_at in [NameLookup::Attached, NameLookup::Withdrawal] {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let mut seam = TestSeam {
                    phase: LeafPhase::AttachedValidation,
                    change: Some(|path: &Path| fs::remove_file(path).unwrap()),
                    looked_up: |lookup: NameLookup,
                                stat: &mut Option<libc::stat>,
                                preflight: Option<libc::stat>| {
                        if lookup == lookup_at {
                            let mut other = zero_linked(preflight.unwrap());
                            other.as_mut().unwrap().st_ino ^= 1;
                            *stat = other;
                        }
                    },
                    preflight: None,
                };

                let error = io_error_at(
                    LeafPhase::AttachedValidation,
                    discover_with(root.path(), &mut seam),
                );

                assert!(
                    error
                        .to_string()
                        .contains("changed identity during discovery"),
                    "{lookup_at:?}: {error}"
                );
            }
        }

        #[test]
        fn runtime_replaced_at_each_leaf_phase_is_not_withdrawn() {
            for phase in AUTHENTICATED_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let replacement = daemon_dir(root.path()).join("replacement-runtime.json");
                fs::write(&replacement, fs::read(runtime_path(root.path())).unwrap()).unwrap();
                fs::set_permissions(&replacement, fs::Permissions::from_mode(0o644)).unwrap();

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::rename(&replacement, path).unwrap();
                });

                let error = io_error_at(phase, result);
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{phase:?}");
                assert!(
                    error
                        .to_string()
                        .contains("changed identity during discovery"),
                    "{phase:?}: {error}"
                );
            }
        }

        #[test]
        fn runtime_symlinked_at_each_leaf_phase_is_not_withdrawn() {
            for phase in AUTHENTICATED_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let target = root.path().join("symlink-target.json");
                fs::write(&target, fs::read(runtime_path(root.path())).unwrap()).unwrap();

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::remove_file(path).unwrap();
                    symlink(&target, path).unwrap();
                });

                let error = io_error_at(phase, result);
                if phase == LeafPhase::Open {
                    assert_eq!(error.raw_os_error(), Some(libc::ELOOP), "{error}");
                } else if phase == LeafPhase::AttachedValidation {
                    assert!(
                        error
                            .to_string()
                            .contains("daemon runtime metadata is not a regular file"),
                        "{phase:?}: {error}"
                    );
                } else {
                    assert!(
                        error
                            .to_string()
                            .contains("changed identity during discovery"),
                        "{phase:?}: {error}"
                    );
                }
            }
        }

        #[test]
        fn runtime_hard_linked_at_each_leaf_phase_is_not_withdrawn() {
            for phase in LEAF_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let hard_link = root.path().join("runtime-hard-link.json");

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::hard_link(path, &hard_link).unwrap();
                });

                let error = io_error_at(phase, result);
                assert!(
                    error
                        .to_string()
                        .contains("has 2 links; expected exactly one"),
                    "{phase:?}: {error}"
                );
            }
        }

        #[test]
        fn runtime_moved_after_open_is_not_withdrawn() {
            for phase in DESCRIPTOR_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let moved = daemon_dir(root.path()).join("moved-runtime.json");

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::rename(path, &moved).unwrap();
                });

                let error = io_error_at(phase, result);
                assert!(
                    error
                        .to_string()
                        .contains("authenticated runtime entry disappeared during discovery"),
                    "{phase:?}: {error}"
                );
            }
        }

        #[test]
        fn unsafe_runtime_unlinked_after_open_is_not_withdrawn() {
            for phase in DESCRIPTOR_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
                    fs::remove_file(path).unwrap();
                });
                let error = io_error_at(phase, result);
                assert!(
                    error.to_string().contains("mode 666 is not owner-readable"),
                    "{phase:?}: {error}"
                );

                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);
                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    fs::write(path, vec![b' '; MAX_DAEMON_RUNTIME_INFO_BYTES + 1]).unwrap();
                    fs::remove_file(path).unwrap();
                });
                let error = io_error_at(phase, result);
                assert!(
                    error
                        .to_string()
                        .contains("daemon runtime metadata exceeds"),
                    "{phase:?}: {error}"
                );
            }
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[test]
        fn runtime_with_acl_unlinked_after_open_is_not_withdrawn() {
            for phase in DESCRIPTOR_PHASES {
                let root = TempDir::new().unwrap();
                write_runtime(root.path(), &DaemonRuntimeInfo::default(), 0o644);

                let result = discover_changing_leaf_at(root.path(), phase, |path| {
                    #[cfg(target_os = "linux")]
                    set_linux_acl_xattr(&fs::File::open(path).unwrap(), LINUX_ACCESS_ACL, 0o644);
                    #[cfg(target_os = "macos")]
                    add_macos_acl(path, "everyone allow read");
                    fs::remove_file(path).unwrap();
                });

                let error = io_error_at(phase, result);
                assert!(
                    error
                        .to_string()
                        .contains("daemon runtime metadata has an extended ACL"),
                    "{phase:?}: {error}"
                );
            }
        }

        #[test]
        fn namespace_ancestry_trusts_only_root_or_the_effective_user() {
            let effective_uid = platform::effective_uid();
            let foreign_uid = if effective_uid == 1 { 2 } else { 1 };

            assert!(platform::namespace_owner_is_trusted(0, effective_uid));
            assert!(platform::namespace_owner_is_trusted(
                effective_uid,
                effective_uid
            ));
            assert!(!platform::namespace_owner_is_trusted(
                foreign_uid,
                effective_uid
            ));
        }

        #[cfg(target_os = "macos")]
        fn add_macos_acl(path: &Path, entry: &str) {
            let status = Command::new("/bin/chmod")
                .arg("+a")
                .arg(entry)
                .arg(path)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "failed to seed macOS ACL on {}",
                path.display()
            );
        }

        #[cfg(target_os = "linux")]
        const LINUX_ACCESS_ACL: &[u8] = b"system.posix_acl_access\0";

        #[cfg(target_os = "linux")]
        const LINUX_DEFAULT_ACL: &[u8] = b"system.posix_acl_default\0";

        #[cfg(target_os = "linux")]
        fn set_linux_acl_xattr(file: &fs::File, name: &'static [u8], mode: u16) {
            const ACL_UNDEFINED_ID: u32 = u32::MAX;

            let mut encoded = Vec::new();
            encoded.extend_from_slice(&2_u32.to_le_bytes());
            let mut push_entry = |tag: u16, permissions: u16, id: u32| {
                encoded.extend_from_slice(&tag.to_le_bytes());
                encoded.extend_from_slice(&permissions.to_le_bytes());
                encoded.extend_from_slice(&id.to_le_bytes());
            };
            let owner_permissions = (mode >> 6) & 0o7;
            let group_permissions = (mode >> 3) & 0o7;
            let other_permissions = mode & 0o7;
            let named_uid = platform::effective_uid().wrapping_add(1);
            push_entry(0x01, owner_permissions, ACL_UNDEFINED_ID);
            push_entry(0x02, group_permissions, named_uid);
            push_entry(0x04, group_permissions, ACL_UNDEFINED_ID);
            push_entry(0x10, group_permissions, ACL_UNDEFINED_ID);
            push_entry(0x20, other_permissions, ACL_UNDEFINED_ID);

            // SAFETY: the descriptor and NUL-terminated name are live, and
            // `encoded` contains the kernel's fixed POSIX ACL xattr format.
            let result = unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr().cast(),
                    encoded.as_ptr().cast(),
                    encoded.len(),
                    0,
                )
            };
            assert_eq!(
                result,
                0,
                "failed to seed POSIX ACL xattr: {}",
                io::Error::last_os_error()
            );
        }

        fn create_state_directories(root: &Path) {
            fs::create_dir(root.join(".packet28")).unwrap();
            fs::set_permissions(root.join(".packet28"), fs::Permissions::from_mode(0o755)).unwrap();
            fs::create_dir(daemon_dir(root)).unwrap();
            fs::set_permissions(daemon_dir(root), fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn write_runtime(root: &Path, runtime: &DaemonRuntimeInfo, mode: u32) {
            create_state_directories(root);
            let path = runtime_path(root);
            fs::write(&path, serde_json::to_vec(runtime).unwrap()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }
}
