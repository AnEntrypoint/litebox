// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! An in-memory file system, not backed by any physical device.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::LiteBox;
use crate::path::Arg;
use crate::sync;

use super::errors::{
    ChmodError, ChownError, CloseError, FileStatusError, MkdirError, OpenError, PathError,
    ReadDirError, ReadError, ReadLinkError, RenameError, RmdirError, SeekError, SymlinkError,
    TruncateError, UnlinkError, WriteError,
};
use super::{DirEntry, FileStatus, FileType, Mode, NodeInfo, SeekWhence, UserInfo};

/// Just a random constant that is distinct from other file systems. In this case, it is
/// `b'IMem'.hex()`.
const DEVICE_ID: usize = 0x494d656d;

/// Block size for file system I/O operations
// TODO(jayb): Determine appropriate block size
const BLOCK_SIZE: usize = 0;

/// A backing implementation for [`FileSystem`](super::FileSystem) storing all files in-memory.
///
/// # Warning
///
/// This has no physical backing store, thus any files in memory are erased as soon as this object
/// is dropped.
pub struct FileSystem<Platform: sync::RawSyncPrimitivesProvider> {
    litebox: LiteBox<Platform>,
    // TODO: Possibly support a single-threaded variant that doesn't have the cost of requiring a
    // sync-primitives platform, as well as cost of mutexes and such?
    root: sync::RwLock<Platform, RootDir<Platform>>,
    current_user: UserInfo,
    // cwd invariant: always ends with a `/`
    current_working_dir: String,
    // a source of freshness for providing unique IDs
    unique_id_freshness: core::sync::atomic::AtomicUsize,
}

impl<Platform: sync::RawSyncPrimitivesProvider> FileSystem<Platform> {
    /// Construct a new `FileSystem` instance
    ///
    /// This function is expected to only be invoked once per platform, as an initialiation step,
    /// and the created `FileSystem` handle is expected to be shared across all usage over the
    /// system.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>) -> Self {
        let litebox = litebox.clone();
        let root = sync::RwLock::new(RootDir::new());
        Self {
            litebox,
            root,
            current_user: UserInfo {
                user: 1000,
                group: 1000,
            },
            current_working_dir: "/".into(),
            unique_id_freshness: 1.into(), // the root dir gets unique ID of 0
        }
    }

    /// Permanently change the fixed uid/gid used for all subsequent permission checks against
    /// this file system (i.e. the "current user" of the single, fixed set of credentials the
    /// whole sandboxed guest runs as -- see the `setuid` syscall handler's doc comment).
    ///
    /// [`FileSystem::new`] defaults this to an unprivileged uid/gid, but some guest rootfs
    /// layouts (e.g. an OCI/container image such as Alpine, whose `/`, `/etc`, `/lib`, etc. are
    /// root-owned at mode `0755` since a real container's initial process runs as root absent an
    /// explicit `USER` directive) require the guest to actually run as root in order to write
    /// into those directories, matching what a real container would allow.
    ///
    /// This is distinct from [`FileSystem::with_root_privileges`], which only grants root
    /// privileges for the duration of a closure (intended for one-off internal setup); this
    /// method changes the persistent identity used for every future operation until changed
    /// again.
    pub fn set_default_user(&mut self, user: u16, group: u16) {
        self.current_user = UserInfo { user, group };
    }

    /// Execute `f` with superuser/root privileges.
    ///
    /// This function primarily exists to initialize files. Most regular interaction with the file
    /// system should be done without this function.
    pub fn with_root_privileges<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let original_user = core::mem::replace(&mut self.current_user, UserInfo::ROOT);
        f(self);
        let root_again = core::mem::replace(&mut self.current_user, original_user);
        if root_again.user != UserInfo::ROOT.user || root_again.group != UserInfo::ROOT.group {
            unreachable!()
        }
    }

    /// Initialize a primarily read-heavy file with static data.
    ///
    /// While this function could technically work with write-heavy files, it has performance
    /// benefits _particularly_ for files that are read-only, compared to doing open+write
    /// operations.
    ///
    /// The file is initialized with clone-on-write semantics for the data, meaning that the first
    /// time a write occurs on the file, it suffers the penalty of the entire data being cloned into
    /// memory, which is why this is intended primarily for read-only files (such as executables).
    ///
    /// # Panics
    ///
    /// Panics if used on
    /// - a closed FD
    /// - a non-file FD
    /// - a file that already contains data
    pub fn initialize_primarily_read_heavy_file(
        &mut self,
        fd: &FileFd<Platform>,
        data: alloc::borrow::Cow<'static, [u8]>,
    ) {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File {
            file,
            read_allowed: _,
            write_allowed: _,
            position: _,
            append_mode: _,
        } = &mut descriptor_table.get_entry_mut(fd).unwrap().entry
        else {
            panic!("must only be used on files, not directories")
        };
        let mut file = file.write();
        assert!(
            file.data.is_empty(),
            "must only be used on empty files during initialization"
        );
        file.data = data;
    }

    /// Execute `f` as a specific user (for testing purposes).
    #[cfg(test)]
    pub fn with_user<F>(&mut self, user: u16, group: u16, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let test_user = UserInfo { user, group };
        let original_user = core::mem::replace(&mut self.current_user, test_user);
        f(self);
        let test_user_again = core::mem::replace(&mut self.current_user, original_user);
        if test_user_again.user != test_user.user || test_user_again.group != test_user.group {
            unreachable!()
        }
    }

    /// (Private) Provide a fresh unique ID
    fn fresh_id(&self) -> usize {
        let res = self
            .unique_id_freshness
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        assert_ne!(
            res,
            usize::MAX,
            "we never expect to hit this, but if we do, someone has made way too many files in this session"
        );
        res
    }
}

