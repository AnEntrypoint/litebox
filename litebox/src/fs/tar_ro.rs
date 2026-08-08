// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A read-only tar-backed file system.
//!
//! ```txt
//!                  __
//!                 / /
//!                / /
//!               / /
//!     ================
//!     |       / /    |
//!     |______/_/_____|
//!     \              /
//!      |            |
//!      |            |
//!      \            /
//!       |          |
//!       |  O  O  O |
//!        \O O O O /
//!        | O O O O|
//!        |________|
//!
//! Taro Milk Tea, Tapioca Bubbles, 50% Sugar, No Ice.
//! ```

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use hashbrown::HashMap;

use crate::fs::{DirEntry, FileType};

use super::{
    Mode, NodeInfo, OFlags, Timestamp, UserInfo,
    backend::{DirHandle, FileHandle, WalkingDirHandle},
    errors::{
        ChmodError, ChownError, MkdirError, OpenError, PathError, ReadDirError, ReadError,
        RmdirError, SetTimesError, TruncateError, UnlinkError, WalkError, WriteError,
    },
    inode_allocator::InodeAllocator,
};

/// Block size for file system I/O operations
// TODO(jayb): Determine appropriate block size
const BLOCK_SIZE: usize = 0;

/// A [`super::backend::Backend`] that stores all files in-memory, via a read-only `.tar` file.
pub struct TarRo {
    tar_index: TarIndex,
}

impl TarRo {
    /// Construct a tar backend using a caller-provided inode allocator.
    #[must_use]
    pub fn new(
        tar_data: alloc::borrow::Cow<'static, [u8]>,
        inode_allocator: InodeAllocator,
    ) -> Self {
        Self {
            tar_index: TarIndex::new(tar_data, inode_allocator),
        }
    }
}

impl super::backend::private::Sealed for TarRo {}

/// Directory handle
#[derive(Clone)]
pub struct TarRoDirHandle {
    idx: usize,
}
/// File handle
#[derive(Clone)]
pub struct TarRoFileHandle {
    idx: usize,
}
impl super::backend::BackendHandles for TarRo {
    type WalkingDirHandle<'a> = TarRoDirHandle;
    type FileHandle = TarRoFileHandle;
    type DirHandle = TarRoDirHandle;
}

