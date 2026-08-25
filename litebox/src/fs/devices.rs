// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Unix-y devices [`super::backend::Backend`].
//!
//! Provides `{stdin,stdout,null,urandom,...}` entries, intended to be mounted at `/dev`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::LiteBox;
use crate::sync::RawSyncPrimitivesProvider;

use super::backend::{
    Backend, BackendHandles, DirHandle, FileHandle, PermissionCheck, Permissioned, SeekBehavior,
    WalkOutcome, WalkStopReason, WalkingDirHandle,
};
use super::errors::{
    ChmodError, ChownError, FileStatusError, MkdirError, OpenError, PathError, ReadDirError,
    ReadError, RmdirError, SetTimesError, TruncateError, UnlinkError, WalkError, WriteError,
};
use super::inode_allocator::InodeAllocator;
use super::{DirEntry, FileStatus, FileType, Mode, NodeInfo, OFlags, Timestamp, UserInfo};

/// Block size for stdio devices
const STDIO_BLOCK_SIZE: usize = 1024;
/// Block size for null device
const NULL_BLOCK_SIZE: usize = 0x1000;
/// Block size for /dev/urandom
const URANDOM_BLOCK_SIZE: usize = 0x1000;

/// Constant node information for all 3 stdio devices:
/// ```console
/// $ stat -L --format 'name=%-11n dev=%d ino=%i rdev=%r' /dev/stdin /dev/stdout /dev/stderr
/// name=/dev/stdin  dev=64 ino=9 rdev=34822
/// name=/dev/stdout dev=64 ino=9 rdev=34822
/// name=/dev/stderr dev=64 ino=9 rdev=34822
/// ```
// XXX(jayb): Should we be pulling the device names and such from the inode allocator?
const STDIO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 64,
    ino: 9,
    rdev: core::num::NonZeroUsize::new(34822),
};
/// Node info for /dev/null
const NULL_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 4,
    // major=1, minor=3
    rdev: core::num::NonZeroUsize::new(0x103),
};
/// Node info for /dev/urandom
const URANDOM_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 8,
    // major=1, minor=9
    rdev: core::num::NonZeroUsize::new(0x109),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Device {
    Stdin,
    Stdout,
    Stderr,
    Null,
    URandom,
}

impl Device {
    const ALL: &'static [(&'static str, Device)] = &[
        ("stdin", Device::Stdin),
        ("stdout", Device::Stdout),
        ("stderr", Device::Stderr),
        ("null", Device::Null),
        ("urandom", Device::URandom),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
    }

    fn file_status(self) -> FileStatus {
        match self {
            Device::Stdin | Device::Stdout | Device::Stderr => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR | Mode::WUSR | Mode::WGRP,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: STDIO_NODE_INFO,
                blksize: STDIO_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            Device::Null => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: NULL_NODE_INFO,
                blksize: NULL_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            Device::URandom => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: URANDOM_NODE_INFO,
                blksize: URANDOM_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
        }
    }
}

/// A [`super::backend::Backend`] that supports Unix-y devices.
pub struct Devices<Platform>
where
    Platform: RawSyncPrimitivesProvider
        + crate::platform::StdioProvider
        + crate::platform::CrngProvider
        + 'static,
{
    litebox: LiteBox<Platform>,
    /// Stable inode info for this backend's root directory.
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> Devices<Platform>
where
    Platform: RawSyncPrimitivesProvider
        + crate::platform::StdioProvider
        + crate::platform::CrngProvider
        + 'static,
{
    /// Construct a new `Devices` backend.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>, allocator: InodeAllocator) -> Self {
        let root_inode = allocator.next();
        Self {
            litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
        }
    }
}

/// Owned file handle; identifies which device backs this fd.
#[derive(Debug, Clone, Copy)]
pub struct DeviceFileHandle {
    device: Device,
}

/// Directory handle
// For devices, since no borrows are needed, we reuse this struct for both the walking handles as
// well as the dir handles.
#[derive(Debug, Clone, Copy)]
pub struct DeviceDirHandle;

impl<Platform> super::backend::private::Sealed for Devices<Platform> where
    Platform: RawSyncPrimitivesProvider
        + crate::platform::StdioProvider
        + crate::platform::CrngProvider
        + 'static
{
}