impl<Platform: sync::RawSyncPrimitivesProvider> super::private::Sealed for FileSystem<Platform> {}

/// Maximum number of final-component symlink hops [`FileSystem::resolve_final_symlinks`] will
/// transparently follow before giving up with `ELOOP`-equivalent behavior. This is intentionally
/// a small, fixed bound rather than full POSIX loop-safe resolution -- see that method's docs.
const MAX_SYMLINK_HOPS: u32 = 8;

impl<Platform: sync::RawSyncPrimitivesProvider> FileSystem<Platform> {
    // Gives the absolute path for `path`, resolving any `.` or `..`s, and making sure to account
    // for any relative paths from current working directory.
    //
    // Note: does NOT account for symlinks.
    fn absolute_path(&self, path: impl crate::path::Arg) -> Result<String, PathError> {
        assert!(self.current_working_dir.ends_with('/'));
        let path = path.as_rust_str()?;
        if path.starts_with('/') {
            // Absolute path
            Ok(path.normalized()?)
        } else {
            // Relative path
            Ok((self.current_working_dir.clone() + path.as_rust_str()?).normalized()?)
        }
    }

    /// Given an already-normalized absolute `path`, transparently follow the *final path
    /// component* if it names a symlink, repeatedly, up to [`MAX_SYMLINK_HOPS`] times.
    ///
    /// This deliberately does NOT resolve symlinks appearing in intermediate/parent path
    /// components (e.g. `/a/b/c` where `a` or `b` is itself a symlink) -- that would require full
    /// POSIX-style multi-component loop-safe resolution, which is out of scope here. This is
    /// sufficient for the scenario this exists to support: `open()`-time resolution of a
    /// shared-library symlink (e.g. `libfoo.so -> libfoo.so.1.2.3`), where the symlink is always
    /// the final component of the path being opened.
    ///
    /// A relative symlink target is resolved relative to the directory containing the symlink
    /// itself (matching Linux semantics). Returns `Ok(resolved_path)` where `resolved_path` is
    /// either `path` unchanged (not a symlink) or the final target path after following all hops.
    /// Returns [`PathError::TooManySymlinkHops`] (`ELOOP`) if more than [`MAX_SYMLINK_HOPS`] hops
    /// would be required.
    fn resolve_final_symlinks(&self, path: String) -> Result<String, PathError> {
        let mut current = path;
        for _ in 0..MAX_SYMLINK_HOPS {
            let root = self.root.read();
            let (_, entry) = root.parent_and_entry(&current, self.current_user)?;
            let Some(Entry::Symlink(symlink)) = entry else {
                return Ok(current);
            };
            let target = symlink.read().target.clone();
            drop(root);
            current = if target.starts_with('/') {
                target.normalized()?
            } else {
                let dir = current.rsplit_once('/').map_or("", |(dir, _)| dir);
                alloc::format!("{dir}/{target}").normalized()?
            };
        }
        Err(PathError::TooManySymlinkHops)
    }
}

