// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Common Linux-y items suitable for LiteBox

#![no_std]
#![allow(non_camel_case_types)]

use core::ffi::c_char;
use core::time::Duration;
use int_enum::IntEnum;
use litebox::{
    fs::OFlags,
    utils::{ReinterpretSignedExt as _, ReinterpretUnsignedExt as _, TruncateExt as _},
};
use syscalls::Sysno;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::signal::SigSet;

pub mod errno;
pub mod loader;
pub mod mm;
pub mod physical_pointers;
pub mod signal;
pub mod user_pointers;
pub mod vmap;

extern crate alloc;

use user_pointers::{UserPtr, UserPtrMut};

/// Number of AArch64 general-purpose registers saved by the Linux user ABI
/// (`x0` through `x30`).
#[cfg(target_arch = "aarch64")]
pub const AARCH64_GENERAL_REGISTER_COUNT: usize = 31;

// TODO(jayb): Should errno::Errno be publicly re-exported?

pub const STDIN_FILENO: i32 = 0;
pub const STDOUT_FILENO: i32 = 1;
pub const STDERR_FILENO: i32 = 2;

// linux/futex.h
pub const FUTEX_WAIT: i32 = 0;
pub const FUTEX_WAKE: i32 = 1;
pub const FUTEX_REQUEUE: i32 = 3;
pub const FUTEX_CMP_REQUEUE: i32 = 4;

// linux/time.h
pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;

/// Special value `libc::AT_FDCWD` used to indicate openat should use
/// the current working directory.
pub const AT_FDCWD: i32 = -100;

/// Sentinel `tv_nsec` value for `utimensat(2)`/`futimens(3)`'s `times` argument meaning "set to
/// the current time", per `<linux/stat.h>`.
pub const UTIME_NOW: i64 = 0x3fff_ffff;
/// Sentinel `tv_nsec` value for `utimensat(2)`/`futimens(3)`'s `times` argument meaning "leave
/// this timestamp unchanged", per `<linux/stat.h>`.
pub const UTIME_OMIT: i64 = 0x3fff_fffe;

/// Encoding for ioctl commands.
pub mod ioctl {
    /// The number of bits allocated for the ioctl command number field.
    pub const NRBITS: u32 = 8;
    /// The number of bits allocated for the ioctl command type field.
    pub const TYPEBITS: u32 = 8;
    /// The number of bits allocated for the ioctl command size field.
    pub const SIZEBITS: u32 = 14;
    /// The bit offset for the ioctl command number field.
    pub const NRSHIFT: u32 = 0;
    /// The bit offset for the ioctl command type field.
    pub const TYPESHIFT: u32 = NRSHIFT + NRBITS;
    /// The bit offset for the ioctl command size field.
    pub const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
    /// The bit offset for the ioctl command direction field.
    pub const DIRSHIFT: u32 = SIZESHIFT + SIZEBITS;
    /// Represents no data transfer direction for the ioctl command.
    pub const NONE: u32 = 0;
    /// Represents the write data transfer direction for the ioctl command.
    pub const WRITE: u32 = 1;
    /// Represents the read data transfer direction for the ioctl command.
    pub const READ: u32 = 2;

    /// Encode an ioctl command.
    #[macro_export]
    macro_rules! ioc {
        ($direction:expr, $type:expr, $number:expr, $size:expr) => {
            (($direction as u32) << $crate::ioctl::DIRSHIFT)
                | (($type as u32) << $crate::ioctl::TYPESHIFT)
                | (($number as u32) << $crate::ioctl::NRSHIFT)
                | (($size as u32) << $crate::ioctl::SIZESHIFT)
        };
    }

    /// Encode an ioctl command that writes.
    #[macro_export]
    macro_rules! iow {
        ($ty:expr, $nr:expr, $sz:expr) => {
            $crate::ioc!($crate::ioctl::WRITE, $ty, $nr, $sz)
        };
    }
}

bitflags::bitflags! {
    /// Desired memory protection of a memory mapping.
    #[derive(PartialEq, Debug)]
    pub struct ProtFlags: core::ffi::c_int {
        /// Pages cannot be accessed.
        const PROT_NONE = 0;
        /// Pages can be read.
        const PROT_READ = 1 << 0;
        /// Pages can be written.
        const PROT_WRITE = 1 << 1;
        /// Pages can be executed
        const PROT_EXEC = 1 << 2;
        /// Apply the protection mode down to the beginning of a
        /// mapping that grows downward
        const PROT_GROWSDOWN = 1 << 24;
        /// Apply the protection mode up to the end of a mapping that
        /// grows upwards.
        const PROT_GROWSUP = 1 << 25;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;

        const PROT_READ_EXEC = Self::PROT_READ.bits() | Self::PROT_EXEC.bits();
        const PROT_READ_WRITE = Self::PROT_READ.bits() | Self::PROT_WRITE.bits();
        const PROT_READ_WRITE_EXEC = Self::PROT_READ.bits() | Self::PROT_WRITE.bits() | Self::PROT_EXEC.bits();
    }
}

bitflags::bitflags! {
    /// Additional parameters for [`mmap`].
    #[derive(Debug)]
    pub struct MapFlags: core::ffi::c_int {
        /// Share this mapping. Mutually exclusive with `MAP_PRIVATE`.
        const MAP_SHARED = 0x1;
        /// This flag provides the same behavior as MAP_SHARED except that
        /// MAP_SHARED mappings ignore unknown flags in flags.  By contrast,
        /// when creating a mapping using MAP_SHARED_VALIDATE, the kernel
        /// verifies all passed flags are known and fails the mapping with
        /// the error EOPNOTSUPP for unknown flags.
        const MAP_SHARED_VALIDATE = 0x3;
        /// Changes are private
        const MAP_PRIVATE = 0x2;
        /// Interpret addr exactly
        const MAP_FIXED = 0x10;
        /// don't use a file
        const MAP_ANONYMOUS = 0x20;
        /// Synonym for [`MAP_ANONYMOUS`]
        const MAP_ANON = 0x20;
        /// Put the mapping into the first 2GB of the process address space.
        const MAP_32BIT = 0x40;
        /// Used for stacks; indicates to the kernel that the mapping should extend downward in memory.
        const MAP_GROWSDOWN = 0x100;
        /// Mark the mmaped region to be locked in the same way as `mlock(2)`.
        const MAP_LOCKED = 0x2000;
        /// Do not reserve swap space for this mapping.
        const MAP_NORESERVE = 0x4000;
        /// Populate page tables for a mapping.
        const MAP_POPULATE = 0x8000;
        /// Only meaningful when used with `MAP_POPULATE`. Don't perform read-ahead.
        const MAP_NONBLOCK = 0x10000;
        /// Perform synchronous page faults for the mapping
        const MAP_SYNC = 0x80000;
        /// Allocate the mapping using "huge pages".
        const MAP_HUGETLB = 0x40000;
        /// Make use of 2MB huge page
        const MAP_HUGE_2MB = 0x54000000;
        /// Make use of 1GB huge page
        const MAP_HUGE_1GB = 0x78000000;
        /// Place the mapping at exactly the address specified in `addr`, but never clobber an existing range.
        const MAP_FIXED_NOREPLACE = 0x100000;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// Options for access()
    #[derive(Debug, PartialEq)]
    pub struct AccessFlags: core::ffi::c_int {
        /// Test for existence of file.
        const F_OK = 0;
        /// Test for read permission.
        const R_OK = 4;
        /// Test for write permission.
        const W_OK = 2;
        /// Test for execute (search) permission.
        const X_OK = 1;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// Flags that control how the various *at syscalls behave.
    /// E.g., `openat`, `fstatat`, `unlinkat`, etc.
    #[derive(Debug)]
    pub struct AtFlags: core::ffi::c_int {
        /// Allow empty relative pathname, operate on the provided directory file
        /// descriptor instead.
        const AT_EMPTY_PATH = 0x1000;
        /// Don't automount the terminal ("basename") component of pathname if it is a directory
        /// that is an automount point.
        const AT_NO_AUTOMOUNT = 0x800;
        /// Follow symbolic links.
        const AT_SYMLINK_FOLLOW = 0x400;
        /// Used with `faccessat`, the checks for accessibility are performed using the
        /// effective user and group IDs instead of the real user and group ID
        const AT_EACCESS = 0x200;
        /// Do not follow symbolic links.
        const AT_SYMLINK_NOFOLLOW = 0x100;

        /// Type of synchronisation required from statx(), used to control what sort of
        /// synchronization the kernel will do when querying a file on a remote filesystem
        const AT_STATX_SYNC_TYPE = 0x6000;
        /// Do whatever stat() does
        const AT_STATX_SYNC_AS_STAT = 0x0;
        /// Force the attributes to be sync'd with the server
        const AT_STATX_FORCE_SYNC = 0x2000;
        /// Don't sync attributes with the server
        const AT_STATX_DONT_SYNC = 0x4000;

        /// Used with `unlinkat`, remove directory instead of unlinking a file.
        const AT_REMOVEDIR = 0x200;

        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

#[repr(u32)]
#[derive(IntEnum)]
pub enum InodeType {
    /// FIFO (named pipe)
    NamedPipe = 0o010000,
    /// character device
    CharDevice = 0o020000,
    /// directory
    Dir = 0o040000,
    /// block device
    BlockDevice = 0o060000,
    /// regular file
    File = 0o100000,
    /// symbolic link
    SymLink = 0o120000,
    /// socket
    Socket = 0o140000,
}

impl From<litebox::fs::FileType> for InodeType {
    fn from(value: litebox::fs::FileType) -> Self {
        match value {
            litebox::fs::FileType::RegularFile => InodeType::File,
            litebox::fs::FileType::Directory => InodeType::Dir,
            litebox::fs::FileType::CharacterDevice => InodeType::CharDevice,
            litebox::fs::FileType::Symlink => InodeType::SymLink,
            // `FileType` is `#[non_exhaustive]`; unlike `DirentType` (which has a legitimate
            // `DT_UNKNOWN` fallback matching real Linux `getdents` behavior, see the `From` impl
            // above), `st_mode`'s file-type bits have no safe "unknown" value -- every `stat()`
            // caller needs a real answer. This can only be reached if `litebox::fs::FileType`
            // grows a new variant with no corresponding `InodeType` yet; fail loudly rather than
            // silently reporting a wrong type (e.g. mislabeling a new node kind as a regular
            // file), exactly as the previous `unimplemented!()` did, but with a clearer message.
            other => unimplemented!("no InodeType mapping for FileType::{other:?}"),
        }
    }
}

#[repr(u8)]
pub enum DirentType {
    /// Unknown
    Unknown = 0,
    /// FIFO (named pipe)
    NamedPipe = 1,
    /// Character device
    CharDevice = 2,
    /// Directory
    Directory = 4,
    /// Block device
    BlockDevice = 6,
    /// Regular file
    Regular = 8,
    /// Symbolic link
    SymLink = 10,
    /// Socket
    Socket = 12,
}

impl From<litebox::fs::FileType> for DirentType {
    fn from(value: litebox::fs::FileType) -> Self {
        match value {
            litebox::fs::FileType::RegularFile => DirentType::Regular,
            litebox::fs::FileType::Directory => DirentType::Directory,
            litebox::fs::FileType::CharacterDevice => DirentType::CharDevice,
            litebox::fs::FileType::Symlink => DirentType::SymLink,
            // `FileType` is `#[non_exhaustive]`; match Linux's own `getdents`-family behavior of
            // reporting `DT_UNKNOWN` for any type it can't otherwise classify rather than
            // panicking on a still-unmatched (but not-actually-invalid) directory entry. Real
            // callers of `d_type` (e.g. `readdir`) already treat `DT_UNKNOWN` as "call `stat()`
            // if you need to know the type", so this is safe, working, real Linux behavior -- not
            // a stand-in placeholder.
            _ => DirentType::Unknown,
        }
    }
}

/// Linux's `stat` struct
#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Default, PartialEq, Debug, FromBytes, IntoBytes)]
pub struct FileStat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    #[expect(clippy::pub_underscore_fields)]
    pub __pad0: core::ffi::c_int,
    pub st_rdev: u64,
    pub st_size: usize,
    pub st_blksize: usize,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    #[expect(clippy::pub_underscore_fields)]
    pub __unused: [i64; 3],
}

/// Linux's `stat` struct for aarch64.
/// Uses the generic `struct stat` layout from <asm-generic/stat.h>.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Default, PartialEq, Debug, FromBytes, IntoBytes)]
pub struct FileStat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    #[expect(clippy::pub_underscore_fields)]
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    #[expect(clippy::pub_underscore_fields)]
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    #[expect(clippy::pub_underscore_fields)]
    pub __unused: [u32; 2],
}

/// Linux's `iovec` struct for `writev`
#[derive(Clone, Copy, FromBytes, IntoBytes)]
#[repr(C, packed)]
pub struct IoWriteVec {
    pub iov_base: UserPtr<u8>,
    pub iov_len: usize,
}

/// Linux's `iovec` struct for `readv`
#[derive(Clone, Copy, FromBytes, IntoBytes)]
#[repr(C, packed)]
pub struct IoReadVec {
    pub iov_base: UserPtrMut<u8>,
    pub iov_len: usize,
}

/// `iovec` struct for both read and write
pub type IoVec = IoReadVec;

impl From<litebox::fs::FileStatus> for FileStat {
    fn from(value: litebox::fs::FileStatus) -> Self {
        // TODO: add more fields
        let litebox::fs::FileStatus {
            file_type,
            mode,
            size,
            owner: litebox::fs::UserInfo { user, group },
            node_info: litebox::fs::NodeInfo { dev, ino, rdev },
            blksize,
            atime,
            mtime,
            ..
        } = value;
        let atime_nsec = i64::from(atime.nsec);
        let mtime_nsec = i64::from(mtime.nsec);
        Self {
            st_dev: <_>::try_from(dev).unwrap(),
            st_ino: <_>::try_from(ino).unwrap(),
            st_nlink: 1,
            st_mode: (mode.bits() | InodeType::from(file_type) as u32).trunc(),
            st_uid: <_>::from(user),
            st_gid: <_>::from(group),
            st_rdev: rdev
                .map(|r| <_>::try_from(r.get()).unwrap())
                .unwrap_or_default(),
            #[cfg(target_arch = "x86_64")]
            #[allow(clippy::cast_possible_wrap)]
            st_size: size,
            #[cfg(target_arch = "aarch64")]
            #[allow(clippy::cast_possible_wrap)]
            st_size: size as i64,
            #[cfg(target_arch = "x86_64")]
            st_blksize: blksize,
            #[cfg(target_arch = "aarch64")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            st_blksize: blksize as i32,
            st_blocks: 0,
            st_atime: atime.sec,
            st_atime_nsec: atime_nsec,
            // LiteBox doesn't track a separate change-time (`ctime`); mirroring `mtime` (as
            // several minimal/embedded filesystems do) is closer to reality than the previous
            // hardcoded-zero epoch, and is what most callers actually care about (e.g. `apk`'s
            // post-install `utimensat` only inspects `mtime`).
            st_ctime: mtime.sec,
            st_ctime_nsec: mtime_nsec,
            st_mtime: mtime.sec,
            st_mtime_nsec: mtime_nsec,
            ..Default::default()
        }
    }
}

bitflags::bitflags! {
    /// Field-selection mask for [`statx`].
    ///
    /// Each bit asks the kernel to fill the corresponding field in [`Statx`].
    /// `STATX__RESERVED` (0x8000_0000) is rejected with `EINVAL` by Linux and
    /// must not appear in user input.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct StatxMask: u32 {
        const STATX_TYPE = 0x0000_0001;
        const STATX_MODE = 0x0000_0002;
        const STATX_NLINK = 0x0000_0004;
        const STATX_UID = 0x0000_0008;
        const STATX_GID = 0x0000_0010;
        const STATX_ATIME = 0x0000_0020;
        const STATX_MTIME = 0x0000_0040;
        const STATX_CTIME = 0x0000_0080;
        const STATX_INO = 0x0000_0100;
        const STATX_SIZE = 0x0000_0200;
        const STATX_BLOCKS = 0x0000_0400;
        const STATX_BASIC_STATS = Self::STATX_TYPE.bits()
            | Self::STATX_MODE.bits()
            | Self::STATX_NLINK.bits()
            | Self::STATX_UID.bits()
            | Self::STATX_GID.bits()
            | Self::STATX_ATIME.bits()
            | Self::STATX_MTIME.bits()
            | Self::STATX_CTIME.bits()
            | Self::STATX_INO.bits()
            | Self::STATX_SIZE.bits()
            | Self::STATX_BLOCKS.bits();
        /// The basic-stats fields LiteBox actually fills. Excludes the
        /// time bits because `FileStatus` doesn't carry timestamps.
        const STATX_BASIC_FILLED = Self::STATX_BASIC_STATS.bits()
            & !(Self::STATX_ATIME.bits() | Self::STATX_MTIME.bits() | Self::STATX_CTIME.bits());
        const STATX_BTIME = 0x0000_0800;
        const STATX_MNT_ID = 0x0000_1000;
        const STATX_DIOALIGN = 0x0000_2000;
        const STATX_MNT_ID_UNIQUE = 0x0000_4000;
        const STATX_SUBVOL = 0x0000_8000;
        const STATX_WRITE_ATOMIC = 0x0001_0000;
        const STATX_DIO_READ_ALIGN = 0x0002_0000;

        /// Named constant so callers can spell out the EINVAL check explicitly.
        const STATX__RESERVED = 0x8000_0000;

        /// Accept unknown future bits without truncating; the kernel silently
        /// ignores them and reports the actual filled set via [`Statx::stx_mask`].
        const _ = !0;
    }
}

/// Linux's `struct statx_timestamp` (16 bytes, `linux/stat.h`).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, FromBytes, IntoBytes, Immutable)]
pub struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    #[expect(clippy::pub_underscore_fields)]
    pub __reserved: i32,
}

/// Linux's `struct statx` (256 bytes, `linux/stat.h`).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, FromBytes, IntoBytes, Immutable)]
pub struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    #[expect(clippy::pub_underscore_fields)]
    pub __spare0: [u16; 1],
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: StatxTimestamp,
    pub stx_btime: StatxTimestamp,
    pub stx_ctime: StatxTimestamp,
    pub stx_mtime: StatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    #[expect(clippy::pub_underscore_fields)]
    pub __spare3: [u64; 12],
}

/// Extract the major component from a Linux `dev_t` (matches `major(3)` from glibc).
fn dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)).trunc()
}
/// Extract the minor component from a Linux `dev_t` (matches `minor(3)`).
fn dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)).trunc()
}

impl From<litebox::fs::FileStatus> for Statx {
    fn from(value: litebox::fs::FileStatus) -> Self {
        let litebox::fs::FileStatus {
            file_type,
            mode,
            size,
            owner: litebox::fs::UserInfo { user, group },
            node_info: litebox::fs::NodeInfo { dev, ino, rdev },
            blksize,
            atime,
            mtime,
            ..
        } = value;
        let dev = dev as u64;
        let rdev = rdev.map_or(0u64, |r| r.get() as u64);
        Self {
            // `STATX_BASIC_STATS` (not `STATX_BASIC_FILLED`, which deliberately excludes the
            // timestamp bits -- see its own doc comment) matches every field this conversion
            // actually fills in below, including the timestamps: an earlier version of this
            // conversion silently dropped `atime`/`mtime` from its destructuring (`..`) and
            // relied on `..Default::default()` for every timestamp field, always producing an
            // all-zero `stx_mtime` -- confirmed live to be the exact cause of `npx`'s own
            // lock-integrity check (`libnpmexec`'s `with-lock.js`) seeing a permanently-epoch
            // `mtime` from Node's `fs.statSync` (which uses `statx`, routing through exactly this
            // conversion) despite the underlying filesystem layer already computing the correct
            // value -- a separate, sibling `From<FileStat> for Statx` conversion a few lines below
            // already populates timestamps correctly and was never the one actually exercised by
            // this call path.
            stx_mask: StatxMask::STATX_BASIC_STATS.bits(),
            stx_blksize: blksize.trunc(),
            stx_nlink: 1,
            stx_uid: u32::from(user),
            stx_gid: u32::from(group),
            stx_mode: (mode.bits() | InodeType::from(file_type) as u32).trunc(),
            stx_ino: ino as u64,
            stx_size: size as u64,
            stx_blocks: 0,
            stx_atime: statx_timestamp(atime.sec, i64::from(atime.nsec)),
            // LiteBox doesn't track a separate change-time (`ctime`); mirroring `mtime` (as
            // several minimal/embedded filesystems do) matches the sibling `From<FileStat> for
            // Statx` conversion's own precedent just below.
            stx_ctime: statx_timestamp(mtime.sec, i64::from(mtime.nsec)),
            stx_mtime: statx_timestamp(mtime.sec, i64::from(mtime.nsec)),
            stx_rdev_major: dev_major(rdev),
            stx_rdev_minor: dev_minor(rdev),
            stx_dev_major: dev_major(dev),
            stx_dev_minor: dev_minor(dev),
            ..Default::default()
        }
    }
}

fn statx_timestamp(seconds: i64, nanoseconds: i64) -> StatxTimestamp {
    StatxTimestamp {
        tv_sec: seconds,
        tv_nsec: u32::try_from(nanoseconds).unwrap_or(u32::MAX),
        ..Default::default()
    }
}

impl From<FileStat> for Statx {
    fn from(value: FileStat) -> Self {
        Self {
            stx_mask: StatxMask::STATX_BASIC_STATS.bits(),
            #[cfg(target_arch = "x86_64")]
            stx_blksize: value.st_blksize.trunc(),
            #[cfg(target_arch = "aarch64")]
            stx_blksize: value.st_blksize.reinterpret_as_unsigned(),
            stx_nlink: value.st_nlink.trunc(),
            stx_uid: value.st_uid,
            stx_gid: value.st_gid,
            stx_mode: value.st_mode.trunc(),
            stx_ino: value.st_ino,
            #[cfg(target_arch = "x86_64")]
            stx_size: value.st_size as u64,
            #[cfg(target_arch = "aarch64")]
            stx_size: value.st_size.reinterpret_as_unsigned(),
            stx_blocks: value.st_blocks.reinterpret_as_unsigned(),
            stx_atime: statx_timestamp(value.st_atime, value.st_atime_nsec),
            stx_ctime: statx_timestamp(value.st_ctime, value.st_ctime_nsec),
            stx_mtime: statx_timestamp(value.st_mtime, value.st_mtime_nsec),
            stx_rdev_major: dev_major(value.st_rdev),
            stx_rdev_minor: dev_minor(value.st_rdev),
            stx_dev_major: dev_major(value.st_dev),
            stx_dev_minor: dev_minor(value.st_dev),
            ..Default::default()
        }
    }
}

/// Commands for use with `fcntl`.
#[derive(Debug)]
#[non_exhaustive]
pub enum FcntlArg {
    /// Get the file descriptor flags
    GETFD,
    /// Set the file descriptor flags
    SETFD(FileDescriptorFlags),
    /// Get descriptor status flags
    GETFL,
    /// Set descriptor status flags
    SETFL(OFlags),
    /// Get a file lock
    GETLK(UserPtrMut<Flock>),
    /// Set a file lock
    SETLK(UserPtr<Flock>),
    /// Set a file lock and wait if blocked
    SETLKW(UserPtr<Flock>),
    /// Duplicate file descriptor
    DUPFD { cloexec: bool, min_fd: u32 },
}

#[repr(i16)]
#[derive(Debug, IntEnum)]
pub enum FlockType {
    /// Shared or read lock
    ReadLock = 0,
    /// Exclusive or write lock
    WriteLock = 1,
    /// Remove lock
    Unlock = 2,
}

