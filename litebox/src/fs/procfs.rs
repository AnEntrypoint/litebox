// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Synthesized `/proc`: format-accurate renderings of the fixed sets `ProcfsEntry::ALL`
//! ([`Procfs`], mounted `/proc`) and `ProcSelfEntry::ALL` ([`ProcSelf`], mounted `/proc/self`);
//! not a general procfs -- gm mutable `mut-1789043963534`. `auxv`/`maps` are load-bearing, not
//! informational: rustix `unwrap()`s auxv, std parses maps -- gm mutable `mut-1789043907427`.

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

/// Per-process data backing `/proc/self/*`, refreshed on every `execve` by whoever owns the shared
/// [`ProcSelfTable`] this backend reads through (see [`ProcSelf::new`]).
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
    /// The process's auxiliary vector in `/proc/[pid]/auxv` form: `(a_type, a_val)` `usize` pairs
    /// in native byte order, terminated by an `AT_NULL` pair. Supplied by the loader from the bytes
    /// it wrote to the initial stack, so the two cannot disagree; load-bearing for rustix -- see gm
    /// mutable `mut-1789043907427`.
    pub auxv: Vec<u8>,
    /// Renders `/proc/[pid]/maps` for the CURRENT process, or `None` before any process has been
    /// loaded. A callback, not a snapshot: the address space changes on every `mmap`/`mprotect`/
    /// `dlopen`, and a closure is the only shape that can name the shim's per-process memory
    /// manager from this crate. Load-bearing for Rust std -- gm mutable `mut-1789043907427`.
    pub maps: Option<alloc::sync::Arc<dyn Fn() -> Vec<u8> + Send + Sync>>,
}

/// Real `/proc/[pid]/stat`: 52 whitespace-separated fields, `comm` parenthesized so readers split
/// on the LAST `)`. `libgtop` parses it positionally -- see gm mutable `mut-1789043806784`.
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

/// Real `/proc/[pid]/status`: a `Key:\tvalue` listing read by key, never by position, so unknown
/// keys are omitted rather than guessed -- see gm mutable `mut-1789043822948`.
fn format_status(info: &ProcSelfInfo) -> Vec<u8> {
    format!(
        "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t0\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\n",
        info.comm, info.pid, info.pid
    )
    .into_bytes()
}

/// Real `/proc/cpuinfo`: one blank-line-terminated `key\t: value` stanza per logical CPU, and the
/// stanza count must match the real host core count -- GLib's `g_get_num_processors()` counts
/// `processor\t:` lines. See gm mutable `mut-1789043826779`.
fn format_cpuinfo(cpu_count: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..cpu_count.max(1) {
        s.push_str(&format!(
            "processor\t: {i}\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t: 158\nmodel name\t: LiteBox Virtual CPU\nstepping\t: 0\ncpu MHz\t: 2000.000\ncache size\t: 8192 KB\nphysical id\t: 0\nsiblings\t: {cpu_count}\ncore id\t: {i}\ncpu cores\t: {cpu_count}\nfpu\t: yes\nflags\t:\nbogomips\t: 4000.00\nclflush size\t: 64\ncache_alignment\t: 64\naddress sizes\t: 46 bits physical, 48 bits virtual\n\n"
        ));
    }
    s.into_bytes()
}

/// Real `/proc/meminfo`: `Key:\tvalue` in kB. `MemFree`/`MemAvailable` must come from the
/// platform's real host query, never a fraction of an invented total -- over-stating free memory
/// drove Xorg into the host's low-memory watchdog. See gm mutable `mut-1789043836796`.
fn format_meminfo(mem_total_kb: u64, mem_avail_kb: u64) -> Vec<u8> {
    // Never advertise more available than total, whatever the platform reported.
    let free = mem_avail_kb.min(mem_total_kb);
    format!(
        "MemTotal:\t{mem_total_kb} kB\nMemFree:\t{free} kB\nMemAvailable:\t{free} kB\nBuffers:\t0 kB\nCached:\t0 kB\nSwapCached:\t0 kB\nSwapTotal:\t0 kB\nSwapFree:\t0 kB\n"
    )
    .into_bytes()
}