impl<Platform: sync::RawSyncPrimitivesProvider> super::FileSystem for FileSystem<Platform> {
    fn open(
        &self,
        path: impl crate::path::Arg,
        mut flags: super::OFlags,
        mode: super::Mode,
    ) -> Result<FileFd<Platform>, OpenError> {
        use super::OFlags;
        let currently_supported_oflags: OFlags = OFlags::CREAT
            | OFlags::RDONLY
            | OFlags::WRONLY
            | OFlags::RDWR
            | OFlags::TRUNC
            | OFlags::NOCTTY
            | OFlags::EXCL
            | OFlags::DIRECTORY
            | OFlags::NONBLOCK
            | OFlags::LARGEFILE
            | OFlags::NOFOLLOW
            | OFlags::APPEND;
        if flags.intersects(currently_supported_oflags.complement()) {
            unimplemented!("{flags:?}")
        }
        let path = self.absolute_path(path)?;
        // Transparently follow a final-component symlink (e.g. dynamic-linker resolution of a
        // shared-library symlink), unless the caller explicitly asked not to (`O_NOFOLLOW`) or is
        // creating the file (in which case there is nothing to follow yet, and `O_CREAT` should
        // create/replace exactly the named path, not some other path a stale symlink points at).
        let path = if flags.contains(OFlags::NOFOLLOW) || flags.contains(OFlags::CREAT) {
            path
        } else {
            self.resolve_final_symlinks(path)?
        };
        let (entry, created) = if flags.contains(OFlags::CREAT) {
            let mut root = self.root.write();
            let (parent, entry) = root.parent_and_entry(&path, self.current_user)?;
            if let Some(entry) = entry {
                if flags.contains(OFlags::EXCL) {
                    return Err(OpenError::AlreadyExists);
                }
                (entry, false)
            } else {
                let Some((_, parent)) = parent else {
                    // Only `/` does not have a parent; any other scenario (e.g., missing ancestor)
                    // is handled already by a `PathError`. If `/` was passed, then it would have
                    // gotten `Some(entry)` out already. Thus, this is unreachable.
                    unreachable!()
                };
                let mut parent = parent.write();
                if !self.current_user.can_write(&parent.perms) {
                    return Err(OpenError::NoWritePerms);
                }
                // When both O_CREAT and O_DIRECTORY are specified in flags and the
                // file specified by pathname does not exist, open() will create a
                // regular file (i.e., O_DIRECTORY is ignored).
                flags.remove(OFlags::DIRECTORY);
                let old = parent.children.insert(
                    path.components().unwrap().last().unwrap().into(),
                    FileType::RegularFile,
                );
                assert!(old.is_none());
                let entry = Entry::File(Arc::new(sync::RwLock::new(FileX {
                    perms: Permissions {
                        mode,
                        userinfo: self.current_user,
                    },
                    data: Vec::new().into(),
                    unique_id: self.fresh_id(),
                })));
                let old = root.entries.insert(path, entry.clone());
                assert!(old.is_none());
                (entry, true)
            }
        } else {
            let root = self.root.read();
            let (_, entry) = root.parent_and_entry(&path, self.current_user)?;
            let Some(entry) = entry else {
                return Err(PathError::NoSuchFileOrDirectory)?;
            };
            (entry, false)
        };
        let access_mode = flags & (OFlags::WRONLY | OFlags::RDWR);
        let read_allowed = if access_mode == OFlags::RDONLY || access_mode == OFlags::RDWR {
            if !created && !self.current_user.can_read(&entry.perms()) {
                return Err(OpenError::AccessNotAllowed);
            }
            true
        } else {
            false
        };
        let write_allowed = if access_mode == OFlags::WRONLY || access_mode == OFlags::RDWR {
            if !created && !self.current_user.can_write(&entry.perms()) {
                return Err(OpenError::AccessNotAllowed);
            }
            true
        } else {
            false
        };
        let append_mode = flags.contains(OFlags::APPEND);
        let fd = match entry {
            Entry::File(file) => {
                if flags.contains(OFlags::DIRECTORY) {
                    return Err(OpenError::PathError(PathError::ComponentNotADirectory));
                }
                self.litebox
                    .descriptor_table_mut()
                    .insert(Descriptor::File {
                        file: file.clone(),
                        read_allowed,
                        write_allowed,
                        position: 0,
                        append_mode,
                    })
            }
            Entry::Dir(dir) => self
                .litebox
                .descriptor_table_mut()
                .insert(Descriptor::Dir { dir: dir.clone() }),
            Entry::Symlink(_) => {
                // Only reachable when the caller passed `O_NOFOLLOW` (symlink-following was
                // skipped above) and the final path component is in fact a symlink -- matches
                // Linux's `open(O_NOFOLLOW)` on a symlink, which fails with `ELOOP`.
                return Err(OpenError::PathError(PathError::TooManySymlinkHops));
            }
        };
        if flags.contains(OFlags::TRUNC) {
            match self.truncate(&fd, 0, true) {
                Ok(()) => {}
                Err(e) => {
                    self.close(&fd).unwrap();
                    return Err(e.into());
                }
            }
        }
        Ok(fd)
    }

    fn close(&self, fd: &FileFd<Platform>) -> Result<(), CloseError> {
        self.litebox.descriptor_table_mut().remove(fd);
        Ok(())
    }

