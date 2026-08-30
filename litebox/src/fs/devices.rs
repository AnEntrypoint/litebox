// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Unix-y devices [`super::backend::Backend`].
//!
//! Provides `{stdin,stdout,null,urandom,...}` entries, intended to be mounted at `/dev`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

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
/// Node info for `/dev/tty0` (major=4, minor=0 -- the real Linux "current VT" console device;
/// see `Documentation/admin-guide/devices.txt`). `seatd`'s `seat_update_vt` opens exactly this
/// path and calls `VT_GETSTATE` on it to learn which numbered VT (`/dev/tty<N>`) is active.
const TTY0_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 21,
    // major=4, minor=0
    rdev: core::num::NonZeroUsize::new(0x0400),
};
/// Node info for `/dev/tty1` (major=4, minor=1 -- the first real numbered VT). This virtual
/// device always reports VT 1 as active (see [`super::super::syscalls::vt`]'s doc comment, or
/// this module's own [`Device::Tty0`]/[`Device::Tty1`] pairing), so `/dev/tty1` is the one
/// `seatd`'s `vt_open`/`vt_close` subsequently open once `VT_GETSTATE` on `/dev/tty0` names it.
const TTY1_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 22,
    // major=4, minor=1
    rdev: core::num::NonZeroUsize::new(0x0401),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Device {
    Stdin,
    Stdout,
    Stderr,
    Null,
    URandom,
    /// `/dev/tty0` -- the "currently active VT" console device (see [`TTY0_NODE_INFO`]).
    Tty0,
    /// `/dev/tty1` -- the one numbered VT this virtual device ever reports as active (see
    /// [`TTY1_NODE_INFO`]).
    Tty1,
}

impl Device {
    const ALL: &'static [(&'static str, Device)] = &[
        ("stdin", Device::Stdin),
        ("stdout", Device::Stdout),
        ("stderr", Device::Stderr),
        ("null", Device::Null),
        ("urandom", Device::URandom),
        ("tty0", Device::Tty0),
        ("tty1", Device::Tty1),
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
            Device::Tty0 | Device::Tty1 => FileStatus {
                file_type: FileType::CharacterDevice,
                // Real VT device nodes are `crw--w----`, group `tty` -- litebox's guest
                // identity always runs as root (see `DriDevice::file_status`'s identical
                // rationale), so group-writable is sufficient for every guest process.
                mode: Mode::RUSR | Mode::WUSR | Mode::WGRP,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: if self == Device::Tty0 {
                    TTY0_NODE_INFO
                } else {
                    TTY1_NODE_INFO
                },
                blksize: STDIO_BLOCK_SIZE,
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
            // Real Linux VT devices support read()/write() (raw keyboard/console I/O); no
            // caller on this codebase's actual VT usage path (`seatd`'s open + VT_GETSTATE/
            // VT_SETMODE/KDSETMODE/KDSKBMODE ioctls, see `litebox_shim_linux`'s VT subsystem)
            // ever reads or writes these nodes, so this deliberately rejects rather than
            // silently returning zero bytes -- matching `DriDevices::read`'s identical
            // "fail loud, not silently wrong" rationale for a device-node shape this backend
            // does not implement the full byte-stream protocol for.
            Device::Tty0 | Device::Tty1 => Err(ReadError::NotForReading),
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
            // See `Device::Tty0 | Device::Tty1`'s identical rationale in `read` above.
            Device::Tty0 | Device::Tty1 => return Err(WriteError::NotForWriting),
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

    fn chmod(&self, _h: &FileHandle, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
    }

    fn seek_behavior(&self, h: &FileHandle) -> SeekBehavior {
        let h = h.get_typed::<Self>();
        match h.device {
            Device::Stdin | Device::Stdout | Device::Stderr | Device::Tty0 | Device::Tty1 => {
                SeekBehavior::NonSeekable
            }
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

    fn chmod(&self, _h: &FileHandle, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
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

    fn chmod(&self, _h: &FileHandle, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
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


/// A leaf file inside `/sys/class/drm/{card0,renderD128}/` -- the minimal set a real
/// `libudev`/`libdrm` device-enumeration walk actually reads:
/// `udev_enumerate_scan_devices()` opens `uevent` (to populate `udev_device` properties)
/// and reads the `dev`/`subsystem` attributes via `sysattr` lookups that fall back to
/// reading these same files directly when no udev database is present (as is always the
/// case here, since litebox has no `udevd`/`/run/udev` database at all). This matches the
/// real, stable shape every Linux kernel has shipped under `/sys/class/drm/cardN/` since
/// DRM's sysfs class was added -- not a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysDrmFile {
    /// `MAJOR=`/`MINOR=`/`DEVNAME=`/`SUBSYSTEM=` key=value lines, the same content the
    /// kernel writes to the real uevent file and that `udevadm`/`libudev` parse to
    /// populate a `udev_device`'s properties without needing a running `udevd`.
    Uevent,
    /// `MAJOR:MINOR` (e.g. `226:0`), the standard sysfs device-node attribute.
    Dev,
    /// Symlink to the (synthetic) `drm` subsystem directory -- `libudev` reads this
    /// link's target basename to populate `udev_device_get_subsystem()`.
    Subsystem,
    /// `<name>/device/uevent` -- distinct from [`SysDrmFile::Uevent`] (which lives at
    /// `<name>/uevent`, one level up): real libdrm's `drmGetDevice2()` (via
    /// `drm_platform_device_alloc`/`drm_device_get_bustype`) reads the *device's own*
    /// uevent file (not the DRM node's) to parse `DRIVER=`/`OF_*` key=value lines that
    /// identify which kernel driver bound to the physical device -- confirmed live, a
    /// failed `openat` on this exact path (`fd=None`) immediately precedes
    /// `types/wlr_drm.c:217]"drmGetDevice2 failed"` once the shallower `device`/
    /// `device/drm`/`device/subsystem` paths above it already resolve. Only reachable via
    /// [`SysDrmDirHandle::DeviceOf`] (never listed in [`SysDrmFile::ALL`], which is scoped
    /// to the real `<name>/` directory's own leaf files) -- kept as a `SysDrmFile` variant
    /// purely to reuse the existing `SysDrmFileHandle { device, file }` read/status
    /// plumbing rather than inventing a parallel handle shape for one file.
    DeviceUevent,
}

impl SysDrmFile {
    const ALL: &'static [(&'static str, SysDrmFile)] = &[
        ("uevent", SysDrmFile::Uevent),
        ("dev", SysDrmFile::Dev),
        ("subsystem", SysDrmFile::Subsystem),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
    }
}

/// Node info for the `/sys/class/drm/card0` directory itself (distinct from
/// `/dev/dri/card0`'s own [`DRI_CARD0_NODE_INFO`] -- sysfs directories and the device
/// nodes they describe are always separate inodes on real Linux too).
const SYS_DRM_CARD0_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 13,
    rdev: None,
};
/// Node info for the `/sys/class/drm/renderD128` directory itself.
const SYS_DRM_RENDERD128_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 14,
    rdev: None,
};

/// Node info for the synthetic `/sys/class/drm/card0/device` directory (see
/// [`SysDrmDirHandle::DeviceOf`] doc comment for why this exists).
const SYS_DRM_CARD0_DEVICE_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 28,
    rdev: None,
};
/// Node info for the synthetic `/sys/class/drm/renderD128/device` directory.
const SYS_DRM_RENDERD128_DEVICE_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 29,
    rdev: None,
};
/// Node info for the synthetic `/sys/class/drm/card0/device/drm` directory (see
/// [`SysDrmDirHandle::DeviceDrmOf`] doc comment for why this exists).
const SYS_DRM_CARD0_DEVICE_DRM_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 30,
    rdev: None,
};
/// Node info for the synthetic `/sys/class/drm/renderD128/device/drm` directory.
const SYS_DRM_RENDERD128_DEVICE_DRM_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 31,
    rdev: None,
};

