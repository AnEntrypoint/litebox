// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Synthesized `/proc` entries [`super::backend::Backend`]s.
//!
//! A stock XFCE/GLib/dbus desktop session (and many ordinary CLI tools) read a small, fixed set
//! of `/proc` paths for introspection or sizing purposes -- none of them need real process
//! introspection to be individually CORRECT, only a format-accurate synthesis. This is
//! deliberately NOT a general-purpose `/proc` filesystem (see `Procfs`/`ProcSelf`'s own doc
//! comments for the exact, fixed set each one covers) -- mirrors the same "minimal, exact files a
//! real client needs" pattern already used by [`super::devices::ProcSysKernel`] for
//! `/proc/sys/kernel/{overflowuid,overflowgid}`.
//!
//! Two backends, mounted separately (like `/dev` + `/dev/dri` in [`super::devices`]):
//! - [`Procfs`], mounted at `/proc`: static/host-derived flat files (`cpuinfo`, `meminfo`,
//!   `mounts`, `uptime`) that need no per-process state.
//! - [`ProcSelf`], mounted at `/proc/self`: files whose content depends on the CURRENT guest
//!   process (`exe`, `cmdline`, `stat`, `status`, `environ`, `mountinfo`) -- backed by a shared
//!   [`ProcSelfInfo`] cell the shim updates on every `execve` (see `ProcSelf::update`).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::LiteBox;
use crate::sync::{RawSyncPrimitivesProvider, RwLock};

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

/// Per-process data backing `/proc/self/*`, updated on every `execve` (see [`ProcSelf::update`]).
///
/// Deliberately minimal: only the fields the synthesized files below actually surface.
#[derive(Clone, Default)]
pub struct ProcSelfInfo {
    /// The currently-executing guest binary's own resolved path (real Linux's
    /// `readlink("/proc/self/exe")` target).
    pub exe_path: String,
    /// `argv`, NUL-separated, matching real Linux `/proc/self/cmdline` format exactly (including
    /// the trailing NUL after the last argument).
    pub cmdline: Vec<u8>,
    /// The process's environment, NUL-separated, matching real Linux `/proc/self/environ` format.
    pub environ: Vec<u8>,
    /// Process ID, for the `stat`/`status` `pid` fields.
    pub pid: i32,
    /// Command name (`argv[0]`'s basename, truncated to 15 bytes on real Linux), for `stat`'s
    /// `comm` field and `status`'s `Name:` field.
    pub comm: String,
}

/// Real `/proc/[pid]/stat` (see `man 5 proc`) has 52 whitespace-separated fields as of Linux
/// 5.x; tools that parse it (e.g. `libgtop`, some process-introspection dbus services) generally
/// only rely on the field COUNT and the position of `pid`/`comm`/`state`, tolerating conservative
/// placeholder values for the rest. `comm` is always parenthesized (fields after it are found by
/// splitting on the LAST `)`, since `comm` itself may contain spaces/parens) -- this synthesis
/// follows that exactly.
fn format_stat(info: &ProcSelfInfo) -> Vec<u8> {
    // field 3 is state; 'R' (running) is always accurate enough for a process that is alive to
    // read its own /proc/self/stat.
    let mut s = format!("{} ({}) R", info.pid, info.comm);
    // Fields 4-52 (ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt, majflt, cmajflt,
    // utime, stime, cutime, cstime, priority, nice, num_threads, itrealvalue, starttime, vsize,
    // rss, rsslim, startcode, endcode, startstack, kstkesp, kstkeip, signal, blocked, sigignore,
    // sigcatch, wchan, nswap, cnswap, exit_signal, processor, rt_priority, policy, delayacct_
    // blkio_ticks, guest_time, cguest_time, start_data, end_data, start_brk, arg_start, arg_end,
    // env_start, env_end, exit_code) -- 49 remaining fields, all conservative zeros/placeholders.
    for _ in 0..49 {
        s.push_str(" 0");
    }
    s.push('\n');
    s.into_bytes()
}

