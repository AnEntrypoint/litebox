// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! An layered file system, layering on [`FileSystem`](super::FileSystem) on top of another.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use hashbrown::{HashMap, HashSet};

use crate::LiteBox;
use crate::fd::{InternalFd, TypedFd};
use crate::path::Arg;
use crate::sync;

use super::errors::{
    ChmodError, ChownError, CloseError, FileStatusError, LinkError, MkdirError, OpenError,
    PathError, ReadDirError, ReadError, ReadLinkError, RenameError, RmdirError, SeekError,
    SetTimesError, SymlinkError, TruncateError, UnlinkError, WriteError,
};
use super::{DirEntry, FileStatus, FileType, Mode, NodeInfo, OFlags, SeekWhence};

/// Just a random constant that is distinct from other file systems. In this case, it is
/// `b'Lyrs'.hex()`.
const DEVICE_ID: usize = 0x4c797273;

/// Possible semantics for layering file systems together
#[non_exhaustive]
pub enum LayeringSemantics {
    /// Lower layer is read-only.
    ///
    /// Any writes to the lower layer have copy-on-write semantics, copying it over to the upper
    /// layer, before performing the write.
    LowerLayerReadOnly,
    /// Lower layer's files are writable.
    ///
    /// No new files can be made at the lower layer, but existing ones can still be written to. An
    /// upper layer file of the same name shadows the lower layer file entirely.
    LowerLayerWritableFiles,
}

/// A backing implementation of [`FileSystem`](super::FileSystem) that layers a file system on top
/// of another: it stores no files itself, resolving every operation in the upper layer first and
/// falling back to the lower layer. A writable open of a file present only in a read-only lower
/// layer has copy-on-write semantics.
pub struct FileSystem<
    Platform: sync::RawSyncPrimitivesProvider,
    Upper: super::FileSystem + 'static,
    Lower: super::FileSystem + 'static,
> {
    litebox: LiteBox<Platform>,
    upper: Upper,
    lower: Lower,
    // TODO: Possibly support a single-threaded variant that doesn't have the cost of requiring a
    // sync-primitives platform, as well as cost of mutexes and such?
    root: sync::RwLock<Platform, RootDir<Upper, Lower>>,
    layering_semantics: LayeringSemantics,
    // cwd invariant: always ends with a `/`
    current_working_dir: String,
    node_info_lookup: sync::RwLock<Platform, HashMap<NodeInfo, usize>>,
    // Serializes `migrate_file_up` end-to-end; never narrow it to the swap, which reopens the
    // `Arc::ptr_eq` race -- gm mutable `layered-migrate-lock-serializes-whole-migration`.
    migrate_lock: sync::Mutex<Platform, ()>,
}

