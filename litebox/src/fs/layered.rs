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
    /// No new files can be made at the lower layer, but any existing files in the lower layer can
    /// still be written to. If an upper level file exists with the same name as a lower layer file,
    /// then it is shadowed, and only the upper layer file would be visible.
    LowerLayerWritableFiles,
}

/// A backing implementation of [`FileSystem`](super::FileSystem) that layers a file system on top
/// of another.
///
/// This particular implementation itself doesn't carry or store any of the files, but delegates to
/// each of the layers. Specifically, this implementation will look for and work with files in
/// the upper layer, unless they don't exist, in which case the lower layer is looked at.
///
/// The current design of layering supports treating the lower layer as read-only, or as a
/// transparent write-through. In read-only lower layer, if a file is opened in writable mode that
/// doesn't exist in the upper layer, but _does_ exist in the lower layer, this will have
/// copy-on-write semantics.
///
/// Future versions of the layering might support other configurable options for the layering.
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
    // Serializes `migrate_file_up` end-to-end: that function reads `self.root` under a lock,
    // releases it, then later re-derives state (`Arc::strong_count`) it assumes is still valid
    // when it swaps the descriptor-table entry over to the upper layer. Two threads racing to
    // migrate the SAME path concurrently (e.g. two guest threads independently opening the same
    // shared library or executable for the first time) can interleave in that gap and violate
    // the swap's own `Arc::ptr_eq` invariant. Migrations are rare (first-open-for-write per file
    // only), so serializing the whole function is a correctness fix with no meaningful cost.
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

    /// (private-only) Create all parent/ancestor directories for a `path`, making sure that each of
    /// these exist in the lower layer. It does _not_ set up `path` itself on the upper layer
    /// though; this is left to the callee to handle.
    ///
    /// NOTE: This is _not_ equivalent to running `mkdir -p {path}` or `mkdir {path}` or anything
    /// like that.
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
                Ok(FileType::RegularFile | FileType::CharacterDevice | FileType::Symlink)
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

    /// (private-only) Migrate a file from lower to upper layer
    ///
    /// It performs a check to make sure that the lower level has the file, and if the lower-level
    /// does not, then it will error out with the relevant `PathError` that can be propagated as
    /// necessary.
    ///
    /// Note: this focuses only on files.
    ///
    /// If `copy_data` is `true`, it copies over the lower data to the upper one, otherwise, it
    /// makes the upper file empty (similar to a truncate). Generally speaking, you want to use
    /// `true` for `copy_data`.
    fn migrate_file_up(&self, path: &str, copy_data: bool) -> Result<(), MigrationError> {
        // Serialize the entire migration end-to-end (see `migrate_lock`'s own doc comment): two
        // threads racing to migrate the SAME path concurrently can otherwise interleave between
        // this function's own `self.root` read and its later `Arc`-based swap, violating that
        // swap's `Arc::ptr_eq` invariant. Held for the whole call, not just the swap, since the
        // race exists across the full open-lower/open-upper/copy/swap sequence, not just its tail.
        let _migrate_guard = self.migrate_lock.lock();

        // This function's mechanics (open-for-read on `self.lower`, open/write on `self.upper`)
        // are agnostic to `self.layering_semantics` -- both `write`'s and `truncate`'s
        // `LowerLayerWritableFiles` branches now call this directly as a fallback when their own
        // attempt to delegate straight to `self.lower` fails because the lower fs's own upper
        // can't hold `path` (see `MigrationError::UpperCannotHoldPath`). There is deliberately no
        // per-semantics guard here anymore -- every caller decides for itself, based on its own
        // control flow, whether reaching this function is the correct thing to do for its layer.

        // We first open the file up at the lower level for reading
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
        // We begin to read the lower file before opening the upper file, just in case the lower
        // file is not really a file (in which case, we don't want to tell the upper layer anything,
        // but error out sooner.
        //
        // Other than that, this is a simple loop that just copies over in chunks by a simple
        // read-write loop.
        let mut upper_fd = None;
        let mut temp_buf = [0u8; 4096];
        loop {
            match self.lower.read(&lower_fd, &mut temp_buf, None) {
                Ok(size) => {
                    if upper_fd.is_none() {
                        // We are here the first time around, and did not error out, yay! We can
                        // actually open up the file.
                        //
                        // First, we make sure we've set up the ancestor directories.
                        match self.mkdir_migrating_ancestor_dirs(path) {
                            Ok(()) => {}
                            Err(MkdirError::ReadOnlyFileSystem) => {
                                // This upper layer physically cannot hold `path` (e.g. `dev_stdio`,
                                // which only backs `/dev`, being asked to migrate a non-`/dev`
                                // path). This is a structural mismatch, not a transient failure --
                                // let the caller fall back to a different upper layer capable of
                                // holding it, rather than treating it as unreachable/fatal.
                                let _ = self.lower.close(&lower_fd);
                                return Err(MigrationError::UpperCannotHoldPath);
                            }
                            Err(e) => unimplemented!("{e} when setting up ancestor dirs"),
                        }
                        // Now we can actually open the file.
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
                        // EOF
                        break;
                    }
                }
                Err(e) => match e {
                    // `NotForReading` is real and reachable here, not merely theoretical: `path`
                    // opened successfully `RDONLY` above (a directory opens fine RDONLY on every
                    // backend in this codebase), but a directory cannot actually be streamed via
                    // `read()` -- confirmed live via a real weston/litebox repro where a
                    // Lower-classified fd's `path` resolved to a directory, not a regular file,
                    // and hit exactly this arm instead of `NotAFile` (whichever earlier stage
                    // classified this path as needing migration did not itself verify it names a
                    // regular file). Treat it the same as `NotAFile` -- both mean "this isn't a
                    // stream of file bytes to migrate", the same real-world condition by two
                    // different backends' error taxonomies, not two different bugs.
                    ReadError::NotAFile | ReadError::NotForReading => {
                        // We can only have this happen the first time around
                        assert!(upper_fd.is_none());
                        // In which case we quit early
                        return Err(MigrationError::NotAFile);
                    }
                    ReadError::ClosedFd => unreachable!(),
                    ReadError::Io => return Err(MigrationError::Io),
                },
            }
        }
        // After migrating the data, we also use these FDs to migrate the node-info over, so that
        // any caller that tries to get the inode before/after the migration sees the same inode.
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
            assert!(old.is_none());
        }
        // Now that we've migrated the data (and node-info) over, we can close out both of the file
        // descriptors.
        self.upper.close(&upper_fd.unwrap()).unwrap();
        self.lower.close(&lower_fd).unwrap();

        // Now we need to migrate all the descriptor entries over.
        //
        // Perf: this does a full scan over all open descriptors: if a process has a HUGE number of
        // open descriptors, this could be slow.
        //
        // This lock is held across the ENTIRE migration loop below (collection through the final
        // `Arc::strong_count` check and swap), not just released after collecting `to_migrate`.
        // `open()`'s `EntryX::Lower` fast path (which `Arc::clone`s this same path's entry into a
        // brand-new descriptor-table slot, bumping its strong count) takes `self.root.read()` --
        // holding the write lock here for the full duration genuinely excludes that racing path,
        // closing the window where a concurrent `open()` could invalidate the strong-count
        // invariant the swap below assumes still holds after `to_migrate` was collected.
        let mut root_guard = self.root.write();
        let RootDir {
            entries: root_entries,
        } = &mut *root_guard;
        // First we figure out which entries need to be moved up. These entries are arc-cloned into
        // a `Vec` so that we can release the lock the file descriptor table when setting things up
        // within the upper layer.
        let to_migrate: alloc::vec::Vec<(InternalFd, usize, OFlags, Entry<Upper, Lower>)> = self
            .litebox
            .descriptor_table()
            .iter::<Self>()
            .filter_map(|(internal_fd, e)| {
                if e.entry.path != path {
                    // Skip any that do not match the path
                    return None;
                }
                match &*e.entry.entry {
                    EntryX::Upper { fd: _ } => {
                        // Need to do nothing, jump to next
                        None
                    }
                    EntryX::Lower { fd: _ } => {
                        // We need to change this up to an upper-level entry.
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
        // Now we can actually perform the migration, since we've unlocked the lock on the
        // file-descriptor table, which allows us to actually access things within the upper/lower
        // levels without trouble.
        for (internal_fd, position, flags, entry) in to_migrate {
            // First, we set up the upper entry we'll be swapping/placing in.
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
            // Then we check up on replacing entries
            match Arc::strong_count(&entry) {
                0..=2 => {
                    // Normally unreachable: while `to_migrate` was being collected, this count
                    // was guaranteed to be >=3 (our own local `entry` clone here, `root_entries`'s
                    // own reference, and the descriptor-table slot's own reference). But
                    // `to_migrate`'s collection only holds the descriptor table's OWN lock (not
                    // `self.root`'s, which THIS loop holds for its whole duration) -- a concurrent
                    // `close()` on this exact `internal_fd`, racing between that collection and
                    // this check, can drop the descriptor-table-side reference in the meantime,
                    // observably reducing the count below 3 by the time we get here. `entry` and
                    // `root_entries`'s own reference are still safely intact either way (the write
                    // lock this loop holds on `self.root` for its whole duration guarantees
                    // `root_entries` itself is untouched); there's simply nothing left in the
                    // descriptor table for THIS `internal_fd` to migrate anymore -- skip it, the
                    // close already tore down whatever it referenced. `upper_entry` was never
                    // installed anywhere else in this branch, so it's still the sole owner of
                    // `upper_fd`; unwrap it back out to close it rather than leaking the fd.
                    let EntryX::Upper { fd: upper_fd } = Arc::into_inner(upper_entry).unwrap()
                    else {
                        unreachable!()
                    };
                    self.upper.close(&upper_fd).ok();
                    continue;
                }
                3 => {
                    // Perfect amount to trigger a `close` on the lower level, and remove
                    // the underlying root entry, since further syncing is no longer
                    // necessary.
                    //
                    // `internal_fd` was captured while iterating the descriptor table under its
                    // own, separate lock (dropped before this loop runs) -- a concurrent `close`
                    // (possibly racing with a `dup` reusing the freed slot for an unrelated file)
                    // on this exact slot between that iteration and now would otherwise make
                    // `with_entry_mut_via_internal_fd` silently swap out a DIFFERENT entry that
                    // now happens to live at the same index, violating the `Arc::ptr_eq` invariant
                    // below without any actual correctness problem for `path`'s own migration --
                    // that slot no longer holds anything referencing `path` at all. Compare-and-
                    // skip instead of blindly swapping: `with_entry_mut_via_internal_fd` only
                    // performs the replace if the closure's own check (comparing against `entry`
                    // first) confirms this is still genuinely the same `Arc` we collected earlier.
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
                        // The slot was closed and/or reused for an unrelated file by a concurrent
                        // `close`/`open` between collection and this replace -- nothing of `path`'s
                        // migration remains to do for this `internal_fd`; move on to the next one.
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
                    // Other FDs are open with the same file too. We'll handle the open one
                    // here locally, and a future FD will take care of the relevant closing.
                    //
                    // Same compare-and-skip rationale as the `3 =>` arm above: `internal_fd` may
                    // have been closed and its slot reused by a concurrent `close`/`open` since
                    // `to_migrate` was collected.
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

        Ok(())
    }

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
    /// The upper layer cannot hold this path at all (e.g. a namespace like `/dev` that only
    /// backs a narrow subtree of the full path space) -- distinct from `Io`, since this is a
    /// structural, always-reproducible mismatch rather than a transient failure. Callers that
    /// have another upper layer capable of holding the path (e.g. an outer layered fs whose own
    /// upper is a general-purpose in-memory fs) should fall back to migrating there instead.
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
            | OFlags::PATH;
        if flags.intersects(currently_supported_oflags.complement()) {
            unimplemented!("{flags:?}")
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
        // If we already have an entry saying it is a tombstone, then we need to quit out early;
        // otherwise, we'll check the levels.
        if let Some(entry) = self.root.read().entries.get(&path) {
            match entry.as_ref() {
                EntryX::Tombstone => {
                    // The file has been cleared out; it used to exist on the lower level, but we
                    // explicitly have placed a tombstone in its place.
                    if flags.contains(OFlags::CREAT) {
                        // Fallthrough, since we will create it at the upper level now. We should
                        // remove the tombstone though.
                        tombstone_removal = true;
                    } else {
                        Err(PathError::NoSuchFileOrDirectory)?;
                    }
                }
                EntryX::Upper { .. } => unreachable!(),
                EntryX::Lower { .. } => {
                    // As an optimization, since a lower-level file entry is always opened with the
                    // same flags, and since it indicates that there is no such file at the upper
                    // level, we can just return that directly (with the "real" flags being wrapped
                    // up in the layered descriptor).
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
                // Another thread which also was attempting to create the same file (on top of a
                // tombstoned file) won on the race to lock `self.root`, and thus it has already
                // removed it for us. We don't need to remove it, and can proceed as normal.
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
                    // We must check if the lower layer contains all the directories; if it does, we
                    // can create the same directories and then re-trigger the open.
                    //
                    // A top-level path (e.g. `/.memfd:17`) splits into an EMPTY `dirname` here
                    // (`rsplit_once('/')` on `/.memfd:17` yields `("", ".memfd:17")`), which is not
                    // itself a valid path `ensure_lower_contains`/`file_status` accepts -- it must be
                    // normalized to root `/` first, exactly like every other path this method
                    // receives already goes through `self.absolute_path()`. Without this, a brand-new
                    // top-level file whose Upper root directory doesn't exist yet silently falls
                    // through to the Lower-layer open path below instead of creating the directory and
                    // retrying on Upper -- the new file is then classified as `EntryX::Lower`, and any
                    // later `truncate`/`fallocate`/write on it hits `migrate_file_up`'s read-side
                    // `unreachable!()` (`ReadError::NotForReading`) instead of writing normally.
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
        // Any errors from lower level now _must_ propagate up, so we can just invoke
        // the lower level and set up the relevant descriptor upon success.
        //
        // `self.lower.open` runs without holding `self.root`'s write lock (it can be slow, e.g. a
        // real syscall/IO), so two threads opening the same not-yet-cached path concurrently can
        // both reach here with their own freshly opened lower-level fd. Whichever thread's
        // `entries.insert` below runs second must NOT blindly overwrite/duplicate the winner's
        // entry (that previously asserted `old.is_none()`, which is not actually guaranteed under
        // concurrency, and left a second live `Lower` fd untracked by `root.entries` -- exactly the
        // divergence that corrupted later `close()`'s `Arc::ptr_eq` bookkeeping). Instead, the loser
        // discards its own redundant fd and reuses the entry the winner already installed, matching
        // the existing cache-hit fast path a few lines above for a path that was already resolved
        // by an earlier, separate open.
        let our_entry = Arc::new(EntryX::Lower {
            fd: self.lower.open(path.as_str(), flags, mode)?,
        });
        let entry = {
            let mut root = self.root.write();
            if let Some(existing) = root.entries.get(&path) {
                let existing = Arc::clone(existing);
                // Safe to drop `root`'s lock before closing: `our_entry` was never published,
                // so no other thread can observe or hold a reference to its fd.
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
        let fd = self.litebox.descriptor_table_mut().insert(Descriptor {
            path,
            flags: original_flags,
            entry,
            position: 0.into(),
        });
        if original_flags.contains(OFlags::TRUNC) {
            // The only scenario where we need to manually trigger truncation is when a file does
            // not exist at the upper level but exists at the lower level; in that case, our
            // `truncate` functionality (at the layered FS itself) should correctly migrate things
            // over and handle them.
            match self.truncate(&fd, 0, true) {
                Ok(()) | Err(TruncateError::IsTerminalDevice) => {
                    // The terminal device is the one case we need to (due to Linux compatibility)
                    // explicitly ignore the truncation ability, and instead silently continue as if
                    // no error was thrown during truncation.
                }
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
            // Was duplicated, don't need to do anything.
            return Ok(());
        };
        let Descriptor {
            path,
            entry,
            flags: _,
            position: _,
        } = removed_entry.entry;
        // We can first sanity check that we don't have a tombstone: none of the other operations
        // should ever cause the entry _at_ an fd to become a tombstone, even if the entry at the
        // path becomes a tombstone due to a file removal.
        match entry.as_ref() {
            EntryX::Upper { .. } | EntryX::Lower { .. } => {}
            EntryX::Tombstone => unreachable!(),
        }
        // Crucially, we need to grab an exclusive lock to the root, so that the counts cannot
        // change while we are reasoning about them.
        let RootDir {
            entries: root_entries,
        } = &mut *self.root.write();
        // Our approach to this changes depending on whether this is an upper level FD or a
        // lower FD.
        match *entry {
            EntryX::Tombstone => {
                // A tombstone should never have even become an FD (if a file was opened, and then
                // was subsequently deleted, then the FD itself would not yet be a tombstone, but
                // would be pointing to the original value).
                unreachable!()
            }
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
                // Lower level FDs almost always have a corresponding entry in the root. Thus, we
                // might need to possibly clean things up from the root.
                //
                // First, we can attempt a fast-path clean-up by quickly check if there are other
                // FDs referring to the same file
                if Arc::strong_count(&entry) > 2 {
                    // There are _definitely_ other FDs pointing at this file, leave it alone
                    return Ok(());
                }
                // Otherwise, either we have ourselves and the root pointing at it OR the root has
                // been tombstoned out after the FDs have been opened at it.
                match **root_entries.get(&path).unwrap() {
                    EntryX::Upper { .. } => unreachable!(),
                    EntryX::Lower { .. } => {
                        // We are going to have to deal with it at the entry too, fallthrough
                    }
                    EntryX::Tombstone => {
                        // A tombstone here means that the root doesn't contain the entry. There may
                        // possibly be other FDs opened for the same file before it was tombstoned
                        // out, so we'll close it out if we are the sole remaining holder;
                        // otherwise, it will be someone else's job to do so.
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
        // Since a write to a lower-level file upgrades the underlying entry out completely to an
        // upper-level file, we don't actually need to worry about a desync; a write to lower-level
        // file will successfully be seen as just being an upper level file. Thus, it is sufficient
        // just to delegate this operation based whether the entry points to upper or lower layers.
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
        // A `Lower` entry's underlying fd is cached in `root.entries` and shared across every
        // `open()` of the same path (see `open`'s cache-hit fast path and its race-loser fallback
        // above) -- unlike an `Upper` fd, which is always a fresh, unshared fd per `open()` call.
        // An implicit `offset: None` read is documented to use "the current file offset" (see this
        // trait method's own doc comment), which for a shared `Lower` fd is NOT this specific
        // layered `Descriptor`'s own position: two independent opens of the same lower-layer file
        // would otherwise silently read from (and advance) one shared position, each stealing bytes
        // the other expected to see from its own start-at-0. Resolve `None` to this descriptor's
        // own tracked `position` explicitly before delegating, so every open of a `Lower` file keeps
        // an independent read cursor, matching real POSIX per-open-fd offset semantics.
        let resolved_offset = match entry.as_ref() {
            EntryX::Upper { .. } => offset,
            EntryX::Lower { .. } => Some(offset.unwrap_or(this_position)),
            EntryX::Tombstone => unreachable!(),
        };
        // Perform the actual operation
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
        // Writing needs to be careful of how it is performing the write. Any upper-level file can
        // instantly be written to; but a lower-level file must become a upper-level file, before
        // actually being written to.
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
                        // Allow direct write to lower layer, unless the lower layer itself can't
                        // hold this path in its own upper (e.g. the lower is a `dev_stdio`-over-
                        // `tar_ro` fs and this path isn't under `/dev`) -- in which case fall
                        // through below to migrate the file into *this* fs's own upper instead,
                        // same as the `LowerLayerReadOnly` case.
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
        // Change it to an upper-level file, also altering the file descriptor.
        drop(entry);
        match self.migrate_file_up(&path, true) {
            Ok(()) => {}
            Err(MigrationError::NoReadPerms) => unimplemented!(),
            Err(MigrationError::NotAFile) => return Err(WriteError::NotAFile),
            Err(MigrationError::Io) => return Err(WriteError::Io),
            Err(MigrationError::PathError(_e)) => unreachable!(),
            // This fs's own upper layer cannot hold `path` (see `UpperCannotHoldPath`'s doc
            // comment) -- e.g. this is the inner `dev_stdio`-over-`tar_ro` fs and `path` isn't
            // under `/dev`. Surface it as `NotForWriting`: not semantically precise, but the
            // closest existing `WriteError` variant, and an outer fs composing this one as its
            // `lower` (see the `EntryX::Lower` branch in the outer `write` above) specifically
            // matches on this to fall back to migrating through its *own* upper instead.
            Err(MigrationError::UpperCannotHoldPath) => return Err(WriteError::NotForWriting),
        }
        // As a sanity check, in debug mode, confirm that it is now an upper file
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
        // Since it has been migrated, we can just re-trigger, causing it to apply to the
        // upper layer
        self.write(fd, buf, offset)
    }

    fn seek(
        &self,
        fd: &FileFd<Platform, Upper, Lower>,
        offset: isize,
        whence: SeekWhence,
    ) -> Result<usize, SeekError> {
        let entry = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| Arc::clone(&descriptor.entry.entry))
            .ok_or(SeekError::ClosedFd)?;
        // Perform the seek, and update the position info
        let position = match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.seek(fd, offset, whence)?,
            EntryX::Lower { fd } => self.lower.seek(fd, offset, whence)?,
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
                                // The lower fs's own upper can't hold this path -- fall back to
                                // migrating into *this* fs's own upper, same pattern as `write`'s
                                // `LowerLayerWritableFiles` branch above.
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
                                    // We must actually migrate this file up, and keep it truncated.
                                    //
                                    // We must first drop the cloned entry to make sure that the ref
                                    // counting works out correctly during migration.
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
                                        // This fs's own upper can't hold `path` (see
                                        // `UpperCannotHoldPath`'s doc comment). Surface as
                                        // `NotForWriting`, the same signal an outer fs composing
                                        // this one as its `lower` already matches on to fall back
                                        // to migrating through its own upper instead (see the
                                        // outer `truncate`'s `LowerLayerWritableFiles` branch).
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
        // Mirrors `truncate` above (see [`super::FileSystem::chmod_fd`]'s doc comment on why
        // this must operate on the already-open handle rather than re-resolving `fd` to a path
        // -- the whole point is to keep working after the caller has `unlink`ed the path this fd
        // was opened at).
        let entry = self
            .litebox
            .descriptor_table()
            .with_entry(fd, |descriptor| Arc::clone(&descriptor.entry.entry))
            .ok_or(ChmodError::Io)?;
        match entry.as_ref() {
            EntryX::Upper { fd } => self.upper.chmod_fd(fd, mode),
            EntryX::Lower { fd } => {
                // A file opened purely from the lower (read-only-relative-to-this-layer, per
                // `LayeringSemantics::LowerLayerReadOnly`) layer was never write-opened through
                // *this* fs (an `O_CREAT`/write-opened path always migrates up at `open` time,
                // matching `write`'s/`truncate`'s own upper-vs-lower split above) -- so a still-
                // open fd resolving to `EntryX::Lower` here can only be a read-only fd, for which
                // real `fchmod` is still valid on Linux (permission bits are independent of the
                // fd's own read/write mode) but this bounded implementation has no upper-
                // migration path for an *already-open, no-longer-path-addressable* fd (unlike
                // `chmod`'s own path-based migrate-then-retry, which works because it re-resolves
                // the path before the migrated file's fd would need to change). No real call site
                // in this codebase (wlroots' shm-file dance, the only real `fchmod` caller,
                // always operates on a freshly created-in-upper file) exercises this, so this
                // stays a hard error rather than growing unverified migration logic.
                let _ = fd;
                Err(ChmodError::Io)
            }
            EntryX::Tombstone => unreachable!(),
        }
    }

    fn chmod(&self, path: impl crate::path::Arg, mode: Mode) -> Result<(), ChmodError> {
        let path = self.absolute_path(path)?;
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
                    // fallthrough
                }
            },
        }
        match self.ensure_lower_contains(&path) {
            Ok(_) => {}
            Err(FileStatusError::Io) => return Err(ChmodError::Io),
            Err(FileStatusError::PathError(e)) => return Err(ChmodError::PathError(e)),
            Err(FileStatusError::ClosedFd) => unreachable!(),
        }
        match self.migrate_file_up(&path, true) {
            Ok(()) => {}
            Err(MigrationError::NoReadPerms) => unimplemented!(),
            Err(MigrationError::NotAFile) => unimplemented!(),
            Err(MigrationError::Io) => return Err(ChmodError::Io),
            Err(MigrationError::PathError(_e)) => unreachable!(),
            Err(MigrationError::UpperCannotHoldPath) => unreachable!(
                "this fs's own upper should always be able to hold a path already confirmed to exist in its lower"
            ),
        }
        // Since it has been migrated, we can just re-trigger, causing it to apply to the
        // upper layer
        self.chmod(path, mode)
    }

    fn chown(
        &self,
        path: impl crate::path::Arg,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError> {
        let path = self.absolute_path(path)?;
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
                    // fallthrough
                }
            },
        }
        match self.ensure_lower_contains(&path) {
            Ok(_) => {}
            Err(FileStatusError::Io) => return Err(ChownError::Io),
            Err(FileStatusError::PathError(e)) => return Err(ChownError::PathError(e)),
            Err(FileStatusError::ClosedFd) => unreachable!(),
        }
        match self.migrate_file_up(&path, true) {
            Ok(()) => {}
            Err(MigrationError::NoReadPerms) => unimplemented!(),
            Err(MigrationError::NotAFile) => unimplemented!(),
            Err(MigrationError::Io) => return Err(ChownError::Io),
            Err(MigrationError::PathError(_e)) => unreachable!(),
            Err(MigrationError::UpperCannotHoldPath) => unreachable!(
                "this fs's own upper should always be able to hold a path already confirmed to exist in its lower"
            ),
        }
        // Since it has been migrated, we can just re-trigger, causing it to apply to the
        // upper layer
        self.chown(path, user, group)
    }

    fn set_times(
        &self,
        path: impl crate::path::Arg,
        atime: Option<super::Timestamp>,
        mtime: Option<super::Timestamp>,
    ) -> Result<(), SetTimesError> {
        let path = self.absolute_path(path)?;
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
                    // fallthrough
                }
            },
        }
        match self.ensure_lower_contains(&path) {
            Ok(_) => {}
            Err(FileStatusError::Io) => return Err(SetTimesError::Io),
            Err(FileStatusError::PathError(e)) => return Err(SetTimesError::PathError(e)),
            Err(FileStatusError::ClosedFd) => unreachable!(),
        }
        match self.migrate_file_up(&path, true) {
            Ok(()) => {}
            Err(MigrationError::NoReadPerms) => unimplemented!(),
            Err(MigrationError::NotAFile) => unimplemented!(),
            Err(MigrationError::Io) => return Err(SetTimesError::Io),
            Err(MigrationError::PathError(_e)) => unreachable!(),
            Err(MigrationError::UpperCannotHoldPath) => unreachable!(
                "this fs's own upper should always be able to hold a path already confirmed to exist in its lower"
            ),
        }
        // Since it has been migrated, we can just re-trigger, causing it to apply to the
        // upper layer
        self.set_times(path, atime, mtime)
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
                        FileType::RegularFile | FileType::Symlink => {
                            // fallthrough
                        }
                        FileType::Directory => {
                            return Err(UnlinkError::IsADirectory);
                        }
                        FileType::CharacterDevice => unimplemented!(),
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
        // Scoped to the common case this method exists to support (a package manager-style
        // atomic replace of a file it just wrote into the writable upper layer, e.g. `apk`
        // installing a downloaded package, or atomically replacing its own on-disk database):
        // `from` must already live purely in the upper (writable) layer -- renaming a source that
        // only exists in the read-only lower layer would require migrating it up first, which is
        // real Linux `EXDEV`-fallback territory (`RenameError::CrossDevice`), same as `rename`'s
        // other implementations in this codebase.
        //
        // `to`, however, is allowed to shadow an existing lower-layer entry (this is exactly the
        // `apk`-installed-database-file case: the database already shipped as part of the base
        // image, so it exists in the lower/tar layer, and `apk` atomically replaces it by
        // rename-over, same pattern as `unlink` already supports via a tombstone below).
        if self.ensure_lower_contains(&from).is_ok() {
            return Err(RenameError::CrossDevice);
        }
        // `to`'s parent directory may only exist in the read-only lower layer so far (e.g. a
        // package extractor's typical write-to-temp-then-atomic-`rename()`-into-place idiom,
        // renaming into a directory that came from the read-only initial rootfs and has not yet
        // been touched on the upper layer) -- mirror `open`'s `O_CREAT` handling and `symlink`'s
        // equivalent fallback above: on a missing-component error, check whether the lower layer
        // has the parent directory, migrate the ancestor chain up to the upper layer if so, and
        // retry.
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
        // If `to` shadows a lower-layer entry, place a tombstone over it -- otherwise a stale
        // cached `EntryX::Lower` entry (from a previous open of `to` before this rename) would
        // keep being returned by `open`, exactly as `unlink` guards against below.
        if self.ensure_lower_contains(&to).is_ok() {
            self.root
                .write()
                .entries
                .insert(to, Arc::new(EntryX::Tombstone));
        } else {
            // No lower-layer shadowing, but there might still be a stale cached entry (e.g. an
            // `EntryX::Upper` from a previous open of a *different* upper-layer file at the same
            // path that was later unlinked and recreated) -- clear it so the next `open` re-reads
            // fresh, mirroring the invalidation `unlink` performs by simply not caching anything
            // for a path with no lower-layer shadow.
            self.root.write().entries.remove(&to);
        }
        Ok(())
    }

    fn link(
        &self,
        oldpath: impl crate::path::Arg,
        newpath: impl crate::path::Arg,
    ) -> Result<(), LinkError> {
        let oldpath = self.absolute_path(oldpath)?;
        let newpath = self.absolute_path(newpath)?;
        // Scoped to the common case this exists to support (Xorg-style atomic lock-file
        // acquisition: link a temp file the caller just wrote into the writable upper layer to
        // its final name) -- same rationale, and the exact same restriction, as `rename` above:
        // `oldpath` must already live purely in the upper layer, or this is real Linux `EXDEV`
        // territory.
        if self.ensure_lower_contains(&oldpath).is_ok() {
            return Err(LinkError::CrossDevice);
        }
        // Fail if anything already exists at `newpath`, in either layer -- matches Linux
        // `link(2)`'s `EEXIST`, and mirrors `symlink`'s identical check just below.
        if self.file_status(newpath.as_str()).is_ok() {
            return Err(LinkError::AlreadyExists);
        }
        // `newpath`'s parent directory may only exist in the read-only lower layer so far --
        // mirror `symlink`'s own identical fallback: on a missing-component error, check whether
        // the lower layer has the parent directory, migrate the ancestor chain up to the upper
        // layer if so, and retry.
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

    fn symlink(
        &self,
        target: impl crate::path::Arg,
        linkpath: impl crate::path::Arg,
    ) -> Result<(), SymlinkError> {
        let linkpath = self.absolute_path(linkpath)?;
        // Fail if anything already exists at `linkpath`, in either layer -- matches Linux
        // `symlink(2)`'s `EEXIST`, and mirrors `open`'s `O_CREAT | O_EXCL` handling above.
        if self.file_status(linkpath.as_str()).is_ok() {
            return Err(SymlinkError::AlreadyExists);
        }
        // Symlink creation only ever targets the writable upper layer -- there is no
        // "copy-on-write" concept for creating a brand new path, unlike writing to an existing
        // lower-layer file. However, `linkpath`'s *parent* directory may only exist in the
        // read-only lower layer so far (e.g. a package extractor creating a brand new symlink
        // inside a directory that came from the read-only initial rootfs and has not yet been
        // touched on the upper layer) -- mirror `open`'s `O_CREAT` handling and `mkdir`'s
        // fallback: on a missing-component error, check whether the lower layer has the parent
        // directory, migrate the ancestor chain up to the upper layer if so, and retry.
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
        // Note: we grab the info from the relevant level and then immediately spit back the same,
        // essentially to ask the compiler to remind us we need to update this when we support
        // inodes and such.
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
        // Mirrors `file_status` above exactly, except the "not an already-open fd" fallback
        // calls `symlink_metadata` (not `file_status`) on each layer, so a final-component
        // symlink's own metadata is preserved instead of being transparently followed. An
        // already-open fd can never be a symlink itself (`open()` always resolves through any
        // final-component symlink to reach the fd it hands back), so that branch is unaffected
        // and stays on `fd_file_status` like `file_status` does.
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
        // Note: we grab the info and then immediately spit back the same, essentially to ask the
        // compiler to remind us we need to update this when we support inodes and such.
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
    // keys are normalized paths; directories do not have the final `/` (thus the root would be at
    // the empty-string key "")
    //
    // Invariant: this only stores lower+tombstone entries, no upper entries will show up here.
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
    // This file should be considered a purely upper-level file, independent of whether lower level file exists or not.
    Upper { fd: TypedFd<Upper> },
    // This file is a lower-level file and does NOT exist in the upper level file.
    Lower { fd: TypedFd<Lower> },
    // This file exists in the lower level, but as far as the layered architecture is concerned,
    // this is marked as deleted. RIP (x_x)
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