impl DriDevice {
    /// The `MAJOR`/`MINOR`/`DEVNAME` values this device reports under
    /// `/sys/class/drm/<name>/`, reusing the exact same major/minor numbers already
    /// established for the real `/dev/dri/<name>` node so the two stay consistent.
    fn major_minor_devname(self) -> (u32, u32, &'static str) {
        match self {
            DriDevice::Card0 => (226, 0, "dri/card0"),
            DriDevice::RenderD128 => (226, 128, "dri/renderD128"),
        }
    }

    fn sys_dir_node_info(self) -> NodeInfo {
        match self {
            DriDevice::Card0 => SYS_DRM_CARD0_DIR_NODE_INFO,
            DriDevice::RenderD128 => SYS_DRM_RENDERD128_DIR_NODE_INFO,
        }
    }

    /// Node info for this device's synthetic `<name>/device` directory.
    fn sys_device_dir_node_info(self) -> NodeInfo {
        match self {
            DriDevice::Card0 => SYS_DRM_CARD0_DEVICE_DIR_NODE_INFO,
            DriDevice::RenderD128 => SYS_DRM_RENDERD128_DEVICE_DIR_NODE_INFO,
        }
    }

    /// Node info for this device's synthetic `<name>/device/drm` directory.
    fn sys_device_drm_dir_node_info(self) -> NodeInfo {
        match self {
            DriDevice::Card0 => SYS_DRM_CARD0_DEVICE_DRM_DIR_NODE_INFO,
            DriDevice::RenderD128 => SYS_DRM_RENDERD128_DEVICE_DRM_DIR_NODE_INFO,
        }
    }
}

/// A [`super::backend::Backend`] exposing the minimal `/sys/class/drm/{card0,renderD128}/`
/// subtree a real `libudev`-based DRM client (e.g. `weston`'s `drm-backend.so`) needs to
/// enumerate litebox's one emulated DRM device. This is deliberately NOT a general
/// procfs/sysfs emulation -- only the exact files real `udev_enumerate_scan_devices()` +
/// `udev_device_new_from_syspath()` calls read (`uevent`, `dev`, `subsystem`) are served,
/// for exactly the two DRM nodes [`DriDevices`] already exposes at `/dev/dri`. Mounted at
/// `/sys/class/drm`; the composer's virtual-directory auto-synthesis (see
/// `super::composer::ComposerBuilder::build`) creates the `/sys` and `/sys/class` ancestor
/// directories automatically, so this backend only needs to handle its own two-level
/// subtree (`card0`/`renderD128`, each containing `uevent`/`dev`/`subsystem`).
///
/// Also serves a third, synthetic level beneath each device: `<name>/device/drm/<name>`.
/// On real hardware, `/sys/class/drm/cardN/device` is a symlink to the card's parent PCI
/// device directory, which itself has its own `drm/` subdirectory listing every DRM node
/// sharing that PCI device (`cardN`, `renderDxxx`) -- i.e. the path loops back around to a
/// sibling of where it started, via the device's real bus topology. `libdrm`'s
/// `drmGetDeviceNameFromFd2()` (called by wlroots' DRM backend, used by `labwc` -- distinct
/// from weston's own DRM backend, which never walks this deep) `stat`s exactly this
/// `device/drm` sub-path while resolving a DRM fd back to its sysfs device name, and fails
/// hard (`ENOENT` -> "Failed to create DRM backend") if it is missing. litebox's virtual
/// DRM device has no real PCI parent to model, so `<name>/device` and `<name>/device/drm`
/// are synthesized as self-referencing directories: `<name>/device/drm/card0` and
/// `<name>/device/drm/renderD128` both resolve straight back to the real, pre-existing
/// `/sys/class/drm/card0` and `/sys/class/drm/renderD128` directories this whole tree
/// originates from (see [`SysDrmDirHandle::DeviceOf`]/[`SysDrmDirHandle::DeviceDrmOf`]).
pub struct SysClassDrm<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> SysClassDrm<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `SysClassDrm` backend.
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