    fn read(
        &self,
        fd: &FileFd<Platform>,
        buf: &mut [u8],
        mut offset: Option<usize>,
    ) -> Result<usize, ReadError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File {
            file,
            read_allowed,
            write_allowed: _,
            position,
            append_mode: _,
        } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(ReadError::ClosedFd)?
            .entry
        else {
            return Err(ReadError::NotAFile);
        };
        if !*read_allowed {
            return Err(ReadError::NotForReading);
        }
        let position = offset.as_mut().unwrap_or(position);
        let file = file.read();
        let start = (*position).min(file.data.len());
        let end = position
            .checked_add(buf.len())
            .unwrap()
            .min(file.data.len());
        debug_assert!(start <= end);
        let retlen = end - start;
        buf[..retlen].copy_from_slice(&file.data[start..end]);
        *position = end;
        Ok(retlen)
    }

    fn write(
        &self,
        fd: &FileFd<Platform>,
        buf: &[u8],
        mut offset: Option<usize>,
    ) -> Result<usize, WriteError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File {
            file,
            read_allowed: _,
            write_allowed,
            position,
            append_mode,
        } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(WriteError::ClosedFd)?
            .entry
        else {
            return Err(WriteError::NotAFile);
        };
        if !*write_allowed {
            return Err(WriteError::NotForWriting);
        }
        // For append mode, we always write at the end of the file.
        // Note: pwrite (offset != None) ignores append mode per POSIX.
        let mut file = file.write();
        let write_position = if *append_mode && offset.is_none() {
            file.data.len()
        } else {
            *offset.as_mut().unwrap_or(position)
        };
        let end_position = write_position.checked_add(buf.len()).unwrap();
        let start = if write_position < file.data.len() {
            let start = write_position;
            let end = end_position.min(file.data.len());
            debug_assert!(start <= end);
            let first_half_len = end - start;
            file.data.to_mut()[start..end].copy_from_slice(&buf[..first_half_len]);
            first_half_len
        } else {
            if write_position > file.data.len() {
                // Need to pad with 0s because position was past the end of the file
                file.data.to_mut().resize(write_position, 0);
            }
            0
        };
        file.data.to_mut().extend(&buf[start..]);
        // Update the file position for positional writes (not pwrite)
        if offset.is_none() {
            *position = end_position;
        }
        Ok(buf.len())
    }

    fn seek(
        &self,
        fd: &FileFd<Platform>,
        offset: isize,
        whence: SeekWhence,
    ) -> Result<usize, SeekError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File {
            file,
            read_allowed: _,
            write_allowed: _,
            position,
            append_mode: _,
        } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(SeekError::ClosedFd)?
            .entry
        else {
            return Err(SeekError::NotAFile);
        };
        let file_len = file.read().data.len();
        let base = match whence {
            SeekWhence::RelativeToBeginning => 0,
            SeekWhence::RelativeToCurrentOffset => *position,
            SeekWhence::RelativeToEnd => file_len,
        };
        let new_posn = base
            .checked_add_signed(offset)
            .ok_or(SeekError::InvalidOffset)?;
        if new_posn > file_len {
            Err(SeekError::InvalidOffset)
        } else {
            *position = new_posn;
            Ok(new_posn)
        }
    }

    fn truncate(
        &self,
        fd: &FileFd<Platform>,
        length: usize,
        reset_offset: bool,
    ) -> Result<(), TruncateError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File {
            file,
            read_allowed: _,
            write_allowed,
            position,
            append_mode: _,
        } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(TruncateError::ClosedFd)?
            .entry
        else {
            return Err(TruncateError::IsDirectory);
        };
        if !*write_allowed {
            return Err(TruncateError::NotForWriting);
        }
        let mut file_data = file.write();
        match length.cmp(&file_data.data.len()) {
            core::cmp::Ordering::Less => match &mut file_data.data {
                alloc::borrow::Cow::Borrowed(d) => {
                    *d = &d[..length];
                }
                alloc::borrow::Cow::Owned(d) => d.truncate(length),
            },
            core::cmp::Ordering::Equal => (),
            core::cmp::Ordering::Greater => file_data.data.to_mut().resize(length, 0),
        }
        if reset_offset {
            *position = 0;
        }
        Ok(())
    }

    fn chmod(&self, path: impl crate::path::Arg, mode: super::Mode) -> Result<(), ChmodError> {
        let path = self.absolute_path(path)?;
        let root = self.root.read();
        let (_, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        match entry {
            Entry::File(file) => {
                let perms = &mut file.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChmodError::NotTheOwner);
                }
                perms.mode = mode;
                Ok(())
            }
            Entry::Dir(dir) => {
                let perms = &mut dir.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChmodError::NotTheOwner);
                }
                perms.mode = mode;
                Ok(())
            }
            Entry::Symlink(symlink) => {
                // Linux's `chmod` follows symlinks and applies to the target; since this bounded
                // implementation doesn't recurse `chmod` through `resolve_final_symlinks`, and a
                // symlink's own permission bits are never actually consulted (see
                // `resolve_final_symlinks`/`read_link`, which only check ownership for chmod
                // itself, not searchability), we permissively apply the mode directly to the
                // symlink's own (otherwise-unused) permission bits rather than erroring.
                let perms = &mut symlink.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChmodError::NotTheOwner);
                }
                perms.mode = mode;
                Ok(())
            }
        }
    }

    fn chown(
        &self,
        path: impl crate::path::Arg,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError> {
        let path = self.absolute_path(path)?;
        let root = self.root.read();
        let (_, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        match entry {
            Entry::File(file) => {
                let perms = &mut file.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChownError::NotTheOwner);
                }
                if let Some(new_user) = user {
                    perms.userinfo.user = new_user;
                }
                if let Some(new_group) = group {
                    perms.userinfo.group = new_group;
                }
                Ok(())
            }
            Entry::Dir(dir) => {
                let perms = &mut dir.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChownError::NotTheOwner);
                }
                if let Some(new_user) = user {
                    perms.userinfo.user = new_user;
                }
                if let Some(new_group) = group {
                    perms.userinfo.group = new_group;
                }
                Ok(())
            }
            Entry::Symlink(symlink) => {
                let perms = &mut symlink.write().perms;
                if !(self.current_user.user == 0 || self.current_user.user == perms.userinfo.user) {
                    return Err(ChownError::NotTheOwner);
                }
                if let Some(new_user) = user {
                    perms.userinfo.user = new_user;
                }
                if let Some(new_group) = group {
                    perms.userinfo.group = new_group;
                }
                Ok(())
            }
        }
    }

    fn unlink(&self, path: impl crate::path::Arg) -> Result<(), UnlinkError> {
        let path = self.absolute_path(path)?;
        let mut root = self.root.write();
        let (parent, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some((_, parent)) = parent else {
            // Attempted to remove `/`
            return Err(UnlinkError::IsADirectory);
        };
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        if let Entry::Dir(_) = entry {
            return Err(UnlinkError::IsADirectory);
        }
        let mut parent = parent.write();
        if !self.current_user.can_write(&parent.perms) {
            return Err(UnlinkError::NoWritePerms);
        }
        let removed = parent
            .children
            .remove(path.components().unwrap().last().unwrap());
        // Just a sanity check
        assert!(matches!(
            removed,
            Some(FileType::RegularFile | FileType::Symlink)
        ));
        let removed = root.entries.remove(&path).unwrap();
        // Just a sanity check
        assert!(matches!(
            removed,
            Entry::File(File { .. }) | Entry::Symlink(_)
        ));
        Ok(())
    }

    fn rename(
        &self,
        from: impl crate::path::Arg,
        to: impl crate::path::Arg,
    ) -> Result<(), RenameError> {
        let from = self.absolute_path(from)?;
        let to = self.absolute_path(to)?;
        if from == to {
            // Renaming a path onto itself is always a (redundant) success on Linux, provided the
            // path actually exists.
            let root = self.root.write();
            let (_, entry) = root.parent_and_entry(&from, self.current_user)?;
            return if entry.is_some() {
                Ok(())
            } else {
                Err(PathError::NoSuchFileOrDirectory)?
            };
        }

        let mut root = self.root.write();

        let (from_parent, from_entry) = root.parent_and_entry(&from, self.current_user)?;
        let Some((from_parent_path, from_parent)) = from_parent else {
            // Attempted to rename `/` itself.
            return Err(RenameError::IsADirectory);
        };
        let Some(from_entry) = from_entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        // Only regular-file rename is supported -- see `FileSystem::rename`'s docs.
        if let Entry::Dir(_) = from_entry {
            return Err(RenameError::IsADirectory);
        }

        let (to_parent, to_entry) = root.parent_and_entry(&to, self.current_user)?;
        let Some((to_parent_path, to_parent)) = to_parent else {
            // Attempted to rename onto `/` itself.
            return Err(RenameError::DestinationIsADirectory);
        };
        if let Some(Entry::Dir(_)) = to_entry {
            return Err(RenameError::DestinationIsADirectory);
        }

        let from_name = from.components().unwrap().last().unwrap().to_owned();
        let to_name = to.components().unwrap().last().unwrap().to_owned();
        let same_parent = from_parent_path == to_parent_path;

        // Check write permission on both parent directories before mutating anything. When
        // `from` and `to` share a parent, lock it only once (`RwLock` is not reentrant, so
        // locking it twice here would deadlock).
        if same_parent {
            let mut parent = from_parent.write();
            if !self.current_user.can_write(&parent.perms) {
                return Err(RenameError::NoWritePerms);
            }
            let removed = parent.children.remove(&from_name);
            debug_assert!(matches!(removed, Some(FileType::RegularFile)));
            parent.children.insert(to_name, FileType::RegularFile);
        } else {
            if !self.current_user.can_write(&from_parent.read().perms)
                || !self.current_user.can_write(&to_parent.read().perms)
            {
                return Err(RenameError::NoWritePerms);
            }
            {
                let mut from_parent = from_parent.write();
                let removed = from_parent.children.remove(&from_name);
                debug_assert!(matches!(removed, Some(FileType::RegularFile)));
            }
            {
                let mut to_parent = to_parent.write();
                to_parent.children.insert(to_name, FileType::RegularFile);
            }
        }

        let moved = root.entries.remove(&from).unwrap();
        debug_assert!(matches!(moved, Entry::File(_)));
        root.entries.insert(to, moved);

        Ok(())
    }

    fn symlink(
        &self,
        target: impl crate::path::Arg,
        linkpath: impl crate::path::Arg,
    ) -> Result<(), SymlinkError> {
        let target = target.as_rust_str().map_err(PathError::from)?.to_owned();
        let linkpath = self.absolute_path(linkpath)?;
        let mut root = self.root.write();
        let (parent, entry) = root.parent_and_entry(&linkpath, self.current_user)?;
        if entry.is_some() {
            return Err(SymlinkError::AlreadyExists);
        }
        let Some((_, parent)) = parent else {
            // Only `/` does not have a parent; `/` always already exists, so `entry.is_some()`
            // above would already have returned `AlreadyExists`. Thus, this is unreachable.
            unreachable!()
        };
        let mut parent = parent.write();
        if !self.current_user.can_write(&parent.perms) {
            return Err(SymlinkError::NoWritePerms);
        }
        let old = parent.children.insert(
            linkpath.components().unwrap().last().unwrap().into(),
            FileType::Symlink,
        );
        assert!(old.is_none());
        let old = root.entries.insert(
            linkpath,
            Entry::Symlink(Arc::new(sync::RwLock::new(SymlinkX {
                perms: Permissions {
                    mode: Mode::RWXU | Mode::RWXG | Mode::RWXO,
                    userinfo: self.current_user,
                },
                target,
                unique_id: self.fresh_id(),
            }))),
        );
        assert!(old.is_none());
        Ok(())
    }

    fn read_link(&self, path: impl crate::path::Arg) -> Result<String, ReadLinkError> {
        let path = self.absolute_path(path)?;
        let root = self.root.read();
        let (_, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        let Entry::Symlink(symlink) = entry else {
            return Err(ReadLinkError::NotASymlink);
        };
        Ok(symlink.read().target.clone())
    }

    fn mkdir(&self, path: impl crate::path::Arg, mode: super::Mode) -> Result<(), MkdirError> {
        let path = self.absolute_path(path)?;
        let mut root = self.root.write();
        let (parent, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some((_parent_path, parent)) = parent else {
            // Attempted to make `/`
            return Err(MkdirError::AlreadyExists);
        };
        let None = entry else {
            return Err(MkdirError::AlreadyExists);
        };
        let mut parent = parent.write();
        if !self.current_user.can_write(&parent.perms) {
            return Err(MkdirError::NoWritePerms);
        }
        let old = parent.children.insert(
            path.components().unwrap().last().unwrap().into(),
            FileType::Directory,
        );
        assert!(old.is_none());
        let old = root.entries.insert(
            path,
            Entry::Dir(Arc::new(sync::RwLock::new(DirX {
                perms: Permissions {
                    mode,
                    userinfo: self.current_user,
                },
                children: HashMap::default(),
                unique_id: self.fresh_id(),
            }))),
        );
        assert!(old.is_none());
        Ok(())
    }

    fn rmdir(&self, path: impl crate::path::Arg) -> Result<(), RmdirError> {
        let path = self.absolute_path(path)?;
        let mut root = self.root.write();
        let (parent, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some((_, parent)) = parent else {
            // Attempted to remove `/`
            return Err(RmdirError::Busy);
        };
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        let Entry::Dir(dir) = entry else {
            return Err(RmdirError::NotADirectory);
        };
        if !dir.read().children.is_empty() {
            return Err(RmdirError::NotEmpty);
        }
        let mut parent = parent.write();
        if !self.current_user.can_write(&parent.perms) {
            return Err(RmdirError::NoWritePerms);
        }
        let removed = parent
            .children
            .remove(path.components().unwrap().last().unwrap());
        // Just a sanity check
        assert!(matches!(removed, Some(FileType::Directory)));
        let removed = root.entries.remove(&path).unwrap();
        // Just a sanity check
        assert!(matches!(removed, Entry::Dir(_)));
        Ok(())
    }

    fn read_dir(&self, fd: &FileFd<Platform>) -> Result<Vec<DirEntry>, ReadDirError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::Dir { dir } = &descriptor_table
            .get_entry(fd)
            .ok_or(ReadDirError::ClosedFd)?
            .entry
        else {
            return Err(ReadDirError::NotADirectory);
        };

        // find the directory path in the root entries by pointer-equality of the Arc
        let mut parent_path = {
            let root = self.root.read();
            root.entries
                .iter()
                .find_map(|(path, entry)| match entry {
                    Entry::Dir(d) if alloc::sync::Arc::ptr_eq(d, dir) => Some(path.clone()),
                    _ => None,
                })
                .unwrap_or(String::new())
        };

        // helper to get NodeInfo by an entries-key (entries keys have no trailing '/')
        let get_node_info = |key: &str| -> Option<NodeInfo> {
            self.root.read().entries.get(key).map(|entry| {
                let ino = match entry {
                    Entry::File(file) => file.read().unique_id,
                    Entry::Dir(dir) => dir.read().unique_id,
                    Entry::Symlink(symlink) => symlink.read().unique_id,
                };
                NodeInfo {
                    dev: DEVICE_ID,
                    ino,
                    rdev: None,
                }
            })
        };

        let mut entries: Vec<DirEntry> = Vec::new();

        // Add "."
        entries.push(DirEntry {
            name: ".".into(),
            file_type: FileType::Directory,
            ino_info: Some(NodeInfo {
                dev: DEVICE_ID,
                ino: dir.read().unique_id,
                rdev: None,
            }),
        });

        // Add ".."
        entries.push(DirEntry {
            name: "..".into(),
            file_type: FileType::Directory,
            ino_info: get_node_info(&parent_path),
        });

        // Append a trailing '/' to `parent_path`.
        // An empty string (`""`) represents the root.
        parent_path.push('/');

        // Add normal children
        entries.extend(dir.read().children.iter().map(|(name, file_type)| {
            let mut full_path = parent_path.clone();
            full_path.push_str(name);
            DirEntry {
                name: name.into(),
                file_type: file_type.clone(),
                ino_info: get_node_info(&full_path),
            }
        }));
        Ok(entries)
    }

    fn file_status(&self, path: impl crate::path::Arg) -> Result<FileStatus, FileStatusError> {
        let path = self.absolute_path(path)?;
        let root = self.root.read();
        let (_, entry) = root.parent_and_entry(&path, self.current_user)?;
        let Some(entry) = entry else {
            return Err(PathError::NoSuchFileOrDirectory)?;
        };
        let (file_type, perms, size, unique_id) = match entry {
            Entry::File(file) => {
                let file = file.read();
                (
                    super::FileType::RegularFile,
                    file.perms.clone(),
                    file.data.len(),
                    file.unique_id,
                )
            }
            Entry::Dir(dir) => {
                let dir = dir.read();
                (
                    super::FileType::Directory,
                    dir.perms.clone(),
                    super::DEFAULT_DIRECTORY_SIZE,
                    dir.unique_id,
                )
            }
            Entry::Symlink(symlink) => {
                let symlink = symlink.read();
                (
                    super::FileType::Symlink,
                    symlink.perms.clone(),
                    symlink.target.len(),
                    symlink.unique_id,
                )
            }
        };
        Ok(FileStatus {
            file_type,
            mode: perms.mode,
            size,
            owner: perms.userinfo,
            node_info: NodeInfo {
                dev: DEVICE_ID,
                ino: unique_id,
                rdev: None,
            },
            blksize: BLOCK_SIZE,
        })
    }

    fn fd_file_status(&self, fd: &FileFd<Platform>) -> Result<FileStatus, FileStatusError> {
        let (file_type, perms, size, unique_id) = match &self
            .litebox
            .descriptor_table()
            .get_entry(fd)
            .ok_or(FileStatusError::ClosedFd)?
            .entry
        {
            Descriptor::File { file, .. } => {
                let file = file.read();
                (
                    super::FileType::RegularFile,
                    file.perms.clone(),
                    file.data.len(),
                    file.unique_id,
                )
            }
            Descriptor::Dir { dir, .. } => {
                let dir = dir.read();
                (
                    super::FileType::Directory,
                    dir.perms.clone(),
                    super::DEFAULT_DIRECTORY_SIZE,
                    dir.unique_id,
                )
            }
        };
        Ok(FileStatus {
            file_type,
            mode: perms.mode,
            size,
            owner: perms.userinfo,
            node_info: NodeInfo {
                dev: DEVICE_ID,
                ino: unique_id,
                rdev: None,
            },
            blksize: BLOCK_SIZE,
        })
    }

    fn get_static_backing_data(&self, fd: &FileFd<Platform>) -> Option<&'static [u8]> {
        let descriptor_table = self.litebox.descriptor_table();
        let entry = descriptor_table.get_entry(fd)?;
        match &entry.entry {
            Descriptor::File { file, .. } => {
                let file = file.read();
                match &file.data {
                    alloc::borrow::Cow::Borrowed(slice) => Some(*slice),
                    alloc::borrow::Cow::Owned(_) => None,
                }
            }
            Descriptor::Dir { .. } => None,
        }
    }
}

struct RootDir<Platform: sync::RawSyncPrimitivesProvider> {
    // keys are normalized paths; directories do not have the final `/` (thus the root would be at
    // the empty-string key "")
    entries: HashMap<String, Entry<Platform>>,
}

// Parent, if it exists, is the path as well as the directory
//
// The entry, if it exists, is just the entry itself
type ParentAndEntry<'a, D, E> = Result<(Option<(&'a str, D)>, Option<E>), PathError>;