/// Real `/proc/mounts`: `device mountpoint fstype options dump pass` per line. A single root entry;
/// real completeness is deliberately out of scope -- gvfs/gio volume monitoring only needs a
/// well-formed root entry. See gm mutable `mut-1789044455985`.
fn format_mounts() -> Vec<u8> {
    b"rootfs / rootfs rw 0 0\n".to_vec()
}

/// Real `/proc/filesystems`: one type per line, `nodev	` prefixed when no block device is needed.
/// Must list only what litebox actually serves -- a missing file reads to `mount`/`libmount` and
/// GIO's volume monitor as "this kernel supports nothing". See gm mutable `mut-1789043857688`.
fn format_filesystems() -> Vec<u8> {
    b"nodev	rootfs
nodev	proc
nodev	sysfs
nodev	devtmpfs
nodev	tmpfs
nodev	devpts
"
        .to_vec()
}

/// Real `/proc/stat`: a `cpu` aggregate line, one `cpuN` line per logical CPU, then the
/// `intr`/`ctxt`/`btime`/`processes`/`procs_running`/`procs_blocked` counters. Counters are
/// honestly zero (litebox does not schedule guest threads); the `cpuN` count is the real payload
/// `nproc` and GLib read. See gm mutable `mut-1789043865374`.
fn format_stat_global(cpu_count: usize) -> Vec<u8> {
    let mut out = String::from("cpu  0 0 0 0 0 0 0 0 0 0
");
    for cpu in 0..cpu_count {
        out.push_str(&format!("cpu{cpu} 0 0 0 0 0 0 0 0 0 0
"));
    }
    out.push_str("intr 0
ctxt 0
btime 0
processes 0
procs_running 1
procs_blocked 0
");
    out.into_bytes()
}

/// Real `/proc/cmdline`: the kernel boot command line, one line. Must exist -- systemd-ish tooling,
/// container-detection heuristics and `dracut`-style probes read a missing file as a broken `/proc`
/// mount rather than an empty command line. See gm mutable `mut-1789043872060`.
fn format_kernel_cmdline() -> Vec<u8> {
    b"BOOT_IMAGE=/litebox root=/dev/root rw
".to_vec()
}

/// Real `/proc/[pid]/mountinfo`: 10+ space-separated fields per line with a literal ` - ` before
/// the last three (fstype, source, super options). A single root entry; completeness is out of
/// scope -- see gm mutable `mut-1789044455985`.
fn format_mountinfo() -> Vec<u8> {
    b"1 0 0:1 / / rw - rootfs rootfs rw\n".to_vec()
}

/// Real `/proc/[pid]/oom_score_adj`: one decimal integer in -1000..=1000. `0` (the kernel default)
/// must be served rather than `ENOENT` -- GLib's `g_spawn`/`gio` and systemd's `oom_score_adjust`
/// save-and-restore it around every spawn, and absence is a failure to report where `0` is simply
/// "nothing to restore". See gm mutable `mut-1789043876626`.
fn format_oom_score_adj() -> Vec<u8> {
    Vec::from(&b"0
"[..])
}

/// Real `/proc/[pid]/cgroup`, unified (v2) form: `hierarchy-ID:controller-list:cgroup-path` per
/// line, so a v2-only system with no controllers of its own is exactly `0::/`. Must exist rather
/// than `ENOENT` -- glib's `g_get_user_runtime_dir` and systemd's `sd_pid_get_unit` read a missing
/// file as "cgroups not mounted at all". See gm mutable `mut-1789043885409`.
fn format_cgroup() -> Vec<u8> {
    Vec::from(&b"0::/
"[..])
}

/// Real `/proc/uptime`: two space-separated float seconds (uptime, summed idle), `\n`-terminated.
/// Idle is reported equal to uptime -- no guest idle accounting exists, and consumers only need two
/// parseable floats. See gm mutable `mut-1789044456945`.
fn format_uptime(uptime_secs: u64) -> Vec<u8> {
    format!("{uptime_secs}.00 {uptime_secs}.00\n").into_bytes()
}

/// A [`Backend`] serving the static/host-derived `/proc` flat files that need no per-process state
/// -- the exact set is `ProcfsEntry::ALL`. Mounted at `/proc`; deliberately not a general procfs
/// (gm mutable `mut-1789043963534`).
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
    /// `cpu_count` must be the real host logical-CPU count (GLib thread-pool sizing depends on it);
    /// `boot_uptime_secs` is fixed for this backend's lifetime. See gm mutable `mut-1789043963534`.
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
    /// `filesystems` -- which filesystem types this "kernel" can mount.
    Filesystems,
    /// `stat` -- kernel/CPU activity counters.
    Stat,
    /// `cmdline` -- the kernel's own boot command line.
    Cmdline,
}

impl ProcfsEntry {
    const ALL: &'static [(&'static str, ProcfsEntry)] = &[
        ("cpuinfo", ProcfsEntry::CpuInfo),
        ("meminfo", ProcfsEntry::MemInfo),
        ("mounts", ProcfsEntry::Mounts),
        ("uptime", ProcfsEntry::Uptime),
        ("filesystems", ProcfsEntry::Filesystems),
        ("stat", ProcfsEntry::Stat),
        ("cmdline", ProcfsEntry::Cmdline),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }
}

/// Node info for each entry -- distinct, stable, fake inode numbers (mirrors
/// [`super::static_files`]'s own allocator-assigned inodes).
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
const PROCFS_FILESYSTEMS_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 5,
    rdev: None,
};
const PROCFS_STAT_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 6,
    rdev: None,
};
const PROCFS_CMDLINE_NODE_INFO: NodeInfo = NodeInfo {
    dev: 6,
    ino: 7,
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
            ProcfsEntry::Filesystems => format_filesystems(),
            ProcfsEntry::Stat => format_stat_global(self.cpu_count),
            ProcfsEntry::Cmdline => format_kernel_cmdline(),
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
                ProcfsEntry::Filesystems => PROCFS_FILESYSTEMS_NODE_INFO,
                ProcfsEntry::Stat => PROCFS_STAT_NODE_INFO,
                ProcfsEntry::Cmdline => PROCFS_CMDLINE_NODE_INFO,
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

/// Every live guest process's [`ProcSelfInfo`], keyed by pid.
///
/// Per-pid rather than one shared cell: a shim-wide [`Backend`] carries no caller identity, so one
/// cell served every process the last `execve`'s `auxv`/`maps` -- gm mutable `mut-1789043924408`.
#[derive(Default)]
pub struct ProcSelfTable {
    by_pid: alloc::collections::BTreeMap<i32, ProcSelfInfo>,
    /// The last pid written, used only when the platform cannot name the calling process (see
    /// [`ProcSelfTable::resolve`]).
    most_recent: Option<i32>,
}

impl ProcSelfTable {
    /// Replaces `pid`'s entry wholesale, as `execve` does.
    pub fn set(&mut self, pid: i32, info: ProcSelfInfo) {
        self.by_pid.insert(pid, info);
        self.most_recent = Some(pid);
    }

    /// Mutates `pid`'s existing entry, for the fields `execve` can only fill in after `load`.
    ///
    /// Does nothing if there is no entry -- a caller completing a snapshot it just wrote always
    /// has one, and inventing a default here would manufacture a process that never existed.
    pub fn with_mut(&mut self, pid: i32, f: impl FnOnce(&mut ProcSelfInfo)) {
        if let Some(info) = self.by_pid.get_mut(&pid) {
            f(info);
        }
    }

    /// Gives `child` its own copy of `parent`'s entry, on a process `clone()`.
    ///
    /// A child not yet `execve`'d runs its parent's binary and argv, so only `pid` differs; `maps`
    /// is NOT inherited, closing over the parent's page manager. gm mutable `mut-1789043924408`.
    pub fn inherit(&mut self, parent: i32, child: i32) {
        let Some(mut info) = self.by_pid.get(&parent).cloned() else {
            return;
        };
        info.pid = child;
        info.maps = None;
        self.by_pid.insert(child, info);
    }

    /// Drops `pid`'s entry, on process exit.
    ///
    /// Not optional: each entry holds that process's whole `cmdline`, `environ` and `auxv` plus an
    /// `Arc` closure pinning its page-manager mapping table. gm mutable `mut-1789043924408`.
    pub fn remove(&mut self, pid: i32) {
        self.by_pid.remove(&pid);
        if self.most_recent == Some(pid) {
            self.most_recent = None;
        }
    }

    /// The entry `/proc/self` should serve to a caller whose pid is `caller`.
    ///
    /// Falls back to the most recently written entry when `caller` is `None` or names a pid with no
    /// entry -- that fallback is the old single-cell behaviour, kept deliberately so this can only
    /// improve an answer, never remove one. See gm mutable `mut-1789043924408`.
    fn resolve(&self, caller: Option<i32>) -> Option<&ProcSelfInfo> {
        caller
            .and_then(|pid| self.by_pid.get(&pid))
            .or_else(|| self.most_recent.and_then(|pid| self.by_pid.get(&pid)))
    }
}

/// A [`Backend`] serving the `/proc/self/*` files whose content depends on the CURRENT guest
/// process -- the exact set is `ProcSelfEntry::ALL`, and `exe` is a symlink (see
/// [`Backend::read_link_at`]). Mounted at `/proc/self`; resolved per CALLER from a shared
/// [`ProcSelfTable`] via `ThreadProvider::current_guest_pid`. gm mutable `mut-1789043924408`.
pub struct ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + crate::platform::ThreadProvider + 'static,
{
    litebox: LiteBox<Platform>,
    root_inode: NodeInfo,
    _alloc: InodeAllocator,
    info: alloc::sync::Arc<RwLock<Platform, ProcSelfTable>>,
}

impl<Platform> ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + crate::platform::ThreadProvider + 'static,
{
    /// Construct a new `ProcSelf` backend sharing the given `info` table -- the caller keeps its
    /// own clone of the same `Arc` to update it on `execve`.
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        allocator: InodeAllocator,
        info: alloc::sync::Arc<RwLock<Platform, ProcSelfTable>>,
    ) -> Self {
        let root_inode = allocator.next();
        Self {
            litebox: litebox.clone(),
            root_inode,
            _alloc: allocator,
            info,
        }
    }

    /// The [`ProcSelfInfo`] this backend should answer with for the CALLING guest process.
    ///
    /// Cloned, not borrowed: the lock must not be held across rendering, and `/proc/self/maps` is a
    /// closure the caller invokes after release. See gm mutable `mut-1789043934584`.
    fn current(&self) -> Option<ProcSelfInfo> {
        let caller = crate::platform::ThreadProvider::current_guest_pid(self.litebox.x.platform);
        self.info.read().resolve(caller).cloned()
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
    Cgroup,
    OomScoreAdj,
    Auxv,
    Maps,
}

impl ProcSelfEntry {
    const ALL: &'static [(&'static str, ProcSelfEntry)] = &[
        ("exe", ProcSelfEntry::Exe),
        ("cmdline", ProcSelfEntry::Cmdline),
        ("stat", ProcSelfEntry::Stat),
        ("status", ProcSelfEntry::Status),
        ("environ", ProcSelfEntry::Environ),
        ("mountinfo", ProcSelfEntry::MountInfo),
        ("cgroup", ProcSelfEntry::Cgroup),
        ("oom_score_adj", ProcSelfEntry::OomScoreAdj),
        ("auxv", ProcSelfEntry::Auxv),
        ("maps", ProcSelfEntry::Maps),
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
const PROC_SELF_CGROUP_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 7,
    rdev: None,
};
const PROC_SELF_OOM_SCORE_ADJ_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 8,
    rdev: None,
};
const PROC_SELF_AUXV_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 9,
    rdev: None,
};
const PROC_SELF_MAPS_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 10,
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
    Platform: RawSyncPrimitivesProvider + crate::platform::ThreadProvider + 'static
{
}