/// Real `/proc/[pid]/status` (see `man 5 proc`) is a human-readable `Key:\tvalue` listing.
/// Only the widely-parsed fields are given accurate values; the rest of a real kernel's listing
/// is omitted rather than guessed, since (unlike `stat`) there is no fixed field count a parser
/// depends on here -- every consumer reads this format by key, not by position.
fn format_status(info: &ProcSelfInfo) -> Vec<u8> {
    format!(
        "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t0\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\n",
        info.comm, info.pid, info.pid
    )
    .into_bytes()
}

/// Real `/proc/cpuinfo` content: one stanza per logical CPU, each ending in a blank line, `\n`
/// terminated `key\t: value` pairs. GLib's `g_get_num_processors()` (and several GUI toolkits'
/// thread-pool sizing) counts `processor\t:` lines, so the STANZA COUNT here must match the real
/// host core count exactly; the other fields are reasonable generic values (unlike the count,
/// nothing real depends on their exact content).
fn format_cpuinfo(cpu_count: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..cpu_count.max(1) {
        s.push_str(&format!(
            "processor\t: {i}\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t: 158\nmodel name\t: LiteBox Virtual CPU\nstepping\t: 0\ncpu MHz\t: 2000.000\ncache size\t: 8192 KB\nphysical id\t: 0\nsiblings\t: {cpu_count}\ncore id\t: {i}\ncpu cores\t: {cpu_count}\nfpu\t: yes\nflags\t:\nbogomips\t: 4000.00\nclflush size\t: 64\ncache_alignment\t: 64\naddress sizes\t: 46 bits physical, 48 bits virtual\n\n"
        ));
    }
    s.into_bytes()
}

/// Real `/proc/meminfo` key-value format (see `man 5 proc`), values in kB, `\n`-terminated.
///
/// Both figures come from the platform's real host query (see
/// `crate::platform::SystemInfoProvider::memory_info_kb`), NOT from a formula over an invented
/// total. The previous implementation derived `MemFree`/`MemAvailable` as a fixed 3/4 of
/// `mem_total_kb` and described that as "a safe over-estimate... real Linux tools treat it as a
/// hint, not a hard guarantee". That reasoning is wrong in the one direction that matters:
/// over-estimating *free* memory is an instruction to the guest to allocate memory the host does
/// not have. Measured live -- a 4 GiB total yielded exactly 3 GiB `MemFree`, and Xorg on
/// `linuxserver/webtop:debian-xfce` allocated to precisely that figure (3104-3128 MiB across
/// three runs), peaking near 8.9 GiB during its final growth step and repeatedly tripping the
/// host's low-memory watchdog, which kills with no error and no exit status.
fn format_meminfo(mem_total_kb: u64, mem_avail_kb: u64) -> Vec<u8> {
    // Never advertise more available than total, whatever the platform reported.
    let free = mem_avail_kb.min(mem_total_kb);
    format!(
        "MemTotal:\t{mem_total_kb} kB\nMemFree:\t{free} kB\nMemAvailable:\t{free} kB\nBuffers:\t0 kB\nCached:\t0 kB\nSwapCached:\t0 kB\nSwapTotal:\t0 kB\nSwapFree:\t0 kB\n"
    )
    .into_bytes()
}

/// Real `/proc/mounts` format: `device mountpoint fstype options dump pass`, one line per mount,
/// space-separated. A single entry describing the guest's own root filesystem as it appears to
/// the guest (matches this shim's own rootfs presentation -- read-only tar-backed lower layer
/// composed with a writable in-mem upper layer) -- real completeness (every synthetic `/dev`,
/// `/proc`, `/sys` mount) is deliberately out of scope; the one real client this exists for
/// (gvfs/gio volume monitoring) only needs a well-formed root entry to avoid aborting/spinning.
fn format_mounts() -> Vec<u8> {
    b"rootfs / rootfs rw 0 0\n".to_vec()
}

/// Real `/proc/[pid]/mountinfo` format (see `man 5 proc`): 10+ space-separated fields per line,
/// with a literal ` - ` separator before the last three (fstype, source, super options). A
/// single root entry, minimal-but-format-correct (see [`format_mounts`]'s doc comment for why
/// completeness is out of scope here).
fn format_mountinfo() -> Vec<u8> {
    b"1 0 0:1 / / rw - rootfs rootfs rw\n".to_vec()
}

