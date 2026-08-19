// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Implementation of file related syscalls, e.g., `open`, `read`, `write`, etc.

use alloc::{
    ffi::CString,
    string::{String, ToString as _},
    vec,
};
use litebox::{
    event::{Events, wait::WaitError},
    fd::{FdEnabledSubsystem, MetadataError, TypedFd},
    fs::{Mode, OFlags, SeekWhence},
    mm::linux::PAGE_SIZE,
    path::{self, Arg as _},
    platform::{StdioStream, TimeProvider},
    sync::RawSyncPrimitivesProvider,
    utils::{ReinterpretSignedExt as _, ReinterpretUnsignedExt as _, TruncateExt as _},
};
use litebox_common_linux::{
    AccessFlags, AtFlags, EfdFlags, EpollCreateFlags, FcntlArg, FileDescriptorFlags, FileStat,
    InodeType, IoReadVec, IoWriteVec, IoctlArg, Statx, StatxMask, TimeParam, errno::Errno,
    signal::Signal,
};
use thiserror::Error;

use crate::{
    GlobalState, ShimFS, ShimPlatform, Task, TermiosState, UserPtr, UserPtrMut, syscalls::signal,
};
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
struct AccessUserInfo {
    user: u32,
    group: u32,
}

impl From<litebox::fs::UserInfo> for AccessUserInfo {
    fn from(value: litebox::fs::UserInfo) -> Self {
        Self {
            user: u32::from(value.user),
            group: u32::from(value.group),
        }
    }
}

/// Task state shared by `CLONE_FS`.
pub(crate) struct FsState<Platform: ShimPlatform> {
    umask: core::sync::atomic::AtomicU32,
    /// The current working directory
    ///
    /// Must end with a '/'.
    cwd: litebox::sync::RwLock<Platform, String>,
}

impl<Platform: ShimPlatform> Clone for FsState<Platform> {
    fn clone(&self) -> Self {
        Self {
            umask: self.umask.load(Ordering::Relaxed).into(),
            cwd: litebox::sync::RwLock::new(self.cwd.read().clone()),
        }
    }
}

impl<Platform: ShimPlatform> FsState<Platform> {
    pub fn new() -> Self {
        Self {
            umask: (Mode::WGRP | Mode::WOTH).bits().into(),
            cwd: litebox::sync::RwLock::new(String::from("/")),
        }
    }

    fn umask(&self) -> Mode {
        Mode::from_bits_retain(self.umask.load(Ordering::Relaxed))
    }
}

/// Task state shared by `CLONE_FILES`.
pub(crate) struct FilesState<Platform: ShimPlatform, FS: ShimFS> {
    /// The filesystem implementation, shared across tasks that share file system.
    pub(crate) fs: alloc::sync::Arc<FS>,
    pub(crate) raw_descriptor_store:
        litebox::sync::RwLock<Platform, litebox::fd::RawDescriptorStorage>,
    max_fd: AtomicUsize,
    /// Absolute path each open file fd was opened with, so `openat`/`fstatat`-family syscalls can
    /// resolve a path given relative to that fd (`dirfd`-relative resolution). Only file fds
    /// (as opposed to sockets/pipes/etc, which cannot serve as a `dirfd`) are ever inserted here.
    fd_paths: litebox::sync::RwLock<Platform, alloc::collections::BTreeMap<usize, CString>>,
}

