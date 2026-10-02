// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Synthesized `/proc`: format-accurate renderings of the fixed sets `ProcfsEntry::ALL`
//! ([`Procfs`], mounted `/proc`) and `ProcSelfEntry::ALL` ([`ProcSelf`], mounted `/proc/self`);
//! not a general procfs -- gm mutable `mut-1789043963534`. `auxv`/`maps` are load-bearing, not
//! informational: rustix `unwrap()`s auxv, std parses maps -- gm mutable `mut-1789043907427`.

use alloc::format;
use alloc::string::String;
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
    /// Lists this process's live thread ids, for `/proc/self/task`. A callback for the same
    /// reason as `maps`: the set changes with every `clone` and thread exit.
    pub tids: Option<alloc::sync::Arc<dyn Fn() -> Vec<i32> + Send + Sync>>,
    /// Lists this process's open raw file descriptors, for `/proc/self/fd`.
    pub fds: Option<alloc::sync::Arc<dyn Fn() -> Vec<(i32, String)> + Send + Sync>>,
    /// `NoNewPrivs:` in `/proc/[pid]/status`: `PR_SET_NO_NEW_PRIVS` state.
    pub no_new_privs: bool,
    /// `Seccomp:` in `/proc/[pid]/status`: 0 disabled, 1 strict, 2 filter -- Linux's own encoding.
    pub seccomp_mode: u8,
}

/// The uptime `/proc/uptime` reports and the process start times in `/proc/[pid]/stat` are measured
/// against. Chromium derives a process creation time from `btime` plus `starttime`, so both must be
/// plausible rather than zero.
pub const FAKE_BOOT_UPTIME_SECS: u64 = 3600;

/// Real `/proc/[pid]/stat`: 52 whitespace-separated fields, `comm` parenthesized so readers split
/// on the LAST `)`. `libgtop` parses it positionally -- see gm mutable `mut-1789043806784`.
fn format_stat(info: &ProcSelfInfo) -> Vec<u8> {
    let ppid = if info.pid == 1 { 0 } else { 1 };
    let start_ticks = FAKE_BOOT_UPTIME_SECS.saturating_sub(5) * 100;
    format!(
        "{pid} ({comm}) R {ppid} {pid} {pid} 0 -1 4194560 100 0 0 0 50 20 0 0 20 0 1 0 {start_ticks}          1073741824 20000 18446744073709551615 4194304 4194305 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0          4194304 4194305 4194305 0 0 0 0 0
",
        pid = info.pid,
        comm = info.comm,
    )
    .into_bytes()
}

/// Real `/proc/[pid]/status`: a `Key:\tvalue` listing read by key, never by position, so unknown
/// keys are omitted rather than guessed -- see gm mutable `mut-1789043822948`.
fn format_status(info: &ProcSelfInfo) -> Vec<u8> {
    format!(
        "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t0\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\nNoNewPrivs:\t{}\nSeccomp:\t{}\n",
        info.comm,
        info.pid,
        info.pid,
        u8::from(info.no_new_privs),
        info.seccomp_mode
    )
    .into_bytes()
}