/// Real `/proc/uptime` format: two space-separated floating point seconds values (system uptime,
/// idle time summed across all CPUs), `\n`-terminated. This shim does not track guest idle time,
/// so idle is conservatively reported equal to uptime -- what actually matters to every known
/// consumer is that this file parses as two valid floats, not their precise values.
fn format_uptime(uptime_secs: u64) -> Vec<u8> {
    format!("{uptime_secs}.00 {uptime_secs}.00\n").into_bytes()
}

/// A [`Backend`] serving the static/host-derived `/proc` flat files that need no per-process
/// state: `cpuinfo`, `meminfo`, `mounts`, `uptime`. Mounted at `/proc`.
///
/// Deliberately NOT a general procfs -- see this module's own doc comment.
pub struct Procfs<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
    cpu_count: usize,
    mem_total_kb: u64,
    mem_avail_kb: u64,
    boot_uptime_secs: u64,
}

impl<Platform> Procfs<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `Procfs` backend.
    ///
    /// `cpu_count` should be the real host logical-CPU count (see
    /// `crate::platform::SystemInfoProvider::cpu_count`) -- this is the one field a real GLib
    /// consumer's thread-pool sizing depends on being accurate. `mem_total_kb` is the host's (or
    /// a reasonable approximation of the guest's) total memory in kB. `boot_uptime_secs` is a
    /// fixed uptime value reported for the lifetime of this backend (this shim does not track a
    /// live wall-clock uptime source usable from `no_std` code).
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        allocator: InodeAllocator,
        cpu_count: usize,
        mem_total_kb: u64,
        mem_avail_kb: u64,
        boot_uptime_secs: u64,
    ) -> Self {
        let root_inode = allocator.next();
        Self {
            _litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
            cpu_count,
            mem_total_kb,
            mem_avail_kb,
            boot_uptime_secs,
        }
    }
}

/// Directory handle: only the backend's mount root exists (a flat namespace).
#[derive(Debug, Clone, Copy)]
pub struct ProcfsDirHandle;

/// Which of the flat files this handle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcfsEntry {
    CpuInfo,
    MemInfo,
    Mounts,
    Uptime,
}

impl ProcfsEntry {
    const ALL: &'static [(&'static str, ProcfsEntry)] = &[
        ("cpuinfo", ProcfsEntry::CpuInfo),
        ("meminfo", ProcfsEntry::MemInfo),
        ("mounts", ProcfsEntry::Mounts),
        ("uptime", ProcfsEntry::Uptime),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }
}

/// Node info for each entry -- distinct, stable, fake inode numbers (mirrors
/// [`super::devices::ProcSysKernel`]'s own fixed constants for its two files).
const PROCFS_CPUINFO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 1,
    rdev: None,
};
const PROCFS_MEMINFO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 2,
    rdev: None,
};
const PROCFS_MOUNTS_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 3,
    rdev: None,
};
const PROCFS_UPTIME_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 4,
    rdev: None,
};

/// Owned file handle; identifies which entry this fd is, and carries its (computed-once, at
/// open time) content so `read`/`file_status` need no further backend state lookups.
#[derive(Debug, Clone)]
pub struct ProcfsFileHandle {
    entry: ProcfsEntry,
    content: Vec<u8>,
}

impl<Platform> super::backend::private::Sealed for Procfs<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for Procfs<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = ProcfsDirHandle;
    type FileHandle = ProcfsFileHandle;
    type DirHandle = ProcfsDirHandle;
}

