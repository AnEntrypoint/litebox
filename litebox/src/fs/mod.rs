// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! File-system related functionality

use crate::fd::{FdEnabledSubsystem, TypedFd};
use crate::path;

use alloc::vec::Vec;
use bitflags::bitflags;

use core::ffi::c_uint;
use core::num::NonZeroUsize;

pub mod backend;
pub mod composer;
pub mod devices;
pub mod errors;
pub mod export;
pub mod import;
pub mod in_mem;
pub(crate) mod inode_allocator;
pub mod layered;
pub mod nine_p;
pub mod procfs;
pub mod resolver;
pub mod static_files;
pub mod tar_ro;

use errors::{
    ChmodError, ChownError, CloseError, FileStatusError, LinkError, MkdirError, OpenError,
    ReadDirError, ReadError, ReadLinkError, RenameError, RmdirError, SeekError, SetTimesError,
    SymlinkError, TruncateError, UnlinkError, WriteError,
};

/// A private module, to help support writing sealed traits. This module should _itself_ never be
/// made public.
mod private {
    /// A trait to help seal the main `FileSystem` trait.
    ///
    /// This trait is explicitly public, but unnameable, thereby preventing code outside this crate
    /// from implementing this trait.
    pub trait Sealed {}
}

/// A `FileSystem` provides access to all file-system related functionality provided by LiteBox.
///
/// The design of the file-system is chosen by the specific underlying implementation of this trait
/// (e.g., [`in_mem::FileSystem`]), each of which are parametric in the platform they run on.
/// However, users of any of these file systems might find benefit in having most of their code
/// depend on this trait, rather than on any individual file system.
pub trait FileSystem: private::Sealed + FdEnabledSubsystem {
    /// Opens a file
    ///
    /// The `mode` is only significant when creating a file
    fn open(
        &self,
        path: impl path::Arg,
        flags: OFlags,
        mode: Mode,
    ) -> Result<TypedFd<Self>, OpenError>;

    /// Close the file at `fd`.
    ///
    /// Future operations on the `fd` will start to return `ClosedFd` errors.
    fn close(&self, fd: &TypedFd<Self>) -> Result<(), CloseError>;

    /// Read from a file descriptor at `offset` into a buffer
    ///
    /// If `offset` is None, the read will start at the current file offset and update the file offset
    /// to the end of the read.
    /// If `offset` is Some, the file offset is not changed.
    fn read(
        &self,
        fd: &TypedFd<Self>,
        buf: &mut [u8],
        offset: Option<usize>,
    ) -> Result<usize, ReadError>;

    /// Write from a buffer to a file descriptor at `offset`
    ///
    /// If `offset` is None, the write will start at the current file offset and update the file offset
    /// to the end of the write.
    /// If `offset` is Some, the file offset is not changed.
    fn write(
        &self,
        fd: &TypedFd<Self>,
        buf: &[u8],
        offset: Option<usize>,
    ) -> Result<usize, WriteError>;

    /// Reposition read/write file offset, by changing it to `offset` relative to `whence`.
    ///
    /// Returns the resulting offset (in bytes from start of file) on success.
    fn seek(
        &self,
        fd: &TypedFd<Self>,
        offset: isize,
        whence: SeekWhence,
    ) -> Result<usize, SeekError>;

    /// Truncate the file to the specified length.
    ///
    /// If shorter than existing size, extra data is lost. If longer than existing size, resize by
    /// adding `\0`s.
    ///
    /// If `reset_offset` is true, the offset is reset to zero; otherwise, it remains unchanged.
    fn truncate(
        &self,
        fd: &TypedFd<Self>,
        length: usize,
        reset_offset: bool,
    ) -> Result<(), TruncateError>;

    /// Change the permissions of a file
    fn chmod(&self, path: impl path::Arg, mode: Mode) -> Result<(), ChmodError>;

    /// Change the permissions of a file via an already-open file descriptor, matching the
    /// semantics of `fchmod(2)`. MUST operate on the fd's already-open inode and MUST NOT
    /// re-resolve `fd` to a path: a caller may `unlink` the file first and then `fchmod` the
    /// still-open fd, as wlroots' `util/shm.c` `allocate_shm_file_pair` does.
    fn chmod_fd(&self, fd: &TypedFd<Self>, mode: Mode) -> Result<(), ChmodError>;

    /// The access-mode/status flags `fd` was actually opened with, for `fcntl(F_GETFL)` to report
    /// correctly. Defaults to `None` (the caller then reports `O_RDONLY`); override it wherever
    /// per-fd open flags are tracked, as [`layered::FileSystem`] does -- without that override
    /// `F_GETFL` broke xkbcomp's `fdopen`. See gm mutable fs-mod-openflags-fcntl-getfl-xkbcomp.
    fn open_flags(&self, _fd: &TypedFd<Self>) -> Option<OFlags> {
        None
    }

