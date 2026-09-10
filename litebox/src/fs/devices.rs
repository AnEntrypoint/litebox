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
/// Block size for /dev/zero
const ZERO_BLOCK_SIZE: usize = 0x1000;
/// Block size for /dev/random
const RANDOM_BLOCK_SIZE: usize = 0x1000;
/// Block size for /dev/full
const FULL_BLOCK_SIZE: usize = 0x1000;

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
/// Node info for `/dev/tty0` (major=4, minor=0 -- the real Linux "current VT" console device).
/// `seatd`'s `seat_update_vt` opens exactly this path and `VT_GETSTATE`s it to learn the active
/// VT. See gm mutable mut-1789043589437.
const TTY0_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 21,
    // major=4, minor=0
    rdev: core::num::NonZeroUsize::new(0x0400),
};
/// Node info for `/dev/tty1` (major=4, minor=1 -- the first real numbered VT). This virtual
/// device always reports VT 1 as active, so `/dev/tty1` is the node `seatd`'s `vt_open`/`vt_close`
/// open once `VT_GETSTATE` on `/dev/tty0` names it. See gm mutable mut-1789043589437.
const TTY1_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 22,
    // major=4, minor=1
    rdev: core::num::NonZeroUsize::new(0x0401),
};
/// Node info for `/dev/zero` (major=1, minor=5 -- real Linux convention). Reads deliver endless
/// NUL bytes, writes are discarded; GTK/GLib/Xorg mmap it as an anonymous-memory substitute.
/// See gm mutable mut-1789043610245.
const ZERO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 23,
    // major=1, minor=5
    rdev: core::num::NonZeroUsize::new(0x105),
};
/// Node info for `/dev/random` (major=1, minor=8 -- real Linux convention). libgcrypt/GnuTLS
/// (dbus, at-spi, gvfs) open this directly; treated identically to [`Device::URandom`], litebox
/// having no entropy-starvation model. See gm mutable mut-1789043610245.
const RANDOM_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 24,
    // major=1, minor=8
    rdev: core::num::NonZeroUsize::new(0x108),
};
/// Node info for `/dev/full` (major=1, minor=7 -- real Linux convention). Reads behave like
/// `/dev/zero`; every write must fail, which some programs' error-handling paths depend on.
/// See gm mutable mut-1789043610245.
const FULL_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 25,
    // major=1, minor=7
    rdev: core::num::NonZeroUsize::new(0x107),
};
/// Node info for `/dev/console` (major=5, minor=1 -- matching the major:minor observed in the
/// real webtop image's own tar device-node entry for this path). Session/init-shaped guest code
/// opens it directly. See gm mutable mut-1789043589437.
const CONSOLE_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 26,
    // major=5, minor=1
    rdev: core::num::NonZeroUsize::new(0x501),
};
/// Node info for `/dev/tty` (major=5, minor=0 -- real Linux convention). The
/// controlling-terminal alias, distinct from the numbered VT devices (`tty0`/`tty1`).
/// See gm mutable mut-1789043589437.
const TTY_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 27,
    // major=5, minor=0
    rdev: core::num::NonZeroUsize::new(0x500),
};

/// What `stat` reports for the `/dev/pts` devpts mount point.
///
/// Lives here although nothing in this crate mounts `/dev/pts`: `FileStatus` is
/// `#[non_exhaustive]`, so only this crate can build one. The shim decides which ptys exist.
#[must_use]
pub fn devpts_dir_status() -> FileStatus {
    FileStatus {
        file_type: FileType::Directory,
        mode: Mode::RWXU
            .union(Mode::RGRP)
            .union(Mode::XGRP)
            .union(Mode::ROTH)
            .union(Mode::XOTH),
        size: super::DEFAULT_DIRECTORY_SIZE,
        owner: UserInfo::ROOT,
        node_info: NodeInfo {
            dev: 5,
            ino: 1,
            rdev: None,
        },
        blksize: super::DEFAULT_DIRECTORY_SIZE,
        atime: Timestamp::default(),
        mtime: Timestamp::default(),
    }
}