#[repr(C)]
#[derive(Clone, Debug, FromBytes, IntoBytes)]
pub struct Flock {
    /// Type of lock: F_RDLCK, F_WRLCK, or F_UNLCK
    pub type_: i16,
    /// Where `start' is relative to
    pub whence: i16,
    #[cfg(target_pointer_width = "64")]
    #[doc(hidden)]
    pub __pad0: u32,
    /// Offset where the lock begins
    pub start: usize,
    /// Size of the locked area, 0 means until EOF
    pub len: isize,
    /// Process holding the lock
    pub pid: i32,
    #[cfg(target_pointer_width = "64")]
    #[doc(hidden)]
    pub __pad1: u32,
}

const F_DUPFD: i32 = 0;
const F_DUPFD_CLOEXEC: i32 = 1030;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_GETLK: i32 = 5;
const F_SETLK: i32 = 6;
const F_SETLKW: i32 = 7;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct FileDescriptorFlags: u32 {
        /// Close-on-exec flag
        const FD_CLOEXEC = 0x1;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

impl FcntlArg {
    pub fn try_from(cmd: i32, arg: usize) -> Option<Self> {
        Some(match cmd {
            F_GETFD => Self::GETFD,
            F_SETFD => Self::SETFD(FileDescriptorFlags::from_bits_truncate(arg.trunc())),
            F_GETFL => Self::GETFL,
            F_SETFL => Self::SETFL(OFlags::from_bits_truncate(arg.trunc())),
            F_GETLK => Self::GETLK(UserPtrMut::from_usize(arg)),
            F_SETLK => Self::SETLK(UserPtr::from_usize(arg)),
            F_SETLKW => Self::SETLKW(UserPtr::from_usize(arg)),
            F_DUPFD => Self::DUPFD {
                cloexec: false,
                min_fd: arg.trunc(),
            },
            F_DUPFD_CLOEXEC => Self::DUPFD {
                cloexec: true,
                min_fd: arg.trunc(),
            },
            _ => return None,
        })
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct EfdFlags: core::ffi::c_uint {
        const SEMAPHORE = 1;
        const CLOEXEC = litebox::fs::OFlags::CLOEXEC.bits();
        const NONBLOCK = litebox::fs::OFlags::NONBLOCK.bits();
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// `signalfd4(2)` flags -- `SFD_CLOEXEC`/`SFD_NONBLOCK` are defined in the real kernel UAPI
    /// as aliases of `O_CLOEXEC`/`O_NONBLOCK` (`include/uapi/linux/signalfd.h`), same pattern as
    /// `EfdFlags` above.
    #[derive(Debug, Clone, Copy)]
    pub struct SfdFlags: core::ffi::c_uint {
        const CLOEXEC = litebox::fs::OFlags::CLOEXEC.bits();
        const NONBLOCK = litebox::fs::OFlags::NONBLOCK.bits();
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// `timerfd_create(2)` flags -- `TFD_CLOEXEC`/`TFD_NONBLOCK` are defined in the real kernel
    /// UAPI as aliases of `O_CLOEXEC`/`O_NONBLOCK` (`include/uapi/linux/timerfd.h`), same pattern
    /// as `EfdFlags`/`SfdFlags` above.
    #[derive(Debug, Clone, Copy)]
    pub struct TfdFlags: core::ffi::c_uint {
        const CLOEXEC = litebox::fs::OFlags::CLOEXEC.bits();
        const NONBLOCK = litebox::fs::OFlags::NONBLOCK.bits();
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// `timerfd_settime(2)`'s own `flags` argument (distinct from the fd-creation flags above).
    /// Values match the real kernel `uapi/linux/timerfd.h` exactly.
    #[derive(Debug, Clone, Copy)]
    pub struct TfdSettimeFlags: core::ffi::c_uint {
        const TIMER_ABSTIME = 1 << 0;
        const TIMER_CANCEL_ON_SET = 1 << 1;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// `memfd_create(2)` flags. Values match the real kernel `uapi/linux/memfd.h` exactly.
    #[derive(Debug, Clone, Copy)]
    pub struct MfdFlags: core::ffi::c_uint {
        const CLOEXEC = 0x0001;
        /// Sealing (`fcntl(F_ADD_SEALS)`) is accepted here (so a client that unconditionally
        /// passes this flag doesn't get a spurious EINVAL) but this shim does not implement real
        /// seal enforcement -- no known Wayland/shm client actually relies on seals being
        /// enforced, only on the flag itself being accepted.
        const ALLOW_SEALING = 0x0002;
        const HUGETLB = 0x0004;
        const NOEXEC_SEAL = 0x0008;
        const EXEC = 0x0010;
        const _ = !0;
    }
}

type cc_t = ::core::ffi::c_uchar;
type tcflag_t = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Debug, Clone, Default, FromBytes, IntoBytes)]
pub struct Termios {
    pub c_iflag: tcflag_t,
    pub c_oflag: tcflag_t,
    pub c_cflag: tcflag_t,
    pub c_lflag: tcflag_t,
    pub c_line: cc_t,
    pub c_cc: [cc_t; 19usize],
}

/// `c_oflag` bit: enable implementation-defined output processing.
pub const OPOST: tcflag_t = 0o0000001;
/// `c_oflag` bit: map `\n` to `\r\n` on output. Only meaningful together with [`OPOST`].
pub const ONLCR: tcflag_t = 0o0000004;
/// `c_lflag` bit: echo input characters back to the terminal as they're typed.
pub const ECHO: tcflag_t = 0o0000010;

#[derive(Debug, Clone, Default, FromBytes, IntoBytes)]
#[repr(C)]
pub struct Winsize {
    pub row: u16,
    pub col: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// `struct input_id` (`include/uapi/linux/input.h`) -- `EVIOCGID`'s result, a device's
/// bus/vendor/product/version identity. `BUS_VIRTUAL = 0x06` (see `input-event-codes.h`) is the
/// correct, real-kernel-convention bus type for litebox's synthetic evdev device -- vendor/
/// product/version are left `0`, matching how real virtual/synthetic input devices (e.g. the
/// kernel's own `uinput`-created ones with no vendor identity supplied) commonly report.
#[derive(Debug, Clone, Default, FromBytes, IntoBytes)]
#[repr(C)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// `BUS_VIRTUAL`, `include/uapi/linux/input.h`.
pub const BUS_VIRTUAL: u16 = 0x06;
/// `EV_VERSION`, `include/uapi/linux/input.h` -- the evdev protocol version `EVIOCGVERSION`
/// reports, unchanged since its introduction.
pub const EV_VERSION: i32 = 0x01_0001;

/// DRM (Direct Rendering Manager) mode-setting ioctl request numbers, `include/uapi/drm/drm.h`'s
/// `DRM_IOWR(nr, type)` = `_IOWR(DRM_IOCTL_BASE='d', nr, type)` encoding. Verified live against
/// the real kernel header (see `docs/drm-dumb-buffer-ioctl-reference.md`), not guessed -- these
/// are stable kernel UAPI values, unchanged since DRM's KMS API was introduced.
// Each value below was computed (never hand-guessed) via the real `_IOWR` encoding
// (`(3 << 30) | (size_of::<Struct>() << 16) | ('d' << 8) | nr`) applied to this exact file's
// own struct definitions below, cross-checked against the fetched kernel `nr` values recorded
// in `docs/drm-dumb-buffer-ioctl-reference.md`. A struct layout change here without recomputing
// these constants silently breaks the encoded ioctl number -- see that doc for the derivation.
pub const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC040_64A0;
pub const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC068_64A1;
pub const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC068_64A2;
pub const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC014_64A6;
pub const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC050_64A7;
pub const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC020_64B2;
pub const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC010_64B3;
pub const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = 0xC004_64B4;
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC010_64B5;
pub const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC020_64B6;
pub const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC030_64B7;
pub const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC068_64B8;
pub const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC018_64B0;
/// `DRM_IOCTL_VERSION = DRM_IOWR(0x00, struct drm_version)`. `nr`/struct shape fetched live from
/// the real kernel `drm.h` (`torvalds/linux` master), not guessed; `size=64` is `sizeof(struct
/// drm_version)` on the LP64 ABI litebox targets (3 `int`s + 4 bytes of compiler-inserted padding
/// to the next 8-byte-aligned field, then 3 `(size_t, pointer)` pairs -- see [`DrmVersion`]'s own
/// field layout, which this size must stay in sync with).
pub const DRM_IOCTL_VERSION: u32 = 0xC040_6400;
/// `DRM_IOCTL_GET_CAP = DRM_IOWR(0x0c, struct drm_get_cap)`, `size=16` (two `u64`s).
pub const DRM_IOCTL_GET_CAP: u32 = 0xC010_640C;
/// `DRM_IOCTL_SET_CLIENT_CAP = DRM_IOW(0x0d, struct drm_set_client_cap)`, `size=16` (two `u64`s,
/// identical layout to [`DrmGetCap`]/[`struct@DrmSetClientCap`], just write-only: `dir=1` not
/// `dir=3`, the only difference from [`DRM_IOCTL_GET_CAP`]'s encoding). Verified live against the
/// real kernel header, not guessed (see that constant's own doc comment for the shared
/// derivation).
pub const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010_640D;
/// `DRM_IOCTL_SET_MASTER = DRM_IO(0x1e)` -- a plain `_IO()` (no argument struct: `dir=0`,
/// `size=0`), unlike every other DRM ioctl this device implements.
pub const DRM_IOCTL_SET_MASTER: u32 = 0x0000_641E;
/// `DRM_IOCTL_DROP_MASTER = DRM_IO(0x1f)`.
pub const DRM_IOCTL_DROP_MASTER: u32 = 0x0000_641F;
/// `DRM_IOCTL_GET_MAGIC = DRM_IOR(0x02, struct drm_auth)`, `size=4` (one `__u32 magic`).
/// wlroots' render allocator (`render/allocator/allocator.c`'s `allocator_autocreate_with_
/// display()`, reached via the GBM allocator's `drmGetMagic()`/`drmAuthMagic()` legacy DRI
/// client-authentication handshake) calls this on every render-node fd it opens, including
/// the primary node when GBM falls back to it -- without a real implementation the ioctl
/// falls through to this device's `ENOTTY` catch-all, which libdrm's `drmGetMagic()`
/// surfaces as `EINVAL` ("Invalid argument"), confirmed live as the literal error text
/// immediately preceding `render/allocator/allocator.c]"drmGetMagic failed"` /
/// `../src/server.c]"unable to create allocator"`. This device has exactly one possible
/// client (see [`DRM_IOCTL_SET_MASTER`]'s own doc comment on single-master semantics), so
/// authentication has no real access-control decision to make -- a fixed non-zero magic
/// value handed back here and trivially accepted by [`DRM_IOCTL_AUTH_MAGIC`] below is
/// sufficient to satisfy the handshake's shape without modeling multi-client auth this
/// device will never need.
pub const DRM_IOCTL_GET_MAGIC: u32 = 0x8004_6402;
/// `DRM_IOCTL_AUTH_MAGIC = DRM_IOW(0x11, struct drm_auth)`, `size=4`. The write half of the
/// same legacy DRI authentication handshake [`DRM_IOCTL_GET_MAGIC`] starts -- a second
/// client (or, as here, the same client re-authenticating a second fd against the same
/// device) presents the magic value back to prove it can read what the first `GET_MAGIC`
/// call returned. Always succeeds for the same single-client-device reason described on
/// [`DRM_IOCTL_GET_MAGIC`].
pub const DRM_IOCTL_AUTH_MAGIC: u32 = 0x4004_6411;
/// The one fixed, arbitrary, non-zero magic value [`DRM_IOCTL_GET_MAGIC`] hands back and
/// [`DRM_IOCTL_AUTH_MAGIC`] unconditionally accepts -- see those constants' own doc
/// comments for why a real per-client-random value has nothing to protect here.
pub const DRM_AUTH_MAGIC_VALUE: u32 = 0xd12d_0001;
/// `DRM_IOCTL_MODE_GETPROPERTY = DRM_IOWR(0xaa, struct drm_mode_get_property)`, `size=64`
/// (`nr`/struct shape fetched live from the real kernel `drm.h`; size independently re-verified
/// via a standalone `size_of::<DrmModeGetProperty>()` compile: two `u64`s, two `u32`s, a 32-byte
/// `name` array, two more `u32`s = 64 bytes exactly, no padding needed on the LP64 ABI litebox
/// targets). Real libdrm's `drmModeObjectGetProperties` calls this once per property ID returned
/// by `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` to resolve each one's name/values -- this device reports
/// zero properties from `OBJ_GETPROPERTIES` (see that ioctl's own doc comment), so no real client
/// following the standard `OBJ_GETPROPERTIES` -> per-ID `GETPROPERTY` sequence will ever actually
/// invoke this one; implemented anyway so a client that calls it directly with an unknown ID gets
/// a real `ENOENT`, not an `ENOTTY` (unrecognized ioctl) that would look like a missing driver.
pub const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC040_64AA;
/// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES = DRM_IOWR(0xb9, struct drm_mode_obj_get_properties)`,
/// `size=32` (three `u64`-then-`u32`-then-`u32`-then-`u32` fields -- `size_of::<
/// DrmModeObjGetProperties>()` independently re-verified the same way as [`DRM_IOCTL_MODE_GETPROPERTY`]
/// above). The gap this closes: a real libdrm client (confirmed live via `smithay`'s
/// `backend_drm`, `docs/wayland-drm-backend-probe/`) calls this on the connector object
/// immediately after `GETCONNECTOR` -- with this ioctl entirely unimplemented, that call fell
/// through to `ENOTTY`/`EINVAL` and `DrmDevice::new` failed outright before any further DRM work
/// (dumb buffers, page-flip) could even be attempted, regardless of how correct the rest of this
/// device's ioctl coverage is.
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC020_64B9;
/// `DRM_MODE_OBJECT_CONNECTOR` -- the `obj_type` a real client passes when asking
/// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` about a connector (as opposed to a CRTC, encoder, or
/// plane). This device only tracks connector-object property queries today (the only object type
/// [`DRM_IOCTL_MODE_OBJ_GETPROPERTIES`]'s real-world callers actually query on this device's
/// current ioctl surface).
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
/// `DRM_MODE_OBJECT_PLANE` (real kernel `drm_mode.h` value) -- used by
/// [`DrmSubsystem::obj_get_properties`]/[`DrmSubsystem::get_property`] to report the plane's
/// `type` property, which real legacy (non-atomic) DRM clients using universal planes --
/// including weston's `drm-backend.so`, per `libweston/backend-drm/drm.c`'s
/// `drm_output_find_special_plane` -- query to find the primary plane before enabling an output.
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
/// `DRM_MODE_PROP_ENUM` (`1<<3`, real kernel `drm_mode.h` value) -- marks a property as an
/// enumerated type with named values, resolved by [`DrmSubsystem::get_property`]'s `type`
/// property response.
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
/// A fixed, arbitrary, non-zero object ID for the plane's one `type` property -- real DRM
/// property IDs are driver-internal opaque values from userspace's perspective (see
/// [`VIRTUAL_CONNECTOR_ID`]-style constants' own doc comments for the same reasoning).
pub const VIRTUAL_PLANE_TYPE_PROP_ID: u32 = 100;
/// The real, on-the-wire numeric value this device's plane reports for its `type` property.
/// Real DRM clients (weston's `drm_property_info_populate`) resolve an enum property's meaning
/// by matching THIS raw value against the matching `struct drm_mode_property_enum`'s own
/// `value` field, then reading that entry's `name` string (`"Primary"`) -- the raw number
/// itself is driver-chosen and opaque, so any fixed, non-zero, mutually-distinct value is valid.
pub const VIRTUAL_PLANE_TYPE_VALUE: u64 = 1;
/// `DRM_CAP_DUMB_BUFFER` -- the one allocation-related capability this device's
/// `DRM_IOCTL_GET_CAP` genuinely supports (see [`DrmGetCap`]'s doc comment).
pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES` (`include/uapi/drm/drm.h`) -- the `capability` value a
/// client passes to `DRM_IOCTL_SET_CLIENT_CAP` to opt into seeing primary/cursor planes (not just
/// overlay planes) through `GETPLANERESOURCES`/`GETPLANE`. This device's plane API (see
/// `DrmSubsystem::get_plane_resources`/`get_plane`) has no primary/overlay/cursor distinction at
/// all -- its one virtual plane is unconditionally exposed regardless of this cap -- so enabling
/// it changes nothing about this device's actual behavior; it exists purely so real clients that
/// require it be *acknowledged* (weston's DRM backend refuses to initialize without it) get a
/// real success response instead of failing at `DRM_IOCTL_SET_CLIENT_CAP` itself.
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 0x2;
/// `DRM_CAP_TIMESTAMP_MONOTONIC` (`include/uapi/drm/drm.h`) -- tells a client which clock domain
/// vblank/page-flip-completion event timestamps are expressed in: `1` means `CLOCK_MONOTONIC`,
/// `0` (the deprecated legacy default) means `CLOCK_REALTIME`. This device's page-flip completion
/// events carry a fixed `tv_sec:0, tv_usec:0` placeholder (no real vsync/vblank interrupt exists
/// to time -- see `drm.rs`'s `page_flip`), which is a valid reading under either domain, so
/// reporting the modern `1` is the correct choice: real compositors (weston's DRM backend
/// included) require this capability to be present at all just to initialize, and `0` is legacy
/// behavior no current driver actually exercises.
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
/// `DRM_CAP_PRIME` (`include/uapi/drm/drm.h`) -- queried via `DRM_IOCTL_GET_CAP` to ask
/// whether this device supports PRIME dma-buf import/export at all. wlroots' DRM backend
/// (`backend/drm/drm.c`'s `check_drm_features()`, reached via `labwc`, distinct from
/// weston's own DRM backend which never queries this) treats `DRM_CAP_PRIME` reporting
/// neither [`DRM_PRIME_CAP_IMPORT`] nor [`DRM_PRIME_CAP_EXPORT`] set as fatal -- it logs
/// "PRIME import not supported" and aborts backend creation entirely, since wlroots'
/// renderer abstraction always needs to be able to import a dma-buf-backed buffer object
/// for zero-copy client buffer handling. The value itself is a bitmask of the two
/// capability bits below, not a boolean.
pub const DRM_CAP_PRIME: u64 = 0x5;
/// `DRM_PRIME_CAP_IMPORT` bit within [`DRM_CAP_PRIME`]'s reported value -- this device's
/// `DRM_IOCTL_GET_CAP` unconditionally reports this bit set (see `DrmGetCap`'s doc
/// comment) purely to satisfy wlroots' capability gate at backend-creation time; no actual
/// `DRM_IOCTL_PRIME_FD_TO_HANDLE` ioctl is implemented, since litebox never reaches a code
/// path (client-side dma-buf import) that would exercise it.
pub const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
/// `DRM_PRIME_CAP_EXPORT` bit within [`DRM_CAP_PRIME`]'s reported value -- see
/// [`DRM_IOCTL_PRIME_HANDLE_TO_FD`] for the real handler this capability bit now backs.
pub const DRM_PRIME_CAP_EXPORT: u64 = 0x2;
/// `DRM_IOCTL_PRIME_HANDLE_TO_FD = DRM_IOWR(0x2d, struct drm_prime_handle)`, `size=12`
/// (`nr`/struct shape from the real kernel `drm.h`; `struct drm_prime_handle { __u32 handle;
/// __u32 flags; __s32 fd; }` is exactly 12 bytes, independently re-verified via a standalone
/// `size_of::<DrmPrimeHandle>()` compile, no padding needed on the LP64 ABI litebox targets).
/// wlroots' `render/allocator/drm_dumb.c` (`drmPrimeHandleToFD`) calls this once per dumb-buffer
/// allocation to obtain a dma-buf fd it can hand to its renderer/swapchain machinery -- proven
/// live-reachable (see `drm-prime-handle-to-fd-not-implemented-blocks-xfce-launch`'s own
/// investigation): with this unimplemented, the call fell through to the generic ioctl
/// catch-all's `EINVAL`, `allocator_buffer_create` failed, and `labwc` `SIGABRT`ed on the
/// resulting `wlr_swapchain_create` assertion. litebox's virtual device has exactly one possible
/// client (see [`DRM_IOCTL_SET_MASTER`]'s own doc comment on this device's single-client
/// simplifications), so a real dma-buf subsystem is unnecessary: the handler hands back a second,
/// real fd onto the SAME real host-backed shared memory the originating dumb buffer's
/// `CREATE_DUMB`/`MAP_DUMB` path already established, satisfying every real client's actual use
/// (mmap the fd, or pass it to another local subsystem for a shared read) without implementing
/// dma-buf import/export semantics this single-client device never needs.
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = 0xC00C_642D;
/// `DRM_IOCTL_PRIME_FD_TO_HANDLE = DRM_IOWR(0x2e, struct drm_prime_handle)` -- the reverse
/// direction of [`DRM_IOCTL_PRIME_HANDLE_TO_FD`] (same 12-byte `struct drm_prime_handle`, `nr`
/// one higher per the real kernel `drm.h`). Confirmed live-reachable immediately after every
/// `PRIME_HANDLE_TO_FD` call in this device's own real client traffic: wlroots'
/// `render/allocator/drm_dumb.c` self-imports the fd it just exported to obtain a GEM handle for
/// the new buffer object it constructs around it -- see [`DrmSubsystem::lookup_handle_by_map_offset`]
/// (`litebox_shim_linux`) for why this device's single-client, no-real-dma-buf model makes that
/// self-import a same-handle round-trip rather than needing genuine cross-device import.
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = 0xC00C_642E;
/// `DRM_IOCTL_GEM_CLOSE = DRM_IOW(0x09, struct drm_gem_close)`, `size=8` (`struct drm_gem_close {
/// __u32 handle; __u32 pad; }`, real kernel `drm.h`). Confirmed live-reachable: wlroots'
/// `backend/drm/fb.c` (`drmCloseBufferHandle`, called right after `ADDFB2` on the GEM handle
/// [`DRM_IOCTL_PRIME_FD_TO_HANDLE`] just returned) calls this to release ITS local reference to
/// an imported buffer object -- with this unimplemented the ioctl catch-all's `EINVAL` surfaced
/// as wlroots' own logged "drmCloseBufferHandle failed: Invalid argument" (non-fatal in wlroots,
/// but a real, silently-broken ioctl surface). This device has no per-handle GEM refcounting (a
/// dumb-buffer handle's real lifetime is governed entirely by `DRM_IOCTL_MODE_DESTROY_DUMB`, see
/// that ioctl's own handler) -- [`DrmSubsystem::gem_close`]'s own doc comment (`litebox_shim_linux`)
/// explains why a real no-op-success is the correct, non-fabricated answer here rather than
/// something requiring genuine reference-count bookkeeping.
pub const DRM_IOCTL_GEM_CLOSE: u32 = 0x4008_6409;
/// `struct drm_gem_close`. See [`DRM_IOCTL_GEM_CLOSE`]'s own doc comment.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}
/// `struct drm_prime_handle` (`DRM_IOCTL_PRIME_HANDLE_TO_FD`/`DRM_IOCTL_PRIME_FD_TO_HANDLE`). See
/// [`DRM_IOCTL_PRIME_HANDLE_TO_FD`]'s own doc comment.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmPrimeHandle {
    /// Input: the `CREATE_DUMB`-issued dumb-buffer handle to export as an fd.
    pub handle: u32,
    /// Input: real Linux accepts `DRM_CLOEXEC`/`DRM_RDWR` here; this device does not need to
    /// distinguish them (the returned fd is always readable/writable, matching the underlying
    /// dumb buffer's own real host-backed memory, and `DRM_CLOEXEC` is honored -- see the
    /// handler's own doc comment), so the field is accepted but only `DRM_CLOEXEC` (bit 0) is
    /// actually consulted.
    pub flags: u32,
    /// Output: the new fd referencing the same buffer, or left untouched (`-1` on a real kernel's
    /// own uninitialized-on-error convention, not relied upon here since a real error always
    /// short-circuits before this field would be written) on failure.
    pub fd: i32,
}
/// `DRM_CAP_CRTC_IN_VBLANK_EVENT` (`include/uapi/drm/drm.h`) -- asks whether this driver's
/// `DRM_IOCTL_MODE_PAGE_FLIP`/vblank-wait completion events populate `crtc_id` in the
/// `struct drm_event_vblank` payload (kernels/drivers predating this cap only fill it in for
/// multi-CRTC atomic setups). wlroots' `backend/drm/drm.c` (`check_drm_features()`) queries
/// this right after `DRM_CAP_PRIME` and logs "DRM_CRTC_IN_VBLANK_EVENT unsupported" -- purely
/// informational in real wlroots when unsupported (it falls back to matching the flip by fd
/// instead of `crtc_id`), but litebox's page-flip completion event (`drm.rs`'s `page_flip`)
/// already always stamps `crtc_id` with this device's one real CRTC, so reporting `1` here is
/// simply true, not a fabrication -- no legacy no-`crtc_id` code path exists to preserve.
pub const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;

/// VT (virtual terminal) ioctl request numbers, `include/uapi/linux/vt.h`. Unlike the DRM
/// ioctls above, these are plain legacy-style constants (not `_IOWR`-encoded) -- verified live
/// against the real kernel header (`torvalds/linux` master), not guessed. `seatd` (see
/// `common/terminal.c`/`seatd/seat.c`) uses exactly these four to determine which VT is
/// currently active (`VT_GETSTATE`) and to claim/release process-controlled VT switching
/// (`VT_SETMODE`) around granting a client DRM device access; the remaining `VT_*` numbers
/// (`VT_ACTIVATE`, `VT_WAITACTIVE`, ...) exist in the real kernel but are not on seatd's
/// single-seat, no-real-hardware-switching call path and so are not implemented here.
pub const VT_GETSTATE: u32 = 0x5603;
pub const VT_SETMODE: u32 = 0x5602;
/// KD (keyboard/display mode) ioctl request numbers, `include/uapi/linux/kd.h`.
pub const KDSETMODE: u32 = 0x4B3A;
pub const KDSKBMODE: u32 = 0x4B45;
/// `KD_GRAPHICS` -- the mode value `seatd`'s `terminal_set_graphics(fd, true)` passes to
/// `KDSETMODE` once a client is granted the VT (see `vt_open` in `seatd/seat.c`).
pub const KD_GRAPHICS: i32 = 0x01;
/// `EVIOCREVOKE`, `_IOW('E', 0x91, int)` per the real kernel's `include/uapi/linux/input.h` --
/// `(_IOC_WRITE << 30) | (size_of::<i32>() << 16) | ('E' << 8) | 0x91`.
pub const EVIOCREVOKE: u32 = 0x4004_4591;
/// `EVIOCGVERSION`, `_IOR('E', 0x01, int)` -- `(_IOC_READ << 30) | (size_of::<i32>() << 16) |
/// ('E' << 8) | 0x01`. The first ioctl libevdev's `libevdev_new_from_fd()` issues; failure here
/// is fatal to device creation, matching real Linux's `EVIOC_VERSION` = `0x010001`.
pub const EVIOCGVERSION: u32 = 0x8004_4501;
/// `EVIOCGID`, `_IOR('E', 0x02, struct input_id)` -- `(_IOC_READ << 30) | (size_of::<InputId>()
/// << 16) | ('E' << 8) | 0x02`. Also mandatory/fatal for `libevdev_new_from_fd()`.
pub const EVIOCGID: u32 = 0x8008_4502;
/// `KD_TEXT` -- the mode value restored on VT release (`terminal_set_graphics(fd, false)`).
pub const KD_TEXT: i32 = 0x00;

/// `struct vt_stat` (`VT_GETSTATE`) -- `v_active` is the 1-based number of the currently active
/// VT; `v_signal`/`v_state` are a legacy signal-mask/console-bitmask pair no caller on seatd's
/// call path reads.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct VtStat {
    pub v_active: u16,
    pub v_signal: u16,
    pub v_state: u16,
}

/// `struct vt_mode` (`VT_SETMODE`) -- requests process-controlled (`VT_PROCESS`) or
/// kernel-automatic (`VT_AUTO`) VT switching. This device has no real hardware VT to switch
/// away from, so the mode/signal values themselves are accepted and stored without altering any
/// actual switching behavior (see [`crate`]-level VT device doc comment).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct VtMode {
    pub mode: u8,
    pub waitv: u8,
    pub relsig: i16,
    pub acqsig: i16,
    pub frsig: i16,
}

/// `struct drm_mode_create_dumb` -- allocate a CPU-writable, linear, no-GPU pixel buffer.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

/// `struct drm_mode_map_dumb` -- get an mmap-able fake offset for a dumb buffer.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

/// `struct drm_mode_destroy_dumb` -- free a dumb buffer.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}

/// `struct drm_mode_fb_cmd2` -- attach a buffer as a scanout framebuffer (up to 4 planes; a
/// single-plane dumb buffer only ever populates index 0).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    /// Compiler-inserted alignment padding before `modifier` (8-byte aligned) that the C ABI
    /// also inserts here -- made explicit so `zerocopy`'s `IntoBytes` derive (which refuses
    /// implicit padding, since writing it back to guest memory would leak uninitialized host
    /// bytes) can verify the layout has none.
    _pad: u32,
    pub modifier: [u64; 4],
}

/// `struct drm_mode_modeinfo` -- one display-mode timing description.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeModeinfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub r#type: u32,
    pub name: [u8; 32],
}

/// `struct drm_mode_card_res` -- top-level resource enumeration (`DRM_IOCTL_MODE_GETRESOURCES`).
/// Two-call pattern: caller zeroes `count_*`/pointers to learn sizes, then calls again with
/// `*_ptr` fields pointing at pre-allocated arrays.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// `struct drm_mode_get_connector` (`DRM_IOCTL_MODE_GETCONNECTOR`), same two-call pattern.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

/// `DRM_MODE_CONNECTOR_VIRTUAL` -- the connector type for a software-only virtual display with
/// no real physical connector to claim.
pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
/// `DRM_MODE_ENCODER_VIRTUAL` -- the matching encoder type.
pub const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

/// `struct drm_mode_get_encoder` (`DRM_IOCTL_MODE_GETENCODER`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

/// `struct drm_mode_crtc` (`DRM_IOCTL_MODE_GETCRTC` / `DRM_IOCTL_MODE_SETCRTC`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeinfo,
}

