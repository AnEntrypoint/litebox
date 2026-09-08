// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Synthesized `/proc` entries [`super::backend::Backend`]s.
//!
//! A stock XFCE/GLib/dbus desktop session (and many ordinary CLI tools) read a small, fixed set
//! of `/proc` paths for introspection or sizing purposes -- none of them need real process
//! introspection to be individually CORRECT, only a format-accurate synthesis. This is
//! deliberately NOT a general-purpose `/proc` filesystem (see `Procfs`/`ProcSelf`'s own doc
//! comments for the exact, fixed set each one covers) -- mirrors the same "minimal, exact files a
//! real client needs" pattern already used by [`super::static_files`] for
//! the constant `/proc/sys` and `/sys` files.
//!
//! `/proc/self/auxv` is load-bearing rather than informational: rustix falls back to reading
//! it when it cannot obtain the auxiliary vector from the initial stack, and it `unwrap()`s
//! the result. Every Rust-coreutils (uutils) binary therefore ABORTS outright without this
//! file -- on `linuxserver/webtop:ubuntu-xfce`, whose /bin/mkdir, /bin/cp and /bin/rm are
//! uutils, that meant every shell script in the boot path failing at its first command.
//!
//! `/proc/self/maps` is load-bearing for the same image and the same reason: Rust's std
//! locates the main thread's stack guard by parsing it when installing the SIGSEGV handler
//! that reports stack overflow. With the file absent it proceeds on a guess, and the first
//! thing the guest does after `rt_sigaction(SIGSEGV)` is take a real SIGSEGV.
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
    /// The process's auxiliary vector in `/proc/[pid]/auxv` form: `(a_type, a_val)` `usize` pairs
    /// in native byte order, terminated by an `AT_NULL` pair.
    ///
    /// Supplied by the loader from the very bytes it wrote to the initial stack, rather than
    /// rebuilt here, so the file and the stack cannot disagree -- see the shim's
    /// `UserStack::push_aux`.
    pub auxv: Vec<u8>,
    /// Renders `/proc/[pid]/maps` for the CURRENT process, or `None` before any process has been
    /// loaded.
    ///
    /// A callback rather than a snapshot, because unlike every other field here the address space
    /// changes constantly -- every `mmap`, `munmap`, `mprotect`, `dlopen` and heap growth alters
    /// it. A value captured at `execve` would be stale by the time anything read it, and the
    /// readers that matter are asking precisely because they need the CURRENT layout.
    ///
    /// It is a closure because this crate cannot name the shim's per-process memory manager; the
    /// shim installs one that holds an `Arc` to it (see `load_program`).
    pub maps: Option<alloc::sync::Arc<dyn Fn() -> Vec<u8> + Send + Sync>>,
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

/// Real `/proc/filesystems` format: one filesystem type per line, `nodev	` prefixed for types
/// that need no backing block device, a bare tab otherwise.
///
/// A live XFCE session read this 18 times in one boot and got `ENOENT` every time. The readers are
/// `mount`/`libmount`, GIO's volume monitor, and anything deciding whether a `tmpfs` or `proc`
/// mount is even possible before attempting it -- a missing file reads as "this kernel supports
/// nothing", which is a different and worse answer than an honest short list.
///
/// The list is exactly what litebox actually serves: the synthesized `proc`/`sysfs` trees, the
/// `devtmpfs`/`tmpfs` shape `/dev` and `/dev/shm` present, and `rootfs` for the tar-backed root
/// [`format_mounts`] already reports. Claiming `ext4`/`overlay`/`fuse` here would be a lie a
/// caller could act on.
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

/// Real `/proc/stat` format: a `cpu` aggregate line, one `cpuN` line per logical CPU, then the
/// `intr`/`ctxt`/`btime`/`processes`/`procs_running`/`procs_blocked` counters.
///
/// Every counter is zero, and that is honest rather than lazy: litebox does not schedule guest
/// threads itself (Windows does), so it has no jiffy accounting to report and inventing plausible
/// numbers would make a monitoring client draw graphs of fiction. What the readers overwhelmingly
/// want from this file is the CPU COUNT -- `nproc`, GLib's `g_get_num_processors` fallback, and
/// several thread-pool sizers count `cpuN` lines -- and that number is real.
///
/// Named `format_stat_global` to keep it distinct from [`format_stat`], which renders the very
/// differently-shaped per-process `/proc/[pid]/stat`.
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