impl<Platform: sync::RawSyncPrimitivesProvider, Upper: super::FileSystem, Lower: super::FileSystem>
    FileSystem<Platform, Upper, Lower>
{
    /// Construct a new `FileSystem` instance
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        upper: Upper,
        lower: Lower,
        layering_semantics: LayeringSemantics,
    ) -> Self {
        let root = sync::RwLock::new(RootDir::new());
        let node_info_lookup = sync::RwLock::new(HashMap::new());
        Self {
            litebox: litebox.clone(),
            upper,
            lower,
            root,
            current_working_dir: "/".into(),
            layering_semantics,
            node_info_lookup,
            migrate_lock: sync::Mutex::new(()),
        }
    }

    /// Access the upper (writable) layer directly, e.g. to export its contents for a snapshot
    /// independent of the read-only lower layer's contents.
    pub fn upper(&self) -> &Upper {
        &self.upper
    }


    /// (private-only) check if the lower level has the path; if there is an I/O or path failure,
    /// propagate the relevant error.
    fn ensure_lower_contains(&self, path: &str) -> Result<FileType, FileStatusError> {
        self.lower.file_status(path).map(|stat| stat.file_type)
    }

    /// (private-only) Create all parent/ancestor directories for `path`, making sure each exists in
    /// the lower layer. It does NOT set up `path` itself on the upper layer -- that is the caller's
    /// job -- and is NOT equivalent to `mkdir -p {path}` or `mkdir {path}`.
    fn mkdir_migrating_ancestor_dirs(&self, path: &str) -> Result<(), MkdirError> {
        let path = self.absolute_path(path)?;
        for dir in path.increasing_ancestors().map_err(PathError::from)? {
            if dir == path {
                return Ok(());
            }
            match self.ensure_lower_contains(dir) {
                Ok(FileType::Directory) => {
                    // The dir does in fact exist; we just need to confirm that the upper layer also
                    // has it.
                    match self
                        .upper
                        .mkdir(dir, self.lower.file_status(dir).unwrap().mode)
                    {
                        Ok(()) => {
                            // fallthrough to next increasing ancestor
                        }
                        Err(e) => match e {
                            MkdirError::AlreadyExists => {
                                // perfectly fine, just fallthrough to next place in the loop
                            }
                            MkdirError::ReadOnlyFileSystem
                            | MkdirError::Io
                            | MkdirError::NoWritePerms
                            | MkdirError::PathError(
                                PathError::ComponentNotADirectory
                                | PathError::InvalidPathname
                                | PathError::NoSearchPerms { .. }
                                | PathError::TooManySymlinkHops,
                            ) => {
                                return Err(e);
                            }
                            MkdirError::PathError(
                                PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                            ) => {
                                unreachable!()
                            }
                        },
                    }
                }
                Ok(FileType::RegularFile | FileType::CharacterDevice | FileType::Symlink | FileType::Fifo)
                | Err(
                    FileStatusError::PathError(PathError::MissingComponent)
                    | FileStatusError::ClosedFd,
                ) => unreachable!(),
                Err(FileStatusError::PathError(PathError::ComponentNotADirectory)) => {
                    unimplemented!()
                }
                Err(FileStatusError::PathError(PathError::InvalidPathname)) => {
                    unreachable!("we just confirmed valid path")
                }
                Err(FileStatusError::PathError(
                    e @ (PathError::NoSearchPerms { .. } | PathError::TooManySymlinkHops),
                )) => {
                    Err(e)?;
                }
                Err(FileStatusError::PathError(PathError::NoSuchFileOrDirectory)) => {
                    assert_ne!(dir, path);
                    Err(PathError::MissingComponent)?;
                }
                Err(FileStatusError::Io) => return Err(MkdirError::Io),
            }
        }
        // The loop above should return at one of its return points
        unreachable!()
    }

    /// (private-only) Migrate a file from lower to upper layer, erroring with the relevant
    /// `PathError` if the lower layer does not have it. Files only, never directories.
    ///
    /// `copy_data` copies the lower bytes up; `false` leaves the upper file empty, as if truncated.
    /// Generally you want `true`.
    fn migrate_file_up(&self, path: &str, copy_data: bool) -> Result<(), MigrationError> {
        // Held for the whole call, not just the swap -- gm mutable
        // `layered-migrate-lock-serializes-whole-migration`.
        let _migrate_guard = self.migrate_lock.lock();

        // Only a REGULAR file (or symlink) has byte contents whose copy is the same object:
        // migrating a character device fabricates an empty regular file shadowing it, which is how
        // `/dev/null` was destroyed -- gm mutable `layered-migrate-refuses-non-regular`.
        match self.ensure_lower_contains(path) {
            Ok(FileType::RegularFile | FileType::Symlink) => {}
            Ok(_) => return Err(MigrationError::NotAFile),
            Err(FileStatusError::Io) => return Err(MigrationError::Io),
            Err(FileStatusError::PathError(e)) => return Err(e)?,
            Err(FileStatusError::ClosedFd) => unreachable!(),
        }

        // Deliberately agnostic to `self.layering_semantics`: every caller decides for itself
        // whether reaching this function is right for its layer -- gm mutable
        // `layered-migrate-refuses-non-regular`.
        let lower_fd = match self.lower.open(path, OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(e) => match e {
                OpenError::AccessNotAllowed => return Err(MigrationError::NoReadPerms),
                OpenError::Io => return Err(MigrationError::Io),
                OpenError::NoWritePerms
                | OpenError::ReadOnlyFileSystem
                | OpenError::AlreadyExists
                | OpenError::TruncateError(_) => unreachable!(),
                OpenError::PathError(path_error) => return Err(path_error)?,
            },
        };
        // Read the lower file BEFORE opening the upper one, so a lower entry that is not really a
        // file errors out before the upper layer has been told anything.
        let mut upper_fd = None;
        let mut temp_buf = [0u8; 4096];
        loop {
            match self.lower.read(&lower_fd, &mut temp_buf, None) {
                Ok(size) => {
                    if upper_fd.is_none() {
                        match self.mkdir_migrating_ancestor_dirs(path) {
                            Ok(()) => {}
                            Err(MkdirError::ReadOnlyFileSystem) => {
                                // This upper layer structurally cannot hold `path`; let the caller
                                // fall back to another upper layer rather than treating it as
                                // fatal -- gm mutable
                                // `layered-parent-copyup-and-upper-cannot-hold`.
                                let _ = self.lower.close(&lower_fd);
                                return Err(MigrationError::UpperCannotHoldPath);
                            }
                            Err(e) => unimplemented!("{e} when setting up ancestor dirs"),
                        }
                        upper_fd = Some(
                            self.upper
                                .open(
                                    path,
                                    OFlags::CREAT | OFlags::WRONLY,
                                    self.lower.fd_file_status(&lower_fd).unwrap().mode,
                                )
                                .unwrap(),
                        );
                    }
                    let upper_fd = upper_fd.as_ref().unwrap();
                    if size > 0 && copy_data {
                        self.upper.write(upper_fd, &temp_buf[..size], None).expect(
                            "writing to upper layer must succeed, or layered file migration is in serious trouble",
                        );
                    } else {
                        break;
                    }
                }
                Err(e) => match e {
                    // `NotForReading` is real and reachable, not theoretical: a directory opens
                    // fine `RDONLY` but cannot be streamed via `read()`. Treat it exactly as
                    // `NotAFile` -- gm mutable `layered-read-notforreading-directory-weston`.
                    ReadError::NotAFile | ReadError::NotForReading => {
                        assert!(upper_fd.is_none());
                        return Err(MigrationError::NotAFile);
                    }
                    ReadError::ClosedFd => unreachable!(),
                    ReadError::Io => return Err(MigrationError::Io),
                },
            }
        }
        // Migrate the node-info over too, so a caller that stats the path either side of the
        // migration sees one unchanging inode.
        let found = self
            .node_info_lookup
            .read()
            .get(&self.lower.fd_file_status(&lower_fd).unwrap().node_info)
            .copied();
        if let Some(layered_id) = found {
            let old = self.node_info_lookup.write().insert(
                self.upper
                    .fd_file_status(upper_fd.as_ref().unwrap())
                    .unwrap()
                    .node_info,
                layered_id,
            );
            // Two threads can genuinely race to migrate the same path and both reach this insert;
            // `node_info_lookup` is a cache, not the source of truth for identity, so never panic
            // here -- gm mutable `layered-node-info-insert-race`.
            if let Some(old_id) = old {
                if old_id != layered_id {
                    litebox_util_log::warn!(
                        old_id:? = old_id, new_id:? = layered_id;
                        "migrate_file_up: node_info_lookup insert raced with a different \
                         layered_id for the same upper node-info key -- keeping the newer entry"
                    );
                }
            }
        }
        self.upper.close(&upper_fd.unwrap()).unwrap();
        self.lower.close(&lower_fd).unwrap();

        // Perf: a full scan over all open descriptors.
        //
        // This write lock is held across the ENTIRE migration loop below, never released after
        // collecting `to_migrate`: that is what excludes `open()`'s `EntryX::Lower` fast path from
        // invalidating the strong-count invariant the swap assumes -- gm mutable
        // `layered-migrate-concurrency-root-lock`.
        let mut root_guard = self.root.write();
        let RootDir {
            entries: root_entries,
        } = &mut *root_guard;
        let to_migrate: alloc::vec::Vec<(InternalFd, usize, OFlags, Entry<Upper, Lower>)> = self
            .litebox
            .descriptor_table()
            .iter::<Self>()
            .filter_map(|(internal_fd, e)| {
                if e.entry.path != path {
                    return None;
                }
                match &*e.entry.entry {
                    EntryX::Upper { fd: _ } => None,
                    EntryX::Lower { fd: _ } => {
                        Some((
                            internal_fd,
                            e.entry.position.load(SeqCst),
                            e.entry.flags,
                            Arc::clone(&e.entry.entry),
                        ))
                    }
                    EntryX::Tombstone => unreachable!(),
                }
            })
            .collect();
        for (internal_fd, position, flags, entry) in to_migrate {
            let upper_fd = self.upper.open(path, flags, Mode::empty()).unwrap();
            if position > 0 {
                self.upper
                    .seek(
                        &upper_fd,
                        isize::try_from(position).unwrap(),
                        SeekWhence::RelativeToBeginning,
                    )
                    .unwrap();
            }
            let upper_entry = Arc::new(EntryX::Upper { fd: upper_fd });
            match Arc::strong_count(&entry) {
                0..=2 => {
                    // Reachable, not unreachable: a concurrent `close()` on this exact
                    // `internal_fd` drops the descriptor-table-side reference after `to_migrate`
                    // was collected, leaving nothing here to migrate. `upper_entry` was never
                    // installed anywhere, so unwrap it back out to close `upper_fd` rather than
                    // leaking it -- gm mutable `layered-migrate-concurrency-root-lock`.
                    let EntryX::Upper { fd: upper_fd } = Arc::into_inner(upper_entry).unwrap()
                    else {
                        unreachable!()
                    };
                    self.upper.close(&upper_fd).ok();
                    continue;
                }
                3 => {
                    // Compare-and-skip rather than blindly swapping: a concurrent `close`/`dup`
                    // may have reused this `internal_fd`'s slot for an unrelated file since
                    // `to_migrate` was collected -- gm mutable
                    // `layered-migrate-concurrency-root-lock`.
                    let old_entry = self.litebox.descriptor_table().with_entry_mut_via_internal_fd::<Self, _, _>(
                        internal_fd,
                        |slot| {
                            if Arc::ptr_eq(&slot.entry.entry, &entry) {
                                Some(core::mem::replace(&mut slot.entry.entry, upper_entry))
                            } else {
                                None
                            }
                        },
                    );
                    let Some(Some(old_entry)) = old_entry else {
                        continue;
                    };
                    assert!(Arc::ptr_eq(&old_entry, &entry));
                    drop(entry);
                    let root_entry = root_entries.remove(path).unwrap();
                    assert!(Arc::ptr_eq(&old_entry, &root_entry));
                    drop(root_entry);
                    let entry = Arc::into_inner(old_entry).unwrap();
                    match entry {
                        EntryX::Upper { .. } | EntryX::Tombstone => unreachable!(),
                        EntryX::Lower { fd } => {
                            self.lower.close(&fd).unwrap();
                        }
                    }
                }
                _ => {
                    // Other fds still share this file, so a future fd does the closing. Same
                    // compare-and-skip for the same slot-reuse race -- gm mutable
                    // `layered-migrate-concurrency-root-lock`.
                    let old_entry = self.litebox.descriptor_table().with_entry_mut_via_internal_fd::<Self, _, _>(
                        internal_fd,
                        |slot| {
                            if Arc::ptr_eq(&slot.entry.entry, &entry) {
                                Some(core::mem::replace(&mut slot.entry.entry, upper_entry))
                            } else {
                                None
                            }
                        },
                    );
                    let Some(Some(old_entry)) = old_entry else {
                        continue;
                    };
                    assert!(Arc::ptr_eq(&old_entry, &entry));
                }
            }
        }

        // `path` is unconditionally `EntryX::Upper` by now, so any `Lower` entry still cached here
        // is stale by construction; leaving it made a just-migrated path keep serving
        // pre-migration lower-layer content to every future `open()`, across processes -- gm
        // mutable `layered-stale-lower-cache-after-migration`.
        if let Some(existing) = root_entries.get(path) {
            if matches!(**existing, EntryX::Lower { .. }) {
                root_entries.remove(path);
            }
        }

        Ok(())
    }

    /// (private-only) Make `path` exist in the UPPER layer, so a metadata-only change
    /// (`chmod`/`chown`/`set_times`) applied there is what the guest observes afterwards.
    ///
    /// A lower-only directory is recreated in the upper layer carrying the lower's own mode;
    /// anything that genuinely cannot be carried up reports [`MetadataMigrationError::ReadOnly`],
    /// an honest `EROFS`. See gm mutable `layered-metadata-copyup-touch-panic`.
    fn migrate_entry_up_for_metadata(&self, path: &str) -> Result<(), MetadataMigrationError> {
        let lower_type = match self.ensure_lower_contains(path) {
            Ok(file_type) => file_type,
            Err(FileStatusError::Io | FileStatusError::ClosedFd) => {
                return Err(MetadataMigrationError::Io);
            }
            Err(FileStatusError::PathError(e)) => return Err(MetadataMigrationError::Path(e)),
        };
        if let FileType::Directory = lower_type {
            let lower_status = match self.lower.file_status(path) {
                Ok(status) => status,
                Err(FileStatusError::Io | FileStatusError::ClosedFd) => {
                    return Err(MetadataMigrationError::Io);
                }
                Err(FileStatusError::PathError(e)) => return Err(MetadataMigrationError::Path(e)),
            };
            match self.mkdir_migrating_ancestor_dirs(path) {
                Ok(()) | Err(MkdirError::AlreadyExists) => {}
                Err(MkdirError::Io) => return Err(MetadataMigrationError::Io),
                Err(MkdirError::PathError(e)) => return Err(MetadataMigrationError::Path(e)),
                Err(MkdirError::NoWritePerms) => return Err(MetadataMigrationError::NotPermitted),
                Err(MkdirError::ReadOnlyFileSystem) => {
                    return Err(MetadataMigrationError::ReadOnly);
                }
            }
            match self.upper.mkdir(path, lower_status.mode) {
                Ok(()) | Err(MkdirError::AlreadyExists) => {}
                Err(MkdirError::Io) => return Err(MetadataMigrationError::Io),
                Err(MkdirError::PathError(e)) => return Err(MetadataMigrationError::Path(e)),
                Err(MkdirError::NoWritePerms) => return Err(MetadataMigrationError::NotPermitted),
                Err(MkdirError::ReadOnlyFileSystem) => {
                    return Err(MetadataMigrationError::ReadOnly);
                }
            }
            // Carry the node-info over so a caller that stats the path either side of the copy-up
            // sees one unchanging inode -- gm mutable `layered-node-info-insert-race`.
            let layered_id = self
                .node_info_lookup
                .read()
                .get(&lower_status.node_info)
                .copied();
            if let (Some(layered_id), Ok(upper_status)) = (layered_id, self.upper.file_status(path))
            {
                self.node_info_lookup
                    .write()
                    .insert(upper_status.node_info, layered_id);
            }
            return Ok(());
        }
        match self.migrate_file_up(path, true) {
            Ok(()) => Ok(()),
            Err(MigrationError::Io) => Err(MetadataMigrationError::Io),
            Err(MigrationError::PathError(e)) => Err(MetadataMigrationError::Path(e)),
            Err(MigrationError::NoReadPerms) => Err(MetadataMigrationError::NotPermitted),
            Err(MigrationError::NotAFile | MigrationError::UpperCannotHoldPath) => {
                Err(MetadataMigrationError::ReadOnly)
            }
        }
    }

    // Gives the absolute path for `path`, resolving `.`/`..` and any relative path against the
    // current working directory. Does NOT account for symlinks.
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

    // Converts a `NodeInfo` from any of the layers into a layered `NodeInfo`
    fn get_layered_nodeinfo(&self, node_info: NodeInfo) -> NodeInfo {
        let mut node_info_lookup = self.node_info_lookup.write();
        let rdev = node_info.rdev;
        // ino starts at 1 (zero represents deleted file)
        let new_id = node_info_lookup.len() + 1;
        let ino = *node_info_lookup.entry(node_info).or_insert(new_id);
        NodeInfo {
            dev: DEVICE_ID,
            ino,
            rdev,
        }
    }
}