impl<Platform: ShimPlatform, FS: ShimFS> FilesState<Platform, FS> {
    /// Duplicate the fd table for `fork()`: the new table starts as an independent copy of the
    /// current fd-number-to-descriptor mapping (each entry shares the same underlying open file
    /// description via `Arc`, matching POSIX `fork()` semantics), while `fs` (the filesystem
    /// backend itself) remains shared, since fork does not create a second filesystem.
    ///
    /// Deliberately **not** a `Clone` impl: uses
    /// [`litebox::fd::RawDescriptorStorage::fork_duplicate`], which needs `litebox` (to allocate
    /// each duplicated fd a genuinely new slot in the *global* descriptor table via
    /// [`litebox::LiteBox::descriptor_table_mut`]) -- a naive `Clone` that merely `Arc::clone`d
    /// the raw store's per-slot ownership tokens would leave the parent and child sharing the
    /// same global-table slot (and hence the same close-tracking) for every fd both processes
    /// have, so closing/dup2-ing a raw fd in *either* process would spuriously invalidate it in
    /// the *other* (see `fork_duplicate`'s doc comment for the full failure mode this avoids --
    /// this was the actual root cause of `dup2()` returning `EBADF` for shell output-redirection
    /// fds surviving a `fork()`).
    pub(crate) fn fork_duplicate(&self, litebox: &litebox::LiteBox<Platform>) -> Self {
        // `Descriptors::duplicate` (used per-subsystem below) is also `dup()`/`dup2()`'s own
        // primitive, and by design does *not* propagate `FD_CLOEXEC` -- POSIX requires a
        // duplicated fd to never inherit the original's close-on-exec flag. `fork()`, however, has
        // the opposite requirement: `FD_CLOEXEC` must survive fork() unchanged and only take
        // effect at the *child's* next `execve()`. Using `duplicate` unchanged here would
        // therefore silently clear `FD_CLOEXEC` on every fd a forked child inherits, causing an
        // fd the parent marked close-on-exec to leak into the child's own subsequent `execve()`
        // (observed in practice: `apk`'s fork()+exec() of a trigger script's `#!/bin/sh`
        // interpreter inheriting fds the parent never intended it to see). Re-read the source
        // fd's `FD_CLOEXEC` metadata and re-apply it to the freshly duplicated fd to restore
        // fork()'s actual semantics.
        fn dup_preserving_cloexec<Platform: litebox::sync::RawSyncPrimitivesProvider, Subsystem>(
            litebox: &litebox::LiteBox<Platform>,
            fd: &TypedFd<Subsystem>,
        ) -> Option<TypedFd<Subsystem>>
        where
            Subsystem: litebox::fd::FdEnabledSubsystem,
        {
            let mut dt = litebox.descriptor_table_mut();
            let cloexec = dt
                .with_metadata(fd, |flags: &FileDescriptorFlags| {
                    flags.contains(FileDescriptorFlags::FD_CLOEXEC)
                })
                .unwrap_or(false);
            let new_fd = dt.duplicate(fd)?;
            if cloexec {
                dt.set_fd_metadata(&new_fd, FileDescriptorFlags::FD_CLOEXEC);
            }
            Some(new_fd)
        }

        let raw_descriptor_store = self.raw_descriptor_store.read().fork_duplicate(
            |fd: &TypedFd<FS>| dup_preserving_cloexec(litebox, fd),
            |fd: &TypedFd<litebox::net::Network<Platform>>| dup_preserving_cloexec(litebox, fd),
            |fd: &TypedFd<litebox::pipes::Pipes<Platform>>| dup_preserving_cloexec(litebox, fd),
            |fd: &TypedFd<super::eventfd::EventfdSubsystem<Platform>>| {
                dup_preserving_cloexec(litebox, fd)
            },
            |fd: &TypedFd<super::epoll::EpollSubsystem<Platform, FS>>| {
                dup_preserving_cloexec(litebox, fd)
            },
            |fd: &TypedFd<super::unix::UnixSocketSubsystem<Platform, FS>>| {
                dup_preserving_cloexec(litebox, fd)
            },
            |fd: &TypedFd<super::pty::PtySubsystem<Platform>>| dup_preserving_cloexec(litebox, fd),
        );
        Self {
            fs: self.fs.clone(),
            raw_descriptor_store: litebox::sync::RwLock::new(raw_descriptor_store),
            max_fd: AtomicUsize::new(self.max_fd.load(Ordering::Relaxed)),
            fd_paths: litebox::sync::RwLock::new(self.fd_paths.read().clone()),
        }
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> FilesState<Platform, FS> {
    pub(crate) fn new(fs: alloc::sync::Arc<FS>) -> Self {
        Self {
            fs,
            raw_descriptor_store: litebox::sync::RwLock::new(
                litebox::fd::RawDescriptorStorage::new(),
            ),
            max_fd: AtomicUsize::new(usize::MAX),
            fd_paths: litebox::sync::RwLock::new(alloc::collections::BTreeMap::new()),
        }
    }

    pub(crate) fn set_max_fd(&self, max_fd: usize) {
        self.max_fd.store(max_fd, Ordering::Relaxed);
    }

    /// Record the absolute path a raw file fd was opened with, so it can later serve as a
    /// `dirfd` for `openat`/`fstatat`-family syscalls.
    pub(crate) fn record_fd_path(&self, raw_fd: usize, path: CString) {
        self.fd_paths.write().insert(raw_fd, path);
    }

    /// Look up the absolute path a raw file fd was opened with, if any.
    pub(crate) fn lookup_fd_path(&self, raw_fd: usize) -> Option<CString> {
        self.fd_paths.read().get(&raw_fd).cloned()
    }

    fn forget_fd_path(&self, raw_fd: usize) {
        self.fd_paths.write().remove(&raw_fd);
    }

    // Returns Ok(raw_fd) if it fits within the max limits already set up; otherwise returns the
    // Err(typed_fd)
    pub(crate) fn insert_raw_fd<Subsystem: FdEnabledSubsystem>(
        &self,
        typed_fd: TypedFd<Subsystem>,
    ) -> Result<usize, TypedFd<Subsystem>> {
        // XXX(jb): should we try to somehow enforce that it is set at the smallest
        // available/unassigned FD number?
        let mut rds = self.raw_descriptor_store.write();
        let raw_fd = rds.fd_into_raw_integer(typed_fd);
        let max_fd = self.max_fd.load(Ordering::Relaxed);
        if raw_fd > max_fd {
            let orig = rds.fd_consume_raw_integer::<Subsystem>(raw_fd).unwrap();
            return Err(alloc::sync::Arc::into_inner(orig).unwrap());
        }
        Ok(raw_fd)
    }
}

/// Registry of `flock(2)` advisory-lock state, keyed by the underlying file's `(dev, ino)`. See
/// `GlobalState::flock_registry`'s doc comment for why this is shim-wide rather than per-process.
pub(crate) type FlockRegistry<Platform> =
    alloc::collections::BTreeMap<(usize, usize), alloc::sync::Arc<FlockFile<Platform>>>;

/// `flock(2)` operation: request a shared lock.
const LOCK_SH: i32 = 1;
/// `flock(2)` operation: request an exclusive lock.
const LOCK_EX: i32 = 2;
/// `flock(2)` operation: release an existing lock.
const LOCK_UN: i32 = 8;
/// `flock(2)` operation flag: don't block if the lock can't be acquired immediately.
const LOCK_NB: i32 = 4;

/// Identifies a single open file description to a [`FlockFile`], so that `flock()` calls made
/// through fds that are `dup()`-derived from the same `open()` (which share one `FlockHolder`, as
/// entry-shared metadata -- the `Arc` clones `with_metadata`/`with_metadata_mut` produce are cheap
/// aliases of the same underlying id, not new holders) are recognized as "the same locker" --
/// matching real `flock()` semantics, where re-locking (including up/downgrading between
/// `LOCK_SH`/`LOCK_EX`) or unlocking from the same open file description never contends with
/// itself, while a *different* open file description (from an independent `open()`, or in a
/// genuinely different process after `fork()`) on the same underlying file is a distinct,
/// contending holder.
///
/// Also releases any lock this open file description holds when the *last* fd referencing it is
/// closed (i.e. when the last `Arc<FlockHolderInner>` -- including the one held inside the
/// `DescriptorEntry`'s metadata itself, not just transient clones taken by individual `flock()`
/// calls -- is actually dropped), matching the kernel's "closing any fd referring to the open file
/// description drops its flock" behavior, without needing a separate explicit close-time hook into
/// every filesystem backend's `close()` implementation.
type FlockHolder<Platform> = alloc::sync::Arc<FlockHolderInner<Platform>>;

struct FlockHolderInner<Platform: RawSyncPrimitivesProvider + TimeProvider> {
    id: u64,
    file: alloc::sync::Arc<FlockFile<Platform>>,
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> FlockHolderInner<Platform> {
    fn new(id: u64, file: alloc::sync::Arc<FlockFile<Platform>>) -> FlockHolder<Platform> {
        alloc::sync::Arc::new(Self { id, file })
    }
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> Drop for FlockHolderInner<Platform> {
    fn drop(&mut self) {
        self.file.unlock(self.id);
    }
}

/// Current lock state of a [`FlockFile`].
enum FlockLockState {
    Unlocked,
    /// Held by one or more open file descriptions in shared mode.
    Shared(alloc::collections::BTreeSet<u64>),
    /// Held by exactly one open file description in exclusive mode.
    Exclusive(u64),
}

/// Advisory-lock state for a single underlying file (identified by `(dev, ino)`), shared by every
/// open file description of that file -- including ones from entirely independent `open()` calls,
/// which is where real contention (as opposed to `dup()`-sharing, handled by [`FlockHolder`])
/// happens. One instance lives in `GlobalState::flock_registry` per distinct locked file.
pub(crate) struct FlockFile<Platform: RawSyncPrimitivesProvider + TimeProvider> {
    state: litebox::sync::Mutex<Platform, FlockLockState>,
    /// Woken whenever the lock is released or downgraded, so blocked waiters can retry.
    pollee: litebox::event::polling::Pollee<Platform>,
}

impl<Platform: RawSyncPrimitivesProvider + TimeProvider> FlockFile<Platform> {
    fn new() -> Self {
        Self {
            state: litebox::sync::Mutex::new(FlockLockState::Unlocked),
            pollee: litebox::event::polling::Pollee::new(),
        }
    }

    fn try_lock_shared(
        &self,
        holder: u64,
    ) -> Result<(), litebox::event::polling::TryOpError<Errno>> {
        let mut state = self.state.lock();
        match &mut *state {
            FlockLockState::Unlocked => {
                let mut holders = alloc::collections::BTreeSet::new();
                holders.insert(holder);
                *state = FlockLockState::Shared(holders);
                Ok(())
            }
            FlockLockState::Shared(holders) => {
                holders.insert(holder);
                Ok(())
            }
            FlockLockState::Exclusive(owner) if *owner == holder => {
                // Downgrade: same open file description already holds the exclusive lock.
                let mut holders = alloc::collections::BTreeSet::new();
                holders.insert(holder);
                *state = FlockLockState::Shared(holders);
                Ok(())
            }
            FlockLockState::Exclusive(_) => Err(litebox::event::polling::TryOpError::TryAgain),
        }
    }

    fn try_lock_exclusive(
        &self,
        holder: u64,
    ) -> Result<(), litebox::event::polling::TryOpError<Errno>> {
        let mut state = self.state.lock();
        match &*state {
            FlockLockState::Unlocked => {
                *state = FlockLockState::Exclusive(holder);
                Ok(())
            }
            FlockLockState::Exclusive(owner) if *owner == holder => Ok(()),
            FlockLockState::Shared(holders) if holders.len() == 1 && holders.contains(&holder) => {
                // Upgrade: same open file description is the sole shared-lock holder.
                *state = FlockLockState::Exclusive(holder);
                Ok(())
            }
            FlockLockState::Exclusive(_) | FlockLockState::Shared(_) => {
                Err(litebox::event::polling::TryOpError::TryAgain)
            }
        }
    }

    fn lock_shared(
        &self,
        cx: &litebox::event::wait::WaitContext<'_, Platform>,
        holder: u64,
        nonblock: bool,
    ) -> Result<u32, Errno> {
        self.pollee
            .wait(cx, nonblock, Events::IN, || self.try_lock_shared(holder))
            .map(|()| 0)
            .map_err(Errno::from)
    }

    fn lock_exclusive(
        &self,
        cx: &litebox::event::wait::WaitContext<'_, Platform>,
        holder: u64,
        nonblock: bool,
    ) -> Result<u32, Errno> {
        self.pollee
            .wait(cx, nonblock, Events::IN, || self.try_lock_exclusive(holder))
            .map(|()| 0)
            .map_err(Errno::from)
    }

    fn unlock(&self, holder: u64) {
        let mut state = self.state.lock();
        let changed = match &mut *state {
            FlockLockState::Exclusive(owner) if *owner == holder => {
                *state = FlockLockState::Unlocked;
                true
            }
            FlockLockState::Unlocked | FlockLockState::Exclusive(_) => false,
            FlockLockState::Shared(holders) => {
                let removed = holders.remove(&holder);
                if holders.is_empty() {
                    *state = FlockLockState::Unlocked;
                }
                removed
            }
        };
        drop(state);
        if changed {
            self.pollee.notify_observers(Events::IN);
        }
    }
}

/// Path in the file system
#[derive(Debug)]
enum FsPath {
    /// Absolute path
    Absolute { path: CString },
    /// Current working directory
    Cwd,
    /// Path is relative to a file descriptor
    FdRelative { fd: u32, path: CString },
    /// Fd
    Fd(u32),
}

/// Maximum size of a file path
pub const PATH_MAX: usize = 4096;

impl FsPath {
    /// Create a new `FsPath` from a dirfd and path.
    ///
    /// CWD-relative paths are resolved immediately to absolute paths.
    fn new(
        dirfd: i32,
        path: impl path::Arg,
        get_cwd: impl FnOnce() -> String,
    ) -> Result<Self, Errno> {
        let path_str = path.as_rust_str()?;
        if path_str.len() > PATH_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        let fs_path = if path_str.starts_with('/') {
            let cpath = path.to_c_str()?.into_owned();
            FsPath::Absolute { path: cpath }
        } else if dirfd >= 0 {
            let dirfd = u32::try_from(dirfd).expect("dirfd >= 0");
            if path_str.is_empty() {
                FsPath::Fd(dirfd)
            } else {
                let cpath = path.to_c_str()?.into_owned();
                FsPath::FdRelative {
                    fd: dirfd,
                    path: cpath,
                }
            }
        } else if dirfd == litebox_common_linux::AT_FDCWD {
            if path_str.is_empty() {
                FsPath::Cwd
            } else {
                // Resolve CWD-relative path to absolute.
                let mut abs = get_cwd();
                abs.push_str(path_str);
                let cpath = CString::new(abs).map_err(|_| Errno::EINVAL)?;
                FsPath::Absolute { path: cpath }
            }
        } else {
            return Err(Errno::EBADF);
        };
        Ok(fs_path)
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    fn get_umask(&self) -> Mode {
        self.fs.borrow().umask()
    }

    /// Resolve a path against the current working directory.
    pub(crate) fn resolve_path(&self, path: impl path::Arg) -> Result<CString, Errno> {
        let path_str = path.as_rust_str().map_err(|_| Errno::EINVAL)?;
        if path_str.is_empty() {
            return Err(Errno::ENOENT);
        }
        if path_str.starts_with('/') {
            CString::new(path_str.to_string()).map_err(|_| Errno::EINVAL)
        } else {
            let mut cwd = self.fs.borrow().cwd.read().clone();
            cwd.push_str(path_str);
            CString::new(cwd).map_err(|_| Errno::EINVAL)
        }
    }

    /// Join a directory's absolute path with a path given relative to it, matching the semantics
    /// `openat`/`fstatat`-family syscalls need for a `dirfd`-relative lookup.
    fn join_dir_relative_path(dir_path: &CString, relative: &CString) -> Result<CString, Errno> {
        let mut joined = dir_path.to_str().map_err(|_| Errno::EINVAL)?.to_string();
        if !joined.ends_with('/') {
            joined.push('/');
        }
        joined.push_str(relative.to_str().map_err(|_| Errno::EINVAL)?);
        CString::new(joined).map_err(|_| Errno::EINVAL)
    }

    /// Resolve `dirfd` (as recorded by a prior successful `open`/`openat` of a directory) to its
    /// absolute path, for `dirfd`-relative resolution.
    fn resolve_dirfd_path(&self, fd: u32) -> Result<CString, Errno> {
        self.files
            .borrow()
            .lookup_fd_path(fd as usize)
            .ok_or(Errno::EBADF)
    }

    /// Resolve a path relative to a dirfd.
    ///
    /// Note that an empty path is not valid for this function, and will be rejected with `ENOENT`.
    fn resolve_path_at(&self, dirfd: i32, pathname: impl path::Arg) -> Result<CString, Errno> {
        let get_cwd = || self.fs.borrow().cwd.read().clone();
        let fs_path = FsPath::new(dirfd, pathname, get_cwd)?;
        match fs_path {
            FsPath::Absolute { path } => Ok(path),
            FsPath::Cwd | FsPath::Fd(_) => Err(Errno::ENOENT),
            FsPath::FdRelative { fd, path } => {
                let dir_path = self.resolve_dirfd_path(fd)?;
                Self::join_dir_relative_path(&dir_path, &path)
            }
        }
    }

    pub(crate) fn do_open(
        &self,
        path: impl path::Arg,
        flags: OFlags,
        mode: Mode,
    ) -> Result<TypedFd<FS>, Errno> {
        let mode = mode & !self.get_umask();
        self.files
            .borrow()
            .fs
            .open(path, flags - OFlags::CLOEXEC, mode)
            .map_err(Errno::from)
    }

    fn do_openat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: OFlags,
        mode: Mode,
    ) -> Result<TypedFd<FS>, Errno> {
        let path = self.resolve_path_at(dirfd, pathname)?;
        self.do_open(path, flags, mode)
    }

    fn insert_raw_file_fd_with_path(
        &self,
        file: TypedFd<FS>,
        flags: OFlags,
        path: Option<CString>,
    ) -> Result<u32, Errno> {
        if flags.contains(OFlags::CLOEXEC) {
            let None = self
                .global
                .litebox
                .descriptor_table_mut()
                .set_fd_metadata(&file, FileDescriptorFlags::FD_CLOEXEC)
            else {
                unreachable!()
            };
        }
        // A freshly-opened `/dev/stdin`/`/dev/stdout`/`/dev/stderr` gets its own new fd distinct
        // from fds 0/1/2, but ioctl's TCGETS/TCSETS*/TIOCGWINSZ handling in `sys_ioctl` requires
        // `StdioStream` metadata to route the request correctly (see `is_a_tty`'s dispatch). Node's
        // libuv (`uv__tty_make_raw`, backing `process.stdin.setRawMode`) reopens `/dev/stdin` to
        // get a private fd for termios operations rather than reusing fd 0 directly -- without this,
        // that reopened fd passes `is_stdio`'s character-device/rdev check (it has the same
        // STDIO_NODE_INFO as the original) but has no `StdioStream` tag, so the ioctl handler bails
        // with ENOTTY before ever reaching TCSETSF, surfacing as `setRawMode ENOTTY`.
        if let Some(path) = &path
            && let Some(stream) = stdio_stream_for_path(path)
        {
            let _ = self
                .global
                .litebox
                .descriptor_table_mut()
                .set_entry_metadata(&file, stream);
            // Also tag with `StdioStatusFlags` (derived from this open's actual `flags`, unlike
            // the bootstrap fd 0/1/2's hardcoded `APPEND | RDWR` in
            // `initialize_stdio_in_shared_descriptors_table`) so `GETFL`/`SETFL` report the real
            // status flags for a reopened stdio fd, and so `do_read`'s non-blocking-stdin check
            // sees `O_NONBLOCK` when the guest passed it to `open("/dev/stdin", ...)` directly
            // instead of via a later `fcntl(F_SETFL)` -- without this, a freshly reopened
            // `/dev/stdin` fd carried no `StdioStatusFlags` metadata at all, so `do_read` could
            // never treat it as non-blocking regardless of the flags it was opened with.
            let _ = self
                .global
                .litebox
                .descriptor_table_mut()
                .set_entry_metadata(
                    &file,
                    crate::StdioStatusFlags(flags & OFlags::STATUS_FLAGS_MASK),
                );
        }
        let files = self.files.borrow();
        let raw_fd = files.insert_raw_fd(file).map_err(|file| {
            files.fs.close(&file).unwrap();
            Errno::EMFILE
        })?;
        if let Some(path) = path {
            files.record_fd_path(raw_fd, path);
        }
        Ok(u32::try_from(raw_fd).unwrap())
    }

    /// Handle syscall `umask`
    pub(crate) fn sys_umask(&self, new_mask: u32) -> Mode {
        let new_mask = Mode::from_bits_truncate(new_mask) & (Mode::RWXU | Mode::RWXG | Mode::RWXO);
        let old_mask = self
            .fs
            .borrow()
            .umask
            .swap(new_mask.bits(), Ordering::Relaxed);
        Mode::from_bits_retain(old_mask)
    }

    /// Handle syscall `open`
    pub fn sys_open(&self, path: impl path::Arg, flags: OFlags, mode: Mode) -> Result<u32, Errno> {
        let path = self.resolve_path(path)?;
        self.do_open_resolved(path, flags, mode)
    }

    /// Handle syscall `openat`
    pub fn sys_openat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: OFlags,
        mode: Mode,
    ) -> Result<u32, Errno> {
        let path = self.resolve_path_at(dirfd, pathname)?;
        self.do_open_resolved(path, flags, mode)
    }

    /// Open an already-resolved absolute `path`, routing `/dev/ptmx` and `/dev/pts/<id>` to the
    /// pty subsystem (see `syscalls::pty`) instead of the ordinary filesystem-backed path -- the
    /// underlying `FileSystem` layer has no live per-open state to back a pty pair (`Device` in
    /// `litebox::fs::devices` is stateless/`Copy`), so these two paths never reach `do_open`/`fs`
    /// at all.
    fn do_open_resolved(&self, path: CString, flags: OFlags, mode: Mode) -> Result<u32, Errno> {
        let path_str = path.to_str().unwrap_or_default();
        if path_str == "/dev/ptmx" {
            let (master, _id) = self.global.ptmx_open();
            return self.insert_raw_pty_fd(master, flags, path);
        }
        if let Some(id_str) = path_str.strip_prefix("/dev/pts/")
            && let Ok(id) = id_str.parse::<u32>()
        {
            let slave = self.global.pts_open(id)?;
            return self.insert_raw_pty_fd(slave, flags, path);
        }
        let file = self.do_open(path.clone(), flags, mode)?;
        self.insert_raw_file_fd_with_path(file, flags, Some(path))
    }

    /// Install a freshly allocated/looked-up pty fd (master via `/dev/ptmx`, slave via
    /// `/dev/pts/<id>`) into this process's raw fd table, mirroring
    /// `insert_raw_file_fd_with_path`'s `O_CLOEXEC`/path-bookkeeping handling for regular files.
    fn insert_raw_pty_fd(
        &self,
        fd: super::pty::PtyFd<Platform>,
        flags: OFlags,
        path: CString,
    ) -> Result<u32, Errno> {
        if flags.contains(OFlags::CLOEXEC) {
            let old = self
                .global
                .litebox
                .descriptor_table_mut()
                .set_fd_metadata(&fd, FileDescriptorFlags::FD_CLOEXEC);
            assert!(old.is_none());
        }
        let files = self.files.borrow();
        let raw_fd = files.insert_raw_fd(fd).map_err(|fd| {
            drop(self.global.litebox.descriptor_table_mut().remove(&fd));
            Errno::EMFILE
        })?;
        files.record_fd_path(raw_fd, path);
        Ok(u32::try_from(raw_fd).unwrap())
    }

    /// Handle syscall `ftruncate`
    pub(crate) fn sys_ftruncate(&self, fd: i32, length: usize) -> Result<(), Errno> {
        let Ok(raw_fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                raw_fd,
                |fd| files.fs.truncate(fd, length, false).map_err(Errno::from),
                |_fd| todo!("net"),
                |_fd| todo!("pipes"),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
            )
            .flatten()
    }

    /// Handle syscall `mknodat` — create a filesystem node.
    pub(crate) fn sys_mknodat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        mode_and_type: u32,
        _dev: u32,
    ) -> Result<(), Errno> {
        const FILE_TYPE_MASK: u32 = 0o170000;

        let file_type = mode_and_type & FILE_TYPE_MASK;
        let file_type = if file_type == 0 {
            // zero translates to S_IFREG
            InodeType::File
        } else {
            InodeType::try_from(file_type).map_err(|_| Errno::EINVAL)?
        };
        match file_type {
            InodeType::File => {
                let mode = Mode::from_bits_truncate(mode_and_type & !FILE_TYPE_MASK);
                let file = self.do_openat(
                    dirfd,
                    pathname,
                    OFlags::CREAT | OFlags::EXCL | OFlags::WRONLY,
                    mode,
                )?;
                let files = self.files.borrow();
                let _ = files.fs.close(&file);
            }
            // TODO: Named pipe, socket, block and char files are not supported
            InodeType::NamedPipe
            | InodeType::Socket
            | InodeType::BlockDevice
            | InodeType::CharDevice
            | InodeType::Dir => return Err(Errno::EPERM),
            InodeType::SymLink => return Err(Errno::EINVAL),
        }
        Ok(())
    }

    /// Handle syscall `unlinkat`
    pub(crate) fn sys_unlinkat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        if flags.intersects(AtFlags::AT_REMOVEDIR.complement()) {
            return Err(Errno::EINVAL);
        }

        let path = self.resolve_path_at(dirfd, pathname)?;
        if flags.contains(AtFlags::AT_REMOVEDIR) {
            self.files.borrow().fs.rmdir(path).map_err(Errno::from)
        } else {
            self.files.borrow().fs.unlink(path).map_err(Errno::from)
        }
    }

    /// Handle syscall `renameat`/`renameat2`
    pub(crate) fn sys_renameat(
        &self,
        olddirfd: i32,
        oldpath: impl path::Arg,
        newdirfd: i32,
        newpath: impl path::Arg,
        flags: u32,
    ) -> Result<(), Errno> {
        // `FileSystem::rename` doesn't support any renameat2 flags (e.g. RENAME_NOREPLACE,
        // RENAME_EXCHANGE, RENAME_WHITEOUT); reject rather than silently ignoring a semantic
        // the caller explicitly asked for, matching `sys_unlinkat`'s handling of unsupported flags.
        if flags != 0 {
            return Err(Errno::EINVAL);
        }

        let old_path = self.resolve_path_at(olddirfd, oldpath)?;
        let new_path = self.resolve_path_at(newdirfd, newpath)?;
        self.files
            .borrow()
            .fs
            .rename(old_path, new_path)
            .map_err(Errno::from)
    }

    /// Handle syscall `symlinkat`
    pub(crate) fn sys_symlinkat(
        &self,
        target: impl path::Arg,
        newdirfd: i32,
        linkpath: impl path::Arg,
    ) -> Result<(), Errno> {
        let linkpath = self.resolve_path_at(newdirfd, linkpath)?;
        self.files
            .borrow()
            .fs
            .symlink(target, linkpath)
            .map_err(Errno::from)
    }

    /// Handle syscall `read`
    ///
    /// `offset` is an optional offset to read from. If `None`, it will read from the current file position.
    /// If `Some`, it will read from the specified offset without changing the current file position.
    pub fn sys_read(&self, fd: i32, buf: &mut [u8], offset: Option<usize>) -> Result<usize, Errno> {
        let Ok(raw_fd) = u32::try_from(fd) else {
            return Err(Errno::EBADF);
        };
        self.do_read(raw_fd, buf, offset)
    }
    pub(crate) fn do_read(
        &self,
        fd: u32,
        buf: &mut [u8],
        offset: Option<usize>,
    ) -> Result<usize, Errno> {
        let files = self.files.borrow();
        // We need to do this cell dance because otherwise Rust can't recognize that the two
        // closures are mutually exclusive.
        let buf: core::cell::RefCell<&mut [u8]> = core::cell::RefCell::new(buf);
        let n = files
            .run_on_raw_fd(
                fd as usize,
                |fd| {
                    // Stdin is the one raw-fd device backed by a platform call
                    // (`StdioProvider::read_from_stdin`) that can genuinely block indefinitely
                    // with no way for litebox to cancel it mid-call (see that trait's and
                    // `stdin_ready`'s doc comments) -- unlike a regular file's `read`, which
                    // always completes immediately regardless of `O_NONBLOCK`. When the guest
                    // has set `O_NONBLOCK` on stdin (via `fcntl(F_SETFL, ...)`, tracked in
                    // `StdioStatusFlags`), honor it the same way a real Linux `read(2)` on a
                    // non-blocking fd with no pending input would: consult the platform's
                    // non-blocking readiness probe first and return `EAGAIN` instead of
                    // descending into the blocking call. This is what actually closes the
                    // `node -e` hang: libuv puts its `uv_tty_t` stdin fd into non-blocking mode
                    // and expects `EAGAIN` on an empty read, not an indefinite block -- the
                    // epoll-readiness fix on its own only helps a caller that reliably re-polls
                    // before every read, which libuv's persistent read-arm loop does not
                    // guarantee.
                    let is_nonblocking_stdin = matches!(
                        self.global
                            .litebox
                            .descriptor_table()
                            .with_metadata(fd, |stream: &StdioStream| *stream),
                        Ok(StdioStream::Stdin)
                    ) && matches!(
                        self.global
                            .litebox
                            .descriptor_table()
                            .with_metadata(fd, |crate::StdioStatusFlags(flags)| flags
                                .contains(OFlags::NONBLOCK)),
                        Ok(true)
                    );
                    if is_nonblocking_stdin && !self.global.platform.stdin_ready() {
                        return Err(Errno::EAGAIN);
                    }
                    files
                        .fs
                        .read(fd, &mut buf.borrow_mut(), offset)
                        .map_err(Errno::from)
                },
                |fd| {
                    espipe_for_non_seekable_offset(offset)?;
                    self.global.receive(
                        &self.wait_cx(),
                        fd,
                        &mut buf.borrow_mut(),
                        litebox_common_linux::ReceiveFlags::empty(),
                        None,
                    )
                },
                |fd| {
                    espipe_for_non_seekable_offset(offset)?;
                    self.global
                        .read_linux_pipe(&self.wait_cx(), fd, &mut buf.borrow_mut())
                },
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|file| {
                        let buf = &mut buf.borrow_mut();
                        if buf.len() < size_of::<u64>() {
                            return Err(Errno::EINVAL);
                        }
                        let value = file.read(&self.wait_cx())?;
                        buf[..size_of::<u64>()].copy_from_slice(&value.to_le_bytes());
                        Ok(size_of::<u64>())
                    })
                },
                |_fd| Err(Errno::EINVAL),
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|file| {
                        file.recvfrom(
                            &self.wait_cx(),
                            &mut buf.borrow_mut(),
                            litebox_common_linux::ReceiveFlags::empty(),
                            None,
                        )
                    })
                },
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|end| end.read(&self.wait_cx(), &mut buf.borrow_mut()))
                },
            )
            .flatten()?;
        // For datagrams, the returned size represents the actual size of the message,
        // which may be larger than the buffer size.
        let capped_size = n.min(buf.borrow().len());
        Ok(capped_size)
    }

    /// Handle syscall `write`
    ///
    /// `offset` is an optional offset to write to. If `None`, it will write to the current file position.
    /// If `Some`, it will write to the specified offset without changing the current file position.
    pub fn sys_write(&self, fd: i32, buf: &[u8], offset: Option<usize>) -> Result<usize, Errno> {
        let Ok(raw_fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        let files = self.files.borrow();
        let res = files
            .run_on_raw_fd(
                raw_fd,
                |fd| files.fs.write(fd, buf, offset).map_err(Errno::from),
                |fd| {
                    espipe_for_non_seekable_offset(offset)?;
                    self.global.sendto(
                        &self.wait_cx(),
                        fd,
                        buf,
                        litebox_common_linux::SendFlags::empty(),
                        None,
                    )
                },
                |fd| {
                    espipe_for_non_seekable_offset(offset)?;
                    self.global.write_linux_pipe(&self.wait_cx(), fd, buf)
                },
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|file| {
                        if buf.len() < size_of::<u64>() {
                            return Err(Errno::EINVAL);
                        }
                        let value: u64 = u64::from_le_bytes(
                            buf[..size_of::<u64>()]
                                .try_into()
                                .map_err(|_| Errno::EINVAL)?,
                        );
                        file.write(&self.wait_cx(), value)
                    })
                },
                |_fd| Err(Errno::EINVAL),
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|file| {
                        file.sendto(self, buf, litebox_common_linux::SendFlags::empty(), None)
                    })
                },
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    espipe_for_non_seekable_offset(offset)?;
                    handle.with_entry(|end| end.write(&self.wait_cx(), buf))
                },
            )
            .flatten();
        if let Err(Errno::EPIPE) = res {
            self.send_signal(Signal::SIGPIPE, signal::siginfo_kill(Signal::SIGPIPE));
        }
        res
    }

    /// Handle syscall `pread64`
    pub fn sys_pread64(&self, fd: i32, buf: &mut [u8], offset: i64) -> Result<usize, Errno> {
        let pos = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        self.sys_read(fd, buf, Some(pos))
    }

    /// Handle syscall `pwrite64`
    pub fn sys_pwrite64(&self, fd: i32, buf: &[u8], offset: i64) -> Result<usize, Errno> {
        let pos = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        self.sys_write(fd, buf, Some(pos))
    }

    fn rewind_sendfile_in_fd(&self, in_raw_fd: usize, unread_n: usize) -> Result<(), Errno> {
        if unread_n == 0 {
            return Ok(());
        }

        let rewind = isize::try_from(unread_n).map_err(|_| Errno::EOVERFLOW)?;
        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                in_raw_fd,
                |fd| {
                    files
                        .fs
                        .seek(fd, -rewind, SeekWhence::RelativeToCurrentOffset)
                        .map(|_| ())
                        .map_err(Errno::from)
                },
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
            )
            .flatten()
    }

    /// Handle syscall `sendfile`
    pub(crate) fn sys_sendfile(
        &self,
        out_fd: i32,
        in_fd: i32,
        offset_ptr: Option<UserPtrMut<i64>>,
        count: usize,
    ) -> Result<usize, Errno> {
        let Ok(in_raw_fd) = u32::try_from(in_fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        // TODO: Linux rejects `sendfile` with `EINVAL` when `out_fd` has `O_APPEND` set.
        self.check_raw_fd_exists(out_fd)?;

        let mut cur_off = offset_ptr
            .map(|p| {
                let off = p.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                if off < 0 {
                    return Err(Errno::EINVAL);
                }
                usize::try_from(off).map_err(|_| Errno::EINVAL)
            })
            .transpose()?;

        let mut kernel_buf = vec![0u8; count.min(PAGE_SIZE)];
        let mut total: usize = 0;

        while total < count {
            let to_read = (count - total).min(kernel_buf.len());

            // Non-FS sources are not seekable; Linux returns ESPIPE for any
            // non-pread-capable source when an offset is supplied, EINVAL otherwise.
            let non_fs_err = if cur_off.is_some() {
                Errno::ESPIPE
            } else {
                Errno::EINVAL
            };
            let read_result = {
                let buf_slice = &mut kernel_buf[..to_read];
                let files = self.files.borrow();
                files
                    .run_on_raw_fd(
                        in_raw_fd,
                        |fd| files.fs.read(fd, buf_slice, cur_off).map_err(Errno::from),
                        |_fd| Err(non_fs_err),
                        |_fd| Err(non_fs_err),
                        |_fd| Err(non_fs_err),
                        |_fd| Err(non_fs_err),
                        |_fd| Err(non_fs_err),
                        |_fd| Err(non_fs_err),
                    )
                    .flatten()
            };
            let read_n = match read_result {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if total == 0 => return Err(e),
                Err(_) => break,
            };

            let write_result = self.sys_write(out_fd, &kernel_buf[..read_n], None);
            let write_n = match write_result {
                Ok(n) => n,
                Err(e) => {
                    if offset_ptr.is_none() {
                        self.rewind_sendfile_in_fd(in_raw_fd, read_n)?;
                    }
                    if total == 0 {
                        return Err(e);
                    }
                    break;
                }
            };

            total += write_n;
            if let Some(ref mut off) = cur_off {
                *off += write_n;
            }
            if write_n < read_n {
                if offset_ptr.is_none() {
                    self.rewind_sendfile_in_fd(in_raw_fd, read_n - write_n)?;
                }
                break;
            }
        }

        if let (Some(p), Some(off)) = (offset_ptr, cur_off) {
            let off = i64::try_from(off).map_err(|_| Errno::EOVERFLOW)?;
            p.write_at_offset::<Platform>(0, off).ok_or(Errno::EFAULT)?;
        }

        Ok(total)
    }
}

fn espipe_for_non_seekable_offset(offset: Option<usize>) -> Result<(), Errno> {
    if offset.is_some() {
        Err(Errno::ESPIPE)
    } else {
        Ok(())
    }
}

/// Maps an absolute path to the [`StdioStream`] it should be tagged with, if it refers to one of
/// the process's standard streams.
///
/// Used to attach `StdioStream` metadata to a *freshly re-opened* `/dev/stdin`/`/dev/stdout`/
/// `/dev/stderr` fd, not just the original bootstrap fds 0/1/2 -- see the call site in
/// `insert_raw_file_fd_with_path` for why this matters (libuv reopens `/dev/stdin` to get a
/// private fd for `tcsetattr`/raw-mode operations).
fn stdio_stream_for_path(path: &CString) -> Option<StdioStream> {
    match path.to_str().ok()? {
        "/dev/stdin" => Some(StdioStream::Stdin),
        "/dev/stdout" => Some(StdioStream::Stdout),
        "/dev/stderr" => Some(StdioStream::Stderr),
        _ => None,
    }
}

const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;

