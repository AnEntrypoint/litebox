// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A table-driven [`Backend`] for read-only files whose content is fixed for a guest's lifetime.
//!
//! # Why this exists
//!
//! A guest desktop reads a long tail of `/proc` and `/sys` files whose real content is a fixed
//! string on any machine litebox will ever emulate -- `/proc/sys/crypto/fips_enabled` is `0`,
//! `/sys/module/apparmor/parameters/enabled` is `N`, `/proc/sys/kernel/cap_last_cap` is a kernel
//! version constant. Each one previously needed either its own ~270-line [`Backend`]
//! implementation (see the `ProcSysKernel` this replaces, which served exactly two files that
//! way) or nothing at all, and "nothing at all" is what the guest got: a live XFCE session was
//! observed asking for eleven distinct such paths and receiving `ENOENT` for every one.
//!
//! Writing an eleventh copy of that boilerplate is the wrong answer to "the guest wants one more
//! constant". This is the mechanism instead: a `(path, bytes)` table, one `Backend`
//! implementation over it, mounted wherever a subtree of constants is wanted. Adding a file is a
//! table row.
//!
//! Paths in the table may contain `/`; the intermediate directories are derived from the table
//! itself at construction, so `cpu0/cpu_capacity` creates a listable `cpu0` with no extra
//! declaration.
//!
//! The table is owned rather than `&'static`, so a value that is constant for the life of a guest
//! but not known at compile time belongs here too -- `/sys/devices/system/cpu/online` is
//! `0-{cpu_count-1}`, fixed once the host CPU count is read and never changing afterwards. The
//! alternative was leaking a `String` per boot to satisfy a `'static` bound, which buys nothing:
//! these files are a few bytes each and are read, not mapped.
//!
//! # What this is not
//!
//! Not a substitute for a synthesized file whose content depends on live state. `/proc/stat`,
//! `/proc/meminfo` and `/proc/self/*` are NOT constants and do not belong here -- they live in
//! [`super::procfs`], which can see the state they report. The test for belonging here is whether
//! the bytes can change while the guest is running.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use alloc::borrow::ToOwned as _;

use crate::LiteBox;
use crate::sync::RawSyncPrimitivesProvider;

use super::backend::{
    Backend, BackendHandles, DirHandle, FileHandle, PermissionCheck, Permissioned, SeekBehavior,
    WalkOutcome, WalkStopReason, WalkedComponent, WalkingDirHandle,
};
use super::errors::{
    ChmodError, ChownError, FileStatusError, MkdirError, OpenError, PathError, ReadDirError,
    ReadError, RmdirError, SetTimesError, TruncateError, UnlinkError, WalkError, WriteError,
};
use super::inode_allocator::InodeAllocator;
use super::{DirEntry, FileStatus, FileType, Mode, NodeInfo, OFlags, Timestamp, UserInfo};

/// One served file: a path relative to the mount root (`/`-separated, no leading `/`), and the
/// exact bytes a read of it returns.
pub type StaticFile = (String, Vec<u8>);

/// Builds a [`StaticFile`] row from the literal form a table is most readable in.
#[must_use]
pub fn file(path: &str, content: &[u8]) -> StaticFile {
    (path.to_owned(), content.to_owned())
}

/// Splits a table path into `(parent directory, basename)`. The mount root's own path is `""`.
fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// A [`Backend`] serving a fixed table of constant files. See the module doc comment.
pub struct StaticFiles<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    files: Vec<StaticFile>,
    /// Inode per file, index-parallel to [`Self::files`].
    file_inodes: Vec<NodeInfo>,
    /// Every directory in the tree, derived from the table. Index 0 is always the mount root,
    /// whose path is `""`.
    dirs: Vec<(String, NodeInfo)>,
    _alloc: InodeAllocator,
}