impl super::backend::Backend for TarRo {
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(TarRoDirHandle { idx: 0 })
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<super::backend::WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let mut current = from.into_typed::<Self>();
        let mut walked_components = Vec::with_capacity(components.len());
        for component in components {
            let child = self.tar_index.dirs[current.idx]
                .children
                .get(*component)
                .ok_or(WalkError::PathError(PathError::NoSuchFileOrDirectory))?;
            let IndexedChild::Dir(child_idx) = *child else {
                return Ok(super::backend::WalkOutcome {
                    components: walked_components,
                    last: WalkingDirHandle::from_typed::<Self>(current),
                    stop_reason: super::backend::WalkStopReason::StoppedAtNonDirectory,
                });
            };

            let child = &self.tar_index.dirs[child_idx];
            walked_components.push(super::backend::WalkedComponent {
                permissions: super::backend::PermissionCheck::ByResolver(
                    super::backend::PermissionInfo {
                        mode: DEFAULT_DIR_MODE,
                        owner: child.owner.unwrap_or(DEFAULT_DIRECTORY_OWNER),
                    },
                ),
            });
            current = TarRoDirHandle { idx: child_idx };
        }
        Ok(super::backend::WalkOutcome {
            components: walked_components,
            last: WalkingDirHandle::from_typed::<Self>(current),
            stop_reason: super::backend::WalkStopReason::CompleteDirectory,
        })
    }

    fn owned_dir_at(
        &self,
        dir: WalkingDirHandle<'_>,
        flags: OFlags,
    ) -> Result<DirHandle, OpenError> {
        if flags.intersects(OFlags::CREAT | OFlags::TRUNC | OFlags::WRONLY | OFlags::RDWR) {
            return Err(OpenError::ReadOnlyFileSystem);
        }
        Ok(DirHandle::from_typed::<Self>(dir.into_typed::<Self>()))
    }

    fn walking_dir_at<'a>(&'a self, dir: &DirHandle) -> Option<WalkingDirHandle<'a>> {
        Some(WalkingDirHandle::from_typed::<Self>(
            dir.get_typed::<Self>().clone(),
        ))
    }

    fn open_file_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
        flags: OFlags,
    ) -> Result<super::backend::Permissioned<FileHandle>, OpenError> {
        let dir = dir.into_typed::<Self>();
        let child = self.tar_index.dirs[dir.idx]
            .children
            .get(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        let IndexedChild::File(file_idx) = *child else {
            // Either a directory (attempted to `open()` it as a file, without `O_DIRECTORY`) or a
            // symlink (the resolver is responsible for following final-component symlinks before
            // calling here; reaching this with `O_NOFOLLOW` on a symlink should surface as ELOOP
            // rather than this generic error, but no caller currently does that against this
            // backend).
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        };
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        if !(flags.contains(OFlags::CREAT) && flags.contains(OFlags::EXCL))
            && (flags.contains(OFlags::CREAT)
                || flags.contains(OFlags::TRUNC)
                || flags.contains(OFlags::WRONLY)
                || flags.contains(OFlags::RDWR))
        {
            return Err(OpenError::ReadOnlyFileSystem);
        }
        let file = &self.tar_index.files[file_idx];
        Ok(super::backend::Permissioned {
            item: FileHandle::from_typed::<Self>(TarRoFileHandle { idx: file_idx }),
            permissions: super::backend::PermissionCheck::ByResolver(
                super::backend::PermissionInfo {
                    mode: file.mode,
                    owner: file.owner,
                },
            ),
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let handle = handle.into_typed::<Self>();
        Ok(self.tar_index.dirs[handle.idx]
            .children
            .iter()
            .map(|(name, child)| {
                let (file_type, node_info) = match *child {
                    IndexedChild::File(idx) => (
                        FileType::RegularFile,
                        self.tar_index.files[idx].node_info.clone(),
                    ),
                    IndexedChild::Dir(idx) => (
                        FileType::Directory,
                        self.tar_index.dirs[idx].node_info.clone(),
                    ),
                    IndexedChild::Symlink(idx) => (
                        FileType::Symlink,
                        self.tar_index.symlinks[idx].node_info.clone(),
                    ),
                };
                DirEntry {
                    name: name.clone(),
                    file_type,
                    ino_info: Some(node_info),
                }
            })
            .collect())
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let file = self.tar_index.file_data(h.get_typed::<Self>().idx);
        let start = offset.min(file.len());
        let end = offset.checked_add(buf.len()).unwrap().min(file.len());
        debug_assert!(start <= end);
        let len = end - start;
        buf[..len].copy_from_slice(&file[start..end]);
        Ok(len)
    }

    fn write(&self, _h: &FileHandle, _buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        Err(WriteError::NotForWriting)
    }

    fn truncate(&self, _h: &FileHandle, _length: usize) -> Result<(), TruncateError> {
        Err(TruncateError::NotForWriting)
    }

    fn seek_behavior(&self, _h: &FileHandle) -> super::backend::SeekBehavior {
        super::backend::SeekBehavior::PositionBased
    }

    fn file_status(
        &self,
        h: &FileHandle,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        let file = &self.tar_index.files[h.get_typed::<Self>().idx];
        Ok(super::FileStatus {
            file_type: FileType::RegularFile,
            mode: file.mode,
            size: file.data_range.len(),
            owner: file.owner,
            node_info: file.node_info.clone(),
            blksize: BLOCK_SIZE,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(
        &self,
        h: &DirHandle,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        let dir = &self.tar_index.dirs[h.get_typed::<Self>().idx];
        Ok(super::FileStatus {
            file_type: FileType::Directory,
            mode: DEFAULT_DIR_MODE,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: dir.owner.unwrap_or(DEFAULT_DIRECTORY_OWNER),
            node_info: dir.node_info.clone(),
            blksize: BLOCK_SIZE,
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

    fn unlink_at(&self, dir: DirHandle, name: &str) -> Result<(), UnlinkError> {
        let dir = dir.into_typed::<Self>();
        match self.tar_index.dirs[dir.idx].children.get(name) {
            Some(IndexedChild::Dir(_)) => Err(UnlinkError::IsADirectory),
            Some(IndexedChild::File(_) | IndexedChild::Symlink(_)) => {
                Err(UnlinkError::ReadOnlyFileSystem)
            }
            None => Err(PathError::NoSuchFileOrDirectory.into()),
        }
    }

    fn rmdir_at(&self, dir: DirHandle, name: &str) -> Result<(), RmdirError> {
        let dir = dir.into_typed::<Self>();
        match self.tar_index.dirs[dir.idx].children.get(name) {
            Some(IndexedChild::Dir(_)) => Err(RmdirError::ReadOnlyFileSystem),
            Some(IndexedChild::File(_) | IndexedChild::Symlink(_)) => {
                Err(RmdirError::NotADirectory)
            }
            None => Err(PathError::NoSuchFileOrDirectory.into()),
        }
    }

    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let dir = dir.into_typed::<Self>();
        let child = self.tar_index.dirs[dir.idx]
            .children
            .get(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        match *child {
            IndexedChild::Symlink(idx) => Ok(Some(self.tar_index.symlinks[idx].target.clone())),
            IndexedChild::File(_) | IndexedChild::Dir(_) => Ok(None),
        }
    }

    fn chmod_at(&self, dir: DirHandle, name: &str, _mode: Mode) -> Result<(), ChmodError> {
        let dir = dir.into_typed::<Self>();
        if self.tar_index.dirs[dir.idx].children.contains_key(name) {
            Err(ChmodError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory.into())
        }
    }

    fn chown_at(
        &self,
        dir: DirHandle,
        name: &str,
        _user: Option<u16>,
        _group: Option<u16>,
    ) -> Result<(), ChownError> {
        let dir = dir.into_typed::<Self>();
        if self.tar_index.dirs[dir.idx].children.contains_key(name) {
            Err(ChownError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory.into())
        }
    }

    fn set_times_at(
        &self,
        dir: DirHandle,
        name: &str,
        _atime: Option<Timestamp>,
        _mtime: Option<Timestamp>,
    ) -> Result<(), SetTimesError> {
        let dir = dir.into_typed::<Self>();
        if self.tar_index.dirs[dir.idx].children.contains_key(name) {
            Err(SetTimesError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory.into())
        }
    }
}

/// An empty tar file to support an empty file system.
pub const EMPTY_TAR_FILE: &[u8] = &[0u8; 10240];

struct IndexedFile {
    data_range: Range<usize>,
    mode: Mode,
    owner: UserInfo,
    node_info: NodeInfo,
}

struct IndexedDir {
    owner: Option<UserInfo>,
    node_info: NodeInfo,
    children: HashMap<String, IndexedChild>,
}

#[derive(Clone, Copy)]
enum IndexedChild {
    File(usize),
    Dir(usize),
    Symlink(usize),
}

struct IndexedSymlink {
    target: String,
    owner: UserInfo,
    node_info: NodeInfo,
}

struct TarIndex {
    tar_data: alloc::borrow::Cow<'static, [u8]>,
    files: Vec<IndexedFile>,
    dirs: Vec<IndexedDir>,
    symlinks: Vec<IndexedSymlink>,
}

/// A single (path, kind) entry discovered while scanning the raw tar headers.
enum RawEntry {
    File { path: String, file_idx: usize },
    Symlink { path: String, symlink_idx: usize },
}

impl TarIndex {
    fn new(tar_data: alloc::borrow::Cow<'static, [u8]>, inode_allocator: InodeAllocator) -> Self {
        // `tar_no_std::TarArchiveRef::entries()` silently *skips* every non-regular-file entry
        // (directories, symlinks, hardlinks, ...) -- see that crate's `ArchiveEntryIterator::next`,
        // which loops past any header whose `TypeFlag::is_regular_file()` is false. That means a
        // symlink shipped in the base rootfs tar (e.g. Alpine's usrmerge `usr/lib -> lib` or
        // `lib -> usr/lib` compat symlinks) is invisible to this filesystem entirely: not indexed
        // as a file, not as a directory, not as anything -- any path walk through it fails with
        // `NoSuchFileOrDirectory`, which is exactly the failure `apk` hits extracting a package
        // whose payload is written through such a symlinked directory.
        //
        // To index symlinks too, we walk the raw 512-byte header blocks ourselves (mirroring what
        // `tar_no_std`'s internal `ArchiveHeaderIterator` does, since that type isn't constructible
        // outside the crate) using the fully-`pub` `PosixHeader`/`TypeFlag` types this crate
        // exposes. `BLOCKSIZE` itself is `512` per the POSIX tar spec (`tar_no_std`'s own private
        // constant of the same value); it is not expected to ever change.
        const BLOCKSIZE: usize = 512;

        let data = tar_data.as_ref();

        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        let mut raw_entries: Vec<RawEntry> = Vec::new();

        let mut block_index = 0usize;
        let total_blocks = data.len() / BLOCKSIZE;
        while block_index < total_blocks {
            // SAFETY: `PosixHeader` is `#[repr(C, packed)]` and exactly `BLOCKSIZE` bytes; the loop
            // guard above ensures a full block is available at this offset within `data`.
            let header = unsafe {
                data.as_ptr()
                    .add(block_index * BLOCKSIZE)
                    .cast::<tar_no_std::PosixHeader>()
                    .as_ref()
                    .unwrap()
            };
            if header.is_zero_block() {
                // One (or, at true end-of-archive, two) all-zero blocks terminate the archive.
                break;
            }
            block_index += 1;

            let Ok(typeflag) = header.typeflag.try_to_type_flag() else {
                continue;
            };
            let Ok(filename) = header.name.as_str() else {
                continue;
            };
            let path = normalize_tar_filename(filename);
            if path.is_empty() {
                continue;
            }

            match typeflag {
                tar_no_std::TypeFlag::REGTYPE | tar_no_std::TypeFlag::AREGTYPE => {
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    let content_start = block_index * BLOCKSIZE;
                    let content_len = header.size.as_number::<usize>().unwrap_or(0);
                    let content_end = content_start.checked_add(content_len).unwrap();
                    block_index += payload_blocks;

                    let file_idx = files.len();
                    files.push(IndexedFile {
                        data_range: content_start..content_end,
                        mode: mode_of_modeflags(header.mode.to_flags().unwrap()),
                        owner: owner_from_posix_header(header),
                        node_info: inode_allocator.next(),
                    });
                    raw_entries.push(RawEntry::File {
                        path: path.into(),
                        file_idx,
                    });
                }
                tar_no_std::TypeFlag::SYMTYPE => {
                    let Ok(target) = header.linkname.as_str() else {
                        continue;
                    };
                    let symlink_idx = symlinks.len();
                    symlinks.push(IndexedSymlink {
                        target: target.into(),
                        owner: owner_from_posix_header(header),
                        node_info: inode_allocator.next(),
                    });
                    raw_entries.push(RawEntry::Symlink {
                        path: path.into(),
                        symlink_idx,
                    });
                }
                _ => {
                    // Directories are implied by file/symlink paths below; hardlinks, device nodes,
                    // and FIFOs are not needed for the base-image use case this backend supports.
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    block_index += payload_blocks;
                }
            }
        }

        let mut dirs = alloc::vec![IndexedDir {
            owner: None,
            node_info: inode_allocator.next(),
            children: HashMap::new(),
        }];
        let mut dirs_by_path: HashMap<String, usize> = [(String::new(), 0)].into_iter().collect();

        for raw_entry in raw_entries {
            match raw_entry {
                RawEntry::File { path, file_idx } => {
                    let owner = files[file_idx].owner;
                    let (parent_dir_idx, name) = ensure_ancestors(
                        &mut dirs,
                        &mut dirs_by_path,
                        &path,
                        owner,
                        &inode_allocator,
                    );
                    dirs[parent_dir_idx]
                        .children
                        .insert(name, IndexedChild::File(file_idx));
                }
                RawEntry::Symlink { path, symlink_idx } => {
                    let owner = symlinks[symlink_idx].owner;
                    let (parent_dir_idx, name) = ensure_ancestors(
                        &mut dirs,
                        &mut dirs_by_path,
                        &path,
                        owner,
                        &inode_allocator,
                    );
                    dirs[parent_dir_idx]
                        .children
                        .insert(name, IndexedChild::Symlink(symlink_idx));
                }
            }
        }

        Self {
            tar_data,
            files,
            dirs,
            symlinks,
        }
    }

    fn file_data(&self, file_idx: usize) -> &[u8] {
        let range = self.files[file_idx].data_range.clone();
        &self.tar_data[range]
    }
}

/// Strip the `./` prefix from tar filenames if present.
///
/// This is helpful for tar files that have been created via `tar cvf foo.tar .`
fn normalize_tar_filename(filename: &str) -> &str {
    filename.strip_prefix("./").unwrap_or(filename)
}

/// Ensure every ancestor directory of `path` exists in `dirs`, returning the immediate parent's
/// index and the final path component's name. Shared by both file and symlink tar entries when
/// building the index in [`TarIndex::new`].
fn ensure_ancestors(
    dirs: &mut Vec<IndexedDir>,
    dirs_by_path: &mut HashMap<String, usize>,
    path: &str,
    owner: UserInfo,
    inode_allocator: &InodeAllocator,
) -> (usize, String) {
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let (name, parent_components) = components.split_last().expect("non-empty path");

    let mut parent = String::new();
    let mut parent_dir_idx = 0;
    for component in parent_components {
        dirs[parent_dir_idx].owner.get_or_insert(owner);
        if parent.is_empty() {
            parent.push_str(component);
        } else {
            parent.push('/');
            parent.push_str(component);
        }
        let child_dir_idx = *dirs_by_path.entry(parent.clone()).or_insert_with(|| {
            dirs.push(IndexedDir {
                owner: Some(owner),
                node_info: inode_allocator.next(),
                children: HashMap::new(),
            });
            dirs.len() - 1
        });
        dirs[parent_dir_idx]
            .children
            .entry((*component).into())
            .or_insert(IndexedChild::Dir(child_dir_idx));
        dirs[child_dir_idx].owner.get_or_insert(owner);
        parent_dir_idx = child_dir_idx;
    }
    (parent_dir_idx, (*name).into())
}

const DEFAULT_DIR_MODE: Mode =
    Mode::from_bits(Mode::RWXU.bits() | Mode::RWXG.bits() | Mode::RWXO.bits()).unwrap();

const DEFAULT_DIRECTORY_OWNER: UserInfo = UserInfo {
    user: 1000,
    group: 1000,
};

fn mode_of_modeflags(perms: tar_no_std::ModeFlags) -> Mode {
    use tar_no_std::ModeFlags;
    let mut mode = Mode::empty();
    mode.set(Mode::RUSR, perms.contains(ModeFlags::OwnerRead));
    mode.set(Mode::WUSR, perms.contains(ModeFlags::OwnerWrite));
    mode.set(Mode::XUSR, perms.contains(ModeFlags::OwnerExec));
    mode.set(Mode::RGRP, perms.contains(ModeFlags::GroupRead));
    mode.set(Mode::WGRP, perms.contains(ModeFlags::GroupWrite));
    mode.set(Mode::XGRP, perms.contains(ModeFlags::GroupExec));
    mode.set(Mode::ROTH, perms.contains(ModeFlags::OthersRead));
    mode.set(Mode::WOTH, perms.contains(ModeFlags::OthersWrite));
    mode.set(Mode::XOTH, perms.contains(ModeFlags::OthersExec));
    mode
}

fn owner_from_posix_header(posix_header: &tar_no_std::PosixHeader) -> UserInfo {
    UserInfo {
        user: posix_header.uid.as_number().unwrap(),
        group: posix_header.gid.as_number().unwrap(),
    }
}