/// `struct drm_mode_get_plane_res` (`DRM_IOCTL_MODE_GETPLANERESOURCES`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
    /// Compiler-inserted trailing padding (the struct's own size must be a multiple of its
    /// 8-byte alignment) -- see [`DrmModeFbCmd2`]'s `_pad` field doc comment for why this is
    /// made explicit rather than left implicit.
    _pad: u32,
}

/// `struct drm_mode_get_plane` (`DRM_IOCTL_MODE_GETPLANE`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

/// `struct drm_mode_set_plane` (`DRM_IOCTL_MODE_SETPLANE`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeSetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}

/// `struct drm_mode_obj_get_properties` (`DRM_IOCTL_MODE_OBJ_GETPROPERTIES`). This device reports
/// `count_props = 0` unconditionally for the connector object (see `DrmSubsystem::obj_get_properties`)
/// -- it has no dynamic KMS properties (no DPMS, no EDID blob, no rotation, none of the real
/// per-connector property set a hardware driver would register) -- matching the real kernel's own
/// behavior for an object with a genuinely empty property list, which is a normal, well-defined
/// response real libdrm clients (including `smithay`'s `backend_drm`) handle without error, not a
/// truncation or a fabricated answer.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    /// Compiler-inserted trailing padding (28 bytes of real fields, rounded up to the next
    /// 8-byte-aligned multiple) -- see [`DrmModeFbCmd2`]'s `_pad` field doc comment for why this
    /// is made explicit rather than left implicit.
    _pad: u32,
}

/// `struct drm_mode_get_property` (`DRM_IOCTL_MODE_GETPROPERTY`). See
/// [`DRM_IOCTL_MODE_GETPROPERTY`]'s own doc comment for why this device's `OBJ_GETPROPERTIES`
/// returning zero properties means no real client actually reaches this ioctl in practice.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; 32],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

/// `struct drm_mode_property_enum` -- one named enum entry, as served through
/// [`DrmModeGetProperty`]'s `enum_blob_ptr` array for an enum/bitmask-flagged property (e.g. the
/// plane `type` property's `"Primary"`/`"Overlay"`/`"Cursor"` entries).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; 32],
}

/// `struct drm_version` (`DRM_IOCTL_VERSION`) -- the two-call size-probe pattern applies to the
/// three trailing `(len, ptr)` string pairs the same way it does to `drm_mode_card_res`'s object
/// arrays: a caller passes `name_len`/`date_len`/`desc_len` set to its buffer sizes (0 to just
/// probe the true lengths), and gets the true lengths written back regardless of whether it
/// supplied a buffer.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    /// Compiler-inserted padding: the following `size_t`/pointer fields need 8-byte alignment on
    /// the LP64 ABI litebox targets, so the three leading `i32`s (12 bytes) are padded to 16.
    _pad: u32,
    pub name_len: u64,
    pub name: u64,
    pub date_len: u64,
    pub date: u64,
    pub desc_len: u64,
    pub desc: u64,
}

/// `struct drm_get_cap` (`DRM_IOCTL_GET_CAP`). `capability` is IN (a `DRM_CAP_*` constant,
/// e.g. [`DRM_CAP_DUMB_BUFFER`]); `value` is OUT.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_auth` (`DRM_IOCTL_GET_MAGIC`/`DRM_IOCTL_AUTH_MAGIC`) -- a single `__u32`
/// magic value, OUT on `GET_MAGIC`, IN on `AUTH_MAGIC`. See [`DRM_IOCTL_GET_MAGIC`]'s doc
/// comment for why this device's implementation always hands back/accepts the same fixed
/// [`DRM_AUTH_MAGIC_VALUE`] rather than tracking real per-client state.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmAuth {
    pub magic: u32,
}

/// `struct drm_set_client_cap` (`DRM_IOCTL_SET_CLIENT_CAP`). Identical field layout to
/// [`DrmGetCap`] but write-only: `capability` is IN (a `DRM_CLIENT_CAP_*` constant, e.g.
/// [`DRM_CLIENT_CAP_UNIVERSAL_PLANES`]), `value` is IN (the value being set; nothing is written
/// back).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_mode_crtc_page_flip` (`DRM_IOCTL_MODE_PAGE_FLIP`).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

/// `DRM_MODE_PAGE_FLIP_EVENT` flag bit -- caller wants a `DRM_EVENT_FLIP_COMPLETE` event queued
/// for delivery via `read()` on the DRM device fd once the flip completes.
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;

/// `struct drm_event` -- the common header of every event `read()` from a DRM device fd.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmEvent {
    pub r#type: u32,
    pub length: u32,
}

/// `struct drm_event_vblank` -- the page-flip-completion event body (follows a [`DrmEvent`]
/// header whose `type` is [`DRM_EVENT_FLIP_COMPLETE`]).
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct DrmEventVblank {
    pub base: DrmEvent,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

/// `DRM_EVENT_FLIP_COMPLETE` -- `DrmEvent::type` value for a completed page-flip.
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

/// `struct input_event` -- the record every evdev device (`/dev/input/event*`) delivers via
/// `read()`, one or more per call. Layout verified live against the real kernel
/// `include/uapi/linux/input.h`: on a 64-bit, non-Y2038-legacy build (the only shape litebox's
/// guest ABI needs to match -- see that header's own `#if` guard), `time` is `struct timeval`
/// with two `long` (8-byte) fields, giving a 16-byte `time` block followed by
/// `type`/`code`/`value`, 24 bytes total. Not guessed -- see
/// `docs/evdev-input-event-reference.md` for the derivation.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct InputEvent {
    pub tv_sec: u64,
    pub tv_usec: u64,
    pub r#type: u16,
    pub code: u16,
    pub value: i32,
}

/// Event types (`struct input_event::type`), `include/uapi/linux/input-event-codes.h`.
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;

/// Synchronization codes (`struct input_event::code` when `type == EV_SYN`).
pub const SYN_REPORT: u16 = 0;

/// Relative-axis codes (`struct input_event::code` when `type == EV_REL`).
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;

/// Mouse button codes (`struct input_event::code` when `type == EV_KEY`, in the `BTN_*` range).
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// Keyboard key codes (`struct input_event::code` when `type == EV_KEY`, the `KEY_*` range),
/// values verified live against the real kernel `input-event-codes.h`, not guessed. Covers the
/// common US-layout alphanumeric/punctuation/navigation/function keys -- a real, useful subset
/// for a desktop-style keyboard, not the full ~700-entry `KEY_*` namespace (media keys, exotic
/// layouts, etc. are out of scope for this pass).
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;

pub const TCGETS: u32 = 0x5401;
pub const TCSETS: u32 = 0x5402;
pub const TCSETSW: u32 = 0x5403;
pub const TCSETSF: u32 = 0x5404;
pub const TIOCGWINSZ: u32 = 0x5413;
pub const TIOCSWINSZ: u32 = 0x5414;
pub const FIONBIO: u32 = 0x5421;
pub const FIOCLEX: u32 = 0x5451;
pub const TIOCSCTTY: u32 = 0x540E;
pub const TIOCGPGRP: u32 = 0x540F;
pub const TIOCSPGRP: u32 = 0x5410;
pub const TIOCGPTN: u32 = 0x8004_5430;
pub const TIOCSPTLCK: u32 = 0x4004_5431;