/// Why [`FileSystem::migrate_entry_up_for_metadata`] could not make a path exist in the upper
/// layer: the four shapes [`ChmodError`], [`ChownError`] and [`SetTimesError`] all share, so each
/// caller converts it with no per-caller reasoning of its own.
enum MetadataMigrationError {
    Io,
    Path(PathError),
    NotPermitted,
    ReadOnly,
}

impl From<MetadataMigrationError> for ChmodError {
    fn from(error: MetadataMigrationError) -> Self {
        match error {
            MetadataMigrationError::Io => Self::Io,
            MetadataMigrationError::Path(e) => Self::PathError(e),
            MetadataMigrationError::NotPermitted => Self::NotTheOwner,
            MetadataMigrationError::ReadOnly => Self::ReadOnlyFileSystem,
        }
    }
}

impl From<MetadataMigrationError> for ChownError {
    fn from(error: MetadataMigrationError) -> Self {
        match error {
            MetadataMigrationError::Io => Self::Io,
            MetadataMigrationError::Path(e) => Self::PathError(e),
            MetadataMigrationError::NotPermitted => Self::NotTheOwner,
            MetadataMigrationError::ReadOnly => Self::ReadOnlyFileSystem,
        }
    }
}

impl From<MetadataMigrationError> for SetTimesError {
    fn from(error: MetadataMigrationError) -> Self {
        match error {
            MetadataMigrationError::Io => Self::Io,
            MetadataMigrationError::Path(e) => Self::PathError(e),
            MetadataMigrationError::NotPermitted => Self::NotPermitted,
            MetadataMigrationError::ReadOnly => Self::ReadOnlyFileSystem,
        }
    }
}