impl<Platform: sync::RawSyncPrimitivesProvider> RootDir<Platform> {
    fn new() -> Self {
        Self {
            entries: [(
                String::new(),
                Entry::Dir(Arc::new(sync::RwLock::new(DirX {
                    perms: Permissions {
                        mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                        userinfo: UserInfo { user: 0, group: 0 },
                    },
                    children: HashMap::default(),
                    unique_id: 0,
                }))),
            )]
            .into_iter()
            .collect(),
        }
    }

    fn parent_and_entry(
        &self,
        path: &str,
        current_user: UserInfo,
    ) -> ParentAndEntry<'_, Dir<Platform>, Entry<Platform>> {
        let mut real_components_seen = false;
        let mut collected = String::new();
        let mut parent_dir = None;
        for p in path.normalized_components()? {
            if p.is_empty() || p == ".." {
                // After normalization, these can only be at the start of the path, so can all be
                // ignored. We do an `assert` here mostly as a sanity check.
                assert!(!real_components_seen);
                continue;
            }
            // We have seen real components, should no longer see any empty or `/`s.
            real_components_seen = true;
            match self
                .entries
                .get_key_value(&collected)
                .ok_or(PathError::MissingComponent)?
            {
                (_, Entry::File(_) | Entry::Symlink(_)) => {
                    return Err(PathError::ComponentNotADirectory);
                }
                (parent_path, Entry::Dir(dir)) => {
                    if !current_user.can_execute(&dir.read().perms) {
                        return Err(PathError::NoSearchPerms {
                            #[cfg(debug_assertions)]
                            dir: parent_path.clone(),
                            #[cfg(debug_assertions)]
                            perms: dir.read().perms.mode,
                        });
                    }
                    parent_dir = Some((parent_path.as_str(), dir.clone()));
                }
            }
            collected += "/";
            collected += p;
        }
        Ok((parent_dir, self.entries.get(&collected).cloned()))
    }
}