/// Real `/proc/cpuinfo`: one blank-line-terminated `key\t: value` stanza per logical CPU, and the
/// stanza count must match the real host core count -- GLib's `g_get_num_processors()` counts
/// `processor\t:` lines. See gm mutable `mut-1789043826779`.
/// The `flags` line, from CPUID: guest code runs natively on the host CPU, so what CPUID says is
/// what the guest can execute (Chromium's launcher, for one, refuses to start without `pni`).
#[cfg(target_arch = "x86_64")]
fn cpu_flags() -> String {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let mut out: Vec<&str> = Vec::new();
    let mut add = |reg: u32, table: &[(u32, &'static str)]| {
        for &(bit, name) in table {
            if reg & (1 << bit) != 0 {
                out.push(name);
            }
        }
    };
    // SAFETY: CPUID is available on every x86_64 CPU.
    let (l1, l7, e1) = (__cpuid(1), __cpuid_count(7, 0), __cpuid(0x8000_0001));
    add(
        l1.edx,
        &[
            (0, "fpu"),
            (1, "vme"),
            (2, "de"),
            (3, "pse"),
            (4, "tsc"),
            (5, "msr"),
            (6, "pae"),
            (7, "mce"),
            (8, "cx8"),
            (9, "apic"),
            (11, "sep"),
            (12, "mtrr"),
            (13, "pge"),
            (14, "mca"),
            (15, "cmov"),
            (16, "pat"),
            (17, "pse36"),
            (19, "clflush"),
            (23, "mmx"),
            (24, "fxsr"),
            (25, "sse"),
            (26, "sse2"),
            (28, "ht"),
        ],
    );
    add(
        e1.edx,
        &[
            (11, "syscall"),
            (20, "nx"),
            (26, "pdpe1gb"),
            (27, "rdtscp"),
            (29, "lm"),
        ],
    );
    add(
        l1.ecx,
        &[
            (0, "pni"),
            (1, "pclmulqdq"),
            (9, "ssse3"),
            (12, "fma"),
            (13, "cx16"),
            (17, "pcid"),
            (19, "sse4_1"),
            (20, "sse4_2"),
            (21, "x2apic"),
            (22, "movbe"),
            (23, "popcnt"),
            (25, "aes"),
            (26, "xsave"),
            (28, "avx"),
            (29, "f16c"),
            (30, "rdrand"),
        ],
    );
    add(e1.ecx, &[(0, "lahf_lm"), (5, "abm")]);
    add(
        l7.ebx,
        &[
            (3, "bmi1"),
            (5, "avx2"),
            (8, "bmi2"),
            (9, "erms"),
            (16, "avx512f"),
            (18, "rdseed"),
            (19, "adx"),
            (29, "sha_ni"),
        ],
    );
    out.extend(["constant_tsc", "nopl", "cpuid", "hypervisor"]);
    out.join(" ")
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_flags() -> String {
    String::new()
}

fn format_cpuinfo(cpu_count: usize) -> Vec<u8> {
    let flags = cpu_flags();
    let mut s = String::new();
    for i in 0..cpu_count.max(1) {
        s.push_str(&format!(
            "processor\t: {i}\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t: 158\nmodel name\t: LiteBox Virtual CPU\nstepping\t: 0\ncpu MHz\t: 2000.000\ncache size\t: 8192 KB\nphysical id\t: 0\nsiblings\t: {cpu_count}\ncore id\t: {i}\ncpu cores\t: {cpu_count}\nfpu\t: yes\nflags\t: {flags}\nbogomips\t: 4000.00\nclflush size\t: 64\ncache_alignment\t: 64\naddress sizes\t: 46 bits physical, 48 bits virtual\n\n"
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
fn format_stat_global(cpu_count: usize, boot_unix_secs: u64) -> Vec<u8> {
    let mut out = String::from("cpu  0 0 0 0 0 0 0 0 0 0
");
    for cpu in 0..cpu_count {
        out.push_str(&format!(
            "cpu{cpu} 0 0 0 0 0 0 0 0 0 0
"
        ));
    }
    out.push_str(&format!("intr 0
ctxt 0
btime {boot_unix_secs}
processes 0
procs_running 1
procs_blocked 0
"));
    out.into_bytes()
}

/// Real `/proc/cmdline`: the kernel boot command line, one line. Must exist -- systemd-ish tooling,
/// container-detection heuristics and `dracut`-style probes read a missing file as a broken `/proc`
/// mount rather than an empty command line. See gm mutable `mut-1789043872060`.
fn format_kernel_cmdline() -> Vec<u8> {
    b"BOOT_IMAGE=/litebox root=/dev/root rw
"
    .to_vec()
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
    Vec::from(
        &b"0
"[..],
    )
}

/// Real `/proc/[pid]/cgroup`, unified (v2) form: `hierarchy-ID:controller-list:cgroup-path` per
/// line, so a v2-only system with no controllers of its own is exactly `0::/`. Must exist rather
/// than `ENOENT` -- glib's `g_get_user_runtime_dir` and systemd's `sd_pid_get_unit` read a missing
/// file as "cgroups not mounted at all". See gm mutable `mut-1789043885409`.
fn format_cgroup() -> Vec<u8> {
    Vec::from(
        &b"0::/
"[..],
    )
}

/// Real `/proc/uptime`: two space-separated float seconds (uptime, summed idle), `\n`-terminated.
/// Idle is reported equal to uptime -- no guest idle accounting exists, and consumers only need two
/// parseable floats. See gm mutable `mut-1789044456945`.
fn format_uptime(uptime_secs: u64) -> Vec<u8> {
    format!("{uptime_secs}.00 {uptime_secs}.00\n").into_bytes()
}

/// A [`Backend`] serving the static/host-derived `/proc` flat files that need no per-process state
/// -- the exact set is `ProcfsEntry::ALL`, PLUS one subdirectory per pid [`ProcSelfTable`] tracks
/// (`stat`/`status`/`cmdline`/`comm`, reusing the same renderers `/proc/self` uses -- see
/// [`ProcfsDirHandle::Pid`]). Mounted at `/proc`; still not a GENERAL procfs -- only pids this
/// table knows about are visible, real Linux's full `/proc/<pid>` file set is not reproduced (no
/// `exe`/`fd`/`maps`/`environ`). Real Linux's own `ps`/`procps` library needs at minimum its own
/// `/proc/<self-pid>/stat` to open successfully -- see gm mutable `mut-1789043963534`.
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
    boot_unix_secs: u64,
    /// The SAME table `/proc/self` reads through, keyed by pid -- lets `/proc/<pid>/*` answer for
    /// any pid this process's `execve`/`clone` history has recorded, not just the caller's own.
    proc_self_info: alloc::sync::Arc<RwLock<Platform, ProcSelfTable>>,
}

impl<Platform> Procfs<Platform>
where
    Platform: RawSyncPrimitivesProvider + 'static,
{
    /// Construct a new `Procfs` backend.
    ///
    /// `cpu_count` must be the real host logical-CPU count (GLib thread-pool sizing depends on it);
    /// `boot_uptime_secs` is fixed for this backend's lifetime. `proc_self_info` must be the same
    /// table instance passed to `/proc/self`'s own [`ProcSelf::new`] -- see gm mutable
    /// `mut-1789043963534`.
    #[must_use]
    pub fn new(
        litebox: &LiteBox<Platform>,
        allocator: InodeAllocator,
        cpu_count: usize,
        mem_total_kb: u64,
        mem_avail_kb: u64,
        boot_uptime_secs: u64,
        boot_unix_secs: u64,
        proc_self_info: alloc::sync::Arc<RwLock<Platform, ProcSelfTable>>,
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
            boot_unix_secs,
            proc_self_info,
        }
    }
}

/// Directory handle: the mount root (flat global files, plus one subdirectory per known pid), or a
/// specific pid's own subdirectory (flat: `stat`/`status`/`cmdline`/`comm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcfsDirHandle {
    Root,
    Pid(i32),
    /// `/proc/<pid>/task`: one subdirectory per live thread id.
    PidTask(i32),
    /// `/proc/<pid>/fd`: one symlink per open descriptor of that process.
    PidFd(i32),
}

/// Parses a `/proc` path component as a pid directory name -- real Linux's own rule: an unsigned
/// decimal integer, no sign, nothing else (so `+1`/`01`-with-exotic-meaning/`-1` are never
/// mistaken for a pid; real `/proc` itself only ever names directories this way).
fn parse_pid_component(s: &str) -> Option<i32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i32>().ok()
}

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

/// Which of the flat per-pid files a `/proc/<pid>/*` handle names -- a small, deliberate subset of
/// [`ProcSelfEntry`] (real `ps`/`procps` needs only these to open successfully and render a
/// listing; `exe`/`environ`/`maps`/etc. would need this table to also carry another process's
/// page-manager closure, which it does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcPidEntry {
    Stat,
    Status,
    Cmdline,
    Comm,
    OomScoreAdj,
    OomAdj,
    Environ,
}

impl ProcPidEntry {
    const ALL: &'static [(&'static str, ProcPidEntry)] = &[
        ("stat", ProcPidEntry::Stat),
        ("status", ProcPidEntry::Status),
        ("cmdline", ProcPidEntry::Cmdline),
        ("comm", ProcPidEntry::Comm),
        ("oom_score_adj", ProcPidEntry::OomScoreAdj),
        ("oom_adj", ProcPidEntry::OomAdj),
        ("environ", ProcPidEntry::Environ),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }
}

/// Which backend file a `/proc` [`ProcfsFileHandle`] names: one of the flat root files, or one of
/// [`ProcPidEntry`]'s files under a specific pid's own subdirectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcfsFileKind {
    Global(ProcfsEntry),
    Pid(i32, ProcPidEntry),
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
    kind: ProcfsFileKind,
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
        WalkingDirHandle::from_typed::<Self>(ProcfsDirHandle::Root)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let mut current = from.into_typed::<Self>();
        let mut walked = Vec::with_capacity(components.len());
        for &component in components {
            match current {
                ProcfsDirHandle::Pid(pid) if component == "fd" => {
                    walked.push(super::backend::WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    });
                    current = ProcfsDirHandle::PidFd(pid);
                    continue;
                }
                ProcfsDirHandle::Pid(pid) if component == "task" => {
                    walked.push(super::backend::WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    });
                    current = ProcfsDirHandle::PidTask(pid);
                    continue;
                }
                ProcfsDirHandle::PidTask(pid) => {
                    let known = parse_pid_component(component).is_some_and(|tid| {
                        self.proc_self_info
                            .read()
                            .thread_ids(pid)
                            .is_some_and(|t| t.contains(&tid))
                    });
                    if !known {
                        return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
                    }
                    // A thread's own directory answers with its process's files.
                    walked.push(super::backend::WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    });
                    current = ProcfsDirHandle::Pid(pid);
                    continue;
                }
                _ => {}
            }
            if current != ProcfsDirHandle::Root {
                // A pid directory holds files plus `task`; anything else ends the walk.
                return Ok(WalkOutcome {
                    components: walked,
                    last: WalkingDirHandle::from_typed::<Self>(current),
                    stop_reason: WalkStopReason::StoppedAtNonDirectory,
                });
            }
            if let Some(pid) = parse_pid_component(component) {
                if self.proc_self_info.read().get(pid).is_some() {
                    walked.push(super::backend::WalkedComponent {
                        permissions: PermissionCheck::ByBackend,
                    });
                    current = ProcfsDirHandle::Pid(pid);
                    continue;
                }
            }
            if ProcfsEntry::from_name(component).is_none() {
                return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
            }
            return Ok(WalkOutcome {
                components: walked,
                last: WalkingDirHandle::from_typed::<Self>(current),
                stop_reason: WalkStopReason::StoppedAtNonDirectory,
            });
        }
        Ok(WalkOutcome {
            components: walked,
            last: WalkingDirHandle::from_typed::<Self>(current),
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
        let dir = dir.into_typed::<Self>();
        if flags.contains(OFlags::DIRECTORY) {
            return Err(OpenError::PathError(PathError::ComponentNotADirectory));
        }
        let (kind, content) = match dir {
            ProcfsDirHandle::PidFd(_) => {
                return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
            }
            ProcfsDirHandle::Root => {
                let entry = ProcfsEntry::from_name(name)
                    .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
                let content = match entry {
                    ProcfsEntry::CpuInfo => format_cpuinfo(self.cpu_count),
                    ProcfsEntry::MemInfo => format_meminfo(self.mem_total_kb, self.mem_avail_kb),
                    ProcfsEntry::Mounts => format_mounts(),
                    ProcfsEntry::Uptime => format_uptime(self.boot_uptime_secs),
                    ProcfsEntry::Filesystems => format_filesystems(),
                    ProcfsEntry::Stat => format_stat_global(self.cpu_count, self.boot_unix_secs),
                    ProcfsEntry::Cmdline => format_kernel_cmdline(),
                };
                (ProcfsFileKind::Global(entry), content)
            }
            ProcfsDirHandle::Pid(pid) => {
                let entry = ProcPidEntry::from_name(name)
                    .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
                // The process may have exited between the `readdir` that found this pid and this
                // `open` -- real Linux reports the same ENOENT for that race.
                let info = self
                    .proc_self_info
                    .read()
                    .get(pid)
                    .cloned()
                    .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
                let content = match entry {
                    ProcPidEntry::Stat => format_stat(&info),
                    ProcPidEntry::Status => format_status(&info),
                    ProcPidEntry::Cmdline => info.cmdline.clone(),
                    ProcPidEntry::Comm => format!("{}\n", info.comm).into_bytes(),
                    ProcPidEntry::OomScoreAdj | ProcPidEntry::OomAdj => b"0\n".to_vec(),
                    ProcPidEntry::Environ => info.environ.clone(),
                };
                (ProcfsFileKind::Pid(pid, entry), content)
            }
            ProcfsDirHandle::PidTask(_) | ProcfsDirHandle::PidFd(_) => {
                return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
            }
        };
        Ok(Permissioned {
            item: FileHandle::from_typed::<Self>(ProcfsFileHandle { kind, content }),
            permissions: PermissionCheck::ByBackend,
        })
    }

    fn read_link_at(
        &self,
        dir: WalkingDirHandle<'_>,
        name: &str,
    ) -> Result<Option<String>, OpenError> {
        let ProcfsDirHandle::PidFd(pid) = dir.into_typed::<Self>() else {
            return Ok(None);
        };
        let fd: i32 = name
            .parse()
            .map_err(|_| OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
        self.proc_self_info
            .read()
            .get(pid)
            .and_then(|info| info.fds.clone())
            .map(|f| f())
            .and_then(|l| l.into_iter().find(|(n, _)| *n == fd))
            .map(|(_, target)| Some(target))
            .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let handle = handle.into_typed::<Self>();
        match handle {
            ProcfsDirHandle::Root => {
                let mut entries: Vec<DirEntry> = ProcfsEntry::ALL
                    .iter()
                    .map(|(n, _)| DirEntry {
                        name: String::from(*n),
                        file_type: FileType::RegularFile,
                        ino_info: None,
                    })
                    .collect();
                entries.extend(
                    self.proc_self_info
                        .read()
                        .pids()
                        .into_iter()
                        .map(|pid| DirEntry {
                            name: format!("{pid}"),
                            file_type: FileType::Directory,
                            ino_info: None,
                        }),
                );
                Ok(entries)
            }
            ProcfsDirHandle::Pid(_) => {
                let mut entries: Vec<DirEntry> = ProcPidEntry::ALL
                    .iter()
                    .map(|(n, _)| DirEntry {
                        name: String::from(*n),
                        file_type: FileType::RegularFile,
                        ino_info: None,
                    })
                    .collect();
                for dir in ["task", "fd"] {
                    entries.push(DirEntry {
                        name: String::from(dir),
                        file_type: FileType::Directory,
                        ino_info: None,
                    });
                }
                Ok(entries)
            }
            ProcfsDirHandle::PidFd(pid) => Ok(self
                .proc_self_info
                .read()
                .get(pid)
                .and_then(|info| info.fds.clone())
                .map(|f| f())
                .unwrap_or_default()
                .into_iter()
                .map(|(fd, _)| DirEntry {
                    name: format!("{fd}"),
                    file_type: FileType::Symlink,
                    ino_info: None,
                })
                .collect()),
            ProcfsDirHandle::PidTask(pid) => Ok(self
                .proc_self_info
                .read()
                .thread_ids(pid)
                .unwrap_or_default()
                .into_iter()
                .map(|tid| DirEntry {
                    name: format!("{tid}"),
                    file_type: FileType::Directory,
                    ino_info: None,
                })
                .collect()),
        }
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

    fn write(&self, h: &FileHandle, buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        // The OOM-killer knobs are accepted and ignored: nothing here ever OOM-kills, and the
        // callers (Chromium's zygote host) only log a failure to set them.
        match h.get_typed::<Self>().kind {
            ProcfsFileKind::Pid(_, ProcPidEntry::OomScoreAdj | ProcPidEntry::OomAdj) => {
                Ok(buf.len())
            }
            _ => Err(WriteError::NotForWriting),
        }
    }

    /// The OOM-killer knobs are the only writable entries here, and their write is
    /// accepted-and-ignored rather than stored: copying one into an upper layer would fabricate a
    /// file that answers every later read with whatever was last written to it.
    fn services_own_writes(&self, path: &str) -> bool {
        path.ends_with("/oom_score_adj") || path.ends_with("/oom_adj")
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
        let node_info = match h.kind {
            ProcfsFileKind::Global(entry) => match entry {
                ProcfsEntry::CpuInfo => PROCFS_CPUINFO_NODE_INFO,
                ProcfsEntry::MemInfo => PROCFS_MEMINFO_NODE_INFO,
                ProcfsEntry::Mounts => PROCFS_MOUNTS_NODE_INFO,
                ProcfsEntry::Uptime => PROCFS_UPTIME_NODE_INFO,
                ProcfsEntry::Filesystems => PROCFS_FILESYSTEMS_NODE_INFO,
                ProcfsEntry::Stat => PROCFS_STAT_NODE_INFO,
                ProcfsEntry::Cmdline => PROCFS_CMDLINE_NODE_INFO,
            },
            // Fake but stable per (pid, entry) -- dev 8 is unused by every other node in this file
            // (global entries use 6, `/proc/self` uses 7).
            ProcfsFileKind::Pid(pid, entry) => NodeInfo {
                dev: 8,
                ino: (pid as i64).unsigned_abs() as usize * 8 + entry as usize,
                rdev: None,
            },
        };
        Ok(FileStatus {
            nlink: 1,
            file_type: FileType::RegularFile,
            mode: if matches!(
                h.kind,
                ProcfsFileKind::Pid(_, ProcPidEntry::OomScoreAdj | ProcPidEntry::OomAdj)
            ) {
                Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
            } else {
                Mode::RUSR | Mode::RGRP | Mode::ROTH
            },
            size: h.content.len(),
            owner: UserInfo::ROOT,
            node_info,
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let node_info = match *h.get_typed::<Self>() {
            ProcfsDirHandle::Root => self.root_inode.clone(),
            ProcfsDirHandle::Pid(pid) => NodeInfo {
                dev: 8,
                ino: (pid as i64).unsigned_abs() as usize * 8 + 100,
                rdev: None,
            },
            ProcfsDirHandle::PidTask(pid) => NodeInfo {
                dev: 8,
                ino: (pid as i64).unsigned_abs() as usize * 8 + 101,
                rdev: None,
            },
            ProcfsDirHandle::PidFd(pid) => NodeInfo {
                dev: 8,
                ino: (pid as i64).unsigned_abs() as usize * 8 + 102,
                rdev: None,
            },
        };
        Ok(FileStatus {
            nlink: 1,
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
        info.tids = None;
        info.fds = None;
        self.by_pid.insert(child, info);
    }

    /// The live thread ids of `pid`, ascending, when that process is known and publishes them.
    pub fn thread_ids(&self, pid: i32) -> Option<Vec<i32>> {
        let info = self.by_pid.get(&pid)?;
        Some(info.tids.as_ref().map_or_else(|| alloc::vec![pid], |f| f()))
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

    /// The entry for a SPECIFIC pid, regardless of caller identity -- backs `/proc/<pid>/*`, unlike
    /// [`Self::resolve`] which answers `/proc/self` for whichever process is asking.
    /// The `exe` path recorded for `pid`, if that process is known.
    pub fn get_exe_path(&self, pid: i32) -> Option<String> {
        self.get(pid)
            .map(|i| i.exe_path.clone())
            .filter(|p| !p.is_empty())
    }

    fn get(&self, pid: i32) -> Option<&ProcSelfInfo> {
        self.by_pid.get(&pid)
    }

    /// A copy of `pid`'s entry without its process-bound parts (`maps` closes over a page manager,
    /// `tids`/`fds` close over the process), for handing a process's identity to a cross-process fork child.
    #[must_use]
    pub fn portable_snapshot(&self, pid: i32) -> Option<ProcSelfInfo> {
        let mut info = self.by_pid.get(&pid)?.clone();
        info.maps = None;
        info.tids = None;
        info.fds = None;
        Some(info)
    }

    /// Every pid this table currently has an entry for, for `/proc`'s own directory listing.
    fn pids(&self) -> Vec<i32> {
        self.by_pid.keys().copied().collect()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSelfDirHandle {
    /// `/proc/self` itself (also what `/proc/self/task/<tid>` resolves to).
    Root,
    /// `/proc/self/task`: one directory per live thread.
    Task,
    /// `/proc/self/fd`: one symlink per open descriptor.
    Fd,
    /// `/proc/self/fdinfo`: one regular file per open descriptor, mirroring `fd`.
    FdInfo,
    /// `/proc/self/ns`: holds `user` and `pid`.
    Ns,
}

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
    UidMap,
    GidMap,
    Setgroups,
    /// `/proc/self/ns/user`. Not in [`Self::ALL`]: it lives one directory down, so a flat name
    /// lookup must never reach it.
    NsUser,
    /// `/proc/self/ns/pid`. Not in [`Self::ALL`], for the same reason as [`Self::NsUser`].
    NsPid,
    /// One `/proc/self/fdinfo/<fd>` file. Not in [`Self::ALL`]: it lives one directory down, and
    /// its name is a descriptor number rather than a fixed entry name.
    FdInfoFile,
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
        ("uid_map", ProcSelfEntry::UidMap),
        ("gid_map", ProcSelfEntry::GidMap),
        ("setgroups", ProcSelfEntry::Setgroups),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }

    /// The `/proc/self/{uid_map,gid_map,setgroups}` control file this entry is, if any: those three
    /// are the only writable entries in this backend.
    fn as_id_map_file(self) -> Option<crate::fs::ident::IdMapFile> {
        match self {
            Self::UidMap => Some(crate::fs::ident::IdMapFile::UidMap),
            Self::GidMap => Some(crate::fs::ident::IdMapFile::GidMap),
            Self::Setgroups => Some(crate::fs::ident::IdMapFile::Setgroups),
            _ => None,
        }
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
const PROC_SELF_UID_MAP_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 11,
    rdev: None,
};
const PROC_SELF_GID_MAP_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 12,
    rdev: None,
};
const PROC_SELF_SETGROUPS_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 13,
    rdev: None,
};
const PROC_SELF_NS_USER_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 14,
    rdev: None,
};
const PROC_SELF_NS_PID_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 15,
    rdev: None,
};
const PROC_SELF_FDINFO_NODE_INFO: NodeInfo = NodeInfo {
    dev: 7,
    ino: 16,
    rdev: None,
};

/// The namespace links `/proc/self/ns` exposes. `user` is there because litebox really implements
/// user namespaces (`clone(CLONE_NEWUSER)`/`unshare`) and Chromium probes it. `pid` is listed too
/// because `Credentials::DropFileSystemAccess` needs a `chroot` target with no subdirectory --
/// `/proc/self/fdinfo` -- and a process that checks its own pid namespace first has to find one.
/// `net` is deliberately absent: litebox implements no network namespace, and claiming one it
/// cannot honour turns a clean `ENOSYS` into a sandbox that silently isolates nothing.
const PROC_SELF_NS_USER_CONTENT: &[u8] = b"user:[4026531837]\n";
const PROC_SELF_NS_PID_CONTENT: &[u8] = b"pid:[4026531836]\n";

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
        WalkingDirHandle::from_typed::<Self>(ProcSelfDirHandle::Root)
    }

    fn walk_directories<'a>(
        &'a self,
        from: WalkingDirHandle<'a>,
        components: &[&str],
    ) -> Result<WalkOutcome<WalkingDirHandle<'a>>, WalkError> {
        let mut current = from.into_typed::<Self>();
        let mut walked = Vec::with_capacity(components.len());
        for &component in components {
            let next = match current {
                ProcSelfDirHandle::Root => match component {
                    "task" => Some(ProcSelfDirHandle::Task),
                    "fd" => Some(ProcSelfDirHandle::Fd),
                    "fdinfo" => Some(ProcSelfDirHandle::FdInfo),
                    "ns" => Some(ProcSelfDirHandle::Ns),
                    _ => None,
                },
                ProcSelfDirHandle::Task => {
                    let tid: i32 = component
                        .parse()
                        .map_err(|_| WalkError::PathError(PathError::NoSuchFileOrDirectory))?;
                    let live = self
                        .current()
                        .and_then(|i| i.tids.map(|f| f()))
                        .is_some_and(|t| t.contains(&tid));
                    if !live {
                        return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
                    }
                    Some(ProcSelfDirHandle::Root)
                }
                ProcSelfDirHandle::Fd => None,
                ProcSelfDirHandle::Ns => None,
                // `fdinfo/<fd>` is always a file, never a directory: it has no subdirectory, which
                // is what makes it usable as a `chroot` target.
                ProcSelfDirHandle::FdInfo => None,
            };
            if let Some(next) = next {
                walked.push(super::backend::WalkedComponent {
                    permissions: PermissionCheck::ByBackend,
                });
                current = next;
                continue;
            }
            // Not a subdirectory: it must name a file of this directory (or not exist).
            let exists = match current {
                ProcSelfDirHandle::Root => ProcSelfEntry::from_name(component).is_some(),
                ProcSelfDirHandle::Ns => matches!(component, "user" | "pid"),
                ProcSelfDirHandle::Fd => component.parse::<i32>().is_ok(),
                ProcSelfDirHandle::FdInfo => component.parse::<i32>().is_ok(),
                ProcSelfDirHandle::Task => false,
            };
            if !exists {
                return Err(WalkError::PathError(PathError::NoSuchFileOrDirectory));
            }
            return Ok(WalkOutcome {
                components: walked,
                last: WalkingDirHandle::from_typed::<Self>(current),
                stop_reason: WalkStopReason::StoppedAtNonDirectory,
            });
        }
        Ok(WalkOutcome {
            components: walked,
            last: WalkingDirHandle::from_typed::<Self>(current),
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
        let dir = dir.into_typed::<Self>();
        let entry = match dir {
            ProcSelfDirHandle::Root => ProcSelfEntry::from_name(name)
                .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory))?,
            ProcSelfDirHandle::Ns => match name {
                "user" => ProcSelfEntry::NsUser,
                "pid" => ProcSelfEntry::NsPid,
                _ => return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory)),
            },
            // `/proc/self/fdinfo/<fd>`: one regular file per live descriptor, mirroring `fd`'s
            // symlinks. Named by fd number exactly like Linux, and (unlike `fd`) a regular file --
            // a directory containing only regular files is what makes `fdinfo` usable as the
            // `chroot` target `Credentials::DropFileSystemAccess` needs.
            ProcSelfDirHandle::FdInfo => {
                let fd: i32 = name
                    .parse()
                    .map_err(|_| OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
                let live = self
                    .current()
                    .and_then(|i| i.fds.map(|f| f()))
                    .is_some_and(|l| l.iter().any(|(n, _)| *n == fd));
                if !live {
                    return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
                }
                ProcSelfEntry::FdInfoFile
            }
            ProcSelfDirHandle::Task | ProcSelfDirHandle::Fd => {
                return Err(OpenError::PathError(PathError::NoSuchFileOrDirectory));
            }
        };
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
            // The id-map control files are the only entries here a guest may write, and their
            // contents come from the caller's own process, not from the `/proc/self` snapshot.
            ProcSelfEntry::UidMap => {
                crate::fs::ident::read_id_map_file(crate::fs::ident::IdMapFile::UidMap)
            }
            ProcSelfEntry::GidMap => {
                crate::fs::ident::read_id_map_file(crate::fs::ident::IdMapFile::GidMap)
            }
            ProcSelfEntry::Setgroups => {
                crate::fs::ident::read_id_map_file(crate::fs::ident::IdMapFile::Setgroups)
            }
            ProcSelfEntry::NsUser => PROC_SELF_NS_USER_CONTENT.to_vec(),
            ProcSelfEntry::NsPid => PROC_SELF_NS_PID_CONTENT.to_vec(),
            // Real Linux reports the descriptor's file offset and status flags here. litebox's
            // per-fd status flags live in the descriptor table, not in this backend, so this stays
            // a well-formed stub: the file's EXISTENCE and type are what a consumer checks.
            ProcSelfEntry::FdInfoFile => alloc::format!("pos:\t0\nflags:\t0\n").into_bytes(),
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
        let dir_handle = dir.into_typed::<Self>();
        if dir_handle == ProcSelfDirHandle::Fd {
            let fd: i32 = name
                .parse()
                .map_err(|_| OpenError::PathError(PathError::NoSuchFileOrDirectory))?;
            return self
                .current()
                .and_then(|i| i.fds.map(|f| f()))
                .and_then(|l| l.into_iter().find(|(n, _)| *n == fd))
                .map(|(_, target)| Some(target))
                .ok_or(OpenError::PathError(PathError::NoSuchFileOrDirectory));
        }
        if dir_handle == ProcSelfDirHandle::Ns && matches!(name, "user" | "pid") {
            return Ok(None);
        }
        if matches!(name, "task" | "fd" | "fdinfo" | "ns") {
            return Ok(None);
        }
        match ProcSelfEntry::from_name(name) {
            Some(ProcSelfEntry::Exe) => Ok(self.current().map(|info| info.exe_path)),
            Some(_) => Ok(None),
            None => Err(OpenError::PathError(PathError::NoSuchFileOrDirectory)),
        }
    }

    fn list_dir_at(&self, handle: DirHandle) -> Result<Vec<DirEntry>, ReadDirError> {
        let info = self.current().unwrap_or_default();
        match handle.into_typed::<Self>() {
            ProcSelfDirHandle::Root => {
                let mut entries: Vec<DirEntry> = ProcSelfEntry::ALL
                    .iter()
                    .map(|(n, e)| DirEntry {
                        name: String::from(*n),
                        // `exe`'s `d_type` must say `Symlink`: a caller trusting `getdents64`'s
                        // `d_type` instead of re-`lstat`ing would never follow it. gm mutable
                        // `mut-1789043942984`.
                        file_type: match e {
                            ProcSelfEntry::Exe => FileType::Symlink,
                            _ => FileType::RegularFile,
                        },
                        ino_info: None,
                    })
                    .collect();
                for dir in ["task", "fd", "fdinfo", "ns"] {
                    entries.push(DirEntry {
                        name: String::from(dir),
                        file_type: FileType::Directory,
                        ino_info: None,
                    });
                }
                Ok(entries)
            }
            ProcSelfDirHandle::Ns => Ok(alloc::vec![
                DirEntry {
                    name: String::from("user"),
                    file_type: FileType::RegularFile,
                    ino_info: None,
                },
                DirEntry {
                    name: String::from("pid"),
                    file_type: FileType::RegularFile,
                    ino_info: None,
                },
            ]),
            ProcSelfDirHandle::Task => Ok(info
                .tids
                .map(|f| f())
                .unwrap_or_default()
                .into_iter()
                .map(|t| DirEntry {
                    name: alloc::format!("{t}"),
                    file_type: FileType::Directory,
                    ino_info: None,
                })
                .collect()),
            ProcSelfDirHandle::Fd => Ok(info
                .fds
                .map(|f| f())
                .unwrap_or_default()
                .into_iter()
                .map(|(fd, _)| DirEntry {
                    name: alloc::format!("{fd}"),
                    file_type: FileType::Symlink,
                    ino_info: None,
                })
                .collect()),
            // Same names as `fd`, but regular files: Chromium's `DropFileSystemAccess` needs a
            // `chroot` target with no subdirectory, and checks that with `getdents`' own `d_type`.
            ProcSelfDirHandle::FdInfo => Ok(info
                .fds
                .map(|f| f())
                .unwrap_or_default()
                .into_iter()
                .map(|(fd, _)| DirEntry {
                    name: alloc::format!("{fd}"),
                    file_type: FileType::RegularFile,
                    ino_info: None,
                })
                .collect()),
        }
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

    /// Only the id-map control files accept a write; every other entry keeps refusing it.
    fn write(&self, h: &FileHandle, buf: &[u8], _offset: usize) -> Result<usize, WriteError> {
        let Some(id_map) = h.get_typed::<Self>().entry.as_id_map_file() else {
            return Err(WriteError::NotForWriting);
        };
        crate::fs::ident::write_id_map_file(id_map, buf)
            .map(|()| buf.len())
            .map_err(|errno| match errno {
                // `WriteError` carries no errno, so a refusal can only take the shape every other
                // fs refusal takes (`NotForWriting`, rendered as `EBADF`, as `nine_p`'s own errno
                // conversion does for `EPERM`/`EACCES`); a malformed map (`EINVAL`) is `Io`.
                1 | 13 => WriteError::NotForWriting,
                _ => WriteError::Io,
            })
    }

    fn truncate(&self, _h: &FileHandle, _len: usize) -> Result<(), TruncateError> {
        Err(TruncateError::NotForWriting)
    }

    /// The id-map control files: a write to one remaps the calling process's ids, so it has to
    /// reach this backend. Copied up, `echo 1000 0 1 > /proc/self/uid_map` would succeed against an
    /// ordinary file in the guest's writable layer and leave `getuid()` still reporting `nobody` --
    /// the mapping would look applied and would not be.
    fn services_own_writes(&self, path: &str) -> bool {
        path.ends_with("/uid_map") || path.ends_with("/gid_map") || path.ends_with("/setgroups")
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
            nlink: 1,
            file_type: FileType::RegularFile,
            // Real Linux's id-map control files are owner-writable; a guest that checks `W_OK`
            // before writing (instead of just writing) has to see that here.
            mode: if h.entry.as_id_map_file().is_some() {
                Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
            } else {
                Mode::RUSR | Mode::RGRP | Mode::ROTH
            },
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
                ProcSelfEntry::UidMap => PROC_SELF_UID_MAP_NODE_INFO,
                ProcSelfEntry::GidMap => PROC_SELF_GID_MAP_NODE_INFO,
                ProcSelfEntry::Setgroups => PROC_SELF_SETGROUPS_NODE_INFO,
                ProcSelfEntry::NsUser => PROC_SELF_NS_USER_NODE_INFO,
                ProcSelfEntry::NsPid => PROC_SELF_NS_PID_NODE_INFO,
                ProcSelfEntry::FdInfoFile => PROC_SELF_FDINFO_NODE_INFO,
            },
            blksize: 0x1000,
            atime: Timestamp::default(),
            mtime: Timestamp::default(),
        })
    }

    fn dir_status(&self, h: &DirHandle) -> Result<FileStatus, FileStatusError> {
        let h = *h.get_typed::<Self>();
        // `nlink` of `/proc/self/task` is `2 + threads`; Chromium's sandbox uses `== 3` as its
        // "single-threaded" test, so it has to be honest.
        let nlink = match h {
            ProcSelfDirHandle::Task => {
                2 + self
                    .current()
                    .and_then(|i| i.tids.map(|f| f().len()))
                    .unwrap_or(1)
            }
            // `.`, plus one for each subdirectory (`task`, `fd`, `fdinfo`, `ns`).
            ProcSelfDirHandle::Root => 5,
            ProcSelfDirHandle::Fd => 2,
            ProcSelfDirHandle::FdInfo => 2,
            ProcSelfDirHandle::Ns => 2,
        };
        Ok(FileStatus {
            nlink: nlink as _,
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