/// What `stat` reports for the pty slave `/dev/pts/<id>`.
///
/// The caller must already have established that this pty is allocated -- see
/// [`devpts_dir_status`].
#[must_use]
pub fn devpts_slave_status(id: u32) -> FileStatus {
    FileStatus {
        file_type: FileType::CharacterDevice,
        // `rw-rw-rw-`. A slave is opened by whoever holds its id, and this crate models no tty
        // group ownership to restrict it with.
        mode: Mode::RUSR
            .union(Mode::WUSR)
            .union(Mode::RGRP)
            .union(Mode::WGRP)
            .union(Mode::ROTH)
            .union(Mode::WOTH),
        size: 0,
        owner: UserInfo::ROOT,
        node_info: NodeInfo {
            dev: 5,
            // Distinct per slave, so two ptys never look like the same file to a caller that
            // compares `(dev, ino)`.
            ino: 0x1000 + id as usize,
            // Real Linux devpts slaves are character devices 136:<id>.
            rdev: core::num::NonZeroUsize::new(0x8800 + id as usize),
        },
        blksize: 0x1000,
        atime: Timestamp::default(),
        mtime: Timestamp::default(),
    }
}

/// `/dev/ptmx`, the pty multiplexer. Real Linux character device 5:2.
const PTMX_NODE_INFO: NodeInfo = NodeInfo {
    dev: 5,
    ino: 28,
    // major=5, minor=2
    rdev: core::num::NonZeroUsize::new(0x502),
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
    /// `/dev/zero` -- endless NUL-byte stream on read, discards writes.
    Zero,
    /// `/dev/random` -- treated identically to [`Device::URandom`]; this backend has no
    /// entropy-starvation model to distinguish the two. See gm mutable mut-1789043610245.
    Random,
    /// `/dev/full` -- reads behave like [`Device::Zero`]; every write fails with `ENOSPC`.
    Full,
    /// `/dev/ptmx` -- the pty multiplexer. `open` never reaches this backend: the shim's
    /// `do_open_resolved` intercepts it and returns a live master from its own registry.
    /// Must stay in `Device::ALL` regardless, or `stat`/`access`/`readdir` return `ENOENT`, glibc's
    /// `openpty`/`grantpt` fail, and `xfce4-terminal` reports "error creating pty".
    /// See gm mutable mut-1789043570653.
    Ptmx,
    /// `/dev/console` -- opens and stats successfully; byte-stream I/O is rejected rather than
    /// faked. See gm mutable mut-1789043589437.
    Console,
    /// `/dev/tty` -- the controlling-terminal alias, deliberately a FIXED node and NOT a real
    /// per-session ctty redirect: `open_file_at` carries no caller identity, so this backend
    /// cannot reach the shim's session state. Serves `isatty`/`ctermid`/stat probing only.
    /// See gm mutable mut-1789043589437.
    Tty,
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
        ("zero", Device::Zero),
        ("random", Device::Random),
        ("full", Device::Full),
        ("console", Device::Console),
        ("tty", Device::Tty),
        ("ptmx", Device::Ptmx),
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
                // Real VT nodes are `crw--w----` group `tty`; litebox's guest identity is
                // always root, so group-writable suffices. See gm mutable mut-1789043627523.
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
            Device::Zero | Device::Full => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: if self == Device::Zero {
                    ZERO_NODE_INFO
                } else {
                    FULL_NODE_INFO
                },
                blksize: if self == Device::Zero {
                    ZERO_BLOCK_SIZE
                } else {
                    FULL_BLOCK_SIZE
                },
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            Device::Random => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: RANDOM_NODE_INFO,
                blksize: RANDOM_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            Device::Console => FileStatus {
                file_type: FileType::CharacterDevice,
                // Real /dev/console is `crw-------` (mode 0600), owner root -- matches the
                // real webtop image's own tar entry for this path (major:minor 5:1).
                mode: Mode::RUSR | Mode::WUSR,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: CONSOLE_NODE_INFO,
                blksize: STDIO_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            // `rw-rw-rw-`, matching real Linux: any user may open the multiplexer.
            Device::Ptmx => FileStatus {
                file_type: FileType::CharacterDevice,
                mode: Mode::RUSR
                    .union(Mode::WUSR)
                    .union(Mode::RGRP)
                    .union(Mode::WGRP)
                    .union(Mode::ROTH)
                    .union(Mode::WOTH),
                size: 0,
                owner: UserInfo::ROOT,
                node_info: PTMX_NODE_INFO,
                blksize: 0x1000,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
            Device::Tty => FileStatus {
                file_type: FileType::CharacterDevice,
                // Real /dev/tty is `crw-rw-rw-` (mode 0666) -- world-writable/readable since
                // any process's own controlling terminal is meant to always be reachable via
                // this path regardless of the tty's own group-restricted permissions.
                mode: Mode::RUSR
                    | Mode::WUSR
                    | Mode::RGRP
                    | Mode::WGRP
                    | Mode::ROTH
                    | Mode::WOTH,
                size: 0,
                owner: UserInfo::ROOT,
                node_info: TTY_NODE_INFO,
                blksize: STDIO_BLOCK_SIZE,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
            },
        }
    }
}