/// Directory handle: the backend's mount root (`/sys/class/drm` itself), inside one
/// specific device's subdirectory (`/sys/class/drm/<name>`), inside that device's
/// synthetic `device` subdirectory (`/sys/class/drm/<name>/device`), or inside that
/// synthetic subdirectory's own `drm` subdirectory (`/sys/class/drm/<name>/device/drm`) --
/// see the doc comment on [`SysClassDrm`] for why the latter two exist and what real
/// sysfs shape they emulate.
#[derive(Debug, Clone, Copy)]
pub enum SysDrmDirHandle {
    Root,
    Device(DriDevice),
    /// `/sys/class/drm/<name>/device` -- the `DriDevice` is the device this synthetic
    /// directory hangs off of (i.e. whose `device` component was walked), not a target.
    DeviceOf(DriDevice),
    /// `/sys/class/drm/<name>/device/drm` -- same `DriDevice` semantics as `DeviceOf`.
    DeviceDrmOf(DriDevice),
}

/// Owned file handle; identifies which device's which sysfs attribute file backs this fd.
#[derive(Debug, Clone, Copy)]
pub struct SysDrmFileHandle {
    device: DriDevice,
    file: SysDrmFile,
}

impl<Platform> super::backend::private::Sealed for SysClassDrm<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for SysClassDrm<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = SysDrmDirHandle;
    type FileHandle = SysDrmFileHandle;
    type DirHandle = SysDrmDirHandle;
}