impl<Platform> BackendHandles for Devices<Platform>
where
    Platform: RawSyncPrimitivesProvider
        + crate::platform::StdioProvider
        + crate::platform::CrngProvider
        + 'static,
{
    type WalkingDirHandle<'a> = DeviceDirHandle;
    type FileHandle = DeviceFileHandle;
    type DirHandle = DeviceDirHandle;
}

impl<Platform> Backend for Devices<Platform>
where
    Platform: RawSyncPrimitivesProvider
        + crate::platform::StdioProvider
        + crate::platform::CrngProvider
        + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(DeviceDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        // Device files are final path targets, so directory walking must stop before them.
        if let Some(&component) = components.first() {
            if Device::from_name(component).is_some() {
                return Ok(WalkOutcome {
                    components: vec![],
                    last: WalkingDirHandle::from_typed::<Self>(from),
                    stop_reason: WalkStopReason::StoppedAtNonDirectory,
                });
            }
            return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
        }
        Ok(WalkOutcome {
            components: vec![],
            last: WalkingDirHandle::from_typed::<Self>(from),
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
        let _dir = dir.into_typed::<Self>();
        let device = Device::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        // `O_NONBLOCK` is accepted here without changing this backend's own `read`/`write`
        // (mirroring the `O_TRUNC` handling below, which is likewise accepted but not literally
        // honored by this backend). `Stdout`/`Stderr`/`Null`/`URandom` never block in the first
        // place, so there is nothing to honor for them. `Stdin` is the one device that can
        // genuinely block (`StdioProvider::read_from_stdin`) -- callers that need `O_NONBLOCK`
        // to actually take effect on a stdin read (e.g. `open("/dev/stdin", O_NONBLOCK)`, the
        // real-world case is libuv/Node putting a reopened stdin fd into non-blocking mode) get
        // it from the shim layer instead: `litebox_shim_linux::syscalls::file::do_read` consults
        // `StdioStatusFlags` metadata and the platform's `stdin_ready` probe to return `EAGAIN`
        // rather than blocking, for any fd tagged `StdioStream::Stdin` -- see
        // `insert_raw_file_fd_with_path`, which tags a freshly-(re)opened `/dev/stdin` with both
        // `StdioStream` and `StdioStatusFlags` metadata derived from these same `flags`. This
        // backend has no such per-fd status-flag storage of its own (`DeviceFileHandle` is a
        // stateless `Copy` type), so previously this `unimplemented!()`'d unconditionally instead
        // of ever reaching that shim-layer handling -- crashing the whole process on any
        // `open("/dev/stdin"|"/dev/stdout"|"/dev/stderr"|"/dev/urandom", O_NONBLOCK)`.

        if flags.contains(OFlags::TRUNC) {
            // Note: matching Linux behavior, this does not actually perform any truncation, and
            // instead, it is silently ignored if you attempt to truncate upon opening stdio.
            debug_assert!(matches!(
                self.truncate(
                    &FileHandle::from_typed::<Self>(DeviceFileHandle { device }),
                    0
                ),
                Err(TruncateError::IsTerminalDevice)
            ));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(DeviceFileHandle { device }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(Device::ALL
            .iter()
            .map(|(n, d)| DirEntry {
                name: String::from(*n),
                file_type: FileType::CharacterDevice,
                ino_info: Some(d.file_status().node_info),
            })
            .collect())
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], _offset: usize) -> Result<usize, ReadError> {
        let h = h.get_typed::<Self>();
        match h.device {
            Device::Stdin => self
                .litebox
                .x
                .platform
                .read_from_stdin(buf)
                .map_err(|e| match e {
                    crate::platform::StdioReadError::Closed => ReadError::Io,
                }),
            Device::Stdout | Device::Stderr => Err(ReadError::NotForReading),
            Device::Null => {
                // /dev/null read returns EOF
                Ok(0)
            }
            Device::URandom => {
                self.litebox.x.platform.fill_bytes_crng(buf);
                Ok(buf.len())
            }
        }
    }

    fn write(&self, h: &FileHandle, buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        let h = h.get_typed::<Self>();
        let stream = match h.device {
            Device::Stdin => return Err(WriteError::NotForWriting),
            Device::Stdout => crate::platform::StdioOutStream::Stdout,
            Device::Stderr => crate::platform::StdioOutStream::Stderr,
            Device::Null | Device::URandom => {
                // /dev/null discards data: report as if written fully
                //
                // Writing to /dev/random or /dev/urandom will update the entropy
                // pool with the data written, but this will not result in a higher
                // entropy count. This means that it will impact the contents read
                // from both files, but it will not make reads from /dev/random
                // faster. For simplicity, we just discard the data written to
                // /dev/urandom here.
                return Ok(buf.len());
            }
        };
        self.litebox
            .x
            .platform
            .write_to(stream, buf)
            .map_err(|e| match e {
                crate::platform::StdioWriteError::Closed => WriteError::Io,
            })
    }

    fn truncate(&self, _h: &FileHandle, _len: usize) -> Result<(), TruncateError> {
        Err(TruncateError::IsTerminalDevice)
    }

    fn seek_behavior(&self, h: &FileHandle) -> SeekBehavior {
        let h = h.get_typed::<Self>();
        match h.device {
            Device::Stdin | Device::Stdout | Device::Stderr => SeekBehavior::NonSeekable,
            Device::Null | Device::URandom => SeekBehavior::ZeroPosition,
        }
    }

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        Ok(h.get_typed::<Self>().device.file_status())
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let _h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info: self.root_inode.clone(),
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

/// Node info for `/dev/dri/card0` (major=226, the real Linux DRM primary-node major).
const DRI_CARD0_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 10,
    // major=226, minor=0
    rdev: core::num::NonZeroUsize::new(0xE200),
};
/// Node info for `/dev/dri/renderD128` (major=226, minor=128, the real Linux DRM
/// render-node convention -- render nodes start at minor 128).
const DRI_RENDERD128_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 11,
    // major=226, minor=128
    rdev: core::num::NonZeroUsize::new(0xE280),
};

/// A DRM device node -- `card0` (the control/modeset node) or `renderD128` (the
/// render-only node). Real DRM devices always ship at least the control node; a render
/// node is only meaningful once real GPU-accelerated rendering (as opposed to the
/// dumb-buffer path) is implemented, but is included now since userspace libraries
/// (`libdrm`) commonly probe for it and quietly skip it if absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriDevice {
    Card0,
    RenderD128,
}