impl<Platform> StaticFiles<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a backend serving `files`.
    ///
    /// Intermediate directories are derived from the table's own paths, so a table row of
    /// `"cpu0/cpu_capacity"` yields a listable `cpu0` directory without declaring it.
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        allocator: InodeAllocator,
        files: Vec<StaticFile>,
    ) -> Self {
        let mut dirs: Vec<(String, NodeInfo)> = vec![(String::new(), allocator.next())];
        for (path, _) in &files {
            for (i, byte) in path.bytes().enumerate() {
                if byte != b'/' {
                    continue;
                }
                let prefix = &path[..i];
                if !dirs.iter().any(|(d, _)| d == prefix) {
                    dirs.push((prefix.to_owned(), allocator.next()));
                }
            }
        }
        let file_inodes = files.iter().map(|_| allocator.next()).collect();
        Self {
            _litebox: litebox.clone(),
            files,
            file_inodes,
            dirs,
            _alloc: allocator,
        }
    }

    /// The index of the directory named `name` directly inside directory index `parent`.
    fn child_dir(&self, parent: usize, name: &str) -> Option<usize> {
        let parent_path = self.dirs[parent].0.as_str();
        self.dirs.iter().position(|(d, _)| {
            let (p, n) = split_parent(d);
            !d.is_empty() && p == parent_path && n == name
        })
    }

    /// The index of the file named `name` directly inside directory index `parent`.
    fn child_file(&self, parent: usize, name: &str) -> Option<usize> {
        let parent_path = self.dirs[parent].0.as_str();
        self.files.iter().position(|(path, _)| {
            let (p, n) = split_parent(path);
            p == parent_path && n == name
        })
    }
}

/// Directory handle: an index into [`StaticFiles::dirs`].
#[derive(Debug, Clone, Copy)]
pub struct StaticFilesDirHandle(usize);

/// File handle: an index into [`StaticFiles::files`].
#[derive(Debug, Clone, Copy)]
pub struct StaticFilesFileHandle(usize);

impl<Platform> super::backend::private::Sealed for StaticFiles<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for StaticFiles<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = StaticFilesDirHandle;
    type FileHandle = StaticFilesFileHandle;
    type DirHandle = StaticFilesDirHandle;
}