impl<Platform> BackendHandles for ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + crate::platform::ThreadProvider + 'static,
{
    type WalkingDirHandle<'a> = ProcSelfDirHandle;
    type FileHandle = ProcSelfFileHandle;
    type DirHandle = ProcSelfDirHandle;
}

impl<Platform> Backend for ProcSelf<Platform>
where
    Platform: RawSyncPrimitivesProvider + crate::platform::ThreadProvider + 'static,
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
        // A caller the table does not know yet (nothing `execve`'d) gets an empty snapshot, not an
        // error -- see gm mutable `mut-1789043934584`.
        let snapshot = self.current().unwrap_or_default();
        let content = match entry {
            // Deliberate deviation: a direct `open` reports the path TEXT, not the symlink target
            // real Linux would open -- `readlink` is the path real consumers use. See gm mutable
            // `mut-1789043942984`.
            ProcSelfEntry::Exe => snapshot.exe_path.clone().into_bytes(),
            ProcSelfEntry::Cmdline => snapshot.cmdline.clone(),
            ProcSelfEntry::Stat => format_stat(&snapshot),
            ProcSelfEntry::Status => format_status(&snapshot),
            ProcSelfEntry::Environ => snapshot.environ.clone(),
            ProcSelfEntry::MountInfo => format_mountinfo(),
            ProcSelfEntry::Cgroup => format_cgroup(),
            ProcSelfEntry::OomScoreAdj => format_oom_score_adj(),
            ProcSelfEntry::Auxv => snapshot.auxv.clone(),
            ProcSelfEntry::Maps => snapshot.maps.as_ref().map_or_else(Vec::new, |f| f()),
        };
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(ProcSelfFileHandle { entry, content }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    /// `exe` is the one real symlink in this backend: `readlink` returns the resolved guest binary
    /// path, and every other entry answers "not a symlink". gm mutable `mut-1789043963534`.
    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let _dir = dir.into_typed::<Self>();
        match ProcSelfEntry::from_name(name) {
            Some(ProcSelfEntry::Exe) => Ok(self.current().map(|info| info.exe_path)),
            Some(_) => Ok(None),
            None => Err(OpenError::PathError(PathError::NoSuchFileOrDirectory)),
        }
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let _handle = handle.into_typed::<Self>();
        Ok(ProcSelfEntry::ALL
            .iter()
            .map(|(n, e)| DirEntry {
                name: String::from(*n),
                // `exe`'s `d_type` must say `Symlink`: a caller trusting `getdents64`'s `d_type`
                // instead of re-`lstat`ing would never follow it. gm mutable `mut-1789043942984`.
                file_type: match e {
                    ProcSelfEntry::Exe => FileType::Symlink,
                    _ => FileType::RegularFile,
                },
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
                ProcSelfEntry::Cgroup => PROC_SELF_CGROUP_NODE_INFO,
                ProcSelfEntry::OomScoreAdj => PROC_SELF_OOM_SCORE_ADJ_NODE_INFO,
                ProcSelfEntry::Auxv => PROC_SELF_AUXV_NODE_INFO,
                ProcSelfEntry::Maps => PROC_SELF_MAPS_NODE_INFO,
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