    /// Change the owner of a file
    fn chown(
        &self,
        path: impl path::Arg,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError>;

    /// Change the last-access and/or last-modification time of a file, matching the semantics of
    /// `utimensat(2)`.
    ///
    /// `atime`/`mtime` of `None` leaves that timestamp unchanged (i.e. `UTIME_OMIT`); resolving
    /// `UTIME_NOW` to an actual current-time value is the caller's responsibility (this trait has
    /// no clock of its own), so `Some(ts)` here always means "set to exactly `ts`".
    fn set_times(
        &self,
        path: impl path::Arg,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
    ) -> Result<(), SetTimesError>;

    /// Unlink a file
    fn unlink(&self, path: impl path::Arg) -> Result<(), UnlinkError>;

    /// Rename (move) `from` to `to`, atomically replacing `to` if it already exists. Regular files
    /// and symlinks only ([`RenameError::IsADirectory`] for a directory); `from` and `to` must be
    /// in the same filesystem/layer, else [`RenameError::CrossDevice`], matching Linux's `EXDEV`.
    /// Exists for the atomic replace-with-a-temp-file pattern, e.g. `apk` installing a package.
    fn rename(&self, from: impl path::Arg, to: impl path::Arg) -> Result<(), RenameError>;

    /// Create a hard link at `newpath` aliasing `oldpath`: both paths share content and metadata
    /// (a write through one is visible through the other), and the file is removed only once every
    /// linking path is unlinked. `oldpath` must be a regular file, never a directory (Linux's
    /// `link(2)` `EPERM`, keeping the tree acyclic). For atomic lock files, e.g. Xorg's own.
    fn link(&self, oldpath: impl path::Arg, newpath: impl path::Arg) -> Result<(), LinkError>;

    /// Create a symbolic link at `linkpath` pointing to `target`. `target` is stored verbatim,
    /// never resolved or validated at creation time -- dangling and relative targets are legal,
    /// as in Linux's `symlink(2)`. For e.g. `apk` installing a shared-library symlink
    /// (`usr/lib/libfoo.so -> libfoo.so.1.2.3`).
    fn symlink(&self, target: impl path::Arg, linkpath: impl path::Arg)
    -> Result<(), SymlinkError>;

    /// Create a named pipe (FIFO) at `path`: an ordinary directory entry whose [`FileType`] is
    /// [`FileType::Fifo`] and which holds no data, so `stat` reports `S_IFIFO` and `open` knows to
    /// hand back a pipe -- the pipe itself is the shim's business, not this trait's. Errors mirror
    /// [`FileSystem::mkdir`]'s; defaults to [`MkdirError::ReadOnlyFileSystem`].
    fn make_fifo(&self, path: impl path::Arg, mode: Mode) -> Result<(), MkdirError> {
        let _ = (path, mode);
        Err(MkdirError::ReadOnlyFileSystem)
    }

    /// Read the target of the symbolic link at `path`.
    ///
    /// Returns [`ReadLinkError::NotASymlink`] if `path` does not name a symbolic link.
    fn read_link(&self, path: impl path::Arg) -> Result<alloc::string::String, ReadLinkError>;

    /// Create a new directory
    fn mkdir(&self, path: impl path::Arg, mode: Mode) -> Result<(), MkdirError>;

    /// Remove a directory
    fn rmdir(&self, path: impl path::Arg) -> Result<(), RmdirError>;

    /// Read directory entries from a directory file descriptor.
    ///
    /// Returns a list of file/directory names (explicitly _not_ including `.` or `..`).
    fn read_dir(&self, fd: &TypedFd<Self>) -> Result<Vec<DirEntry>, ReadDirError>;

    /// Obtain the status of a file/directory/... on the file-system.
    fn file_status(&self, path: impl path::Arg) -> Result<FileStatus, FileStatusError>;

    /// Equivalent to [`Self::file_status`], but does not follow a symlink named by the FINAL path
    /// component (`lstat(2)`/`AT_SYMLINK_NOFOLLOW`): returns the symlink's own metadata, and must
    /// succeed for a dangling symlink. Earlier components resolve normally. The default body keeps
    /// the following behavior, right for an implementer with no symlinks (e.g. [`in_mem`]).
    fn symlink_metadata(&self, path: impl path::Arg) -> Result<FileStatus, FileStatusError> {
        self.file_status(path)
    }

    /// Equivalent to [`Self::file_status`], but open an open `fd` instead.
    fn fd_file_status(&self, fd: &TypedFd<Self>) -> Result<FileStatus, FileStatusError>;

    /// Get static backing data for a file, if available and supported.
    ///
    /// This method returns the (entire) underlying static byte slice if the file's contents are
    /// backed by borrowed static data (e.g., loaded via `initialize_primarily_read_heavy_file`).
    ///
    /// Returns `None` if indicating no static backing data is available/supported.
    #[expect(unused_variables, reason = "default body, non-underscored param names")]
    fn get_static_backing_data(&self, fd: &TypedFd<Self>) -> Option<&'static [u8]> {
        None
    }
}