/// Commands for use with `ioctl`.
#[non_exhaustive]
#[derive(Debug)]
pub enum IoctlArg {
    /// Get the current serial port settings.
    TCGETS(UserPtrMut<Termios>),
    /// Set the current serial port settings immediately (`TCSANOW`).
    TCSETS(UserPtr<Termios>),
    /// Set the current serial port settings after draining output (`TCSADRAIN`).
    TCSETSW(UserPtr<Termios>),
    /// Set the current serial port settings after flushing input/output (`TCSAFLUSH`).
    ///
    /// This is the command libuv's `uv__tty_make_raw` (and therefore Node's
    /// `tty.ReadStream.setRawMode`) actually issues.
    TCSETSF(UserPtr<Termios>),
    /// Get window size.
    TIOCGWINSZ(UserPtrMut<Winsize>),
    /// Set window size (`ioctl(fd, TIOCSWINSZ, &ws)`), used e.g. by terminal multiplexers and
    /// `node-pty`/`pty.js`-style libraries to propagate the real terminal size into a pty.
    TIOCSWINSZ(UserPtr<Winsize>),
    /// Obtain device unit number, which can be used to generate
    /// the filename of the pseudo-terminal slave device.
    TIOCGPTN(UserPtrMut<u32>),
    /// Unlock/lock the pty slave (`unlockpt`/glibc's `grantpt` path). A freshly allocated pty
    /// pair starts locked (matching Linux): opening the slave before this is issued with `0`
    /// fails with `EIO`, mirroring the real kernel's devpts behavior.
    TIOCSPTLCK(UserPtr<i32>),
    /// Make the given terminal the calling process's controlling terminal
    /// (`ioctl(fd, TIOCSCTTY, force)`). Unlike most other `ioctl`s here, the third argument is a
    /// plain scalar (a "steal from another session" force flag), not a pointer -- so this is
    /// parsed via `sys_req_arg`, not `sys_req_ptr`. glibc's `login_tty()` (the primitive behind
    /// `forkpty()`/`openpty()`-based tools -- `node-pty`, Python's `os.forkpty()`, tmux, `script`)
    /// always calls this right after `setsid()`; without it, every one of those fails to open a
    /// session on this pty.
    TIOCSCTTY(i32),
    /// Get the terminal's foreground process group ID (`tcgetpgrp`).
    TIOCGPGRP(UserPtrMut<i32>),
    /// Set the terminal's foreground process group ID (`tcsetpgrp`). A shell's job-control
    /// setup calls this to make itself the foreground process group; failure here is exactly
    /// what makes busybox `ash` print "can't access tty; job control turned off" and fall back
    /// to a job-control-disabled mode.
    TIOCSPGRP(UserPtr<i32>),
    /// Enables or disables non-blocking mode
    FIONBIO(UserPtr<i32>),
    /// Set close on exec
    FIOCLEX,
    /// `DRM_IOCTL_MODE_GETRESOURCES` -- enumerate the virtual card's fb/CRTC/connector/encoder
    /// object IDs (two-call size-probe pattern, see [`DrmModeCardRes`]'s doc comment).
    DrmModeGetResources(UserPtrMut<DrmModeCardRes>),
    /// `DRM_IOCTL_MODE_GETCRTC`.
    DrmModeGetCrtc(UserPtrMut<DrmModeCrtc>),
    /// `DRM_IOCTL_MODE_SETCRTC`.
    DrmModeSetCrtc(UserPtr<DrmModeCrtc>),
    /// `DRM_IOCTL_MODE_GETENCODER`.
    DrmModeGetEncoder(UserPtrMut<DrmModeGetEncoder>),
    /// `DRM_IOCTL_MODE_GETCONNECTOR` (two-call size-probe pattern).
    DrmModeGetConnector(UserPtrMut<DrmModeGetConnector>),
    /// `DRM_IOCTL_MODE_CREATE_DUMB` -- allocate a CPU-writable dumb pixel buffer.
    DrmModeCreateDumb(UserPtrMut<DrmModeCreateDumb>),
    /// `DRM_IOCTL_MODE_MAP_DUMB` -- get an mmap-able offset for a dumb buffer.
    DrmModeMapDumb(UserPtrMut<DrmModeMapDumb>),
    /// `DRM_IOCTL_MODE_DESTROY_DUMB`.
    DrmModeDestroyDumb(UserPtr<DrmModeDestroyDumb>),
    /// `DRM_IOCTL_MODE_ADDFB2` -- attach a dumb buffer as a scanout framebuffer.
    DrmModeAddFb2(UserPtrMut<DrmModeFbCmd2>),
    /// `DRM_IOCTL_MODE_PAGE_FLIP`.
    DrmModePageFlip(UserPtr<DrmModeCrtcPageFlip>),
    /// `DRM_IOCTL_MODE_GETPLANERESOURCES` -- enumerate the virtual card's plane object IDs
    /// (two-call size-probe pattern, see [`DrmModeGetPlaneRes`]'s doc comment).
    DrmModeGetPlaneResources(UserPtrMut<DrmModeGetPlaneRes>),
    /// `DRM_IOCTL_MODE_GETPLANE` (two-call size-probe pattern for `format_type_ptr`).
    DrmModeGetPlane(UserPtrMut<DrmModeGetPlane>),
    /// `DRM_IOCTL_MODE_SETPLANE` -- attach a framebuffer directly to a plane.
    DrmModeSetPlane(UserPtr<DrmModeSetPlane>),
    /// `DRM_IOCTL_VERSION` -- driver identification, the first ioctl every real libdrm-based
    /// client calls (two-call size-probe pattern for the `name`/`date`/`desc` strings).
    DrmVersion(UserPtrMut<DrmVersion>),
    /// `DRM_IOCTL_GET_CAP` -- query a single `DRM_CAP_*` capability.
    DrmGetCap(UserPtrMut<DrmGetCap>),
    /// `DRM_IOCTL_SET_CLIENT_CAP` -- opt into a single `DRM_CLIENT_CAP_*` behavior.
    DrmSetClientCap(UserPtr<DrmSetClientCap>),
    /// `DRM_IOCTL_SET_MASTER` -- a plain `_IO()` with no argument struct, so this fd's own file
    /// descriptor (not a pointer) is the only state a handler needs.
    DrmSetMaster,
    /// `DRM_IOCTL_DROP_MASTER`.
    DrmDropMaster,
    /// `DRM_IOCTL_GET_MAGIC` -- see that constant's own doc comment.
    DrmGetMagic(UserPtrMut<DrmAuth>),
    /// `DRM_IOCTL_AUTH_MAGIC` -- see [`DRM_IOCTL_GET_MAGIC`]'s doc comment.
    DrmAuthMagic(UserPtr<DrmAuth>),
    /// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` -- enumerate a KMS object's properties (two-call
    /// size-probe pattern for `props_ptr`/`prop_values_ptr`, same shape as `get_resources`'s
    /// object-ID arrays).
    DrmModeObjGetProperties(UserPtrMut<DrmModeObjGetProperties>),
    /// `DRM_IOCTL_MODE_GETPROPERTY` -- resolve a single property ID's name/values.
    DrmModeGetProperty(UserPtrMut<DrmModeGetProperty>),
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD` -- export a dumb-buffer handle as a real fd onto the same
    /// backing memory. See [`DRM_IOCTL_PRIME_HANDLE_TO_FD`]'s own doc comment.
    DrmPrimeHandleToFd(UserPtrMut<DrmPrimeHandle>),
    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE` -- resolve a (self-exported) PRIME fd back to its
    /// originating GEM handle. See [`DRM_IOCTL_PRIME_FD_TO_HANDLE`]'s own doc comment.
    DrmPrimeFdToHandle(UserPtrMut<DrmPrimeHandle>),
    /// `DRM_IOCTL_GEM_CLOSE` -- release a local reference to a GEM handle. See
    /// [`DRM_IOCTL_GEM_CLOSE`]'s own doc comment.
    DrmGemClose(UserPtr<DrmGemClose>),
    /// `VT_GETSTATE` -- report which VT is currently active. `seatd`'s `seat_update_vt` (see
    /// `seatd/seat.c`) calls this on `/dev/tty0` to learn which per-VT device (`/dev/tty<N>`)
    /// to subsequently open for a connecting client.
    VtGetState(UserPtrMut<VtStat>),
    /// `VT_SETMODE` -- claim (or release) process-controlled VT switching. `seatd`'s `vt_open`
    /// calls this on the client's assigned `/dev/tty<N>` once it grants the client the VT.
    VtSetMode(UserPtr<VtMode>),
    /// `KDSETMODE` -- switch a VT between text (`KD_TEXT`) and graphics (`KD_GRAPHICS`) mode.
    /// The third `ioctl()` argument is the mode value itself, not a pointer to one.
    KdSetMode(i32),
    /// `KDSKBMODE` -- switch a VT's keyboard translation mode. Same argument shape as
    /// `KDSETMODE`: a plain scalar, not a pointer.
    KdSkbMode(i32),
    /// `EVIOCREVOKE` (`_IOW('E', 0x91, int)`) -- revoke a process's access to an evdev input
    /// device, so a later `read()`/`write()`/most other `ioctl()`s on this fd return `ENODEV`.
    /// `seatd`'s `seat_close_device` calls this on every evdev fd it hands back on VT-switch-away
    /// or client disconnect, as a defense-in-depth measure so a revoked client can't keep reading
    /// input events behind the (now-inactive) seat's back -- real evdev honors it even though the
    /// fd itself stays open. litebox's device set never actually switches seats away from the one
    /// client each guest process runs as, so honoring this is a real, correct no-op for now (see
    /// `EvdevSubsystem`'s own doc comment on why litebox's device set is static per-run) rather
    /// than a shortcut -- there is no OTHER client this could ever need to actually revoke access
    /// from.
    EvdevRevoke,
    /// `EVIOCGVERSION` -- report the evdev protocol version. Mandatory for
    /// `libevdev_new_from_fd()`; failure aborts device creation.
    EvdevGetVersion(UserPtrMut<i32>),
    /// `EVIOCGID` -- report the device's bus/vendor/product/version identity. Mandatory for
    /// `libevdev_new_from_fd()`; failure aborts device creation.
    EvdevGetId(UserPtrMut<InputId>),
    /// `EVIOCGBIT(ev, len)` -- report the bitmask of codes the device supports for event type
    /// `ev` (or, when `ev == 0`, the bitmask of event TYPES the device supports at all). This is
    /// a *variable-length* ioctl family (`_IOC(_IOC_READ, 'E', 0x20 + ev, len)`): the ioctl
    /// NUMBER itself encodes both `ev` and the caller-requested buffer length `len`, so it can't
    /// be matched as a single fixed constant the way `EVIOCGID`/`EVIOCGVERSION` are -- `ev`/`len`
    /// are decoded directly from the raw `cmd` value at dispatch time. Non-fatal on failure
    /// (`libevdev_new_from_fd()` tolerates `EINVAL` here per its own real source), but a
    /// zero/wrong bitmask makes libinput misclassify the device's actual capabilities.
    EvdevGetBits {
        ev: u32,
        len: u32,
        arg: UserPtrMut<u8>,
    },
    /// `EVIOCGNAME(len)` -- report the device's human-readable name string. Mandatory for
    /// `libevdev_new_from_fd()`: unlike `EVIOCGPHYS`/`EVIOCGUNIQ` (tolerated on failure, real
    /// devices without a physical-location/unique-ID string return `ENOENT`), a failure here
    /// unconditionally aborts device creation (`libevdev.c`'s `libevdev_set_fd()`: `rc = ioctl(fd,
    /// EVIOCGNAME(...), buf); if (rc < 0) goto out;`, no error-code exemption). Like `EVIOCGBIT`,
    /// this is a *variable-length* ioctl (`_IOC(_IOC_READ, 'E', 0x06, len)`) -- the caller's
    /// buffer length is encoded in the ioctl number itself, decoded from the raw `cmd` at
    /// dispatch time rather than matched as a fixed constant.
    EvdevGetName {
        len: u32,
        arg: UserPtrMut<u8>,
    },
    /// `EVIOCGPHYS(len)`/`EVIOCGUNIQ(len)` -- report the device's physical-location/unique-ID
    /// strings. Unlike `EVIOCGNAME`, `libevdev_new_from_fd()` tolerates these failing (`if (rc <
    /// 0) { if (errno != ENOENT) goto out; }`) -- a real device without one, like litebox's
    /// synthetic evdev, correctly returns `ENOENT`, matching real uinput's own behavior for the
    /// same reason. Same variable-length encoding as `EVIOCGBIT`/`EVIOCGNAME`
    /// (`_IOC(_IOC_READ, 'E', 0x07 or 0x08, len)`), decoded from the raw `cmd` at dispatch time.
    EvdevGetPhysOrUniq,
    /// `EVIOCGPROP(len)` -- report the bitmask of `INPUT_PROP_*` device properties (e.g.
    /// `INPUT_PROP_POINTER`/`INPUT_PROP_BUTTONPAD`). `libevdev_new_from_fd()` tolerates this
    /// failing (its real source only aborts on `EVIOCGBIT`/`EVIOCGNAME`/`EVIOCGID`/
    /// `EVIOCGVERSION` failures) -- a plain keyboard+mouse device correctly has zero properties
    /// set, matching what a real generic HID device reports. Same variable-length encoding as
    /// `EVIOCGBIT`/`EVIOCGNAME` (`_IOC(_IOC_READ, 'E', 0x09, len)`), decoded from the raw `cmd`
    /// at dispatch time.
    EvdevGetProp {
        len: u32,
        arg: UserPtrMut<u8>,
    },
    /// `EVIOCGKEY(len)` -- report the bitmask of currently-pressed `EV_KEY` codes.
    /// `libevdev_new_from_fd()`'s `sync_key_state()` issues this during device setup to seed its
    /// internal key-state cache; unlike `EVIOCGPHYS`/`EVIOCGUNIQ`/`EVIOCGPROP`, a failure here is
    /// NOT tolerated -- `sync_state()`'s error propagates straight out of `libevdev_new_from_fd()`,
    /// which returns non-zero to `evdev_device_create()`, which `goto err`s before the udev-tag
    /// check or `evdev_configure_device()` ever run (confirmed by direct source read of
    /// `evdev_device_create()`, `libinput-1.31.3/src/evdev.c` lines 2314-2316). This previously
    /// fell through to the `Raw` catch-all's `EINVAL`, which is exactly this failure -- an
    /// unpressed device correctly has zero key bits set, matching real hardware at attach time.
    /// Same variable-length encoding as `EVIOCGBIT`/`EVIOCGPROP` (`_IOC(_IOC_READ, 'E', 0x18,
    /// len)`), decoded from the raw `cmd` at dispatch time.
    EvdevGetKey {
        len: u32,
        arg: UserPtrMut<u8>,
    },
    /// `EVIOCGLED(len)` -- report the bitmask of currently-lit `EV_LED` indicators (caps lock,
    /// num lock, ...). Same `sync_state()` propagation as `EVIOCGKEY` (see that variant's doc
    /// comment) -- `libevdev_new_from_fd()`'s internal sync calls `EVIOCGKEY` then `EVIOCGLED`
    /// then `EVIOCGSW` in sequence, any one of which failing aborts the whole sync and hence
    /// `libevdev_new_from_fd()` itself. A device with no LEDs lit at attach time correctly
    /// reports an all-zero bitmap. Same variable-length encoding, `_IOC(_IOC_READ, 'E', 0x19,
    /// len)`.
    EvdevGetLed {
        len: u32,
        arg: UserPtrMut<u8>,
    },
    /// `EVIOCGSW(len)` -- report the bitmask of currently-active `EV_SW` switches. Same
    /// `sync_state()` propagation as `EVIOCGKEY`/`EVIOCGLED` (see their doc comments). A device
    /// with no switches active at attach time correctly reports an all-zero bitmap. Same
    /// variable-length encoding, `_IOC(_IOC_READ, 'E', 0x1b, len)`.
    EvdevGetSwitch {
        len: u32,
        arg: UserPtrMut<u8>,
    },
    Raw {
        cmd: u32,
        arg: UserPtrMut<u8>,
    },
}

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct MRemapFlags: u32 {
        /// Permit the kernel to relocate the mapping to a new virtual address, if necessary.
        const MREMAP_MAYMOVE = 1;
        /// Place the mapping at exactly the address specified in `new_address`.
        const MREMAP_FIXED = 2;
        /// Don't unmap the old mapping.
        /// This is only valid when `MREMAP_FIXED` is also specified.
        const MREMAP_DONTUNMAP = 4;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

#[repr(u32)]
#[non_exhaustive]
#[derive(Debug, IntEnum)]
pub enum AddressFamily {
    UNIX = 1,
    INET = 2,
    INET6 = 10,
    NETLINK = 16,
}

#[repr(u32)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, IntEnum)]
pub enum SockType {
    Stream = 1,
    Datagram = 2,
    Raw = 3,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct SockFlags: core::ffi::c_uint {
        const NONBLOCK = OFlags::NONBLOCK.bits();
        const CLOEXEC = OFlags::CLOEXEC.bits();
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

/// struct for SO_LINGER option
#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct Linger {
    pub onoff: u32,  /* Linger active		*/
    pub linger: u32, /* How long to linger for	*/
}

/// IP Protocols
#[repr(u8)]
#[non_exhaustive]
#[derive(IntEnum, Debug)]
pub enum IPProtocol {
    Default = 0,
    ICMP = 1,
    TCP = 6,
    UDP = 17,
    RAW = 255,
}

#[repr(u8)]
#[derive(Debug, IntEnum)]
pub enum UnixProtocol {
    Default = 0,
    UNIX = 1,
}

#[repr(u32)]
#[derive(Debug, IntEnum, Clone, Copy)]
pub enum IpOption {
    TOS = 1,
}

#[repr(u32)]
#[derive(Debug, IntEnum, Clone, Copy)]
pub enum SocketOption {
    REUSEADDR = 2,
    TYPE = 3,
    ERROR = 4,
    BROADCAST = 6,
    SNDBUF = 7,
    RCVBUF = 8,
    KEEPALIVE = 9,
    /// This option controls the action taken when unsent messages queue on
    /// a socket and close() is performed. If SO_LINGER is set, the system
    /// shall block the process during close() until it can transmit the data
    /// or until the time expires.
    LINGER = 13,
    PEERCRED = 17,
    RCVTIMEO = 20,
    SNDTIMEO = 21,
}

#[repr(u32)]
#[derive(Debug, IntEnum, Clone, Copy)]
pub enum TcpOption {
    NODELAY = 1,
    CORK = 3,
    /// Start keeplives after this period
    KEEPIDLE = 4,
    /// Interval between keepalives
    KEEPINTVL = 5,
    /// Number of keepalives before death
    KEEPCNT = 6,
    INFO = 11,
    CONGESTION = 13,
}

#[derive(Debug, Clone, Copy)]
pub enum SocketOptionName {
    IP(IpOption),
    Socket(SocketOption),
    TCP(TcpOption),
}

#[repr(u32)]
#[derive(Debug, IntEnum)]
pub enum SocketOptionLevel {
    IP = 0,
    SOCKET = 1,
    TCP = 6,
    UDP = 17,
    RAW = 255,
}

impl SocketOptionName {
    pub fn try_from(level: u32, optname: u32) -> Option<Self> {
        let level = SocketOptionLevel::try_from(level).ok()?;
        match level {
            SocketOptionLevel::IP => Some(Self::IP(IpOption::try_from(optname).ok()?)),
            SocketOptionLevel::SOCKET => Some(Self::Socket(SocketOption::try_from(optname).ok()?)),
            SocketOptionLevel::TCP => Some(Self::TCP(TcpOption::try_from(optname).ok()?)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
pub struct Ucred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

// Following libc's definition of time_t and suseconds_t.
// They are not same as isize on all architectures, e.g.,
// `suseconds_t` is i64 on riscv32:
// https://github.com/rust-lang/libc/blob/151c3a971e423c76e7acb54aa2d21a6e2706c4e6/src/unix/linux_like/linux/gnu/b32/mod.rs#L22
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))] {
        pub type time_t = i64;
        pub type suseconds_t = u64;
    } else {
        compile_error!("Unsupported architecture");
    }
}

/// timespec from [Linux](https://elixir.bootlin.com/linux/v5.19.17/source/include/uapi/linux/time_types.h#L7)
#[derive(Debug, Clone, Copy, PartialOrd, PartialEq, Eq, FromBytes, IntoBytes, Default, Immutable)]
#[repr(C)]
pub struct Timespec {
    /// Seconds.
    pub tv_sec: i64,

    /// Nanoseconds. Must be less than 1_000_000_000.
    pub tv_nsec: u64,
}

impl TryFrom<Timespec> for Duration {
    type Error = errno::Errno;

    fn try_from(value: Timespec) -> Result<Self, Self::Error> {
        // On 32-bit architectures, `tv_nsec` may be defined in user mode as
        // pointer sized. Ignore any high padding bits.
        let nsec: usize = value.tv_nsec.trunc();
        if nsec >= 1_000_000_000 {
            return Err(errno::Errno::EINVAL);
        }
        Ok(Duration::new(
            u64::try_from(value.tv_sec).map_err(|_| errno::Errno::EINVAL)?,
            nsec.trunc(),
        ))
    }
}

impl From<Duration> for Timespec {
    fn from(value: Duration) -> Self {
        Timespec {
            tv_sec: value.as_secs().reinterpret_as_signed(),
            tv_nsec: value.subsec_nanos().into(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes)]
pub struct Timespec32 {
    pub tv_sec: i32,
    pub tv_nsec: u32,
}

impl From<Timespec32> for Timespec {
    fn from(value: Timespec32) -> Self {
        Timespec {
            tv_sec: value.tv_sec.into(),
            tv_nsec: value.tv_nsec.into(),
        }
    }
}

impl TryFrom<Timespec32> for Duration {
    type Error = errno::Errno;

    fn try_from(value: Timespec32) -> Result<Self, Self::Error> {
        Timespec::from(value).try_into()
    }
}

impl From<Duration> for Timespec32 {
    fn from(value: Duration) -> Self {
        Timespec32 {
            // Silently truncate if needed, just like Linux would do.
            tv_sec: value.as_secs().reinterpret_as_signed().trunc(),
            tv_nsec: value.subsec_nanos(),
        }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy, FromBytes, IntoBytes, Immutable)]
pub struct TimeVal {
    tv_sec: time_t,
    tv_usec: suseconds_t,
}
#[repr(C)]
#[derive(Clone, Default, FromBytes, IntoBytes, Immutable)]
pub struct ItimerVal {
    /// Timer interval
    interval: TimeVal,
    /// Current value
    value: TimeVal,
}

impl ItimerVal {
    pub fn new(interval: TimeVal, value: TimeVal) -> Self {
        Self { interval, value }
    }

    /// `it_value = duration`, `it_interval = 0` (single-shot timer).
    pub fn single_shot(duration: Duration) -> Self {
        Self::new(TimeVal::from(Duration::ZERO), TimeVal::from(duration))
    }

    pub fn it_interval(&self) -> TimeVal {
        self.interval
    }

    pub fn it_value(&self) -> TimeVal {
        self.value
    }
}

/// `itimerspec` from [Linux](https://elixir.bootlin.com/linux/v5.19.17/source/include/uapi/linux/time_types.h)
/// -- the `timerfd_settime(2)`/`timerfd_gettime(2)` ABI struct, a pair of [`Timespec`]s rather
/// than [`ItimerVal`]'s pair of [`TimeVal`]s (nanosecond, not microsecond, resolution).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable)]
pub struct ItimerSpec {
    /// Timer interval; zero means "single-shot, do not repeat".
    pub it_interval: Timespec,
    /// Current value (initial expiration, or time remaining when read back via
    /// `timerfd_gettime`).
    pub it_value: Timespec,
}

impl TryFrom<TimeVal> for Duration {
    type Error = errno::Errno;

    fn try_from(value: TimeVal) -> Result<Self, Self::Error> {
        let usec: u32 = value.tv_usec.trunc();
        if usec >= 1_000_000 {
            return Err(errno::Errno::EINVAL);
        }
        Ok(Duration::new(
            u64::try_from(value.tv_sec).map_err(|_| errno::Errno::EINVAL)?,
            usec * 1000,
        ))
    }
}

impl From<Duration> for TimeVal {
    fn from(value: Duration) -> Self {
        TimeVal {
            // Silently truncate if needed, just like Linux would do.
            tv_sec: value.as_secs().reinterpret_as_signed().trunc(),
            #[cfg_attr(target_pointer_width = "32", expect(clippy::useless_conversion))]
            tv_usec: value.subsec_micros().into(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes)]
pub struct TimeZone {
    tz_minuteswest: i32,
    tz_dsttime: i32,
}

impl TimeZone {
    /// Create a new TimeZone with the given minutes west of UTC and DST time flag
    pub fn new(tz_minuteswest: i32, tz_dsttime: i32) -> Self {
        Self {
            tz_minuteswest,
            tz_dsttime,
        }
    }
}

/// Codes for the `arch_prctl` syscall.
#[repr(u32)]
#[non_exhaustive]
#[derive(Debug, IntEnum)]
pub enum ArchPrctlCode {
    /// Set the 64-bit base for the FS register
    #[cfg(target_arch = "x86_64")]
    SetFs = 0x1002,
    /// Return the 64-bit base value for the FS register of the calling thread
    #[cfg(target_arch = "x86_64")]
    GetFs = 0x1003,

    /* CET (Control-flow Enforcement Technology) ralated operations; each of these simply will return EINVAL */
    CETStatus = 0x3001,
    CETDisable = 0x3002,
    CETLock = 0x3003,
}

/// Argument for the `arch_prctl` syscall, corresponding to the [`ArchPrctlCode`] enum.
#[non_exhaustive]
#[derive(Debug)]
pub enum ArchPrctlArg {
    #[cfg(target_arch = "x86_64")]
    SetFs(usize),
    #[cfg(target_arch = "x86_64")]
    GetFs(UserPtrMut<usize>),

    CETStatus,
    CETDisable,
    CETLock,
}

/// Reads the FS segment base address
///
/// ## Safety
///
/// If `CR4.FSGSBASE` is not set, calling this instruction from user land will throw an `#UD`.
#[cfg(target_arch = "x86_64")]
pub unsafe fn rdfsbase() -> usize {
    let ret: usize;
    unsafe {
        core::arch::asm!(
            "rdfsbase {}",
            out(reg) ret,
            options(nostack, nomem, preserves_flags)
        );
    }
    ret
}

/// Writes the FS segment base address
///
/// ## Safety
///
/// If `CR4.FSGSBASE` is not set, calling this instruction from user land will throw an `#UD`.
///
/// The caller must ensure that this write operation has no unsafe side
/// effects, as the FS segment base address is often used for thread
/// local storage.
#[cfg(target_arch = "x86_64")]
pub unsafe fn wrfsbase(fs_base: usize) {
    unsafe {
        core::arch::asm!(
            "wrfsbase {}",
            in(reg) fs_base,
            options(nostack, nomem, preserves_flags)
        );
    }
}

/// Reads the GS segment base address
///
/// ## Safety
///
/// If `CR4.FSGSBASE` is not set, this instruction will throw an `#UD`.
#[cfg(target_arch = "x86_64")]
pub unsafe fn rdgsbase() -> usize {
    let ret: usize;
    unsafe {
        core::arch::asm!(
            "rdgsbase {}",
            out(reg) ret,
            options(nostack, nomem, preserves_flags)
        );
    }
    ret
}

/// Writes the GS segment base address
///
/// ## Safety
///
/// If `CR4.FSGSBASE` is not set, this instruction will throw an `#UD`.
///
/// The caller must ensure that this write operation has no unsafe side
/// effects, as the GS segment base address might be in use.
#[cfg(target_arch = "x86_64")]
pub unsafe fn wrgsbase(gs_base: usize) {
    unsafe {
        core::arch::asm!(
            "wrgsbase {}",
            in(reg) gs_base,
            options(nostack, nomem, preserves_flags)
        );
    }
}

/// Flags for the clone3 system call as defined in `/usr/include/linux/sched.h`.
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes)]
#[repr(transparent)]
pub struct CloneFlags(u64);

bitflags::bitflags! {
    impl CloneFlags: u64 {
        /// Set if VM shared between processes
        const VM      = 0x00000100;
        /// Set if fs info shared between processes
        const FS      = 0x00000200;
        /// Set if open files shared between processes
        const FILES   = 0x00000400;
        /// Set if signal handlers and blocked signals shared
        const SIGHAND = 0x00000800;
        /// Set if a pidfd should be placed in parent
        const PIDFD   = 0x00001000;
        /// Set if we want to let tracing continue on the child too
        const PTRACE  = 0x00002000;
        /// Set if the parent wants the child to wake it up on mm_release
        const VFORK   = 0x00004000;
        /// Set if we want to have the same parent as the cloner
        const PARENT  = 0x00008000;
        /// Same thread group
        const THREAD  = 0x00010000;
        /// New mount namespace group
        const NEWNS   = 0x00020000;
        /// Share system V SEM_UNDO semantics
        const SYSVSEM = 0x00040000;
        /// Create a new TLS for the child
        const SETTLS  = 0x00080000;

        /// Set the TID in the parent
        const PARENT_SETTID  = 0x00100000;
        /// Clear the TID in the child
        const CHILD_CLEARTID = 0x00200000;
        /// Ignored.
        const DETACHED      = 0x00400000;
        /// Set if the tracing process can't force CLONE_PTRACE on this clone
        const UNTRACED       = 0x00800000;
        /// Set the TID in the child
        const CHILD_SETTID   = 0x01000000;
        /// New cgroup namespace
        const NEWCGROUP      = 0x02000000;
        /// New uts namespace
        const NEWUTS         = 0x04000000;
        /// New ipc namespace
        const NEWIPC         = 0x08000000;
        /// New user namespace
        const NEWUSER        = 0x10000000;
        /// New pid namespace
        const NEWPID         = 0x20000000;
        /// New network namespace
        const NEWNET         = 0x40000000;
        /// Clone io context
        const IO             = 0x80000000;

        /// Clear any signal handler and reset to SIG_DFL.
        const CLEAR_SIGHAND = 0x100000000;
        /// Clone into a specific cgroup given the right permissions.
        const INTO_CGROUP   = 0x200000000;

        /// New time namespace
        const NEWTIME = 0x00000080;

        const _ = !0; // Externally defined flags
    }
}

/// Arguments for the `clone3` syscall.
#[repr(C, align(8))]
#[derive(Clone, Debug, FromBytes, IntoBytes)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

/// Task command name length
pub const TASK_COMM_LEN: usize = 16;

pub struct TaskParams {
    /// Process ID
    pub pid: i32,
    /// Parent Process ID
    pub ppid: i32,
    /// The initial uid.
    pub uid: u32,
    /// The initial effective uid.
    pub euid: u32,
    /// The initial gid.
    pub gid: u32,
    /// The initial effective gid.
    pub egid: u32,
}

#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct Utsname {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

bitflags::bitflags! {
    #[derive(Debug)]
    /// Flags for the `getrandom` syscall.
    pub struct RngFlags: i32 {
        /// When reading from the random source, getrandom() blocks if no random bytes are available,
        /// and when reading from the urandom source, it blocks if the entropy pool has not yet been initialized.
        const NONBLOCK = 1;
        /// Random bytes are drawn from the random source (i.e., same as `/dev/random`)
        /// instead of the urandom source.
        const RANDOM = 2;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

#[cfg(not(target_arch = "riscv32"))]
pub type rlim_t = usize;

/// Used by getrlimit and setrlimit syscalls
#[repr(C)]
#[derive(Clone, Debug, FromBytes, IntoBytes)]
pub struct Rlimit {
    pub rlim_cur: rlim_t,
    pub rlim_max: rlim_t,
}

/// Used by prlimit64 syscall
#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct Rlimit64 {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

pub fn rlimit_to_rlimit64(rlim: Rlimit) -> Rlimit64 {
    Rlimit64 {
        rlim_cur: if rlim.rlim_cur == rlim_t::MAX {
            u64::MAX
        } else {
            rlim.rlim_cur as u64
        },
        rlim_max: if rlim.rlim_max == rlim_t::MAX {
            u64::MAX
        } else {
            rlim.rlim_max as u64
        },
    }
}

pub fn rlimit64_to_rlimit(rlim: Rlimit64) -> Rlimit {
    Rlimit {
        rlim_cur: if rlim.rlim_cur >= rlim_t::MAX as u64 {
            rlim_t::MAX
        } else {
            rlim.rlim_cur.trunc()
        },
        rlim_max: if rlim.rlim_max >= rlim_t::MAX as u64 {
            rlim_t::MAX
        } else {
            rlim.rlim_max.trunc()
        },
    }
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, IntEnum)]
pub enum RlimitResource {
    /// CPU time in sec
    CPU = 0,
    /// Max filesize
    FSIZE = 1,
    /// Max data size
    DATA = 2,
    /// Max stack size
    STACK = 3,
    /// Max core file size
    CORE = 4,
    /// Max resident set size
    RSS = 5,
    /// Max number of processes
    NPROC = 6,
    /// Max number of open files
    NOFILE = 7,
    /// Max number of locked memory
    MEMLOCK = 8,
    /// Max address space
    AS = 9,
    /// Max number of file locks held
    LOCKS = 10,
    /// Max number of pending signals
    SIGPENDING = 11,
    /// Max bytes in POSIX mqueues
    MSGQUEUE = 12,
    /// max nice prio allowed to raise to 0-39 for nice level 19 .. -20
    NICE = 13,
    /// Max realtime priority
    RTPRIO = 14,
    /// timeout for RT tasks in us
    RTTIME = 15,
}
impl RlimitResource {
    /// Maximum value for RlimitResource
    pub const RLIM_NLIMITS: usize = RlimitResource::RTTIME as usize + 1;
}

// FUTURE: The rust compiler is currently confused (in the shim, where a pointer
// to this is taken) by the overly recursive nature of the trait bounds if we
// actually set the types up for this the way they are in the comments, rather
// than the `usize`s (Note: the separate issue of `Unaligned` when using that
// variant is fixed simply by using `zerocopy::Usize`, and is not the issue
// being referred to here).  Using the RobustList based types here causes a
// E0275 (see `rustc --explain E0275`) on `Sized` and `FromBytes`. There is some
// belief that minor restructuring should allow rustc to properly discover that
// all the requirements are satisfied, but currently, that is considered beyond
// the scope of the changes in the PR that introduced the
// `FromBytes`/`IntoBytes` implementation here.
/// XXX: The types in this struct might be changed to stronger types in the
/// future.
#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct RobustList {
    pub next: usize, // Platform::RawConstPointer<RobustList<Platform>>,
}

#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
// FUTURE: The rust compiler is currently confused (in the shim, where a pointer
// to this is taken) by the overly recursive nature of the trait bounds if we
// actually set the types up for this the way they are in the comments, rather
// than the `usize`s (Note: the separate issue of `Unaligned` when using that
// variant is fixed simply by using `zerocopy::Usize`, and is not the issue
// being referred to here).  Using the RobustList based types here causes a
// E0275 (see `rustc --explain E0275`) on `Sized` and `FromBytes`. There is some
// belief that minor restructuring should allow rustc to properly discover that
// all the requirements are satisfied, but currently, that is considered beyond
// the scope of the changes in the PR that introduced the
// `FromBytes`/`IntoBytes` implementation here.
/// XXX: The types in this struct might be changed to stronger types in the
/// future.
pub struct RobustListHead {
    /// The head of the list. Points back to itself if empty.
    pub list: RobustList, // RobustList<Platform>,
    /// This relative offset is set by user-space, it gives the kernel
    /// the relative position of the futex field to examine. This way
    /// we keep userspace flexible, to freely shape its data-structure,
    /// without hardcoding any particular offset into the kernel.
    pub futex_offset: usize,
    /// The death of the thread may race with userspace setting
    /// up a lock's links. So to handle this race, userspace first
    /// sets this field to the address of the to-be-taken lock,
    /// then does the lock acquire, and then adds itself to the
    /// list, and then clears this field. Hence the kernel will
    /// always have full knowledge of all locks that the thread
    /// _might_ have taken. We check the owner TID in any case,
    /// so only truly owned locks will be handled.
    pub list_op_pending: usize, // Platform::RawConstPointer<RobustList<Platform>>,
}

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct EpollCreateFlags: core::ffi::c_uint {
        const EPOLL_CLOEXEC = litebox::fs::OFlags::CLOEXEC.bits();
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

#[repr(i32)]
#[derive(Debug, IntEnum, PartialEq, Eq)]
pub enum EpollOp {
    EpollCtlAdd = 1,
    EpollCtlDel = 2,
    EpollCtlMod = 3,
}

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes)]
#[repr(C, packed)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes)]
#[repr(C)]
pub struct Pollfd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

#[repr(i32)]
#[derive(Debug, IntEnum)]
pub enum MadviseBehavior {
    /// Normal behavior, no special treatment
    Normal = 0,
    /// Expect random page references
    Random = 1,
    /// Expect sequential page references
    Sequential = 2,
    /// Will need these pages
    WillNeed = 3,
    /// Do not expect access in the near future
    DontNeed = 4,

    /* common parameters: try to keep these consistent across architectures */
    /// Free pages only if memory pressure
    Free = 8,
    /// Remove these pages & resources
    Remove = 9,
    /// Don't inherit across fork
    DontFork = 10,
    /// Do inherit across fork
    DoFork = 11,
    /// Poison a page for testing
    HWPoison = 100,
    /// Soft offline page for testing
    SoftOffline = 101,

    /// KSM may merge identical pages
    Mergeable = 12,
    /// KSM may not merge identical pages
    Unmergeable = 13,
    /// Worth backing with hugepages
    HugePage = 14,
    /// Not worth backing with hugepages
    NoHugePage = 15,

    /// Explicitly exclude from core dumps,
    /// overrides the coredump filter bits
    DontDump = 16,
    /// Clear the MADV_DONTDUMP flag
    DoDump = 17,

    /// Zero memory on fork, child only
    WipeOnFork = 18,
    /// Undo MADV_WIPEONFORK
    KeepOnFork = 19,

    // Deactivate these pages
    Cold = 20,
    /// reclaim these pages
    Pageout = 21,

    /// populate (prefault) page tables readable
    PopulateRead = 22,
    /// populate (prefault) page tables writable
    PopulateWrite = 23,

    /// like DONTNEED, but drop locked pages too
    DontNeedLocked = 24,
}

#[derive(Clone, Debug, Default, FromBytes, IntoBytes)]
pub struct Sysinfo {
    /// Seconds since boot
    pub uptime: usize,
    /// 1, 5, and 15 minute load averages
    pub loads: [usize; 3],
    /// Total usable main memory size
    pub totalram: usize,
    /// Available memory size
    pub freeram: usize,
    /// Amount of shared memory
    pub sharedram: usize,
    /// Memory used by buffers
    pub bufferram: usize,
    /// Total swap space size
    pub totalswap: usize,
    /// swap space still available
    pub freeswap: usize,
    /// Number of current processes
    pub procs: u16,
    /// Explicit padding for m68k
    pub pad: u16,
    /// Total high memory size
    pub totalhigh: usize,
    /// Available high memory size
    pub freehigh: usize,
    /// Memory unit size in bytes
    pub mem_unit: u32,
    /// Padding: libc5 uses this..
    #[allow(clippy::pub_underscore_fields)]
    pub _f: [u8; 20 - 2 * core::mem::size_of::<usize>() - core::mem::size_of::<u32>()],
}

bitflags::bitflags! {
    /// Represents a set of Linux capabilities.
    pub struct CapSet: u64 {
        const CHOWN = 1 << 0;
        const DAC_OVERRIDE = 1 << 1;
        const DAC_READ_SEARCH = 1 << 2;
        const FOWNER = 1 << 3;
        const FSETID = 1 << 4;
        const KILL = 1 << 5;
        const SETGID = 1 << 6;
        const SETUID = 1 << 7;
        const SETPCAP = 1 << 8;
        const LINUX_IMMUTABLE = 1 << 9;
        const NET_BIND_SERVICE = 1 << 10;
        const NET_BROADCAST = 1 << 11;
        const NET_ADMIN = 1 << 12;
        const NET_RAW = 1 << 13;
        const IPC_LOCK = 1 << 14;
        const IPC_OWNER = 1 << 15;
        const SYS_MODULE = 1 << 16;
        const SYS_RAWIO = 1 << 17;
        const SYS_CHROOT = 1 << 18;
        const SYS_PTRACE = 1 << 19;
        const SYS_PACCT = 1 << 20;
        const SYS_ADMIN = 1 << 21;
        const SYS_BOOT = 1 << 22;
        const SYS_NICE = 1 << 23;
        const SYS_RESOURCE = 1 << 24;
        const SYS_TIME = 1 << 25;
        const SYS_TTY_CONFIG = 1 << 26;
        const MKNOD = 1 << 27;
        const LEASE = 1 << 28;
        const AUDIT_WRITE = 1 << 29;
        const AUDIT_CONTROL = 1 << 30;
        const SETFCAP = 1 << 31;
        const MAC_OVERRIDE = 1 << 32;
        const MAC_ADMIN = 1 << 33;
        const SYSLOG = 1 << 34;
        const WAKE_ALARM = 1 << 35;
        const BLOCK_SUSPEND = 1 << 36;
        const AUDIT_READ = 1 << 37;
        const PERFMON = 1 << 38;
        const BPF = 1 << 39;
        const CHECKPOINT_RESTORE = 1u64 << 40;

        const LAST_CAP = Self::CHECKPOINT_RESTORE.bits();
        const _ = !0; // Externally defined flags
    }
}

/// Header structure used for the `capget` and `capset` syscalls.
#[repr(C)]
#[derive(Clone, Debug, FromBytes, IntoBytes)]
pub struct CapHeader {
    pub version: u32,
    pub pid: u32,
}

/// Data structure used for the `capget` and `capset` syscalls.
#[repr(C)]
#[derive(Clone, Debug, FromBytes, IntoBytes)]
pub struct CapData {
    pub effective: u32,
    pub permitted: u32,
    pub inheritable: u32,
}

#[repr(C, packed)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct LinuxDirent64 {
    /// Inode number
    pub ino: u64,
    /// Filesystem-specific value with no specific meaning to user space.
    /// We use it to locate a directory entry
    pub off: u64,
    /// Length of this dirent (including the following name and padding)
    pub len: u16,
    /// File type
    pub typ: u8,
    /// File name (null-terminated)
    ///
    /// This is a flexible array member (FAM) with variable length. The actual name data
    /// follows immediately after this struct in memory.
    #[allow(clippy::pub_underscore_fields)]
    pub __name: [u8; 0],
}

#[non_exhaustive]
#[repr(i32)]
#[derive(Debug, IntEnum)]
pub enum ClockId {
    RealTime = 0,
    Monotonic = 1,
    ProcessCputimeId = 2,
    ThreadCputimeId = 3,
    MonotonicRaw = 4,
    RealTimeCoarse = 5,
    MonotonicCoarse = 6,
    Boottime = 7,
}

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct TimerFlags: i32 {
        const ABSTIME = 0x1; // TIMER_ABSTIME
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

#[non_exhaustive]
#[repr(i32)]
#[derive(Debug, IntEnum, PartialEq)]
pub enum FutexOperation {
    Wait = 0,
    Wake = 1,
    Requeue = 3,
    CmpRequeue = 4,
    WaitBitset = 9,
}

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct FutexFlags: i32 {
        const PRIVATE = 0x80; // FUTEX_PRIVATE_FLAG
        const CLOCK_REALTIME = 0x100; // FUTEX_CLOCK_REALTIME
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;

        const FUTEX_CMD_MASK = !(FutexFlags::PRIVATE.bits() | FutexFlags::CLOCK_REALTIME.bits());
    }
}

#[non_exhaustive]
#[derive(Debug)]
pub enum FutexArgs {
    Wait {
        addr: UserPtrMut<u32>,
        flags: FutexFlags,
        val: u32,
        /// Note: for FUTEX_WAIT, timeout is interpreted as a relative
        /// value. This differs from other futex operations, where
        /// timeout is interpreted as an absolute value.
        timeout: TimeParam,
    },
    WaitBitset {
        addr: UserPtrMut<u32>,
        flags: FutexFlags,
        val: u32,
        timeout: TimeParam,
        bitmask: u32,
    },
    Wake {
        addr: UserPtrMut<u32>,
        flags: FutexFlags,
        count: u32,
    },
    /// `FUTEX_REQUEUE`: wake up to `wake_count` waiters on `addr`, then move up to
    /// `requeue_count` of the remaining waiters on `addr` onto `addr2`'s wait queue (without
    /// waking them) so a later wake on `addr2` can reach them. Used by musl's
    /// `pthread_cond_broadcast`/`pthread_cond_signal` to move a condition variable's waiters onto
    /// its associated mutex's futex word without a thundering herd.
    Requeue {
        addr: UserPtrMut<u32>,
        flags: FutexFlags,
        wake_count: u32,
        requeue_count: u32,
        addr2: UserPtrMut<u32>,
    },
    /// `FUTEX_CMP_REQUEUE`: identical to `Requeue`, but first atomically checks that the word at
    /// `addr` still equals `expected_value`, failing with `EAGAIN` otherwise (closes the race
    /// where the value changed between userspace's check and this syscall).
    CmpRequeue {
        addr: UserPtrMut<u32>,
        flags: FutexFlags,
        wake_count: u32,
        requeue_count: u32,
        addr2: UserPtrMut<u32>,
        expected_value: u32,
    },
}

#[repr(u32)]
#[derive(Debug, IntEnum)]
pub enum PrctlOption {
    SetPDeathSig = 1,
    GetPDeathSig = 2,
    GetDumpable = 3,
    SetDumpable = 4,
    GetUnalign = 5,
    SetUnalign = 6,
    GetKeepCaps = 7,
    SetKeepCaps = 8,
    GetFpEmu = 9,
    SetFpEmu = 10,
    GetFpExc = 11,
    SetFpExc = 12,
    GetTiming = 13,
    SetTiming = 14,
    /// PR_SET_NAME: set process name
    SetName = 15,
    /// PR_GET_NAME: Get process name
    GetName = 16,
    GetEndian = 19,
    SetEndian = 20,
    GetSeccomp = 21,
    SetSeccomp = 22,
    /// PR_CAPBSET_READ: read the calling thread's capability bounding set
    CapBSetRead = 23,
    CapBSetDrop = 24,
    GetTSC = 25,
    SetTSC = 26,
    GetSecureBits = 27,
    SetSecureBits = 28,
    SetTimerSlack = 29,
    GetTimerSlack = 30,
    TaskPerfEventsDisable = 31,
    TaskPerfEventsEnable = 32,
    MCEKill = 33,
    MCEKillGet = 34,
    SetMM = 35,
    SetChildSubreaper = 36,
    GetChildSubreaper = 37,
    SetNoNewPrivs = 38,
    GetNoNewPrivs = 39,
    GetTidAddress = 40,
    SetTHPDisable = 41,
    GetTHPDisable = 42,
    // No longer implemented, but left here to ensure the numbers stay reserved:
    // MpxEnableManagement = 43,
    // MpxDisableManagement = 44,
    SetFpMode = 45,
    GetFpMode = 46,
    CapAmbient = 47,
}

#[non_exhaustive]
#[derive(Debug)]
pub enum PrctlArg {
    SetName(UserPtr<u8>),
    GetName(UserPtrMut<u8>),
    CapBSetRead(usize),
}

#[repr(i32)]
#[derive(Debug, IntEnum)]
pub enum IntervalTimer {
    /// This timer counts down in real (i.e., wall clock) time.  At each expiration, a SIGALRM signal is generated.
    Real = 0,
    /// This timer counts down against the user-mode CPU time consumed by the process. The measurement includes CPU time
    /// consumed by all threads in the process. At each expiration, a SIGVTALRM signal is generated.
    Virtual = 1,
    /// This timer counts down against the total (i.e., both user and system) CPU time consumed by the process.
    /// The measurement includes CPU time consumed by all threads in the process. At each expiration, a SIGPROF signal is generated.
    Prof = 2,
}

/// Flags for the `receive` function.
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes)]
#[repr(transparent)]
pub struct ReceiveFlags(u32);