impl<Platform> Backend for SysClassDrm<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Root)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        match from {
            SysDrmDirHandle::Root => {
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Root),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                let Some(device) = DriDevice::from_name(component) else {
                    return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
                };
                // Walked one real directory level (`card0`/`renderD128`) -- the resolver
                // uses `components.len()` both to check per-level permissions and, via
                // `walk_path_following_symlinks`, to know how many of the caller's path
                // components were consumed as directories, so this MUST be populated
                // (unlike the flat `DriDevices`/`Devices` backends, which never walk past
                // their mount root and so correctly leave this empty).
                let walked = vec![WalkedComponent {
                    permissions: PermissionCheck::ByBackend,
                }];
                if components.len() == 1 {
                    return Ok(WalkOutcome {
                        components: walked,
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(
                            device,
                        )),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                }
                // Second component: must name one of this device's leaf files (stop here,
                // a leaf file is never a directory, leaving the resolver/caller to resolve
                // the final component itself, matching `TarRo`'s own convention), OR name
                // the synthetic `device` subdirectory, in which case walking continues.
                if components.len() == 2 && SysDrmFile::from_name(components[1]).is_some() {
                    return Ok(WalkOutcome {
                        components: walked,
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(
                            device,
                        )),
                        stop_reason: WalkStopReason::StoppedAtNonDirectory,
                    });
                }
                if components[1] == "device" {
                    let mut outcome = self.walk_directories(
                        WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::DeviceOf(device)),
                        &components[2..],
                    )?;
                    // Prepend BOTH components this arm itself consumed -- the
                    // `card0`/`renderD128` component (`walked`, already built above) AND
                    // the `device` component itself (the delegated call above only knows
                    // about components past `device`, so it never counts `device` in its
                    // own returned `components`). The composer's own walk-length invariant
                    // requires the returned `components` count to match the total path
                    // components consumed when `stop_reason` is `CompleteDirectory`
                    // (`composer.rs` asserts `walked_len == prefix_len`), so undercounting
                    // here would trip that assertion one level up.
                    let mut components_out = walked;
                    components_out.push(WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    });
                    components_out.append(&mut outcome.components);
                    return Ok(WalkOutcome {
                        components: components_out,
                        last: outcome.last,
                        stop_reason: outcome.stop_reason,
                    });
                }
                Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
            }
            SysDrmDirHandle::Device(device) => {
                // Re-entering with a handle already inside a device directory (e.g. via
                // `walking_dir_at` after `openat(dirfd, ...)`): behave identically to the
                // fresh-root walk above, just without re-consuming the device-name
                // component (it was already consumed to produce this handle).
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(
                            device,
                        )),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                if SysDrmFile::from_name(component).is_some() {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(
                            device,
                        )),
                        stop_reason: WalkStopReason::StoppedAtNonDirectory,
                    });
                }
                if component == "device" {
                    let mut outcome = self.walk_directories(
                        WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::DeviceOf(device)),
                        &components[1..],
                    )?;
                    // Count the `device` component itself -- see the identical comment in
                    // the `Root` arm's `"device"` branch above for why this is required.
                    let mut components_out = vec![WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    }];
                    components_out.append(&mut outcome.components);
                    return Ok(WalkOutcome {
                        components: components_out,
                        last: outcome.last,
                        stop_reason: outcome.stop_reason,
                    });
                }
                Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
            }
            SysDrmDirHandle::DeviceOf(device) => {
                // Inside the synthetic `<name>/device` directory: only `drm` exists here
                // (see `SysClassDrm`'s doc comment for why), and walking into it continues
                // one more synthetic level.
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::DeviceOf(
                            device,
                        )),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                if component == "drm" {
                    let walked = vec![WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    }];
                    if components.len() == 1 {
                        return Ok(WalkOutcome {
                            components: walked,
                            last: WalkingDirHandle::from_typed::<Self>(
                                SysDrmDirHandle::DeviceDrmOf(device),
                            ),
                            stop_reason: WalkStopReason::CompleteDirectory,
                        });
                    }
                    let mut outcome = self.walk_directories(
                        WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::DeviceDrmOf(
                            device,
                        )),
                        &components[1..],
                    )?;
                    // Prepend the `drm` component this arm consumed -- same walk-length
                    // invariant as the `Root`/`"device"` case above.
                    let mut components_out = walked;
                    components_out.append(&mut outcome.components);
                    return Ok(WalkOutcome {
                        components: components_out,
                        last: outcome.last,
                        stop_reason: outcome.stop_reason,
                    });
                }
                // `<name>/device/subsystem` -- same rationale as `Device(_)`'s own
                // `subsystem` leaf (see `read_link_at`'s doc comment): real wlroots
                // (`types/wlr_drm.c`'s `drmGetDevice2()`, via libdrm's
                // `drm_device_get_subsystem_type`) reads this symlink to classify the
                // *device's* bus (not the DRM node's own class, which is what the
                // shallower `<name>/subsystem` link resolves) -- confirmed live, a
                // `readlinkat` on this exact path immediately precedes
                // `types/wlr_drm.c:217]"drmGetDevice2 failed"` when unhandled. This is a
                // leaf (non-directory) stop, not a further walkable directory.
                if component == "subsystem" || component == "uevent" {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::DeviceOf(
                            device,
                        )),
                        stop_reason: WalkStopReason::StoppedAtNonDirectory,
                    });
                }
                Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
            }
            SysDrmDirHandle::DeviceDrmOf(device) => {
                // Inside the synthetic `<name>/device/drm` directory: entries here are
                // `card0`/`renderD128`, each resolving straight back to the real, existing
                // `/sys/class/drm/<name>` directory (this is the self-referencing loop the
                // real PCI-topology-based sysfs shape produces on real hardware -- see
                // `SysClassDrm`'s doc comment).
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(
                            SysDrmDirHandle::DeviceDrmOf(device),
                        ),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                let Some(target) = DriDevice::from_name(component) else {
                    return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
                };
                let walked = vec![WalkedComponent {
                    permissions: PermissionCheck::ByBackend,
                }];
                if components.len() == 1 {
                    return Ok(WalkOutcome {
                        components: walked,
                        last: WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(
                            target,
                        )),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                }
                // Beyond this point (e.g. `.../drm/card0/uevent`), delegate straight into
                // the real `Device(target)` walk logic -- looping back is exactly the
                // point, so no separate handling is needed past here. Same walk-length
                // invariant as above: prepend the `card0`/`renderD128` component this arm
                // consumed to whatever the delegated call reports.
                let mut outcome = self.walk_directories(
                    WalkingDirHandle::from_typed::<Self>(SysDrmDirHandle::Device(target)),
                    &components[1..],
                )?;
                let mut components_out = walked;
                components_out.append(&mut outcome.components);
                Ok(WalkOutcome {
                    components: components_out,
                    last: outcome.last,
                    stop_reason: outcome.stop_reason,
                })
            }
        }
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
        // `device/drm` is a synthetic directory with no leaf files of its own (only
        // further directory entries, handled by `walk_directories`/`list_dir_at`), so
        // opening a plain file inside it is always ENOENT. `device` itself has exactly
        // one real leaf file (`uevent`, see [`SysDrmFile::DeviceUevent`]'s doc comment) --
        // `subsystem` is a symlink, opened via `read_link_at` instead, never through here.
        let (device, file) = match dir {
            SysDrmDirHandle::Device(device) => {
                let file = SysDrmFile::from_name(name)
                    .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
                (device, file)
            }
            SysDrmDirHandle::DeviceOf(device) if name == "uevent" => {
                (device, SysDrmFile::DeviceUevent)
            }
            _ => return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory)),
        };

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(SysDrmFileHandle { device, file }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let handle = handle.into_typed::<Self>();
        match handle {
            SysDrmDirHandle::Root => Ok(DriDevice::ALL
                .iter()
                .map(|(n, d)| DirEntry {
                    name: String::from(*n),
                    file_type: FileType::Directory,
                    ino_info: Some(d.sys_dir_node_info()),
                })
                .collect()),
            SysDrmDirHandle::Device(_) => Ok(SysDrmFile::ALL
                .iter()
                .map(|(n, f)| DirEntry {
                    name: String::from(*n),
                    file_type: if matches!(f, SysDrmFile::Subsystem) {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    },
                    ino_info: None,
                })
                .collect()),
            SysDrmDirHandle::DeviceOf(device) => Ok(vec![
                DirEntry {
                    name: String::from("drm"),
                    file_type: FileType::Directory,
                    ino_info: Some(device.sys_device_drm_dir_node_info()),
                },
                DirEntry {
                    name: String::from("subsystem"),
                    file_type: FileType::Symlink,
                    ino_info: None,
                },
                DirEntry {
                    name: String::from("uevent"),
                    file_type: FileType::RegularFile,
                    ino_info: None,
                },
            ]),
            SysDrmDirHandle::DeviceDrmOf(_) => Ok(DriDevice::ALL
                .iter()
                .map(|(n, d)| DirEntry {
                    name: String::from(*n),
                    file_type: FileType::Directory,
                    ino_info: Some(d.sys_dir_node_info()),
                })
                .collect()),
        }
    }

    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let dir = dir.into_typed::<Self>();
        match dir {
            SysDrmDirHandle::Device(_) => {
                let Some(SysDrmFile::Subsystem) = SysDrmFile::from_name(name) else {
                    return Ok(None);
                };
                // Real sysfs `subsystem` links are relative, e.g. `../../../../class/drm`,
                // resolving back up to the `drm` class directory -- `libudev` only reads the
                // link target's basename (`drm`) to populate `udev_device_get_subsystem()`, so
                // the exact number of `../` hops does not matter as long as the final basename
                // is right (litebox's `/sys/class/drm` mount is itself a virtual directory with
                // no real sibling classes, so this link is illustrative rather than
                // independently walkable -- matching real udev's own basename-only usage).
                Ok(Some(String::from("../../../class/drm")))
            }
            SysDrmDirHandle::DeviceOf(_) => {
                if name != "subsystem" {
                    return Ok(None);
                }
                // `<name>/device/subsystem` -- real sysfs points this at the device's real
                // bus subsystem directory (`../../../bus/pci` for a real PCI GPU). litebox's
                // virtual DRM device has no real bus parent to model (see `SysClassDrm`'s
                // doc comment on the synthetic `device` subtree), so this resolves to
                // `platform` -- the real kernel's own choice for a DRM device with no
                // discoverable discrete bus (e.g. `simpledrm`/`vkms`), and the value
                // `drmGetDevice2()` (via libdrm's `drm_device_get_subsystem_type`) most
                // readily recognizes as "not a proper PCI/USB/platform device to introspect
                // further" without treating the lookup itself as an error -- confirmed live,
                // this is the only remaining unresolved path in wlroots' render-node open
                // sequence for `226:128` once `device`/`device/drm` themselves resolve.
                Ok(Some(String::from("../../../bus/platform")))
            }
            SysDrmDirHandle::Root | SysDrmDirHandle::DeviceDrmOf(_) => Ok(None),
        }
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let h = h.get_typed::<Self>();
        let (major, minor, devname) = h.device.major_minor_devname();
        let content = match h.file {
            SysDrmFile::Uevent => {
                format!("MAJOR={major}\nMINOR={minor}\nDEVNAME={devname}\nSUBSYSTEM=drm\n")
            }
            SysDrmFile::Dev => format!("{major}:{minor}\n"),
            SysDrmFile::Subsystem => return Err(ReadError::NotForReading),
            // `<name>/device/uevent` -- real platform-bus devices with no removable-media/
            // module-alias properties worth reporting typically carry just `DRIVER=`
            // (`drm_device_get_bustype()` only needs the file to exist and be readable; it
            // does not require any specific key to be present to classify the device as
            // platform-bus rather than PCI/USB, since bus classification already happened
            // via the `subsystem` symlink read immediately before this).
            SysDrmFile::DeviceUevent => String::from("DRIVER=litebox\n"),
        };
        let bytes = content.as_bytes();
        let start = offset.min(bytes.len());
        let end = bytes.len();
        let len = (end - start).min(buf.len());
        buf[..len].copy_from_slice(&bytes[start..start + len]);
        Ok(len)
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
        let h = h.get_typed::<Self>();
        let (major, minor, _devname) = h.device.major_minor_devname();
        let size = match h.file {
            SysDrmFile::Uevent => {
                format!("MAJOR={major}\nMINOR={minor}\nDEVNAME=...\nSUBSYSTEM=drm\n").len()
            }
            SysDrmFile::Dev => format!("{major}:{minor}\n").len(),
            SysDrmFile::Subsystem => 0,
            SysDrmFile::DeviceUevent => "DRIVER=litebox\n".len(),
        };
        Ok(FileStatus {
            // Real sysfs attribute files report as regular files (`lstat` on the
            // `subsystem` symlink itself is handled by the resolver via `read_link_at`,
            // never reaching here for a plain, symlink-following `open()`/`stat()`).
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size,
            owner: UserInfo::ROOT,
            node_info: NodeInfo {
                dev: 5,
                ino: match (h.device, h.file) {
                    (DriDevice::Card0, SysDrmFile::Uevent) => 15,
                    (DriDevice::Card0, SysDrmFile::Dev) => 16,
                    (DriDevice::Card0, SysDrmFile::Subsystem) => 17,
                    (DriDevice::RenderD128, SysDrmFile::Uevent) => 18,
                    (DriDevice::RenderD128, SysDrmFile::Dev) => 19,
                    (DriDevice::RenderD128, SysDrmFile::Subsystem) => 20,
                    (DriDevice::Card0, SysDrmFile::DeviceUevent) => 33,
                    (DriDevice::RenderD128, SysDrmFile::DeviceUevent) => 34,
                },
                rdev: None,
            },
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let h = h.get_typed::<Self>();
        let node_info = match h {
            SysDrmDirHandle::Root => self.root_inode.clone(),
            SysDrmDirHandle::Device(device) => device.sys_dir_node_info(),
            SysDrmDirHandle::DeviceOf(device) => device.sys_device_dir_node_info(),
            SysDrmDirHandle::DeviceDrmOf(device) => device.sys_device_drm_dir_node_info(),
        };
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info,
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

/// Node info for the one file this backend serves, `/run/udev/data/c13:64`.
const UDEV_DB_EVENT0_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 25,
    rdev: None,
};

/// Real eudev per-device-database `E:` property lines this backend serves for
/// `/run/udev/data/c13:64` -- each parsed by `udev_device_read_db()`
/// (`src/libudev/libudev-device.c`) into a real udev property via
/// `udev_device_add_property_from_string()`. libinput's `evdev_configure_device()`
/// (`src/evdev.c`) reads these SPECIFIC property names (`ID_INPUT`/`ID_INPUT_MOUSE`/
/// `ID_INPUT_KEYBOARD`, matched against `evdev_udev_tag_matches[]`) to decide whether a
/// device is tagged as supported input at all -- a device with NO `ID_INPUT` property
/// hits `evdev_configure_device`'s very first check (`(udev_tags &
/// EVDEV_UDEV_TAG_INPUT) == 0`) and is rejected with "not tagged as supported input
/// device", logged by the caller as "not using input device". This is NOT read from any
/// ioctl or sysfs attribute -- real udev normally derives these properties at boot via
/// `hwdb`/`udev` rules matching the device's real evdev capabilities, which litebox has no
/// equivalent of; serving them directly here is the correct, faithful substitute for
/// litebox's one static, known-shape virtual device (a keyboard+mouse-capable device,
/// matching [`EvdevSubsystem`]'s real `push_key`/`push_rel` capability range).
const UDEV_DB_EVENT0_CONTENT: &[u8] = b"E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_KEYBOARD=1\n";

/// A [`super::backend::Backend`] exposing `/run/udev/data/c13:64` -- real eudev's
/// per-device database file (`udev_device_read_db()`, `src/libudev/libudev-device.c`):
/// merely being ABLE TO OPEN this file (any content, even empty) is what real eudev
/// treats as "this device has a database entry" -> `udev_device->is_initialized = true`.
/// `libinput_udev_create_context()`'s own device-enumeration walk
/// (`udev_input_add_devices()`, `src/udev-seat.c`) explicitly skips any device where
/// `udev_device_get_is_initialized()` is false ("skip unconfigured input device") --
/// litebox has no real `udevd` ever running to create this file, so without it, the one
/// virtual input device [`SysClassInput`]/[`InputDevices`] otherwise correctly exposes is
/// silently rejected by libinput's own enumeration filter. Beyond mere openability, the
/// file's CONTENT also matters -- see [`UDEV_DB_EVENT0_CONTENT`]'s own doc comment. This is
/// deliberately NOT a general `/run/udev/data` emulation -- exactly one, fixed file is
/// served, matching litebox's one static virtual input device; a real system's device
/// database has one entry per real device and is written by `udevd` at boot, which
/// litebox has no equivalent of (correctly -- see [`EvdevSubsystem`]'s doc comment on why
/// litebox's device set is intentionally static per-run).
pub struct UdevDb<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> UdevDb<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `UdevDb` backend.
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

/// Owned file handle; only one file exists, so no fields are needed.
#[derive(Debug, Clone, Copy)]
pub struct UdevDbFileHandle;

/// Directory handle, reused for both walking and owned dir handles (no borrows needed).
#[derive(Debug, Clone, Copy)]
pub struct UdevDbDirHandle;

impl<Platform> super::backend::private::Sealed for UdevDb<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for UdevDb<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = UdevDbDirHandle;
    type FileHandle = UdevDbFileHandle;
    type DirHandle = UdevDbDirHandle;
}