enum Entry<Platform: sync::RawSyncPrimitivesProvider> {
    File(File<Platform>),
    Dir(Dir<Platform>),
    Symlink(Symlink<Platform>),
}

impl<Platform: sync::RawSyncPrimitivesProvider> Entry<Platform> {
    fn perms(&self) -> Permissions {
        match self {
            Self::File(file) => file.read().perms.clone(),
            Self::Dir(dir) => dir.read().perms.clone(),
            Self::Symlink(symlink) => symlink.read().perms.clone(),
        }
    }
}

impl<Platform: sync::RawSyncPrimitivesProvider> Clone for Entry<Platform> {
    fn clone(&self) -> Self {
        match self {
            Self::File(file) => Self::File(file.clone()),
            Self::Dir(dir) => Self::Dir(dir.clone()),
            Self::Symlink(symlink) => Self::Symlink(symlink.clone()),
        }
    }
}

type Dir<Platform> = Arc<sync::RwLock<Platform, DirX>>;

pub(crate) struct DirX {
    perms: Permissions,
    children: HashMap<String, FileType>,
    unique_id: usize,
}

type File<Platform> = Arc<sync::RwLock<Platform, FileX>>;

pub(crate) struct FileX {
    perms: Permissions,
    data: alloc::borrow::Cow<'static, [u8]>,
    unique_id: usize,
}