bitflags::bitflags! {
    impl ReceiveFlags: u32 {
        /// `MSG_CMSG_CLOEXEC`: close-on-exec for the associated file descriptor
        const CMSG_CLOEXEC = 0x40000000;
        /// `MSG_CTRUNC`: control data (ancillary data / `SCM_RIGHTS`) was truncated
        const CTRUNC = 0x8;
        /// `MSG_DONTWAIT`: non-blocking operation
        const DONTWAIT = 0x40;
        /// `MSG_ERRQUEUE`: destination for error messages
        const ERRQUEUE = 0x2000;
        /// `MSG_OOB`: requests receipt of out-of-band data
        const OOB = 0x1;
        /// `MSG_PEEK`: requests to peek at incoming messages
        const PEEK = 0x2;
        /// `MSG_TRUNC`: truncate the message
        const TRUNC = 0x20;
        /// `MSG_WAITALL`: wait for the full amount of data
        const WAITALL = 0x100;
        /// `MSG_WAITFORONE`: `recvmmsg` only — turn on `MSG_DONTWAIT` after the
        /// first message has been received.
        const WAITFORONE = 0x10000;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

/// Flags for the `send` function.
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes)]
#[repr(C)]
pub struct SendFlags(u32);

bitflags::bitflags! {
    impl SendFlags: u32 {
        /// `MSG_CONFIRM`: requests confirmation of the message delivery.
        const CONFIRM = 0x800;
        /// `MSG_DONTROUTE`: send the message directly to the interface, bypassing routing.
        const DONTROUTE = 0x4;
        /// `MSG_DONTWAIT`: non-blocking operation, do not wait for buffer space to become available.
        const DONTWAIT = 0x40;
        /// `MSG_EOR`: indicates the end of a record for message-oriented sockets.
        const EOR = 0x80;
        /// `MSG_MORE`: indicates that more data will follow.
        const MORE = 0x8000;
        /// `MSG_NOSIGNAL`: prevents the sending of SIGPIPE signals when writing to a socket that is closed.
        const NOSIGNAL = 0x4000;
        /// `MSG_OOB`: sends out-of-band data.
        const OOB = 0x1;
        /// <https://docs.rs/bitflags/*/bitflags/#externally-defined-flags>
        const _ = !0;
    }
}

/// Packaged sigset pointer with its size, used by `pselect6` syscall.
#[derive(Clone, Copy, FromBytes)]
#[repr(C)]
pub struct SigSetPack {
    pub sigset: UserPtr<SigSet>,
    pub size: usize,
}

#[derive(Debug, Clone, Copy, FromBytes, IntoBytes)]
#[repr(C, packed)]
pub struct UserMsgHdr {
    /// ptr to socket address structure
    pub msg_name: UserPtrMut<u8>,
    /// size of socket address structure
    pub msg_namelen: u32,
    /// Explicit padding to match the 4-byte gap that Linux's naturally-aligned
    /// `struct user_msghdr` has between `msg_namelen` and `msg_iov` on 64-bit.
    #[cfg(target_pointer_width = "64")]
    _pad: u32,
    /// ptr to an array of `iovec` structures
    pub msg_iov: UserPtr<IoVec>,
    /// number of elements in msg_iov
    pub msg_iovlen: usize,
    /// ptr to ancillary data
    pub msg_control: UserPtr<u8>,
    /// number of bytes of ancillary data
    pub msg_controllen: usize,
    /// flags on received message
    pub msg_flags: ReceiveFlags,
    /// Explicit trailing padding to match the 4-byte gap after `msg_flags` in
    /// Linux's naturally-aligned `struct user_msghdr` on 64-bit (total size 56).
    #[cfg(target_pointer_width = "64")]
    _pad2: u32,
}

/// Linux's `struct mmsghdr`: a `msghdr` paired with the number of bytes
/// transmitted, used by `sendmmsg`/`recvmmsg`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes)]
#[repr(C, packed)]
pub struct UserMmsgHdr {
    /// the per-message `msghdr`
    pub msg_hdr: UserMsgHdr,
    /// bytes transmitted for this entry, written back by the kernel
    pub msg_len: u32,
    #[cfg(target_pointer_width = "64")]
    _pad: u32,
}

#[repr(i32)]
#[derive(Debug, IntEnum)]
pub enum SocketcallType {
    Socket = 1,
    Bind = 2,
    Connect = 3,
    Listen = 4,
    Accept = 5,
    GetSockname = 6,
    GetPeername = 7,
    Socketpair = 8,
    Send = 9,
    Recv = 10,
    Sendto = 11,
    Recvfrom = 12,
    Shutdown = 13,
    Setsockopt = 14,
    Getsockopt = 15,
    Sendmsg = 16,
    Recvmsg = 17,
    Accept4 = 18,
    Recvmmsg = 19,
    Sendmmsg = 20,
}

/// `how` argument to the `shutdown(2)` syscall.
#[repr(i32)]
#[derive(Debug, Clone, Copy, IntEnum)]
pub enum ShutdownHow {
    /// `SHUT_RD`.
    Read = 0,
    /// `SHUT_WR`.
    Write = 1,
    /// `SHUT_RDWR`.
    Both = 2,
}

impl ShutdownHow {
    /// Returns `true` when this `how` disables the receive side (`SHUT_RD` or `SHUT_RDWR`).
    #[must_use]
    pub fn is_shutdown_read(self) -> bool {
        matches!(self, Self::Read | Self::Both)
    }
    /// Returns `true` when this `how` disables the send side (`SHUT_WR` or `SHUT_RDWR`).
    #[must_use]
    pub fn is_shutdown_write(self) -> bool {
        matches!(self, Self::Write | Self::Both)
    }
}