impl<Platform> Backend for UdevDb<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(UdevDbDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            if component == "c13:64" {
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
        if name != "c13:64" {
            return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
        }
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(UdevDbFileHandle),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(vec![DirEntry {
            name: String::from("c13:64"),
            file_type: FileType::RegularFile,
            ino_info: Some(UDEV_DB_EVENT0_NODE_INFO),
        }])
    }

    fn read(&self, _h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        // See `UDEV_DB_EVENT0_CONTENT`'s own doc comment: these `E:` property lines are
        // what makes libinput's `evdev_configure_device()` tag this device as supported
        // input at all, not just "openable".
        let content = UDEV_DB_EVENT0_CONTENT;
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

    fn file_status(&self, _h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size: UDEV_DB_EVENT0_CONTENT.len(),
            owner: UserInfo::ROOT,
            node_info: UDEV_DB_EVENT0_NODE_INFO,
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, _h: &DirHandle) -> Result<FileStatus, FileStatusError> {
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

/// A leaf file inside `/sys/class/input/event0/` -- the minimal set a real
/// `libudev`-based input client (`libinput_udev_create_context()`'s
/// `udev_enumerate_scan_devices()` walk) needs: `uevent` (populates `udev_device`
/// properties without a running `udevd`) and `subsystem` (a symlink whose basename
/// `udev_device_get_subsystem()` reads). Mirrors [`SysDrmFile`]'s exact shape, scoped to
/// litebox's one virtual input device (`/dev/input/event0`, see [`InputDevice::Event0`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysInputFile {
    /// `MAJOR=`/`MINOR=`/`DEVNAME=`/`SUBSYSTEM=` key=value lines.
    Uevent,
    /// `MAJOR:MINOR` (e.g. `13:64`), the standard sysfs device-node attribute.
    Dev,
    /// Symlink to the (synthetic) `input` subsystem directory.
    Subsystem,
}

impl SysInputFile {
    const ALL: &'static [(&'static str, SysInputFile)] = &[
        ("uevent", SysInputFile::Uevent),
        ("dev", SysInputFile::Dev),
        ("subsystem", SysInputFile::Subsystem),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
    }
}