pub(crate) fn try_into_whence(value: i16) -> Result<SeekWhence, i16> {
    match value {
        SEEK_SET => Ok(SeekWhence::RelativeToBeginning),
        SEEK_CUR => Ok(SeekWhence::RelativeToCurrentOffset),
        SEEK_END => Ok(SeekWhence::RelativeToEnd),
        _ => Err(value),
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `lseek`
    pub fn sys_lseek(&self, fd: i32, offset: isize, whence: SeekWhence) -> Result<usize, Errno> {
        let Ok(raw_fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                raw_fd,
                |fd| match files.fs.seek(fd, offset, whence) {
                    Ok(pos) => Ok(pos),
                    Err(litebox::fs::errors::SeekError::NotAFile) => {
                        let base: usize = match whence {
                            SeekWhence::RelativeToBeginning => 0,
                            SeekWhence::RelativeToCurrentOffset => self
                                .global
                                .litebox
                                .descriptor_table()
                                .with_metadata(fd, |off: &Diroff| off.0)
                                .unwrap_or(0),
                            SeekWhence::RelativeToEnd => {
                                return Err(Errno::EINVAL);
                            }
                        };
                        let new_pos = base.checked_add_signed(offset).ok_or(Errno::EINVAL)?;
                        self.global
                            .litebox
                            .descriptor_table_mut()
                            .set_fd_metadata(fd, Diroff(new_pos));
                        Ok(new_pos)
                    }
                    Err(e) => Err(Errno::from(e)),
                },
                |_| Err(Errno::ESPIPE),
                |_| Err(Errno::ESPIPE),
                |_| Err(Errno::ESPIPE),
                |_| Err(Errno::ESPIPE),
                |_| Err(Errno::ESPIPE),
                |_| Err(Errno::ESPIPE),
            )
            .flatten()
    }

    fn do_mkdir(&self, pathname: impl path::Arg, mode: Mode) -> Result<(), Errno> {
        let mode = mode & !self.get_umask();
        self.files
            .borrow()
            .fs
            .mkdir(pathname, mode)
            .map_err(Errno::from)
    }

    /// Handle syscall `mkdirat`
    pub(crate) fn sys_mkdirat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        mode: u32,
    ) -> Result<(), Errno> {
        let pathname = self.resolve_path_at(dirfd, pathname)?;
        self.do_mkdir(pathname, Mode::from_bits_retain(mode))
    }

    /// Handle syscall `chmod`/`fchmodat`/`fchmodat2`.
    ///
    /// `chmod(path, mode)` is dispatched as `fchmodat(AT_FDCWD, path, mode)` (see the
    /// `Sysno::chmod` mapping), matching the `mkdir`/`mkdirat` pattern already used above.
    /// `fchmodat2`'s extra `flags` argument is not currently forwarded here (only `flags == 0`,
    /// i.e. no `AT_SYMLINK_NOFOLLOW`, is meaningful on Linux for `chmod` anyway, since POSIX
    /// requires symlink permission bits to be ignored/not-followable in the first place).
    pub(crate) fn sys_fchmodat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        mode: u32,
    ) -> Result<(), Errno> {
        let pathname = self.resolve_path_at(dirfd, pathname)?;
        self.files
            .borrow()
            .fs
            .chmod(pathname, Mode::from_bits_retain(mode))
            .map_err(Errno::from)
    }

    /// Handle syscall `fchmod`.
    ///
    /// Resolves the already-open file descriptor back to the absolute path it was opened at
    /// (recorded by `do_open`, the same mechanism `fchdir`/`*at`-family dirfd resolution uses)
    /// and applies the mode change through the same path-based `FileSystem::chmod` the fs layer
    /// already implements.
    pub(crate) fn sys_fchmod(&self, fd: u32, mode: u32) -> Result<(), Errno> {
        let pathname = self.resolve_dirfd_path(fd)?;
        self.files
            .borrow()
            .fs
            .chmod(pathname, Mode::from_bits_retain(mode))
            .map_err(Errno::from)
    }

    pub(crate) fn do_close(&self, raw_fd: usize) -> Result<(), Errno> {
        self.do_close_and_replace::<FS>(raw_fd, None)
    }

    /// Close the file at `raw_fd` and optionally place a new file in the same slot.
    ///
    /// This function ensure `close` and `insert` are done atomically.
    fn do_close_and_replace<S: FdEnabledSubsystem>(
        &self,
        raw_fd: usize,
        replace: Option<TypedFd<S>>,
    ) -> Result<(), Errno> {
        enum ConsumedFd<Platform: ShimPlatform, FS: ShimFS> {
            Fs(alloc::sync::Arc<TypedFd<FS>>),
            Network(alloc::sync::Arc<TypedFd<litebox::net::Network<Platform>>>),
            Pipes(alloc::sync::Arc<TypedFd<litebox::pipes::Pipes<Platform>>>),
            Eventfd(alloc::sync::Arc<TypedFd<super::eventfd::EventfdSubsystem<Platform>>>),
            Epoll(alloc::sync::Arc<TypedFd<super::epoll::EpollSubsystem<Platform, FS>>>),
            Unix(alloc::sync::Arc<TypedFd<super::unix::UnixSocketSubsystem<Platform, FS>>>),
            Pty(alloc::sync::Arc<TypedFd<super::pty::PtySubsystem<Platform>>>),
        }

        let files = self.files.borrow();
        let mut rds = files.raw_descriptor_store.write();
        let consumed: ConsumedFd<Platform, FS> = match rds.fd_consume_raw_integer::<FS>(raw_fd) {
            Ok(fd) => ConsumedFd::Fs(fd),
            Err(litebox::fd::ErrRawIntFd::NotFound) => {
                if let Some(new_fd) = replace {
                    let success = rds.fd_into_specific_raw_integer(new_fd, raw_fd);
                    assert!(success, "raw_fd slot is empty, so insert must succeed");
                }
                return Err(Errno::EBADF);
            }
            Err(litebox::fd::ErrRawIntFd::InvalidSubsystem) => {
                if let Ok(fd) =
                    rds.fd_consume_raw_integer::<litebox::net::Network<Platform>>(raw_fd)
                {
                    ConsumedFd::Network(fd)
                } else if let Ok(fd) =
                    rds.fd_consume_raw_integer::<litebox::pipes::Pipes<Platform>>(raw_fd)
                {
                    ConsumedFd::Pipes(fd)
                } else if let Ok(fd) =
                    rds.fd_consume_raw_integer::<super::eventfd::EventfdSubsystem<Platform>>(raw_fd)
                {
                    ConsumedFd::Eventfd(fd)
                } else if let Ok(fd) =
                    rds.fd_consume_raw_integer::<super::epoll::EpollSubsystem<Platform, FS>>(raw_fd)
                {
                    ConsumedFd::Epoll(fd)
                } else if let Ok(fd) = rds
                    .fd_consume_raw_integer::<super::unix::UnixSocketSubsystem<Platform, FS>>(
                        raw_fd,
                    )
                {
                    ConsumedFd::Unix(fd)
                } else if let Ok(fd) =
                    rds.fd_consume_raw_integer::<super::pty::PtySubsystem<Platform>>(raw_fd)
                {
                    ConsumedFd::Pty(fd)
                } else {
                    unreachable!("all subsystems covered")
                }
            }
        };

        // Insert the replacement into the now-vacated slot while still holding the lock.
        if let Some(new_fd) = replace {
            let success = rds.fd_into_specific_raw_integer(new_fd, raw_fd);
            assert!(
                success,
                "we just consumed this raw_fd, so it must be available"
            );
        }
        drop(rds);
        files.forget_fd_path(raw_fd);

        match consumed {
            ConsumedFd::Fs(fd) => {
                if let Ok(raw_fd) = i32::try_from(raw_fd) {
                    self.finalize_elf_patch(raw_fd);
                }
                files.fs.close(&fd).map_err(Errno::from)
            }
            ConsumedFd::Network(fd) => self.global.close_socket(&self.wait_cx(), fd),
            ConsumedFd::Pipes(fd) => self.global.close_linux_pipe(&fd),
            ConsumedFd::Eventfd(fd) => {
                let entry = {
                    let mut dt = self.global.litebox.descriptor_table_mut();
                    dt.remove(&fd)
                };
                // do not hold any locks while dropping the entry
                drop(entry);
                Ok(())
            }
            ConsumedFd::Epoll(fd) => {
                let entry = {
                    let mut dt = self.global.litebox.descriptor_table_mut();
                    dt.remove(&fd)
                };
                // do not hold any locks while dropping the entry
                drop(entry);
                Ok(())
            }
            ConsumedFd::Unix(fd) => {
                let entry = {
                    let mut dt = self.global.litebox.descriptor_table_mut();
                    dt.remove(&fd)
                };
                // do not hold any locks while dropping the entry
                drop(entry);
                Ok(())
            }
            ConsumedFd::Pty(fd) => {
                let entry = {
                    let mut dt = self.global.litebox.descriptor_table_mut();
                    dt.remove(&fd)
                };
                // Closing the *master* side's last reference releases this shim's own held
                // template copy of the slave (see `GlobalState::ptmx_closed`); any fds a guest
                // already obtained via `/dev/pts/<id>` keep working exactly like any other
                // `dup()`'d fd surviving the original fd's close.
                if let Some(end) = &entry
                    && end.is_master()
                {
                    self.global.ptmx_closed(end.pair().id);
                }
                // do not hold any locks while dropping the entry
                drop(entry);
                Ok(())
            }
        }
    }

    /// Handle syscall `close`
    pub(crate) fn sys_close(&self, fd: i32) -> Result<(), Errno> {
        let Ok(raw_fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        self.do_close(raw_fd)
    }

    /// Handle syscall `preadv`
    pub(crate) fn sys_preadv(
        &self,
        fd: i32,
        iovec: UserPtr<IoReadVec>,
        iovcnt: usize,
        offset: i64,
    ) -> Result<usize, Errno> {
        let base_offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        self.check_raw_fd_exists(fd)?;
        check_iovcnt(iovcnt)?;
        let iovs: &[IoReadVec] = &iovec
            .to_owned_slice::<Platform>(iovcnt)
            .ok_or(Errno::EFAULT)?;
        let mut kernel_buffer = vec![0u8; PAGE_SIZE];
        read_from_iovec::<_, Platform>(iovs, &mut kernel_buffer, |buf, total| {
            let cur_offset = base_offset.checked_add(total).ok_or(Errno::EOVERFLOW)?;
            self.sys_read(fd, buf, Some(cur_offset))
        })
    }

    /// Handle syscall `pwritev`
    pub(crate) fn sys_pwritev(
        &self,
        fd: i32,
        iovec: UserPtr<IoWriteVec>,
        iovcnt: usize,
        offset: i64,
    ) -> Result<usize, Errno> {
        let base_offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        self.check_raw_fd_exists(fd)?;
        check_iovcnt(iovcnt)?;
        let iovs: &[IoWriteVec] = &iovec
            .to_owned_slice::<Platform>(iovcnt)
            .ok_or(Errno::EFAULT)?;
        // TODO: Linux ignores pwritev's offset for O_APPEND files; see the O_APPEND bug documented in pwrite(2).
        write_to_iovec::<_, Platform>(iovs, |buf, total| {
            let cur_offset = base_offset.checked_add(total).ok_or(Errno::EOVERFLOW)?;
            self.sys_write(fd, buf, Some(cur_offset))
        })
    }

    /// Handle syscall `readv`
    pub(crate) fn sys_readv(
        &self,
        fd: i32,
        iovec: UserPtr<IoReadVec>,
        iovcnt: usize,
    ) -> Result<usize, Errno> {
        self.check_raw_fd_exists(fd)?;
        check_iovcnt(iovcnt)?;
        let iovs: &[IoReadVec] = &iovec
            .to_owned_slice::<Platform>(iovcnt)
            .ok_or(Errno::EFAULT)?;
        let mut kernel_buffer = vec![0u8; PAGE_SIZE];
        // TODO: The data transfers performed by readv() and writev() are atomic: the data
        // written by writev() is written as a single block that is not intermingled with
        // output from writes in other processes
        read_from_iovec::<_, Platform>(iovs, &mut kernel_buffer, |buf, _total| {
            self.sys_read(fd, buf, None)
        })
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    fn check_raw_fd_exists(&self, fd: i32) -> Result<(), Errno> {
        let raw_fd = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        if self
            .files
            .borrow()
            .raw_descriptor_store
            .read()
            .is_alive(raw_fd)
        {
            Ok(())
        } else {
            Err(Errno::EBADF)
        }
    }
}

/// Linux's `IOV_MAX` / `UIO_MAXIOV`: the kernel rejects iovec counts above this
/// with `EINVAL` for `readv`/`writev`/`preadv`/`pwritev`.
const IOV_MAX: usize = 1024;
const SSIZE_MAX: usize = isize::MAX as usize;

fn check_iovcnt(iovcnt: usize) -> Result<(), Errno> {
    if iovcnt > IOV_MAX {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

fn check_iov_lens(iov_lens: impl IntoIterator<Item = usize>) -> Result<(), Errno> {
    let mut total = 0usize;
    for iov_len in iov_lens {
        total = total.checked_add(iov_len).ok_or(Errno::EINVAL)?;
        if total > SSIZE_MAX {
            return Err(Errno::EINVAL);
        }
    }
    Ok(())
}

/// Drain reads into a sequence of user iovecs.
fn read_from_iovec<F, Platform: ShimPlatform>(
    iovs: &[IoReadVec],
    kernel_buffer: &mut [u8],
    mut read_fn: F,
) -> Result<usize, Errno>
where
    F: FnMut(&mut [u8], usize) -> Result<usize, Errno>,
{
    check_iov_lens(iovs.iter().map(|iov| iov.iov_len))?;

    let bail = |total: usize, e: Errno| if total > 0 { Ok(total) } else { Err(e) };
    let mut total_read = 0;
    'outer: for iov in iovs {
        let iov_base = iov.iov_base;
        let iov_len = iov.iov_len;
        if iov_len == 0 {
            continue;
        }
        let mut iov_filled = 0;
        while iov_filled < iov_len {
            let to_read = (iov_len - iov_filled).min(kernel_buffer.len());
            let size = match read_fn(&mut kernel_buffer[..to_read], total_read) {
                Ok(0) => break 'outer,
                Ok(s) => s,
                Err(e) => return bail(total_read, e),
            };
            if iov_base
                .copy_from_slice::<Platform>(iov_filled, &kernel_buffer[..size])
                .is_none()
            {
                return bail(total_read, Errno::EFAULT);
            }
            iov_filled += size;
            total_read += size;
            if size < to_read {
                // Short read from the source — treat as EOF for the remaining iovecs.
                break 'outer;
            }
        }
    }
    Ok(total_read)
}

/// Drain writes from a sequence of user iovecs.
///
/// `write_fn` receives the contents of each iovec along with the total number of
/// bytes already written from earlier iovecs.
pub(super) fn write_to_iovec<F, Platform: ShimPlatform>(
    iovs: &[IoWriteVec],
    mut write_fn: F,
) -> Result<usize, Errno>
where
    F: FnMut(&[u8], usize) -> Result<usize, Errno>,
{
    check_iov_lens(iovs.iter().map(|iov| iov.iov_len))?;

    // If any bytes have already been delivered from earlier iovecs, an error
    // collapses to `Ok(total)` so partial progress is reported to user space.
    let bail = |total: usize, e: Errno| if total > 0 { Ok(total) } else { Err(e) };
    let mut kernel_buffer = alloc::vec::Vec::new();
    let mut total_written = 0;
    'outer: for iov in iovs {
        let iov_base = iov.iov_base;
        let iov_len = iov.iov_len;
        if iov_len == 0 {
            continue;
        }
        if kernel_buffer.is_empty() {
            kernel_buffer.resize(PAGE_SIZE, 0);
        }
        let mut iov_written = 0;
        while iov_written < iov_len {
            let to_write = (iov_len - iov_written).min(kernel_buffer.len());
            let base_offset = isize::try_from(iov_written).unwrap();
            for (byte_offset, byte) in (0_isize..).zip(kernel_buffer[..to_write].iter_mut()) {
                let Some(value) = iov_base.read_at_offset::<Platform>(base_offset + byte_offset)
                else {
                    return bail(total_written, Errno::EFAULT);
                };
                *byte = value;
            }
            let size = match write_fn(&kernel_buffer[..to_write], total_written) {
                Ok(size) => size,
                Err(err) => return bail(total_written, err),
            };
            iov_written += size;
            total_written += size;
            if size < to_write {
                // Okay to transfer fewer bytes than requested.
                break 'outer;
            }
        }
    }
    Ok(total_written)
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `writev`
    pub(crate) fn sys_writev(
        &self,
        fd: i32,
        iovec: UserPtr<IoWriteVec>,
        iovcnt: usize,
    ) -> Result<usize, Errno> {
        self.check_raw_fd_exists(fd)?;
        check_iovcnt(iovcnt)?;
        let iovs: &[IoWriteVec] = &iovec
            .to_owned_slice::<Platform>(iovcnt)
            .ok_or(Errno::EFAULT)?;
        // TODO: The data transfers performed by readv() and writev() are atomic: the data
        // written by writev() is written as a single block that is not intermingled with
        // output from writes in other processes
        write_to_iovec::<_, Platform>(iovs, |buf, _total| self.sys_write(fd, buf, None))
    }

    fn validate_access_mode(mode: &AccessFlags) -> Result<(), Errno> {
        let valid_mode = AccessFlags::R_OK | AccessFlags::W_OK | AccessFlags::X_OK;
        if mode.intersects(valid_mode.complement()) {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn do_access_mode(
        mode: Mode,
        owner: AccessUserInfo,
        caller: AccessUserInfo,
        access_mode: &AccessFlags,
    ) -> Result<(), Errno> {
        if access_mode.is_empty() {
            return Ok(());
        }
        if caller.user == 0 {
            if access_mode.contains(AccessFlags::X_OK)
                && !mode.intersects(Mode::XUSR | Mode::XGRP | Mode::XOTH)
            {
                return Err(Errno::EACCES);
            }
            return Ok(());
        }
        // TODO: Linux also uses group bits when `owner.group` is in the caller's supplementary
        // group list. `AccessUserInfo` only carries the real/effective primary group today.
        let (read, write, execute) = if caller.user == owner.user {
            (Mode::RUSR, Mode::WUSR, Mode::XUSR)
        } else if caller.group == owner.group {
            (Mode::RGRP, Mode::WGRP, Mode::XGRP)
        } else {
            (Mode::ROTH, Mode::WOTH, Mode::XOTH)
        };
        if access_mode.contains(AccessFlags::R_OK) && !mode.contains(read) {
            return Err(Errno::EACCES);
        }
        if access_mode.contains(AccessFlags::W_OK) && !mode.contains(write) {
            return Err(Errno::EACCES);
        }
        if access_mode.contains(AccessFlags::X_OK) && !mode.contains(execute) {
            return Err(Errno::EACCES);
        }
        Ok(())
    }

    fn access_user(&self, flags: &AtFlags) -> AccessUserInfo {
        if flags.contains(AtFlags::AT_EACCESS) {
            AccessUserInfo {
                user: self.credentials.euid,
                group: self.credentials.egid,
            }
        } else {
            AccessUserInfo {
                user: self.credentials.uid,
                group: self.credentials.gid,
            }
        }
    }

    fn do_access(
        &self,
        pathname: impl path::Arg,
        mode: AccessFlags,
        caller: AccessUserInfo,
    ) -> Result<(), Errno> {
        let status = self.files.borrow().fs.file_status(pathname)?;
        let owner = status.owner.into();
        Self::do_access_mode(status.mode, owner, caller, &mode)
    }

    /// Handle syscall `faccessat`
    pub(crate) fn sys_faccessat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        mode: AccessFlags,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        let supported_flags =
            AtFlags::AT_EACCESS | AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH;
        // TODO: `AT_SYMLINK_NOFOLLOW` is accepted for Linux compatibility, but LiteBox file
        // status lookups do not currently follow symlinks in any backend.
        if flags.intersects(supported_flags.complement()) {
            return Err(Errno::EINVAL);
        }

        Self::validate_access_mode(&mode)?;
        let caller = self.access_user(&flags);
        let get_cwd = || self.fs.borrow().cwd.read().clone();
        let fs_path = FsPath::new(dirfd, pathname, get_cwd)?;
        match fs_path {
            FsPath::Absolute { path } => self.do_access(path, mode, caller),
            FsPath::Cwd if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                let cwd = get_cwd();
                self.do_access(cwd, mode, caller)
            }
            FsPath::Fd(fd) if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                let stat: FileStat = descriptor_stat(fd as usize, self)?;
                let owner = AccessUserInfo {
                    user: stat.st_uid,
                    group: stat.st_gid,
                };
                Self::do_access_mode(
                    Mode::from_bits_truncate(stat.st_mode & 0o7777),
                    owner,
                    caller,
                    &mode,
                )
            }
            FsPath::Cwd | FsPath::Fd(_) => Err(Errno::ENOENT),
            FsPath::FdRelative { .. } => {
                log_unsupported!("fd-relative faccessat is not supported yet");
                Err(Errno::EINVAL)
            }
        }
    }

    /// Read the target of a symbolic link
    ///
    /// The caller must pass an absolute path.
    ///
    /// Handles the hardcoded `/proc/self/fd/<fd>` case, then falls through to real symlinks
    /// stored on the filesystem (e.g. shared-library symlinks like `libfoo.so -> libfoo.so.1`
    /// extracted by a package manager).
    fn do_readlink(&self, fullpath: &str) -> Result<String, Errno> {
        if let Some(stripped) = fullpath.strip_prefix("/proc/self/fd/") {
            let fd = stripped.parse::<u32>().map_err(|_| Errno::EINVAL)?;
            match fd {
                0 => return Ok("/dev/stdin".to_string()),
                1 => return Ok("/dev/stdout".to_string()),
                2 => return Ok("/dev/stderr".to_string()),
                _ => {
                    // Any other fd: this used to unconditionally panic, crashing the whole
                    // runner on something as ordinary as Python's
                    // `os.readlink(f"/proc/self/fd/{fd}")` (used by e.g. introspection/sandboxing
                    // libraries to see what a descriptor points to) or a shell's `<()` process
                    // substitution. If the fd was opened from a real path, return that path (the
                    // common case: a plain file); otherwise -- a pipe/socket/eventfd/pty/etc,
                    // none of which have a filesystem path -- fall back to a synthetic
                    // descriptor string, matching the *spirit* of real Linux's
                    // "pipe:[12345]"/"socket:[12345]"/"anon_inode:[eventfd]" (without trying to
                    // replicate its exact per-kind naming or inode numbers).
                    self.check_raw_fd_exists(i32::try_from(fd).map_err(|_| Errno::EBADF)?)?;
                    return Ok(self
                        .files
                        .borrow()
                        .lookup_fd_path(fd as usize)
                        .and_then(|p| p.into_string().ok())
                        .unwrap_or_else(|| alloc::format!("anon_inode:[fd{fd}]")));
                }
            }
        }

        self.files
            .borrow()
            .fs
            .read_link(fullpath)
            .map_err(Errno::from)
    }

    /// Handle syscall `readlink`
    pub fn sys_readlink(&self, pathname: impl path::Arg, buf: &mut [u8]) -> Result<usize, Errno> {
        self.sys_readlinkat(litebox_common_linux::AT_FDCWD, pathname, buf)
    }

    /// Handle syscall `readlinkat`
    pub fn sys_readlinkat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        buf: &mut [u8],
    ) -> Result<usize, Errno> {
        let pathname = self.resolve_path_at(dirfd, pathname)?;
        let path = self.do_readlink(pathname.to_str().map_err(|_| Errno::EINVAL)?)?;
        let bytes = path.as_bytes();
        let min_len = core::cmp::min(buf.len(), bytes.len());
        buf[..min_len].copy_from_slice(&bytes[..min_len]);
        Ok(min_len)
    }
}