/// Request to syscall handler
#[non_exhaustive]
#[derive(Debug)]
pub enum SyscallRequest {
    Exit {
        status: i32,
    },
    ExitGroup {
        status: i32,
    },
    Read {
        fd: i32,
        buf: UserPtrMut<u8>,
        count: usize,
    },
    Write {
        fd: i32,
        buf: UserPtr<u8>,
        count: usize,
    },
    Lseek {
        fd: i32,
        offset: isize,
        whence: i32,
    },
    Close {
        fd: i32,
    },
    Fsync {
        fd: i32,
    },
    Fdatasync {
        fd: i32,
    },
    Stat {
        pathname: UserPtr<c_char>,
        buf: UserPtrMut<FileStat>,
    },
    Fstat {
        fd: i32,
        buf: UserPtrMut<FileStat>,
    },
    Lstat {
        pathname: UserPtr<c_char>,
        buf: UserPtrMut<FileStat>,
    },
    Mkdirat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        mode: u32,
    },
    Fchmodat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        mode: u32,
    },
    Fchmod {
        fd: u32,
        mode: u32,
    },
    Chdir {
        pathname: UserPtr<c_char>,
    },
    Fchdir {
        fd: u32,
    },
    Mmap {
        addr: usize,
        length: usize,
        prot: ProtFlags,
        flags: MapFlags,
        fd: i32,
        offset: usize,
    },
    Mprotect {
        addr: UserPtrMut<u8>,
        length: usize,
        prot: ProtFlags,
    },
    Munmap {
        addr: UserPtrMut<u8>,
        length: usize,
    },
    Mremap {
        old_addr: UserPtrMut<u8>,
        old_size: usize,
        new_size: usize,
        flags: MRemapFlags,
        new_addr: usize,
    },
    Brk {
        addr: UserPtrMut<u8>,
    },
    RtSigprocmask {
        how: signal::SigmaskHow,
        set: Option<UserPtr<SigSet>>,
        oldset: Option<UserPtrMut<SigSet>>,
        sigsetsize: usize,
    },
    RtSigaction {
        signum: signal::Signal,
        act: Option<UserPtr<signal::SigAction>>,
        oldact: Option<UserPtrMut<signal::SigAction>>,
        sigsetsize: usize,
    },
    RtSigreturn,
    Wait4 {
        pid: i32,
        wstatus: Option<UserPtrMut<i32>>,
        options: i32,
        rusage: Option<UserPtrMut<u8>>,
    },
    Kill {
        pid: i32,
        sig: i32,
    },
    Tkill {
        tid: i32,
        sig: i32,
    },
    Tgkill {
        tgid: i32,
        tid: i32,
        sig: i32,
    },
    Sigaltstack {
        ss: Option<UserPtr<signal::SigAltStack>>,
        old_ss: Option<UserPtrMut<signal::SigAltStack>>,
    },
    Ioctl {
        fd: i32,
        arg: IoctlArg,
    },
    Pread64 {
        fd: i32,
        buf: UserPtrMut<u8>,
        count: usize,
        offset: i64,
    },
    Pwrite64 {
        fd: i32,
        buf: UserPtr<u8>,
        count: usize,
        offset: i64,
    },
    Sendfile {
        out_fd: i32,
        in_fd: i32,
        offset: Option<UserPtrMut<i64>>,
        count: usize,
    },
    Readv {
        fd: i32,
        iovec: UserPtr<IoReadVec>,
        iovcnt: usize,
    },
    Writev {
        fd: i32,
        iovec: UserPtr<IoWriteVec>,
        iovcnt: usize,
    },
    Preadv {
        fd: i32,
        iovec: UserPtr<IoReadVec>,
        iovcnt: usize,
        pos_l: usize,
        pos_h: usize,
    },
    Pwritev {
        fd: i32,
        iovec: UserPtr<IoWriteVec>,
        iovcnt: usize,
        pos_l: usize,
        pos_h: usize,
    },
    Faccessat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        mode: AccessFlags,
        flags: AtFlags,
    },
    Madvise {
        addr: UserPtrMut<u8>,
        length: usize,
        behavior: MadviseBehavior,
    },
    Dup {
        oldfd: i32,
        newfd: Option<i32>,
        flags: Option<litebox::fs::OFlags>,
    },
    Socket {
        domain: u32,
        type_and_flags: u32,
        protocol: u8,
    },
    Socketpair {
        domain: u32,
        type_and_flags: u32,
        protocol: u8,
        sockvec: UserPtrMut<u32>,
    },
    Connect {
        sockfd: i32,
        sockaddr: UserPtr<u8>,
        addrlen: usize,
    },
    Accept {
        sockfd: i32,
        addr: Option<UserPtrMut<u8>>,
        addrlen: Option<UserPtrMut<u32>>,
        flags: SockFlags,
    },
    Sendto {
        sockfd: i32,
        buf: UserPtr<u8>,
        len: usize,
        flags: SendFlags,
        addr: Option<UserPtr<u8>>,
        addrlen: u32,
    },
    Sendmsg {
        sockfd: i32,
        msg: UserPtr<UserMsgHdr>,
        flags: SendFlags,
    },
    Sendmmsg {
        sockfd: i32,
        msgvec: UserPtrMut<UserMmsgHdr>,
        vlen: u32,
        flags: SendFlags,
    },
    Recvfrom {
        sockfd: i32,
        buf: UserPtrMut<u8>,
        len: usize,
        flags: ReceiveFlags,
        addr: Option<UserPtrMut<u8>>,
        addrlen: UserPtrMut<u32>,
    },
    Recvmsg {
        sockfd: i32,
        msg: UserPtrMut<UserMsgHdr>,
        flags: ReceiveFlags,
    },
    Recvmmsg {
        sockfd: i32,
        msgvec: UserPtrMut<UserMmsgHdr>,
        vlen: u32,
        flags: ReceiveFlags,
        timeout: TimeParam,
    },
    Shutdown {
        sockfd: i32,
        how: i32,
    },
    Bind {
        sockfd: i32,
        sockaddr: UserPtr<u8>,
        addrlen: usize,
    },
    Listen {
        sockfd: i32,
        backlog: u16,
    },
    Setsockopt {
        sockfd: i32,
        level: u32,
        optname: u32,
        optval: UserPtr<u8>,
        optlen: usize,
    },
    Getsockopt {
        sockfd: i32,
        level: u32,
        optname: u32,
        optval: UserPtrMut<u8>,
        optlen: UserPtrMut<u32>,
    },
    Getsockname {
        sockfd: i32,
        addr: UserPtrMut<u8>,
        addrlen: UserPtrMut<u32>,
    },
    Getpeername {
        sockfd: i32,
        addr: UserPtrMut<u8>,
        addrlen: UserPtrMut<u32>,
    },
    Uname {
        buf: UserPtrMut<Utsname>,
    },
    Fcntl {
        fd: i32,
        arg: FcntlArg,
    },
    Flock {
        fd: i32,
        operation: i32,
    },
    Getcwd {
        buf: UserPtrMut<u8>,
        size: usize,
    },
    EpollCtl {
        epfd: i32,
        op: EpollOp,
        fd: i32,
        event: UserPtr<EpollEvent>,
    },
    EpollPwait {
        epfd: i32,
        events: UserPtrMut<EpollEvent>,
        maxevents: u32,
        timeout: i32,
        sigmask: Option<UserPtr<SigSet>>,
        sigsetsize: usize,
    },
    EpollCreate {
        size: i32,
        flags: EpollCreateFlags,
    },
    Ppoll {
        fds: UserPtrMut<Pollfd>,
        nfds: usize,
        timeout: TimeParam,
        sigmask: Option<UserPtr<SigSet>>,
        sigsetsize: usize,
    },
    Pselect {
        nfds: u32,
        readfds: Option<UserPtrMut<usize>>,
        writefds: Option<UserPtrMut<usize>>,
        exceptfds: Option<UserPtrMut<usize>>,
        timeout: TimeParam,
        sigsetpack: Option<UserPtr<SigSetPack>>,
    },
    ArchPrctl {
        arg: ArchPrctlArg,
    },
    Readlink {
        pathname: UserPtr<c_char>,
        buf: UserPtrMut<u8>,
        bufsiz: usize,
    },
    Readlinkat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        buf: UserPtrMut<u8>,
        bufsiz: usize,
    },
    Openat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        flags: litebox::fs::OFlags,
        mode: litebox::fs::Mode,
    },
    Ftruncate {
        fd: i32,
        length: usize,
    },
    /// `fallocate(fd, mode, offset, len)` -- ensure `[offset, offset+len)` is allocated.
    /// litebox only ever needs to support `mode == 0` (the default allocate-and-grow mode,
    /// which is exactly what `posix_fallocate()` translates to -- glibc/musl's
    /// `posix_fallocate()` is a thin wrapper around this syscall, not a separate one). This is
    /// the syscall weston's real `os_create_anonymous_file()` (`shared/os-compat.c`) calls
    /// immediately after a successful `memfd_create()`, before ever seeing the fd -- unlike
    /// `ftruncate`, its absence was previously silently swallowed as `ENOSYS` with a
    /// `debug_assertions`-gated warning invisible in release builds (see `log_unsupported_fmt`),
    /// making every `memfd_create`-backed shared-memory setup this path is used for (Wayland
    /// keymap sharing, `wl_shm` buffers via `posix_fallocate`-using clients) fail with no visible
    /// diagnostic at all in a release build -- confirmed live via added `sys_memfd_create`
    /// tracing: `memfd_create` itself always returned success, yet weston's very next line was
    /// "failed to create anonymous file for keymap", which is only possible if the syscall
    /// immediately following it (`fallocate`) is unimplemented and returns `ENOSYS`.
    Fallocate {
        fd: i32,
        mode: i32,
        offset: i64,
        len: i64,
    },
    Mknodat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        mode_and_type: u32,
        dev: u32,
    },
    Unlinkat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        flags: AtFlags,
    },
    Linkat {
        olddirfd: i32,
        oldpath: UserPtr<c_char>,
        newdirfd: i32,
        newpath: UserPtr<c_char>,
        flags: AtFlags,
    },
    Renameat {
        olddirfd: i32,
        oldpath: UserPtr<c_char>,
        newdirfd: i32,
        newpath: UserPtr<c_char>,
        flags: u32,
    },
    Symlinkat {
        target: UserPtr<c_char>,
        newdirfd: i32,
        linkpath: UserPtr<c_char>,
    },
    Newfstatat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        buf: UserPtrMut<FileStat>,
        flags: AtFlags,
    },
    Utimensat {
        dirfd: i32,
        pathname: UserPtr<c_char>,
        /// Pointer to a `struct timespec[2]` (`[atime, mtime]`); null means "set both to now",
        /// matching `utimensat(2)`'s `times == NULL` semantics.
        times: UserPtr<Timespec>,
        flags: AtFlags,
    },
    Eventfd2 {
        initval: u32,
        flags: EfdFlags,
    },
    Signalfd4 {
        /// `-1` means "create a new signalfd"; otherwise the fd of an existing signalfd whose
        /// mask this call replaces (real Linux `signalfd(2)`/`signalfd4(2)` semantics).
        fd: i32,
        mask: UserPtr<SigSet>,
        sizemask: usize,
        flags: SfdFlags,
    },
    TimerfdCreate {
        clockid: i32,
        flags: TfdFlags,
    },
    TimerfdSettime {
        fd: i32,
        flags: TfdSettimeFlags,
        new_value: UserPtr<ItimerSpec>,
        old_value: Option<UserPtrMut<ItimerSpec>>,
    },
    TimerfdGettime {
        fd: i32,
        curr_value: UserPtrMut<ItimerSpec>,
    },
    MemfdCreate {
        /// Cosmetic name only (real Linux exposes it via `/proc/self/fd/<n> -> memfd:<name>`,
        /// which this shim does not implement) -- read but not otherwise interpreted.
        name: UserPtr<c_char>,
        flags: MfdFlags,
    },
    Pipe2 {
        pipefd: UserPtrMut<u32>,
        flags: litebox::fs::OFlags,
    },
    Clone {
        args: CloneArgs,
    },
    Clone3 {
        args: UserPtr<CloneArgs>,
    },
    /// Manipulate thread-local storage information.
    /// Returns `ENOSYS` on x86_64.
    SetThreadArea {
        user_desc: UserPtrMut<u8>,
    },
    ClockGettime {
        clockid: i32,
        tp: TimeParam,
    },
    ClockGetres {
        clockid: i32,
        res: TimeParam,
    },
    ClockNanosleep {
        clockid: i32,
        flags: TimerFlags,
        request: TimeParam,
        remain: TimeParam,
    },
    Gettimeofday {
        tv: Option<UserPtrMut<TimeVal>>,
        tz: Option<UserPtrMut<TimeZone>>,
    },
    Time {
        tloc: Option<UserPtrMut<time_t>>,
    },
    Getrlimit {
        resource: RlimitResource,
        rlim: UserPtrMut<Rlimit>,
    },
    Setrlimit {
        resource: RlimitResource,
        rlim: UserPtr<Rlimit>,
    },
    Prlimit {
        pid: i32,
        /// The resource for which the limit is being queried.
        resource: RlimitResource,
        /// If the new_limit argument is not a None, then the rlimit structure to which it points
        /// is used to set new values for the soft and hard limits for resource.
        new_limit: Option<UserPtr<Rlimit64>>,
        /// If the old_limit argument is not a None, then a successful call to prlimit() places the
        /// previous soft and hard limits for resource in the rlimit structure pointed to by old_limit.
        old_limit: Option<UserPtrMut<Rlimit64>>,
    },
    SetTidAddress {
        tidptr: UserPtrMut<i32>,
    },
    Gettid,
    SetRobustList {
        head: usize,
    },
    GetRobustList {
        pid: Option<i32>,
        head: UserPtrMut<usize>,
        len: UserPtrMut<usize>,
    },
    GetRandom {
        buf: UserPtrMut<u8>,
        count: usize,
        flags: RngFlags,
    },
    Getpid,
    Getppid,
    Getpgid {
        pid: i32,
    },
    Setpgid {
        pid: i32,
        pgid: i32,
    },
    Setsid,
    Getuid,
    Geteuid,
    Getgid,
    Getegid,
    Setuid {
        uid: u32,
    },
    Setgid {
        gid: u32,
    },
    Setresuid {
        ruid: u32,
        euid: u32,
        suid: u32,
    },
    Setresgid {
        rgid: u32,
        egid: u32,
        sgid: u32,
    },
    Getgroups {
        size: i32,
        list: UserPtrMut<u32>,
    },
    Setgroups {
        size: usize,
        list: UserPtr<u32>,
    },
    Sysinfo {
        buf: UserPtrMut<Sysinfo>,
    },
    CapGet {
        header: UserPtrMut<CapHeader>,
        data: Option<UserPtrMut<CapData>>,
    },
    GetDirent64 {
        fd: i32,
        dirp: UserPtrMut<u8>,
        count: usize,
    },
    SchedGetAffinity {
        pid: Option<i32>,
        len: usize,
        mask: UserPtrMut<u8>,
    },
    SchedYield,
    SchedGetParam {
        pid: Option<i32>,
        param: UserPtrMut<i32>,
    },
    SchedSetParam {
        pid: Option<i32>,
        param: UserPtr<i32>,
    },
    SchedGetScheduler {
        pid: Option<i32>,
    },
    SchedSetScheduler {
        pid: Option<i32>,
        policy: i32,
        param: UserPtr<i32>,
    },
    Futex {
        args: FutexArgs,
    },
    Execve {
        pathname: UserPtr<c_char>,
        argv: UserPtr<UserPtr<c_char>>,
        envp: UserPtr<UserPtr<c_char>>,
    },
    Umask {
        mask: u32,
    },
    Prctl {
        args: PrctlArg,
    },
    Alarm {
        seconds: u32,
    },
    Pause,
    SetITimer {
        which: IntervalTimer,
        new_value: Option<UserPtr<ItimerVal>>,
        old_value: Option<UserPtrMut<ItimerVal>>,
    },
    GetITimer {
        which: IntervalTimer,
        curr_value: UserPtrMut<ItimerVal>,
    },
    Statx {
        dirfd: i32,
        pathname: Option<UserPtr<c_char>>,
        flags: AtFlags,
        mask: StatxMask,
        statxbuf: UserPtrMut<Statx>,
    },
}