/// Node info for the `/sys/class/input/event0` directory itself (distinct from
/// `/dev/input/event0`'s own [`INPUT_EVENT0_NODE_INFO`] -- sysfs directories and the
/// device nodes they describe are always separate inodes on real Linux too).
const SYS_INPUT_EVENT0_DIR_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 21,
    rdev: None,
};

/// A [`super::backend::Backend`] exposing the minimal `/sys/class/input/event0/` subtree
/// a real `libudev`-based input client needs to enumerate litebox's one emulated evdev
/// device. Deliberately NOT a general procfs/sysfs emulation -- only the exact files real
/// `udev_enumerate_scan_devices()` + `udev_device_new_from_syspath()` calls read
/// (`uevent`, `dev`, `subsystem`) are served, for the one node [`InputDevices`] already
/// exposes at `/dev/input/event0`. Mounted at `/sys/class/input`; the composer's virtual-
/// directory auto-synthesis creates the `/sys` and `/sys/class` ancestor directories
/// automatically, so this backend only needs to handle its own one-level subtree
/// (`event0`, containing `uevent`/`dev`/`subsystem`). Only one device exists, so unlike
/// [`SysClassDrm`] this backend has no device-selector enum to match on.
pub struct SysClassInput<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> SysClassInput<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `SysClassInput` backend.
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