/// Real `/proc/cmdline` content: the kernel's own boot command line, one line.
///
/// There is no bootloader here and no kernel command line to report, so the honest content is the
/// bare minimum a real kernel always carries. Readers (systemd-ish tooling, container-detection
/// heuristics, `dracut`-style probes) parse it for `key=value` options and treat a missing file as
/// a broken `/proc` mount rather than as an empty command line.
fn format_kernel_cmdline() -> Vec<u8> {
    b"BOOT_IMAGE=/litebox root=/dev/root rw
".to_vec()
}

/// Real `/proc/[pid]/mountinfo` format (see `man 5 proc`): 10+ space-separated fields per line,
/// with a literal ` - ` separator before the last three (fstype, source, super options). A
/// single root entry, minimal-but-format-correct (see [`format_mounts`]'s doc comment for why
/// completeness is out of scope here).
fn format_mountinfo() -> Vec<u8> {
    b"1 0 0:1 / / rw - rootfs rootfs rw\n".to_vec()
}

/// Real `/proc/[pid]/oom_score_adj` content: the OOM-killer score adjustment, one decimal integer
/// on a line, in the range -1000..=1000.
///
/// `0` is the kernel's own default for a process that has not been adjusted, and it is the honest
/// answer here: litebox has no OOM killer, so nothing is ever adjusted away from the default.
///
/// This matters because the readers treat absence and neutrality differently. GLib's
/// `g_spawn`/`gio` launch paths, systemd's `oom_score_adjust`, and several session managers read
/// this file to save-and-restore the value around spawning a child; a missing file makes that a
/// visible failure to report, whereas `0` is simply "nothing to restore". The webtop desktop hit
/// it on essentially every process launch.
fn format_oom_score_adj() -> Vec<u8> {
    Vec::from(&b"0
"[..])
}

/// Real `/proc/[pid]/cgroup` content, unified-hierarchy (cgroup v2) form.
///
/// The format is `hierarchy-ID:controller-list:cgroup-path` per line. On a v2-only system --
/// which is what every current container runtime presents, and what this shim most closely
/// resembles, having no cgroup controllers of its own -- there is exactly one line, the hierarchy
/// ID is `0`, and the controller list is empty: `0::/`.
///
/// This existing as a real file rather than `ENOENT` matters because it is not an optional
/// nicety for the consumers that read it. glib's `g_get_user_runtime_dir`, systemd's
/// `sd_pid_get_unit`, and libcontainer-aware code all probe it, and several of them treat a
/// missing file (`ENOENT`) differently from a v2 answer -- a MISSING file reads as "cgroups are
/// not mounted at all, this is a pre-2008 kernel", which is a state no modern userspace is
/// prepared for, whereas `0::/` reads as "cgroup v2, this process is in the root group", which is
/// both true here and the case every one of them handles. The webtop stack read this thousands of
/// times in a single boot.
fn format_cgroup() -> Vec<u8> {
    Vec::from(&b"0::/
"[..])
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
/// # Why this is a table and not one cell
///
/// It was one cell. A [`Backend`] is shim-wide -- built once, shared by every guest process -- and
/// the trait carries no identity of the caller, so `/proc/self` was served from a single
/// `ProcSelfInfo` that whichever process `execve`'d most recently overwrote. Every other process
/// then read that one's `exe`, `cmdline`, `environ`, `stat`, `status`, `auxv` and `maps` as its
/// own.
///
/// For `exe` and `cmdline` that is wrong but inert. The other two are not inert:
///
/// - `/proc/self/auxv` is read by rustix when it cannot get the auxiliary vector from the initial
///   stack, and the result is `unwrap()`ed. Handing it another binary's `AT_PHDR`/`AT_ENTRY`/
///   `AT_BASE` is handing it a description of an address space the caller does not have.
/// - `/proc/self/maps` is parsed by Rust's std to locate the main thread's stack guard before it
///   installs the handler that reports stack overflow. Another process's map means another
///   process's stack bounds.
///
/// Both are read during early process startup, which is exactly when a busy session has several
/// processes starting at once -- so the window is not narrow, and what lands in it varies run to
/// run. A desktop whose startup outcome differs between identical runs is the symptom this shape
/// produces.
///
/// [`Backend`]: super::backend::Backend
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
    /// A `fork()`ed child that has not `execve`'d yet genuinely IS running its parent's binary with
    /// its parent's argv and environment, so real Linux's `/proc/<child>/exe` and `cmdline` are the
    /// parent's -- only the `pid` field differs. Without this the child has no entry at all and
    /// falls back to whichever process wrote last (see [`Self::resolve`]), which for a desktop --
    /// where every shell script in the startup path forks constantly and most of those children
    /// never `execve` -- is usually some unrelated process.
    ///
    /// `maps` is deliberately NOT carried over: it is a closure over the PARENT's page manager, and
    /// the child's address space is its own from the moment it starts. A child that `execve`s gets a
    /// renderer for its own page manager there; one that does not would rather report nothing than
    /// report its parent's address space as its own.
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
    /// Not optional housekeeping: each entry holds that process's whole `cmdline`, `environ` and
    /// `auxv`, plus an `Arc` closure keeping its page-manager mapping table alive. A session that
    /// starts thousands of short-lived processes would otherwise hold every one of them forever.
    pub fn remove(&mut self, pid: i32) {
        self.by_pid.remove(&pid);
        if self.most_recent == Some(pid) {
            self.most_recent = None;
        }
    }

    /// The entry `/proc/self` should serve to a caller whose pid is `caller`.
    ///
    /// Falls back to the most recently written entry when `caller` is `None` (a platform that does
    /// not track per-thread guest pids) or names a pid with no entry. That fallback IS the old
    /// single-cell behaviour, deliberately: it is what a platform without
    /// [`ThreadProvider::current_guest_pid`] could do anyway, so keeping it means this change can
    /// only improve an answer, never remove one.
    ///
    /// [`ThreadProvider::current_guest_pid`]: crate::platform::ThreadProvider::current_guest_pid
    fn resolve(&self, caller: Option<i32>) -> Option<&ProcSelfInfo> {
        caller
            .and_then(|pid| self.by_pid.get(&pid))
            .or_else(|| self.most_recent.and_then(|pid| self.by_pid.get(&pid)))
    }
}

/// A [`Backend`] serving `/proc/self/*` files whose content depends on the CURRENT guest process:
/// `exe` (a symlink, via [`Backend::read_link_at`]), `cmdline`, `stat`, `status`, `environ`,
/// `mountinfo`. Mounted at `/proc/self`.
///
/// Backed by a shared [`ProcSelfTable`] (`Arc<RwLock<...>>`) the shim's `execve` handling writes
/// one entry per guest process into, and resolved per CALLER via
/// [`ThreadProvider::current_guest_pid`] -- see [`ProcSelfTable`]'s own doc comment for what this
/// replaced (one global cell holding whichever process `execve`'d last) and why `auxv` and `maps`
/// made that actively dangerous rather than merely inaccurate.
///
/// [`ThreadProvider::current_guest_pid`]: crate::platform::ThreadProvider::current_guest_pid
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
    /// own clone of the same `Arc` to update it on `execve` (see this module's doc comment).
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
    /// Cloned rather than borrowed because the lock cannot be held across the rendering below, and
    /// because `/proc/self/maps` is a closure the caller invokes after the lock is released.
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
        // `unwrap_or_default` rather than an error: a process that has not yet `execve`'d has no
        // snapshot, and an empty one renders every file as empty -- the same thing the single
        // global cell produced before any process had run, and a better answer than refusing the
        // read outright to a caller the table simply does not know yet.
        let snapshot = self.current().unwrap_or_default();
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
                // `exe` is a symlink and `read_link_at` above already treats it as one; reporting
                // `RegularFile` here contradicted that, and a caller that trusts `d_type` from
                // `getdents64` rather than re-`lstat`ing (which is the whole point of `d_type`)
                // would never follow it.
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