/// Possible errors when migrating a file up from lower to upper layer
#[derive(thiserror::Error, Debug)]
pub enum MigrationError {
    #[error("does not point to a file")]
    NotAFile,
    #[error("no read access permissions")]
    NoReadPerms,
    #[error("I/O error")]
    Io,
    #[error(transparent)]
    PathError(#[from] PathError),
    /// The upper layer cannot hold this path at all (e.g. a namespace like `/dev` that only backs
    /// a narrow subtree) -- a structural, always-reproducible mismatch, distinct from `Io`.
    /// Callers with another upper layer capable of holding the path should migrate there instead.
    #[error("upper layer cannot hold this path")]
    UpperCannotHoldPath,
}

impl<Platform: sync::RawSyncPrimitivesProvider, Upper: super::FileSystem, Lower: super::FileSystem>
    super::private::Sealed for FileSystem<Platform, Upper, Lower>
{
}

impl<
    Platform: sync::RawSyncPrimitivesProvider,
    Upper: super::FileSystem + 'static,
    Lower: super::FileSystem + 'static,
> super::FileSystem for FileSystem<Platform, Upper, Lower>
{
    fn open(
        &self,
        path: impl crate::path::Arg,
        flags: OFlags,
        mode: Mode,
    ) -> Result<FileFd<Platform, Upper, Lower>, OpenError> {
        let currently_supported_oflags: OFlags = OFlags::CREAT
            | OFlags::RDONLY
            | OFlags::WRONLY
            | OFlags::RDWR
            | OFlags::EXCL
            | OFlags::TRUNC
            | OFlags::NOCTTY
            | OFlags::DIRECTORY
            | OFlags::NONBLOCK
            | OFlags::LARGEFILE
            | OFlags::NOFOLLOW
            | OFlags::APPEND
            // Hints that say HOW to do the I/O, not WHAT to open: none changes what `open` returns
            // or what the caller may then do, so each is accepted and ignored -- gm mutable
            // `layered-open-unlisted-oflag-einval-not-panic`.
            | OFlags::NOATIME
            | OFlags::DSYNC
            | OFlags::SYNC
            | OFlags::DIRECT
            | OFlags::ASYNC
            | OFlags::CLOEXEC
            | OFlags::PATH;
        // An unlisted flag is REPORTED, never fatal: this was `unimplemented!()`, which panics the
        // HOST and killed a live XFCE session with `not implemented: OFlags(NOATIME)`. `EINVAL`
        // leaves the decision with the caller -- gm mutable
        // `layered-open-unlisted-oflag-einval-not-panic`.
        if flags.intersects(currently_supported_oflags.complement()) {
            litebox_util_log::warn!(flags:? = flags; "open: unsupported open flag(s)");
            return Err(OpenError::PathError(PathError::InvalidPathname));
        }
        let path = self.absolute_path(path)?;
        if flags.contains(OFlags::CREAT) {
            if flags.contains(OFlags::EXCL) {
                // O_EXCL with O_CREAT: fail if file already exists anywhere (upper or lower layer)
                if self.file_status(path.as_str()).is_ok() {
                    return Err(OpenError::AlreadyExists);
                }
            } else {
                // We must first attempt to open the file _without_ creating it, and only if that fails,
                // do we fall-through and end up creating it (which will happen on the upper layer).
                if let Ok(fd) = self.open(path.as_str(), flags - OFlags::CREAT, mode) {
                    return Ok(fd);
                }
            }
        }
        let mut tombstone_removal = false;
        if let Some(entry) = self.root.read().entries.get(&path) {
            match entry.as_ref() {
                EntryX::Tombstone => {
                    if flags.contains(OFlags::CREAT) {
                        tombstone_removal = true;
                    } else {
                        Err(PathError::NoSuchFileOrDirectory)?;
                    }
                }
                EntryX::Upper { .. } => unreachable!(),
                EntryX::Lower { .. } => {
                    // A cached lower entry is always opened with the same flags, and its presence
                    // means there is no such file at the upper level, so it can be returned
                    // directly with the caller's "real" flags wrapped in the layered descriptor.
                    return Ok(self.litebox.descriptor_table_mut().insert(Descriptor {
                        path,
                        flags,
                        entry: Arc::clone(entry),
                        position: 0.into(),
                    }));
                }
            }
        }
        if tombstone_removal {
            if let Some(entry) = self.root.write().entries.remove(&path) {
                let EntryX::Tombstone = *entry else {
                    unreachable!()
                };
            } else {
                // A racing thread creating the same file over the same tombstone already removed
                // it; proceed as normal.
            }
        }
        // Otherwise, we first check the upper level, creating an entry if needed
        match self.upper.open(&*path, flags, mode) {
            Ok(fd) => {
                let entry = Arc::new(EntryX::Upper { fd });
                return Ok(self.litebox.descriptor_table_mut().insert(Descriptor {
                    path,
                    flags,
                    entry,
                    position: 0.into(),
                }));
            }
            Err(e) => match &e {
                OpenError::AccessNotAllowed
                | OpenError::Io
                | OpenError::NoWritePerms
                | OpenError::ReadOnlyFileSystem
                | OpenError::AlreadyExists
                | OpenError::TruncateError(
                    TruncateError::IsDirectory
                    | TruncateError::NotForWriting
                    | TruncateError::IsTerminalDevice
                    | TruncateError::ClosedFd
                    | TruncateError::PathOnlyFd
                    | TruncateError::Io,
                )
                | OpenError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    // None of these can be handled by lower level, just quit out early
                    return Err(e);
                }
                OpenError::PathError(PathError::MissingComponent)
                    if flags.contains(OFlags::CREAT) =>
                {
                    // A top-level path such as `/.memfd:17` splits into an EMPTY `dirname`, which
                    // no path-taking call accepts; it must be normalized to root `/` or the new
                    // file is misclassified as `EntryX::Lower` -- gm mutable
                    // `layered-open-creat-empty-dirname-root`.
                    let dirname = match path.rsplit_once('/').unwrap().0 {
                        "" => "/",
                        d => d,
                    };
                    if let Ok(FileType::Directory) = self.ensure_lower_contains(dirname) {
                        // We must migrate the directories above, and then re-trigger the open
                        self.mkdir_migrating_ancestor_dirs(&path).unwrap();
                        return self.open(path, flags, mode);
                    }
                    // Otherwise, handle-able by a lower level, fallthrough
                }
                OpenError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {
                    // Handle-able by a lower level, fallthrough
                }
            },
        }
        // An upper-layer symlink must be resolved and retried against the FULL layered view, never
        // re-queried on the lower layer under the symlink's own name, which only exists on the
        // upper layer. `O_NOFOLLOW` skips the retry entirely -- gm mutable
        // `layered-open-upper-symlink-resolve-retry`.
        if !flags.contains(OFlags::NOFOLLOW)
            && let Ok(target) = self.upper.read_link(path.as_str())
        {
            let resolved = if target.starts_with('/') {
                target
            } else {
                let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
                alloc::format!("{dir}/{target}")
            };
            if let Ok(resolved) = resolved.normalized() {
                return self.open(resolved, flags, mode);
            }
        }
        // We must check the lower level, creating an entry if needed
        let original_flags = flags;
        let mut flags = flags;
        // Prevent creation or truncation of files at lower level
        flags.remove(OFlags::CREAT);
        flags.remove(OFlags::TRUNC);
        match self.layering_semantics {
            LayeringSemantics::LowerLayerReadOnly => {
                // Switch the lower level to read-only; the other calls will take care of
                // copying into the upper level if/when necessary.
                flags.remove(OFlags::RDWR);
                flags.remove(OFlags::WRONLY);
                flags.insert(OFlags::RDONLY);
            }
            LayeringSemantics::LowerLayerWritableFiles => {
                // Do nothing more to the flags, because we might be writing things to lower level.
                // We just make sure that there is no creation happening, that's all :)
                assert!(!flags.contains(OFlags::CREAT));
                assert!(!flags.contains(OFlags::TRUNC));
            }
        }
        // `self.lower.open` runs without `self.root`'s write lock, so two threads opening the same
        // not-yet-cached path both arrive here with their own fd; the loser must discard its own
        // and reuse the winner's entry, never overwrite it -- gm mutable
        // `layered-open-lower-race-loser-closes-fd`.
        let our_entry = Arc::new(EntryX::Lower {
            fd: self.lower.open(path.as_str(), flags, mode)?,
        });
        let entry = {
            let mut root = self.root.write();
            if let Some(existing) = root.entries.get(&path) {
                let existing = Arc::clone(existing);
                // Safe to drop `root`'s lock before closing: `our_entry` was never published, so no
                // other thread can observe or hold a reference to its fd.
                drop(root);
                let EntryX::Lower { fd } = Arc::into_inner(our_entry)
                    .expect("our_entry was never shared, so this must be its sole owner")
                else {
                    unreachable!("our_entry was constructed as EntryX::Lower above")
                };
                self.lower.close(&fd).unwrap();
                existing
            } else {
                root.entries.insert(path.clone(), Arc::clone(&our_entry));
                our_entry
            }
        };
        // `O_TRUNC` applies to REGULAR FILES ONLY, as Linux's `do_open()` gates `handle_truncate()`
        // on `S_ISREG`; truncating a device here is what destroyed `/dev/null` for GNU `ld` -- gm
        // mutable `layered-otrunc-regular-only-devnull-chain`.
        //
        // `migrate_file_up` INDEPENDENTLY refuses non-regular entries, so the invariant holds at
        // both ends and does not rest on this check alone -- gm mutable
        // `layered-migrate-refuses-non-regular`.
        let truncate_applies = original_flags.contains(OFlags::TRUNC)
            && matches!(
                self.ensure_lower_contains(&path),
                Ok(FileType::RegularFile)
            );
        let fd = self.litebox.descriptor_table_mut().insert(Descriptor {
            path,
            flags: original_flags,
            entry,
            position: 0.into(),
        });
        if truncate_applies {
            match self.truncate(&fd, 0, true) {
                Ok(()) | Err(TruncateError::IsTerminalDevice) => {}
                Err(e) => {
                    self.close(&fd).unwrap();
                    return Err(e.into());
                }
            }
        }
        Ok(fd)
    }

    fn close(&self, fd: &FileFd<Platform, Upper, Lower>) -> Result<(), CloseError> {
        let Some(removed_entry) = self.litebox.descriptor_table_mut().remove(fd) else {
            return Ok(());
        };
        let Descriptor {
            path,
            entry,
            flags: _,
            position: _,
        } = removed_entry.entry;
        match entry.as_ref() {
            EntryX::Upper { .. } | EntryX::Lower { .. } => {}
            EntryX::Tombstone => unreachable!(),
        }
        // The exclusive root lock is what keeps the `Arc` counts below from changing while they are
        // being reasoned about.
        let RootDir {
            entries: root_entries,
        } = &mut *self.root.write();
        match *entry {
            EntryX::Tombstone => unreachable!(),
            EntryX::Upper { .. } => {
                // Upper-level FDs do not have any entry in the root, nor do they share anything via
                // `Arc`s. Thus, we can deal with them individually.
                assert_eq!(Arc::strong_count(&entry), 1);
                // Specifically, we can just immediately close them out, consuming the entry itself.
                let EntryX::Upper { fd } = Arc::into_inner(entry).unwrap() else {
                    unreachable!()
                };
                self.upper.close(&fd)
            }
            EntryX::Lower { .. } => {
                if Arc::strong_count(&entry) > 2 {
                    // Other fds definitely still point at this file; leave it alone.
                    return Ok(());
                }
                // Either only this fd and the root point at it, or the root was tombstoned out after
                // the fds were opened.
                match **root_entries.get(&path).unwrap() {
                    EntryX::Upper { .. } => unreachable!(),
                    EntryX::Lower { .. } => {
                        // We are going to have to deal with it at the entry too, fallthrough
                    }
                    EntryX::Tombstone => {
                        // Other fds may have been opened before the tombstone, so close the
                        // underlying fd only when this is the sole remaining holder.
                        match Arc::into_inner(entry) {
                            Some(EntryX::Upper { .. } | EntryX::Tombstone) => unreachable!(),
                            Some(EntryX::Lower { fd }) => {
                                // We are the sole remaining holder of the FD. Let us clean things
                                // up at the lower level.
                                return self.lower.close(&fd);
                            }
                            None => {
                                // Someone else's job. We can quit successfully.
                                return Ok(());
                            }
                        }
                    }
                }
                // Pull out the root entry, and perform a quick sanity check, and drop it out
                // entirely, which should lead us to become the sole owner.
                let root_entry = root_entries.remove(&path).unwrap();
                assert!(Arc::ptr_eq(&entry, &root_entry));
                assert!(matches!(*root_entry, EntryX::Lower { .. }));
                drop(root_entry);
                // We are now assured that we can close out the underlying file; we are the only
                // holder of the entry, and thus can change it from an Arc to the underlying value
                // itself, and then close it out.
                let EntryX::Lower { fd, .. } = Arc::into_inner(entry).unwrap() else {
                    unreachable!()
                };
                self.lower.close(&fd)
            }
        }
    }