/// Directory handle: either the backend's mount root (`/sys/class/input` itself) or
/// inside the one device's subdirectory (`/sys/class/input/event0`).
#[derive(Debug, Clone, Copy)]
pub enum SysInputDirHandle {
    Root,
    Device,
}

/// Owned file handle; identifies which sysfs attribute file backs this fd (only one
/// device exists, so no device selector is needed alongside the file kind).
#[derive(Debug, Clone, Copy)]
pub struct SysInputFileHandle {
    file: SysInputFile,
}

impl<Platform> super::backend::private::Sealed for SysClassInput<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for SysClassInput<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = SysInputDirHandle;
    type FileHandle = SysInputFileHandle;
    type DirHandle = SysInputDirHandle;
}

impl<Platform> Backend for SysClassInput<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Root)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        match from {
            SysInputDirHandle::Root => {
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Root),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                if component != "event0" {
                    return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
                }
                let walked = vec![WalkedComponent {
                    permissions: PermissionCheck::ByBackend,
                }];
                if components.len() == 1 {
                    return Ok(WalkOutcome {
                        components: walked,
                        last: WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Device),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                }
                if components.len() == 2 && SysInputFile::from_name(components[1]).is_some() {
                    return Ok(WalkOutcome {
                        components: walked,
                        last: WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Device),
                        stop_reason: WalkStopReason::StoppedAtNonDirectory,
                    });
                }
                Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
            }
            SysInputDirHandle::Device => {
                let Some(&component) = components.first() else {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Device),
                        stop_reason: WalkStopReason::CompleteDirectory,
                    });
                };
                if SysInputFile::from_name(component).is_some() {
                    return Ok(WalkOutcome {
                        components: vec![],
                        last: WalkingDirHandle::from_typed::<Self>(SysInputDirHandle::Device),
                        stop_reason: WalkStopReason::StoppedAtNonDirectory,
                    });
                }
                Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
            }
        }
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
        let SysInputDirHandle::Device = dir else {
            return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
        };
        let file = SysInputFile::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(SysInputFileHandle { file }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let handle = handle.into_typed::<Self>();
        match handle {
            SysInputDirHandle::Root => Ok(vec![DirEntry {
                name: String::from("event0"),
                file_type: FileType::Directory,
                ino_info: Some(SYS_INPUT_EVENT0_DIR_NODE_INFO),
            }]),
            SysInputDirHandle::Device => Ok(SysInputFile::ALL
                .iter()
                .map(|(n, f)| DirEntry {
                    name: String::from(*n),
                    file_type: if matches!(f, SysInputFile::Subsystem) {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    },
                    ino_info: None,
                })
                .collect()),
        }
    }

    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let dir = dir.into_typed::<Self>();
        let SysInputDirHandle::Device = dir else {
            return Ok(None);
        };
        let Some(SysInputFile::Subsystem) = SysInputFile::from_name(name) else {
            return Ok(None);
        };
        Ok(Some(String::from("../../../class/input")))
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let h = h.get_typed::<Self>();
        let content = match h.file {
            SysInputFile::Uevent => {
                String::from("MAJOR=13\nMINOR=64\nDEVNAME=input/event0\nSUBSYSTEM=input\n")
            }
            SysInputFile::Dev => String::from("13:64\n"),
            SysInputFile::Subsystem => return Err(ReadError::NotForReading),
        };
        let bytes = content.as_bytes();
        let start = offset.min(bytes.len());
        let end = bytes.len();
        let len = (end - start).min(buf.len());
        buf[..len].copy_from_slice(&bytes[start..start + len]);
        Ok(len)
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
        let h = h.get_typed::<Self>();
        let size = match h.file {
            SysInputFile::Uevent => {
                "MAJOR=13\nMINOR=64\nDEVNAME=input/event0\nSUBSYSTEM=input\n".len()
            }
            SysInputFile::Dev => "13:64\n".len(),
            SysInputFile::Subsystem => 0,
        };
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size,
            owner: UserInfo::ROOT,
            node_info: NodeInfo {
                dev: 5,
                ino: match h.file {
                    SysInputFile::Uevent => 22,
                    SysInputFile::Dev => 23,
                    SysInputFile::Subsystem => 24,
                },
                rdev: None,
            },
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let h = h.get_typed::<Self>();
        let node_info = match h {
            SysInputDirHandle::Root => self.root_inode.clone(),
            SysInputDirHandle::Device => SYS_INPUT_EVENT0_DIR_NODE_INFO,
        };
        Ok(FileStatus {
            file_type: FileType::Directory,
            mode: Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            size: super::DEFAULT_DIRECTORY_SIZE,
            owner: UserInfo::ROOT,
            node_info,
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

/// The one `/sys/dev/char/<major>:<minor>` reverse-lookup symlink litebox's static device
/// set needs, and its target directory under `/sys/class/*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysDevCharEntry {
    /// `13:64` -- the virtual input device, target `../../class/input/event0`.
    Input,
    /// `226:0` -- the virtual DRM primary node, target `../../class/drm/card0`. Needed by
    /// `wlroots`' DRM backend (`drmGetDeviceNameFromFd2()`, used by labwc/sway): unlike
    /// weston's DRM backend, which opens `/dev/dri/card0` by its already-known path and
    /// never re-derives it via this reverse lookup, wlroots calls `fstat()` on the fd then
    /// resolves this symlink to canonicalize the device name -- without it,
    /// `drmGetDeviceNameFromFd2()` fails with ENOENT and wlroots aborts backend creation
    /// before ever reaching DRM_IOCTL_MODE_GETRESOURCES, confirmed live (weston's DRM
    /// backend works fine against the same virtual card with this entry absent; labwc's does
    /// not).
    Drm,
    /// `226:128` -- the virtual DRM render node, target `../../class/drm/renderD128`. Same
    /// rationale as [`SysDevCharEntry::Drm`], but reached from a *different* wlroots code
    /// path: after the primary node's DRM backend is up, wlroots' `wlr_drm_backend` (via
    /// `types/wlr_drm.c`'s `drmGetDevice2()`) opens the render node to build a GBM/EGL
    /// renderer for client buffer allocation, and that call needs `226:128`'s own reverse
    /// lookup for the identical reason `226:0`'s was needed for the primary node -- confirmed
    /// live, `sys_stat` on `/sys/dev/char/226:128/device/drm` returning `ENOENT` immediately
    /// precedes `types/wlr_drm.c:217]"drmGetDevice2 failed"` in a real repro capture, with the
    /// shallower `/sys/dev/char/226:128` symlink itself already resolvable once this entry
    /// exists (the deeper `device/drm` subtree it resolves into was already served for both
    /// `card0` and `renderD128` by [`SysClassDrm`] -- only this reverse-lookup entry was
    /// missing).
    DrmRender,
}

impl SysDevCharEntry {
    const ALL: &'static [(&'static str, SysDevCharEntry)] = &[
        ("13:64", SysDevCharEntry::Input),
        ("226:0", SysDevCharEntry::Drm),
        ("226:128", SysDevCharEntry::DrmRender),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }

    fn target(self) -> &'static str {
        match self {
            SysDevCharEntry::Input => "../../class/input/event0",
            SysDevCharEntry::Drm => "../../class/drm/card0",
            SysDevCharEntry::DrmRender => "../../class/drm/renderD128",
        }
    }
}