impl<Platform> Backend for Procfs<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(ProcfsDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            if ProcfsEntry::from_name(component).is_none() {
                return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
            }
            return Ok(WalkOutcome {
                components: vec![],
                last: WalkingDirHandle::from_typed::<Self>(from),
                stop_reason: WalkStopReason::StoppedAtNonDirectory,
            });
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
        let entry = ProcfsEntry::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        let content = match entry {
            ProcfsEntry::CpuInfo => format_cpuinfo(self.cpu_count),
            ProcfsEntry::MemInfo => format_meminfo(self.mem_total_kb, self.mem_avail_kb),
            ProcfsEntry::Mounts => format_mounts(),
            ProcfsEntry::Uptime => format_uptime(self.boot_uptime_secs),
        };
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(ProcfsFileHandle { entry, content }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(ProcfsEntry::ALL
            .iter()
            .map(|(n, _)| DirEntry {
                name: String::from(*n),
                file_type: FileType::RegularFile,
                ino_info: None,
            })
            .collect())
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let h = h.get_typed::<Self>();
        let content = &h.content;
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

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        let h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size: h.content.len(),
            owner: UserInfo::ROOT,
            node_info: match h.entry {
                ProcfsEntry::CpuInfo => PROCFS_CPUINFO_NODE_INFO,
                ProcfsEntry::MemInfo => PROCFS_MEMINFO_NODE_INFO,
                ProcfsEntry::Mounts => PROCFS_MOUNTS_NODE_INFO,
                ProcfsEntry::Uptime => PROCFS_UPTIME_NODE_INFO,
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

/// A [`Backend`] serving `/proc/self/*` files whose content depends on the CURRENT guest process:
/// `exe` (a symlink, via [`Backend::read_link_at`]), `cmdline`, `stat`, `status`, `environ`,
/// `mountinfo`. Mounted at `/proc/self`.
///
/// Backed by a shared [`ProcSelfInfo`] cell (`Arc<RwLock<...>>`), updated by the shim's `execve`
/// handling via [`ProcSelf::handle`] + [`ProcSelfInfo`]'s own fields -- this backend has no
/// per-task concept of its own (a [`Backend`] is shim-wide, not per-process), so it always
/// reflects whichever process most recently `execve`'d, matching this shim's current
/// single-live-process-tree usage.
pub struct ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    _litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
    info: alloc::sync::Arc<RwLock<Platform, ProcSelfInfo>>,
}

impl<Platform> ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `ProcSelf` backend sharing the given `info` cell -- the caller keeps its
    /// own clone of the same `Arc` to update it on `execve` (see this module's doc comment).
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        allocator: InodeAllocator,
        info: alloc::sync::Arc<RwLock<Platform, ProcSelfInfo>>,
    ) -> Self {
        let root_inode = allocator.next();
        Self {
            _litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
            info,
        }
    }
}

/// Directory handle: only the backend's mount root exists (a flat namespace).
#[derive(Debug, Clone, Copy)]
pub struct ProcSelfDirHandle;

/// Which of the flat files this handle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcSelfEntry {
    Exe,
    Cmdline,
    Stat,
    Status,
    Environ,
    MountInfo,
}

impl ProcSelfEntry {
    const ALL: &'static [(&'static str, ProcSelfEntry)] = &[
        ("exe", ProcSelfEntry::Exe),
        ("cmdline", ProcSelfEntry::Cmdline),
        ("stat", ProcSelfEntry::Stat),
        ("status", ProcSelfEntry::Status),
        ("environ", ProcSelfEntry::Environ),
        ("mountinfo", ProcSelfEntry::MountInfo),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }
}

const PROC_SELF_EXE_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 1,
    rdev: None,
};
const PROC_SELF_CMDLINE_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 2,
    rdev: None,
};
const PROC_SELF_STAT_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 3,
    rdev: None,
};
const PROC_SELF_STATUS_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 4,
    rdev: None,
};
const PROC_SELF_ENVIRON_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 5,
    rdev: None,
};
const PROC_SELF_MOUNTINFO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 6,
    rdev: None,
};

/// Owned file handle; identifies which entry this fd is, and carries its (computed-once, at open
/// time -- a fresh snapshot of the shared cell) content.
#[derive(Debug, Clone)]
pub struct ProcSelfFileHandle {
    entry: ProcSelfEntry,
    content: Vec<u8>,
}

impl<Platform> super::backend::private::Sealed for ProcSelf<Platform> where
    Platform: RawSyncPrimitivesProvider + 'static
{
}