impl DriDevice {
    const ALL: &'static [(&'static str, DriDevice)] =
        &[("card0", DriDevice::Card0), ("renderD128", DriDevice::RenderD128)];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
    }

    fn file_status(self) -> FileStatus {
        let node_info = match self {
            DriDevice::Card0 => DRI_CARD0_NODE_INFO,
            DriDevice::RenderD128 => DRI_RENDERD128_NODE_INFO,
        };
        FileStatus {
            file_type: FileType::CharacterDevice,
            // Real DRM nodes are `crw-rw----`, group `video` -- litebox's own guest
            // identity always runs as root (see `initialize_root_in_mem_layer`'s doc
            // comment elsewhere in this codebase), so group-readable is sufficient for
            // every guest process to open this node without needing a real group-membership
            // model.
            mode: Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP,
            size: 0,
            owner: UserInfo::ROOT,
            node_info,
            blksize: NULL_BLOCK_SIZE,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        }
    }
}

/// A [`super::backend::Backend`] exposing `/dev/dri/{card0,renderD128}` -- the DRM
/// device nodes a "dumb buffer" software display client opens to enumerate a virtual
/// display, allocate a pixel buffer, and page-flip it. Mounted as its own nested backend
/// at `/dev/dri` (see the composer's nested-mount support), separate from [`Devices`]
/// at `/dev`, since [`Devices`]' own `walk_directories` is a flat, single-level
/// namespace with no subdirectory support.
///
/// This backend only handles the filesystem-visible SHAPE of the device nodes (open,
/// stat, permissions, directory listing) -- the actual DRM ioctl protocol (buffer
/// allocation, mode-setting, page-flip) is handled by `litebox_shim_linux`'s
/// `DrmSubsystem`, reached once a guest has successfully `open()`ed one of these nodes,
/// mirroring how `Devices`' own stdio entries are thin filesystem shells around state
/// that actually lives in the shim layer.
pub struct DriDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> DriDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `DriDevices` backend.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>, allocator: InodeAllocator) -> Self {
        let root_inode = allocator.next();
        Self {
            _litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
        }
    }
}