bitflags! {
    /// `S_I*` constants for open, ...
    #[repr(transparent)]
    #[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
    pub struct Mode: c_uint {
        /// `S_IRWXU`: user (file owner) has read, write, and execute permission
        const RWXU = 0o00700;
        /// `S_IRUSR`: user has read permission
        const RUSR = 0o00400;
        /// `S_IWUSR`: user has write permission
        const WUSR = 0o00200;
        /// `S_IXUSR`: user has execute permission
        const XUSR = 0o00100;
        /// `S_IRWXG`: group has read, write, and execute permission
        const RWXG = 0o00070;
        /// `S_IRGRP`: group has read permission
        const RGRP = 0o00040;
        /// `S_IWGRP`: group has write permission
        const WGRP = 0o00020;
        /// `S_IXGRP`: group has execute permission
        const XGRP = 0o00010;
        /// `S_IRWXO`: others have read, write, and execute permission
        const RWXO = 0o00007;
        /// `S_IROTH`: others have read permission
        const ROTH = 0o00004;
        /// `S_IWOTH`: others have write permission
        const WOTH = 0o00002;
        /// `S_IXOTH`: others have execute permission
        const XOTH = 0o00001;
        /// `S_ISUID`: set-user-ID bit
        const SUID = 0o0004000;
        /// `S_ISGID`: set-group-ID bit (see inode(7)).
        const SGID = 0o0002000;
        /// `S_ISVTX`: sticky bit (see inode(7)).
        const SVTX = 0o0001000;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

/// Types of files on a file-system.
///
/// See [`FileSystem::file_status`].
#[derive(Debug, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub enum FileType {
    RegularFile,
    Directory,
    CharacterDevice,
    Symlink,
    /// A named pipe (FIFO), created by [`FileSystem::make_fifo`].
    Fifo,
}

bitflags! {
    /// `O_*` constants for use with open, ...
    #[repr(transparent)]
    #[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
    pub struct OFlags: c_uint {
        /// `O_RDONLY`: read-only
        const RDONLY = 0x0;
        /// `O_WRONLY`: write-only
        const WRONLY = 0x1;
        /// `O_RDWR`: read/write.
        ///
        /// This is not equal to `RDONLY | WRONLY`. It's a distinct flag.
        const RDWR = 0x2;
        /// `O_APPEND`: append mode
        const APPEND = 0x400;
        /// `O_ASYNC`: signal-driven I/O
        const ASYNC = 0x2000;
        /// `O_CLOEXEC`: close-on-exec flag
        const CLOEXEC = 0x80000;
        /// `O_CREAT`: if path does not exist, create it as a regular file
        const CREAT = 0x40;
        /// `O_DIRECT`: try to minimize cache effects of I/O for this file
        #[cfg(target_arch = "x86_64")]
        const DIRECT = 0x4000;
        #[cfg(target_arch = "aarch64")]
        const DIRECT = 0x10000;
        /// `O_DIRECTORY`: fail if not a directory
        #[cfg(target_arch = "x86_64")]
        const DIRECTORY = 0x10000;
        #[cfg(target_arch = "aarch64")]
        const DIRECTORY = 0x4000;
        /// `O_DSYNC`: write operations on the file will complete according to the requirements of
        /// synchronized I/O *data* integrity completion.
        const DSYNC = 0x1000;
        /// `O_EXCL`: exclusive use
        const EXCL = 0x80;
        /// `O_LARGEFILE`: allow large file support
        #[cfg(target_arch = "x86_64")]
        const LARGEFILE = 0x8000;
        #[cfg(target_arch = "aarch64")]
        const LARGEFILE = 0x20000;
        /// `O_NOATIME`: do not update access time
        const NOATIME = 0x40000;
        /// `O_NOCTTY`: do not assign controlling terminal
        const NOCTTY = 0x100;
        /// `O_NOFOLLOW`: fail if the path does not point to a regular file
        #[cfg(target_arch = "x86_64")]
        const NOFOLLOW = 0x20000;
        #[cfg(target_arch = "aarch64")]
        const NOFOLLOW = 0x8000;
        /// `O_NDELAY`: non-blocking mode (same as NONBLOCK)
        const NDELAY = 0x800;
        /// `O_NONBLOCK`: non-blocking mode (same as NDELAY)
        const NONBLOCK = 0x800;
        /// `O_PATH`: open a file descriptor for path resolution only
        const PATH = 0x200000;
        /// `O_SYNC`: write operations on the file will complete according to the requirements of
        /// synchronized I/O file integrity completion (by contrast with the synchronized I/O data
        /// integrity completion provided by `O_DSYNC`.)
        const SYNC = 0x101000;
        /// `O_TMPFILE`: create an unnamed temporary file
        #[cfg(target_arch = "x86_64")]
        const TMPFILE = 0x410000;
        #[cfg(target_arch = "aarch64")]
        const TMPFILE = 0x404000;
        /// `O_TRUNC`: truncate the file to zero length
        const TRUNC = 0x200;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;

        /// All file status flags + access modes
        const STATUS_FLAGS_MASK = Self::APPEND.bits()
            | Self::NONBLOCK.bits()
            | Self::DSYNC.bits()
            | Self::ASYNC.bits()
            | Self::DIRECT.bits()
            | Self::LARGEFILE.bits()
            | Self::NOATIME.bits()
            | Self::SYNC.bits()
            | Self::PATH.bits()
            | Self::RDONLY.bits()
            | Self::WRONLY.bits()
            | Self::RDWR.bits();
    }
}

/// The `whence` directive to [`FileSystem::seek`]
#[derive(Copy, Clone)]
pub enum SeekWhence {
    /// The file offset is set to `offset` bytes.
    RelativeToBeginning,
    /// The file offset is set to its current location plus `offset` bytes.
    RelativeToCurrentOffset,
    /// The file offset is set to the size of the file plus `offset` bytes.
    RelativeToEnd,
}

/// The status of a file/directory/... on the file-system, inspired by `stat(3type)`.
///
/// This is explicitly a non-exhaustive struct with public members. As LiteBox evolves, more
/// elements might be added to this struct, allowing file systems to provide richer information
/// about the status of files. However, users of LiteBox must not depend on the completeness or even
/// layout of this particular type.
#[non_exhaustive]
pub struct FileStatus {
    /// File type
    pub file_type: FileType,
    /// Permissions for the file
    pub mode: Mode,
    /// Size of the file, in bytes. This value considered informative if this is a regular file.
    pub size: usize,
    /// Owner of the file
    pub owner: UserInfo,
    /// Information about this particular node
    pub node_info: NodeInfo,
    /// Block size for file system I/O
    pub blksize: usize,
    /// Last access time
    pub atime: Timestamp,
    /// Last modification time
    pub mtime: Timestamp,
}

impl FileStatus {
    /// Build the [`FileStatus`] for a symlink itself (never the file it points at) -- what
    /// [`FileSystem::symlink_metadata`] returns for a path whose final component is a symlink.
    /// `target_len` is the byte length of the unresolved target. Takes no `mode`: Linux always
    /// reports a symlink's own mode as `lrwxrwxrwx`, so full `rwxrwxrwx` bits are always set.
    #[must_use]
    pub fn symlink(
        target_len: usize,
        owner: UserInfo,
        node_info: NodeInfo,
        blksize: usize,
        atime: Timestamp,
        mtime: Timestamp,
    ) -> Self {
        Self {
            file_type: FileType::Symlink,
            mode: Mode::RWXU | Mode::RWXG | Mode::RWXO,
            size: target_len,
            owner,
            node_info,
            blksize,
            atime,
            mtime,
        }
    }
}

/// A timestamp, expressed as a duration since the Unix epoch (1970-01-01T00:00:00Z).
///
/// This is a minimal, storage-only timestamp type: [`FileSystem`] implementations that support
/// [`FileSystem::set_times`] store whatever value is set here and hand it back verbatim from
/// [`FileSystem::file_status`]/[`FileSystem::fd_file_status`]; there is no implied relationship
/// with any wall clock. Callers (e.g. syscall shims) are responsible for resolving "current time"
/// before constructing a `Timestamp`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    /// Whole seconds since the Unix epoch.
    pub sec: i64,
    /// Nanosecond remainder, in `[0, 1_000_000_000)`.
    pub nsec: u32,
}

/// User information
#[derive(Clone, Copy, Debug)]
pub struct UserInfo {
    /// User ID for the owner
    pub user: u16,
    /// Group ID for the owner
    pub group: u16,
}

/// Device/Inode information
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct NodeInfo {
    /// Device number
    pub dev: usize,
    /// Inode number
    pub ino: usize,
    /// Device that is being referred to (will be `Some(...)` only if special file)
    pub rdev: Option<NonZeroUsize>,
}

/// Directory entries returned by [`FileSystem::read_dir`]
#[derive(Debug)]
#[non_exhaustive]
pub struct DirEntry {
    pub name: alloc::string::String,
    pub file_type: FileType,
    pub ino_info: Option<NodeInfo>,
}

impl UserInfo {
    /// The root user
    pub const ROOT: Self = Self { user: 0, group: 0 };
}

/// The size reported as the size of a directory.
const DEFAULT_DIRECTORY_SIZE: usize = 4096;