fn descriptor_stat<Platform: ShimPlatform, FS: ShimFS, T>(
    raw_fd: usize,
    task: &Task<Platform, FS>,
) -> Result<T, Errno>
where
    T: From<litebox::fs::FileStatus> + From<FileStat>,
{
    // TODO: give correct values for the synthesized branches.
    let synthetic = |mode_bits: u32, blksize: usize| FileStat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 1,
        st_mode: mode_bits.trunc(),
        st_uid: 0,
        st_gid: 0,
        st_rdev: 0,
        st_size: 0,
        #[cfg(target_arch = "aarch64")]
        st_blksize: blksize as i32,
        #[cfg(not(target_arch = "aarch64"))]
        st_blksize: blksize,
        st_blocks: 0,
        ..Default::default()
    };
    let socket_mode = litebox_common_linux::InodeType::Socket as u32
        | (Mode::RWXU | Mode::RWXG | Mode::RWXO).bits();
    let rw_user_mode = (Mode::RUSR | Mode::WUSR).bits();
    let files = task.files.borrow();
    files
        .run_on_raw_fd(
            raw_fd,
            |fd| {
                files
                    .fs
                    .fd_file_status(fd)
                    .map(T::from)
                    .map_err(Errno::from)
            },
            |_fd| Ok(T::from(synthetic(socket_mode, 4096))),
            |fd| {
                Ok(T::from(synthetic(
                    task.global.linux_pipe_mode_bits(fd)?,
                    4096,
                )))
            },
            |_fd| Ok(T::from(synthetic(rw_user_mode, 4096))),
            |_fd| Ok(T::from(synthetic(rw_user_mode, 0))),
            |_fd| Ok(T::from(synthetic(socket_mode, 4096))),
            |_fd| {
                Ok(T::from(synthetic(
                    litebox_common_linux::InodeType::CharDevice as u32 | rw_user_mode,
                    0,
                )))
            },
        )
        .flatten()
}

pub(crate) fn get_file_descriptor_flags<Platform: ShimPlatform, FS: ShimFS>(
    raw_fd: usize,
    global: &GlobalState<Platform, FS>,
    files: &FilesState<Platform, FS>,
) -> Result<FileDescriptorFlags, Errno> {
    // Currently, only one such flag is defined: FD_CLOEXEC, the close-on-exec flag.
    // See https://www.man7.org/linux/man-pages/man2/F_GETFD.2const.html
    fn get_flags<Platform: ShimPlatform, FS: ShimFS, S: FdEnabledSubsystem>(
        global: &GlobalState<Platform, FS>,
        fd: &TypedFd<S>,
    ) -> FileDescriptorFlags {
        global
            .litebox
            .descriptor_table()
            .with_metadata(fd, |flags: &FileDescriptorFlags| *flags)
            .unwrap_or(FileDescriptorFlags::empty())
    }
    files.run_on_raw_fd(
        raw_fd,
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
        |fd| get_flags(global, fd),
    )
}