/// Owned file handle; identifies which DRI device node backs this fd.
#[derive(Debug, Clone, Copy)]
pub struct DriDeviceFileHandle {
    device: DriDevice,
}

/// Directory handle, reused for both walking and owned dir handles (no borrows needed).
#[derive(Debug, Clone, Copy)]
pub struct DriDeviceDirHandle;

impl<Platform> super::backend::private::Sealed for DriDevices<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for DriDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = DriDeviceDirHandle;
    type FileHandle = DriDeviceFileHandle;
    type DirHandle = DriDeviceDirHandle;
}

impl<Platform> Backend for DriDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(DriDeviceDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            if DriDevice::from_name(component).is_some() {
                return Ok(WalkOutcome {
                    components: vec![],
                    last: WalkingDirHandle::from_typed::<Self>(from),
                    stop_reason: WalkStopReason::StoppedAtNonDirectory,
                });
            }
            return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
        }
        Ok(WalkOutcome {
            components: vec![],
            last: WalkingDirHandle::from_typed::<Self>(from),
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
        let _dir = dir.into_typed::<Self>();
        let device = DriDevice::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(DriDeviceFileHandle { device }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(DriDevice::ALL
            .iter()
            .map(|(n, d)| DirEntry {
                name: String::from(*n),
                file_type: FileType::CharacterDevice,
                ino_info: Some(d.file_status().node_info),
            })
            .collect())
    }

    fn read(&self, _h: &FileHandle, _buf: &mut [u8], _offset: usize) -> Result<usize, ReadError> {
        // Real Linux DRM device nodes DO support read() -- it delivers queued
        // DRM_EVENT_FLIP_COMPLETE/DRM_EVENT_VBLANK events (struct drm_event), not raw pixel
        // bytes. That event-delivery path isn't implemented yet (page-flip completion is a
        // stub in this pass -- see DrmSubsystem's own doc comment), so reads are rejected
        // outright for now rather than silently returning zero bytes as if no event were
        // ever pending, which would be a worse lie: a real client polling for flip
        // completion would spin forever instead of failing loudly.
        Err(ReadError::NotForReading)
    }

    fn write(&self, _h: &FileHandle, _buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        Err(WriteError::NotForWriting)
    }

    fn truncate(&self, _h: &FileHandle, _len: usize) -> Result<(), TruncateError> {
        Err(TruncateError::IsTerminalDevice)
    }

    fn seek_behavior(&self, _h: &FileHandle) -> SeekBehavior {
        SeekBehavior::NonSeekable
    }

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        Ok(h.get_typed::<Self>().device.file_status())
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let _h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info: self.root_inode.clone(),
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

/// Node info for `/dev/input/event0` (major=13 "Input core", minor=64 "First event
/// queue" -- both confirmed against the kernel's own
/// `Documentation/admin-guide/devices.txt` registry, not guessed).
const INPUT_EVENT0_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 12,
    // major=13, minor=64
    rdev: core::num::NonZeroUsize::new(0x0D40),
};

/// An evdev input device node -- only `event0` (one virtual keyboard+mouse device) is
/// exposed in this pass; a real system typically has one event node per physical input
/// device, but a single combined node is a real, valid evdev shape (e.g. a USB
/// keyboard-with-trackpad reports both `EV_KEY` and `EV_REL` on one node) and is
/// sufficient for a single virtual display with one virtual input source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDevice {
    Event0,
}

impl InputDevice {
    const ALL: &'static [(&'static str, InputDevice)] = &[("event0", InputDevice::Event0)];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
    }

    fn file_status(self) -> FileStatus {
        let InputDevice::Event0 = self;
        FileStatus {
            file_type: FileType::CharacterDevice,
            // Real evdev nodes are `crw-r-----`, group `input` -- same rationale as
            // `DriDevice::file_status`: litebox's guest identity is always root, so
            // group-readable is enough for every guest process to open this node.
            mode: Mode::RUSR | Mode::WUSR | Mode::RGRP,
            size: 0,
            owner: UserInfo::ROOT,
            node_info: INPUT_EVENT0_NODE_INFO,
            blksize: NULL_BLOCK_SIZE,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        }
    }
}