/// Node info for the `13:64` entry.
const SYS_DEV_CHAR_INPUT_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 26,
    rdev: None,
};

/// Node info for the `226:0` entry.
const SYS_DEV_CHAR_DRM_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 27,
    rdev: None,
};

/// Node info for the `226:128` entry.
const SYS_DEV_CHAR_DRM_RENDER_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 32,
    rdev: None,
};

/// A [`super::backend::Backend`] exposing `/sys/dev/char/<major>:<minor>` -- the standard
/// sysfs reverse-lookup symlink from a character device's `(major, minor)` pair back to
/// its `/sys/class/*` directory. Real `libudev`'s `udev_device_new_from_devnum()` (used by
/// `seatd`'s own `seat_open_device()` to canonicalize/re-validate a device path via
/// `realpath()` + a `stat()`-then-devnum-lookup) reads exactly this symlink; without it,
/// seatd's device-open sequence silently fails and immediately closes the just-opened fd
/// (confirmed live: `sys_stat` on `/sys/dev/char/13:64` returns `ENOENT` immediately before
/// seatd's own `"Closing device"` log line, with zero error in between). Deliberately NOT a
/// general sysfs `dev/char` emulation -- only the two entries litebox's static virtual
/// device set needs (see [`SysClassInput`]/[`SysClassDrm`]): weston's DRM backend opens
/// `/dev/dri/card0` by its already-known path and never re-derives it via this reverse
/// lookup, but wlroots' DRM backend (labwc/sway) calls `drmGetDeviceNameFromFd2()`, which
/// does depend on this lookup succeeding -- confirmed live, `226:0` was originally omitted
/// on the (wrong, weston-only) assumption that no DRM consumer needed it.
pub struct SysDevChar<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
}

impl<Platform> SysDevChar<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `SysDevChar` backend.
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

/// Directory handle: only the backend's mount root exists (a flat namespace, no
/// per-entry subdirectories).
#[derive(Debug, Clone, Copy)]
pub struct SysDevCharDirHandle;

/// Owned file handle; identifies which `<major>:<minor>` entry this fd is (only used for
/// `read_link_at`, since the entry is always a symlink, never opened for read/write).
#[derive(Debug, Clone, Copy)]
pub struct SysDevCharFileHandle {
    entry: SysDevCharEntry,
}

impl<Platform> super::backend::private::Sealed for SysDevChar<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for SysDevChar<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = SysDevCharDirHandle;
    type FileHandle = SysDevCharFileHandle;
    type DirHandle = SysDevCharDirHandle;
}

impl<Platform> Backend for SysDevChar<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(SysDevCharDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        let Some(&component) = components.first() else {
            return Ok(WalkOutcome {
                components: vec![],
                last: WalkingDirHandle::from_typed::<Self>(from),
                stop_reason: WalkStopReason::CompleteDirectory,
            });
        };
        if SysDevCharEntry::from_name(component).is_none() {
            return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
        }
        Ok(WalkOutcome {
            components: vec![],
            last: WalkingDirHandle::from_typed::<Self>(from),
            stop_reason: WalkStopReason::StoppedAtNonDirectory,
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
        let entry = SysDevCharEntry::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(SysDevCharFileHandle { entry }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(SysDevCharEntry::ALL
            .iter()
            .map(|(n, _)| DirEntry {
                name: String::from(*n),
                file_type: FileType::Symlink,
                ino_info: None,
            })
            .collect())
    }

    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let _dir = dir.into_typed::<Self>();
        let Some(entry) = SysDevCharEntry::from_name(name) else {
            return Ok(None);
        };
        Ok(Some(String::from(entry.target())))
    }

    fn read(&self, _h: &FileHandle, _buf: &mut [u8], _offset: usize) -> Result<usize, ReadError> {
        Err(ReadError::NotForReading)
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
        let h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size: 0,
            owner: UserInfo::ROOT,
            node_info: match h.entry {
                SysDevCharEntry::Input => SYS_DEV_CHAR_INPUT_NODE_INFO,
                SysDevCharEntry::Drm => SYS_DEV_CHAR_DRM_NODE_INFO,
                SysDevCharEntry::DrmRender => SYS_DEV_CHAR_DRM_RENDER_NODE_INFO,
            },
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, _h: &DirHandle) -> Result<FileStatus, FileStatusError> {
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