fn set_file_descriptor_flags<Platform: ShimPlatform, FS: ShimFS>(
    raw_fd: usize,
    global: &GlobalState<Platform, FS>,
    files: &FilesState<Platform, FS>,
    flags: FileDescriptorFlags,
) -> Result<(), Errno> {
    fn set_flags<Platform: ShimPlatform, FS: ShimFS, S: FdEnabledSubsystem>(
        global: &GlobalState<Platform, FS>,
        fd: &TypedFd<S>,
        flags: FileDescriptorFlags,
    ) {
        let _old = global
            .litebox
            .descriptor_table_mut()
            .set_fd_metadata(fd, flags);
    }

    files.run_on_raw_fd(
        raw_fd,
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
        |fd| set_flags(global, fd, flags),
    )?;
    Ok(())
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Get the file status of `pathname`.
    ///
    /// The `pathname` must be absolute.
    fn do_stat<T: From<litebox::fs::FileStatus>>(
        &self,
        pathname: impl path::Arg,
        follow_symlink: bool,
    ) -> Result<T, Errno> {
        let normalized_path = pathname.normalized()?;
        let path = if follow_symlink {
            self.do_readlink(normalized_path.as_str())
                .unwrap_or(normalized_path)
        } else {
            normalized_path
        };
        let status = self.files.borrow().fs.file_status(path)?;
        Ok(T::from(status))
    }

    /// Handle syscall `stat`
    pub fn sys_stat(&self, pathname: impl path::Arg) -> Result<FileStat, Errno> {
        let pathname = self.resolve_path(pathname)?;
        self.do_stat(pathname, true)
    }

    /// Handle syscall `lstat`
    ///
    /// `lstat` is identical to `stat`, except that if `pathname` is a symbolic link,
    /// then it returns information about the link itself, not the file that the link refers to.
    /// TODO: we do not support symbolic links yet.
    pub fn sys_lstat(&self, pathname: impl path::Arg) -> Result<FileStat, Errno> {
        let pathname = self.resolve_path(pathname)?;
        self.do_stat(pathname, false)
    }

    /// Handle syscall `fstat`
    pub fn sys_fstat(&self, fd: i32) -> Result<FileStat, Errno> {
        let Ok(raw_fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        descriptor_stat(raw_fd, self)
    }

    fn do_fstatat<T>(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: AtFlags,
    ) -> Result<T, Errno>
    where
        T: From<litebox::fs::FileStatus> + From<FileStat>,
    {
        let get_cwd = || self.fs.borrow().cwd.read().clone();
        let fs_path = FsPath::new(dirfd, pathname, get_cwd)?;
        match fs_path {
            FsPath::Absolute { path } => {
                self.do_stat(path, !flags.contains(AtFlags::AT_SYMLINK_NOFOLLOW))
            }
            FsPath::Cwd if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                Ok(T::from(self.files.borrow().fs.file_status(get_cwd())?))
            }
            FsPath::Fd(fd) if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                descriptor_stat(fd as usize, self)
            }
            FsPath::Cwd | FsPath::Fd(_) => Err(Errno::ENOENT),
            FsPath::FdRelative { fd, path } => {
                let dir_path = self.resolve_dirfd_path(fd)?;
                let joined = Self::join_dir_relative_path(&dir_path, &path)?;
                self.do_stat(joined, !flags.contains(AtFlags::AT_SYMLINK_NOFOLLOW))
            }
        }
    }

    /// Handle syscall `newfstatat`
    pub(crate) fn sys_newfstatat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: AtFlags,
    ) -> Result<FileStat, Errno> {
        let current_support_flags = AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_NOFOLLOW;
        if flags.intersects(current_support_flags.complement()) {
            log_unsupported!("unsupported flags: {flags:?}");
            return Err(Errno::EINVAL);
        }

        self.do_fstatat(dirfd, pathname, flags)
    }

    /// Handle syscall `statx`
    pub(crate) fn sys_statx(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        flags: AtFlags,
        mask: StatxMask,
    ) -> Result<Statx, Errno> {
        if mask.contains(StatxMask::STATX__RESERVED) {
            return Err(Errno::EINVAL);
        }
        // `AT_NO_AUTOMOUNT` and the `AT_STATX_*` sync
        // hints are accepted as no-ops since LiteBox filesystems
        // do not automount or sync to a remote.
        let allowed = AtFlags::AT_EMPTY_PATH
            | AtFlags::AT_NO_AUTOMOUNT
            | AtFlags::AT_SYMLINK_NOFOLLOW
            | AtFlags::AT_STATX_FORCE_SYNC
            | AtFlags::AT_STATX_DONT_SYNC;
        if flags.intersects(allowed.complement()) {
            log_unsupported!("unsupported statx flags: {flags:?}");
            return Err(Errno::EINVAL);
        }
        if flags.contains(AtFlags::AT_STATX_FORCE_SYNC | AtFlags::AT_STATX_DONT_SYNC) {
            return Err(Errno::EINVAL);
        }

        // `mask` is informational past this point: the underlying FS doesn't
        // support field selection, so we always fill the basic stats and
        // report the actual filled set via `Statx::stx_mask`. Matches Linux's
        // documented behavior of returning more than what was asked.
        self.do_fstatat(dirfd, pathname, flags)
    }

    /// Resolve a `(atime_spec, mtime_spec)` pair of raw `struct timespec` values (as read from
    /// the guest's `times[2]` array, or synthesized for `times == NULL`) into the
    /// `Option<litebox::fs::Timestamp>` pair `FileSystem::set_times` expects: `UTIME_NOW`
    /// resolves against the shim's current wall-clock time, `UTIME_OMIT` becomes `None` (leave
    /// unchanged), and any other value is taken as an explicit timestamp.
    fn resolve_utimes_pair(
        &self,
        atime: litebox_common_linux::Timespec,
        mtime: litebox_common_linux::Timespec,
    ) -> Result<
        (
            Option<litebox::fs::Timestamp>,
            Option<litebox::fs::Timestamp>,
        ),
        Errno,
    > {
        let resolve_one =
            |ts: litebox_common_linux::Timespec| -> Result<Option<litebox::fs::Timestamp>, Errno> {
                #[allow(clippy::cast_possible_wrap)]
                let tv_nsec = ts.tv_nsec as i64;
                if tv_nsec == litebox_common_linux::UTIME_OMIT {
                    return Ok(None);
                }
                if tv_nsec == litebox_common_linux::UTIME_NOW {
                    let now = self.real_time_as_duration_since_epoch();
                    return Ok(Some(litebox::fs::Timestamp {
                        sec: now.as_secs().reinterpret_as_signed(),
                        nsec: now.subsec_nanos(),
                    }));
                }
                if !(0..1_000_000_000).contains(&tv_nsec) {
                    return Err(Errno::EINVAL);
                }
                Ok(Some(litebox::fs::Timestamp {
                    sec: ts.tv_sec,
                    nsec: ts.tv_nsec.trunc(),
                }))
            };
        Ok((resolve_one(atime)?, resolve_one(mtime)?))
    }

    /// Handle syscall `utimensat`.
    ///
    /// Also covers `futimens`/`futimesat`/`utimes`/`utime`, which on this shim's target (musl
    /// libc, e.g. Alpine) are all implemented purely as userspace wrappers around this syscall
    /// (`futimens(fd, times)` in particular compiles down to `utimensat(fd, NULL, times, 0)`),
    /// so no separate handling of those syscall numbers is needed.
    pub(crate) fn sys_utimensat(
        &self,
        dirfd: i32,
        pathname: impl path::Arg,
        times: Option<(
            litebox_common_linux::Timespec,
            litebox_common_linux::Timespec,
        )>,
        flags: AtFlags,
    ) -> Result<(), Errno> {
        let allowed = AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_NOFOLLOW;
        if flags.intersects(allowed.complement()) {
            log_unsupported!("unsupported utimensat flags: {flags:?}");
            return Err(Errno::EINVAL);
        }

        let now = || {
            let now = self.real_time_as_duration_since_epoch();
            litebox::fs::Timestamp {
                sec: now.as_secs().reinterpret_as_signed(),
                nsec: now.subsec_nanos(),
            }
        };
        let (atime, mtime) = match times {
            None => (Some(now()), Some(now())),
            Some((a, m)) => self.resolve_utimes_pair(a, m)?,
        };

        let get_cwd = || self.fs.borrow().cwd.read().clone();
        let fs_path = FsPath::new(dirfd, pathname, get_cwd)?;
        let path = match fs_path {
            FsPath::Absolute { path } => path,
            FsPath::Cwd if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                get_cwd().as_str().to_c_str()?.into_owned()
            }
            FsPath::Fd(fd) if flags.contains(AtFlags::AT_EMPTY_PATH) => {
                self.resolve_dirfd_path(fd)?
            }
            FsPath::Cwd | FsPath::Fd(_) => return Err(Errno::ENOENT),
            FsPath::FdRelative { fd, path } => {
                let dir_path = self.resolve_dirfd_path(fd)?;
                Self::join_dir_relative_path(&dir_path, &path)?
            }
        };

        self.files
            .borrow()
            .fs
            .set_times(path, atime, mtime)
            .map_err(Errno::from)
    }

    pub(crate) fn sys_fcntl(&self, fd: i32, arg: FcntlArg) -> Result<u32, Errno> {
        let Ok(desc) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };

        let files = self.files.borrow();
        match arg {
            FcntlArg::GETFD => Ok(get_file_descriptor_flags(desc, &self.global, &files)?.bits()),
            FcntlArg::SETFD(flags) => {
                set_file_descriptor_flags(desc, &self.global, &files, flags).map(|()| 0)
            }
            FcntlArg::GETFL => {
                macro_rules! getfl_from_metadata {
                    ($fd:expr, $MetaType:path) => {
                        Ok(self
                            .global
                            .litebox
                            .descriptor_table()
                            .with_metadata($fd, |$MetaType(flags)| {
                                *flags & OFlags::STATUS_FLAGS_MASK
                            })
                            .unwrap_or(OFlags::empty()))
                    };
                }
                macro_rules! getfl_from_handle {
                    ($fd:ident) => {{
                        // TODO: Consider shared metadata table?
                        let handle = self
                            .global
                            .litebox
                            .descriptor_table()
                            .entry_handle($fd)
                            .ok_or(Errno::EBADF)?;
                        handle.with_entry(|file| Ok(file.get_status()))
                    }};
                }
                Ok(files
                    .run_on_raw_fd(
                        desc,
                        |fd| getfl_from_metadata!(fd, crate::StdioStatusFlags),
                        |fd| getfl_from_metadata!(fd, crate::syscalls::net::SocketOFlags),
                        |fd| self.global.linux_pipe_status_flags(fd),
                        |fd| getfl_from_handle!(fd),
                        |fd| getfl_from_handle!(fd),
                        |fd| getfl_from_handle!(fd),
                        |fd| getfl_from_handle!(fd),
                    )
                    .flatten()?
                    .bits())
            }
            FcntlArg::SETFL(flags) => {
                let setfl_mask = OFlags::APPEND
                    | OFlags::NONBLOCK
                    | OFlags::NDELAY
                    | OFlags::DIRECT
                    | OFlags::NOATIME;
                let flags = flags & setfl_mask;
                macro_rules! toggle_flags {
                    ($fd:ident) => {{
                        // TODO: Consider shared metadata table?
                        let handle = self
                            .global
                            .litebox
                            .descriptor_table()
                            .entry_handle($fd)
                            .ok_or(Errno::EBADF)?;
                        handle.with_entry(|file| {
                            let diff = (file.get_status() & setfl_mask) ^ flags;
                            if diff.intersects(OFlags::APPEND | OFlags::DIRECT | OFlags::NOATIME) {
                                log_unsupported!("unsupported flags");
                            }
                            file.set_status(flags & setfl_mask, true);
                            file.set_status(flags.complement() & setfl_mask, false);
                        });
                    }};
                }
                macro_rules! setfl_in_metadata {
                    ($fd:expr, $MetaType:path, $no_metadata_msg:expr) => {
                        setfl_in_metadata!($fd, $MetaType, $no_metadata_msg, |diff: OFlags| {
                            if diff.intersects(OFlags::APPEND | OFlags::DIRECT | OFlags::NOATIME) {
                                log_unsupported!("unsupported flags");
                            }
                        })
                    };
                    ($fd:expr, $MetaType:path, $no_metadata_msg:expr, $check_diff:expr) => {
                        self.global
                            .litebox
                            .descriptor_table_mut()
                            .with_metadata_mut($fd, |$MetaType(f)| {
                                let diff = (*f & setfl_mask) ^ flags;
                                $check_diff(diff);
                                f.toggle(diff);
                            })
                            .map_err(|err| match err {
                                MetadataError::ClosedFd => Errno::EBADF,
                                MetadataError::NoSuchMetadata => $no_metadata_msg,
                            })
                    };
                }
                files.run_on_raw_fd(
                    desc,
                    |fd| {
                        // Most `fd`s dispatched here are plain regular files (not stdio), which
                        // carry no `StdioStatusFlags` metadata at all. LiteBox's `FileSystem`
                        // trait has no mechanism to store or honor per-fd status flags for
                        // regular files (mirroring `GETFL`'s `fs` closure above, which likewise
                        // falls back to `OFlags::empty()` when this metadata is absent). On real
                        // Linux, `fcntl(F_SETFL, ...)` on a regular file is accepted but has no
                        // effect on read/write blocking behavior, so treat a missing-metadata fd
                        // here as a successful no-op rather than an error.
                        match self
                            .global
                            .litebox
                            .descriptor_table_mut()
                            .with_metadata_mut(fd, |crate::StdioStatusFlags(f)| {
                                let diff = (*f & setfl_mask) ^ flags;
                                if diff
                                    .intersects(OFlags::APPEND | OFlags::DIRECT | OFlags::NOATIME)
                                {
                                    log_unsupported!("unsupported flags");
                                }
                                f.toggle(diff);
                            }) {
                            Ok(()) | Err(MetadataError::NoSuchMetadata) => Ok(()),
                            Err(MetadataError::ClosedFd) => Err(Errno::EBADF),
                        }
                    },
                    |fd| {
                        setfl_in_metadata!(
                            fd,
                            crate::syscalls::net::SocketOFlags,
                            unreachable!("all sockets have SocketOFlags when created")
                        )
                    },
                    |fd| {
                        self.global
                            .set_linux_pipe_status_flags(fd, flags, setfl_mask)
                    },
                    |fd| {
                        toggle_flags!(fd);
                        Ok(())
                    },
                    |_fd| todo!("epoll"),
                    |fd| {
                        toggle_flags!(fd);
                        Ok(())
                    },
                    |fd| {
                        toggle_flags!(fd);
                        Ok(())
                    },
                )??;
                Ok(0)
            }
            FcntlArg::GETLK(lock) => {
                self.files
                    .borrow()
                    .run_on_raw_fd(
                        desc,
                        |_fd| {
                            let mut flock =
                                lock.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                            let lock_type = litebox_common_linux::FlockType::try_from(flock.type_)
                                .map_err(|_| Errno::EINVAL)?;
                            if let litebox_common_linux::FlockType::Unlock = lock_type {
                                return Err(Errno::EINVAL);
                            }

                            // Note LiteBox does not support multiple processes yet, and one process
                            // can always acquire the lock it owns, so return `Unlock` unconditionally.
                            flock.type_ = litebox_common_linux::FlockType::Unlock as i16;
                            lock.write_at_offset::<Platform>(0, flock)
                                .ok_or(Errno::EFAULT)?;
                            Ok(0)
                        },
                        // Real Linux's fcntl(2) record locks (F_GETLK/F_SETLK/F_SETLKW) only
                        // apply to regular files; calling them on a socket or pipe fd returns
                        // EINVAL, not a panic.
                        |_fd| Err(Errno::EINVAL),
                        |_fd| Err(Errno::EINVAL),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                    )
                    .flatten()
            }
            FcntlArg::SETLK(lock) | FcntlArg::SETLKW(lock) => {
                self.files
                    .borrow()
                    .run_on_raw_fd(
                        desc,
                        |_fd| {
                            let flock = lock.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                            let _ = litebox_common_linux::FlockType::try_from(flock.type_)
                                .map_err(|_| Errno::EINVAL)?;

                            // Note LiteBox does not support multiple processes yet, and one process
                            // can always acquire the lock it owns, so we don't need to maintain anything.
                            Ok(0)
                        },
                        |_fd| Err(Errno::EINVAL),
                        |_fd| Err(Errno::EINVAL),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                        |_fd| Err(Errno::EBADF),
                    )
                    .flatten()
            }
            FcntlArg::DUPFD { cloexec, min_fd } => {
                let new_file = self
                    .do_dup_inner(
                        desc,
                        if cloexec {
                            OFlags::CLOEXEC
                        } else {
                            OFlags::empty()
                        },
                        DupFdRequest::LowestAtOrAbove(min_fd as usize),
                    )
                    .map_err(|e| match e {
                        DupFdError::BadFd => Errno::EBADF,
                        DupFdError::TooManyFiles => Errno::EMFILE,
                        DupFdError::TargetFdExceedsLimit => Errno::EINVAL,
                    })?;
                Ok(new_file.try_into().unwrap())
            }
            _ => unimplemented!(),
        }
    }

    /// Handle syscall `flock`.
    ///
    /// `flock(2)` locks are associated with the *open file description*, not the file descriptor
    /// number: two fds obtained via `dup()`/`dup2()`/`dup3()` (or inherited across `fork()`) from
    /// the same `open()` call share one lock and release/re-acquire it together, while two
    /// independent `open()` calls on the same path get independent open file descriptions whose
    /// locks nonetheless *contend* with each other (real kernel flock semantics: the lock is
    /// conceptually attached to the open file description, but mutual exclusion is checked against
    /// every other open file description of the same underlying file).
    ///
    /// This is implemented with two pieces of state:
    ///   - [`FlockFile`]: one per underlying file (keyed by `(dev, ino)` in
    ///     `GlobalState::flock_registry`), tracking who currently holds the lock and any waiters.
    ///     This is the actual contention/mutual-exclusion point, shared across every open file
    ///     description of that file, matching kernel semantics for independent `open()`s.
    ///   - A holder id stored in this open file description's `DescriptorEntry`-scoped metadata
    ///     (via `set_entry_metadata`/`with_metadata_mut`, which is exactly LiteBox's existing
    ///     "shared across `dup()`'d fds of the same open, independent across separate `open()`s"
    ///     mechanism -- see `litebox::fd::Descriptors::duplicate` -- already used for e.g. file
    ///     offsets), identifying *which* open file description currently holds the lock so a
    ///     `LOCK_UN`/re-`LOCK_EX` from the same open file description is idempotent/self-consistent
    ///     rather than contending with itself.
    pub(crate) fn sys_flock(&self, fd: i32, operation: i32) -> Result<u32, Errno> {
        let Ok(desc) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };

        let nonblock = operation & LOCK_NB != 0;
        let op = operation & !LOCK_NB;
        if !matches!(op, LOCK_SH | LOCK_EX | LOCK_UN) {
            return Err(Errno::EINVAL);
        }

        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                desc,
                |fd| self.do_flock(fd, op, nonblock),
                // `flock()` on a non-regular-file fd (socket/pipe/eventfd/epoll/unix socket) is
                // rejected with `EINVAL`, matching Linux (only regular files, directories, and a
                // handful of special files support `flock()`; none of LiteBox's other fd kinds do).
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
                |_fd| Err(Errno::EINVAL),
            )
            .flatten()
    }

    fn do_flock(&self, fd: &TypedFd<FS>, op: i32, nonblock: bool) -> Result<u32, Errno> {
        let node_info = self
            .files
            .borrow()
            .fs
            .fd_file_status(fd)
            .map_err(Errno::from)?
            .node_info;
        let key = (node_info.dev, node_info.ino);

        let flock_file = {
            let mut registry = self.global.flock_registry.lock();
            registry
                .entry(key)
                .or_insert_with(|| alloc::sync::Arc::new(FlockFile::new()))
                .clone()
        };

        // The holder id identifies this open file description (not this fd number) to the
        // `FlockFile`. It is stored as entry-shared metadata so it is visible to (and shared by)
        // every fd `dup()`-derived from this one, but independent of any other `open()` of the
        // same path. `with_metadata_mut` takes the fd-table write lock for the duration of the
        // closure, so the read-if-present/insert-if-absent below is atomic with respect to other
        // `flock()` calls racing to lazily initialize the same entry's holder.
        let mut dt = self.global.litebox.descriptor_table_mut();
        let holder = match dt.with_metadata_mut(fd, |h: &mut FlockHolder<Platform>| h.clone()) {
            Ok(h) => h,
            Err(MetadataError::ClosedFd) => return Err(Errno::EBADF),
            Err(MetadataError::NoSuchMetadata) => {
                let id = self
                    .global
                    .next_flock_holder_id
                    .fetch_add(1, Ordering::Relaxed);
                let h = FlockHolderInner::new(id, flock_file.clone());
                dt.set_entry_metadata(fd, h.clone());
                h
            }
        };
        drop(dt);

        match op {
            LOCK_SH => flock_file.lock_shared(&self.wait_cx(), holder.id, nonblock),
            LOCK_EX => flock_file.lock_exclusive(&self.wait_cx(), holder.id, nonblock),
            LOCK_UN => {
                flock_file.unlock(holder.id);
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// Handle syscall `getcwd`
    pub fn sys_getcwd(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let cwd = self.fs.borrow().cwd.read().clone();
        // need to account for the null terminator
        if cwd.len() >= buf.len() {
            return Err(Errno::ERANGE);
        }

        let Ok(name) = CString::new(cwd) else {
            return Err(Errno::EINVAL);
        };
        let bytes = name.as_bytes_with_nul();
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Handle syscall `chdir`
    pub fn sys_chdir(&self, pathname: impl path::Arg) -> Result<(), Errno> {
        use litebox::fs::FileType;
        use litebox::fs::errors::{FileStatusError, PathError};
        use litebox::path::Arg as _;

        // Resolve relative paths against CWD, then normalize (handle `.` / `..`).
        let resolved = self.resolve_path(pathname)?;
        let abs_path = resolved.normalized().map_err(|_| Errno::EINVAL)?;

        // Verify the path exists and is a directory.
        match self.files.borrow().fs.file_status(abs_path.as_str()) {
            Ok(status) => {
                if status.file_type != FileType::Directory {
                    return Err(Errno::ENOTDIR);
                }
            }
            Err(FileStatusError::PathError(PathError::NoSuchFileOrDirectory)) => {
                return Err(Errno::ENOENT);
            }
            Err(FileStatusError::PathError(_)) => {
                return Err(Errno::EACCES);
            }
            Err(_) => {
                return Err(Errno::ENOENT);
            }
        }

        // Ensure the CWD ends with '/'.
        let mut new_cwd = abs_path;
        if !new_cwd.ends_with('/') {
            new_cwd.push('/');
        }

        *self.fs.borrow().cwd.write() = new_cwd;
        Ok(())
    }

    /// Handle syscall `fchdir`
    pub fn sys_fchdir(&self, fd: u32) -> Result<(), Errno> {
        use litebox::fs::FileType;
        use litebox::fs::errors::{FileStatusError, PathError};
        use litebox::path::Arg as _;

        // `resolve_dirfd_path` already records the absolute path an fd was opened at (as used by
        // `openat`/`*at`-family syscalls' dirfd resolution), which is exactly what `fchdir` needs
        // to translate an already-open directory fd into the CWD path `chdir` itself works with.
        let dir_path = self.resolve_dirfd_path(fd)?;
        let abs_path = dir_path
            .to_str()
            .map_err(|_| Errno::EINVAL)?
            .normalized()
            .map_err(|_| Errno::EINVAL)?;

        match self.files.borrow().fs.file_status(abs_path.as_str()) {
            Ok(status) => {
                if status.file_type != FileType::Directory {
                    return Err(Errno::ENOTDIR);
                }
            }
            Err(FileStatusError::PathError(PathError::NoSuchFileOrDirectory)) => {
                return Err(Errno::ENOENT);
            }
            Err(FileStatusError::PathError(_)) => {
                return Err(Errno::EACCES);
            }
            Err(_) => {
                return Err(Errno::ENOENT);
            }
        }

        let mut new_cwd = abs_path;
        if !new_cwd.ends_with('/') {
            new_cwd.push('/');
        }

        *self.fs.borrow().cwd.write() = new_cwd;
        Ok(())
    }
}

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `pipe2`
    pub fn sys_pipe2(&self, flags: OFlags) -> Result<(u32, u32), Errno> {
        let pipe = self.global.create_linux_pipe(flags)?;

        let files = self.files.borrow();
        let wr_raw_fd = files.insert_raw_fd(pipe.writer).map_err(|writer| {
            self.global.close_linux_pipe(&writer).unwrap();
            Errno::EMFILE
        })?;
        let rd_raw_fd = files.insert_raw_fd(pipe.reader).map_err(|reader| {
            let writer = files
                .raw_descriptor_store
                .write()
                .fd_consume_raw_integer(wr_raw_fd)
                .unwrap();
            self.global.close_linux_pipe(&writer).unwrap();
            self.global.close_linux_pipe(&reader).unwrap();
            Errno::EMFILE
        })?;
        Ok((rd_raw_fd.try_into().unwrap(), wr_raw_fd.try_into().unwrap()))
    }

    pub fn sys_eventfd2(&self, initval: u32, flags: EfdFlags) -> Result<u32, Errno> {
        if flags
            .intersects((EfdFlags::SEMAPHORE | EfdFlags::CLOEXEC | EfdFlags::NONBLOCK).complement())
        {
            return Err(Errno::EINVAL);
        }

        let eventfd = super::eventfd::EventFile::new(u64::from(initval), flags);
        let mut dt = self.global.litebox.descriptor_table_mut();
        let typed = dt.insert::<super::eventfd::EventfdSubsystem<Platform>>(eventfd);
        if flags.contains(EfdFlags::CLOEXEC) {
            let old = dt.set_fd_metadata(&typed, FileDescriptorFlags::FD_CLOEXEC);
            assert!(old.is_none());
        }
        drop(dt);
        let files = self.files.borrow();
        let raw_fd = files.insert_raw_fd(typed).map_err(|typed| {
            self.global
                .litebox
                .descriptor_table_mut()
                .remove(&typed)
                .unwrap();
            Errno::EMFILE
        })?;
        Ok(raw_fd.try_into().unwrap())
    }

    /// Handle a `TCGETS`/`TCSETS`/`TCSETSW`/`TCSETSF`/`TIOCGWINSZ` ioctl on a stdio fd.
    ///
    /// `litebox` has no real POSIX termios/tty layer underneath on every platform, so this
    /// tracks the guest-requested `termios` state per fd (mirroring `flock`'s
    /// `FlockHolder`/`with_metadata_mut` idiom) and accepts every `TCSETS*` variant as success --
    /// `TCGETS` then reflects back whatever was last set, so a `tcgetattr`/`tcsetattr`/`tcgetattr`
    /// round-trip (exactly what libuv's `uv__tty_make_raw` performs to save/restore terminal
    /// state around raw mode) observes consistent, self-coherent state instead of always reading
    /// back zeroed flags regardless of what was set.
    fn stdio_ioctl(&self, fd: &TypedFd<FS>, arg: &IoctlArg) -> Result<u32, Errno> {
        match arg {
            IoctlArg::TCGETS(termios_ptr) => {
                let dt = self.global.litebox.descriptor_table();
                let termios = dt
                    .with_metadata(fd, |t: &TermiosState| t.0.clone())
                    .unwrap_or_else(|_| TermiosState::default().0);
                termios_ptr
                    .write_at_offset::<Platform>(0, termios)
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TCSETS(termios_ptr)
            | IoctlArg::TCSETSW(termios_ptr)
            | IoctlArg::TCSETSF(termios_ptr) => {
                let termios = termios_ptr
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?;
                let mut dt = self.global.litebox.descriptor_table_mut();
                dt.set_entry_metadata(fd, TermiosState(termios));
                Ok(0)
            }
            IoctlArg::TIOCGWINSZ(ws) => {
                // Query the real terminal size where the platform can provide it (e.g. via
                // `GetConsoleScreenBufferInfo` on Windows); fall back to the traditional 80x24
                // default otherwise. A guest's own line editor (e.g. `ash`'s `lineedit.c`) uses
                // this to decide the column width at which to wrap its *own* echoed-input
                // redisplay, so returning a fake, too-narrow size here (previously hardcoded to
                // 20x20) caused spurious wraps in the echo of typed input well before the real
                // terminal would ever need to wrap.
                let (row, col) = self.global.platform.tty_window_size().unwrap_or((24, 80));
                ws.write_at_offset::<Platform>(
                    0,
                    litebox_common_linux::Winsize {
                        row,
                        col,
                        xpixel: 0,
                        ypixel: 0,
                    },
                )
                .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            // Both are pty-specific: meaningless (and `ENOTTY` on real Linux) for a plain stdio
            // fd, unlike `pty_ioctl`'s handling of the same commands on an actual pty.
            IoctlArg::TIOCGPTN(_) | IoctlArg::TIOCSPTLCK(_) => Err(Errno::ENOTTY),
            IoctlArg::TIOCGPGRP(pgrp_ptr) => {
                let dt = self.global.litebox.descriptor_table();
                let pgid = dt
                    .with_metadata(fd, |p: &crate::ForegroundPgid| p.0)
                    .unwrap_or(self.pid);
                pgrp_ptr
                    .write_at_offset::<Platform>(0, pgid)
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TIOCSPGRP(pgrp_ptr) => {
                let pgid = pgrp_ptr
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?;
                if pgid <= 0 {
                    return Err(Errno::EINVAL);
                }
                let mut dt = self.global.litebox.descriptor_table_mut();
                dt.set_entry_metadata(fd, crate::ForegroundPgid(pgid));
                Ok(0)
            }
            IoctlArg::TIOCSCTTY(_) => {
                // Make this fd the calling process's controlling terminal: accept, and make the
                // caller's own process group the terminal's foreground group (real Linux's
                // default when a session leader with no controlling terminal issues this),
                // mirroring `TIOCSPGRP`'s own metadata handling directly above.
                let pgid = self.sys_getpgid(0)?;
                let mut dt = self.global.litebox.descriptor_table_mut();
                dt.set_entry_metadata(fd, crate::ForegroundPgid(pgid));
                Ok(0)
            }
            IoctlArg::TIOCSWINSZ(_) => {
                // No window-size state is tracked for plain stdio fds (unlike ptys, where
                // `pty_ioctl` stores it on the shared `PtyPair`): accept-and-ignore, matching
                // this build's "accept every TCSETS*-family ioctl" stance rather than ENOTTY.
                Ok(0)
            }
            _ => todo!(),
        }
    }

    /// Handle a `TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCSWINSZ`/`TIOCGPTN`/`TIOCSPTLCK`/
    /// `TIOCGPGRP`/`TIOCSPGRP` ioctl on a pty fd (master or slave).
    ///
    /// `TIOCGPTN`/`TIOCSPTLCK` are master-only (matching real Linux, which returns `ENOTTY` for
    /// them on the slave); every other command works on both sides, reading/writing the state
    /// shared on the pty's [`super::pty::PtyPair`] so master and slave observe the same tty
    /// state, exactly as real Linux's master/slave pair do.
    fn pty_ioctl(&self, end: &super::pty::PtyEnd<Platform>, arg: &IoctlArg) -> Result<u32, Errno> {
        let pair = end.pair();
        match arg {
            IoctlArg::TCGETS(termios_ptr) => {
                termios_ptr
                    .write_at_offset::<Platform>(0, pair.get_termios())
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TCSETS(termios_ptr)
            | IoctlArg::TCSETSW(termios_ptr)
            | IoctlArg::TCSETSF(termios_ptr) => {
                let termios = termios_ptr
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?;
                pair.set_termios(termios);
                Ok(0)
            }
            IoctlArg::TIOCGWINSZ(ws) => {
                ws.write_at_offset::<Platform>(0, pair.get_winsize())
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TIOCSWINSZ(ws) => {
                let winsize = ws.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                pair.set_winsize(winsize);
                Ok(0)
            }
            IoctlArg::TIOCGPTN(ptr) => {
                if !end.is_master() {
                    return Err(Errno::ENOTTY);
                }
                ptr.write_at_offset::<Platform>(0, pair.id)
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TIOCSPTLCK(ptr) => {
                if !end.is_master() {
                    return Err(Errno::ENOTTY);
                }
                let val = ptr.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                pair.set_locked(val != 0);
                Ok(0)
            }
            IoctlArg::TIOCGPGRP(pgrp_ptr) => {
                let pgid = match pair.get_fg_pgid() {
                    0 => self.pid,
                    pgid => pgid,
                };
                pgrp_ptr
                    .write_at_offset::<Platform>(0, pgid)
                    .ok_or(Errno::EFAULT)?;
                Ok(0)
            }
            IoctlArg::TIOCSPGRP(pgrp_ptr) => {
                let pgid = pgrp_ptr
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?;
                if pgid <= 0 {
                    return Err(Errno::EINVAL);
                }
                pair.set_fg_pgid(pgid);
                Ok(0)
            }
            IoctlArg::TIOCSCTTY(_) => {
                // Make this pty the calling process's controlling terminal. We don't track "does
                // this process already have a different controlling terminal" or "is another
                // session already using this pty" (no session model at all -- see
                // `sys_setsid`'s doc comment), so this always succeeds, matching this build's
                // "accept and remember" idiom. Setting the terminal's foreground process group to
                // the caller's own group is what real Linux does by default here, and is exactly
                // what glibc's `login_tty()` (the primitive under `forkpty()`/`node-pty`/tmux)
                // relies on: it calls `setsid()` then `ioctl(fd, TIOCSCTTY, 0)` and expects the
                // pty to already be routing job-control signals to its own new group afterward.
                let pgid = self.sys_getpgid(0)?;
                pair.set_fg_pgid(pgid);
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn is_stdio(&self, fs: &FS, fd: &TypedFd<FS>) -> Result<bool, Errno> {
        match fs.fd_file_status(fd) {
            Ok(status) => {
                // See https://www.kernel.org/doc/Documentation/admin-guide/devices.txt
                let major = status.node_info.rdev.map_or(0, |v| v.get() >> 8);
                Ok((136..=143).contains(&major)
                    && status.file_type == litebox::fs::FileType::CharacterDevice)
            }
            Err(litebox::fs::errors::FileStatusError::ClosedFd) => Err(Errno::EBADF),
            Err(_) => unimplemented!(),
        }
    }

    /// Handle syscall `ioctl`
    pub fn sys_ioctl(&self, fd: i32, arg: IoctlArg) -> Result<u32, Errno> {
        let Ok(desc) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };

        let files = self.files.borrow();
        match arg {
            IoctlArg::FIONBIO(arg) => {
                let val = arg.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
                self.files
                    .borrow()
                    .run_on_raw_fd(
                        desc,
                        |file_fd| {
                            // Mirrors `FcntlArg::SETFL`'s raw-fd branch: most fds dispatched here
                            // are plain regular files carrying no `StdioStatusFlags` metadata at
                            // all (real Linux accepts `ioctl(FIONBIO)` on a regular file as a
                            // no-op too), so a missing-metadata result is success, not an error.
                            // For stdio fds this is what makes `do_read`'s non-blocking-stdin
                            // check observe the flag `FIONBIO` set, not just `fcntl(F_SETFL)`.
                            match self
                                .global
                                .litebox
                                .descriptor_table_mut()
                                .with_metadata_mut(file_fd, |crate::StdioStatusFlags(flags)| {
                                    flags.set(OFlags::NONBLOCK, val != 0);
                                }) {
                                Ok(()) | Err(MetadataError::NoSuchMetadata) => Ok(()),
                                Err(MetadataError::ClosedFd) => Err(Errno::EBADF),
                            }
                        },
                        |socket_fd| {
                            if let Err(e) = self
                                .global
                                .litebox
                                .descriptor_table_mut()
                                .with_metadata_mut(
                                    socket_fd,
                                    |crate::syscalls::net::SocketOFlags(flags)| {
                                        flags.set(OFlags::NONBLOCK, val != 0);
                                    },
                                )
                            {
                                match e {
                                    MetadataError::ClosedFd => return Err(Errno::EBADF),
                                    MetadataError::NoSuchMetadata => unreachable!(),
                                }
                            }
                            Ok(())
                        },
                        |fd| {
                            self.global
                                .pipes
                                .update_flags(fd, litebox::pipes::Flags::NON_BLOCKING, val != 0)
                                .map_err(Errno::from)
                        },
                        |fd| {
                            let handle = self
                                .global
                                .litebox
                                .descriptor_table()
                                .entry_handle(fd)
                                .ok_or(Errno::EBADF)?;
                            handle.with_entry(|file| {
                                file.set_status(OFlags::NONBLOCK, val != 0);
                            });
                            Ok(())
                        },
                        |fd| {
                            let handle = self
                                .global
                                .litebox
                                .descriptor_table()
                                .entry_handle(fd)
                                .ok_or(Errno::EBADF)?;
                            handle.with_entry(|file| {
                                file.set_status(OFlags::NONBLOCK, val != 0);
                            });
                            Ok(())
                        },
                        |fd| {
                            let handle = self
                                .global
                                .litebox
                                .descriptor_table()
                                .entry_handle(fd)
                                .ok_or(Errno::EBADF)?;
                            handle.with_entry(|file| {
                                file.set_status(OFlags::NONBLOCK, val != 0);
                            });
                            Ok(())
                        },
                        |fd| {
                            let handle = self
                                .global
                                .litebox
                                .descriptor_table()
                                .entry_handle(fd)
                                .ok_or(Errno::EBADF)?;
                            handle.with_entry(|end| {
                                end.set_status(OFlags::NONBLOCK, val != 0);
                            });
                            Ok(())
                        },
                    )
                    .flatten()?;
                Ok(0)
            }
            IoctlArg::FIOCLEX => files.run_on_raw_fd(
                desc,
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    // FIOCLEX (set close-on-exec) is a descriptor-table-level flag, not a
                    // file-type-specific one, so it applies identically regardless of what kind
                    // of fd this is -- unlike `net`/`pipes` above, which used to panic
                    // (`todo!()`) here despite `set_fd_metadata` working the same way for them
                    // as for every other fd type in this match.
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
                |fd| {
                    let _old = self
                        .global
                        .litebox
                        .descriptor_table_mut()
                        .set_fd_metadata(fd, FileDescriptorFlags::FD_CLOEXEC);
                    Ok(0)
                },
            )?,
            IoctlArg::TCGETS(..)
            | IoctlArg::TCSETS(..)
            | IoctlArg::TCSETSW(..)
            | IoctlArg::TCSETSF(..)
            | IoctlArg::TIOCGWINSZ(..)
            | IoctlArg::TIOCSWINSZ(..)
            | IoctlArg::TIOCGPTN(..)
            | IoctlArg::TIOCSPTLCK(..)
            | IoctlArg::TIOCSCTTY(..)
            | IoctlArg::TIOCGPGRP(..)
            | IoctlArg::TIOCSPGRP(..) => files.run_on_raw_fd(
                desc,
                |fd| {
                    if self.is_stdio(&files.fs, fd)? {
                        let stream = self
                            .global
                            .litebox
                            .descriptor_table()
                            .with_metadata(fd, |stream: &StdioStream| *stream)
                            .map_err(|_| {
                                // A character device in the tty major-number range without
                                // `StdioStream` metadata: shouldn't happen now that
                                // `insert_raw_file_fd_with_path` tags a freshly (re)opened
                                // `/dev/stdin`/`/dev/stdout`/`/dev/stderr`, but keep this as a
                                // defensive fallback for any other path into a stdio-shaped fd.
                                litebox_util_log::error!(
                                    "standard stream is missing StdioStream metadata"
                                );
                                Errno::ENOTTY
                            })?;
                        if self.global.platform.is_a_tty(stream) {
                            self.stdio_ioctl(fd, &arg)
                        } else {
                            Err(Errno::ENOTTY)
                        }
                    } else {
                        Err(Errno::ENOTTY)
                    }
                },
                |_fd| Err(Errno::ENOTTY),
                |_fd| Err(Errno::ENOTTY),
                |_fd| Err(Errno::ENOTTY),
                |_fd| Err(Errno::ENOTTY),
                |_fd| Err(Errno::ENOTTY),
                |fd| {
                    let handle = self
                        .global
                        .litebox
                        .descriptor_table()
                        .entry_handle(fd)
                        .ok_or(Errno::EBADF)?;
                    handle.with_entry(|end| self.pty_ioctl(end, &arg))
                },
            )?,
            _ => {
                log_unsupported!("ioctl with arg {:?}", arg);
                Err(Errno::EINVAL)
            }
        }
    }

    /// Handle syscall `epoll_create` and `epoll_create1`
    pub fn sys_epoll_create(&self, flags: EpollCreateFlags) -> Result<u32, Errno> {
        if flags.intersects(EpollCreateFlags::EPOLL_CLOEXEC.complement()) {
            return Err(Errno::EINVAL);
        }

        let epoll_file = super::epoll::EpollFile::new();
        let mut dt = self.global.litebox.descriptor_table_mut();
        let typed = dt.insert::<super::epoll::EpollSubsystem<Platform, FS>>(epoll_file);
        if flags.contains(EpollCreateFlags::EPOLL_CLOEXEC) {
            let old = dt.set_fd_metadata(&typed, FileDescriptorFlags::FD_CLOEXEC);
            assert!(old.is_none());
        }
        drop(dt);
        let files = self.files.borrow();
        let raw_fd = files.insert_raw_fd(typed).map_err(|typed| {
            self.global
                .litebox
                .descriptor_table_mut()
                .remove(&typed)
                .unwrap();
            Errno::EMFILE
        })?;
        Ok(raw_fd.try_into().unwrap())
    }

    /// Handle syscall `epoll_ctl`
    pub(crate) fn sys_epoll_ctl(
        &self,
        epfd: i32,
        op: litebox_common_linux::EpollOp,
        fd: i32,
        event: UserPtr<litebox_common_linux::EpollEvent>,
    ) -> Result<(), Errno> {
        let Ok(epfd) = u32::try_from(epfd) else {
            return Err(Errno::EBADF);
        };
        let Ok(fd) = u32::try_from(fd) else {
            return Err(Errno::EBADF);
        };
        if epfd == fd {
            return Err(Errno::EINVAL);
        }

        let files = self.files.borrow();

        let epoll_fd = files
            .raw_descriptor_store
            .read()
            .fd_from_raw_integer::<super::epoll::EpollSubsystem<Platform, FS>>(epfd as usize)
            .map_err(|_| Errno::EBADF)?;
        let file_descriptor = super::epoll::EpollDescriptor::try_from(&files, fd as usize)?;

        let event = if op == litebox_common_linux::EpollOp::EpollCtlDel {
            None
        } else {
            Some(event.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?)
        };
        let handle = self
            .global
            .litebox
            .descriptor_table()
            .entry_handle(&epoll_fd)
            .ok_or(Errno::EBADF)?;
        handle.with_entry(|entry| entry.epoll_ctl(&self.global, op, fd, &file_descriptor, event))
    }

    /// Handle syscall `epoll_pwait`
    pub fn sys_epoll_pwait(
        &self,
        epfd: i32,
        events: UserPtrMut<litebox_common_linux::EpollEvent>,
        maxevents: u32,
        timeout: i32,
        sigmask: Option<UserPtr<litebox_common_linux::signal::SigSet>>,
        sigsetsize: usize,
    ) -> Result<usize, Errno> {
        let sigmask = if let Some(sigmask) = sigmask {
            if sigsetsize != core::mem::size_of::<litebox_common_linux::signal::SigSet>() {
                return Err(Errno::EINVAL);
            }
            Some(sigmask.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?)
        } else {
            None
        };
        let Ok(epfd) = u32::try_from(epfd) else {
            return Err(Errno::EBADF);
        };
        let maxevents = maxevents as usize;
        if maxevents == 0
            || maxevents > i32::MAX as usize / size_of::<litebox_common_linux::EpollEvent>()
        {
            return Err(Errno::EINVAL);
        }
        let timeout = if timeout >= 0 {
            #[allow(clippy::cast_sign_loss, reason = "timeout is a positive integer")]
            Some(core::time::Duration::from_millis(timeout as u64))
        } else {
            None
        };
        let handle = {
            let files = self.files.borrow();
            {
                let raw_fd = usize::try_from(epfd).or(Err(Errno::EBADF))?;
                let Ok(fd) = files
                    .raw_descriptor_store
                    .read()
                    .fd_from_raw_integer::<crate::syscalls::epoll::EpollSubsystem<Platform, FS>>(
                    raw_fd,
                ) else {
                    return Err(Errno::EBADF);
                };
                self.global
                    .litebox
                    .descriptor_table()
                    .entry_handle(&fd)
                    .ok_or(Errno::EBADF)?
            }
        };
        let do_wait = || {
            handle.with_entry(|epoll_file| {
                match epoll_file.wait(
                    &self.global,
                    &self.wait_cx().with_timeout(timeout),
                    maxevents,
                ) {
                    Ok(epoll_events) => {
                        if !epoll_events.is_empty() {
                            events
                                .copy_from_slice::<Platform>(0, &epoll_events)
                                .ok_or(Errno::EFAULT)?;
                        }
                        Ok(epoll_events.len())
                    }
                    Err(WaitError::TimedOut) => Ok(0),
                    Err(WaitError::Interrupted) => Err(Errno::EINTR),
                }
            })
        };
        if let Some(sigmask) = sigmask {
            self.with_temporary_signal_mask(sigmask, do_wait)
        } else {
            do_wait()
        }
    }

    /// Handle syscall `ppoll`.
    pub fn sys_ppoll(
        &self,
        fds: UserPtrMut<litebox_common_linux::Pollfd>,
        nfds: usize,
        timeout: TimeParam,
        sigmask: Option<UserPtr<litebox_common_linux::signal::SigSet>>,
        sigsetsize: usize,
    ) -> Result<usize, Errno> {
        let sigmask = if let Some(sigmask) = sigmask {
            if sigsetsize != core::mem::size_of::<litebox_common_linux::signal::SigSet>() {
                // Expected via ppoll(2) manpage
                return Err(Errno::EINVAL);
            }
            Some(sigmask.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?)
        } else {
            None
        };
        let timeout = timeout.read::<Platform>()?;
        let nfds_signed = isize::try_from(nfds).map_err(|_| Errno::EINVAL)?;

        let mut set = super::epoll::PollSet::with_capacity(nfds);
        for i in 0..nfds_signed {
            let fd = fds.read_at_offset::<Platform>(i).ok_or(Errno::EFAULT)?;

            let events = litebox::event::Events::from_bits_truncate(
                fd.events.reinterpret_as_unsigned().into(),
            );
            set.add_fd(fd.fd, events);
        }

        let mut do_wait = || {
            set.wait(
                &self.global,
                &self.wait_cx().with_timeout(timeout),
                &self.files.borrow(),
            )
        };
        let wait_result = if let Some(sigmask) = sigmask {
            self.with_temporary_signal_mask(sigmask, do_wait)
        } else {
            do_wait()
        };
        match wait_result {
            Ok(()) => {}
            Err(WaitError::Interrupted) => {
                // TODO: update the remaining time.
                return Err(Errno::EINTR);
            }
            Err(WaitError::TimedOut) => {
                // A timeout occurred. Scan one last time.
                set.scan(&self.global, &self.files.borrow());
            }
        }

        // Write just the revents back.
        let fds_base_addr = fds.as_usize();
        let mut ready_count = 0;
        for (i, revents) in set.revents().enumerate() {
            // TODO: This is not great from a provenance perspective. Consider
            // adding cast+add methods to UserPtr/UserPtrMut.
            let fd_addr = fds_base_addr + i * core::mem::size_of::<litebox_common_linux::Pollfd>();
            let revents_ptr = UserPtrMut::<i16>::from_usize(
                fd_addr + core::mem::offset_of!(litebox_common_linux::Pollfd, revents),
            );
            let revents: u16 = revents.bits().trunc();
            revents_ptr
                .write_at_offset::<Platform>(0, revents.reinterpret_as_signed())
                .ok_or(Errno::EFAULT)?;
            if revents != 0 {
                ready_count += 1;
            }
        }
        Ok(ready_count)
    }

    pub(crate) fn do_pselect(
        &self,
        nfds: u32,
        readfds: Option<&mut bitvec::vec::BitVec>,
        writefds: Option<&mut bitvec::vec::BitVec>,
        exceptfds: Option<&mut bitvec::vec::BitVec>,
        timeout: Option<core::time::Duration>,
    ) -> Result<usize, Errno> {
        // XXX: semantic issue likely should be fixed here to make sure EBADF is triggered early
        // enough if needed. Previously, `file_table_len` used to be
        // `self.files.borrow().file_descriptors.read().len()` before `file_descriptors` was
        // removed to clean up the table handling.
        let file_table_len = usize::MAX;
        let mut set = super::epoll::PollSet::with_capacity(nfds as usize);
        for i in 0..nfds {
            let mut events = litebox::event::Events::empty();
            if readfds.as_ref().is_some_and(|set| set[i as usize]) {
                events |= litebox::event::Events::IN;
            }
            if writefds.as_ref().is_some_and(|set| set[i as usize]) {
                events |= litebox::event::Events::OUT;
            }
            if exceptfds.as_ref().is_some_and(|set| set[i as usize]) {
                events |= litebox::event::Events::PRI;
            }
            if !events.is_empty() {
                if i as usize >= file_table_len {
                    return Err(Errno::EBADF);
                }
                set.add_fd(i.reinterpret_as_signed(), events);
            }
        }

        match set.wait(
            &self.global,
            &self.wait_cx().with_timeout(timeout),
            &self.files.borrow(),
        ) {
            Ok(()) => {}
            Err(WaitError::Interrupted) => {
                // TODO: update the remaining time.
                return Err(Errno::EINTR);
            }
            Err(WaitError::TimedOut) => {
                // A timeout occurred. Scan one last time.
                set.scan(&self.global, &self.files.borrow());
            }
        }

        let mut ready_count = 0;
        let mut process_fdset =
            |fds: Option<&mut bitvec::vec::BitVec>, target_events: Events| -> Result<(), Errno> {
                if let Some(fds) = fds {
                    fds.fill(false);
                    for (i, revents) in set.revents_with_fds() {
                        if revents.contains(Events::NVAL) {
                            return Err(Errno::EBADF);
                        }
                        if revents.intersects(target_events) {
                            // no negative fds added to the set
                            fds.set(i.reinterpret_as_unsigned() as usize, true);
                            ready_count += 1;
                        }
                    }
                }
                Ok(())
            };
        process_fdset(readfds, Events::IN | Events::ALWAYS_POLLED)?;
        process_fdset(writefds, Events::OUT | Events::ALWAYS_POLLED)?;
        process_fdset(exceptfds, Events::PRI)?;
        Ok(ready_count)
    }

    /// Handle syscall `pselect`.
    pub(crate) fn sys_pselect(
        &self,
        nfds: u32,
        readfds: Option<UserPtrMut<usize>>,
        writefds: Option<UserPtrMut<usize>>,
        exceptfds: Option<UserPtrMut<usize>>,
        timeout: TimeParam,
        sigsetpack: Option<UserPtr<litebox_common_linux::SigSetPack>>,
    ) -> Result<usize, Errno> {
        let sigmask = if let Some(sigsetpack) = sigsetpack {
            let sigsetpack = sigsetpack
                .read_at_offset::<Platform>(0)
                .ok_or(Errno::EFAULT)?;
            if sigsetpack.size != core::mem::size_of::<litebox_common_linux::signal::SigSet>() {
                return Err(Errno::EINVAL);
            }
            Some(
                sigsetpack
                    .sigset
                    .read_at_offset::<Platform>(0)
                    .ok_or(Errno::EFAULT)?,
            )
        } else {
            None
        };
        let timeout = timeout.read::<Platform>()?;
        if nfds >= i32::MAX as u32
            || nfds as usize
                > self
                    .process()
                    .limits
                    .get_rlimit_cur(litebox_common_linux::RlimitResource::NOFILE)
        {
            return Err(Errno::EINVAL);
        }
        let len = (nfds as usize).div_ceil(core::mem::size_of::<usize>() * 8);
        let mut kreadfds = readfds
            .map(|fds| fds.to_owned_slice::<Platform>(len).ok_or(Errno::EFAULT))
            .transpose()?
            .map(|fds| bitvec::vec::BitVec::from_vec(fds.into_vec()));
        let mut kwritefds = writefds
            .map(|fds| fds.to_owned_slice::<Platform>(len).ok_or(Errno::EFAULT))
            .transpose()?
            .map(|fds| bitvec::vec::BitVec::from_vec(fds.into_vec()));
        let mut kexceptfds = exceptfds
            .map(|fds| fds.to_owned_slice::<Platform>(len).ok_or(Errno::EFAULT))
            .transpose()?
            .map(|fds| bitvec::vec::BitVec::from_vec(fds.into_vec()));

        let mut do_pselect = || {
            self.do_pselect(
                nfds,
                kreadfds.as_mut(),
                kwritefds.as_mut(),
                kexceptfds.as_mut(),
                timeout,
            )
        };
        let count = if let Some(sigmask) = sigmask {
            self.with_temporary_signal_mask(sigmask, do_pselect)
        } else {
            do_pselect()
        }?;

        if let Some(fds) = kreadfds {
            readfds
                .unwrap()
                .write_slice_at_offset::<Platform>(0, fds.as_raw_slice())
                .ok_or(Errno::EFAULT)?;
        }
        if let Some(fds) = kwritefds {
            writefds
                .unwrap()
                .write_slice_at_offset::<Platform>(0, fds.as_raw_slice())
                .ok_or(Errno::EFAULT)?;
        }
        if let Some(fds) = kexceptfds {
            exceptfds
                .unwrap()
                .write_slice_at_offset::<Platform>(0, fds.as_raw_slice())
                .ok_or(Errno::EFAULT)?;
        }

        Ok(count)
    }

    fn do_dup(&self, file: usize, flags: OFlags) -> Result<usize, DupFdError> {
        self.do_dup_inner(file, flags, DupFdRequest::LowestAvailable)
    }

    fn do_dup_inner(
        &self,
        file: usize,
        flags: OFlags,
        target: DupFdRequest,
    ) -> Result<usize, DupFdError> {
        fn dup<Platform: ShimPlatform, FS: ShimFS, S: FdEnabledSubsystem>(
            task: &Task<Platform, FS>,
            files: &FilesState<Platform, FS>,
            fd: &TypedFd<S>,
            close_on_exec: bool,
            target: DupFdRequest,
        ) -> Result<usize, DupFdError> {
            let max_fd = task
                .process()
                .limits
                .get_rlimit_cur(litebox_common_linux::RlimitResource::NOFILE);
            match target {
                DupFdRequest::Exact(target) if target >= max_fd => {
                    return Err(DupFdError::TargetFdExceedsLimit);
                }
                DupFdRequest::LowestAtOrAbove(min_fd) if min_fd >= max_fd => {
                    return Err(DupFdError::TargetFdExceedsLimit);
                }
                _ => {}
            }

            let mut dt = task.global.litebox.descriptor_table_mut();
            let fd: TypedFd<_> = dt.duplicate(fd).ok_or(DupFdError::BadFd)?;
            if close_on_exec {
                let old = dt.set_fd_metadata(&fd, FileDescriptorFlags::FD_CLOEXEC);
                assert!(old.is_none());
            }
            drop(dt);

            let new_fd = match target {
                DupFdRequest::Exact(target) => {
                    let _ = task.do_close_and_replace(target, Some(fd));
                    target
                }
                DupFdRequest::LowestAvailable => {
                    let rds = &mut *files.raw_descriptor_store.write();
                    rds.fd_into_raw_integer(fd)
                }
                DupFdRequest::LowestAtOrAbove(min_fd) => {
                    let rds = &mut *files.raw_descriptor_store.write();
                    let mut raw_fd = min_fd;
                    for occupied_raw_fd in rds.iter_alive().skip_while(|&fd| fd < min_fd) {
                        if occupied_raw_fd != raw_fd {
                            break;
                        }
                        raw_fd += 1;
                    }
                    let success = rds.fd_into_specific_raw_integer(fd, raw_fd);
                    assert!(success);
                    raw_fd
                }
            };
            if new_fd >= max_fd {
                let _ = task.do_close(new_fd);
                return Err(DupFdError::TooManyFiles);
            }
            Ok(new_fd)
        }

        let close_on_exec = flags.contains(OFlags::CLOEXEC);
        let files = self.files.borrow();
        files
            .run_on_raw_fd(
                file,
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
                |fd| dup(self, &files, fd, close_on_exec, target),
            )
            .map_err(|_| DupFdError::BadFd)?
    }

    /// Handle syscall `dup/dup2/dup3`
    ///
    /// The dup() system call creates a copy of the file descriptor oldfd, using the lowest-numbered unused file descriptor for the new descriptor.
    /// The dup2() system call performs the same task as dup(), but instead of using the lowest-numbered unused file descriptor, it uses the file descriptor number specified in newfd.
    /// The dup3() system call is similar to dup2(), but it also takes an additional flags argument that can be used to set the close-on-exec flag for the new file descriptor.
    pub fn sys_dup(
        &self,
        oldfd: i32,
        newfd: Option<i32>,
        flags: Option<OFlags>,
    ) -> Result<u32, Errno> {
        self.check_raw_fd_exists(oldfd)?;
        let oldfd = u32::try_from(oldfd).map_err(|_| Errno::EBADF)?;
        let oldfd_usize = usize::try_from(oldfd).or(Err(Errno::EBADF))?;
        if let Some(newfd) = newfd {
            // dup2/dup3
            let Ok(newfd) = u32::try_from(newfd) else {
                return Err(Errno::EBADF);
            };
            if oldfd == newfd {
                // Different from dup3, if oldfd is a valid file descriptor, and newfd has the same value
                // as oldfd, then dup2() does nothing.
                return if flags.is_some() {
                    // dup3
                    Err(Errno::EINVAL)
                } else {
                    // dup2
                    Ok(oldfd)
                };
            }
            let newfd_usize = usize::try_from(newfd).or(Err(Errno::EBADF))?;
            self.do_dup_inner(
                oldfd_usize,
                flags.unwrap_or(OFlags::empty()),
                DupFdRequest::Exact(newfd_usize),
            )
        } else {
            // dup
            self.do_dup(oldfd_usize, flags.unwrap_or(OFlags::empty()))
        }
        .map_err(|e| match e {
            DupFdError::BadFd | DupFdError::TargetFdExceedsLimit => Errno::EBADF,
            DupFdError::TooManyFiles => Errno::EMFILE,
        })
        .map(|new_fd| {
            let files = self.files.borrow();
            if let Some(path) = files.lookup_fd_path(oldfd_usize) {
                files.record_fd_path(new_fd, path);
            }
            u32::try_from(new_fd).unwrap()
        })
    }
}

#[derive(Clone, Copy)]
enum DupFdRequest {
    LowestAvailable,
    LowestAtOrAbove(usize),
    /// Duplicate to the specified fd, closing it first if it's open.
    Exact(usize),
}

#[derive(Error, Debug)]
enum DupFdError {
    #[error("Bad file descriptor")]
    BadFd,
    #[error("Too many open files")]
    TooManyFiles,
    #[error("Target fd exceeds process limit")]
    TargetFdExceedsLimit,
}

#[derive(Clone, Copy, Debug, Default)]
struct Diroff(usize);

const DIRENT_STRUCT_BYTES_WITHOUT_NAME: usize =
    core::mem::offset_of!(litebox_common_linux::LinuxDirent64, __name);

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `getdents64`
    pub(crate) fn sys_getdirent64(
        &self,
        fd: i32,
        dirp: UserPtrMut<u8>,
        count: usize,
    ) -> Result<usize, Errno> {
        let Ok(fd) = u32::try_from(fd).and_then(usize::try_from) else {
            return Err(Errno::EBADF);
        };
        let files = self.files.borrow();
        files.run_on_raw_fd(
            fd,
            |file| {
                let dir_off: Diroff = self
                    .global
                    .litebox
                    .descriptor_table()
                    .with_metadata(file, |off: &Diroff| *off)
                    .unwrap_or_default();
                let mut dir_off = dir_off.0;
                let mut nbytes = 0;

                let mut entries = files.fs.read_dir(file)?;
                entries.sort_by(|a, b| a.name.cmp(&b.name));

                for entry in entries.iter().skip(dir_off) {
                    // include null terminator and make it aligned
                    let len = (DIRENT_STRUCT_BYTES_WITHOUT_NAME + entry.name.len() + 1)
                        .next_multiple_of(align_of::<litebox_common_linux::LinuxDirent64>());
                    if nbytes + len > count {
                        // not enough space
                        if nbytes == 0 {
                            // not enough space for even a single entry
                            return Err(Errno::EINVAL);
                        }
                        break;
                    }
                    let dirent64 = litebox_common_linux::LinuxDirent64 {
                        ino: entry.ino_info.as_ref().map_or(0, |node_info| node_info.ino) as u64,
                        off: dir_off as u64,
                        len: len.trunc(),
                        typ: litebox_common_linux::DirentType::from(entry.file_type.clone()) as u8,
                        __name: [0; 0],
                    };
                    let hdr_ptr = UserPtrMut::from_usize(dirp.as_usize() + nbytes);
                    hdr_ptr
                        .write_at_offset::<Platform>(0, dirent64)
                        .ok_or(Errno::EFAULT)?;
                    let name_ptr = UserPtrMut::from_usize(
                        hdr_ptr.as_usize() + DIRENT_STRUCT_BYTES_WITHOUT_NAME,
                    );
                    name_ptr
                        .write_slice_at_offset::<Platform>(0, entry.name.as_bytes())
                        .ok_or(Errno::EFAULT)?;
                    // set the null terminator and padding
                    let zeros_len = len - (DIRENT_STRUCT_BYTES_WITHOUT_NAME + entry.name.len());
                    name_ptr
                        .write_slice_at_offset::<Platform>(
                            isize::try_from(entry.name.len()).unwrap(),
                            &vec![0; zeros_len],
                        )
                        .ok_or(Errno::EFAULT)?;
                    nbytes += len;
                    dir_off += 1;
                }
                let _old = self
                    .global
                    .litebox
                    .descriptor_table_mut()
                    .set_fd_metadata(file, Diroff(dir_off));
                Ok(nbytes)
            },
            |_fd| Err(Errno::ENOTDIR),
            |_fd| Err(Errno::ENOTDIR),
            |_fd| Err(Errno::ENOTDIR),
            |_fd| Err(Errno::ENOTDIR),
            |_fd| Err(Errno::ENOTDIR),
            |_fd| Err(Errno::ENOTDIR),
        )?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use core::cell::Cell;
    use litebox::fs::Mode;

    extern crate std;

    #[test]
    fn write_to_iovec_returns_partial_after_later_error() {
        let first = b"first";
        let second = b"second";
        let iovs = [
            IoWriteVec {
                iov_base: UserPtr::from_usize(first.as_ptr().expose_provenance()),
                iov_len: first.len(),
            },
            IoWriteVec {
                iov_base: UserPtr::from_usize(second.as_ptr().expose_provenance()),
                iov_len: second.len(),
            },
        ];
        let calls = Cell::new(0);

        let result =
            write_to_iovec::<_, crate::syscalls::tests::TestPlatform>(&iovs, |buf, total| {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    assert_eq!(buf, first);
                    assert_eq!(total, 0);
                    Ok(buf.len())
                } else {
                    assert_eq!(buf, second);
                    assert_eq!(total, first.len());
                    Err(Errno::EPIPE)
                }
            });

        assert_eq!(result, Ok(first.len()));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn read_from_iovec_breaks_on_eof() {
        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        let iovs = [
            IoReadVec {
                iov_base: UserPtrMut::from_usize(first.as_mut_ptr().expose_provenance()),
                iov_len: first.len(),
            },
            IoReadVec {
                iov_base: UserPtrMut::from_usize(second.as_mut_ptr().expose_provenance()),
                iov_len: second.len(),
            },
        ];
        let mut kernel_buffer = [0u8; 8];
        let calls = Cell::new(0);

        let result = read_from_iovec::<_, crate::syscalls::tests::TestPlatform>(
            &iovs,
            &mut kernel_buffer,
            |buf, total| {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    assert_eq!(total, 0);
                    buf.fill(b'a');
                    Ok(buf.len())
                } else {
                    assert_eq!(total, 4);
                    Ok(0)
                }
            },
        );

        assert_eq!(result, Ok(4));
        assert_eq!(calls.get(), 2);
        assert_eq!(&first, b"aaaa");
        assert_eq!(&second, &[0u8; 4]);
    }

    #[test]
    fn read_from_iovec_chunks_iov_larger_than_kernel_buffer() {
        let mut dest = [0u8; 12];
        let iovs = [IoReadVec {
            iov_base: UserPtrMut::from_usize(dest.as_mut_ptr().expose_provenance()),
            iov_len: dest.len(),
        }];
        let mut kernel_buffer = [0u8; 4];
        let calls = Cell::new(0);

        let result = read_from_iovec::<_, crate::syscalls::tests::TestPlatform>(
            &iovs,
            &mut kernel_buffer,
            |buf, total| {
                assert_eq!(buf.len(), 4);
                assert_eq!(total, calls.get() * 4);
                let marker = b'a' + u8::try_from(calls.get()).unwrap();
                buf.fill(marker);
                calls.set(calls.get() + 1);
                Ok(buf.len())
            },
        );

        assert_eq!(result, Ok(12));
        assert_eq!(calls.get(), 3);
        assert_eq!(&dest, b"aaaabbbbcccc");
    }

    #[test]
    fn read_from_iovec_returns_partial_after_later_error() {
        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        let iovs = [
            IoReadVec {
                iov_base: UserPtrMut::from_usize(first.as_mut_ptr().expose_provenance()),
                iov_len: first.len(),
            },
            IoReadVec {
                iov_base: UserPtrMut::from_usize(second.as_mut_ptr().expose_provenance()),
                iov_len: second.len(),
            },
        ];
        let mut kernel_buffer = [0u8; 4];
        let calls = Cell::new(0);

        let result = read_from_iovec::<_, crate::syscalls::tests::TestPlatform>(
            &iovs,
            &mut kernel_buffer,
            |buf, total| {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    assert_eq!(total, 0);
                    buf.fill(b'x');
                    Ok(buf.len())
                } else {
                    assert_eq!(total, 4);
                    Err(Errno::EIO)
                }
            },
        );

        assert_eq!(result, Ok(4));
        assert_eq!(calls.get(), 2);
        assert_eq!(&first, b"xxxx");
    }

    #[test]
    fn fspath_new() {
        // Absolute paths should never invoke the get_cwd closure.
        let fp = FsPath::new(litebox_common_linux::AT_FDCWD, "/usr/bin", || {
            panic!("get_cwd should not be called for absolute paths")
        })
        .unwrap();
        assert!(matches!(fp, FsPath::Absolute { path } if path.to_str().unwrap() == "/usr/bin"));

        // Relative path resolves against CWD.
        let fp = FsPath::new(litebox_common_linux::AT_FDCWD, "foo/bar", || {
            String::from("/home/")
        })
        .unwrap();
        assert!(
            matches!(fp, FsPath::Absolute { path } if path.to_str().unwrap() == "/home/foo/bar")
        );

        // Empty path at AT_FDCWD → Cwd variant.
        let fp = FsPath::new(litebox_common_linux::AT_FDCWD, "", || {
            panic!("get_cwd should not be called for empty Cwd path")
        })
        .unwrap();
        assert!(matches!(fp, FsPath::Cwd));

        // Positive fd + empty path → Fd variant.
        let fp = FsPath::new(5, "", || panic!("should not be called")).unwrap();
        assert!(matches!(fp, FsPath::Fd(5)));

        // Invalid dirfd → EBADF.
        let err = FsPath::new(-1, "file.txt", || panic!("should not be called")).unwrap_err();
        assert_eq!(err, Errno::EBADF);

        // Path exceeding PATH_MAX → ENAMETOOLONG.
        let long_path = "a".repeat(PATH_MAX + 1);
        let err = FsPath::new(litebox_common_linux::AT_FDCWD, long_path.as_str(), || {
            String::from("/")
        })
        .unwrap_err();
        assert_eq!(err, Errno::ENAMETOOLONG);
    }

    #[test]
    fn getcwd_and_chdir() {
        let task = crate::syscalls::tests::init_platform(None);

        // Default CWD is root.
        let mut buf = [0u8; 256];
        let len = task.sys_getcwd(&mut buf).unwrap();
        let cwd = core::str::from_utf8(&buf[..len - 1]).unwrap(); // strip NUL
        assert_eq!(cwd, "/");

        // chdir + getcwd round trip.
        task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "/test_chdir_dir", 0o777)
            .unwrap();
        task.sys_chdir("/test_chdir_dir").unwrap();
        let len = task.sys_getcwd(&mut buf).unwrap();
        let cwd = core::str::from_utf8(&buf[..len - 1]).unwrap();
        assert_eq!(cwd, "/test_chdir_dir/");

        // chdir to nonexistent path → ENOENT.
        assert_eq!(
            task.sys_chdir("/does_not_exist").unwrap_err(),
            Errno::ENOENT
        );

        // chdir to a regular file → ENOTDIR.
        let fd = task
            .sys_open(
                "/test_chdir_file",
                litebox::fs::OFlags::CREAT | litebox::fs::OFlags::WRONLY,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap();
        let _ = task.sys_close(i32::try_from(fd).unwrap());
        assert_eq!(
            task.sys_chdir("/test_chdir_file").unwrap_err(),
            Errno::ENOTDIR
        );

        // getcwd with too-small buffer → ERANGE.
        let mut tiny = [0u8; 1];
        assert_eq!(task.sys_getcwd(&mut tiny).unwrap_err(), Errno::ERANGE);
    }

    #[test]
    fn chdir_relative_path() {
        let task = crate::syscalls::tests::init_platform(None);

        // Create nested dirs: /rel_parent/rel_child
        task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "/rel_parent", 0o777)
            .unwrap();
        task.sys_mkdirat(
            litebox_common_linux::AT_FDCWD,
            "/rel_parent/rel_child",
            0o777,
        )
        .unwrap();

        // chdir to /rel_parent first, then relative chdir into child.
        task.sys_chdir("/rel_parent").unwrap();
        task.sys_chdir("rel_child").unwrap();

        let mut buf = [0u8; 256];
        let len = task.sys_getcwd(&mut buf).unwrap();
        let cwd = core::str::from_utf8(&buf[..len - 1]).unwrap();
        assert_eq!(cwd, "/rel_parent/rel_child/");

        // chdir("..") should normalize back to /rel_parent/.
        task.sys_chdir("..").unwrap();
        let len = task.sys_getcwd(&mut buf).unwrap();
        let cwd = core::str::from_utf8(&buf[..len - 1]).unwrap();
        assert_eq!(cwd, "/rel_parent/");
    }

    #[test]
    fn mknodat_regular_file_does_not_consume_fd_limit() {
        use litebox_common_linux::{Rlimit, RlimitResource};

        let task = crate::syscalls::tests::init_platform(None);
        let old_limit = task.do_prlimit(RlimitResource::NOFILE, None).unwrap();
        task.do_prlimit(
            RlimitResource::NOFILE,
            Some(Rlimit {
                rlim_cur: 3,
                rlim_max: old_limit.rlim_max,
            }),
        )
        .unwrap();
        let path = "/mknodat_at_fd_limit";

        let result = task.sys_mknodat(
            litebox_common_linux::AT_FDCWD,
            path,
            InodeType::File as u32 | (Mode::RUSR | Mode::WUSR).bits(),
            0,
        );

        assert!(
            task.sys_stat(path).is_ok(),
            "mknodat created the file before returning {result:?}"
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn empty_pathnames_return_enoent() {
        let task = crate::syscalls::tests::init_platform(None);

        assert_eq!(
            task.sys_open("", OFlags::RDONLY, Mode::empty())
                .unwrap_err(),
            Errno::ENOENT
        );
        assert_eq!(
            task.sys_open("", OFlags::CREAT | OFlags::WRONLY, Mode::RWXU)
                .unwrap_err(),
            Errno::ENOENT
        );
        assert_eq!(task.sys_stat("").unwrap_err(), Errno::ENOENT);
        assert_eq!(
            task.sys_unlinkat(litebox_common_linux::AT_FDCWD, "", AtFlags::empty())
                .unwrap_err(),
            Errno::ENOENT
        );
        assert_eq!(
            task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "", 0o755)
                .unwrap_err(),
            Errno::ENOENT
        );
        assert_eq!(
            task.sys_mknodat(
                litebox_common_linux::AT_FDCWD,
                "",
                InodeType::File as u32 | Mode::RWXU.bits(),
                0,
            )
            .unwrap_err(),
            Errno::ENOENT
        );
        let mut buffer = [0u8; 16];
        assert_eq!(
            task.sys_readlinkat(litebox_common_linux::AT_FDCWD, "", &mut buffer)
                .unwrap_err(),
            Errno::ENOENT
        );
    }

    /// Verify every path-taking syscall resolves relative paths after `chdir`.
    #[test]
    fn all_path_syscalls_respect_chdir() {
        use litebox_common_linux::{AccessFlags, AtFlags};

        let task = crate::syscalls::tests::init_platform(None);

        // Set up: mkdir + chdir into /cwd_test/.
        task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "/cwd_test", 0o777)
            .unwrap();
        task.sys_chdir("/cwd_test").unwrap();

        // ── sys_open: create a file via relative path ──
        let fd = task
            .sys_open(
                "file.txt",
                litebox::fs::OFlags::CREAT | litebox::fs::OFlags::WRONLY,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap();
        task.sys_close(i32::try_from(fd).unwrap()).unwrap();

        // ── sys_stat: stat the relative file ──
        task.sys_stat("file.txt").unwrap();

        // ── sys_lstat: lstat the relative file ──
        task.sys_lstat("file.txt").unwrap();

        // ── sys_faccessat: check relative file is accessible ──
        task.sys_faccessat(
            litebox_common_linux::AT_FDCWD,
            "file.txt",
            AccessFlags::F_OK,
            AtFlags::empty(),
        )
        .unwrap();

        // ── create a subdirectory via relative path ──
        task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "subdir", 0o777)
            .unwrap();
        task.sys_stat("/cwd_test/subdir").unwrap(); // verify via absolute

        // ── sys_openat (AT_FDCWD + relative): open inside the new subdir ──
        let fd = task
            .sys_openat(
                litebox_common_linux::AT_FDCWD,
                "subdir/inner.txt",
                litebox::fs::OFlags::CREAT | litebox::fs::OFlags::WRONLY,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap();
        task.sys_close(i32::try_from(fd).unwrap()).unwrap();

        // ── sys_newfstatat (AT_FDCWD + relative) ──
        task.sys_newfstatat(
            litebox_common_linux::AT_FDCWD,
            "subdir/inner.txt",
            AtFlags::empty(),
        )
        .unwrap();

        // ── sys_unlinkat: remove a file via relative path ──
        task.sys_unlinkat(
            litebox_common_linux::AT_FDCWD,
            "subdir/inner.txt",
            AtFlags::empty(),
        )
        .unwrap();
        assert_eq!(
            task.sys_stat("/cwd_test/subdir/inner.txt").unwrap_err(),
            Errno::ENOENT
        );

        // ── sys_unlinkat (AT_REMOVEDIR): remove directory via relative path ──
        task.sys_unlinkat(
            litebox_common_linux::AT_FDCWD,
            "subdir",
            AtFlags::AT_REMOVEDIR,
        )
        .unwrap();
        assert_eq!(
            task.sys_stat("/cwd_test/subdir").unwrap_err(),
            Errno::ENOENT
        );
    }

    #[test]
    fn tcsetsw_and_tcsetsf_round_trip_through_tcgets() {
        // Regression test for the ENOTTY node.js `setRawMode` crash: libuv's
        // `uv__tty_make_raw` calls `tcsetattr(fd, TCSAFLUSH, ...)`, i.e. ioctl command
        // `TCSETSF`, never plain `TCSETS`. Exercise `stdio_ioctl` directly (bypassing
        // `is_a_tty`, which the mock platform always reports `false` for) to verify the
        // real per-fd `TermiosState` storage/round-trip logic added to close that gap.
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();
        let raw_fd_lookup = files.raw_descriptor_store.read().fd_from_raw_integer(0);
        assert!(
            raw_fd_lookup.is_ok(),
            "test harness invariant: fd 0 must be the stdio-initialized stdin fd"
        );
        let Ok(stdin_fd) = raw_fd_lookup else {
            return;
        };

        // Before any TCSETS*, TCGETS must succeed with the documented all-zero default
        // rather than erroring (there is no real termios layer underneath).
        let mut got = litebox_common_linux::Termios {
            c_iflag: 0xFFFF_FFFF,
            c_oflag: 0xFFFF_FFFF,
            c_cflag: 0xFFFF_FFFF,
            c_lflag: 0xFFFF_FFFF,
            c_line: 0xFF,
            c_cc: [0xFF; 19],
        };
        let got_ptr = UserPtrMut::from_usize((&raw mut got).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TCGETS(got_ptr)),
            Ok(0)
        );
        assert_eq!(got.c_lflag, 0);

        // TCSETSF (what libuv actually issues for raw mode) must be accepted, not rejected
        // as an unrecognized/`Raw` ioctl command.
        let mut raw_termios = litebox_common_linux::Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            // ICANON/ECHO cleared is what raw mode disables; use a recognizable nonzero
            // value here purely to prove round-trip fidelity through storage.
            c_lflag: 0x1234,
            c_line: 0,
            c_cc: [0; 19],
        };
        let set_ptr = UserPtr::from_usize((&raw mut raw_termios).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TCSETSF(set_ptr)),
            Ok(0)
        );

        // TCGETS afterwards must reflect exactly what TCSETSF stored.
        let mut after_flush = litebox_common_linux::Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 19],
        };
        let after_flush_ptr = UserPtrMut::from_usize((&raw mut after_flush).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TCGETS(after_flush_ptr)),
            Ok(0)
        );
        assert_eq!(after_flush.c_lflag, 0x1234);

        // TCSETSW (TCSADRAIN) must also be accepted and likewise observable via TCGETS.
        let mut drain_termios = litebox_common_linux::Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0x5678,
            c_line: 0,
            c_cc: [0; 19],
        };
        let drain_ptr = UserPtr::from_usize((&raw mut drain_termios).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TCSETSW(drain_ptr)),
            Ok(0)
        );
        let mut after_drain = litebox_common_linux::Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 19],
        };
        let after_drain_ptr = UserPtrMut::from_usize((&raw mut after_drain).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TCGETS(after_drain_ptr)),
            Ok(0)
        );
        assert_eq!(after_drain.c_lflag, 0x5678);
    }

    #[test]
    fn tiocgwinsz_falls_back_to_80x24_when_platform_has_no_real_size() {
        // Regression test for a genuine echo-wrapping bug: `TIOCGWINSZ` used to unconditionally
        // report a hardcoded 20x20 window, regardless of the real terminal size. Guests' own
        // line editors (e.g. `ash`'s `lineedit.c`) query this to decide the column width at
        // which to wrap their own echoed-input redisplay, so a fake 20-column width caused
        // spurious `\r\n` wraps to be inserted into the echo of typed input at ~20 characters,
        // well before the real terminal (which may be 80, 120, or wider) would ever need to
        // wrap. `MockPlatform::tty_window_size` returns `None` (no real terminal to query), so
        // this exercises the fallback path: it must be the traditional 80x24 default, not the
        // old hardcoded 20x20.
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();
        let Ok(stdin_fd) = files.raw_descriptor_store.read().fd_from_raw_integer(0) else {
            panic!("test harness invariant: fd 0 must be the stdio-initialized stdin fd");
        };
        drop(files);

        let mut ws = litebox_common_linux::Winsize {
            row: 0xFFFF,
            col: 0xFFFF,
            xpixel: 0xFFFF,
            ypixel: 0xFFFF,
        };
        let ws_ptr = UserPtrMut::from_usize((&raw mut ws).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCGWINSZ(ws_ptr)),
            Ok(0)
        );
        assert_eq!(ws.row, 24);
        assert_eq!(ws.col, 80);
        assert_ne!(
            (ws.row, ws.col),
            (20, 20),
            "must not regress to the old hardcoded 20x20 fake window size"
        );
    }

    #[test]
    fn tiocgpgrp_defaults_to_process_pid() {
        // Before any TIOCSPGRP, TIOCGPGRP must reflect the calling process's own pgid (which
        // defaults to its own pid), matching real Linux's default for a freshly opened
        // controlling terminal.
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();
        let Ok(stdin_fd) = files.raw_descriptor_store.read().fd_from_raw_integer(0) else {
            panic!("test harness invariant: fd 0 must be the stdio-initialized stdin fd");
        };
        drop(files);

        let mut pgrp: i32 = -1;
        let pgrp_ptr = UserPtrMut::from_usize((&raw mut pgrp).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCGPGRP(pgrp_ptr)),
            Ok(0)
        );
        assert_eq!(pgrp, task.sys_getpid());
    }

    #[test]
    fn tiocspgrp_then_tiocgpgrp_round_trip() {
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();
        let Ok(stdin_fd) = files.raw_descriptor_store.read().fd_from_raw_integer(0) else {
            panic!("test harness invariant: fd 0 must be the stdio-initialized stdin fd");
        };
        drop(files);

        let set_val: i32 = 4242;
        let set_ptr = UserPtr::from_usize((&raw const set_val).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCSPGRP(set_ptr)),
            Ok(0)
        );

        let mut got: i32 = -1;
        let got_ptr = UserPtrMut::from_usize((&raw mut got).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCGPGRP(got_ptr)),
            Ok(0)
        );
        assert_eq!(got, 4242);
    }

    #[test]
    fn tiocspgrp_rejects_nonpositive_pgid() {
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();
        let Ok(stdin_fd) = files.raw_descriptor_store.read().fd_from_raw_integer(0) else {
            panic!("test harness invariant: fd 0 must be the stdio-initialized stdin fd");
        };
        drop(files);

        let bad_val: i32 = 0;
        let bad_ptr = UserPtr::from_usize((&raw const bad_val).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCSPGRP(bad_ptr)),
            Err(Errno::EINVAL)
        );

        let negative_val: i32 = -1;
        let negative_ptr = UserPtr::from_usize((&raw const negative_val).expose_provenance());
        assert_eq!(
            task.stdio_ioctl(&stdin_fd, &IoctlArg::TIOCSPGRP(negative_ptr)),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn reopened_dev_stdin_gets_stdio_stream_metadata() {
        // Regression test for the real ENOTTY node.js `setRawMode` crash reproduced live via a
        // genuinely console-attached process: node's libuv (`uv__tty_make_raw`) doesn't issue
        // TCGETS/TCSETSF on fd 0 directly -- it reopens `/dev/stdin` to get a private fd first.
        // That fresh fd passes `is_stdio`'s character-device/rdev-major check (same
        // STDIO_NODE_INFO as the original), but before this fix it had no `StdioStream` metadata
        // attached (that was only ever set once at process bootstrap for fds 0/1/2 in
        // `initialize_stdio_in_shared_descriptors_table`), so `sys_ioctl`'s TCGETS/TCSETS*
        // handling unconditionally bailed with ENOTTY before even reaching `is_a_tty` --
        // independent of, and not fixed by, the earlier TCSETSW/TCSETSF-command-mapping fix
        // alone. `sys_ioctl`'s end-to-end behavior additionally depends on the platform's
        // `is_a_tty` (which the mock platform always reports `false` for, orthogonal to this fix
        // -- see `tcsetsw_and_tcsetsf_round_trip_through_tcgets` above), so this test verifies the
        // metadata-attachment half directly rather than through the full ioctl dispatch.
        let task = crate::syscalls::tests::init_platform(None);

        let raw_fd = task
            .sys_open("/dev/stdin", OFlags::RDONLY, Mode::empty())
            .expect("reopening /dev/stdin must succeed");

        let files = task.files.borrow();
        let fd = files
            .raw_descriptor_store
            .read()
            .fd_from_raw_integer::<crate::DefaultFS<crate::syscalls::tests::TestPlatform>>(
                usize::try_from(raw_fd).unwrap(),
            )
            .expect("freshly opened /dev/stdin must resolve to a filesystem-backed fd");
        let stream = task
            .global
            .litebox
            .descriptor_table()
            .with_metadata(&fd, |stream: &StdioStream| *stream)
            .expect("reopened /dev/stdin must carry StdioStream::Stdin metadata");
        assert_eq!(stream, StdioStream::Stdin);
    }

    #[test]
    fn reopened_dev_stdin_with_o_nonblock_gets_stdio_status_flags_metadata() {
        // Regression test for the `open("/dev/stdin", O_NONBLOCK)` panic (fixed in
        // `litebox::fs::devices`'s `open_file_at`, which used to `unimplemented!()`
        // unconditionally for `O_NONBLOCK` on any of the devices it serves) and its follow-on
        // gap: even with that panic fixed, a freshly reopened `/dev/stdin` fd carried no
        // `StdioStatusFlags` metadata at all -- only `StdioStream`, attached above in
        // `reopened_dev_stdin_gets_stdio_stream_metadata` -- so `do_read`'s non-blocking-stdin
        // `EAGAIN` check (which consults `StdioStatusFlags`) could never see `O_NONBLOCK` for
        // it, regardless of the flags it was actually opened with. Confirms
        // `insert_raw_file_fd_with_path` now also tags a reopened `/dev/stdin` with
        // `StdioStatusFlags` reflecting its real open flags.
        let task = crate::syscalls::tests::init_platform(None);

        let raw_fd = task
            .sys_open(
                "/dev/stdin",
                OFlags::RDONLY | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .expect("reopening /dev/stdin with O_NONBLOCK must succeed, not panic");

        let files = task.files.borrow();
        let fd = files
            .raw_descriptor_store
            .read()
            .fd_from_raw_integer::<crate::DefaultFS<crate::syscalls::tests::TestPlatform>>(
                usize::try_from(raw_fd).unwrap(),
            )
            .expect("freshly opened /dev/stdin must resolve to a filesystem-backed fd");
        let flags = task
            .global
            .litebox
            .descriptor_table()
            .with_metadata(&fd, |crate::StdioStatusFlags(flags)| *flags)
            .expect("reopened /dev/stdin must carry StdioStatusFlags metadata");
        assert!(flags.contains(OFlags::NONBLOCK));
    }

    #[test]
    fn readlink_proc_self_fd_for_arbitrary_open_fd_does_not_panic() {
        // Regression test: `readlink("/proc/self/fd/<N>")` used to unconditionally panic
        // (`unimplemented!()`) for any fd other than 0/1/2 -- something as ordinary as Python's
        // `os.readlink(f"/proc/self/fd/{fd}")` (used by introspection/sandboxing libraries) or a
        // shell's `<()` process substitution would crash the whole runner.
        let task = crate::syscalls::tests::init_platform(None);

        // A path-backed fd: must resolve back to the path it was opened with.
        task.sys_open(
            "/readlink_target",
            OFlags::CREAT | OFlags::WRONLY,
            Mode::RWXU,
        )
        .unwrap();
        let fd = task
            .sys_open("/readlink_target", OFlags::RDONLY, Mode::empty())
            .unwrap();
        let mut buf = [0u8; 64];
        let n = task
            .sys_readlink(alloc::format!("/proc/self/fd/{fd}"), &mut buf)
            .unwrap();
        assert_eq!(core::str::from_utf8(&buf[..n]).unwrap(), "/readlink_target");

        // A pathless fd (a pipe): must not panic, and must return a non-empty synthetic path
        // rather than erroring, matching real Linux's "pipe:[ino]"-style fallback.
        let (reader, _writer) = task.sys_pipe2(OFlags::empty()).unwrap();
        let mut buf2 = [0u8; 64];
        let n = task
            .sys_readlink(alloc::format!("/proc/self/fd/{reader}"), &mut buf2)
            .unwrap();
        assert!(
            n > 0,
            "must return a non-empty synthetic path for a pathless fd"
        );

        // An fd that was never opened: EBADF, not a panic.
        assert_eq!(
            task.sys_readlink("/proc/self/fd/999999", &mut buf2)
                .unwrap_err(),
            Errno::EBADF
        );
    }

    #[test]
    fn reopened_dev_stdout_and_stderr_get_stdio_stream_metadata() {
        let task = crate::syscalls::tests::init_platform(None);
        let files = task.files.borrow();

        for (path, expected) in [
            ("/dev/stdout", StdioStream::Stdout),
            ("/dev/stderr", StdioStream::Stderr),
        ] {
            let raw_fd = task
                .sys_open(path, OFlags::WRONLY, Mode::empty())
                .unwrap_or_else(|e| panic!("reopening {path} must succeed: {e:?}"));
            let fd = files
                .raw_descriptor_store
                .read()
                .fd_from_raw_integer::<crate::DefaultFS<crate::syscalls::tests::TestPlatform>>(
                    usize::try_from(raw_fd).unwrap(),
                )
                .unwrap();
            let stream = task
                .global
                .litebox
                .descriptor_table()
                .with_metadata(&fd, |stream: &StdioStream| *stream)
                .unwrap_or_else(|_| panic!("reopened {path} must carry StdioStream metadata"));
            assert_eq!(stream, expected);
        }
    }

    #[test]
    fn open_o_trunc_on_a_directory_returns_eisdir_instead_of_panicking() {
        // Regression test: `open(dir_path, O_TRUNC, ...)` used to panic (`unimplemented!()`) in
        // `From<OpenError> for Errno`, because the `OpenError::TruncateError(TruncateError::
        // IsDirectory)` case fell through to the catch-all arm instead of being mapped to
        // `EISDIR`. Triggerable via ordinary shell redirection (`cmd > /some/existing/dir`) or
        // any program that opens a path for writing without first checking whether it is a
        // directory.
        let task = crate::syscalls::tests::init_platform(None);
        task.sys_mkdirat(litebox_common_linux::AT_FDCWD, "/a_directory", 0o777)
            .unwrap();

        let err = task
            .sys_open(
                "/a_directory",
                OFlags::WRONLY | OFlags::TRUNC,
                Mode::empty(),
            )
            .unwrap_err();
        assert_eq!(err, Errno::EISDIR);
    }

    #[test]
    fn ppoll_with_sigmask_does_not_panic_and_reports_ready_fd() {
        // Regression test: `ppoll()` with a non-null sigmask used to unconditionally panic
        // (`unimplemented!("no sigmask support yet")`), which is the standard signal-safe-
        // polling idiom used by many real-world event loops/daemons to avoid the self-pipe
        // race. Mirrors the sigmask handling `sys_pselect` already implements correctly via
        // `with_temporary_signal_mask`.
        let task = crate::syscalls::tests::init_platform(None);
        let (reader, writer) = task.sys_pipe2(OFlags::empty()).unwrap();
        task.sys_write(i32::try_from(writer).unwrap(), b"x", None)
            .unwrap();

        let mut pollfd = litebox_common_linux::Pollfd {
            fd: i32::try_from(reader).unwrap(),
            events: 0x0001, // POLLIN
            revents: 0,
        };
        let fds_ptr = UserPtrMut::from_usize((&raw mut pollfd).expose_provenance());

        let sigmask = litebox_common_linux::signal::SigSet::empty();
        let sigmask_ptr = UserPtr::from_usize((&raw const sigmask).expose_provenance());

        let ready = task
            .sys_ppoll(
                fds_ptr,
                1,
                TimeParam::None,
                Some(sigmask_ptr),
                core::mem::size_of::<litebox_common_linux::signal::SigSet>(),
            )
            .unwrap();
        assert_eq!(ready, 1);
        assert_eq!(pollfd.revents, 0x0001);
    }

    #[test]
    fn ppoll_with_wrong_sigsetsize_returns_einval_instead_of_panicking() {
        let task = crate::syscalls::tests::init_platform(None);
        let mut pollfd = litebox_common_linux::Pollfd {
            fd: 0,
            events: 0x0001,
            revents: 0,
        };
        let fds_ptr = UserPtrMut::from_usize((&raw mut pollfd).expose_provenance());
        let sigmask = litebox_common_linux::signal::SigSet::empty();
        let sigmask_ptr = UserPtr::from_usize((&raw const sigmask).expose_provenance());

        let err = task
            .sys_ppoll(fds_ptr, 1, TimeParam::None, Some(sigmask_ptr), 1)
            .unwrap_err();
        assert_eq!(err, Errno::EINVAL);
    }

    #[test]
    fn epoll_pwait_with_sigmask_does_not_panic_and_reports_ready_fd() {
        // Regression test: `epoll_pwait()` with a non-null sigmask used to unconditionally
        // panic (`todo!("sigmask not supported")`).
        let task = crate::syscalls::tests::init_platform(None);
        let (reader, writer) = task.sys_pipe2(OFlags::empty()).unwrap();
        task.sys_write(i32::try_from(writer).unwrap(), b"x", None)
            .unwrap();

        let epfd = task
            .sys_epoll_create(litebox_common_linux::EpollCreateFlags::empty())
            .unwrap();
        let ctl_event = litebox_common_linux::EpollEvent {
            events: 0x0001, // EPOLLIN
            data: 0,
        };
        let ctl_event_ptr = UserPtr::from_usize((&raw const ctl_event).expose_provenance());
        task.sys_epoll_ctl(
            i32::try_from(epfd).unwrap(),
            litebox_common_linux::EpollOp::EpollCtlAdd,
            i32::try_from(reader).unwrap(),
            ctl_event_ptr,
        )
        .unwrap();

        let mut out_event = litebox_common_linux::EpollEvent { events: 0, data: 0 };
        let out_event_ptr = UserPtrMut::from_usize((&raw mut out_event).expose_provenance());
        let sigmask = litebox_common_linux::signal::SigSet::empty();
        let sigmask_ptr = UserPtr::from_usize((&raw const sigmask).expose_provenance());

        let ready = task
            .sys_epoll_pwait(
                i32::try_from(epfd).unwrap(),
                out_event_ptr,
                1,
                -1,
                Some(sigmask_ptr),
                core::mem::size_of::<litebox_common_linux::signal::SigSet>(),
            )
            .unwrap();
        assert_eq!(ready, 1);
        assert_eq!(out_event.events & 0x0001, 0x0001);
    }

    #[test]
    fn fcntl_getlk_and_setlk_on_a_pipe_return_einval_instead_of_panicking() {
        // Regression test: `fcntl(F_GETLK/F_SETLK/F_SETLKW)` on a pipe (or socket) fd used to
        // unconditionally panic (`todo!("pipes")`/`todo!("net")`). Real Linux's record locks
        // only apply to regular files and return EINVAL for a pipe/socket fd.
        let task = crate::syscalls::tests::init_platform(None);
        let (reader, _writer) = task.sys_pipe2(OFlags::empty()).unwrap();
        let reader = i32::try_from(reader).unwrap();

        let mut flock = litebox_common_linux::Flock {
            type_: litebox_common_linux::FlockType::ReadLock as i16,
            whence: 0,
            #[cfg(target_pointer_width = "64")]
            __pad0: 0,
            start: 0,
            len: 0,
            pid: 0,
            #[cfg(target_pointer_width = "64")]
            __pad1: 0,
        };
        let lock_ptr = UserPtrMut::from_usize((&raw mut flock).expose_provenance());
        assert_eq!(
            task.sys_fcntl(reader, FcntlArg::GETLK(lock_ptr))
                .unwrap_err(),
            Errno::EINVAL
        );

        let lock_ptr = UserPtr::from_usize((&raw const flock).expose_provenance());
        assert_eq!(
            task.sys_fcntl(reader, FcntlArg::SETLK(lock_ptr))
                .unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            task.sys_fcntl(reader, FcntlArg::SETLKW(lock_ptr))
                .unwrap_err(),
            Errno::EINVAL
        );
    }

    #[test]
    fn fioclex_on_a_pipe_sets_cloexec_instead_of_panicking() {
        // Regression test: `ioctl(fd, FIOCLEX)` on a pipe (or socket) fd used to unconditionally
        // panic (`todo!("pipes")`/`todo!("net")`), even though the underlying `set_fd_metadata`
        // call it needs to make is identical to every other fd type in this dispatch.
        let task = crate::syscalls::tests::init_platform(None);
        let (reader, _writer) = task.sys_pipe2(OFlags::empty()).unwrap();
        let reader = i32::try_from(reader).unwrap();

        assert_eq!(task.sys_ioctl(reader, IoctlArg::FIOCLEX), Ok(0));
        let flags = task.sys_fcntl(reader, FcntlArg::GETFD).unwrap();
        assert_eq!(
            flags & litebox_common_linux::FileDescriptorFlags::FD_CLOEXEC.bits(),
            litebox_common_linux::FileDescriptorFlags::FD_CLOEXEC.bits()
        );
    }

    #[test]
    fn pipe2_o_direct_returns_einval_instead_of_panicking() {
        // Regression test: pipe2(..., O_DIRECT) ("packet mode", not implemented by this shim's
        // pipes) used to unconditionally panic (todo!("O_DIRECT not supported")).
        let task = crate::syscalls::tests::init_platform(None);
        assert_eq!(task.sys_pipe2(OFlags::DIRECT).unwrap_err(), Errno::EINVAL);
    }
}
