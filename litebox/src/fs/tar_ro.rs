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
            tar_index: TarIndex::from_layers(alloc::vec![tar_data], inode_allocator),
        }
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
            tar_index: TarIndex::from_layers(layers, inode_allocator),
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

    /// Exposes a file's bytes as a `'static` slice for `try_allocate_cow_pages`. Only a
    /// `Cow::Borrowed` layer (a host-mmapped tar handed in uncopied) can yield one; `Cow::Owned`
    /// must keep returning `None`, and returning `None` for both costs every tar-backed exec the
    /// CoW-mmap fast path -- see gm mutable fs-tarro-static-backing-cow.
    fn get_static_backing_data(&self, h: &FileHandle) -> Option<&'static [u8]> {
        let idx = h.get_typed::<Self>().idx;
        let file = &self.tar_index.files[idx];
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
    /// Which layer's tar blob (index into `TarIndex::layers`) `data_range` refers into.
    layer_idx: usize,
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
    layers: Vec<alloc::borrow::Cow<'static, [u8]>>,
    files: Vec<IndexedFile>,
    dirs: Vec<IndexedDir>,
    symlinks: Vec<IndexedSymlink>,
}

/// A single (path, kind) entry discovered while scanning one layer's raw tar headers, still
/// tagged with its originating layer index so a later layer's whiteout can remove exactly the
/// entries an earlier layer contributed (and nothing from a still-later layer that re-created
/// the same path).
enum RawEntry {
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
    Whiteout { path: String },
    /// `.wh..wh..opq`: clear every entry earlier layers contributed under this entry's own
    /// parent directory (but not the directory itself), per the OCI opaque-whiteout spec.
    OpaqueWhiteout { parent: String },
    /// A POSIX hard link (`tar_no_std::TypeFlag::LINK`): `path` aliases whatever `link_target`
    /// resolves to once every layer is folded. Must be indexed (busybox's official image ships
    /// `bin/busybox` itself as a hard link) and must stay deferred -- see gm mutable
    /// fs-tarro-hardlink-busybox.
    HardLink {
        path: String,
        link_target: String,
    },
}

impl TarIndex {
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
        inode_allocator: &InodeAllocator,
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
                let (parent, basename) = path
                    .rsplit_once('/')
                    .unwrap_or(("", path.as_str()));
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
                        node_info: inode_allocator.next(),
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
                        node_info: inode_allocator.next(),
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
                _ => {
                    // Directories are implied by file/symlink paths below; device nodes and FIFOs
                    // are not needed for the base-image use case this backend supports.
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
        inode_allocator: InodeAllocator,
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
                &inode_allocator,
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
            node_info: inode_allocator.next(),
            children: HashMap::new(),
        }];
        let mut dirs_by_path: HashMap<String, usize> = [(String::new(), 0)].into_iter().collect();

        for (path, entry) in live {
            match entry {
                RawLiveEntry::File(file_idx) => {
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
                RawLiveEntry::Symlink(symlink_idx) => {
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
            layers,
            files,
            dirs,
            symlinks,
        }
    }

    fn file_data(&self, file_idx: usize) -> &[u8] {
        let file = &self.files[file_idx];
        &self.layers[file.layer_idx][file.data_range.clone()]
    }
}

/// The entry currently "live" (visible in the final merged tree) at a given path, tracked while
/// folding every layer's raw entries in bottom-to-top order. Directories themselves have no
/// entry here -- they're implied purely by the paths of the files/symlinks that survive the
/// fold, exactly as the pre-multi-layer single-tar builder already worked.
#[derive(Clone, Copy)]
enum RawLiveEntry {
    File(usize),
    Symlink(usize),
}

/// Remove `path` itself, plus every currently-live entry whose path is nested under it (i.e.
/// `path` was itself a directory in an earlier layer), from `live`. Used both by an exact-path
/// whiteout (`.wh.<name>`, which may target a whole directory subtree in an earlier layer) and
/// before inserting a fresh file/symlink at `path` (a later layer's file may replace what was
/// previously a directory at the same path, or vice versa).
fn remove_path_and_descendants(live: &mut alloc::collections::BTreeMap<String, RawLiveEntry>, path: &str) {
    live.remove(path);
    remove_descendants_of(live, path);
}

/// Remove every currently-live entry nested strictly under `parent` (not `parent` itself). Used
/// by opaque-whiteout handling, and as the subtree-removal half of
/// [`remove_path_and_descendants`].
fn remove_descendants_of(live: &mut alloc::collections::BTreeMap<String, RawLiveEntry>, parent: &str) {
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
    // An unparseable or out-of-range octal uid/gid degrades to 0 (root), never panics -- see gm
    // mutable fs-tarro-malformed-tar-field-tolerance.
    UserInfo {
        user: posix_header.uid.as_number().unwrap_or(0),
        group: posix_header.gid.as_number().unwrap_or(0),
    }
}