    fn read(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
        buf: &mut [u8],
        offset: Option<usize>,
    ) -> Result<usize, ReadError> {
        // A write to a lower-level file upgrades its entry wholesale to an upper-level one, so there
        // is no desync to guard against here -- plain delegation on the entry's layer suffices.
        let (entry, this_position) = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| {
                let access_mode = descriptor.entry.flags & (OFlags::WRONLY | OFlags::RDWR);
                if access_mode == OFlags::WRONLY {
                    Err(ReadError::NotForReading)
                } else {
                    Ok((
                        Arc::clone(&descriptor.entry.entry),
                        descriptor.entry.position.load(SeqCst),
                    ))
                }
            })
            .ok_or(ReadError::ClosedFd)
            .flatten()?;
        // A `Lower` fd is cached and SHARED across every `open()` of the same path, so `None` must
        // resolve to this descriptor's own tracked position, not the backend's shared cursor, or
        // two opens steal each other's bytes -- gm mutable
        // `layered-lower-fd-shared-cursor-read-and-seek`.
        let resolved_offset = match entry.as_ref() {
            EntryX::Upper { .. } => offset,
            EntryX::Lower { .. } => Some(offset.unwrap_or(this_position)),
            EntryX::Tombstone => unreachable!(),
        };
        let num_bytes = match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.read(fd, buf, resolved_offset)?,
            EntryX::Lower { fd } => self.lower.read(fd, buf, resolved_offset)?,
            EntryX::Tombstone => unreachable!(),
        };
        if offset.is_none() {
            self.litebox
                .descriptor_table()
                .get_entry(fd)
                .ok_or(ReadError::ClosedFd)?
                .entry
                .position
                .fetch_add(num_bytes, SeqCst);
        }
        Ok(num_bytes)
    }

    fn write(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
        buf: &[u8],
        offset: Option<usize>,
    ) -> Result<usize, WriteError> {
        // An upper-level file is written directly; a lower-level file must first become an
        // upper-level file.
        let (entry, path) = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| {
                if !descriptor.entry.flags.contains(OFlags::WRONLY)
                    && !descriptor.entry.flags.contains(OFlags::RDWR)
                {
                    Err(WriteError::NotForWriting)
                } else {
                    Ok((
                        Arc::clone(&descriptor.entry.entry),
                        descriptor.entry.path.clone(),
                    ))
                }
            })
            .ok_or(WriteError::ClosedFd)
            .flatten()?;
        match entry.as_ref() {
            EntryX::Upper { fd: upper_fd } => {
                let num_bytes = self.upper.write(upper_fd, buf, offset)?;
                self.litebox
                    .descriptor_table()
                    .get_entry(fd)
                    .unwrap()
                    .entry
                    .position
                    .fetch_add(num_bytes, SeqCst);
                return Ok(num_bytes);
            }
            EntryX::Lower { fd: lower_fd } => {
                match self.layering_semantics {
                    LayeringSemantics::LowerLayerReadOnly => {
                        // fallthrough
                    }
                    LayeringSemantics::LowerLayerWritableFiles => {
                        // Direct write to the lower layer, unless the lower layer cannot hold this
                        // path in its own upper, in which case fall through and migrate into *this*
                        // fs's upper -- gm mutable `layered-parent-copyup-and-upper-cannot-hold`.
                        match self.lower.write(lower_fd, buf, offset) {
                            Ok(num_bytes) => {
                                if let Some(e) = self.litebox.descriptor_table().get_entry(fd) {
                                    e.entry.position.fetch_add(num_bytes, SeqCst);
                                }
                                return Ok(num_bytes);
                            }
                            Err(WriteError::NotForWriting) => {
                                // fallthrough to migrate into this fs's own upper
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            EntryX::Tombstone => unreachable!(),
        }
        drop(entry);
        match self.migrate_file_up(&path, true) {
            Ok(()) => {}
            Err(MigrationError::NoReadPerms) => unimplemented!(),
            Err(MigrationError::NotAFile) => return Err(WriteError::NotAFile),
            Err(MigrationError::Io) => return Err(WriteError::Io),
            Err(MigrationError::PathError(_e)) => unreachable!(),
            // `NotForWriting` is imprecise but is exactly the signal an outer fs composing this one
            // as its `lower` matches on to migrate through its own upper instead -- gm mutable
            // `layered-parent-copyup-and-upper-cannot-hold`.
            Err(MigrationError::UpperCannotHoldPath) => return Err(WriteError::NotForWriting),
        }
        debug_assert!(matches!(
            *self
                .litebox
                .descriptor_table()
                .get_entry(fd)
                .unwrap()
                .entry
                .entry,
            EntryX::Upper { .. }
        ));
        self.write(fd, buf, offset)
    }

    fn seek(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
        offset: isize,
        whence: SeekWhence,
    ) -> Result<usize, SeekError> {
        let (entry, this_position) = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| {
                (
                    Arc::clone(&descriptor.entry.entry),
                    descriptor.entry.position.load(SeqCst),
                )
            })
            .ok_or(SeekError::ClosedFd)?;
        let position = match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.seek(fd, offset, whence)?,
            // A `Lower` fd is cached and SHARED, so `SEEK_CUR` must be resolved against this
            // descriptor's own position and delegated as an absolute seek -- delegating it
            // unchanged looped s6-overlay's `preinit` for ever. `SEEK_SET`/`SEEK_END` never consult
            // the shared cursor -- gm mutable `layered-lower-fd-shared-cursor-read-and-seek`.
            EntryX::Lower { fd } => match whence {
                SeekWhence::RelativeToCurrentOffset => {
                    let absolute = this_position
                        .checked_add_signed(offset)
                        .ok_or(SeekError::InvalidOffset)?;
                    let absolute =
                        isize::try_from(absolute).map_err(|_| SeekError::InvalidOffset)?;
                    self.lower
                        .seek(fd, absolute, SeekWhence::RelativeToBeginning)?
                }
                SeekWhence::RelativeToBeginning | SeekWhence::RelativeToEnd => {
                    self.lower.seek(fd, offset, whence)?
                }
            },
            EntryX::Tombstone => unreachable!(),
        };
        if let Some(e) = self.litebox.descriptor_table().get_entry(fd) {
            e.entry.position.store(position, SeqCst);
        }
        Ok(position)
    }

    fn truncate(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
        length: usize,
        reset_offset: bool,
    ) -> Result<(), TruncateError> {
        let (flags, entry) = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| {
                (descriptor.entry.flags, Arc::clone(&descriptor.entry.entry))
            })
            .ok_or(TruncateError::ClosedFd)?;
        let layered_fd = fd;
        match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.truncate(fd, length, reset_offset),
            EntryX::Lower { fd } => {
                match self.layering_semantics {
                    LayeringSemantics::LowerLayerWritableFiles => {
                        match self.lower.truncate(fd, length, reset_offset) {
                            Err(TruncateError::NotForWriting) => {
                                // The lower fs's own upper cannot hold this path; migrate into
                                // *this* fs's upper instead -- gm mutable
                                // `layered-parent-copyup-and-upper-cannot-hold`.
                                drop(entry);
                                let path = self
                                    .litebox
                                    .descriptor_table()
                                    .with_entry(layered_fd, |descriptor| {
                                        descriptor.entry.path.clone()
                                    })
                                    .ok_or(TruncateError::ClosedFd)?;
                                self.migrate_file_up(&path, false)
                                    .map_err(|e| unreachable!("unexpected migration failure: {e}"))
                            }
                            other => other,
                        }
                    }
                    LayeringSemantics::LowerLayerReadOnly => {
                        if flags.contains(OFlags::WRONLY) || flags.contains(OFlags::RDWR) {
                            // We might need to migrate the file up
                            match self.lower.truncate(fd, length, reset_offset) {
                                Ok(()) | Err(TruncateError::ClosedFd) => unreachable!(),
                                Err(TruncateError::IsDirectory) => Err(TruncateError::IsDirectory),
                                Err(TruncateError::IsTerminalDevice) => {
                                    Err(TruncateError::IsTerminalDevice)
                                }
                                Err(TruncateError::PathOnlyFd) => Err(TruncateError::PathOnlyFd),
                                Err(TruncateError::NotForWriting) => {
                                    // The cloned entry must be dropped first, so the refcounting
                                    // `migrate_file_up` reasons about works out.
                                    drop(entry);
                                    let path = self
                                        .litebox
                                        .descriptor_table()
                                        .with_entry(layered_fd, |descriptor| {
                                            descriptor.entry.path.clone()
                                        })
                                        .ok_or(TruncateError::ClosedFd)?;
                                    match self.migrate_file_up(&path, false) {
                                        Ok(()) => Ok(()),
                                        // `NotForWriting` is the signal an outer fs composing this
                                        // one as its `lower` matches on -- gm mutable
                                        // `layered-parent-copyup-and-upper-cannot-hold`.
                                        Err(MigrationError::UpperCannotHoldPath) => {
                                            Err(TruncateError::NotForWriting)
                                        }
                                        Err(e) => {
                                            unreachable!("unexpected migration failure: {e}")
                                        }
                                    }
                                }
                                Err(TruncateError::Io) => Err(TruncateError::Io),
                            }
                        } else {
                            // The lower level truncate will correctly identify dir/file and handle
                            // the difference in erroring.
                            self.lower.truncate(fd, length, reset_offset)
                        }
                    }
                }
            }
            EntryX::Tombstone => unreachable!(),
        }
    }

    fn chmod_fd(&self, fd: &FileFd<Platform, Upper, Lower>, mode: Mode) -> Result<(), ChmodError> {
        // Must operate on the already-open handle, never re-resolve `fd` to a path: the point is to
        // keep working after the caller has `unlink`ed it -- gm mutable
        // `layered-chmod-fd-lower-hard-error`.
        let entry = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| Arc::clone(&descriptor.entry.entry))
            .ok_or(ChmodError::Io)?;
        match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.chmod_fd(fd, mode),
            EntryX::Lower { fd } => {
                // A still-open fd resolving to `Lower` can only be read-only, and there is no
                // upper-migration path for an already-open, no-longer-path-addressable fd; no real
                // caller (wlroots' shm dance) exercises it, so this stays a hard error rather than
                // growing unverified migration logic -- gm mutable
                // `layered-chmod-fd-lower-hard-error`.
                let _ = fd;
                Err(ChmodError::Io)
            }
            EntryX::Tombstone => unreachable!(),
        }
    }

    fn open_flags(&self, fd: &FileFd<Platform, Upper, Lower>) -> Option<OFlags> {
        // `Descriptor::flags` is the real per-fd open-time access mode and the only correct source
        // for `fcntl(F_GETFL)`; never fall back to `StdioStatusFlags` metadata, which exists only on
        // re-opened `/dev/std*` fds and made `xkbcomp`'s `fdopen(fd, "w")` fail on every ordinary
        // file -- gm mutable `layered-open-flags-xkbcomp-fdopen`.
        self.litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| descriptor.entry.flags & OFlags::STATUS_FLAGS_MASK)
    }

    fn chmod(&self, path: impl crate::path::Arg, mode: Mode) -> Result<(), ChmodError> {
        let path = self.absolute_path(path)?;
        let mut migrated_up = false;
        loop {
            match self.upper.chmod(path.as_str(), mode) {
                Ok(()) => return Ok(()),
                Err(e) => match e {
                    ChmodError::NotTheOwner
                    | ChmodError::Io
                    | ChmodError::ReadOnlyFileSystem
                    | ChmodError::PathError(
                        PathError::ComponentNotADirectory
                        | PathError::InvalidPathname
                        | PathError::NoSearchPerms { .. }
                        | PathError::TooManySymlinkHops,
                    ) => {
                        return Err(e);
                    }
                    ChmodError::PathError(
                        PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                    ) => {
                        if migrated_up {
                            return Err(e);
                        }
                    }
                },
            }
            self.migrate_entry_up_for_metadata(&path)
                .map_err(ChmodError::from)?;
            migrated_up = true;
        }
    }

    fn chown(
        &self,
        path: impl crate::path::Arg,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError> {
        let path = self.absolute_path(path)?;
        let mut migrated_up = false;
        loop {
            match self.upper.chown(path.as_str(), user, group) {
                Ok(()) => return Ok(()),
                Err(e) => match e {
                    ChownError::NotTheOwner
                    | ChownError::Io
                    | ChownError::ReadOnlyFileSystem
                    | ChownError::PathError(
                        PathError::ComponentNotADirectory
                        | PathError::InvalidPathname
                        | PathError::NoSearchPerms { .. }
                        | PathError::TooManySymlinkHops,
                    ) => {
                        return Err(e);
                    }
                    ChownError::PathError(
                        PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                    ) => {
                        if migrated_up {
                            return Err(e);
                        }
                    }
                },
            }
            self.migrate_entry_up_for_metadata(&path)
                .map_err(ChownError::from)?;
            migrated_up = true;
        }
    }

    fn set_times(
        &self,
        path: impl crate::path::Arg,
        atime: Option<super::Timestamp>,
        mtime: Option<super::Timestamp>,
    ) -> Result<(), SetTimesError> {
        let path = self.absolute_path(path)?;
        let mut migrated_up = false;
        loop {
            match self.upper.set_times(path.as_str(), atime, mtime) {
                Ok(()) => return Ok(()),
                Err(e) => match e {
                    SetTimesError::NotPermitted
                    | SetTimesError::Io
                    | SetTimesError::ReadOnlyFileSystem
                    | SetTimesError::PathError(
                        PathError::ComponentNotADirectory
                        | PathError::InvalidPathname
                        | PathError::NoSearchPerms { .. }
                        | PathError::TooManySymlinkHops,
                    ) => {
                        return Err(e);
                    }
                    SetTimesError::PathError(
                        PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                    ) => {
                        if migrated_up {
                            return Err(e);
                        }
                    }
                },
            }
            self.migrate_entry_up_for_metadata(&path)
                .map_err(SetTimesError::from)?;
            migrated_up = true;
        }
    }

    fn unlink(&self, path: impl crate::path::Arg) -> Result<(), UnlinkError> {
        let path = self.absolute_path(path)?;
        match self.upper.unlink(path.as_str()) {
            Ok(()) => {
                // If the lower level contains the file, then we need to place a tombstone in its
                // path, to prevent the lower level from showing up above.
                if self.ensure_lower_contains(&path).is_ok() {
                    // fallthrough to place the tombstone
                } else {
                    // Lower level doesn't contain it, we are done (with success, since we actually
                    // removed the file).
                    return Ok(());
                }
            }
            Err(e) => match e {
                UnlinkError::NoWritePerms
                | UnlinkError::Io
                | UnlinkError::IsADirectory
                | UnlinkError::ReadOnlyFileSystem
                | UnlinkError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    return Err(e);
                }
                UnlinkError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {
                    // We must now check if the lower level contains the file; if it does not, we
                    // must exit with failure. Otherwise, we fallthrough to place the tombstone.
                    match self.ensure_lower_contains(&path).map_err(|e| match e {
                        FileStatusError::Io => UnlinkError::Io,
                        FileStatusError::PathError(p) => UnlinkError::PathError(p),
                        FileStatusError::ClosedFd => unreachable!(),
                    })? {
                        FileType::RegularFile | FileType::Symlink | FileType::Fifo => {
                            // fallthrough
                        }
                        FileType::Directory => {
                            return Err(UnlinkError::IsADirectory);
                        }
                        // A device node is unlinkable exactly like a regular file: `rm /dev/null`
                        // succeeds on Linux, and the tombstone reproduces that without touching the
                        // synthetic `/dev` mount -- gm mutable `layered-unlink-chardev-tombstone`.
                        FileType::CharacterDevice => {
                            // fallthrough
                        }
                    }
                }
            },
        }
        // We can now place a tombstone over the lower level file, marking it as deleted, without
        // actually changing the lower level.
        self.root
            .write()
            .entries
            .insert(path, Arc::new(EntryX::Tombstone));
        Ok(())
    }

    fn rename(
        &self,
        from: impl crate::path::Arg,
        to: impl crate::path::Arg,
    ) -> Result<(), RenameError> {
        let from = self.absolute_path(from)?;
        let to = self.absolute_path(to)?;
        // `from` must already live purely in the upper layer; a lower-only source is real Linux
        // `EXDEV` territory. `to` IS allowed to shadow a lower-layer entry (the `apk`
        // replace-its-own-database case) -- gm mutable
        // `layered-rename-link-exdev-and-invalidation`.
        if self.ensure_lower_contains(&from).is_ok() {
            return Err(RenameError::CrossDevice);
        }
        // `to`'s parent directory may so far exist only in the read-only lower layer: on a
        // missing-component error, migrate the ancestor chain up and retry -- gm mutable
        // `layered-parent-copyup-and-upper-cannot-hold`.
        match self.upper.rename(&from, &to) {
            Ok(()) => {}
            Err(RenameError::PathError(PathError::MissingComponent)) => {
                let dirname = to.rsplit_once('/').unwrap().0;
                if let Ok(FileType::Directory) = self.ensure_lower_contains(dirname) {
                    self.mkdir_migrating_ancestor_dirs(&to)
                        .map_err(|e| match e {
                            MkdirError::NoWritePerms => RenameError::NoWritePerms,
                            MkdirError::ReadOnlyFileSystem => RenameError::ReadOnlyFileSystem,
                            MkdirError::Io => RenameError::Io,
                            MkdirError::AlreadyExists => unreachable!(),
                            MkdirError::PathError(e) => RenameError::PathError(e),
                        })?;
                    self.upper.rename(&from, &to)?;
                } else {
                    return Err(RenameError::PathError(PathError::MissingComponent));
                }
            }
            Err(e) => return Err(e),
        }
        // Invalidate `open`'s cache for `to` by PLAIN REMOVAL, never a tombstone: a tombstone means
        // "deleted" and makes `open` answer `ENOENT` without ever checking `self.upper`, which is
        // how `sed -i` broke every later open of the file it had just written -- gm mutable
        // `layered-rename-link-exdev-and-invalidation`.
        self.root.write().entries.remove(&to);
        Ok(())
    }

    fn link(
        &self,
        oldpath: impl crate::path::Arg,
        newpath: impl crate::path::Arg,
    ) -> Result<(), LinkError> {
        let oldpath = self.absolute_path(oldpath)?;
        let newpath = self.absolute_path(newpath)?;
        // `oldpath` must already live purely in the upper layer (Xorg-style lock-file acquisition
        // links a temp file it just wrote); a lower-only source is real Linux `EXDEV` -- gm mutable
        // `layered-rename-link-exdev-and-invalidation`.
        if self.ensure_lower_contains(&oldpath).is_ok() {
            return Err(LinkError::CrossDevice);
        }
        // Anything already at `newpath` in EITHER layer is `link(2)`'s `EEXIST`, since creation only
        // ever targets the upper layer -- gm mutable
        // `layered-parent-copyup-and-upper-cannot-hold`.
        if self.file_status(newpath.as_str()).is_ok() {
            return Err(LinkError::AlreadyExists);
        }
        // `newpath`'s parent may so far exist only in the read-only lower layer: migrate the
        // ancestor chain up and retry -- gm mutable
        // `layered-parent-copyup-and-upper-cannot-hold`.
        match self.upper.link(&oldpath, newpath.as_str()) {
            Ok(()) => Ok(()),
            Err(LinkError::PathError(PathError::MissingComponent)) => {
                let dirname = newpath.rsplit_once('/').unwrap().0;
                if let Ok(FileType::Directory) = self.ensure_lower_contains(dirname) {
                    self.mkdir_migrating_ancestor_dirs(&newpath)
                        .map_err(|e| match e {
                            MkdirError::NoWritePerms => LinkError::NoWritePerms,
                            MkdirError::ReadOnlyFileSystem => LinkError::ReadOnlyFileSystem,
                            MkdirError::Io => LinkError::Io,
                            MkdirError::AlreadyExists => unreachable!(),
                            MkdirError::PathError(e) => LinkError::PathError(e),
                        })?;
                    self.upper.link(oldpath, newpath)
                } else {
                    Err(LinkError::PathError(PathError::MissingComponent))
                }
            }
            Err(e) => Err(e),
        }
    }

    fn make_fifo(&self, path: impl crate::path::Arg, mode: Mode) -> Result<(), MkdirError> {
        let path = self.absolute_path(path)?;
        // Anything already at `path` in EITHER layer is `EEXIST`, since creation only ever targets
        // the upper layer -- gm mutable `layered-parent-copyup-and-upper-cannot-hold`.
        if self.file_status(path.as_str()).is_ok() {
            return Err(MkdirError::AlreadyExists);
        }
        match self.upper.make_fifo(path.as_str(), mode) {
            Ok(()) => Ok(()),
            // `path`'s parent may so far exist only in the read-only lower layer; migrate the
            // ancestor chain up and retry -- gm mutable
            // `layered-parent-copyup-and-upper-cannot-hold`.
            Err(MkdirError::PathError(PathError::MissingComponent)) => {
                let dirname = path.rsplit_once('/').unwrap().0;
                if let Ok(FileType::Directory) = self.ensure_lower_contains(dirname) {
                    self.mkdir_migrating_ancestor_dirs(&path)?;
                    self.upper.make_fifo(path.as_str(), mode)
                } else {
                    Err(MkdirError::PathError(PathError::MissingComponent))
                }
            }
            Err(e) => Err(e),
        }
    }

    fn symlink(
        &self,
        target: impl crate::path::Arg,
        linkpath: impl crate::path::Arg,
    ) -> Result<(), SymlinkError> {
        let linkpath = self.absolute_path(linkpath)?;
        // Anything already at `linkpath` in EITHER layer is `symlink(2)`'s `EEXIST`, since creation
        // only ever targets the upper layer -- gm mutable
        // `layered-parent-copyup-and-upper-cannot-hold`.
        if self.file_status(linkpath.as_str()).is_ok() {
            return Err(SymlinkError::AlreadyExists);
        }
        // There is no copy-on-write concept for creating a brand new path, but `linkpath`'s parent
        // may so far exist only in the read-only lower layer: migrate the ancestor chain up and
        // retry -- gm mutable `layered-parent-copyup-and-upper-cannot-hold`.
        match self.upper.symlink(&target, linkpath.as_str()) {
            Ok(()) => Ok(()),
            Err(SymlinkError::PathError(PathError::MissingComponent)) => {
                let dirname = linkpath.rsplit_once('/').unwrap().0;
                if let Ok(FileType::Directory) = self.ensure_lower_contains(dirname) {
                    self.mkdir_migrating_ancestor_dirs(&linkpath)
                        .map_err(|e| match e {
                            MkdirError::NoWritePerms => SymlinkError::NoWritePerms,
                            MkdirError::ReadOnlyFileSystem => SymlinkError::ReadOnlyFileSystem,
                            MkdirError::Io => SymlinkError::Io,
                            MkdirError::AlreadyExists => unreachable!(),
                            MkdirError::PathError(e) => SymlinkError::PathError(e),
                        })?;
                    self.upper.symlink(target, linkpath)
                } else {
                    Err(SymlinkError::PathError(PathError::MissingComponent))
                }
            }
            Err(e) => Err(e),
        }
    }

    fn read_link(&self, path: impl crate::path::Arg) -> Result<String, ReadLinkError> {
        let path = self.absolute_path(path)?;
        match self.upper.read_link(path.as_str()) {
            Ok(target) => Ok(target),
            Err(e) => match e {
                ReadLinkError::NotASymlink | ReadLinkError::Io => Err(e),
                ReadLinkError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    // None of these can be handled by lower level, just quit out early
                    Err(e)
                }
                ReadLinkError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {
                    // Not present (or not readable as a symlink) at the upper layer; check lower.
                    self.lower.read_link(path)
                }
            },
        }
    }

    fn mkdir(&self, path: impl crate::path::Arg, mode: Mode) -> Result<(), MkdirError> {
        let path = self.absolute_path(path)?;
        match self.upper.mkdir(path.as_str(), mode) {
            Ok(()) => {
                // If we could successfully make the directory, we know that things are "sane" at
                // the upper level, but we must also check the lower level to make sure that this
                // directory didn't already exist.
                if self.ensure_lower_contains(&path).is_ok() {
                    return Err(MkdirError::AlreadyExists);
                }
                return Ok(());
            }
            Err(e) => match e {
                MkdirError::NoWritePerms
                | MkdirError::Io
                | MkdirError::AlreadyExists
                | MkdirError::ReadOnlyFileSystem
                | MkdirError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    return Err(e);
                }
                MkdirError::PathError(PathError::NoSuchFileOrDirectory) => {
                    unreachable!()
                }
                MkdirError::PathError(PathError::MissingComponent) => {
                    // fallthrough
                }
            },
        }
        // We know that at least one of the components is missing. We should check each of the
        // components individually, making directories for any components that already exist at the
        // lower layer, and erroring out if no lower layer component exists of that form.
        self.mkdir_migrating_ancestor_dirs(&path)?;
        // And then now we can make the upper directory.
        self.upper.mkdir(path, mode)
    }

    fn rmdir(&self, path: impl crate::path::Arg) -> Result<(), RmdirError> {
        let path = self.absolute_path(path)?;

        // Prevent removing root explicitly (even if upper is empty).
        if path == "/" {
            return Err(RmdirError::Busy);
        }

        let dir_fd = match self.open(
            path.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) => match e {
                OpenError::PathError(PathError::ComponentNotADirectory) => {
                    return Err(RmdirError::NotADirectory);
                }
                OpenError::PathError(pe) => return Err(pe.into()),
                OpenError::AccessNotAllowed => todo!(),
                OpenError::Io => return Err(RmdirError::Io),
                OpenError::ReadOnlyFileSystem => {
                    return Err(RmdirError::ReadOnlyFileSystem);
                }
                OpenError::NoWritePerms
                | OpenError::AlreadyExists
                | OpenError::TruncateError(_) => {
                    unreachable!()
                }
            },
        };
        let entries = match self.read_dir(&dir_fd) {
            Ok(entries) => entries,
            Err(
                ReadDirError::ClosedFd | ReadDirError::NotADirectory | ReadDirError::PathOnlyFd,
            ) => {
                unreachable!()
            }
            Err(ReadDirError::Io) => return Err(RmdirError::Io),
        };
        self.close(&dir_fd).expect("close dir fd failed");
        // "." and ".." are always present; anything more => not empty.
        if entries.len() > 2 {
            return Err(RmdirError::NotEmpty);
        }

        // blindly rmdir at upper layer, suppressing non-existence errors.
        if let Err(e) = self.upper.rmdir(path.as_str()) {
            match e {
                RmdirError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {
                    // fallthrough
                }
                RmdirError::NotEmpty
                | RmdirError::NotADirectory
                | RmdirError::ReadOnlyFileSystem
                | RmdirError::PathError(
                    PathError::ComponentNotADirectory | PathError::InvalidPathname,
                ) => unreachable!(),
                RmdirError::Busy
                | RmdirError::NoWritePerms
                | RmdirError::Io
                | RmdirError::PathError(
                    PathError::NoSearchPerms { .. } | PathError::TooManySymlinkHops,
                ) => return Err(e),
            }
        }

        if let LayeringSemantics::LowerLayerReadOnly = self.layering_semantics {
            self.root
                .write()
                .entries
                .insert(path, Arc::new(EntryX::Tombstone));
        } else {
            // If lower layer is writable, we can just rmdir there too, suppressing non-existence errors.
            if let Err(e) = self.lower.rmdir(path.as_str()) {
                match e {
                    RmdirError::PathError(
                        PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                    ) => {
                        // fallthrough
                    }
                    RmdirError::NotEmpty
                    | RmdirError::NotADirectory
                    | RmdirError::ReadOnlyFileSystem
                    | RmdirError::PathError(
                        PathError::ComponentNotADirectory | PathError::InvalidPathname,
                    ) => unreachable!(),
                    RmdirError::Busy
                    | RmdirError::NoWritePerms
                    | RmdirError::Io
                    | RmdirError::PathError(
                        PathError::NoSearchPerms { .. } | PathError::TooManySymlinkHops,
                    ) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn read_dir(&self, fd: &FileFd<Platform, Upper, Lower>) -> Result<Vec<DirEntry>, ReadDirError> {
        let (entry, path) = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| {
                (
                    Arc::clone(&descriptor.entry.entry),
                    descriptor.entry.path.clone(),
                )
            })
            .ok_or(ReadDirError::ClosedFd)?;

        let mut entries = match entry.as_ref() {
            EntryX::Upper { fd } => {
                // Get entries from upper layer
                let mut upper_entries = self.upper.read_dir(fd)?;

                // Try to get entries from lower layer for the same path
                if let Ok(lower_fd) = self
                    .lower
                    .open(path.as_str(), OFlags::RDONLY, Mode::empty())
                {
                    if let Ok(lower_entries) = self.lower.read_dir(&lower_fd) {
                        // Merge entries, avoiding duplicates (upper layer takes precedence)
                        let upper_names: HashSet<String> =
                            upper_entries.iter().map(|e| e.name.clone()).collect();

                        for lower_entry in lower_entries {
                            if !upper_names.contains(&lower_entry.name) {
                                upper_entries.push(lower_entry);
                            }
                        }
                    }
                    let _ = self.lower.close(&lower_fd);
                }

                upper_entries
            }
            EntryX::Lower { fd } => {
                // This is the easy case, nothing to deal with upper entries.
                self.lower.read_dir(fd)?
            }
            EntryX::Tombstone => unreachable!(),
        };

        for e in &mut entries {
            if let Some(ni) = e.ino_info.take() {
                e.ino_info = Some(self.get_layered_nodeinfo(ni));
            }
        }
        Ok(entries)
    }

    fn file_status(&self, path: impl crate::path::Arg) -> Result<FileStatus, FileStatusError> {
        // The fields are destructured and immediately re-assembled so the compiler forces an update
        // here when inode support lands.
        let path = self.absolute_path(path)?;
        if let Some(entry) = self.root.read().entries.get(&path) {
            let FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info,
                blksize,
                atime,
                mtime,
            } = match entry.as_ref() {
                EntryX::Upper { fd } => self.upper.fd_file_status(fd)?,
                EntryX::Lower { fd } => self.lower.fd_file_status(fd)?,
                EntryX::Tombstone => {
                    return Err(PathError::NoSuchFileOrDirectory)?;
                }
            };
            return Ok(FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info: self.get_layered_nodeinfo(node_info),
                blksize,
                atime,
                mtime,
            });
        }
        // The file is not open, we must look at the levels themselves.
        match self.upper.file_status(&*path) {
            Ok(FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info,
                blksize,
                atime,
                mtime,
            }) => {
                return Ok(FileStatus {
                    file_type,
                    mode,
                    size,
                    owner,
                    node_info: self.get_layered_nodeinfo(node_info),
                    blksize,
                    atime,
                    mtime,
                });
            }
            Err(e) => match e {
                FileStatusError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    // None of these can be handled by lower level, just quit out early
                    return Err(e);
                }
                FileStatusError::Io => return Err(e),
                FileStatusError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {
                    // Handle-able by a lower level, fallthrough
                }
                FileStatusError::ClosedFd => unreachable!(),
            },
        }
        let FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info,
            blksize,
            atime,
            mtime,
        } = self.lower.file_status(path)?;
        Ok(FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info: self.get_layered_nodeinfo(node_info),
            blksize,
            atime,
            mtime,
        })
    }

    fn symlink_metadata(&self, path: impl crate::path::Arg) -> Result<FileStatus, FileStatusError> {
        // The not-an-already-open-fd fallback must call `symlink_metadata`, not `file_status`, on
        // each layer, so a final-component symlink's own metadata survives. An already-open fd can
        // never itself be a symlink, so that branch stays on `fd_file_status` -- gm mutable
        // `layered-doc-trims`.
        let path = self.absolute_path(path)?;
        if let Some(entry) = self.root.read().entries.get(&path) {
            let FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info,
                blksize,
                atime,
                mtime,
            } = match entry.as_ref() {
                EntryX::Upper { fd } => self.upper.fd_file_status(fd)?,
                EntryX::Lower { fd } => self.lower.fd_file_status(fd)?,
                EntryX::Tombstone => {
                    return Err(PathError::NoSuchFileOrDirectory)?;
                }
            };
            return Ok(FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info: self.get_layered_nodeinfo(node_info),
                blksize,
                atime,
                mtime,
            });
        }
        match self.upper.symlink_metadata(&*path) {
            Ok(FileStatus {
                file_type,
                mode,
                size,
                owner,
                node_info,
                blksize,
                atime,
                mtime,
            }) => {
                return Ok(FileStatus {
                    file_type,
                    mode,
                    size,
                    owner,
                    node_info: self.get_layered_nodeinfo(node_info),
                    blksize,
                    atime,
                    mtime,
                });
            }
            Err(e) => match e {
                FileStatusError::PathError(
                    PathError::ComponentNotADirectory
                    | PathError::InvalidPathname
                    | PathError::NoSearchPerms { .. }
                    | PathError::TooManySymlinkHops,
                ) => {
                    return Err(e);
                }
                FileStatusError::Io => return Err(e),
                FileStatusError::PathError(
                    PathError::NoSuchFileOrDirectory | PathError::MissingComponent,
                ) => {}
                FileStatusError::ClosedFd => unreachable!(),
            },
        }
        let FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info,
            blksize,
            atime,
            mtime,
        } = self.lower.symlink_metadata(path)?;
        Ok(FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info: self.get_layered_nodeinfo(node_info),
            blksize,
            atime,
            mtime,
        })
    }

    fn fd_file_status(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
    ) -> Result<FileStatus, FileStatusError> {
        let entry = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| Arc::clone(&descriptor.entry.entry))
            .ok_or(FileStatusError::ClosedFd)?;
        let FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info,
            blksize,
            atime,
            mtime,
        } = match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.fd_file_status(fd)?,
            EntryX::Lower { fd } => self.lower.fd_file_status(fd)?,
            EntryX::Tombstone => unreachable!(),
        };
        Ok(FileStatus {
            file_type,
            mode,
            size,
            owner,
            node_info: self.get_layered_nodeinfo(node_info),
            blksize,
            atime,
            mtime,
        })
    }

    fn get_static_backing_data(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
    ) -> Option<&'static [u8]> {
        let entry = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| Arc::clone(&descriptor.entry.entry))?;
        match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.get_static_backing_data(fd),
            EntryX::Lower { fd } => self.lower.get_static_backing_data(fd),
            EntryX::Tombstone => unreachable!(),
        }
    }
}