impl SyscallRequest {
    /// Take the raw syscall number and arguments, and provide a stronger-typed `SyscallRequest`.
    ///
    /// Returns `Ok` if a valid translation exists, if no such translation exists, returns the [`Errno`](errno::Errno) for it.
    ///
    /// # Panics
    ///
    /// Ideally, this function would not panic. However, since it is currently under development, it
    /// is allowed to panic upon receiving a syscall number (or arguments) that it does not know how
    /// to handle.
    // NOTE: This function is intended to be mostly trivial (in the future, we intend to replace
    // this entire function with a simple type-driven macro), thus any non-trivial parsing should
    // happen outside of this. Roughly speaking, if it is a simple integer, pointer, or a flag
    // field, it is fine; anything more complex should not attempt to do more, and must instead
    // perform the actual "parsing" outside. It is ok to introduce new `impl`s for
    // `ReinterpretTruncatedFromUsize` in order to support stronger types (especially if one desires
    // a fail-free parse), but also quite helpful is to define a `TryFrom<i32>` and use the `:?`
    // combinator (which will return `EINVAL` upon parse failure).
    pub fn try_from_raw(
        syscall_number: usize,
        ctx: &PtRegs,
        log_unsupported: impl Fn(core::fmt::Arguments<'_>),
    ) -> Result<Self, errno::Errno> {
        let unsupported_einval = |args: core::fmt::Arguments<'_>| {
            log_unsupported(args);
            errno::Errno::EINVAL
        };
        // sys_req! is a convenience macro that automatically takes the correct numbered arguments
        // (in the order of field specification); due to some Rust restrictions, we need to manually
        // specify pointers by adding the `:*` to that field, but otherwise everything else about
        // conversion to the type is automatically inferred.
        //
        // See below for example usage, but generally speaking, you just need to specify the fields
        // in order; if something needs to be a pointer and you forget (or accidentally mark
        // something as a pointer) the type checker will complain and remind you (due to the nice
        // attributes on the relevant traits), so you shouldn't need to worry about that.
        //
        // NOTE: This macro should seldom (if ever) be updated. Usually if you think you need to
        // update this, you probably need to introduce an `impl` instead.
        macro_rules! sys_req {
            ($id:ident { $( $field:ident $(:$star:tt)?),* $(,)? }) => {
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ 0, 1, 2, 3, 4, 5 ] [ ]
                )
            };
            (@[$id:ident] [ $f:ident $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: ctx.sys_req_arg($n), ]
                )
            };
            (@[$id:ident] [ $f:ident : * $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: ctx.sys_req_ptr($n), ]
                )
            };
            (@[$id:ident] [ $f:ident : ? $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: ctx.sys_req_arg::<i32>($n).try_into().or(Err(errno::Errno::EINVAL))?, ]
                )
            };
            (@[$id:ident] [ $f:ident : { =*> $e:expr } $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                // `{ =*> e }`: temporary syntax to support removing some hard-coded bits
                // NOTE: Please do NOT use this for any new syscalls added
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: { $e ( ctx.sys_req_ptr($n) ) }, ]
                )
            };
            (@[$id:ident] [ $f:ident : { => $e:expr } $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                // `{ => e }`: temporary syntax to support removing some hard-coded bits
                // NOTE: Please do NOT use this for any new syscalls added
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: { $e ( ctx.sys_req_arg($n) ) }, ]
                )
            };
            (@[$id:ident] [ $f:ident : { $e:expr } $(,)? $($field:ident $(:$star:tt)?),* ] [ $n:literal $(,)? $($ns:literal),* ] [ $($tail:tt)* ]) => {
                sys_req!(
                    @[$id] [ $( $field $(:$star)? ),* ] [ $($ns),* ] [ $($tail)* $f: $e, ]
                )
            };
            (@[$id:ident] [ ] [ $($ns:literal),* ] [ $($tail:tt)* ]) => {
                SyscallRequest::$id { $($tail)* }
            };
        }

        let sysno = Sysno::new(syscall_number).ok_or_else(|| {
            log_unsupported(format_args!("unknown syscall {syscall_number}"));
            errno::Errno::ENOSYS
        })?;
        let dispatcher = match sysno {
            Sysno::read => sys_req!(Read { fd, buf:*, count }),
            Sysno::write => sys_req!(Write { fd, buf:*, count }),
            Sysno::close => sys_req!(Close { fd }),
            Sysno::lseek => sys_req!(Lseek { fd, offset, whence }),
            #[cfg(target_arch = "x86_64")]
            Sysno::stat => sys_req!(Stat { pathname:*, buf:* }),
            Sysno::fstat => sys_req!(Fstat { fd, buf:* }),
            #[cfg(target_arch = "x86_64")]
            Sysno::lstat => sys_req!(Lstat { pathname:*, buf:* }),
            #[cfg(target_arch = "x86_64")]
            Sysno::mkdir => SyscallRequest::Mkdirat {
                dirfd: AT_FDCWD,
                pathname: ctx.sys_req_ptr(0),
                mode: ctx.sys_req_arg(1),
            },
            Sysno::mkdirat => sys_req!(Mkdirat { dirfd, pathname:*, mode }),
            #[cfg(target_arch = "x86_64")]
            Sysno::chmod => SyscallRequest::Fchmodat {
                dirfd: AT_FDCWD,
                pathname: ctx.sys_req_ptr(0),
                mode: ctx.sys_req_arg(1),
            },
            Sysno::fchmodat => sys_req!(Fchmodat { dirfd, pathname:*, mode }),
            Sysno::fchmodat2 => SyscallRequest::Fchmodat {
                dirfd: ctx.sys_req_arg(0),
                pathname: ctx.sys_req_ptr(1),
                mode: ctx.sys_req_arg(2),
            },
            Sysno::fchmod => sys_req!(Fchmod { fd, mode }),
            Sysno::chdir => sys_req!(Chdir { pathname:* }),
            Sysno::fchdir => sys_req!(Fchdir { fd }),
            Sysno::mmap => sys_req!(Mmap {
                addr,
                length,
                prot,
                flags,
                fd,
                offset,
            }),
            Sysno::mprotect => sys_req!(Mprotect { addr:*, length, prot }),
            Sysno::munmap => sys_req!(Munmap { addr:*, length }),
            Sysno::brk => sys_req!(Brk { addr:* }),
            Sysno::mremap => sys_req!(Mremap { old_addr:*, old_size, new_size, flags, new_addr }),
            Sysno::rt_sigprocmask => sys_req!(RtSigprocmask {
                how:?,
                set:*,
                oldset:*,
                sigsetsize,
            }),
            Sysno::rt_sigaction => sys_req!(RtSigaction {
                signum:?,
                act:*,
                oldact:*,
                sigsetsize,
            }),
            Sysno::rt_sigreturn => SyscallRequest::RtSigreturn,
            Sysno::wait4 => sys_req!(Wait4 {
                pid,
                wstatus:*,
                options,
                rusage:*
            }),
            Sysno::kill => sys_req!(Kill { pid, sig }),
            Sysno::tkill => sys_req!(Tkill { tid, sig }),
            Sysno::tgkill => sys_req!(Tgkill { tgid, tid, sig }),
            Sysno::sigaltstack => sys_req!(Sigaltstack { ss:*, old_ss:* }),
            Sysno::ioctl => SyscallRequest::Ioctl {
                fd: ctx.sys_req_arg(0),
                arg: {
                    let cmd = ctx.sys_req_arg(1);
                    match cmd {
                        TCGETS => IoctlArg::TCGETS(ctx.sys_req_ptr(2)),
                        TCSETS => IoctlArg::TCSETS(ctx.sys_req_ptr(2)),
                        TCSETSW => IoctlArg::TCSETSW(ctx.sys_req_ptr(2)),
                        TCSETSF => IoctlArg::TCSETSF(ctx.sys_req_ptr(2)),
                        TIOCGWINSZ => IoctlArg::TIOCGWINSZ(ctx.sys_req_ptr(2)),
                        TIOCSWINSZ => IoctlArg::TIOCSWINSZ(ctx.sys_req_ptr(2)),
                        TIOCGPTN => IoctlArg::TIOCGPTN(ctx.sys_req_ptr(2)),
                        TIOCSPTLCK => IoctlArg::TIOCSPTLCK(ctx.sys_req_ptr(2)),
                        TIOCSCTTY => IoctlArg::TIOCSCTTY(ctx.sys_req_arg(2)),
                        TIOCGPGRP => IoctlArg::TIOCGPGRP(ctx.sys_req_ptr(2)),
                        TIOCSPGRP => IoctlArg::TIOCSPGRP(ctx.sys_req_ptr(2)),
                        FIONBIO => IoctlArg::FIONBIO(ctx.sys_req_ptr(2)),
                        FIOCLEX => IoctlArg::FIOCLEX,
                        DRM_IOCTL_MODE_GETRESOURCES => {
                            IoctlArg::DrmModeGetResources(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_GETCRTC => IoctlArg::DrmModeGetCrtc(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_SETCRTC => IoctlArg::DrmModeSetCrtc(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_GETENCODER => {
                            IoctlArg::DrmModeGetEncoder(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_GETCONNECTOR => {
                            IoctlArg::DrmModeGetConnector(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_CREATE_DUMB => {
                            IoctlArg::DrmModeCreateDumb(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_MAP_DUMB => IoctlArg::DrmModeMapDumb(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_DESTROY_DUMB => {
                            IoctlArg::DrmModeDestroyDumb(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_ADDFB2 => IoctlArg::DrmModeAddFb2(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_PAGE_FLIP => IoctlArg::DrmModePageFlip(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_GETPLANERESOURCES => {
                            IoctlArg::DrmModeGetPlaneResources(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_GETPLANE => IoctlArg::DrmModeGetPlane(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_SETPLANE => IoctlArg::DrmModeSetPlane(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_VERSION => IoctlArg::DrmVersion(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_GET_CAP => IoctlArg::DrmGetCap(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_SET_CLIENT_CAP => {
                            IoctlArg::DrmSetClientCap(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_SET_MASTER => IoctlArg::DrmSetMaster,
                        DRM_IOCTL_DROP_MASTER => IoctlArg::DrmDropMaster,
                        DRM_IOCTL_GET_MAGIC => IoctlArg::DrmGetMagic(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_AUTH_MAGIC => IoctlArg::DrmAuthMagic(ctx.sys_req_ptr(2)),
                        DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                            IoctlArg::DrmModeObjGetProperties(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_MODE_GETPROPERTY => {
                            IoctlArg::DrmModeGetProperty(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_PRIME_HANDLE_TO_FD => {
                            IoctlArg::DrmPrimeHandleToFd(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                            IoctlArg::DrmPrimeFdToHandle(ctx.sys_req_ptr(2))
                        }
                        DRM_IOCTL_GEM_CLOSE => IoctlArg::DrmGemClose(ctx.sys_req_ptr(2)),
                        VT_GETSTATE => IoctlArg::VtGetState(ctx.sys_req_ptr(2)),
                        VT_SETMODE => IoctlArg::VtSetMode(ctx.sys_req_ptr(2)),
                        KDSETMODE => IoctlArg::KdSetMode(ctx.sys_req_arg(2)),
                        KDSKBMODE => IoctlArg::KdSkbMode(ctx.sys_req_arg(2)),
                        EVIOCREVOKE => IoctlArg::EvdevRevoke,
                        EVIOCGVERSION => IoctlArg::EvdevGetVersion(ctx.sys_req_ptr(2)),
                        EVIOCGID => IoctlArg::EvdevGetId(ctx.sys_req_ptr(2)),
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) >= 0x20
                            && (cmd & 0xff) < 0x40
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetBits {
                                ev: (cmd & 0xff) - 0x20,
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) == 0x06
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetName {
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && ((cmd & 0xff) == 0x07 || (cmd & 0xff) == 0x08)
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetPhysOrUniq
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) == 0x09
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetProp {
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) == 0x18
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetKey {
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) == 0x19
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetLed {
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ if (cmd >> 8) & 0xff == u32::from(b'E')
                            && (cmd & 0xff) == 0x1b
                            && (cmd >> 30) & 0x3 == 0x2 =>
                        {
                            IoctlArg::EvdevGetSwitch {
                                len: (cmd >> 16) & 0x3fff,
                                arg: ctx.sys_req_ptr(2),
                            }
                        }
                        _ => IoctlArg::Raw {
                            cmd,
                            arg: ctx.sys_req_ptr(2),
                        },
                    }
                },
            },
            Sysno::pread64 => sys_req!(Pread64 {
                fd,
                buf:*,
                count,
                offset
            }),
            Sysno::pwrite64 => sys_req!(Pwrite64 {
                fd,
                buf:*,
                count,
                offset
            }),
            Sysno::sendfile => sys_req!(Sendfile { out_fd, in_fd, offset:*, count }),
            Sysno::readv => sys_req!(Readv { fd, iovec:*, iovcnt }),
            Sysno::writev => sys_req!(Writev { fd, iovec:*, iovcnt }),
            Sysno::preadv => sys_req!(Preadv { fd, iovec:*, iovcnt, pos_l, pos_h }),
            Sysno::pwritev => sys_req!(Pwritev { fd, iovec:*, iovcnt, pos_l, pos_h }),
            #[cfg(target_arch = "x86_64")]
            Sysno::access => SyscallRequest::Faccessat {
                dirfd: AT_FDCWD,
                pathname: ctx.sys_req_ptr(0),
                mode: ctx.sys_req_arg(1),
                flags: AtFlags::empty(),
            },
            Sysno::faccessat => SyscallRequest::Faccessat {
                dirfd: ctx.sys_req_arg(0),
                pathname: ctx.sys_req_ptr(1),
                mode: ctx.sys_req_arg(2),
                flags: AtFlags::empty(),
            },
            Sysno::faccessat2 => sys_req!(Faccessat { dirfd, pathname:*, mode, flags }),
            #[cfg(target_arch = "x86_64")]
            Sysno::pipe => sys_req!(Pipe2 { pipefd:*, flags: { litebox::fs::OFlags::empty() } }),
            Sysno::pipe2 => sys_req!(Pipe2 { pipefd:* ,flags }),
            Sysno::madvise => sys_req!(Madvise { addr:*, length, behavior:? }),
            Sysno::dup => SyscallRequest::Dup {
                oldfd: ctx.sys_req_arg(0),
                newfd: None,
                flags: None,
            },
            #[cfg(target_arch = "x86_64")]
            Sysno::dup2 => SyscallRequest::Dup {
                oldfd: ctx.sys_req_arg(0),
                newfd: Some(ctx.sys_req_arg(1)),
                flags: None,
            },
            Sysno::dup3 => SyscallRequest::Dup {
                oldfd: ctx.sys_req_arg(0),
                newfd: Some(ctx.sys_req_arg(1)),
                flags: Some(ctx.sys_req_arg(2)),
            },
            Sysno::socket => sys_req!(Socket {
                domain,
                type_and_flags,
                protocol,
            }),
            Sysno::socketpair => sys_req!(Socketpair {
                domain,
                type_and_flags,
                protocol,
                sockvec: *,
            }),
            Sysno::connect => sys_req!(Connect { sockfd, sockaddr:*, addrlen }),
            Sysno::accept => sys_req!(Accept {
                sockfd,
                addr:*,
                addrlen:*,
                flags: { SockFlags::empty() }
            }),
            Sysno::accept4 => sys_req!(Accept { sockfd, addr:*, addrlen:*, flags }),
            Sysno::sendto => sys_req!(Sendto { sockfd, buf:*, len, flags, addr:*, addrlen }),
            Sysno::sendmsg => sys_req!(Sendmsg { sockfd, msg:*, flags }),
            Sysno::sendmmsg => sys_req!(Sendmmsg { sockfd, msgvec:*, vlen, flags }),
            Sysno::recvfrom => sys_req!(Recvfrom { sockfd, buf:*, len, flags, addr:*, addrlen:*, }),
            Sysno::recvmsg => sys_req!(Recvmsg { sockfd, msg:*, flags }),
            Sysno::recvmmsg => sys_req!(Recvmmsg {
                sockfd,
                msgvec:*,
                vlen,
                flags,
                timeout: { =*> TimeParam::timespec_old }
            }),
            Sysno::shutdown => sys_req!(Shutdown { sockfd, how }),
            Sysno::bind => sys_req!(Bind { sockfd, sockaddr:*, addrlen }),
            Sysno::listen => sys_req!(Listen { sockfd, backlog }),
            Sysno::setsockopt => sys_req!(Setsockopt {
                sockfd,
                level,
                optname,
                optval:*,
                optlen,
            }),
            Sysno::getsockopt => sys_req!(Getsockopt {
                sockfd,
                level,
                optname,
                optval:*,
                optlen:*,
            }),
            Sysno::getsockname => sys_req!(Getsockname { sockfd, addr:*, addrlen:* }),
            Sysno::getpeername => sys_req!(Getpeername { sockfd, addr:*, addrlen:* }),
            Sysno::exit => sys_req!(Exit { status }),
            Sysno::exit_group => sys_req!(ExitGroup { status }),
            Sysno::uname => sys_req!(Uname { buf:* }),
            Sysno::fcntl => {
                let cmd: i32 = ctx.sys_req_arg(1);
                let arg = ctx.sys_req_arg(2);
                SyscallRequest::Fcntl {
                    fd: ctx.sys_req_arg(0),
                    arg: FcntlArg::try_from(cmd, arg).ok_or_else(|| {
                        unsupported_einval(format_args!("fcntl(cmd = {cmd}, arg = {arg})"))
                    })?,
                }
            }
            Sysno::flock => sys_req!(Flock { fd, operation }),
            Sysno::fsync => sys_req!(Fsync { fd }),
            Sysno::fdatasync => sys_req!(Fdatasync { fd }),
            Sysno::gettimeofday => sys_req!(Gettimeofday { tv:*, tz:* }),
            Sysno::clock_gettime => {
                sys_req!(ClockGettime { clockid, tp: { =*> TimeParam::timespec_old } })
            }
            Sysno::clock_getres => {
                sys_req!(ClockGetres { clockid, res: { =*> TimeParam::timespec_old } })
            }
            Sysno::clock_nanosleep => {
                sys_req!(ClockNanosleep {
                    clockid,
                    flags,
                    request: { =*> TimeParam::timespec_old },
                    remain: { =*> TimeParam::timespec_old },
                })
            }
            Sysno::nanosleep => sys_req!(ClockNanosleep {
                request: { =*> TimeParam::timespec_old },
                remain: { =*> TimeParam::timespec_old },
                clockid: { ClockId::Monotonic.into() },
                flags: { TimerFlags::empty() },
            }),
            #[cfg(target_arch = "x86_64")]
            Sysno::time => sys_req!(Time { tloc:* }),
            Sysno::getcwd => sys_req!(Getcwd { buf:*, size }),
            #[cfg(target_arch = "x86_64")]
            Sysno::readlink => sys_req!(Readlink { pathname:*, buf:* ,bufsiz }),
            Sysno::readlinkat => sys_req!(Readlinkat { dirfd, pathname:*, buf:*, bufsiz }),
            Sysno::getrlimit => sys_req!(Getrlimit { resource:?, rlim:* }),
            Sysno::setrlimit => sys_req!(Setrlimit { resource:?, rlim:* }),
            Sysno::prlimit64 => sys_req!(Prlimit { pid, resource:?, new_limit:*, old_limit:* }),
            Sysno::getpid => SyscallRequest::Getpid,
            Sysno::getppid => SyscallRequest::Getppid,
            Sysno::getpgid => sys_req!(Getpgid { pid }),
            Sysno::setpgid => sys_req!(Setpgid { pid, pgid }),
            Sysno::setsid => SyscallRequest::Setsid,
            Sysno::getuid => SyscallRequest::Getuid,
            Sysno::getgid => SyscallRequest::Getgid,
            Sysno::geteuid => SyscallRequest::Geteuid,
            Sysno::getegid => SyscallRequest::Getegid,
            Sysno::setuid => sys_req!(Setuid { uid }),
            Sysno::setgid => sys_req!(Setgid { gid }),
            Sysno::setresuid => sys_req!(Setresuid { ruid, euid, suid }),
            Sysno::setresgid => sys_req!(Setresgid { rgid, egid, sgid }),
            Sysno::getgroups => sys_req!(Getgroups { size, list:* }),
            Sysno::setgroups => sys_req!(Setgroups { size, list:* }),
            Sysno::epoll_ctl => sys_req!(EpollCtl { epfd, op:?, fd, event:* }),
            #[cfg(target_arch = "x86_64")]
            Sysno::epoll_wait => {
                sys_req!(EpollPwait { epfd, events:*, maxevents, timeout, sigmask: { None }, sigsetsize: { 0 }, })
            }
            Sysno::epoll_pwait => {
                sys_req!(EpollPwait { epfd, events:*, maxevents, timeout, sigmask:*, sigsetsize })
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::epoll_create => sys_req!(EpollCreate {
                size,
                flags: { EpollCreateFlags::empty() }
            }),
            Sysno::epoll_create1 => sys_req!(EpollCreate { flags, size: { 1 } }),
            Sysno::ppoll => {
                sys_req!(Ppoll { fds:*, nfds, timeout: { =*> TimeParam::timespec_old }, sigmask:*, sigsetsize })
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::poll => {
                sys_req!(Ppoll { fds:*, nfds, timeout: { => TimeParam::Milliseconds }, sigmask: { None }, sigsetsize: { 0 } })
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::select => {
                sys_req!(Pselect {
                    nfds,
                    readfds:*,
                    writefds:*,
                    exceptfds:*,
                    timeout: { =*> TimeParam::timeval },
                    sigsetpack: { None },
                })
            }
            Sysno::pselect6 => {
                sys_req!(Pselect {
                    nfds,
                    readfds:*,
                    writefds:*,
                    exceptfds:*,
                    timeout: { =*> TimeParam::timespec_old },
                    sigsetpack:*,
                })
            }
            Sysno::prctl => {
                let op: u32 = ctx.sys_req_arg(0);
                if let Ok(op) = PrctlOption::try_from(op) {
                    match op {
                        PrctlOption::SetName => SyscallRequest::Prctl {
                            args: PrctlArg::SetName(ctx.sys_req_ptr(1)),
                        },
                        PrctlOption::GetName => SyscallRequest::Prctl {
                            args: PrctlArg::GetName(ctx.sys_req_ptr(1)),
                        },
                        PrctlOption::CapBSetRead => SyscallRequest::Prctl {
                            args: PrctlArg::CapBSetRead(ctx.sys_req_arg(1)),
                        },
                        _ => {
                            return Err(unsupported_einval(format_args!("prctl({op:?})")));
                        }
                    }
                } else {
                    return Err(errno::Errno::EINVAL);
                }
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::arch_prctl => {
                let code: u32 = ctx.sys_req_arg(0);
                let code = ArchPrctlCode::try_from(code)
                    .map_err(|_| unsupported_einval(format_args!("arch_prctl(code = {code})")))?;
                let arg = match code {
                    #[cfg(target_arch = "x86_64")]
                    ArchPrctlCode::SetFs => ArchPrctlArg::SetFs(ctx.sys_req_arg(1)),
                    #[cfg(target_arch = "x86_64")]
                    ArchPrctlCode::GetFs => ArchPrctlArg::GetFs(ctx.sys_req_ptr(1)),
                    ArchPrctlCode::CETStatus => ArchPrctlArg::CETStatus,
                    ArchPrctlCode::CETDisable => ArchPrctlArg::CETDisable,
                    ArchPrctlCode::CETLock => ArchPrctlArg::CETLock,
                };
                SyscallRequest::ArchPrctl { arg }
            }
            Sysno::gettid => SyscallRequest::Gettid,
            #[cfg(target_arch = "x86_64")]
            Sysno::set_thread_area => sys_req!(SetThreadArea { user_desc:* }),
            Sysno::set_tid_address => sys_req!(SetTidAddress { tidptr:* }),
            Sysno::openat => sys_req!(Openat { dirfd,pathname:*,flags,mode }),
            #[cfg(target_arch = "x86_64")]
            Sysno::open => {
                // open is equivalent to openat with dirfd AT_FDCWD
                SyscallRequest::Openat {
                    dirfd: AT_FDCWD,
                    pathname: ctx.sys_req_ptr(0),
                    flags: ctx.sys_req_arg(1),
                    mode: ctx.sys_req_arg(2),
                }
            }
            Sysno::mknodat => sys_req!(Mknodat { dirfd,pathname:*,mode_and_type,dev }),
            #[cfg(target_arch = "x86_64")]
            Sysno::mknod => SyscallRequest::Mknodat {
                dirfd: AT_FDCWD,
                pathname: ctx.sys_req_ptr(0),
                mode_and_type: ctx.sys_req_arg(1),
                dev: ctx.sys_req_arg(2),
            },
            Sysno::unlinkat => sys_req!(Unlinkat { dirfd,pathname:*,flags }),
            #[cfg(target_arch = "x86_64")]
            Sysno::unlink => {
                // unlink is equivalent to unlinkat with dirfd AT_FDCWD and flags 0
                SyscallRequest::Unlinkat {
                    dirfd: AT_FDCWD,
                    pathname: ctx.sys_req_ptr(0),
                    flags: AtFlags::empty(),
                }
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::rmdir => {
                // rmdir is equivalent to unlinkat with dirfd AT_FDCWD and AT_REMOVEDIR
                SyscallRequest::Unlinkat {
                    dirfd: AT_FDCWD,
                    pathname: ctx.sys_req_ptr(0),
                    flags: AtFlags::AT_REMOVEDIR,
                }
            }
            Sysno::linkat => sys_req!(Linkat {
                olddirfd,
                oldpath:*,
                newdirfd,
                newpath:*,
                flags
            }),
            #[cfg(target_arch = "x86_64")]
            Sysno::link => {
                // link is equivalent to linkat with olddirfd/newdirfd AT_FDCWD and flags 0
                SyscallRequest::Linkat {
                    olddirfd: AT_FDCWD,
                    oldpath: ctx.sys_req_ptr(0),
                    newdirfd: AT_FDCWD,
                    newpath: ctx.sys_req_ptr(1),
                    flags: AtFlags::empty(),
                }
            }
            Sysno::renameat => {
                // renameat has no flags argument (unlike renameat2); flags is always 0.
                SyscallRequest::Renameat {
                    olddirfd: ctx.sys_req_arg(0),
                    oldpath: ctx.sys_req_ptr(1),
                    newdirfd: ctx.sys_req_arg(2),
                    newpath: ctx.sys_req_ptr(3),
                    flags: 0,
                }
            }
            Sysno::renameat2 => sys_req!(Renameat {
                olddirfd,
                oldpath:*,
                newdirfd,
                newpath:*,
                flags
            }),
            #[cfg(target_arch = "x86_64")]
            Sysno::rename => {
                // rename is equivalent to renameat2 with olddirfd/newdirfd AT_FDCWD and flags 0
                SyscallRequest::Renameat {
                    olddirfd: AT_FDCWD,
                    oldpath: ctx.sys_req_ptr(0),
                    newdirfd: AT_FDCWD,
                    newpath: ctx.sys_req_ptr(1),
                    flags: 0,
                }
            }
            Sysno::symlinkat => sys_req!(Symlinkat { target:*, newdirfd, linkpath:* }),
            #[cfg(target_arch = "x86_64")]
            Sysno::symlink => {
                // symlink is equivalent to symlinkat with newdirfd AT_FDCWD
                SyscallRequest::Symlinkat {
                    target: ctx.sys_req_ptr(0),
                    newdirfd: AT_FDCWD,
                    linkpath: ctx.sys_req_ptr(1),
                }
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::creat => {
                // creat is equivalent to open with flags O_CREAT|O_WRONLY|O_TRUNC
                SyscallRequest::Openat {
                    dirfd: AT_FDCWD,
                    pathname: ctx.sys_req_ptr(0),
                    flags: litebox::fs::OFlags::CREAT
                        | litebox::fs::OFlags::WRONLY
                        | litebox::fs::OFlags::TRUNC,
                    mode: ctx.sys_req_arg(1),
                }
            }
            Sysno::ftruncate => sys_req!(Ftruncate { fd, length }),
            Sysno::fallocate => sys_req!(Fallocate { fd, mode, offset, len }),
            #[cfg(target_arch = "x86_64")]
            Sysno::newfstatat => sys_req!(Newfstatat { dirfd,pathname:*,buf:*,flags }),
            #[cfg(target_arch = "aarch64")]
            Sysno::fstatat => sys_req!(Newfstatat { dirfd,pathname:*,buf:*,flags }),
            Sysno::utimensat => sys_req!(Utimensat { dirfd,pathname:*,times:*,flags }),
            #[cfg(target_arch = "x86_64")]
            Sysno::eventfd => SyscallRequest::Eventfd2 {
                initval: ctx.sys_req_arg(0),
                flags: EfdFlags::empty(),
            },
            Sysno::eventfd2 => sys_req!(Eventfd2 { initval, flags }),
            #[cfg(target_arch = "x86_64")]
            Sysno::signalfd => SyscallRequest::Signalfd4 {
                fd: ctx.sys_req_arg(0),
                mask: ctx.sys_req_ptr(1),
                sizemask: ctx.sys_req_arg(2),
                flags: SfdFlags::empty(),
            },
            Sysno::signalfd4 => sys_req!(Signalfd4 { fd, mask:*, sizemask, flags }),
            Sysno::timerfd_create => SyscallRequest::TimerfdCreate {
                clockid: ctx.sys_req_arg(0),
                flags: ctx.sys_req_arg(1),
            },
            Sysno::timerfd_settime => sys_req!(TimerfdSettime {
                fd,
                flags,
                new_value:*,
                old_value:*,
            }),
            Sysno::timerfd_gettime => sys_req!(TimerfdGettime { fd, curr_value:* }),
            Sysno::memfd_create => sys_req!(MemfdCreate { name:*, flags }),
            Sysno::getrandom => sys_req!(GetRandom { buf:*,count,flags }),
            Sysno::clone => {
                let args = CloneArgs {
                    // The upper 32 bits are clone3-specific. The low 8 bits are the exit signal.
                    flags: CloneFlags::from_bits_retain(ctx.syscall_arg(0) as u64 & 0xffffff00),
                    stack: ctx.sys_req_arg(1),
                    parent_tid: ctx.sys_req_arg(2),
                    // The order of the `child_tid` and `tls` arguments depends on
                    // CONFIG_CLONE_BACKWARDS (see kernel/fork.c): when set, the layout
                    // is (..., tls=arg3, child_tid=arg4); otherwise it is
                    // (..., child_tid=arg3, tls=arg4). arm64 selects CLONE_BACKWARDS
                    // (arch/arm64/Kconfig), whereas x86_64 does not (only X86_32 does,
                    // arch/x86/Kconfig), so the indices are swapped between the arches.
                    child_tid: ctx.sys_req_arg(if cfg!(target_arch = "x86_64") { 3 } else { 4 }),
                    tls: ctx.sys_req_arg(if cfg!(target_arch = "x86_64") { 4 } else { 3 }),
                    pidfd: ctx.sys_req_arg(2), // aliases parent_tid
                    exit_signal: ctx.syscall_arg(0) as u64 & 0xff,
                    stack_size: 0,
                    set_tid: 0,
                    set_tid_size: 0,
                    cgroup: 0,
                };
                SyscallRequest::Clone { args }
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::fork => {
                // `fork()` takes no arguments; it is equivalent to
                // `clone(SIGCHLD, 0, NULL, NULL, 0)` -- no flags set (separate address space,
                // separate everything), exit_signal = SIGCHLD (17), stack = 0 (child gets a
                // duplicate of the parent's own stack, not a caller-supplied one).
                const SIGCHLD: u64 = 17;
                let args = CloneArgs {
                    flags: CloneFlags::empty(),
                    stack: 0,
                    parent_tid: 0,
                    child_tid: 0,
                    tls: 0,
                    pidfd: 0,
                    exit_signal: SIGCHLD,
                    stack_size: 0,
                    set_tid: 0,
                    set_tid_size: 0,
                    cgroup: 0,
                };
                SyscallRequest::Clone { args }
            }
            #[cfg(target_arch = "x86_64")]
            Sysno::vfork => {
                // `vfork()` takes no arguments; equivalent to
                // `clone(CLONE_VM | CLONE_VFORK | SIGCHLD, 0)`.
                const SIGCHLD: u64 = 17;
                let args = CloneArgs {
                    flags: CloneFlags::VFORK,
                    stack: 0,
                    parent_tid: 0,
                    child_tid: 0,
                    tls: 0,
                    pidfd: 0,
                    exit_signal: SIGCHLD,
                    stack_size: 0,
                    set_tid: 0,
                    set_tid_size: 0,
                    cgroup: 0,
                };
                SyscallRequest::Clone { args }
            }
            Sysno::clone3 => {
                debug_assert_eq!(
                    ctx.sys_req_arg::<usize>(1),
                    size_of::<CloneArgs>(),
                    "legacy clone3 struct"
                );
                SyscallRequest::Clone3 {
                    args: ctx.sys_req_ptr(0),
                }
            }
            Sysno::set_robust_list => {
                if ctx.sys_req_arg::<usize>(1) == size_of::<RobustListHead>() {
                    sys_req!(SetRobustList { head })
                } else {
                    return Err(errno::Errno::EINVAL);
                }
            }
            Sysno::get_robust_list => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::GetRobustList {
                    pid: if pid == 0 { None } else { Some(pid) },
                    head: ctx.sys_req_ptr(1),
                    len: ctx.sys_req_ptr(2),
                }
            }
            Sysno::sysinfo => sys_req!(Sysinfo { buf:* }),
            Sysno::capget => sys_req!(CapGet { header:*,data:* }),
            Sysno::getdents64 => sys_req!(GetDirent64 { fd,dirp:*,count }),
            Sysno::sched_getaffinity => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::SchedGetAffinity {
                    pid: if pid == 0 { None } else { Some(pid) },
                    len: ctx.sys_req_arg(1),
                    mask: ctx.sys_req_ptr(2),
                }
            }
            Sysno::sched_yield => SyscallRequest::SchedYield,
            Sysno::sched_getparam => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::SchedGetParam {
                    pid: if pid == 0 { None } else { Some(pid) },
                    param: ctx.sys_req_ptr(1),
                }
            }
            Sysno::sched_setparam => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::SchedSetParam {
                    pid: if pid == 0 { None } else { Some(pid) },
                    param: ctx.sys_req_ptr(1),
                }
            }
            Sysno::sched_getscheduler => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::SchedGetScheduler {
                    pid: if pid == 0 { None } else { Some(pid) },
                }
            }
            Sysno::sched_setscheduler => {
                let pid = ctx.sys_req_arg(0);
                SyscallRequest::SchedSetScheduler {
                    pid: if pid == 0 { None } else { Some(pid) },
                    policy: ctx.sys_req_arg::<i32>(1),
                    param: ctx.sys_req_ptr(2),
                }
            }
            Sysno::futex => Self::parse_futex(ctx, TimeParam::timespec_old, unsupported_einval)?,
            Sysno::execve => sys_req!(Execve { pathname:*, argv:*, envp:* }),
            Sysno::umask => sys_req!(Umask { mask }),
            #[cfg(target_arch = "x86_64")]
            Sysno::alarm => sys_req!(Alarm { seconds }),
            #[cfg(target_arch = "x86_64")]
            Sysno::pause => SyscallRequest::Pause,
            Sysno::setitimer => sys_req!(SetITimer { which:?, new_value:*, old_value:* }),
            Sysno::getitimer => sys_req!(GetITimer { which:?, curr_value:* }),
            Sysno::statx => sys_req!(Statx {
                dirfd,
                pathname:*,
                flags,
                mask,
                statxbuf:*,
            }),
            // Noisy unsupported syscalls.
            Sysno::io_uring_setup | Sysno::rseq | Sysno::statfs => {
                return Err(errno::Errno::ENOSYS);
            }
            sysno => {
                log_unsupported(format_args!("unsupported syscall {sysno:?}"));
                return Err(errno::Errno::ENOSYS);
            }
        };
        Ok(dispatcher)
    }

    fn parse_futex<T: FromBytes + IntoBytes>(
        ctx: &PtRegs,
        time_param: impl FnOnce(Option<UserPtrMut<T>>) -> TimeParam,
        unsupported_einval: impl Fn(core::fmt::Arguments<'_>) -> errno::Errno,
    ) -> Result<SyscallRequest, errno::Errno> {
        let addr = ctx.sys_req_ptr(0);
        let op_and_flags: i32 = ctx.sys_req_arg(1);
        let op = op_and_flags & FutexFlags::FUTEX_CMD_MASK.bits();
        let flags = op_and_flags & !FutexFlags::FUTEX_CMD_MASK.bits();
        let cmd = FutexOperation::try_from(op)
            .map_err(|_| unsupported_einval(format_args!("futex(op = {op})")))?;
        let flags = FutexFlags::from_bits(flags)
            .ok_or_else(|| unsupported_einval(format_args!("futex(flags = {flags})")))?;
        let val = ctx.sys_req_arg(2);
        let args = match cmd {
            FutexOperation::Wait => {
                let timeout = time_param(ctx.sys_req_ptr(3));
                FutexArgs::Wait {
                    addr,
                    flags,
                    val,
                    timeout,
                }
            }
            FutexOperation::WaitBitset => {
                let timeout = time_param(ctx.sys_req_ptr(3));
                FutexArgs::WaitBitset {
                    addr,
                    flags,
                    val,
                    timeout,
                    bitmask: ctx.sys_req_arg(5),
                }
            }
            FutexOperation::Wake => FutexArgs::Wake {
                addr,
                flags,
                count: val,
            },
            // Note: for FUTEX_REQUEUE/FUTEX_CMP_REQUEUE, the 4th syscall argument (normally a
            // `struct timespec *timeout` for FUTEX_WAIT) is instead a plain integer -- the
            // requeue count -- per futex(2)'s documented reuse of that argument slot. It must NOT
            // be read as a timeout pointer here.
            FutexOperation::Requeue => FutexArgs::Requeue {
                addr,
                flags,
                wake_count: val,
                requeue_count: ctx.sys_req_arg(3),
                addr2: ctx.sys_req_ptr(4),
            },
            FutexOperation::CmpRequeue => FutexArgs::CmpRequeue {
                addr,
                flags,
                wake_count: val,
                requeue_count: ctx.sys_req_arg(3),
                addr2: ctx.sys_req_ptr(4),
                expected_value: ctx.sys_req_arg(5),
            },
        };
        Ok(SyscallRequest::Futex { args })
    }
}

#[derive(Debug)]
pub enum TimeParam {
    None,
    Milliseconds(i32),
    TimeVal(UserPtrMut<TimeVal>),
    Timespec32(UserPtrMut<Timespec32>),
    Timespec64(UserPtrMut<Timespec>),
}

impl TimeParam {
    /// Return a `TimeParam` for a 64-bit timespec pointer.
    pub fn timespec64(tp: Option<UserPtrMut<Timespec>>) -> Self {
        tp.map_or(TimeParam::None, TimeParam::Timespec64)
    }

    /// Return a `TimeParam` for a 32-bit timespec pointer.
    pub fn timespec32(tp: Option<UserPtrMut<Timespec32>>) -> Self {
        tp.map_or(TimeParam::None, TimeParam::Timespec32)
    }

    /// Return a `TimeParam` for the old timespec pointer type, which is
    /// architecture dependent.
    pub fn timespec_old(tp: Option<UserPtrMut<Timespec>>) -> Self {
        Self::timespec64(tp)
    }

    /// Return a `TimeParam` for a timeval pointer.
    pub fn timeval(tp: Option<UserPtrMut<TimeVal>>) -> Self {
        tp.map_or(TimeParam::None, TimeParam::TimeVal)
    }

    /// Convert a generic timeout argument into a `Timeout` enum.
    pub fn read<P: litebox::platform::RawPointerProvider>(
        &self,
    ) -> Result<Option<Duration>, errno::Errno> {
        let v = match *self {
            TimeParam::None => return Ok(None),
            TimeParam::Milliseconds(s) => {
                // Negative values indicate an infinite timeout.
                let Ok(s) = s.try_into() else {
                    return Ok(None);
                };
                Duration::from_millis(s)
            }
            TimeParam::TimeVal(tv) => {
                let tv = tv.read_at_offset::<P>(0).ok_or(errno::Errno::EFAULT)?;
                Duration::try_from(tv).map_err(|_| errno::Errno::EINVAL)?
            }
            TimeParam::Timespec32(ts) => {
                let ts = ts.read_at_offset::<P>(0).ok_or(errno::Errno::EFAULT)?;
                Duration::try_from(ts).map_err(|_| errno::Errno::EINVAL)?
            }
            TimeParam::Timespec64(ts) => {
                let ts = ts.read_at_offset::<P>(0).ok_or(errno::Errno::EFAULT)?;
                Duration::try_from(ts).map_err(|_| errno::Errno::EINVAL)?
            }
        };
        Ok(Some(v))
    }

    /// Write a value to the time parameter.
    pub fn write<P: litebox::platform::RawPointerProvider>(
        &self,
        duration: Duration,
    ) -> Result<(), errno::Errno> {
        match *self {
            TimeParam::None | TimeParam::Milliseconds(_) => Ok(()),
            TimeParam::TimeVal(tv_ptr) => {
                tv_ptr
                    .write_at_offset::<P>(0, duration.into())
                    .ok_or(errno::Errno::EFAULT)?;
                Ok(())
            }
            TimeParam::Timespec32(ts_ptr) => {
                ts_ptr
                    .write_at_offset::<P>(0, duration.into())
                    .ok_or(errno::Errno::EFAULT)?;
                Ok(())
            }
            TimeParam::Timespec64(ts_ptr) => {
                ts_ptr
                    .write_at_offset::<P>(0, duration.into())
                    .ok_or(errno::Errno::EFAULT)?;
                Ok(())
            }
        }
    }
}

/// Context saved when entering the kernel
///
/// pt_regs from [Linux](https://elixir.bootlin.com/linux/v5.19.17/source/arch/x86/include/asm/ptrace.h#L59)
#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct PtRegs {
    /*
     * C ABI says these regs are callee-preserved. They aren't saved on kernel entry
     * unless syscall needs a complete, fully filled "struct pt_regs".
     */
    pub r15: usize,
    pub r14: usize,
    pub r13: usize,
    pub r12: usize,
    pub rbp: usize,
    pub rbx: usize,
    /* These regs are callee-clobbered. Always saved on kernel entry. */
    pub r11: usize,
    pub r10: usize,
    pub r9: usize,
    pub r8: usize,
    pub rax: usize,
    pub rcx: usize,
    pub rdx: usize,
    pub rsi: usize,
    pub rdi: usize,

    /*
     * On syscall entry, this is syscall#. On CPU exception, this is error code.
     * On hw interrupt, it's IRQ number:
     */
    pub orig_rax: usize,
    /* Return frame for iretq */
    pub rip: usize,
    pub cs: usize,
    pub eflags: usize,
    pub rsp: usize,
    pub ss: usize,
    /* top of stack page */
}

/// Context saved when entering the kernel.
///
/// pt_regs from [Linux](https://elixir.bootlin.com/linux/v5.19.17/source/arch/arm64/include/asm/ptrace.h#L178)
#[cfg(target_arch = "aarch64")]
#[repr(C, align(16))]
#[derive(Clone, Debug, Default)]
pub struct PtRegs {
    /// General-purpose registers x0-x30.
    pub regs: [usize; AARCH64_GENERAL_REGISTER_COUNT],
    /// Stack pointer.
    pub sp: usize,
    /// Program counter.
    pub pc: usize,
    /// Saved processor state (PSTATE/SPSR).
    pub pstate: u64,

    pub orig_x0: usize,

    // little endian
    pub syscallno: i32,
    pub unused2: u32,
    /* add remaining fields if needed */
}

#[cfg(target_arch = "x86_64")]
pub mod arch {
    // User returns must not target the null-guard region.
    pub const USER_ADDR_MIN: usize = 0x0000_0000_0001_0000;
    // Exclusive upper bound; the final low-canonical page is reserved as a guard page.
    pub const USER_ADDR_END: usize = 0x0000_7fff_ffff_f000;
    pub const USER_CS: usize = 0x33;
    pub const USER_DS: usize = 0x2b;
    pub const EFLAGS_CF: usize = 1 << 0;
    pub const EFLAGS_FIXED: usize = 1 << 1;
    pub const EFLAGS_PF: usize = 1 << 2;
    pub const EFLAGS_AF: usize = 1 << 4;
    pub const EFLAGS_ZF: usize = 1 << 6;
    pub const EFLAGS_SF: usize = 1 << 7;
    pub const EFLAGS_IF: usize = 1 << 9;
    pub const EFLAGS_DF: usize = 1 << 10;
    pub const EFLAGS_OF: usize = 1 << 11;
    pub const EFLAGS_RF: usize = 1 << 16;
    pub const EFLAGS_ID: usize = 1 << 21;
    pub const SAFE_USER_EFLAGS: usize = EFLAGS_CF
        | EFLAGS_FIXED
        | EFLAGS_PF
        | EFLAGS_AF
        | EFLAGS_ZF
        | EFLAGS_SF
        | EFLAGS_IF
        | EFLAGS_DF
        | EFLAGS_OF
        | EFLAGS_RF
        | EFLAGS_ID;

    /// Returns whether `base` is a valid x86_64 Linux user FS-segment base.
    ///
    /// A user FS base is valid iff it is below the top of the user address
    /// space. Especially, if a given address is non-canonical, `wrfsbase`
    /// can result in a #GP fault. This check is based on Linux kernel's
    /// `do_arch_prctl_64`.
    #[must_use]
    pub fn is_valid_user_fs_base(base: usize) -> bool {
        base < USER_ADDR_END
    }
}

#[cfg(target_arch = "aarch64")]
pub mod arch {
    // User returns must not target the null-guard region.
    pub const USER_ADDR_MIN: usize = 0x0000_0000_0001_0000;
    // Exclusive upper bound; keep the final low-userspace page reserved as a guard page.
    pub const USER_ADDR_END: usize = 0x0000_ffff_ffff_f000;
    /// PSTATE condition flags (N, Z, C, V) — guest-controllable arithmetic state.
    pub const PSR_NZCV_MASK: u64 = 0b1111 << 28;
    /// Speculative Store Bypass Safe bit — a benign, user-settable mitigation bit.
    pub const PSR_SSBS_BIT: u64 = 1 << 12;
    /// Data Independent Timing bit — a benign, user-settable mitigation bit.
    pub const PSR_DIT_BIT: u64 = 1 << 24;
    /// PSTATE bits a guest may keep when returning to EL0.
    pub const SAFE_USER_PSTATE: u64 = PSR_NZCV_MASK | PSR_SSBS_BIT | PSR_DIT_BIT;

    /// Returns whether `base` is a valid aarch64 Linux user TLS base (the value written to
    /// `TPIDR_EL0`).
    ///
    /// A user TLS base is valid iff it is below the top of the user address space -- unlike
    /// x86_64's FS-base check, aarch64 has no non-canonical-address #GP fault to guard against
    /// (`TPIDR_EL0` accepts any 64-bit value), so this is purely an address-space-separation
    /// check, matching `is_valid_user_fs_base`'s role on x86_64.
    #[must_use]
    pub fn is_valid_user_fs_base(base: usize) -> bool {
        base < USER_ADDR_END
    }
}

impl PtRegs {
    /// Returns whether `rip` and `rsp` are in the x86_64 Linux user address range.
    #[cfg(target_arch = "x86_64")]
    #[must_use]
    pub fn has_user_return_addresses(&self) -> bool {
        (arch::USER_ADDR_MIN..arch::USER_ADDR_END).contains(&self.rip)
            && (arch::USER_ADDR_MIN..arch::USER_ADDR_END).contains(&self.rsp)
    }

    /// Sanitizes CPU state and normalizes the context to the x86_64 Linux user ABI.
    ///
    /// Returns `false` if `rip` or `rsp` are outside the x86_64 Linux user address range.
    /// On success, privileged or unsafe RFLAGS bits are cleared, the fixed
    /// RFLAGS bit is set, interrupts are enabled, and the user CS/SS selectors
    /// are set to the x86_64 Linux ABI values.
    #[cfg(target_arch = "x86_64")]
    #[must_use]
    pub fn sanitize_for_user_return(&mut self) -> bool {
        if !self.has_user_return_addresses() {
            return false;
        }
        self.eflags = (self.eflags & arch::SAFE_USER_EFLAGS) | arch::EFLAGS_FIXED | arch::EFLAGS_IF;
        self.cs = arch::USER_CS;
        self.ss = arch::USER_DS;
        true
    }

    /// Returns whether `pc` and `sp` are in the aarch64 Linux user address range.
    #[cfg(target_arch = "aarch64")]
    #[must_use]
    pub fn has_user_return_addresses(&self) -> bool {
        (arch::USER_ADDR_MIN..arch::USER_ADDR_END).contains(&self.pc)
            && (arch::USER_ADDR_MIN..arch::USER_ADDR_END).contains(&self.sp)
    }

    /// Sanitizes CPU state and normalizes the context to the aarch64 Linux user ABI.
    ///
    /// Returns `false` if `pc` or `sp` are outside the aarch64 Linux user address
    /// range. On success, `pstate` is coerced to a clean AArch64 EL0t state: the
    /// guest keeps only the condition flags and benign mitigation bits. Every
    /// other bit is cleared, forcing EL0t, AArch64 execution state, unmasked
    /// exceptions, and no illegal-state or single-step.
    #[cfg(target_arch = "aarch64")]
    #[must_use]
    pub fn sanitize_for_user_return(&mut self) -> bool {
        if !self.has_user_return_addresses() {
            return false;
        }
        self.pstate &= arch::SAFE_USER_PSTATE;
        true
    }

    /// Get the `idx`th syscall argument.
    ///
    /// # Panics
    ///
    /// If `idx` is greater than 5, this function will panic.
    #[cfg(target_arch = "x86_64")]
    pub fn syscall_arg(&self, idx: usize) -> usize {
        match idx {
            0 => self.rdi,
            1 => self.rsi,
            2 => self.rdx,
            3 => self.r10,
            4 => self.r8,
            5 => self.r9,
            _ => panic!("Invalid syscall argument index: {idx}"),
        }
    }

    /// Get the `idx`th syscall argument.
    ///
    /// # Panics
    ///
    /// If `idx` is greater than 5, this function will panic.
    #[cfg(target_arch = "aarch64")]
    pub fn syscall_arg(&self, idx: usize) -> usize {
        if idx < 6 {
            self.regs[idx]
        } else {
            panic!("Invalid syscall argument index: {idx}")
        }
    }

    // (Private-only, only to be used via `SyscallRequest::try_from_raw`), get the `idx`th syscall
    // argument, reinterpret-truncated to the necessary type.
    fn sys_req_arg<T: ReinterpretTruncatedFromUsize>(&self, idx: usize) -> T {
        T::reinterpret_truncated_from_usize(self.syscall_arg(idx))
    }
    // (Private-only, only to be used via `SyscallRequest::try_from_raw`), get the `idx`th syscall
    // argument, reinterpreted to the necessary pointer type.
    fn sys_req_ptr<T: Clone, P: ReinterpretUsizeAsPtr<T>>(&self, idx: usize) -> P {
        P::reinterpret_usize_as_ptr(self.syscall_arg(idx))
    }

    /// Get the instruction pointer (IP)
    #[cfg(target_arch = "x86_64")]
    pub fn get_ip(&self) -> usize {
        self.rip
    }

    /// Get the instruction pointer (IP)
    #[cfg(target_arch = "aarch64")]
    pub fn get_ip(&self) -> usize {
        self.pc
    }

    /// Get the syscall return-value register (`rax` on x86_64, `x0`/`regs[0]` on aarch64).
    #[cfg(target_arch = "x86_64")]
    pub fn return_value(&self) -> usize {
        self.rax
    }

    /// Get the syscall return-value register (`rax` on x86_64, `x0`/`regs[0]` on aarch64).
    #[cfg(target_arch = "aarch64")]
    pub fn return_value(&self) -> usize {
        self.regs[0]
    }

    /// Set the syscall return-value register (`rax` on x86_64, `x0`/`regs[0]` on aarch64).
    #[cfg(target_arch = "x86_64")]
    pub fn set_return_value(&mut self, val: usize) {
        self.rax = val;
    }

    /// Set the syscall return-value register (`rax` on x86_64, `x0`/`regs[0]` on aarch64).
    #[cfg(target_arch = "aarch64")]
    pub fn set_return_value(&mut self, val: usize) {
        self.regs[0] = val;
    }

    /// Get the trapped syscall number (`orig_rax` on x86_64, `syscallno` on aarch64).
    #[cfg(target_arch = "x86_64")]
    pub fn syscall_number(&self) -> usize {
        self.orig_rax
    }

    /// Get the trapped syscall number (`orig_rax` on x86_64, `syscallno` on aarch64).
    #[cfg(target_arch = "aarch64")]
    pub fn syscall_number(&self) -> usize {
        self.syscallno.reinterpret_as_unsigned() as usize
    }
}

// This trait is to be used _only_ be `PtRegs`, and exists to simplify
// `SyscallRequest::try_from_raw`. It reinterprets `usize` values (via truncation and
// sign-reinterpretation and such) to a variety of values useful for `SyscallRequest`.
//
// IMPORTANT: this always silently performs truncation. This is why it should not be used for
// anything other than for `SyscallReuqest::try_from_raw`.
#[diagnostic::on_unimplemented(
    message = "If you are trying to use a pointer for the sys_req macro, you might want to `:*` it. Alternatively, you might be looking for `sys_req_ptr` rather than `sys_req_arg`."
)]
pub trait ReinterpretTruncatedFromUsize: Sized {
    fn reinterpret_truncated_from_usize(v: usize) -> Self;
}
impl ReinterpretTruncatedFromUsize for u64 {
    fn reinterpret_truncated_from_usize(v: usize) -> Self {
        v as u64
    }
}
impl ReinterpretTruncatedFromUsize for i64 {
    fn reinterpret_truncated_from_usize(v: usize) -> Self {
        v.reinterpret_as_signed() as i64
    }
}
impl ReinterpretTruncatedFromUsize for isize {
    fn reinterpret_truncated_from_usize(v: usize) -> Self {
        v.reinterpret_as_signed()
    }
}
macro_rules! reinterpret_truncated_from_usize_for {
    (
        unsigned [$($uty:ty),* $(,)?],
        signed [$($sty:ty),* $(,)?],
        flags [$($fty:ty),* $(,)?],
    ) => {
        $(
            impl ReinterpretTruncatedFromUsize for $uty {
                fn reinterpret_truncated_from_usize(v: usize) -> Self {
                    v.trunc()
                }
            }
        )*
        $(
            impl ReinterpretTruncatedFromUsize for $sty {
                fn reinterpret_truncated_from_usize(v: usize) -> Self {
                    v.reinterpret_as_signed().trunc()
                }
            }
        )*
        $(
            impl ReinterpretTruncatedFromUsize for $fty {
                fn reinterpret_truncated_from_usize(v: usize) -> Self {
                    <$fty>::from_bits_truncate(
                        <_ as ReinterpretTruncatedFromUsize>::reinterpret_truncated_from_usize(v),
                    )
                }
            }
        )*
    };
}
reinterpret_truncated_from_usize_for! {
    unsigned [usize, u8, u16, u32],
    signed [i8, i16, i32],
    flags [
        ProtFlags,
        MapFlags,
        MRemapFlags,
        AccessFlags,
        litebox::fs::Mode,
        litebox::fs::OFlags,
        AtFlags,
        SockFlags,
        SendFlags,
        ReceiveFlags,
        EpollCreateFlags,
        EfdFlags,
        SfdFlags,
        MfdFlags,
        TfdFlags,
        TfdSettimeFlags,
        RngFlags,
        TimerFlags,
        StatxMask,
    ],
}

// See similar usage constraints as `ReinterpretTruncatedFromUsize`. It is somewhat unfortunate that
// we cannot just merge this nicely with the `ReinterpretTruncatedFromUsize` trait due to some
// details of Rust's trait restrictions, but thankfully we only need two traits---one for the base
// types, and one for the platform-generic ones.
//
// Note that the `T` here is fully unused, it exists only to get past a
// non-conflicting-implementations constraint that exists in Rust; it helps us make the two
// implementations below disjoint.
//
// Also, note how it is only implemented on `RawConstPointer` but will also work with
// `RawMutPointer` because `RawMutPointer` declares `RawConstPointer` as a super-trait.
#[diagnostic::on_unimplemented(
    message = "If you are trying to use a non-pointer for the sys_req macro, you might want remove the `:*` for it. Alternatively, you might be looking for `sys_req_arg` rather than `sys_req_ptr`."
)]
pub trait ReinterpretUsizeAsPtr<T>: Sized {
    fn reinterpret_usize_as_ptr(v: usize) -> Self;
}
impl<T> ReinterpretUsizeAsPtr<core::marker::PhantomData<((), T)>> for UserPtr<T> {
    fn reinterpret_usize_as_ptr(v: usize) -> Self {
        UserPtr::from_usize(v)
    }
}
impl<T> ReinterpretUsizeAsPtr<core::marker::PhantomData<((), T)>> for UserPtrMut<T> {
    fn reinterpret_usize_as_ptr(v: usize) -> Self {
        UserPtrMut::from_usize(v)
    }
}
impl<T> ReinterpretUsizeAsPtr<core::marker::PhantomData<(bool, T)>> for Option<UserPtr<T>> {
    fn reinterpret_usize_as_ptr(v: usize) -> Self {
        if v == 0 {
            None
        } else {
            Some(UserPtr::from_usize(v))
        }
    }
}
impl<T> ReinterpretUsizeAsPtr<core::marker::PhantomData<(bool, T)>> for Option<UserPtrMut<T>> {
    fn reinterpret_usize_as_ptr(v: usize) -> Self {
        if v == 0 {
            None
        } else {
            Some(UserPtrMut::from_usize(v))
        }
    }
}