/// A [`super::backend::Backend`] exposing `/dev/input/event0` -- the evdev node a guest
/// keyboard/mouse-driven GUI toolkit reads raw `struct input_event` records from.
/// Mounted as its own nested backend at `/dev/input`, mirroring [`DriDevices`] at
/// `/dev/dri` (see that type's own doc comment for why a nested mount is needed instead
/// of adding directly to the flat, single-level [`Devices`] namespace).
///
/// This backend only handles the filesystem-visible SHAPE of the device node (open,
/// stat, permissions, directory listing) -- the actual evdev protocol (capability-query
/// ioctls, and the real `input_event` byte stream) is handled by `litebox_shim_linux`'s
/// `EvdevSubsystem`, reached once a guest has successfully `open()`ed this node,
/// mirroring how [`DriDevices`] hands off to `litebox_shim_linux`'s `DrmSubsystem`.
pub struct InputDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> InputDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `InputDevices` backend.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>, allocator: InodeAllocator) -> Self {
        let root_inode = allocator.next();
        Self {
            _litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
        }
    }
}

/// Owned file handle; identifies which input device node backs this fd (currently
/// always [`InputDevice::Event0`], kept as a field rather than a unit struct so a
/// second event node is a non-breaking addition later).
#[derive(Debug, Clone, Copy)]
pub struct InputDeviceFileHandle {
    device: InputDevice,
}

/// Directory handle, reused for both walking and owned dir handles (no borrows needed).
#[derive(Debug, Clone, Copy)]
pub struct InputDeviceDirHandle;

impl<Platform> super::backend::private::Sealed for InputDevices<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for InputDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = InputDeviceDirHandle;
    type FileHandle = InputDeviceFileHandle;
    type DirHandle = InputDeviceDirHandle;
}

impl<Platform> Backend for InputDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(InputDeviceDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            if InputDevice::from_name(component).is_some() {
                return Ok(WalkOutcome {
                    components: vec![],
                    last: WalkingDirHandle::from_typed::<Self>(from),
                    stop_reason: WalkStopReason::StoppedAtNonDirectory,
                });
            }
            return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
        }
        Ok(WalkOutcome {
            components: vec![],
            last: WalkingDirHandle::from_typed::<Self>(from),
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
        let _dir = dir.into_typed::<Self>();
        let device = InputDevice::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(InputDeviceFileHandle { device }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(InputDevice::ALL
            .iter()
            .map(|(n, d)| DirEntry {
                name: String::from(*n),
                file_type: FileType::CharacterDevice,
                ino_info: Some(d.file_status().node_info),
            })
            .collect())
    }

    fn read(&self, _h: &FileHandle, _buf: &mut [u8], _offset: usize) -> Result<usize, ReadError> {
        // Real evdev reads deliver queued `struct input_event` records, handled by
        // `litebox_shim_linux`'s `EvdevSubsystem` (reached once the guest has opened this
        // node) rather than this filesystem-shape-only backend -- see this type's own doc
        // comment. Rejecting outright here (rather than silently returning zero bytes) is
        // deliberate: `EvdevSubsystem` intercepts `read()` on this fd before this method is
        // ever reached in practice (mirroring `DriDevices::read`'s identical rationale), so
        // reaching this specific code path means something bypassed that interception.
        Err(ReadError::NotForReading)
    }

    fn write(&self, _h: &FileHandle, _buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        Err(WriteError::NotForWriting)
    }

    fn truncate(&self, _h: &FileHandle, _len: usize) -> Result<(), TruncateError> {
        Err(TruncateError::IsTerminalDevice)
    }

    fn seek_behavior(&self, _h: &FileHandle) -> SeekBehavior {
        SeekBehavior::NonSeekable
    }

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        Ok(h.get_typed::<Self>().device.file_status())
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let _h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info: self.root_inode.clone(),
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