type Symlink<Platform> = Arc<sync::RwLock<Platform, SymlinkX>>;

pub(crate) struct SymlinkX {
    perms: Permissions,
    target: String,
    unique_id: usize,
}

#[derive(Clone, Debug)]
struct Permissions {
    mode: Mode,
    userinfo: UserInfo,
}

impl UserInfo {
    fn can_read(self, perms: &Permissions) -> bool {
        perms.can_read_by(self)
    }
    fn can_write(self, perms: &Permissions) -> bool {
        perms.can_write_by(self)
    }
    fn can_execute(self, perms: &Permissions) -> bool {
        perms.can_execute_by(self)
    }
}

impl Permissions {
    fn can_read_by(&self, current: UserInfo) -> bool {
        if self.userinfo.user == current.user {
            self.mode.contains(Mode::RUSR)
        } else if self.userinfo.group == current.group {
            self.mode.contains(Mode::RGRP)
        } else {
            self.mode.contains(Mode::ROTH)
        }
    }
    fn can_write_by(&self, current: UserInfo) -> bool {
        if self.userinfo.user == current.user {
            self.mode.contains(Mode::WUSR)
        } else if self.userinfo.group == current.group {
            self.mode.contains(Mode::WGRP)
        } else {
            self.mode.contains(Mode::WOTH)
        }
    }
    fn can_execute_by(&self, current: UserInfo) -> bool {
        if self.userinfo.user == current.user {
            self.mode.contains(Mode::XUSR)
        } else if self.userinfo.group == current.group {
            self.mode.contains(Mode::XGRP)
        } else {
            self.mode.contains(Mode::XOTH)
        }
    }
}

pub(crate) enum Descriptor<Platform: sync::RawSyncPrimitivesProvider> {
    File {
        file: File<Platform>,
        read_allowed: bool,
        write_allowed: bool,
        position: usize,
        append_mode: bool,
    },
    Dir {
        dir: Dir<Platform>,
    },
}

crate::fd::enable_fds_for_subsystem! {
    @ Platform: { sync::RawSyncPrimitivesProvider };
    FileSystem<Platform>;
    @ Platform: { sync::RawSyncPrimitivesProvider };
    Descriptor<Platform>;
    -> FileFd<Platform>;
}