struct Descriptor<Upper: super::FileSystem + 'static, Lower: super::FileSystem + 'static> {
    path: String,
    flags: OFlags,
    entry: Entry<Upper, Lower>,
    position: AtomicUsize,
}

struct RootDir<Upper: super::FileSystem + 'static, Lower: super::FileSystem + 'static> {
    // Keys are normalized paths, directories without the final `/` (so the root is the empty-string
    // key). Invariant: only `Lower` and `Tombstone` entries are ever stored here, never `Upper`.
    entries: HashMap<String, Entry<Upper, Lower>>,
}

impl<Upper: super::FileSystem, Lower: super::FileSystem> RootDir<Upper, Lower> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

type Entry<Upper, Lower> = Arc<EntryX<Upper, Lower>>;

enum EntryX<Upper: super::FileSystem + 'static, Lower: super::FileSystem + 'static> {
    // Purely an upper-level file, whether or not a lower-level file exists.
    Upper { fd: TypedFd<Upper> },
    // A lower-level file that does NOT exist in the upper level.
    Lower { fd: TypedFd<Lower> },
    // Exists in the lower level, but is marked deleted as far as the layering is concerned.
    Tombstone,
}

impl<Upper: super::FileSystem + 'static, Lower: super::FileSystem + 'static> core::fmt::Debug
    for EntryX<Upper, Lower>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Upper { fd: _ } => f.debug_struct("Upper").finish_non_exhaustive(),
            Self::Lower { fd: _ } => f.debug_struct("Lower").finish_non_exhaustive(),
            Self::Tombstone => write!(f, "Tombstone"),
        }
    }
}

crate::fd::enable_fds_for_subsystem! {
    @Platform: { sync::RawSyncPrimitivesProvider }, Upper: { super::FileSystem + 'static }, Lower: { super::FileSystem + 'static };
    FileSystem<Platform, Upper, Lower>;
    @Upper: { super::FileSystem + 'static }, Lower: { super::FileSystem + 'static };
    Descriptor<Upper, Lower>;
    -> FileFd<Platform, Upper, Lower>;
}
