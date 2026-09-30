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
        Self::from_layers(alloc::vec![tar_data], inode_allocator)
    }

    /// Construct a tar backend from multiple OCI-style layer tars, applied bottom-to-top
    /// (`layers[0]` is the base layer, `layers[last]` the topmost). A later layer's whiteouts --
    /// `.wh.<name>` (delete one sibling entry) and `.wh..wh..opq` (clear every pre-existing entry
    /// under its own parent) -- are applied against everything earlier layers indexed.
    #[must_use]
    pub fn from_layers(
        layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
        inode_allocator: InodeAllocator,
    ) -> Self {
        Self {
            tar_index: TreeIndex::from_layers(layers)
                .flatten(&inode_allocator),
        }
    }

    /// The flat, position-independent form of this backend's directory index. Persist it and
    /// hand it back to [`Self::from_flat_index`] (typically as a read-only file mapping) so other
    /// processes look files up directly in the shared bytes instead of rebuilding a tree.
    #[must_use]
    pub fn flat_index(&self) -> &[u8] {
        &self.tar_index.flat
    }

    /// Adopt a [`Self::flat_index`] previously produced for the SAME `layers`. `None` when the
    /// bytes are not a well-formed flat index (see [`is_flat_index`]).
    #[must_use]
    pub fn from_flat_index(
        layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
        flat: alloc::borrow::Cow<'static, [u8]>,
        inode_allocator: InodeAllocator,
    ) -> Option<Self> {
        Some(Self {
            tar_index: TarIndex::from_flat(layers, flat, &inode_allocator)?,
        })
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
            let child = self
                .tar_index
                .child(current.idx, component)
                .ok_or(WalkError::PathError(PathError::NoSuchFileOrDirectory))?;
            let IndexedChild::Dir(child_idx) = child else {
                return Ok(super::backend::WalkOutcome {
                    components: walked_components,
                    last: WalkingDirHandle::from_typed::<Self>(current),
                    stop_reason: super::backend::WalkStopReason::StoppedAtNonDirectory,
                });
            };

            let child = self.tar_index.dir(child_idx);
            walked_components.push(super::backend::WalkedComponent {
                permissions: super::backend::PermissionCheck::ByResolver(
                    super::backend::PermissionInfo {
                        mode: child.mode.unwrap_or(DEFAULT_DIR_MODE),
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
        let child = self
            .tar_index
            .child(dir.idx, name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        let IndexedChild::File(file_idx) = child else {
            // A symlink reaching here means `O_NOFOLLOW`, where Linux returns ELOOP rather than
            // this generic error -- see gm mutable fs-tarro-nofollow-eloop-gap.
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
        let file = self.tar_index.file(file_idx);
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
        Ok(self
            .tar_index
            .children(handle.idx)
            .map(|(name, child)| {
                let (file_type, node_info) = match child {
                    IndexedChild::File(idx) => (
                        FileType::RegularFile,
                        self.tar_index.file(idx).node_info,
                    ),
                    IndexedChild::Dir(idx) => (
                        FileType::Directory,
                        self.tar_index.dir(idx).node_info,
                    ),
                    IndexedChild::Symlink(idx) => (
                        FileType::Symlink,
                        self.tar_index.symlink(idx).1,
                    ),
                };
                DirEntry {
                    name: String::from(name),
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

    /// Exposes a file's bytes as a `'static` slice for `try_allocate_cow_pages`. Only a
    /// `Cow::Borrowed` layer (a host-mmapped tar handed in uncopied) can yield one; `Cow::Owned`
    /// must keep returning `None`, and returning `None` for both costs every tar-backed exec the
    /// CoW-mmap fast path -- see gm mutable fs-tarro-static-backing-cow.
    fn get_static_backing_data(&self, h: &FileHandle) -> Option<&'static [u8]> {
        let idx = h.get_typed::<Self>().idx;
        let file = self.tar_index.file(idx);
        match &self.tar_index.layers[file.layer_idx] {
            alloc::borrow::Cow::Borrowed(data) => Some(&data[file.data_range.clone()]),
            alloc::borrow::Cow::Owned(_) => None,
        }
    }

    fn truncate(&self, _h: &FileHandle, _length: usize) -> Result<(), TruncateError> {
        Err(TruncateError::NotForWriting)
    }

    fn chmod(&self, _h: &FileHandle, _mode: super::Mode) -> Result<(), super::errors::ChmodError> {
        Err(super::errors::ChmodError::ReadOnlyFileSystem)
    }

    fn seek_behavior(&self, _h: &FileHandle) -> super::backend::SeekBehavior {
        super::backend::SeekBehavior::PositionBased
    }

    fn file_status(
        &self,
        h: &FileHandle,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        let file = self.tar_index.file(h.get_typed::<Self>().idx);
        Ok(super::FileStatus {
            nlink: 1,
            file_type: FileType::RegularFile,
            mode: file.mode,
            size: file.data_range.len(),
            owner: file.owner,
            node_info: file.node_info,
            blksize: BLOCK_SIZE,
            atime: Timestamp {
                sec: file.mtime,
                nsec: 0,
            },
            mtime: Timestamp {
                sec: file.mtime,
                nsec: 0,
            },
        })
    }

    fn dir_status(
        &self,
        h: &DirHandle,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        let dir = self.tar_index.dir(h.get_typed::<Self>().idx);
        Ok(super::FileStatus {
            nlink: 1,
            file_type: FileType::Directory,
            mode: dir.mode.unwrap_or(DEFAULT_DIR_MODE),
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: dir.owner.unwrap_or(DEFAULT_DIRECTORY_OWNER),
            node_info: dir.node_info,
            blksize: BLOCK_SIZE,
            atime: Timestamp {
                sec: dir.mtime,
                nsec: 0,
            },
            mtime: Timestamp {
                sec: dir.mtime,
                nsec: 0,
            },
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
        match self.tar_index.child(dir.idx, name) {
            Some(IndexedChild::Dir(_)) => Err(UnlinkError::IsADirectory),
            Some(IndexedChild::File(_) | IndexedChild::Symlink(_)) => {
                Err(UnlinkError::ReadOnlyFileSystem)
            }
            None => Err(PathError::NoSuchFileOrDirectory.into()),
        }
    }

    fn rmdir_at(&self, dir: DirHandle, name: &str) -> Result<(), RmdirError> {
        let dir = dir.into_typed::<Self>();
        match self.tar_index.child(dir.idx, name) {
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
        let child = self
            .tar_index
            .child(dir.idx, name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        match child {
            IndexedChild::Symlink(idx) => Ok(Some(String::from(self.tar_index.symlink(idx).0))),
            IndexedChild::File(_) | IndexedChild::Dir(_) => Ok(None),
        }
    }

    fn chmod_at(&self, dir: DirHandle, name: &str, _mode: Mode) -> Result<(), ChmodError> {
        let dir = dir.into_typed::<Self>();
        if self.tar_index.child(dir.idx, name).is_some() {
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
        if self.tar_index.child(dir.idx, name).is_some() {
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
        if self.tar_index.child(dir.idx, name).is_some() {
            Err(SetTimesError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory.into())
        }
    }
}

/// An empty tar file to support an empty file system.
pub const EMPTY_TAR_FILE: &[u8] = &[0u8; 10240];

struct IndexedFile {
    /// Which layer's tar blob (index into `TarIndex::layers`) `data_range` refers into.
    layer_idx: usize,
    data_range: Range<usize>,
    mode: Mode,
    owner: UserInfo,
    /// Modification time (seconds since the epoch) from the tar header; 0 when the index was
    /// rebuilt from merged live entries, which do not carry it.
    mtime: i64,
}

struct IndexedDir {
    owner: Option<UserInfo>,
    /// Modification time from the directory's own tar entry; 0 for implied directories.
    mtime: i64,
    /// Permission bits from the directory's own tar entry; `None` for implied directories.
    mode: Option<Mode>,
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
}

struct TreeIndex {
    layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
    files: Vec<IndexedFile>,
    dirs: Vec<IndexedDir>,
    symlinks: Vec<IndexedSymlink>,
}

const FLAT_MAGIC: [u8; 4] = *b"TFX1";
const FLAT_HEADER_LEN: usize = 64;
const FILE_RECORD_LEN: usize = 40;
const DIR_RECORD_LEN: usize = 32;
const SYMLINK_RECORD_LEN: usize = 16;
const CHILD_RECORD_LEN: usize = 16;
const CHILD_KIND_FILE: u32 = 0;
const CHILD_KIND_DIR: u32 = 1;
const CHILD_KIND_SYMLINK: u32 = 2;
const DIR_HAS_OWNER: u32 = 1;
const DIR_HAS_MODE: u32 = 2;

struct FileRecord {
    layer_idx: usize,
    data_range: Range<usize>,
    mode: Mode,
    owner: UserInfo,
    node_info: NodeInfo,
    mtime: i64,
}

struct DirRecord {
    owner: Option<UserInfo>,
    mtime: i64,
    mode: Option<Mode>,
    node_info: NodeInfo,
}

struct FlatHeader {
    file_count: usize,
    dir_count: usize,
    symlink_count: usize,
    files_off: usize,
    dirs_off: usize,
    symlinks_off: usize,
    children_off: usize,
    strings_off: usize,
}

impl FlatHeader {
    fn parse(flat: &[u8]) -> Option<Self> {
        let header = flat.get(..FLAT_HEADER_LEN)?;
        if header[..4] != FLAT_MAGIC {
            return None;
        }
        let word = |at: usize| -> usize {
            u32::from_le_bytes(header[at..at + 4].try_into().unwrap()) as usize
        };
        let long = |at: usize| -> Option<usize> {
            usize::try_from(u64::from_le_bytes(header[at..at + 8].try_into().unwrap())).ok()
        };
        let parsed = Self {
            file_count: word(4),
            dir_count: word(8),
            symlink_count: word(12),
            files_off: long(24)?,
            dirs_off: long(32)?,
            symlinks_off: long(40)?,
            children_off: long(48)?,
            strings_off: long(56)?,
        };
        let child_count = word(16);
        let fits = |off: usize, count: usize, record: usize| {
            count
                .checked_mul(record)
                .and_then(|n| n.checked_add(off))
                .is_some_and(|end| end <= flat.len())
        };
        (parsed.dir_count != 0
            && fits(parsed.files_off, parsed.file_count, FILE_RECORD_LEN)
            && fits(parsed.dirs_off, parsed.dir_count, DIR_RECORD_LEN)
            && fits(parsed.symlinks_off, parsed.symlink_count, SYMLINK_RECORD_LEN)
            && fits(parsed.children_off, child_count, CHILD_RECORD_LEN)
            && parsed.strings_off <= flat.len())
        .then_some(parsed)
    }
}

/// Whether `bytes` is a well-formed [`TarRo::flat_index`] image.
#[must_use]
pub fn is_flat_index(bytes: &[u8]) -> bool {
    FlatHeader::parse(bytes).is_some()
}

/// The read-only directory index, held as one flat byte image (fixed-size record tables plus a
/// string blob, children sorted by name) that is read in place. The image has no pointers, so it
/// can live in a file mapping shared by every process that serves the same layers.
struct TarIndex {
    layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
    flat: alloc::borrow::Cow<'static, [u8]>,
    file_count: usize,
    dir_count: usize,
    files_off: usize,
    dirs_off: usize,
    symlinks_off: usize,
    children_off: usize,
    strings_off: usize,
    device: usize,
    first_ino: usize,
}

impl TarIndex {
    fn from_flat(
        layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
        flat: alloc::borrow::Cow<'static, [u8]>,
        inode_allocator: &InodeAllocator,
    ) -> Option<Self> {
        let header = FlatHeader::parse(&flat)?;
        let (device, first_ino) = inode_allocator
            .reserve((header.dir_count + header.file_count + header.symlink_count) as u64);
        Some(Self {
            layers,
            flat,
            file_count: header.file_count,
            dir_count: header.dir_count,
            files_off: header.files_off,
            dirs_off: header.dirs_off,
            symlinks_off: header.symlinks_off,
            children_off: header.children_off,
            strings_off: header.strings_off,
            device,
            first_ino,
        })
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.flat[at..at + 4].try_into().unwrap())
    }

    fn u64_at(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.flat[at..at + 8].try_into().unwrap())
    }

    fn u16_at(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.flat[at..at + 2].try_into().unwrap())
    }

    fn string_at(&self, offset: u32, len: u32) -> &str {
        let start = self.strings_off + offset as usize;
        core::str::from_utf8(&self.flat[start..start + len as usize]).unwrap_or("")
    }

    fn node_info(&self, ordinal: usize) -> NodeInfo {
        NodeInfo {
            dev: self.device,
            ino: self.first_ino + ordinal,
            rdev: None,
        }
    }

    fn file(&self, idx: usize) -> FileRecord {
        let at = self.files_off + idx * FILE_RECORD_LEN;
        FileRecord {
            layer_idx: self.u32_at(at) as usize,
            mode: Mode::from_bits_truncate(self.u32_at(at + 4)),
            owner: UserInfo {
                user: self.u16_at(at + 8),
                group: self.u16_at(at + 10),
            },
            mtime: self.u64_at(at + 16) as i64,
            data_range: self.u64_at(at + 24) as usize..self.u64_at(at + 32) as usize,
            node_info: self.node_info(self.dir_count + idx),
        }
    }

    fn dir(&self, idx: usize) -> DirRecord {
        let at = self.dirs_off + idx * DIR_RECORD_LEN;
        let flags = self.u32_at(at + 4);
        DirRecord {
            owner: (flags & DIR_HAS_OWNER != 0).then(|| UserInfo {
                user: self.u16_at(at),
                group: self.u16_at(at + 2),
            }),
            mode: (flags & DIR_HAS_MODE != 0).then(|| Mode::from_bits_truncate(self.u32_at(at + 8))),
            mtime: self.u64_at(at + 24) as i64,
            node_info: self.node_info(idx),
        }
    }

    fn symlink(&self, idx: usize) -> (&str, NodeInfo) {
        let at = self.symlinks_off + idx * SYMLINK_RECORD_LEN;
        (
            self.string_at(self.u32_at(at), self.u32_at(at + 4)),
            self.node_info(self.dir_count + self.file_count + idx),
        )
    }

    fn child_record(&self, index: usize) -> (&str, IndexedChild) {
        let at = self.children_off + index * CHILD_RECORD_LEN;
        let name = self.string_at(self.u32_at(at), self.u32_at(at + 4));
        let target = self.u32_at(at + 12) as usize;
        let child = match self.u32_at(at + 8) {
            CHILD_KIND_FILE => IndexedChild::File(target),
            CHILD_KIND_DIR => IndexedChild::Dir(target),
            _ => IndexedChild::Symlink(target),
        };
        (name, child)
    }

    fn child_span(&self, dir: usize) -> Range<usize> {
        let at = self.dirs_off + dir * DIR_RECORD_LEN;
        let first = self.u32_at(at + 16) as usize;
        first..first + self.u32_at(at + 20) as usize
    }

    fn child(&self, dir: usize, name: &str) -> Option<IndexedChild> {
        let span = self.child_span(dir);
        let (mut low, mut high) = (span.start, span.end);
        while low < high {
            let mid = low + (high - low) / 2;
            let (candidate, child) = self.child_record(mid);
            match candidate.as_bytes().cmp(name.as_bytes()) {
                core::cmp::Ordering::Equal => return Some(child),
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
            }
        }
        None
    }

    fn children(&self, dir: usize) -> impl Iterator<Item = (&str, IndexedChild)> {
        self.child_span(dir).map(|index| self.child_record(index))
    }

    fn file_data(&self, file_idx: usize) -> &[u8] {
        let file = self.file(file_idx);
        &self.layers[file.layer_idx][file.data_range]
    }
}

impl TreeIndex {
    fn flatten(self, inode_allocator: &InodeAllocator) -> TarIndex {
        let Self {
            layers,
            files,
            dirs,
            symlinks,
        } = self;
        let mut file_order: Vec<usize> = Vec::new();
        let mut file_slot: HashMap<usize, u32> = HashMap::new();
        let mut symlink_order: Vec<usize> = Vec::new();
        let mut symlink_slot: HashMap<usize, u32> = HashMap::new();
        let mut strings: Vec<u8> = Vec::new();
        let mut children: Vec<u8> = Vec::new();
        let mut dir_records: Vec<u8> = Vec::with_capacity(dirs.len() * DIR_RECORD_LEN);
        let mut child_total = 0u32;
        let push_string = |strings: &mut Vec<u8>, text: &str| -> (u32, u32) {
            let offset = strings.len() as u32;
            strings.extend_from_slice(text.as_bytes());
            (offset, text.len() as u32)
        };
        for dir in &dirs {
            let mut names: Vec<(&String, IndexedChild)> =
                dir.children.iter().map(|(name, child)| (name, *child)).collect();
            names.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let first = child_total;
            for (name, child) in &names {
                let (name_off, name_len) = push_string(&mut strings, name);
                let (kind, target) = match *child {
                    IndexedChild::File(old) => {
                        let next = file_order.len() as u32;
                        let slot = *file_slot.entry(old).or_insert_with(|| {
                            file_order.push(old);
                            next
                        });
                        (CHILD_KIND_FILE, slot)
                    }
                    IndexedChild::Dir(idx) => (CHILD_KIND_DIR, idx as u32),
                    IndexedChild::Symlink(old) => {
                        let next = symlink_order.len() as u32;
                        let slot = *symlink_slot.entry(old).or_insert_with(|| {
                            symlink_order.push(old);
                            next
                        });
                        (CHILD_KIND_SYMLINK, slot)
                    }
                };
                children.extend_from_slice(&name_off.to_le_bytes());
                children.extend_from_slice(&name_len.to_le_bytes());
                children.extend_from_slice(&kind.to_le_bytes());
                children.extend_from_slice(&target.to_le_bytes());
            }
            child_total += names.len() as u32;
            let owner = dir.owner.unwrap_or(DEFAULT_DIRECTORY_OWNER);
            let mut flags = 0u32;
            if dir.owner.is_some() {
                flags |= DIR_HAS_OWNER;
            }
            if dir.mode.is_some() {
                flags |= DIR_HAS_MODE;
            }
            dir_records.extend_from_slice(&owner.user.to_le_bytes());
            dir_records.extend_from_slice(&owner.group.to_le_bytes());
            dir_records.extend_from_slice(&flags.to_le_bytes());
            dir_records.extend_from_slice(&dir.mode.unwrap_or(DEFAULT_DIR_MODE).bits().to_le_bytes());
            dir_records.extend_from_slice(&0u32.to_le_bytes());
            dir_records.extend_from_slice(&first.to_le_bytes());
            dir_records.extend_from_slice(&(names.len() as u32).to_le_bytes());
            dir_records.extend_from_slice(&dir.mtime.to_le_bytes());
        }
        let mut file_records: Vec<u8> = Vec::with_capacity(file_order.len() * FILE_RECORD_LEN);
        for &old in &file_order {
            let file = &files[old];
            file_records.extend_from_slice(&(file.layer_idx as u32).to_le_bytes());
            file_records.extend_from_slice(&file.mode.bits().to_le_bytes());
            file_records.extend_from_slice(&file.owner.user.to_le_bytes());
            file_records.extend_from_slice(&file.owner.group.to_le_bytes());
            file_records.extend_from_slice(&0u32.to_le_bytes());
            file_records.extend_from_slice(&file.mtime.to_le_bytes());
            file_records.extend_from_slice(&(file.data_range.start as u64).to_le_bytes());
            file_records.extend_from_slice(&(file.data_range.end as u64).to_le_bytes());
        }
        let mut symlink_records: Vec<u8> =
            Vec::with_capacity(symlink_order.len() * SYMLINK_RECORD_LEN);
        for &old in &symlink_order {
            let symlink = &symlinks[old];
            let (offset, len) = push_string(&mut strings, &symlink.target);
            symlink_records.extend_from_slice(&offset.to_le_bytes());
            symlink_records.extend_from_slice(&len.to_le_bytes());
            symlink_records.extend_from_slice(&symlink.owner.user.to_le_bytes());
            symlink_records.extend_from_slice(&symlink.owner.group.to_le_bytes());
            symlink_records.extend_from_slice(&0u32.to_le_bytes());
        }
        let files_off = FLAT_HEADER_LEN;
        let dirs_off = files_off + file_records.len();
        let symlinks_off = dirs_off + dir_records.len();
        let children_off = symlinks_off + symlink_records.len();
        let strings_off = children_off + children.len();
        let mut flat = Vec::with_capacity(strings_off + strings.len());
        flat.extend_from_slice(&FLAT_MAGIC);
        flat.extend_from_slice(&(file_order.len() as u32).to_le_bytes());
        flat.extend_from_slice(&(dirs.len() as u32).to_le_bytes());
        flat.extend_from_slice(&(symlink_order.len() as u32).to_le_bytes());
        flat.extend_from_slice(&child_total.to_le_bytes());
        flat.extend_from_slice(&0u32.to_le_bytes());
        for off in [files_off, dirs_off, symlinks_off, children_off, strings_off] {
            flat.extend_from_slice(&(off as u64).to_le_bytes());
        }
        flat.extend_from_slice(&file_records);
        flat.extend_from_slice(&dir_records);
        flat.extend_from_slice(&symlink_records);
        flat.extend_from_slice(&children);
        flat.extend_from_slice(&strings);
        TarIndex::from_flat(layers, alloc::borrow::Cow::Owned(flat), inode_allocator)
            .expect("a freshly flattened index is well formed")
    }
}

/// A single (path, kind) entry discovered while scanning one layer's raw tar headers, still
/// tagged with its originating layer index so a later layer's whiteout can remove exactly the
/// entries an earlier layer contributed (and nothing from a still-later layer that re-created
/// the same path).
enum RawEntry {
    /// An explicit directory entry (`DIRTYPE`), kept so empty directories exist.
    Dir {
        path: String,
        owner: UserInfo,
        mtime: i64,
        mode: Mode,
    },
    File {
        path: String,
        file_idx: usize,
    },
    Symlink {
        path: String,
        symlink_idx: usize,
    },
    /// `.wh.<name>`: delete the single sibling entry `<name>` (file, symlink, or whole directory
    /// subtree) contributed by any earlier layer. Never removes an entry a later layer in the
    /// same merge re-creates, since whiteouts are applied strictly in bottom-to-top layer order.
    Whiteout {
        path: String,
    },
    /// `.wh..wh..opq`: clear every entry earlier layers contributed under this entry's own
    /// parent directory (but not the directory itself), per the OCI opaque-whiteout spec.
    OpaqueWhiteout {
        parent: String,
    },
    /// A POSIX hard link (`tar_no_std::TypeFlag::LINK`): `path` aliases whatever `link_target`
    /// resolves to once every layer is folded. Must be indexed (busybox's official image ships
    /// `bin/busybox` itself as a hard link) and must stay deferred -- see gm mutable
    /// fs-tarro-hardlink-busybox.
    HardLink {
        path: String,
        link_target: String,
    },
}

impl TreeIndex {
    /// Parse one layer's raw tar bytes into a flat list of `RawEntry`, tagging every file/symlink
    /// with `layer_idx` so cross-layer merge order is preserved. Shared by both the single-tar
    /// legacy path and the multi-layer OCI path -- parsing itself has no whiteout awareness; that
    /// is applied afterward, once entries from every layer are in one bottom-to-top ordered list.
    fn parse_layer(
        data: &[u8],
        layer_idx: usize,
        files: &mut Vec<IndexedFile>,
        symlinks: &mut Vec<IndexedSymlink>,
        raw_entries: &mut Vec<RawEntry>,
    ) {
        // `tar_no_std::TarArchiveRef::entries()` skips every non-regular-file entry, so symlinks
        // must be indexed by walking the raw header blocks here -- see gm mutable
        // fs-tarro-raw-header-walk-apk (`apk` through Alpine's usrmerge symlinks).
        // `BLOCKSIZE` is `512` per the POSIX tar spec and is not expected to change.
        const BLOCKSIZE: usize = 512;

        // A PAX extended header (`XHDTYPE`, typeflag `'x'`) precedes the one entry it applies to
        // and carries overrides -- most commonly `path=<full name>` -- for any field the following
        // header's own fixed-width fields can't hold (GNU/POSIX tar's answer to the 100-byte `name`
        // field being too short for a deeply nested path, e.g. anything under a real
        // `node_modules/`). Carried across loop iterations and consumed by the very next non-`x`
        // entry, matching every other real tar reader's PAX semantics.
        let mut pending_pax_path: Option<String> = None;

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

            if typeflag == tar_no_std::TypeFlag::XHDTYPE {
                let payload_blocks = header.payload_block_count().unwrap_or(0);
                let content_start = block_index * BLOCKSIZE;
                let content_len = header.size.as_number::<usize>().unwrap_or(0);
                let content_end = content_start.saturating_add(content_len).min(data.len());
                block_index += payload_blocks;
                if let Ok(payload) = core::str::from_utf8(&data[content_start..content_end]) {
                    pending_pax_path =
                        parse_pax_path(payload).map(|p| normalize_tar_filename(p).into());
                }
                continue;
            }
            // A global extended header (`XGLTYPE`) applies to every subsequent entry in the
            // archive, not just the next one -- not needed by any base image this backend
            // supports; skip its payload without touching `pending_pax_path`.
            if typeflag == tar_no_std::TypeFlag::XGLTYPE {
                let payload_blocks = header.payload_block_count().unwrap_or(0);
                block_index += payload_blocks;
                continue;
            }

            let path = if let Some(pax_path) = pending_pax_path.take() {
                pax_path
            } else {
                let Ok(filename) = header.name.as_str() else {
                    continue;
                };
                // POSIX ustar splits an over-100-byte path across `name` and the 155-byte `prefix`
                // field; ignoring `prefix` silently corrupts the path to its basename -- see gm
                // mutable fs-tarro-ustar-prefix-join.
                match header.prefix.as_str() {
                    Ok(prefix) if !prefix.is_empty() => {
                        let mut joined = String::from(normalize_tar_filename(prefix));
                        joined.push('/');
                        joined.push_str(normalize_tar_filename(filename));
                        joined
                    }
                    _ => normalize_tar_filename(filename).into(),
                }
            };
            if path.is_empty() {
                continue;
            }

            // A whiteout marker ships as a zero-length regular-file entry, not a distinct tar type
            // flag, so `.wh.<name>` / `.wh..wh..opq` must be detected by basename -- see gm
            // mutable fs-tarro-whiteout-basename.
            {
                let (parent, basename) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                if basename == ".wh..wh..opq" {
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    block_index += payload_blocks;
                    raw_entries.push(RawEntry::OpaqueWhiteout {
                        parent: parent.into(),
                    });
                    continue;
                }
                if let Some(target_name) = basename.strip_prefix(".wh.") {
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    block_index += payload_blocks;
                    let whiteout_path = if parent.is_empty() {
                        String::from(target_name)
                    } else {
                        let mut joined = String::from(parent);
                        joined.push('/');
                        joined.push_str(target_name);
                        joined
                    };
                    raw_entries.push(RawEntry::Whiteout {
                        path: whiteout_path,
                    });
                    continue;
                }
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
                        layer_idx,
                        data_range: content_start..content_end,
                        // An unparseable octal mode degrades to rwxrwxrwx, never panics -- see gm
                        // mutable fs-tarro-malformed-tar-field-tolerance.
                        mode: header
                            .mode
                            .to_flags()
                            .map_or(DEFAULT_DIR_MODE, mode_of_modeflags),
                        owner: owner_from_posix_header(header),
                        mtime: tar_mtime_seconds(header),
                    });
                    raw_entries.push(RawEntry::File { path, file_idx });
                }
                tar_no_std::TypeFlag::SYMTYPE => {
                    let Ok(target) = header.linkname.as_str() else {
                        continue;
                    };
                    let symlink_idx = symlinks.len();
                    symlinks.push(IndexedSymlink {
                        target: target.into(),
                        owner: owner_from_posix_header(header),
                    });
                    raw_entries.push(RawEntry::Symlink { path, symlink_idx });
                }
                tar_no_std::TypeFlag::LINK => {
                    let Ok(link_target) = header.linkname.as_str() else {
                        continue;
                    };
                    raw_entries.push(RawEntry::HardLink {
                        path,
                        link_target: normalize_tar_filename(link_target).into(),
                    });
                }
                tar_no_std::TypeFlag::DIRTYPE => {
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    block_index += payload_blocks;
                    raw_entries.push(RawEntry::Dir {
                        path: path.trim_end_matches('/').into(),
                        owner: owner_from_posix_header(header),
                        mtime: tar_mtime_seconds(header),
                        mode: header
                            .mode
                            .to_flags()
                            .map_or(DEFAULT_DIR_MODE, mode_of_modeflags),
                    });
                }
                _ => {
                    // Device nodes and FIFOs are not needed for the base-image use case this
                    // backend supports.
                    let payload_blocks = header.payload_block_count().unwrap_or(0);
                    block_index += payload_blocks;
                }
            }
        }
    }

    /// Build an index from one or more OCI-style layer tars, applied bottom-to-top. `layers[0]`
    /// is the base layer; each subsequent layer's whiteout/opaque-whiteout entries remove
    /// entries contributed by any strictly-earlier layer (never a later one, since layers are
    /// folded in order) before that layer's own real files/symlinks are added.
    fn from_layers(
        layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
    ) -> Self {
        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        let mut raw_entries: Vec<RawEntry> = Vec::new();

        for (layer_idx, layer) in layers.iter().enumerate() {
            Self::parse_layer(
                layer.as_ref(),
                layer_idx,
                &mut files,
                &mut symlinks,
                &mut raw_entries,
            );
        }

        // Fold `raw_entries` (already in bottom-to-top, then within-layer tar order) into a
        // path -> latest-contributing-entry map. A whiteout removes whatever the map currently
        // holds for its target path (and, for a directory target, every path nested under it); an
        // opaque whiteout removes everything currently nested under its parent. Because entries
        // are folded strictly in layer order, a later layer's real file always naturally
        // overwrites (not merely un-deletes) whatever an earlier layer's whiteout removed, and a
        // whiteout can never remove something a *later* layer goes on to (re-)create.
        let mut live: alloc::collections::BTreeMap<String, RawLiveEntry> =
            alloc::collections::BTreeMap::new();
        // Hard links resolve against `live` only once every layer is folded -- see gm mutable
        // fs-tarro-hardlink-busybox.
        let mut deferred_hardlinks: Vec<(String, String)> = Vec::new();

        for raw_entry in raw_entries {
            match raw_entry {
                RawEntry::Dir {
                    path,
                    owner,
                    mtime,
                    mode,
                } => {
                    if path.is_empty() {
                        continue;
                    }
                    // Only this node is replaced: earlier layers' children stay.
                    if !matches!(live.get(path.as_str()), Some(RawLiveEntry::Dir(..))) {
                        live.remove(path.as_str());
                    }
                    live.insert(path, RawLiveEntry::Dir(owner, mtime, mode));
                }
                RawEntry::File { path, file_idx } => {
                    remove_path_and_descendants(&mut live, &path);
                    live.insert(path, RawLiveEntry::File(file_idx));
                }
                RawEntry::Symlink { path, symlink_idx } => {
                    remove_path_and_descendants(&mut live, &path);
                    live.insert(path, RawLiveEntry::Symlink(symlink_idx));
                }
                RawEntry::Whiteout { path } => {
                    remove_path_and_descendants(&mut live, &path);
                }
                RawEntry::OpaqueWhiteout { parent } => {
                    remove_descendants_of(&mut live, &parent);
                }
                RawEntry::HardLink { path, link_target } => {
                    remove_path_and_descendants(&mut live, &path);
                    deferred_hardlinks.push((path, link_target));
                }
            }
        }
        for (path, link_target) in deferred_hardlinks {
            if let Some(&resolved) = live.get(link_target.as_str()) {
                live.insert(path, resolved);
            }
            // An unresolvable hard-link target is dropped, not an error -- see gm mutable
            // fs-tarro-hardlink-busybox.
        }

        let mut dirs = alloc::vec![IndexedDir {
            owner: None,
            mtime: 0,
            mode: None,
            children: HashMap::new(),
        }];
        let mut dirs_by_path: HashMap<String, usize> = [(String::new(), 0)].into_iter().collect();

        for (path, entry) in live {
            match entry {
                RawLiveEntry::Dir(owner, mtime, mode) => {
                    let mut probe = path.clone();
                    probe.push_str("/x");
                    ensure_ancestors(
                        &mut dirs,
                        &mut dirs_by_path,
                        &probe,
                        owner,
                    );
                    if let Some(&idx) = dirs_by_path.get(path.as_str()) {
                        dirs[idx].owner = Some(owner);
                        dirs[idx].mtime = mtime;
                        dirs[idx].mode = Some(mode);
                    }
                }
                RawLiveEntry::File(file_idx) => {
                    let owner = files[file_idx].owner;
                    let (parent_dir_idx, name) = ensure_ancestors(
                        &mut dirs,
                        &mut dirs_by_path,
                        &path,
                        owner,
                    );
                    dirs[parent_dir_idx]
                        .children
                        .insert(name, IndexedChild::File(file_idx));
                }
                RawLiveEntry::Symlink(symlink_idx) => {
                    let owner = symlinks[symlink_idx].owner;
                    let (parent_dir_idx, name) = ensure_ancestors(
                        &mut dirs,
                        &mut dirs_by_path,
                        &path,
                        owner,
                    );
                    dirs[parent_dir_idx]
                        .children
                        .insert(name, IndexedChild::Symlink(symlink_idx));
                }
            }
        }

        Self {
            layers,
            files,
            dirs,
            symlinks,
        }
    }
}

/// The entry currently "live" (visible in the final merged tree) at a given path, tracked while
/// folding every layer's raw entries in bottom-to-top order. Directories themselves have no
/// entry here -- they're implied purely by the paths of the files/symlinks that survive the
/// fold, exactly as the pre-multi-layer single-tar builder already worked.
#[derive(Clone, Copy)]
enum RawLiveEntry {
    Dir(UserInfo, i64, Mode),
    File(usize),
    Symlink(usize),
}

/// Remove `path` itself, plus every currently-live entry whose path is nested under it (i.e.
/// `path` was itself a directory in an earlier layer), from `live`. Used both by an exact-path
/// whiteout (`.wh.<name>`, which may target a whole directory subtree in an earlier layer) and
/// before inserting a fresh file/symlink at `path` (a later layer's file may replace what was
/// previously a directory at the same path, or vice versa).
fn remove_path_and_descendants(
    live: &mut alloc::collections::BTreeMap<String, RawLiveEntry>,
    path: &str,
) {
    live.remove(path);
    remove_descendants_of(live, path);
}

/// Remove every currently-live entry nested strictly under `parent` (not `parent` itself). Used
/// by opaque-whiteout handling, and as the subtree-removal half of
/// [`remove_path_and_descendants`].
fn remove_descendants_of(
    live: &mut alloc::collections::BTreeMap<String, RawLiveEntry>,
    parent: &str,
) {
    if parent.is_empty() {
        // An empty parent means "everything" would match `starts_with("")` unconditionally --
        // only reachable via a root-level opaque whiteout, which legitimately does mean "clear
        // the entire index built so far".
        live.clear();
        return;
    }
    // Must stay a range query: the `retain` scan it replaced was O(entries^2) and cost 14 s to
    // index a 2.5 GB rootfs. The `["parent/", "parent0")` bound relies on `BTreeMap` byte
    // ordering. See gm mutable fs-tarro-subtree-range-query-perf.
    let start = {
        let mut p = String::from(parent);
        p.push('/');
        p
    };
    let end = {
        let mut p = String::from(parent);
        p.push('0');
        p
    };
    let doomed: Vec<String> = live
        .range::<str, _>((
            core::ops::Bound::Included(start.as_str()),
            core::ops::Bound::Excluded(end.as_str()),
        ))
        .map(|(path, _)| path.clone())
        .collect();
    for path in doomed {
        live.remove(&path);
    }
}

/// Extract the `path` record's value from a PAX extended header payload (POSIX.1-2001 `pax`
/// format: a sequence of `"<length> <keyword>=<value>\n"` records, `<length>` counting itself).
/// Ignores every other keyword (`mtime`, `uid`, `linkpath`, ...) -- only the long-name override is
/// needed by this backend's read-only, metadata-light use case.
fn parse_pax_path(payload: &str) -> Option<&str> {
    let mut rest = payload;
    while !rest.is_empty() {
        let (len_str, after_len) = rest.split_once(' ')?;
        let len: usize = len_str.parse().ok()?;
        if len == 0 || len > rest.len() {
            return None;
        }
        let record = &rest[..len];
        let body = &record[len_str.len() + 1..];
        let body = body.strip_suffix('\n').unwrap_or(body);
        if let Some(value) = body.strip_prefix("path=") {
            return Some(value);
        }
        rest = &rest[len..];
        let _ = after_len;
    }
    None
}

/// Strip the `./` prefix from tar filenames if present.
///
/// This is helpful for tar files that have been created via `tar cvf foo.tar .`
fn normalize_tar_filename(filename: &str) -> &str {
    filename.strip_prefix("./").unwrap_or(filename)
}

/// Ensure every ancestor directory of `path` exists in `dirs`, returning the immediate parent's
/// index and the final path component's name. Shared by both file and symlink tar entries when
/// building the index in [`TarIndex::from_layers`].
fn ensure_ancestors(
    dirs: &mut Vec<IndexedDir>,
    dirs_by_path: &mut HashMap<String, usize>,
    path: &str,
    owner: UserInfo,
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
                mtime: 0,
                mode: None,
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

/// The header's modification time in seconds. `tar_no_std` declares this field decimal, but ustar
/// stores every numeric field in octal, so it is parsed here from the raw field text.
fn tar_mtime_seconds(header: &tar_no_std::PosixHeader) -> i64 {
    header
        .mtime
        .as_inner()
        .as_str_until_first_space()
        .ok()
        .and_then(|t| i64::from_str_radix(t.trim_matches('\0'), 8).ok())
        .unwrap_or(0)
}

fn owner_from_posix_header(posix_header: &tar_no_std::PosixHeader) -> UserInfo {
    // An unparseable or out-of-range octal uid/gid degrades to 0 (root), never panics -- see gm
    // mutable fs-tarro-malformed-tar-field-tolerance.
    UserInfo {
        user: posix_header.uid.as_number().unwrap_or(0),
        group: posix_header.gid.as_number().unwrap_or(0),
    }
}