impl<Platform> Backend for StaticFiles<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(StaticFilesDirHandle(0))
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let mut current = from.into_typed::<Self>();
        let mut walked = Vec::with_capacity(components.len());
        for component in components {
            if let Some(child) = self.child_dir(current.0, component) {
                walked.push(WalkedComponent {
                    // Everything here is world-readable and world-traversable with no per-entry
                    // variation, so there is nothing for the resolver to check that this backend
                    // has not already decided.
                    permissions: PermissionCheck::ByBackend,
                });
                current = StaticFilesDirHandle(child);
                continue;
            }
            if self.child_file(current.0, component).is_some() {
                return Ok(WalkOutcome {
                    components: walked,
                    last: WalkingDirHandle::from_typed::<Self>(current),
                    stop_reason: WalkStopReason::StoppedAtNonDirectory,
                });
            }
            return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
        }
        Ok(WalkOutcome {
            components: walked,
            last: WalkingDirHandle::from_typed::<Self>(current),
            stop_reason: WalkStopReason::CompleteDirectory,
        })
    }

    fn owned_dir_at(
        &self,
        dir: WalkingDirHandle<'_>,
        _flags: OFlags,
    ) -> Result<DirHandle, OpenError> {
        Ok(DirHandle::from_typed::<Self>(dir.into_typed::<Self>()))
    }

    fn walking_dir_at<'a>(&'a self, dir: &DirHandle) -> Option<WalkingDirHandle<'a>> {
        Some(WalkingDirHandle::from_typed::<Self>(
            *dir.get_typed::<Self>(),
        ))
    }

    fn open_file_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
        flags: OFlags,
    ) -> Result<Permissioned<FileHandle>, OpenError> {
        let dir = dir.into_typed::<Self>();
        if let Some(idx) = self.child_file(dir.0, name) {
            if flags.contains(OFlags::DIRECTORY) {
                return Err(OpenError::PathError(PathError::ComponentNotADirectory));
            }
            return Ok(Permissioned {
                item: FileHandle::from_typed::<Self>(StaticFilesFileHandle(idx)),
                permissions: PermissionCheck::ByBackend,
            });
        }
        // A directory reached through `open_file_at` is a real entry, just not a file. Reporting
        // `NoSuchFileOrDirectory` for something that demonstrably exists would misinform a caller
        // deciding whether to create it. `ComponentNotADirectory` is what `tar_ro` answers in the
        // same situation -- see its own `open_file_at` for why this generic error is the closest
        // available fit, there being no `IsADirectory` variant on `PathError`.
        if self.child_dir(dir.0, name).is_some() {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        Err(OpenError::PathError(PathError::NoSuchFileOrDirectory))
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let handle = handle.into_typed::<Self>();
        let parent = self.dirs[handle.0].0.as_str();
        let mut entries: Vec<DirEntry> = Vec::new();
        for (idx, (path, node)) in self.dirs.iter().enumerate() {
            let _ = idx;
            if path.is_empty() {
                continue;
            }
            let (p, n) = split_parent(path);
            if p == parent {
                entries.push(DirEntry {
                    name: String::from(n),
                    file_type: FileType::Directory,
                    ino_info: Some(node.clone()),
                });
            }
        }
        for (idx, (path, _)) in self.files.iter().enumerate() {
            let (p, n) = split_parent(path);
            if p == parent {
                entries.push(DirEntry {
                    name: String::from(n),
                    file_type: FileType::RegularFile,
                    ino_info: Some(self.file_inodes[idx].clone()),
                });
            }
        }
        Ok(entries)
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let content = self.files[h.get_typed::<Self>().0].1.as_slice();
        if offset >= content.len() {
            return Ok(0);
        }
        let remaining = &content[offset..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        Ok(n)
    }

    fn write(&self, _h: &FileHandle, _buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        Err(WriteError::NotForWriting)
    }

    fn truncate(&self, _h: &FileHandle, _len: usize) -> Result<(), TruncateError> {
        Err(TruncateError::NotForWriting)
    }

    fn chmod(&self, _h: &FileHandle, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
    }

    fn seek_behavior(&self, _h: &FileHandle) -> SeekBehavior {
        SeekBehavior::PositionBased
    }

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        let idx = h.get_typed::<Self>().0;
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size: self.files[idx].1.len(),
            owner: UserInfo::ROOT,
            node_info: self.file_inodes[idx].clone(),
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let idx = h.get_typed::<Self>().0;
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info: self.dirs[idx].1.clone(),
            blksize: super::DEFAULT_DIRECTORY_SIZE,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn create_file_at(
        &self,
        _dir: DirHandle,
        _name: &str,
        _mode: Mode,
    ) -> Result<FileHandle, OpenError> {
        Err(OpenError::ReadOnlyFileSystem)
    }

    fn mkdir_at(&self, _dir: DirHandle, _name: &str, _mode: Mode) -> Result<DirHandle, MkdirError> {
        Err(MkdirError::ReadOnlyFileSystem)
    }

    fn unlink_at(&self, _dir: DirHandle, _name: &str) -> Result<(), UnlinkError> {
        Err(UnlinkError::ReadOnlyFileSystem)
    }

    fn rmdir_at(&self, _dir: DirHandle, _name: &str) -> Result<(), RmdirError> {
        Err(RmdirError::ReadOnlyFileSystem)
    }

    fn chmod_at(&self, _dir: DirHandle, _name: &str, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
    }

    fn chown_at(
        &self,
        _dir: DirHandle,
        _name: &str,
        _user: Option<u16>,
        _group: Option<u16>,
    ) -> Result<(), ChownError> {
        Err(ChownError::ReadOnlyFileSystem)
    }

    fn set_times_at(
        &self,
        _dir: DirHandle,
        _name: &str,
        _atime: Option<Timestamp>,
        _mtime: Option<Timestamp>,
    ) -> Result<(), SetTimesError> {
        Err(SetTimesError::ReadOnlyFileSystem)
    }
}