impl<Platform> BackendHandles for ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    type WalkingDirHandle<'a> = ProcSelfDirHandle;
    type FileHandle = ProcSelfFileHandle;
    type DirHandle = ProcSelfDirHandle;
}

impl<Platform> Backend for ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    fn root(&self) -> WalkingDirHandle<'_> {
        WalkingDirHandle::from_typed::<Self>(ProcSelfDirHandle)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let from = from.into_typed::<Self>();
        if let Some(&component) = components.first() {
            if ProcSelfEntry::from_name(component).is_none() {
                return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
            }
            return Ok(WalkOutcome {
                components: vec![],
                last: WalkingDirHandle::from_typed::<Self>(from),
                stop_reason: WalkStopReason::StoppedAtNonDirectory,
            });
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

    /// `exe` is a symlink on real Linux; every other entry is a regular file, so this reports
    /// "not a symlink" for them and lets [`Self::read_link_at`] handle `exe` itself.
    fn open_file_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
        flags: OFlags,
    ) -> Result<Permissioned<FileHandle>, OpenError> {
        let _dir = dir.into_typed::<Self>();
        let entry = ProcSelfEntry::from_name(name)
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        let snapshot = self.info.read().clone();
        let content = match entry {
            // A direct `open("/proc/self/exe")` (rather than `readlink`) on real Linux opens the
            // symlink's TARGET (the executable itself) -- but this shim has no real inode for the
            // running binary to hand back a matching fd for, and no known guest workload in scope
            // for this task opens `exe` directly rather than reading it via `readlink`, so this
            // reports the path text itself rather than attempting to open the target. `readlink`
            // (via `read_link_at` below) is the well-formed path real consumers use.
            ProcSelfEntry::Exe => snapshot.exe_path.clone().into_bytes(),
            ProcSelfEntry::Cmdline => snapshot.cmdline.clone(),
            ProcSelfEntry::Stat => format_stat(&snapshot),
            ProcSelfEntry::Status => format_status(&snapshot),
            ProcSelfEntry::Environ => snapshot.environ.clone(),
            ProcSelfEntry::MountInfo => format_mountinfo(),
        };
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(ProcSelfFileHandle { entry, content }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    /// `exe` is the one real symlink in this backend -- see this module's own doc comment for
    /// why `/proc/self/exe` matters enough to get real symlink semantics (`readlink` returning
    /// the resolved guest binary path) rather than a plain-file stand-in.
    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let _dir = dir.into_typed::<Self>();
        match ProcSelfEntry::from_name(name) {
            Some(ProcSelfEntry::Exe) => Ok(Some(self.info.read().exe_path.clone())),
            Some(_) => Ok(None),
            None => Err(OpenError::PathError(PathError::NoSuchFileOrDirectory)),
        }
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(ProcSelfEntry::ALL
            .iter()
            .map(|(n, _)| DirEntry {
                name: String::from(*n),
                file_type: FileType::RegularFile,
                ino_info: None,
            })
            .collect())
    }

    fn read(&self, h: &FileHandle, buf: &mut [u8], offset: usize) -> Result<usize, ReadError> {
        let h = h.get_typed::<Self>();
        let content = &h.content;
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

    fn file_status(&self, h: &FileHandle) -> Result<FileStatus, FileStatusError> {
        let h = h.get_typed::<Self>();
        Ok(FileStatus {
            file_type: FileType::RegularFile,
            mode: Mode::RUSR | Mode::RGRP | Mode::ROTH,
            size: h.content.len(),
            owner: UserInfo::ROOT,
            node_info: match h.entry {
                ProcSelfEntry::Exe => PROC_SELF_EXE_NODE_INFO,
                ProcSelfEntry::Cmdline => PROC_SELF_CMDLINE_NODE_INFO,
                ProcSelfEntry::Stat => PROC_SELF_STAT_NODE_INFO,
                ProcSelfEntry::Status => PROC_SELF_STATUS_NODE_INFO,
                ProcSelfEntry::Environ => PROC_SELF_ENVIRON_NODE_INFO,
                ProcSelfEntry::MountInfo => PROC_SELF_MOUNTINFO_NODE_INFO,
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