/// LOUD diagnostic for an `open("/dev/<name>")` this backend does not register: without it the
/// guest sees only a generic `ENOENT` and names no missing device.
/// Must stay `format!`-free -- a fixed stack buffer straight to
/// [`crate::platform::StdioProvider::write_to`] -- so it is safe where the heap may not be.
/// See gm mutable mut-1789043615583.
fn diag_raw_print_dev_open_miss<Platform: crate::platform::StdioProvider>(
    platform: &Platform,
    name: &str,
) {
    const PREFIX: &[u8] = b"[diag-dev-open-miss] unregistered /dev/ path opened: /dev/";
    let mut line = [0u8; 192];
    let mut pos = 0usize;
    let n = PREFIX.len().min(line.len());
    line[..n].copy_from_slice(&PREFIX[..n]);
    pos += n;
    let name_bytes = name.as_bytes();
    let avail = line.len().saturating_sub(pos).saturating_sub(1);
    let take = name_bytes.len().min(avail);
    line[pos..pos + take].copy_from_slice(&name_bytes[..take]);
    pos += take;
    if pos < line.len() {
        line[pos] = b'\n';
        pos += 1;
    }
    let _ = platform.write_to(crate::platform::StdioOutStream::Stderr, &line[..pos]);
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
        let device = match Device::from_name(name) {
            Some(device) => device,
            None => {
                diag_raw_print_dev_open_miss(self.litebox.x.platform, name);
                return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
            }
        };

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        // `O_NONBLOCK` must be accepted and ignored here, never rejected: libuv/Node reopens
        // `/dev/stdin` non-blocking, and `EAGAIN` is delivered by the shim's `do_read` from
        // `StdioStatusFlags` metadata, not by this stateless backend.
        // See gm mutable mut-1789043596575.

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
            Device::URandom | Device::Random => {
                // `/dev/random` is treated identically to `/dev/urandom`: no entropy-starvation
                // model. See gm mutable mut-1789043610245.
                self.litebox.x.platform.fill_bytes_crng(buf);
                Ok(buf.len())
            }
            Device::Zero | Device::Full => {
                // /dev/zero and /dev/full both deliver an endless NUL-byte stream on read.
                buf.fill(0);
                Ok(buf.len())
            }
            // Reject, never return 0 bytes: `seatd` only ever ioctls these nodes, so a silent
            // empty read would hide a real caller this backend cannot serve.
            // See gm mutable mut-1789043589437.
            Device::Tty0 | Device::Tty1 | Device::Console | Device::Tty | Device::Ptmx => {
                Err(ReadError::NotForReading)
            }
        }
    }

    fn write(&self, h: &FileHandle, buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        let h = h.get_typed::<Self>();
        let stream = match h.device {
            Device::Stdin => return Err(WriteError::NotForWriting),
            Device::Stdout => crate::platform::StdioOutStream::Stdout,
            Device::Stderr => crate::platform::StdioOutStream::Stderr,
            Device::Null | Device::URandom | Device::Random => {
                // Discarded, not stirred in: a real `/dev/[u]random` write perturbs the entropy
                // pool (without raising the entropy count), which litebox does not model.
                // See gm mutable mut-1789043610245.
                return Ok(buf.len());
            }
            Device::Zero => {
                // /dev/zero discards writes, same as /dev/null.
                return Ok(buf.len());
            }
            Device::Full => {
                // Real `/dev/full` gives `ENOSPC`; `WriteError` has no no-space variant, so `Io`
                // is the deliberate stand-in -- never `Ok`, which callers' error paths test for.
                // See gm mutable mut-1789043610245.
                return Err(WriteError::Io);
            }
            // Reject, never silently succeed -- see gm mutable mut-1789043589437.
            Device::Tty0 | Device::Tty1 | Device::Console | Device::Tty | Device::Ptmx => {
                return Err(WriteError::NotForWriting);
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

    fn chmod(&self, _h: &FileHandle, _mode: Mode) -> Result<(), ChmodError> {
        Err(ChmodError::ReadOnlyFileSystem)
    }

    fn seek_behavior(&self, h: &FileHandle) -> SeekBehavior {
        let h = h.get_typed::<Self>();
        match h.device {
            Device::Stdin
            | Device::Stdout
            | Device::Stderr
            | Device::Tty0
            | Device::Tty1
            | Device::Console
            | Device::Tty
            | Device::Ptmx => SeekBehavior::NonSeekable,
            Device::Null | Device::URandom | Device::Zero | Device::Random | Device::Full => {
                SeekBehavior::ZeroPosition
            }
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

/// A DRM device node -- `card0` (the control/modeset node) or `renderD128` (the render-only
/// node). `renderD128` is exposed even though only the dumb-buffer path is implemented, because
/// `libdrm` commonly probes for a render node and quietly skips it if absent.
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
            // Real DRM nodes are `crw-rw----` group `video`; litebox's guest identity is
            // always root, so group-readable suffices. See gm mutable mut-1789043627523.
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

/// A [`super::backend::Backend`] exposing `/dev/dri/{card0,renderD128}` -- the device-node SHAPE
/// only (open/stat/permissions/listing); the DRM ioctl protocol lives in `litebox_shim_linux`'s
/// `DrmSubsystem`. Must be a nested mount at `/dev/dri`: [`Devices`]' own `walk_directories` is a
/// flat single-level namespace with no subdirectory support.
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
        // Real DRM `read()` delivers queued `drm_event` records, not pixel bytes, and that path
        // is still a stub: reject rather than return 0, or a client polling for flip completion
        // spins forever. See gm mutable mut-1789043637459.
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

/// An evdev input device node. Only `event0` is exposed: one combined keyboard+mouse node is a
/// real, valid evdev shape (a USB keyboard-with-trackpad reports both `EV_KEY` and `EV_REL` on one
/// node) and is sufficient for one virtual display with one virtual input source.
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
            // Real evdev nodes are `crw-r-----` group `input`; litebox's guest identity is
            // always root, so group-readable suffices. See gm mutable mut-1789043627523.
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

/// The set of currently-allocated pty ids, shared with `litebox_shim_linux`'s `GlobalState`.
/// A deliberate mirror of the shim's own `pty_registry`, whose typed-fd values cannot cross the
/// crate boundary; `ptmx_open`/`ptmx_closed`/`attach_pty_stdio` are the only sites that mutate
/// either, so the two cannot drift.
pub type PtsRegistry = alloc::collections::BTreeSet<u32>;

/// A [`super::backend::Backend`] exposing `/dev/pts/<id>` for every live pty. It exists so that
/// `/dev/pts` ITSELF is `open(O_DIRECTORY)`-able and listable: glibc's `ttyname_r` opens and scans
/// that directory to cross-check its `/proc/self/fd` readlink, and fails every `openpty()` without
/// it. Nested mount at `/dev/pts`, under the same constraint as [`DriDevices`].
pub struct PtsDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    registry: alloc::sync::Arc<crate::sync::RwLock<Platform, PtsRegistry>>,
}

impl<Platform> PtsDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `PtsDevices` backend sharing the given `registry` -- the shim keeps its own
    /// clone of the same `Arc` and updates it as ptys are allocated and freed.
    #[must_use]
    pub fn new(
        _litebox: &LiteBox<Platform>,
        _allocator: InodeAllocator,
        registry: alloc::sync::Arc<crate::sync::RwLock<Platform, PtsRegistry>>,
    ) -> Self {
        Self { registry }
    }
}

/// Owned file handle; identifies which pty slave (by id) backs this fd. Never carries real I/O --
/// the shim intercepts every `open("/dev/pts/<id>")` first; this is the correctly-shaped fallback
/// for a stat/access that interception misses, so such a caller gets an answer and not a panic.
#[derive(Debug, Clone, Copy)]
pub struct PtsDeviceFileHandle {
    id: u32,
}

/// Directory handle, reused for both walking and owned dir handles (no borrows needed).
#[derive(Debug, Clone, Copy)]
pub struct PtsDeviceDirHandle;

impl<Platform> super::backend::private::Sealed for PtsDevices<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for PtsDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = PtsDeviceDirHandle;
    type FileHandle = PtsDeviceFileHandle;
    type DirHandle = PtsDeviceDirHandle;
}

impl<Platform> Backend for PtsDevices<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(PtsDeviceDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            let exists = component
                .parse::<u32>()
                .is_ok_and(|id| self.registry.read().contains(&id));
            if exists {
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
        let id = name
            .parse::<u32>()
            .ok()
            .filter(|id| self.registry.read().contains(id))
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;

        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }

        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(PtsDeviceFileHandle { id }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(self
            .registry
            .read()
            .iter()
            .map(|id| DirEntry {
                name: format!("{id}"),
                file_type: FileType::CharacterDevice,
                ino_info: Some(devpts_slave_status(*id).node_info),
            })
            .collect())
    }

    fn read(&self, _h: &FileHandle, _buf: &mut [u8], _offset: usize) -> Result<usize, ReadError> {
        // Real pty I/O never reaches this backend -- see gm mutable mut-1789043570653.
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
        Ok(devpts_slave_status(h.get_typed::<Self>().id))
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let _h = h.get_typed::<Self>();
        // Must be the SAME shape the shim's own `stat("/dev/pts")` answers with, not a separate
        // `root_inode`: glibc's `ttyname_r` compares the two. See gm mutable mut-1789043570653.
        Ok(devpts_dir_status())
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

/// A [`super::backend::Backend`] exposing `/dev/input/event0` -- the device-node SHAPE only
/// (open/stat/permissions/listing); the evdev capability ioctls and `input_event` byte stream live
/// in `litebox_shim_linux`'s `EvdevSubsystem`. Nested mount at `/dev/input`, under the same
/// constraint as [`DriDevices`].
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
        // `EvdevSubsystem` intercepts `read()` on this fd before this method is ever reached, so
        // reaching it means something bypassed that interception: reject rather than return 0.
        // See gm mutable mut-1789043637459.
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


/// A leaf file inside `/sys/class/drm/{card0,renderD128}/` -- exactly the set a real
/// `libudev`/`libdrm` enumeration walk falls back to reading when no `udevd` database exists,
/// which in litebox is always.
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
    /// `<name>/device/uevent` -- the *device's own* uevent file, one level below
    /// [`SysDrmFile::Uevent`]; libdrm's `drmGetDevice2()` fails without it. Deliberately absent
    /// from [`SysDrmFile::ALL`], which covers only the real `<name>/` directory, and therefore
    /// reachable solely via [`SysDrmDirHandle::DeviceOf`].
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

/// Node info for the synthetic `/sys/class/drm/card0/device` directory.
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
/// Node info for the synthetic `/sys/class/drm/card0/device/drm` directory.
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

/// A [`super::backend::Backend`] serving the minimal `/sys/class/drm/{card0,renderD128}/` subtree
/// a `libudev` DRM client enumerates (`uevent`/`dev`/`subsystem`), plus a synthetic
/// `<name>/device/drm/<name>` looping back to `/sys/class/drm/<name>`: libdrm's
/// `drmGetDeviceNameFromFd2()` (wlroots/labwc) `stat`s it or fails "Failed to create DRM backend".
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

/// Directory handle: the backend's mount root (`/sys/class/drm` itself), one specific device's
/// subdirectory (`/sys/class/drm/<name>`), that device's synthetic `device` subdirectory, or that
/// subdirectory's own `drm` subdirectory.
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
                // MUST be populated: the resolver reads `components.len()` both for per-level
                // permissions and to know how many path components were consumed as directories.
                // See gm mutable mut-1789043705517.
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
                // Second component: a leaf file stops the walk -- the caller resolves the final
                // component itself, as `TarRo` does -- while `device` continues it.
                // See gm mutable mut-1789043705517.
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
                    // Prepend BOTH components consumed here: `card0`/`renderD128` AND `device`
                    // itself, which the delegated call never counts. Undercounting trips the
                    // composer's own walk-length assertion. See gm mutable mut-1789043705517.
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
                    // Count the `device` component itself -- see gm mutable mut-1789043705517.
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
                // Inside the synthetic `<name>/device` directory: only `drm` exists here, and
                // walking into it continues one more synthetic level.
                // See gm mutable mut-1789043688678.
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
                    // invariant; see gm mutable mut-1789043705517.
                    let mut components_out = walked;
                    components_out.append(&mut outcome.components);
                    return Ok(WalkOutcome {
                        components: components_out,
                        last: outcome.last,
                        stop_reason: outcome.stop_reason,
                    });
                }
                // Leaf stops, not walkable directories: libdrm's `drmGetDevice2()` fails if
                // either is unhandled. See gm mutable mut-1789043688678.
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
                // Inside the synthetic `<name>/device/drm` directory: `card0`/`renderD128` each
                // resolve back to the real `/sys/class/drm/<name>` -- the self-referencing loop
                // real PCI topology produces. See gm mutable mut-1789043688678.
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
                // Beyond here (e.g. `.../drm/card0/uevent`) delegate into the real
                // `Device(target)` walk -- looping back is the point -- prepending the component
                // this arm consumed. See gm mutable mut-1789043705517.
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
        // `device/drm` has no leaf files of its own, so a plain-file open inside it is always
        // `ENOENT`; `device` has exactly one (`uevent`), its `subsystem` being a symlink served
        // by `read_link_at`. See gm mutable mut-1789043688678.
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
                // Only the basename is load-bearing: `libudev` reads it for
                // `udev_device_get_subsystem()` and never walks the `../` hops.
                // See gm mutable mut-1789043689557.
                Ok(Some(String::from("../../../class/drm")))
            }
            SysDrmDirHandle::DeviceOf(_) => {
                if name != "subsystem" {
                    return Ok(None);
                }
                // Must be `platform`, never `pci`: it is what the kernel itself reports for a
                // bus-less DRM device (`simpledrm`/`vkms`) and the one value libdrm's
                // `drm_device_get_subsystem_type()` accepts without erroring.
                // See gm mutable mut-1789043688678.
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
            // `drm_device_get_bustype()` only needs this file to exist and be readable -- no
            // specific key -- bus classification having already happened via the `subsystem`
            // symlink read just before. See gm mutable mut-1789043688678.
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

/// The exact `E:` property lines `/run/udev/data/c13:64` must carry. libinput's
/// `evdev_configure_device()` rejects any device without `ID_INPUT` ("not tagged as supported
/// input device"); real udev derives these from `hwdb` rules at boot, which litebox has none of.
/// See gm mutable mut-1789043722510.
const UDEV_DB_EVENT0_CONTENT: &[u8] = b"E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_KEYBOARD=1\n";

/// A [`super::backend::Backend`] serving exactly one file, `/run/udev/data/c13:64` -- eudev's
/// per-device database entry. Its mere openability sets `udev_device->is_initialized`, without
/// which `libinput_udev_create_context()` silently skips the one virtual input device
/// [`InputDevices`] exposes; its contents matter too, see [`UDEV_DB_EVENT0_CONTENT`].
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
        // These `E:` lines are what make libinput's `evdev_configure_device()` tag this device
        // as supported input at all, not merely "openable". See gm mutable mut-1789043722510.
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

/// A leaf file inside `/sys/class/input/event0/` -- the same minimal set as [`SysDrmFile`], which
/// is what a real `libudev` input client's `udev_enumerate_scan_devices()` walk reads, scoped to
/// litebox's one virtual input device ([`InputDevice::Event0`]).
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

/// A [`super::backend::Backend`] serving the minimal `/sys/class/input/event0/` subtree a
/// `libudev` input client enumerates for the one node [`InputDevices`] exposes --
/// `uevent`/`dev`/`subsystem`, never general sysfs. One device only, so unlike [`SysClassDrm`]
/// there is no device-selector enum.
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
    /// `226:0` -- the virtual DRM primary node, target `../../class/drm/card0`. Required by
    /// wlroots' `drmGetDeviceNameFromFd2()` (labwc/sway), which weston never calls -- so a
    /// weston-only check passes with this entry missing while labwc aborts backend creation.
    /// See gm mutable mut-1789043688678.
    Drm,
    /// `226:128` -- the virtual DRM render node, target `../../class/drm/renderD128`. The same
    /// reverse lookup as [`SysDevCharEntry::Drm`], needed by a later wlroots path: the GBM/EGL
    /// render-node open, which reports `drmGetDevice2 failed` without it.
    /// See gm mutable mut-1789043688678.
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

/// A [`super::backend::Backend`] serving `/sys/dev/char/<major>:<minor>` -- the sysfs reverse
/// lookup from a char device's `(major, minor)` back to its `/sys/class/*` directory. `libudev`'s
/// `udev_device_new_from_devnum()` reads it; without it seatd silently closes the fd it just
/// opened, and wlroots' `drmGetDeviceNameFromFd2()` fails.
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

